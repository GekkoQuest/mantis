//! The forward renderer: GPU-driven scene, clustered forward+ lighting, cascaded sun
//! shadows, cooked indirect light, SSAO, outlines, and post-processing (TAA, bloom, tone
//! mapping, grading), as a render graph.
//!
//! Each frame has two halves:
//! - [`Renderer::prepare`] (CPU, allocation-free after warm-up): batches the scene,
//!   assigns lights to clusters, fits shadow cascades, builds every per-frame uniform.
//! - [`Renderer::encode`]: uploads what `prepare` built and records the graph: cull
//!   (compute, per view), particle simulation, shadows, depth prepass, SSAO, forward,
//!   outline, particles, TAA, bloom, composite, UI.
//!
//! Views: index 0 is the camera; 1 to 4 are shadow cascades. Each view has its own indirect
//! arguments and visible list, filled by the cull pass from the shared instance table.

use glam::Vec3;
use mantis_formats::material::Pass;
use mantis_formats::sh::ShL1;

use crate::culling::{CullUniform, GpuCuller};
use crate::deform::{DeformError, DeformPool, PaletteRange, PaletteRows, VatId, VatUpload};
use crate::gpu_types::{DYNAMIC_STRIDE, GpuDrawParams, GpuFrame, GpuPassView, SkinVertex, Vertex};
use crate::graph::exec::{GraphPass, ImportedBuffer, ImportedTexture, Imports, PassContext, RenderGraph};
use crate::graph::{
    BufferAccess, GraphBuilder, PassKind, TextureAccess, TextureDesc, TextureHandle, WriteMode,
};
use crate::layouts::Layouts;
use crate::lighting::clusters::{ClusterConfig, Clusters, GpuPointLight, PointLight};
use crate::lighting::csm::{CascadeConfig, MAX_CASCADES, fit_cascades};
use crate::materials::{MaterialCache, MaterialHandle, MaterialLoadError, TargetFormats};
use crate::math::{Camera, Frustum};
use crate::mesh::{MeshError, MeshStore};
use crate::occlusion::{CullBuffers, DepthPyramid, OcclusionCuller, OcclusionUniform};
use crate::particles::{ParticleConfig, ParticleSystem};
use crate::post::{BloomPass, CompositePass, HasPost, PostChain, PostSettings, TaaPass};
use crate::scene::{
    BatchDraw, Deform, Deformation, DrawIndexedIndirect, FramePrep, GpuBatch, GpuInstance, InstanceHandle,
    LightmapBinding, MaterialId, MeshId, MeshRange, Scene, SceneError,
};
use crate::ui_pass::UiRenderer;
use crate::world_light::{LightmapPage, WorldLight, WorldLightConfig};

/// Velocity target format: screen-space motion in uv units (current minus previous).
pub const VELOCITY_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rg16Float;
/// The velocity cleared where no surface was drawn: the TAA reprojects those pixels
/// through depth and the camera alone.
pub const NO_VELOCITY: f64 = 100.0;
/// HDR color format.
pub const HDR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;
/// Camera depth format (reverse Z).
pub const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;
/// Shadow map format.
pub const SHADOW_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;
/// Views: the camera plus every cascade.
pub const VIEWS: usize = 1 + MAX_CASCADES;
const SSAO_SAMPLES: usize = 16;

/// Renderer configuration.
#[derive(Clone, Copy, Debug)]
pub struct RendererConfig {
    /// Output width in pixels.
    pub width: u32,
    /// Output height in pixels.
    pub height: u32,
    /// Output (swapchain or offscreen) format.
    pub output_format: wgpu::TextureFormat,
    /// Most instances.
    pub max_instances: u32,
    /// Most distinct material and mesh batches.
    pub max_batches: u32,
    /// Shared vertex buffer capacity.
    pub max_vertices: u32,
    /// Shared index buffer capacity.
    pub max_indices: u32,
    /// Cascaded shadows.
    pub cascades: CascadeConfig,
    /// Light clustering.
    pub clusters: ClusterConfig,
    /// Shadow map size per cascade.
    pub shadow_resolution: u32,
    /// Deformation pool slots (`vec4`) for bone palettes, three per bone.
    pub palette_slots: u32,
    /// Deformation pool slots (`vec4`) for vertex animations, two per texel.
    pub vat_slots: u32,
    /// Particle system capacities.
    pub particles: ParticleConfig,
    /// Most UI quads drawn per frame.
    pub ui_quads: u32,
    /// Two-phase GPU occlusion culling of the camera view
    /// ([`crate::occlusion`]). Off: frustum culling only.
    pub occlusion: bool,
    /// Streamed indirect light capacities (lightmap pages, probe atlas).
    pub world_light: WorldLightConfig,
}

impl RendererConfig {
    /// Defaults for a `width x height` output.
    pub fn new(width: u32, height: u32, output_format: wgpu::TextureFormat) -> Self {
        Self {
            width,
            height,
            output_format,
            max_instances: 16_384,
            max_batches: 256,
            max_vertices: 1 << 20,
            max_indices: 1 << 22,
            cascades: CascadeConfig::default(),
            clusters: ClusterConfig::default(),
            shadow_resolution: 2048,
            palette_slots: 3 * 64 * 256,
            vat_slots: 1 << 20,
            particles: ParticleConfig {
                max_particles: 1 << 16,
                max_effects: 64,
                max_emitters: 256,
            },
            ui_quads: 16_384,
            occlusion: true,
            world_light: WorldLightConfig::default(),
        }
    }
}

/// Per-frame scene-level inputs.
#[derive(Clone, Copy, Debug)]
pub struct FrameInputs {
    /// The camera.
    pub camera: Camera,
    /// Seconds since the session started.
    pub time: f32,
    /// Direction sunlight travels.
    pub sun_direction: Vec3,
    /// Sun color times intensity.
    pub sun_color: Vec3,
    /// Whether the sun casts shadows.
    pub shadows: bool,
    /// Sky ambient.
    pub sky: ShL1,
    /// Multiplier on cooked and sky ambient.
    pub ambient_intensity: f32,
    /// Exposure before tone mapping.
    pub exposure: f32,
    /// Background color (linear HDR).
    pub clear_color: [f64; 4],
    /// SSAO strength, 0 disables.
    pub ssao_strength: f32,
    /// Post-processing.
    pub post: PostSettings,
}

/// Statistics of one prepared frame.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct FrameStats {
    /// Instances submitted to culling.
    pub instances: u32,
    /// Batches (indirect draws per view).
    pub batches: u32,
    /// Point lights clustered.
    pub lights: u32,
    /// Cascades rendered.
    pub cascades: u32,
    /// Live particle emitter instances.
    pub particle_emitters: u32,
}

/// Renderer errors.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RendererError {
    /// The render graph failed to compile or bind.
    Graph(String),
    /// A material failed to load.
    Material(MaterialLoadError),
    /// A mesh failed to upload.
    Mesh(MeshError),
    /// A scene operation failed.
    Scene(SceneError),
    /// The deformation pool refused an allocation or upload.
    Deform(DeformError),
    /// Streamed indirect light refused a load or lookup.
    WorldLight(crate::world_light::WorldLightError),
}

impl core::fmt::Display for RendererError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            RendererError::Graph(e) => write!(f, "render graph: {e}"),
            RendererError::Material(e) => write!(f, "material: {e}"),
            RendererError::Mesh(e) => write!(f, "mesh: {e}"),
            RendererError::Scene(e) => write!(f, "scene: {e}"),
            RendererError::Deform(e) => write!(f, "deformation: {e}"),
            RendererError::WorldLight(e) => write!(f, "world light: {e}"),
        }
    }
}

impl std::error::Error for RendererError {}

/// One view's culling output.
#[derive(Debug)]
struct ViewGpu {
    args: wgpu::Buffer,
    cull: wgpu::Buffer,
    cull_bind: wgpu::BindGroup,
    scene_bind: wgpu::BindGroup,
}

/// The camera view's occlusion culling: the late list's buffers, the pyramid, and the
/// cull bind groups.
#[derive(Debug)]
struct OcclusionGpu {
    enabled: bool,
    culler: OcclusionCuller,
    uniform: wgpu::Buffer,
    late_args: wgpu::Buffer,
    late_scene_bind: wgpu::BindGroup,
    early_bind: wgpu::BindGroup,
    late_bind: wgpu::BindGroup,
    pyramid: DepthPyramid,
    /// Kept alive for the bind groups.
    _visibility: wgpu::Buffer,
    _late_visible: wgpu::Buffer,
}

/// The scene group's bind group over one visible list.
#[allow(clippy::too_many_arguments)] // The scene group's resources, one per binding.
fn scene_bind_group(
    device: &wgpu::Device,
    layouts: &Layouts,
    label: &str,
    instances: &wgpu::Buffer,
    visible: &wgpu::Buffer,
    draw_params: &wgpu::Buffer,
    deform: &wgpu::Buffer,
    previous_models: &wgpu::Buffer,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some(label),
        layout: &layouts.scene,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: instances.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: visible.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: draw_params,
                    offset: 0,
                    size: core::num::NonZeroU64::new(DYNAMIC_STRIDE),
                }),
            },
            wgpu::BindGroupEntry {
                binding: mantis_shadergen::layout::DEFORM_POOL_BINDING,
                resource: deform.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: mantis_shadergen::layout::PREVIOUS_MODELS_BINDING,
                resource: previous_models.as_entire_binding(),
            },
        ],
    })
}

/// Everything the graph passes read. Owned by the renderer next to the graph.
#[derive(Debug)]
pub struct Shared {
    layouts: Layouts,
    materials: MaterialCache,
    meshes: MeshStore,
    deform: DeformPool,
    particles: ParticleSystem,
    ui: UiRenderer,
    instances: wgpu::Buffer,
    batches: wgpu::Buffer,
    views: Vec<ViewGpu>,
    culler: GpuCuller,
    frame: wgpu::Buffer,
    pass_views: wgpu::Buffer,
    draw_params: wgpu::Buffer,
    frame_basic_bind: wgpu::BindGroup,
    frame_forward_bind: Option<wgpu::BindGroup>,
    cluster_grid: wgpu::Buffer,
    cluster_indices: wgpu::Buffer,
    cluster_lights: wgpu::Buffer,
    shadow_sampler: wgpu::Sampler,
    linear_clamp: wgpu::Sampler,
    world_light: WorldLight,
    /// The camera view's occlusion culling.
    occlusion: OcclusionGpu,
    /// Every instance's previous-frame model matrix (motion vectors).
    previous_models: wgpu::Buffer,
    /// The velocity target (imported into the graph; readable for tests and tools).
    velocity: (wgpu::Texture, wgpu::TextureView),
    ssao_pipeline: wgpu::RenderPipeline,
    ssao_layout: wgpu::BindGroupLayout,
    ssao_params: wgpu::Buffer,
    ssao_bind: Option<wgpu::BindGroup>,
    post: PostChain,
    // Per frame, written by prepare.
    draws: Vec<BatchDraw>,
    instance_count: u32,
    cascade_count: u32,
    clear_color: wgpu::Color,
}

/// The renderer.
#[derive(Debug)]
pub struct Renderer {
    config: RendererConfig,
    shared: Shared,
    graph: RenderGraph<Shared>,
    output: TextureHandle,
    draw_lists: crate::graph::BufferHandle,
    scene: Scene,
    prep: FramePrep,
    clusters: Clusters,
    lights: Vec<PointLight>,
    gpu_frame: GpuFrame,
    pass_views: [GpuPassView; VIEWS],
    cull_uniforms: [CullUniform; VIEWS],
    draw_params: Vec<GpuDrawParams>,
    ssao_params: SsaoParams,
    history_prev: TextureHandle,
    history_next: TextureHandle,
    velocity: TextureHandle,
    pyramid: TextureHandle,
    particle_pool: crate::graph::BufferHandle,
    last_time: Option<f32>,
    /// The previous prepared frame's unjittered view projection.
    prev_view_proj: Option<glam::Mat4>,
    stats: FrameStats,
}

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct SsaoParams {
    proj: [[f32; 4]; 4],
    inv_proj: [[f32; 4]; 4],
    viewport: [f32; 4],
    settings: [f32; 4],
    kernel: [[f32; 4]; SSAO_SAMPLES],
}

impl SsaoParams {
    #[allow(clippy::cast_precision_loss)] // The sample count is tiny.
    fn new(proj: glam::Mat4, [w, h]: [f32; 2], strength: f32) -> Self {
        Self {
            proj: proj.to_cols_array_2d(),
            inv_proj: proj.inverse().to_cols_array_2d(),
            viewport: [w, h, 1.0 / w, 1.0 / h],
            settings: [0.5, strength, 0.02, SSAO_SAMPLES as f32],
            kernel: ssao_kernel(),
        }
    }
}

/// Deterministic SSAO hemisphere kernel: points inside the unit hemisphere (+z), denser
/// near the center.
#[allow(clippy::cast_precision_loss)] // Sample indices are tiny.
fn ssao_kernel() -> [[f32; 4]; SSAO_SAMPLES] {
    let mut out = [[0.0; 4]; SSAO_SAMPLES];
    let golden = core::f32::consts::PI * (3.0 - 5.0f32.sqrt());
    for (i, k) in out.iter_mut().enumerate() {
        let frac = (i as f32 + 0.5) / SSAO_SAMPLES as f32;
        let height = 1.0 - frac;
        let ring = (1.0 - height * height).max(0.0).sqrt();
        let (sin, cos) = (golden * i as f32).sin_cos();
        let scale = 0.1 + 0.9 * frac * frac;
        *k = [
            cos * ring * scale,
            sin * ring * scale,
            height.max(0.15) * scale,
            0.0,
        ];
    }
    out
}

fn uniform_buffer(device: &wgpu::Device, label: &str, size: u64) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

fn storage_buffer(device: &wgpu::Device, label: &str, size: u64, extra: wgpu::BufferUsages) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: size.max(16),
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST | extra,
        mapped_at_creation: false,
    })
}

fn fullscreen_pipeline(
    device: &wgpu::Device,
    label: &str,
    source: &str,
    entry: &str,
    layout: &wgpu::BindGroupLayout,
    format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(label),
        source: wgpu::ShaderSource::Wgsl(source.into()),
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(label),
        bind_group_layouts: &[Some(layout)],
        immediate_size: 0,
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &module,
            entry_point: Some("vs_fullscreen"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: &[],
        },
        primitive: wgpu::PrimitiveState {
            front_face: crate::gpu_types::FRONT_FACE,
            ..Default::default()
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: &module,
            entry_point: Some(entry),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    })
}

fn layout_entries(
    device: &wgpu::Device,
    label: &str,
    texture: wgpu::TextureSampleType,
) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some(label),
        entries: &[
            crate::layouts::entry(
                0,
                wgpu::ShaderStages::FRAGMENT,
                wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
            ),
            crate::layouts::entry(
                1,
                wgpu::ShaderStages::FRAGMENT,
                wgpu::BindingType::Texture {
                    sample_type: texture,
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
            ),
        ],
    })
}

/// Handles of the graph resources the renderer binds outside passes.
struct GraphHandles {
    output: TextureHandle,
    particle_pool: crate::graph::BufferHandle,
    history_prev: TextureHandle,
    history_next: TextureHandle,
    velocity: TextureHandle,
    pyramid: TextureHandle,
    bloom: TextureHandle,
    draw_lists: crate::graph::BufferHandle,
    depth: TextureHandle,
    shadow: TextureHandle,
    ssao: TextureHandle,
    hdr: TextureHandle,
}

impl Renderer {
    /// Creates the renderer for a device.
    ///
    /// # Errors
    /// [`RendererError::Graph`] if the frame graph fails to compile or bind.
    #[allow(clippy::too_many_lines)] // One-time construction of every GPU object, in order.
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        bindless: bool,
        config: RendererConfig,
    ) -> Result<Self, RendererError> {
        let layouts = Layouts::new(device, bindless);
        let materials = MaterialCache::new(
            device,
            queue,
            bindless,
            TargetFormats {
                hdr: HDR_FORMAT,
                depth: DEPTH_FORMAT,
                shadow: SHADOW_FORMAT,
                velocity: VELOCITY_FORMAT,
            },
        );
        let meshes = MeshStore::new(device, config.max_vertices, config.max_indices);
        let deform = DeformPool::new(device, config.palette_slots, config.vat_slots);
        let particles = ParticleSystem::new(device, config.particles, HDR_FORMAT, DEPTH_FORMAT);
        let ui = UiRenderer::new(device, config.output_format, config.ui_quads);
        let max_instances = u64::from(config.max_instances.max(1));
        let max_batches = u64::from(config.max_batches.max(1));
        let instances = storage_buffer(
            device,
            "scene.instances",
            max_instances * core::mem::size_of::<GpuInstance>() as u64,
            wgpu::BufferUsages::empty(),
        );
        let batches = storage_buffer(
            device,
            "scene.batches",
            max_batches * core::mem::size_of::<GpuBatch>() as u64,
            wgpu::BufferUsages::empty(),
        );
        let culler = GpuCuller::new(device);
        let previous_models = storage_buffer(
            device,
            "scene.previous_models",
            max_instances * 64,
            wgpu::BufferUsages::empty(),
        );
        let draw_params = uniform_buffer(device, "scene.draw_params", max_batches * DYNAMIC_STRIDE);
        let mut views = Vec::with_capacity(VIEWS);
        let mut camera_visible = None;
        for v in 0..VIEWS {
            let args = storage_buffer(
                device,
                "view.args",
                max_batches * core::mem::size_of::<DrawIndexedIndirect>() as u64,
                wgpu::BufferUsages::INDIRECT | wgpu::BufferUsages::COPY_SRC,
            );
            let visible = storage_buffer(
                device,
                "view.visible",
                max_instances * 4,
                wgpu::BufferUsages::COPY_SRC,
            );
            let cull = uniform_buffer(device, "view.cull", core::mem::size_of::<CullUniform>() as u64);
            let cull_bind = culler.bind_group_parts(device, &cull, &instances, &batches, &args, &visible);
            let scene_bind = scene_bind_group(
                device,
                &layouts,
                if v == 0 { "scene.camera" } else { "scene.cascade" },
                &instances,
                &visible,
                &draw_params,
                deform.buffer(),
                &previous_models,
            );
            if v == 0 {
                camera_visible = Some(visible.clone());
            }
            views.push(ViewGpu {
                args,
                cull,
                cull_bind,
                scene_bind,
            });
        }
        let frame = uniform_buffer(device, "frame", core::mem::size_of::<GpuFrame>() as u64);
        let pass_views = uniform_buffer(device, "pass_views", VIEWS as u64 * DYNAMIC_STRIDE);
        let pass_view_binding = || {
            wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                buffer: &pass_views,
                offset: 0,
                size: core::num::NonZeroU64::new(DYNAMIC_STRIDE),
            })
        };
        let frame_basic_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("frame.basic"),
            layout: &layouts.frame_basic,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: frame.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: pass_view_binding(),
                },
            ],
        });
        let cc = config.clusters;
        let cluster_grid = storage_buffer(
            device,
            "clusters.grid",
            u64::from(cc.cluster_count()) * 8,
            wgpu::BufferUsages::empty(),
        );
        let cluster_indices = storage_buffer(
            device,
            "clusters.indices",
            u64::from(cc.max_indices) * 4,
            wgpu::BufferUsages::empty(),
        );
        let cluster_lights = storage_buffer(
            device,
            "clusters.lights",
            u64::from(cc.max_lights) * core::mem::size_of::<GpuPointLight>() as u64,
            wgpu::BufferUsages::empty(),
        );
        let shadow_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("shadow"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            compare: Some(wgpu::CompareFunction::LessEqual),
            ..Default::default()
        });
        let linear_clamp = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("linear_clamp"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let world_light = WorldLight::new(device, config.world_light);
        let occlusion = {
            let culler = OcclusionCuller::new(device);
            let uniform = uniform_buffer(
                device,
                "occlusion.uniform",
                core::mem::size_of::<OcclusionUniform>() as u64,
            );
            let visibility = storage_buffer(
                device,
                "occlusion.visibility",
                max_instances * 4,
                wgpu::BufferUsages::empty(),
            );
            let late_args = storage_buffer(
                device,
                "occlusion.late_args",
                max_batches * core::mem::size_of::<DrawIndexedIndirect>() as u64,
                wgpu::BufferUsages::INDIRECT | wgpu::BufferUsages::COPY_SRC,
            );
            let late_visible = storage_buffer(
                device,
                "occlusion.late_visible",
                max_instances * 4,
                wgpu::BufferUsages::COPY_SRC,
            );
            let pyramid = culler.pyramid(device, config.width, config.height);
            let camera = views
                .first()
                .ok_or_else(|| RendererError::Graph("no camera view".to_owned()))?;
            let camera_visible =
                camera_visible.ok_or_else(|| RendererError::Graph("no camera list".to_owned()))?;
            let early_bind = culler.bind_group(
                device,
                CullBuffers {
                    uniform: &uniform,
                    instances: &instances,
                    batches: &batches,
                    args: &camera.args,
                    visible: &camera_visible,
                    visibility: &visibility,
                },
                &pyramid,
            );
            let late_bind = culler.bind_group(
                device,
                CullBuffers {
                    uniform: &uniform,
                    instances: &instances,
                    batches: &batches,
                    args: &late_args,
                    visible: &late_visible,
                    visibility: &visibility,
                },
                &pyramid,
            );
            let late_scene_bind = scene_bind_group(
                device,
                &layouts,
                "scene.camera_late",
                &instances,
                &late_visible,
                &draw_params,
                deform.buffer(),
                &previous_models,
            );
            OcclusionGpu {
                enabled: config.occlusion,
                culler,
                uniform,
                late_args,
                late_scene_bind,
                early_bind,
                late_bind,
                pyramid,
                _visibility: visibility,
                _late_visible: late_visible,
            }
        };
        let velocity_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("velocity"),
            size: wgpu::Extent3d {
                width: config.width.max(1),
                height: config.height.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: VELOCITY_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let velocity_view = velocity_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let ssao_layout = layout_entries(device, "ssao", wgpu::TextureSampleType::Depth);
        let ssao_pipeline = fullscreen_pipeline(
            device,
            "ssao",
            include_str!("shaders/ssao.wgsl"),
            "fs_ssao",
            &ssao_layout,
            wgpu::TextureFormat::R8Unorm,
        );
        let ssao_params = uniform_buffer(device, "ssao.params", core::mem::size_of::<SsaoParams>() as u64);
        let post = PostChain::new(device, queue, config.width, config.height, config.output_format);
        let mut shared = Shared {
            layouts,
            materials,
            meshes,
            deform,
            particles,
            ui,
            instances,
            batches,
            views,
            culler,
            frame,
            pass_views,
            draw_params,
            frame_basic_bind,
            frame_forward_bind: None,
            cluster_grid,
            cluster_indices,
            cluster_lights,
            shadow_sampler,
            linear_clamp,
            world_light,
            occlusion,
            previous_models,
            velocity: (velocity_texture, velocity_view),
            ssao_pipeline,
            ssao_layout,
            ssao_params,
            ssao_bind: None,
            post,
            draws: Vec::with_capacity(config.max_batches as usize),
            instance_count: 0,
            cascade_count: 0,
            clear_color: wgpu::Color::BLACK,
        };
        let (graph, handles) = build_graph(device, &config)?;
        bind_graph_resources(device, &mut shared, &graph, &handles)?;
        #[allow(clippy::cast_precision_loss)] // Pixel sizes are far below 2^24.
        let aspect = config.width as f32 / config.height.max(1) as f32;
        Ok(Self {
            config,
            graph,
            output: handles.output,
            draw_lists: handles.draw_lists,
            history_prev: handles.history_prev,
            history_next: handles.history_next,
            velocity: handles.velocity,
            pyramid: handles.pyramid,
            particle_pool: handles.particle_pool,
            last_time: None,
            prev_view_proj: None,
            scene: Scene::new(config.max_instances as usize, config.max_batches as usize, false),
            prep: FramePrep::with_capacity(config.max_instances as usize, config.max_batches as usize),
            clusters: Clusters::new(config.clusters, 1.0, aspect),
            lights: Vec::with_capacity(config.clusters.max_lights as usize),
            gpu_frame: bytemuck::Zeroable::zeroed(),
            pass_views: [bytemuck::Zeroable::zeroed(); VIEWS],
            cull_uniforms: [bytemuck::Zeroable::zeroed(); VIEWS],
            draw_params: Vec::with_capacity(config.max_batches as usize),
            ssao_params: bytemuck::Zeroable::zeroed(),
            stats: FrameStats::default(),
            shared,
        })
    }

    /// The scene (spawn, move, and despawn instances here).
    pub fn scene_mut(&mut self) -> &mut Scene {
        &mut self.scene
    }

    /// Uploads a mesh.
    ///
    /// # Errors
    /// [`RendererError::Mesh`].
    pub fn add_mesh(
        &mut self,
        queue: &wgpu::Queue,
        vertices: &[Vertex],
        indices: &[u32],
    ) -> Result<MeshId, RendererError> {
        let range: MeshRange = self
            .shared
            .meshes
            .add(queue, vertices, indices)
            .map_err(RendererError::Mesh)?;
        Ok(self.scene.add_mesh(range))
    }

    /// Uploads a cooked mesh (with its skinning stream when it has one).
    ///
    /// # Errors
    /// [`RendererError::Mesh`].
    pub fn add_mesh_asset(
        &mut self,
        queue: &wgpu::Queue,
        mesh: &mantis_formats::mesh::MeshAsset,
    ) -> Result<MeshId, RendererError> {
        let vertices = crate::assets::mesh_vertices(mesh);
        match crate::assets::mesh_skin(mesh) {
            Some(skin) => self.add_skinned_mesh(queue, &vertices, &skin, &mesh.indices),
            None => self.add_mesh(queue, &vertices, &mesh.indices),
        }
    }

    /// Uploads a skinned mesh (vertices plus the parallel skinning stream).
    ///
    /// # Errors
    /// [`RendererError::Mesh`].
    pub fn add_skinned_mesh(
        &mut self,
        queue: &wgpu::Queue,
        vertices: &[Vertex],
        skin: &[SkinVertex],
        indices: &[u32],
    ) -> Result<MeshId, RendererError> {
        let range = self
            .shared
            .meshes
            .add_skinned(queue, vertices, skin, indices)
            .map_err(RendererError::Mesh)?;
        Ok(self.scene.add_mesh(range))
    }

    /// Uploads a baked vertex animation (for the mid and far crowd tiers).
    ///
    /// # Errors
    /// [`RendererError::Deform`].
    pub fn add_vat(&mut self, queue: &wgpu::Queue, vat: &VatUpload<'_>) -> Result<VatId, RendererError> {
        self.shared
            .deform
            .add_vat(queue, vat)
            .map_err(RendererError::Deform)
    }

    /// Spawns a skinned instance with a palette of `bones` bones (identity until
    /// [`Renderer::set_palette`]). `bounds` (model space) must contain every animated
    /// vertex.
    ///
    /// # Errors
    /// [`RendererError::Deform`] when the palette region is full, [`RendererError::Scene`].
    pub fn spawn_skinned(
        &mut self,
        mesh: MeshId,
        material: MaterialId,
        model: glam::Mat4,
        bones: u32,
        bounds: crate::math::Sphere,
    ) -> Result<InstanceHandle, RendererError> {
        let palette = self
            .shared
            .deform
            .alloc_palette(bones)
            .map_err(RendererError::Deform)?;
        let deformation = Deformation::Skinned {
            palette_base: palette.base,
            bones,
        };
        self.scene
            .spawn_with(mesh, material, model, deformation, Some(bounds))
            .inspect_err(|_| self.shared.deform.free_palette(palette))
            .map_err(RendererError::Scene)
    }

    /// Writes a skinned instance's palette (from `mantis_anim`'s skinning palette).
    /// Allocation-free; a change counts as movement for TAA.
    ///
    /// # Errors
    /// [`RendererError::Scene`] for a stale or non-skinned handle, [`RendererError::Deform`]
    /// for more entries than bones or non-finite values.
    pub fn set_palette(&mut self, h: InstanceHandle, palette: &[PaletteRows]) -> Result<(), RendererError> {
        let Deformation::Skinned { palette_base, bones } =
            self.scene.deformation(h).map_err(RendererError::Scene)?
        else {
            return Err(RendererError::Scene(SceneError::DeformationKind));
        };
        let changed = self
            .shared
            .deform
            .write_palette(
                PaletteRange {
                    base: palette_base,
                    bones,
                },
                palette,
            )
            .map_err(RendererError::Deform)?;
        let _ = changed;
        Ok(())
    }

    /// Spawns an instance playing a vertex animation, `seconds` into it. The mesh must
    /// have the animation's vertex count.
    ///
    /// # Errors
    /// [`RendererError::Deform`] for an unknown animation, [`RendererError::Scene`].
    pub fn spawn_vat(
        &mut self,
        mesh: MeshId,
        material: MaterialId,
        model: glam::Mat4,
        vat: VatId,
        seconds: f32,
    ) -> Result<InstanceHandle, RendererError> {
        let info = *self
            .shared
            .deform
            .vat(vat)
            .ok_or(RendererError::Deform(DeformError::Stale))?;
        let deformation = Deformation::Vat {
            first_texel: info.first_texel,
            vertices: info.vertex_count,
            frames: info.frame_count,
            frame: info.frame_at(seconds),
        };
        self.scene
            .spawn_with(
                mesh,
                material,
                model,
                deformation,
                Some(info.bounds.bounding_sphere()),
            )
            .map_err(RendererError::Scene)
    }

    /// Moves a vertex-animated instance to `seconds` into `vat` (the animation it was
    /// spawned with). Allocation-free; a change counts as movement for TAA.
    ///
    /// # Errors
    /// [`RendererError::Deform`] for an unknown animation or one the instance does not
    /// play, [`RendererError::Scene`].
    pub fn set_vat_time(&mut self, h: InstanceHandle, vat: VatId, seconds: f32) -> Result<(), RendererError> {
        let info = *self
            .shared
            .deform
            .vat(vat)
            .ok_or(RendererError::Deform(DeformError::Stale))?;
        match self.scene.deformation(h).map_err(RendererError::Scene)? {
            Deformation::Vat {
                first_texel,
                vertices,
                frames,
                ..
            } if first_texel == info.first_texel => self
                .scene
                .set_deformation(
                    h,
                    Deformation::Vat {
                        first_texel,
                        vertices,
                        frames,
                        frame: info.frame_at(seconds),
                    },
                )
                .map_err(RendererError::Scene),
            _ => Err(RendererError::Deform(DeformError::Stale)),
        }
    }

    /// Removes an instance and releases its palette.
    ///
    /// # Errors
    /// [`RendererError::Scene`] for a stale handle.
    pub fn despawn(&mut self, h: InstanceHandle) -> Result<(), RendererError> {
        let deformation = self.scene.deformation(h).map_err(RendererError::Scene)?;
        self.scene.despawn(h).map_err(RendererError::Scene)?;
        if let Deformation::Skinned { palette_base, bones } = deformation {
            self.shared.deform.free_palette(PaletteRange {
                base: palette_base,
                bones,
            });
        }
        Ok(())
    }

    /// Loads a material; its handle doubles as the scene's material id.
    ///
    /// # Errors
    /// [`RendererError::Material`].
    pub fn add_material(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        asset: mantis_formats::material::MaterialAsset,
        textures: [Option<&wgpu::TextureView>; 4],
    ) -> Result<MaterialId, RendererError> {
        let h = self
            .shared
            .materials
            .add(device, queue, &self.shared.layouts, asset, textures)
            .map_err(RendererError::Material)?;
        Ok(MaterialId(h.index()))
    }

    /// Replaces material `material` in place with `asset` (hot reload): instances drawn
    /// with it keep their handle and draw with the new material from the next frame. A
    /// failing asset leaves the old material live.
    ///
    /// # Errors
    /// [`RendererError::Material`] (see `MaterialCache::replace`).
    pub fn replace_material(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        material: MaterialId,
        asset: mantis_formats::material::MaterialAsset,
        textures: [Option<&wgpu::TextureView>; 4],
    ) -> Result<(), RendererError> {
        self.shared
            .materials
            .replace(
                device,
                queue,
                &self.shared.layouts,
                MaterialHandle::from_index(material.0),
                asset,
                textures,
            )
            .map_err(RendererError::Material)
    }

    /// Unloads a material with no live instances (its empty batches are dropped); the id
    /// may be reused by a later [`Renderer::add_material`].
    ///
    /// # Errors
    /// [`RendererError::Scene`] ([`SceneError::InUse`]) or [`RendererError::Material`].
    pub fn remove_material(
        &mut self,
        device: &wgpu::Device,
        material: MaterialId,
    ) -> Result<(), RendererError> {
        self.scene
            .release_material(material)
            .map_err(RendererError::Scene)?;
        self.shared
            .materials
            .remove(
                device,
                &self.shared.layouts,
                MaterialHandle::from_index(material.0),
            )
            .map_err(RendererError::Material)
    }

    /// Unloads a mesh with no live instances: its vertex and index ranges return to the
    /// free lists; the id may be reused by a later [`Renderer::add_mesh`].
    ///
    /// # Errors
    /// [`RendererError::Scene`] ([`SceneError::InUse`] or unknown mesh).
    pub fn remove_mesh(&mut self, mesh: MeshId) -> Result<(), RendererError> {
        let range = self.scene.remove_mesh(mesh).map_err(RendererError::Scene)?;
        self.shared.meshes.remove(&range);
        Ok(())
    }

    /// Free vertices and indices in the shared mesh buffers.
    pub fn mesh_space(&self) -> (u32, u32) {
        self.shared.meshes.free()
    }

    /// Sets a material parameter by name (a scalar from `value[0]`, or a color), without
    /// rebuilding anything.
    ///
    /// # Errors
    /// [`RendererError::Material`] for an unknown material or parameter.
    pub fn set_material_param(
        &mut self,
        queue: &wgpu::Queue,
        material: MaterialId,
        name: &str,
        value: [f32; 4],
    ) -> Result<(), RendererError> {
        self.shared
            .materials
            .set_param(queue, MaterialHandle::from_index(material.0), name, value)
            .map_err(RendererError::Material)
    }

    /// The streamed world's indirect light: load and unload sector lightmaps and probe
    /// volumes, and set the time of day.
    pub fn world_light_mut(&mut self) -> &mut WorldLight {
        &mut self.shared.world_light
    }

    /// The streamed world's indirect light.
    pub fn world_light(&self) -> &WorldLight {
        &self.shared.world_light
    }

    /// Adds a static instance lit by a resident lightmap page; `uv_scale` and `uv_offset`
    /// place the mesh's second uv set in the sector lightmap (a sector placement's
    /// rectangle).
    ///
    /// # Errors
    /// [`RendererError::WorldLight`] when the page is not resident;
    /// [`RendererError::Scene`] as [`Scene::spawn`].
    pub fn spawn_lightmapped(
        &mut self,
        mesh: MeshId,
        material: MaterialId,
        model: glam::Mat4,
        page: LightmapPage,
        uv_scale: [f32; 2],
        uv_offset: [f32; 2],
    ) -> Result<InstanceHandle, RendererError> {
        let rect = self
            .shared
            .world_light
            .page_rect(page, uv_scale, uv_offset)
            .map_err(RendererError::WorldLight)?;
        self.scene
            .spawn_lightmapped(mesh, material, model, LightmapBinding { page: page.0, rect })
            .map_err(RendererError::Scene)
    }

    /// Replaces this frame's point lights (copied into preallocated storage; lights past
    /// the cluster configuration's capacity are dropped and counted by clustering).
    pub fn set_lights(&mut self, lights: &[PointLight]) {
        self.lights.clear();
        let room = self.lights.capacity();
        self.lights.extend(lights.iter().take(room).copied());
    }

    /// Statistics of the last prepared frame.
    pub fn stats(&self) -> FrameStats {
        self.stats
    }

    /// The velocity target of the last encoded frame: per pixel, the screen-space motion
    /// of the surface since the previous frame in uv units (current minus previous,
    /// unjittered), [`NO_VELOCITY`] where nothing was drawn. Copyable, for tests and tools.
    pub fn velocity_texture(&self) -> &wgpu::Texture {
        &self.shared.velocity.0
    }

    /// The configuration.
    pub fn config(&self) -> &RendererConfig {
        &self.config
    }

    /// CPU frame preparation. Allocation-free after the first frame.
    #[allow(clippy::cast_precision_loss)] // Pixel sizes are far below 2^24.
    pub fn prepare(&mut self, inputs: &FrameInputs) -> FrameStats {
        let c = &self.config;
        let (w, h) = (c.width as f32, c.height.max(1) as f32);
        let camera = inputs.camera;
        let post = inputs.post;
        let view = camera.view();
        let proj = camera.projection();
        let view_proj = proj * view;
        let prev_view_proj = self.prev_view_proj.replace(view_proj).unwrap_or(view_proj);
        // The camera renders jittered for TAA; culling and cascades stay unjittered.
        let rendered = self.shared.post.frame_projection(proj, &post) * view;
        // Scene batches and instances.
        let instances = self.scene.prepare(&mut self.prep);
        self.shared.draws.clear();
        self.shared.draws.extend_from_slice(&self.prep.draws);
        self.shared.instance_count = instances;
        self.draw_params.clear();
        self.draw_params
            .extend(self.prep.batches.iter().map(|b| GpuDrawParams {
                base: b.base,
                ..bytemuck::Zeroable::zeroed()
            }));
        // Lights.
        let light_stats = self.clusters.assign(&view, &self.lights);
        // Cascades.
        let cascades = fit_cascades(&camera, inputs.sun_direction, &c.cascades);
        let cascade_count = if inputs.shadows {
            c.cascades.count.min(MAX_CASCADES)
        } else {
            0
        };
        self.shared.cascade_count = u32::try_from(cascade_count).unwrap_or(0);
        // Views: camera then cascades.
        let mut view_projs = [view_proj; VIEWS];
        for (slot, cascade) in view_projs.iter_mut().skip(1).zip(&cascades) {
            *slot = cascade.view_proj;
        }
        for (i, vp) in view_projs.iter().enumerate() {
            let frustum = Frustum::from_view_projection(vp);
            if let Some(u) = self.cull_uniforms.get_mut(i) {
                *u = CullUniform {
                    planes: frustum.gpu_planes(),
                    instance_count: instances,
                    pad: [0; 3],
                };
            }
            if let Some(pv) = self.pass_views.get_mut(i) {
                pv.view_proj = if i == 0 { rendered } else { *vp }.to_cols_array_2d();
                pv.position = [camera.position.x, camera.position.y, camera.position.z, 0.0];
            }
        }
        // Frame uniform.
        let f = &mut self.gpu_frame;
        f.view = view.to_cols_array_2d();
        f.proj = proj.to_cols_array_2d();
        f.view_proj = view_proj.to_cols_array_2d();
        f.inv_view_proj = view_proj.inverse().to_cols_array_2d();
        f.prev_view_proj = prev_view_proj.to_cols_array_2d();
        f.camera_position = [
            camera.position.x,
            camera.position.y,
            camera.position.z,
            inputs.time,
        ];
        f.viewport = [w, h, 1.0 / w, 1.0 / h];
        let sun = inputs.sun_direction.try_normalize().unwrap_or(Vec3::NEG_Y);
        f.sun_direction = [sun.x, sun.y, sun.z, 0.0];
        f.sun_color = [
            inputs.sun_color.x,
            inputs.sun_color.y,
            inputs.sun_color.z,
            if cascade_count > 0 { 1.0 } else { 0.0 },
        ];
        f.sky_sh = inputs.sky.rgb;
        fill_cascades(f, &cascades, cascade_count);
        let gp = self.clusters.gpu_params(c.width, c.height);
        f.cluster_dims = gp.dims;
        f.cluster_depth = gp.depth;
        f.cluster_tile = gp.tile;
        (f.probe_grid, f.probe_atlas) = self.shared.world_light.frame_params(inputs.ambient_intensity);
        f.indirect_params = [0.0; 4];
        self.step_particles(inputs.time, rendered, &view);
        // SSAO and post.
        self.ssao_params = SsaoParams::new(proj, [w, h], inputs.ssao_strength);
        self.shared.post.prepare(view_proj, &post, inputs.exposure);
        let [red, green, blue, alpha] = inputs.clear_color;
        self.shared.clear_color = wgpu::Color {
            r: red,
            g: green,
            b: blue,
            a: alpha,
        };
        self.stats = FrameStats {
            instances,
            batches: u32::try_from(self.prep.batches.len()).unwrap_or(u32::MAX),
            lights: light_stats.lights,
            cascades: self.shared.cascade_count,
            particle_emitters: self.shared.particles.live_emitters(),
        };
        self.stats
    }

    /// Advances the particle system with the frame's time step, drawn with the camera's
    /// rendered (possibly jittered) matrix so billboards match the depth prepass.
    fn step_particles(&mut self, time: f32, rendered: glam::Mat4, view: &glam::Mat4) {
        let dt = self.last_time.map_or(0.0, |t| (time - t).clamp(0.0, 0.25));
        self.last_time = Some(time);
        let _ = self
            .shared
            .particles
            .prepare(dt, rendered, view.row(0).truncate(), view.row(1).truncate());
    }

    /// Writes the occlusion cull's per-frame data (zeroed late-list args and the uniform)
    /// and the light clusters.
    fn upload_occlusion(&self, queue: &wgpu::Queue) {
        let o = &self.shared.occlusion;
        if !self.prep.args.is_empty() {
            queue.write_buffer(&o.late_args, 0, bytemuck::cast_slice(&self.prep.args));
        }
        let camera = self
            .cull_uniforms
            .first()
            .copied()
            .unwrap_or_else(bytemuck::Zeroable::zeroed);
        let uniform = OcclusionUniform {
            planes: camera.planes,
            view_proj: self.gpu_frame.view_proj,
            pyramid: o.pyramid.params(),
            instance_count: camera.instance_count,
            pad: [0; 3],
        };
        queue.write_buffer(&o.uniform, 0, bytemuck::bytes_of(&uniform));
        let s = &self.shared;
        if !self.clusters.grid().is_empty() {
            queue.write_buffer(&s.cluster_grid, 0, bytemuck::cast_slice(self.clusters.grid()));
        }
        if !self.clusters.indices().is_empty() {
            queue.write_buffer(
                &s.cluster_indices,
                0,
                bytemuck::cast_slice(self.clusters.indices()),
            );
        }
        if !self.clusters.lights().is_empty() {
            queue.write_buffer(&s.cluster_lights, 0, bytemuck::cast_slice(self.clusters.lights()));
        }
    }

    /// Uploads the prepared frame and records the graph into `encoder`, rendering into
    /// `output` (a texture of the configured output format with `RENDER_ATTACHMENT`).
    ///
    /// # Errors
    /// [`RendererError::Graph`] if the output import is unusable.
    pub fn encode(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        output: (&wgpu::Texture, &wgpu::TextureView),
    ) -> Result<(), RendererError> {
        self.shared.deform.upload(queue);
        self.shared.world_light.flush(queue);
        let s = &self.shared;
        queue.write_buffer(&s.frame, 0, bytemuck::bytes_of(&self.gpu_frame));
        for (i, pv) in self.pass_views.iter().enumerate() {
            queue.write_buffer(&s.pass_views, i as u64 * DYNAMIC_STRIDE, bytemuck::bytes_of(pv));
        }
        if !self.draw_params.is_empty() {
            queue.write_buffer(&s.draw_params, 0, bytemuck::cast_slice(&self.draw_params));
        }
        if !self.prep.instances.is_empty() {
            queue.write_buffer(&s.instances, 0, bytemuck::cast_slice(&self.prep.instances));
            queue.write_buffer(
                &s.previous_models,
                0,
                bytemuck::cast_slice(&self.prep.previous_models),
            );
        }
        if !self.prep.batches.is_empty() {
            queue.write_buffer(&s.batches, 0, bytemuck::cast_slice(&self.prep.batches));
        }
        self.upload_occlusion(queue);
        for (view, u) in s.views.iter().zip(&self.cull_uniforms) {
            if !self.prep.args.is_empty() {
                queue.write_buffer(&view.args, 0, bytemuck::cast_slice(&self.prep.args));
            }
            queue.write_buffer(&view.cull, 0, bytemuck::bytes_of(u));
        }
        queue.write_buffer(&s.ssao_params, 0, bytemuck::bytes_of(&self.ssao_params));
        s.post.upload(queue);
        s.particles.upload(queue);
        let (prev, next) = s.post.history_imports();
        let imported = [
            ImportedTexture {
                resource: self.output.resource(),
                texture: output.0,
                view: output.1,
            },
            ImportedTexture {
                resource: self.history_prev.resource(),
                texture: &prev.0,
                view: &prev.1,
            },
            ImportedTexture {
                resource: self.history_next.resource(),
                texture: &next.0,
                view: &next.1,
            },
            ImportedTexture {
                resource: self.velocity.resource(),
                texture: &s.velocity.0,
                view: &s.velocity.1,
            },
            ImportedTexture {
                resource: self.pyramid.resource(),
                texture: s.occlusion.pyramid.texture(),
                view: s.occlusion.pyramid.view(),
            },
        ];
        // The draw lists are one logical resource (every view's indirect args and visible
        // lists); the camera view's args buffer stands for it in the graph's usage check.
        let lists = self.shared.views.first().map(|v| ImportedBuffer {
            resource: self.draw_lists.resource(),
            buffer: &v.args,
        });
        let pool = ImportedBuffer {
            resource: self.particle_pool.resource(),
            buffer: s.particles.particle_buffer(),
        };
        let both;
        let buffers: &[ImportedBuffer<'_>] = match lists {
            Some(l) => {
                both = [l, pool];
                &both
            }
            None => core::slice::from_ref(&pool),
        };
        self.graph
            .execute(
                device,
                queue,
                encoder,
                &Imports {
                    textures: &imported,
                    buffers,
                },
                &self.shared,
            )
            .map_err(|e| RendererError::Graph(e.to_string()))?;
        self.shared.post.finish_frame();
        Ok(())
    }

    /// Uploads a new color grading (baked into the lookup table).
    pub fn set_grading(&self, queue: &wgpu::Queue, grading: &mantis_formats::color_grading::ColorGrading) {
        self.shared.post.set_grading(queue, grading);
    }

    /// Forgets the TAA history (camera cuts and teleports).
    pub fn reset_history(&mut self) {
        self.shared.post.reset_history();
    }

    /// Hands this frame's UI to the renderer: the draw list's quads and the glyph atlas
    /// (its dirty region uploads now; the texture is recreated when the atlas grows).
    /// Without a call the previous frame's UI is drawn again.
    pub fn set_ui(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        quads: &[mantis_ui::draw::UiQuad],
        atlas: &mut mantis_ui::atlas::GlyphAtlas,
    ) {
        let target = [self.config.width, self.config.height];
        self.shared.ui.prepare(device, queue, quads, atlas, target);
    }

    /// UI pass counters.
    pub fn ui_stats(&self) -> crate::ui_pass::UiStats {
        self.shared.ui.stats()
    }

    /// The particle system (register effects, spawn, move, stop, and kill instances).
    pub fn particles_mut(&mut self) -> &mut ParticleSystem {
        &mut self.shared.particles
    }

    /// The particle system.
    pub fn particles(&self) -> &ParticleSystem {
        &self.shared.particles
    }

    /// The material cache (for tests and tools).
    pub fn materials(&self) -> &MaterialCache {
        &self.shared.materials
    }

    /// The camera view's indirect argument buffer (for tests and diagnostics).
    pub fn camera_args(&self) -> Option<&wgpu::Buffer> {
        self.shared.views.first().map(|v| &v.args)
    }

    /// The camera's occlusion late-list indirect args (instances drawn because the late
    /// cull found them visible though they were not visible last frame).
    pub fn camera_late_args(&self) -> &wgpu::Buffer {
        &self.shared.occlusion.late_args
    }
}

/// Writes the cascade matrices, splits, and parameters into the frame uniform.
#[allow(clippy::cast_precision_loss)] // At most four cascades.
fn fill_cascades(f: &mut GpuFrame, cascades: &[crate::lighting::csm::Cascade], count: usize) {
    for (slot, cascade) in f.cascade_view_proj.iter_mut().zip(cascades) {
        *slot = cascade.view_proj.to_cols_array_2d();
    }
    for (slot, cascade) in f.cascade_splits.iter_mut().zip(cascades) {
        *slot = cascade.split_far;
    }
    let texel = cascades.first().map_or(0.0, |c| c.texel_world);
    f.cascade_params = [count as f32, 0.0005, texel * 1.5, 1.0];
}

/// Builds and compiles the frame graph.
#[allow(clippy::too_many_lines, clippy::many_single_char_names)] // One block per pass, in frame order; `p` is each pass builder.
fn build_graph(
    device: &wgpu::Device,
    config: &RendererConfig,
) -> Result<(RenderGraph<Shared>, GraphHandles), RendererError> {
    let (w, h) = (config.width.max(1), config.height.max(1));
    let mut g = GraphBuilder::new();
    let output = g.import_texture("output", TextureDesc::d2(w, h, config.output_format));
    let draw_lists = g.import_buffer("draw_lists", crate::graph::BufferDesc { size: 16 });
    let particle_pool = g.import_buffer("particle_pool", crate::graph::BufferDesc { size: 16 });
    let shadow = g.create_texture(
        "shadow_cascades",
        TextureDesc {
            depth_or_array_layers: u32::try_from(MAX_CASCADES).unwrap_or(4),
            ..TextureDesc::d2(config.shadow_resolution, config.shadow_resolution, SHADOW_FORMAT)
        },
    );
    let depth = g.create_texture("depth", TextureDesc::d2(w, h, DEPTH_FORMAT));
    let ssao = g.create_texture("ssao", TextureDesc::d2(w, h, wgpu::TextureFormat::R8Unorm));
    let hdr = g.create_texture("hdr", TextureDesc::d2(w, h, HDR_FORMAT));
    let history_prev = g.import_texture(
        "taa_history_prev",
        TextureDesc::d2(w, h, crate::post::POST_FORMAT),
    );
    let history_next = g.import_texture("taa_history", TextureDesc::d2(w, h, crate::post::POST_FORMAT));
    let velocity = g.import_texture("velocity", TextureDesc::d2(w, h, VELOCITY_FORMAT));
    let pyramid = g.import_texture(
        "depth_pyramid",
        TextureDesc {
            mip_level_count: crate::occlusion::pyramid_levels(w, h),
            ..TextureDesc::d2(w, h, crate::occlusion::PYRAMID_FORMAT)
        },
    );
    let bloom = g.create_texture("bloom", crate::post::bloom_desc(w, h));
    let mut passes: Vec<(crate::graph::PassId, Box<dyn GraphPass<Shared>>)> = Vec::new();

    let (id, lists) = {
        let mut p = g.add_pass("cull", PassKind::Compute);
        let lists = p.write_buffer(draw_lists, BufferAccess::StorageWrite, WriteMode::Load);
        (p.id(), lists)
    };
    passes.push((id, Box::new(CullPass)));
    let (id, pool1) = {
        let mut p = g.add_pass("particles_sim", PassKind::Compute);
        let b = p.write_buffer(particle_pool, BufferAccess::StorageWrite, WriteMode::Load);
        (p.id(), b)
    };
    passes.push((id, Box::new(ParticleSimPass)));
    let (id, shadow1) = {
        let mut p = g.add_pass("shadows", PassKind::Render);
        p.read_buffer(lists, BufferAccess::Indirect);
        let s = p.write_texture(shadow, TextureAccess::DepthWrite, WriteMode::Discard);
        (p.id(), s)
    };
    passes.push((id, Box::new(ShadowPass { shadow, layers: None })));
    let (id, depth1, velocity1) = {
        let mut p = g.add_pass("depth_prepass", PassKind::Render);
        p.read_buffer(lists, BufferAccess::Indirect);
        let d = p.write_texture(depth, TextureAccess::DepthWrite, WriteMode::Discard);
        let v = p.write_texture(velocity, TextureAccess::ColorTarget, WriteMode::Discard);
        (p.id(), d, v)
    };
    passes.push((
        id,
        Box::new(DrawPass {
            pass: Pass::Depth,
            color: Some(velocity),
            depth,
            list: List::Early,
        }),
    ));
    // Occlusion: the pyramid from the early depth, the late cull, the late prepass.
    let (id, pyramid1) = {
        let mut p = g.add_pass("depth_pyramid", PassKind::Compute);
        p.read_texture(depth1, TextureAccess::Sampled);
        let t = p.write_texture(pyramid, TextureAccess::StorageWrite, WriteMode::Discard);
        (p.id(), t)
    };
    passes.push((id, Box::new(PyramidPass)));
    let (id, lists) = {
        let mut p = g.add_pass("late_cull", PassKind::Compute);
        p.read_texture(pyramid1, TextureAccess::Sampled);
        let l = p.write_buffer(lists, BufferAccess::StorageWrite, WriteMode::Load);
        (p.id(), l)
    };
    passes.push((id, Box::new(LateCullPass)));
    let (id, depth1, velocity1) = {
        let mut p = g.add_pass("depth_prepass_late", PassKind::Render);
        p.read_buffer(lists, BufferAccess::Indirect);
        let d = p.write_texture(depth1, TextureAccess::DepthWrite, WriteMode::Load);
        let v = p.write_texture(velocity1, TextureAccess::ColorTarget, WriteMode::Load);
        (p.id(), d, v)
    };
    passes.push((
        id,
        Box::new(DrawPass {
            pass: Pass::Depth,
            color: Some(velocity),
            depth,
            list: List::Late,
        }),
    ));
    let (id, ssao1) = {
        let mut p = g.add_pass("ssao", PassKind::Render);
        p.read_texture(depth1, TextureAccess::Sampled);
        let a = p.write_texture(ssao, TextureAccess::ColorTarget, WriteMode::Discard);
        (p.id(), a)
    };
    passes.push((id, Box::new(SsaoPass { target: ssao })));
    let (id, hdr1) = {
        let mut p = g.add_pass("forward", PassKind::Render);
        p.read_buffer(lists, BufferAccess::Indirect);
        p.read_texture(shadow1, TextureAccess::Sampled);
        p.read_texture(ssao1, TextureAccess::Sampled);
        p.read_texture(depth1, TextureAccess::DepthRead);
        let c = p.write_texture(hdr, TextureAccess::ColorTarget, WriteMode::Discard);
        (p.id(), c)
    };
    passes.push((
        id,
        Box::new(DrawPass {
            pass: Pass::Forward,
            color: Some(hdr),
            depth,
            list: List::Both,
        }),
    ));
    let (id, hdr2, depth2) = {
        let mut p = g.add_pass("outline", PassKind::Render);
        p.read_buffer(lists, BufferAccess::Indirect);
        let c = p.write_texture(hdr1, TextureAccess::ColorTarget, WriteMode::Load);
        let d = p.write_texture(depth1, TextureAccess::DepthWrite, WriteMode::Load);
        (p.id(), c, d)
    };
    passes.push((
        id,
        Box::new(DrawPass {
            pass: Pass::Outline,
            color: Some(hdr),
            depth,
            list: List::Both,
        }),
    ));
    let (id, hdr3) = {
        let mut p = g.add_pass("particles", PassKind::Render);
        p.read_buffer(pool1, BufferAccess::StorageRead);
        p.read_texture(depth2, TextureAccess::DepthRead);
        let c = p.write_texture(hdr2, TextureAccess::ColorTarget, WriteMode::Load);
        (p.id(), c)
    };
    passes.push((id, Box::new(ParticleDrawPass { color: hdr, depth })));
    let (id, resolved) = {
        let mut p = g.add_pass("taa", PassKind::Render);
        p.read_texture(hdr3, TextureAccess::Sampled);
        p.read_texture(depth2, TextureAccess::Sampled);
        p.read_texture(history_prev, TextureAccess::Sampled);
        p.read_texture(velocity1, TextureAccess::Sampled);
        let t = p.write_texture(history_next, TextureAccess::ColorTarget, WriteMode::Discard);
        (p.id(), t)
    };
    passes.push((id, Box::new(TaaPass { target: history_next })));
    let (id, bloom1) = {
        let mut p = g.add_pass("bloom", PassKind::Render);
        p.read_texture(resolved, TextureAccess::Sampled);
        let b = p.write_texture(bloom, TextureAccess::ColorTarget, WriteMode::Discard);
        (p.id(), b)
    };
    passes.push((id, Box::new(BloomPass)));
    let (id, out1) = {
        let mut p = g.add_pass("composite", PassKind::Render);
        p.read_texture(resolved, TextureAccess::Sampled);
        p.read_texture(bloom1, TextureAccess::Sampled);
        let o = p.write_texture(output, TextureAccess::ColorTarget, WriteMode::Discard);
        (p.id(), o)
    };
    passes.push((id, Box::new(CompositePass { output })));
    let (id, out2) = {
        let mut p = g.add_pass("ui", PassKind::Render);
        let o = p.write_texture(out1, TextureAccess::ColorTarget, WriteMode::Load);
        (p.id(), o)
    };
    passes.push((id, Box::new(UiPass { output })));
    g.output(out2);
    let compiled = g.compile().map_err(|e| RendererError::Graph(e.to_string()))?;
    let graph =
        RenderGraph::new(device, compiled, passes).map_err(|e| RendererError::Graph(e.to_string()))?;
    Ok((
        graph,
        GraphHandles {
            output,
            particle_pool,
            history_prev,
            history_next,
            velocity,
            pyramid,
            bloom,
            draw_lists,
            depth,
            shadow,
            ssao,
            hdr,
        },
    ))
}

/// Builds the bind groups that reference graph textures (once per compiled graph).
#[allow(clippy::too_many_lines)] // Declarative bind group tables, one entry per binding.
fn bind_graph_resources(
    device: &wgpu::Device,
    s: &mut Shared,
    graph: &RenderGraph<Shared>,
    h: &GraphHandles,
) -> Result<(), RendererError> {
    let missing = |what: &str| RendererError::Graph(format!("{what} has no physical texture"));
    let (shadow_tex, _) = graph
        .transient_texture(h.shadow)
        .ok_or_else(|| missing("shadow"))?;
    let shadow_view = shadow_tex.create_view(&wgpu::TextureViewDescriptor {
        dimension: Some(wgpu::TextureViewDimension::D2Array),
        ..Default::default()
    });
    let (_, ssao_view) = graph.transient_texture(h.ssao).ok_or_else(|| missing("ssao"))?;
    let (_, depth_view) = graph.transient_texture(h.depth).ok_or_else(|| missing("depth"))?;
    s.occlusion
        .culler
        .bind_depth(device, &mut s.occlusion.pyramid, depth_view);
    let pass_view = wgpu::BindingResource::Buffer(wgpu::BufferBinding {
        buffer: &s.pass_views,
        offset: 0,
        size: core::num::NonZeroU64::new(DYNAMIC_STRIDE),
    });
    let probe_views = s.world_light.probe_views();
    s.frame_forward_bind = Some(device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("frame.forward"),
        layout: &s.layouts.frame_forward,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: s.frame.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: pass_view,
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::TextureView(&shadow_view),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: wgpu::BindingResource::Sampler(&s.shadow_sampler),
            },
            wgpu::BindGroupEntry {
                binding: 4,
                resource: s.cluster_grid.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 5,
                resource: s.cluster_indices.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 6,
                resource: s.cluster_lights.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 7,
                resource: wgpu::BindingResource::TextureView(&probe_views[0]),
            },
            wgpu::BindGroupEntry {
                binding: 8,
                resource: wgpu::BindingResource::TextureView(&probe_views[1]),
            },
            wgpu::BindGroupEntry {
                binding: 9,
                resource: wgpu::BindingResource::TextureView(&probe_views[2]),
            },
            wgpu::BindGroupEntry {
                binding: 10,
                resource: wgpu::BindingResource::Sampler(&s.linear_clamp),
            },
            wgpu::BindGroupEntry {
                binding: 11,
                resource: wgpu::BindingResource::TextureView(s.world_light.page_view()),
            },
            wgpu::BindGroupEntry {
                binding: mantis_shadergen::layout::SSAO_BINDING,
                resource: wgpu::BindingResource::TextureView(ssao_view),
            },
            wgpu::BindGroupEntry {
                binding: mantis_shadergen::layout::LIGHTMAP_PAGES_BINDING,
                resource: s.world_light.page_table().as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: mantis_shadergen::layout::PROBE_SECTORS_BINDING,
                resource: s.world_light.probe_table().as_entire_binding(),
            },
        ],
    }));
    s.ssao_bind = Some(device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("ssao"),
        layout: &s.ssao_layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: s.ssao_params.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(depth_view),
            },
        ],
    }));
    s.post
        .bind(device, graph, (h.hdr, h.depth, h.bloom), &s.velocity.1)
        .map_err(RendererError::Graph)?;
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Passes
// ---------------------------------------------------------------------------------------

struct CullPass;

impl GraphPass<Shared> for CullPass {
    fn execute(&mut self, ctx: &mut PassContext<'_>, s: &Shared) {
        let views = 1 + s.cascade_count as usize;
        for (i, view) in s.views.iter().take(views).enumerate() {
            if i == 0 && s.occlusion.enabled {
                s.occlusion
                    .culler
                    .record_early(ctx.encoder, &s.occlusion.early_bind, s.instance_count);
            } else {
                s.culler.record(ctx.encoder, &view.cull_bind, s.instance_count);
            }
        }
    }
}

/// Builds the depth pyramid from the early prepass's depth.
struct PyramidPass;

impl GraphPass<Shared> for PyramidPass {
    fn execute(&mut self, ctx: &mut PassContext<'_>, s: &Shared) {
        if s.occlusion.enabled {
            s.occlusion
                .culler
                .record_pyramid(ctx.encoder, &s.occlusion.pyramid);
        }
    }
}

/// Tests every instance against the pyramid: next frame's visibility, and the late list.
struct LateCullPass;

impl GraphPass<Shared> for LateCullPass {
    fn execute(&mut self, ctx: &mut PassContext<'_>, s: &Shared) {
        if s.occlusion.enabled {
            s.occlusion
                .culler
                .record_late(ctx.encoder, &s.occlusion.late_bind, s.instance_count);
        }
    }
}

/// Which of the camera's visible lists a draw pass draws.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum List {
    /// The early list (every list of the shadow views).
    Early,
    /// The occlusion late list.
    Late,
    /// Both.
    Both,
}

/// Records one view's batches with the given pass's pipelines.
fn draw_batches(
    rp: &mut wgpu::RenderPass<'_>,
    s: &Shared,
    pass: Pass,
    view: usize,
    frame_bind: &wgpu::BindGroup,
    list: List,
) {
    let Some(v) = s.views.get(view) else { return };
    let late = view == 0 && s.occlusion.enabled;
    let lists = [
        (list != List::Late).then_some((&v.scene_bind, &v.args)),
        (late && list != List::Early).then_some((&s.occlusion.late_scene_bind, &s.occlusion.late_args)),
    ];
    for (scene_bind, args) in lists.into_iter().flatten() {
        draw_list(rp, s, pass, view, frame_bind, scene_bind, args);
    }
}

/// Records every batch of one visible list.
fn draw_list(
    rp: &mut wgpu::RenderPass<'_>,
    s: &Shared,
    pass: Pass,
    view: usize,
    frame_bind: &wgpu::BindGroup,
    scene_bind: &wgpu::BindGroup,
    args: &wgpu::Buffer,
) {
    let view_offset = u32::try_from(view as u64 * DYNAMIC_STRIDE).unwrap_or(0);
    rp.set_vertex_buffer(0, s.meshes.vertices.slice(..));
    rp.set_vertex_buffer(1, s.meshes.skin.slice(..));
    rp.set_index_buffer(s.meshes.indices.slice(..), wgpu::IndexFormat::Uint32);
    for draw in &s.draws {
        let handle = MaterialHandle::from_index(draw.material.0);
        let pipeline = match draw.deform {
            Deform::Static => s.materials.request(handle, pass, draw.lightmapped),
            deform => s.materials.request_deformed(handle, pass, deform),
        };
        let (Some(pipeline), Some(material)) = (pipeline, s.materials.bind_group_of(handle)) else {
            continue;
        };
        let draw_offset = u32::try_from(u64::from(draw.batch) * DYNAMIC_STRIDE).unwrap_or(0);
        rp.set_pipeline(pipeline);
        rp.set_bind_group(mantis_shadergen::layout::GROUP_FRAME, frame_bind, &[view_offset]);
        rp.set_bind_group(mantis_shadergen::layout::GROUP_SCENE, scene_bind, &[draw_offset]);
        rp.set_bind_group(mantis_shadergen::layout::GROUP_MATERIAL, material, &[]);
        if let Some(table) = s.materials.texture_table() {
            rp.set_bind_group(mantis_shadergen::layout::GROUP_TEXTURES, table, &[]);
        }
        rp.draw_indexed_indirect(
            args,
            u64::from(draw.batch) * core::mem::size_of::<DrawIndexedIndirect>() as u64,
        );
    }
}

struct ShadowPass {
    shadow: TextureHandle,
    layers: Option<Vec<wgpu::TextureView>>,
}

impl GraphPass<Shared> for ShadowPass {
    fn execute(&mut self, ctx: &mut PassContext<'_>, s: &Shared) {
        if self.layers.is_none() {
            let Some(texture) = ctx.texture(self.shadow) else {
                return;
            };
            let views = (0..u32::try_from(MAX_CASCADES).unwrap_or(4))
                .map(|layer| {
                    texture.create_view(&wgpu::TextureViewDescriptor {
                        dimension: Some(wgpu::TextureViewDimension::D2),
                        base_array_layer: layer,
                        array_layer_count: Some(1),
                        ..Default::default()
                    })
                })
                .collect();
            self.layers = Some(views);
        }
        let Some(layers) = &self.layers else { return };
        // Every layer is cleared, so unused cascades never hold stale depth.
        for (c, layer) in layers.iter().enumerate() {
            let mut rp = ctx.encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("shadow_cascade"),
                color_attachments: &[],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: layer,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                ..Default::default()
            });
            if u32::try_from(c).is_ok_and(|c| c < s.cascade_count) {
                draw_batches(&mut rp, s, Pass::Shadow, 1 + c, &s.frame_basic_bind, List::Early);
            }
        }
    }
}

struct DrawPass {
    pass: Pass,
    color: Option<TextureHandle>,
    depth: TextureHandle,
    list: List,
}

impl GraphPass<Shared> for DrawPass {
    fn execute(&mut self, ctx: &mut PassContext<'_>, s: &Shared) {
        let Some(depth) = ctx.texture_view(self.depth).cloned() else {
            return;
        };
        let color = self.color.and_then(|c| ctx.texture_view(c).cloned());
        let (color_load, depth_load) = match self.pass {
            Pass::Forward => (wgpu::LoadOp::Clear(s.clear_color), wgpu::LoadOp::Load),
            Pass::Depth if self.list == List::Late => (wgpu::LoadOp::Load, wgpu::LoadOp::Load),
            Pass::Depth => (
                wgpu::LoadOp::Clear(wgpu::Color {
                    r: NO_VELOCITY,
                    g: NO_VELOCITY,
                    b: 0.0,
                    a: 0.0,
                }),
                wgpu::LoadOp::Clear(0.0),
            ),
            Pass::Outline | Pass::Shadow => (wgpu::LoadOp::Load, wgpu::LoadOp::Load),
        };
        let attachments = [color.as_ref().map(|view| wgpu::RenderPassColorAttachment {
            view,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load: color_load,
                store: wgpu::StoreOp::Store,
            },
        })];
        let mut rp = ctx.encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("draw"),
            color_attachments: if color.is_some() { &attachments } else { &[] },
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &depth,
                depth_ops: Some(wgpu::Operations {
                    load: depth_load,
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            ..Default::default()
        });
        let frame_bind = match self.pass {
            Pass::Forward => s.frame_forward_bind.as_ref(),
            Pass::Depth | Pass::Outline | Pass::Shadow => Some(&s.frame_basic_bind),
        };
        if let Some(bind) = frame_bind {
            draw_batches(&mut rp, s, self.pass, 0, bind, self.list);
        }
    }
}

struct SsaoPass {
    target: TextureHandle,
}

impl GraphPass<Shared> for SsaoPass {
    fn execute(&mut self, ctx: &mut PassContext<'_>, s: &Shared) {
        let Some(view) = ctx.texture_view(self.target).cloned() else {
            return;
        };
        let mut rp = ctx.encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("ssao"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::WHITE),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        if let Some(bind) = &s.ssao_bind {
            rp.set_pipeline(&s.ssao_pipeline);
            rp.set_bind_group(0, bind, &[]);
            rp.draw(0..3, 0..1);
        }
    }
}

struct UiPass {
    output: TextureHandle,
}

impl GraphPass<Shared> for UiPass {
    fn execute(&mut self, ctx: &mut PassContext<'_>, s: &Shared) {
        let Some(view) = ctx.texture_view(self.output).cloned() else {
            return;
        };
        let mut rp = ctx.encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("ui"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        s.ui.record(&mut rp);
    }
}

struct ParticleSimPass;

impl GraphPass<Shared> for ParticleSimPass {
    fn execute(&mut self, ctx: &mut PassContext<'_>, s: &Shared) {
        let mut pass = ctx.encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("particles_sim"),
            timestamp_writes: None,
        });
        s.particles.record_simulate(&mut pass);
    }
}

struct ParticleDrawPass {
    color: TextureHandle,
    depth: TextureHandle,
}

impl GraphPass<Shared> for ParticleDrawPass {
    fn execute(&mut self, ctx: &mut PassContext<'_>, s: &Shared) {
        let (Some(color), Some(depth)) = (
            ctx.texture_view(self.color).cloned(),
            ctx.texture_view(self.depth).cloned(),
        ) else {
            return;
        };
        let mut rp = ctx.encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("particles"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &color,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &depth,
                depth_ops: None,
                stencil_ops: None,
            }),
            ..Default::default()
        });
        s.particles.record_draw(&mut rp);
    }
}

impl HasPost for Shared {
    fn post(&self) -> &PostChain {
        &self.post
    }
}
