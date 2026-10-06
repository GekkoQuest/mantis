//! Post-processing (plan 8.3): temporal anti-aliasing with sharpening, bloom, ACES tone
//! mapping, and color grading from data.
//!
//! - **TAA**: the camera projection is jittered by a Halton (2, 3) sequence; each frame is
//!   blended into a history buffer reprojected through the velocity target the depth
//!   prepasses write (camera and object motion; texels with no velocity fall back to
//!   reprojection through depth), clamped to the current frame's 3x3 neighborhood in
//!   `YCoCg`. History ping-pongs between two persistent
//!   textures imported into the graph.
//! - **Bloom**: a soft-knee threshold, a downsample chain over a mip pyramid, and a tent
//!   upsample added back level by level.
//! - **Composite**: contrast-adaptive sharpening, bloom, exposure, ACES, then the grading
//!   lookup table baked from `mantis_formats::color_grading::ColorGrading`.

use bytemuck::{Pod, Zeroable};
use glam::{Mat4, Vec2};
use mantis_formats::color_grading::ColorGrading;
use mantis_formats::half::f32_to_f16;

use crate::gpu_types::DYNAMIC_STRIDE;
use crate::graph::TextureHandle;
use crate::graph::exec::{GraphPass, PassContext, RenderGraph};
use crate::layouts::entry;

const FRAGMENT: wgpu::ShaderStages = wgpu::ShaderStages::FRAGMENT;

/// Grading lookup table edge.
pub const LUT_SIZE: u32 = 32;
/// Bloom pyramid levels (starting at half resolution).
pub const BLOOM_MIPS: u32 = 5;
/// HDR format of the history and bloom textures.
pub const POST_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

/// Per-frame post settings.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct PostSettings {
    /// Temporal anti-aliasing (jitter and history).
    pub taa: bool,
    /// Weight of the current frame in the history blend (0.05 to 0.2 typical).
    pub taa_current_weight: f32,
    /// Sharpening, 0 to 1.
    pub sharpen: f32,
    /// Bloom added to the image, 0 disables.
    pub bloom_intensity: f32,
    /// Brightness where bloom starts.
    pub bloom_threshold: f32,
    /// Softness of the threshold, 0 to 1.
    pub bloom_knee: f32,
}

impl PostSettings {
    /// Everything off: the composite is tone mapping and grading only.
    pub const OFF: PostSettings = PostSettings {
        taa: false,
        taa_current_weight: 1.0,
        sharpen: 0.0,
        bloom_intensity: 0.0,
        bloom_threshold: 1.0,
        bloom_knee: 0.5,
    };
}

impl Default for PostSettings {
    fn default() -> Self {
        Self {
            taa: true,
            taa_current_weight: 0.1,
            sharpen: 0.3,
            bloom_intensity: 0.05,
            bloom_threshold: 1.0,
            bloom_knee: 0.5,
        }
    }
}

/// The radical inverse of `index` in `base`.
#[expect(clippy::cast_precision_loss)] // Small indices and bases.
pub fn halton(mut index: u32, base: u32) -> f32 {
    let mut f = 1.0f32;
    let mut r = 0.0f32;
    let b = base.max(2);
    while index > 0 {
        f /= b as f32;
        r += f * (index % b) as f32;
        index /= b;
    }
    r
}

/// Subpixel jitter for frame `frame`, in pixels within [-0.5, 0.5): an 8-frame Halton
/// (2, 3) cycle.
pub fn jitter(frame: u64) -> Vec2 {
    let i = u32::try_from(frame % 8).unwrap_or(0) + 1;
    Vec2::new(halton(i, 2) - 0.5, halton(i, 3) - 0.5)
}

/// `proj` shifted so geometry lands `jitter_px` pixels right and down on a
/// `width x height` target.
#[must_use]
#[expect(clippy::cast_precision_loss)] // Pixel sizes are far below 2^24.
pub fn jittered(proj: Mat4, jitter_px: Vec2, width: u32, height: u32) -> Mat4 {
    let jx = 2.0 * jitter_px.x / width.max(1) as f32;
    let jy = -2.0 * jitter_px.y / height.max(1) as f32;
    // clip.xy += jitter * clip.w, applied to every column.
    let mut m = proj;
    for col in [&mut m.x_axis, &mut m.y_axis, &mut m.z_axis, &mut m.w_axis] {
        col.x += jx * col.w;
        col.y += jy * col.w;
    }
    m
}

/// Bakes the grading table: `LUT_SIZE` cubed RGBA16F texels, red fastest.
#[expect(clippy::cast_precision_loss)] // LUT coordinates are tiny.
pub fn bake_lut(grading: &ColorGrading) -> Vec<[u16; 4]> {
    let n = LUT_SIZE;
    let scale = 1.0 / (n - 1) as f32;
    let mut out = Vec::with_capacity((n * n * n) as usize);
    for b in 0..n {
        for g in 0..n {
            for r in 0..n {
                let [cr, cg, cb] = grading.apply([r as f32 * scale, g as f32 * scale, b as f32 * scale]);
                out.push([f32_to_f16(cr), f32_to_f16(cg), f32_to_f16(cb), f32_to_f16(1.0)]);
            }
        }
    }
    out
}

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Debug, Pod, Zeroable)]
struct TaaParams {
    reproject: [[f32; 4]; 4],
    viewport: [f32; 4],
    settings: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Debug, Pod, Zeroable)]
struct BloomParams {
    settings: [f32; 4],
    texel: [f32; 4],
    pad: [[f32; 4]; 14],
}

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Debug, Pod, Zeroable)]
struct CompositeParams {
    settings: [f32; 4],
}

/// Something that owns a post chain (the renderer's shared pass state).
pub trait HasPost {
    /// The post chain.
    fn post(&self) -> &PostChain;
}

/// Post-processing GPU state.
#[derive(Debug)]
pub struct PostChain {
    width: u32,
    height: u32,
    taa_pipeline: wgpu::RenderPipeline,
    taa_layout: wgpu::BindGroupLayout,
    taa_params: wgpu::Buffer,
    taa_binds: Vec<wgpu::BindGroup>,
    down_pipeline: wgpu::RenderPipeline,
    up_pipeline: wgpu::RenderPipeline,
    bloom_layout: wgpu::BindGroupLayout,
    bloom_params: wgpu::Buffer,
    /// Downsample bind groups: index 0 for each history parity, then levels 1 onward.
    bloom_down_binds: Vec<wgpu::BindGroup>,
    bloom_up_binds: Vec<wgpu::BindGroup>,
    bloom_views: Vec<wgpu::TextureView>,
    composite_pipeline: wgpu::RenderPipeline,
    composite_layout: wgpu::BindGroupLayout,
    composite_params: wgpu::Buffer,
    composite_binds: Vec<wgpu::BindGroup>,
    lut: wgpu::Texture,
    lut_view: wgpu::TextureView,
    linear: wgpu::Sampler,
    history: [(wgpu::Texture, wgpu::TextureView); 2],
    /// Which history texture is "next" this frame.
    parity: usize,
    history_valid: bool,
    prev_view_proj: Mat4,
    frame: u64,
    taa_cpu: TaaParams,
    bloom_cpu: [BloomParams; BLOOM_MIPS as usize],
    composite_cpu: CompositeParams,
}

fn view(v: &wgpu::TextureView) -> wgpu::BindingResource<'_> {
    wgpu::BindingResource::TextureView(v)
}

/// The (previous, next) history pair at `parity`.
fn history_pair<T>(history: &[T; 2], parity: usize) -> (&T, &T) {
    let [a, b] = history;
    if parity == 0 { (b, a) } else { (a, b) }
}

fn tex(sample_type: wgpu::TextureSampleType, dim: wgpu::TextureViewDimension) -> wgpu::BindingType {
    wgpu::BindingType::Texture {
        sample_type,
        view_dimension: dim,
        multisampled: false,
    }
}

fn uniform(dynamic: bool) -> wgpu::BindingType {
    wgpu::BindingType::Buffer {
        ty: wgpu::BufferBindingType::Uniform,
        has_dynamic_offset: dynamic,
        min_binding_size: None,
    }
}

fn pipeline(
    device: &wgpu::Device,
    label: &str,
    source: &str,
    entry_point: &str,
    layout: &wgpu::BindGroupLayout,
    format: wgpu::TextureFormat,
    additive: bool,
) -> wgpu::RenderPipeline {
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(label),
        source: wgpu::ShaderSource::Wgsl(source.into()),
    });
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(label),
        bind_group_layouts: &[Some(layout)],
        immediate_size: 0,
    });
    let blend = additive.then_some(wgpu::BlendState {
        color: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::One,
            operation: wgpu::BlendOperation::Add,
        },
        alpha: wgpu::BlendComponent::REPLACE,
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(&layout),
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
            entry_point: Some(entry_point),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend,
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    })
}

/// Bloom pyramid description for a `width x height` output.
pub fn bloom_desc(width: u32, height: u32) -> crate::graph::TextureDesc {
    crate::graph::TextureDesc {
        mip_level_count: BLOOM_MIPS,
        ..crate::graph::TextureDesc::d2((width / 2).max(1), (height / 2).max(1), POST_FORMAT)
    }
}

impl PostChain {
    /// Creates every post pipeline and the persistent history and grading textures.
    #[expect(clippy::too_many_lines)] // One-time construction, in pass order.
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        width: u32,
        height: u32,
        output: wgpu::TextureFormat,
    ) -> Self {
        let float = wgpu::TextureSampleType::Float { filterable: true };
        let unfiltered = wgpu::TextureSampleType::Float { filterable: false };
        let d2 = wgpu::TextureViewDimension::D2;
        let filtering = wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering);
        let taa_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("post.taa"),
            entries: &[
                entry(0, FRAGMENT, uniform(false)),
                entry(1, FRAGMENT, tex(unfiltered, d2)),
                entry(2, FRAGMENT, tex(wgpu::TextureSampleType::Depth, d2)),
                entry(3, FRAGMENT, tex(float, d2)),
                entry(4, FRAGMENT, filtering),
                entry(5, FRAGMENT, tex(unfiltered, d2)),
            ],
        });
        let bloom_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("post.bloom"),
            entries: &[
                entry(0, FRAGMENT, uniform(true)),
                entry(1, FRAGMENT, tex(float, d2)),
                entry(2, FRAGMENT, filtering),
            ],
        });
        let composite_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("post.composite"),
            entries: &[
                entry(0, FRAGMENT, uniform(false)),
                entry(1, FRAGMENT, tex(unfiltered, d2)),
                entry(2, FRAGMENT, tex(float, d2)),
                entry(3, FRAGMENT, tex(float, wgpu::TextureViewDimension::D3)),
                entry(4, FRAGMENT, filtering),
            ],
        });
        let taa_src = include_str!("shaders/post_taa.wgsl");
        let bloom_src = include_str!("shaders/post_bloom.wgsl");
        let composite_src = include_str!("shaders/post_composite.wgsl");
        let taa_pipeline = pipeline(
            device,
            "post.taa",
            taa_src,
            "fs_taa",
            &taa_layout,
            POST_FORMAT,
            false,
        );
        let down_pipeline = pipeline(
            device,
            "post.bloom_down",
            bloom_src,
            "fs_bloom_down",
            &bloom_layout,
            POST_FORMAT,
            false,
        );
        let up_pipeline = pipeline(
            device,
            "post.bloom_up",
            bloom_src,
            "fs_bloom_up",
            &bloom_layout,
            POST_FORMAT,
            true,
        );
        let composite_pipeline = pipeline(
            device,
            "post.composite",
            composite_src,
            "fs_composite",
            &composite_layout,
            output,
            false,
        );
        let buffer = |label: &str, size: u64| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        };
        let lut = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("post.lut"),
            size: wgpu::Extent3d {
                width: LUT_SIZE,
                height: LUT_SIZE,
                depth_or_array_layers: LUT_SIZE,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D3,
            format: wgpu::TextureFormat::Rgba16Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let lut_view = lut.create_view(&wgpu::TextureViewDescriptor::default());
        let history_texture = |label: &str| {
            let t = device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: width.max(1),
                    height: height.max(1),
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: POST_FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            let v = t.create_view(&wgpu::TextureViewDescriptor::default());
            (t, v)
        };
        let linear = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("post.linear"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let chain = Self {
            width,
            height,
            taa_pipeline,
            taa_layout,
            taa_params: buffer("post.taa", core::mem::size_of::<TaaParams>() as u64),
            taa_binds: Vec::new(),
            down_pipeline,
            up_pipeline,
            bloom_layout,
            bloom_params: buffer("post.bloom", u64::from(BLOOM_MIPS) * DYNAMIC_STRIDE),
            bloom_down_binds: Vec::new(),
            bloom_up_binds: Vec::new(),
            bloom_views: Vec::new(),
            composite_pipeline,
            composite_layout,
            composite_params: buffer("post.composite", core::mem::size_of::<CompositeParams>() as u64),
            composite_binds: Vec::new(),
            lut,
            lut_view,
            linear,
            history: [
                history_texture("post.history.0"),
                history_texture("post.history.1"),
            ],
            parity: 0,
            history_valid: false,
            prev_view_proj: Mat4::IDENTITY,
            frame: 0,
            taa_cpu: Zeroable::zeroed(),
            bloom_cpu: [Zeroable::zeroed(); BLOOM_MIPS as usize],
            composite_cpu: Zeroable::zeroed(),
        };
        chain.set_grading(queue, &ColorGrading::NEUTRAL);
        chain
    }

    /// Bakes and uploads a grading table.
    pub fn set_grading(&self, queue: &wgpu::Queue, grading: &ColorGrading) {
        let texels = bake_lut(grading);
        queue.write_texture(
            self.lut.as_image_copy(),
            bytemuck::cast_slice(&texels),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(LUT_SIZE * 8),
                rows_per_image: Some(LUT_SIZE),
            },
            wgpu::Extent3d {
                width: LUT_SIZE,
                height: LUT_SIZE,
                depth_or_array_layers: LUT_SIZE,
            },
        );
    }

    /// Forgets the history (camera cut, teleport, resize).
    pub fn reset_history(&mut self) {
        self.history_valid = false;
    }

    /// Builds the bind groups that reference graph textures (once per compiled graph).
    ///
    /// # Errors
    /// A message naming a graph texture without a physical backing.
    #[expect(clippy::too_many_lines)] // Declarative bind group tables, one entry per binding.
    pub fn bind<F: ?Sized>(
        &mut self,
        device: &wgpu::Device,
        graph: &RenderGraph<F>,
        (hdr, depth, bloom): (TextureHandle, TextureHandle, TextureHandle),
        velocity: &wgpu::TextureView,
    ) -> Result<(), String> {
        let (_, hdr_view) = graph
            .transient_texture(hdr)
            .ok_or("hdr has no physical texture")?;
        let (_, depth_view) = graph
            .transient_texture(depth)
            .ok_or("depth has no physical texture")?;
        let (bloom_tex, _) = graph
            .transient_texture(bloom)
            .ok_or("bloom has no physical texture")?;
        self.bloom_views = (0..BLOOM_MIPS)
            .map(|mip| {
                bloom_tex.create_view(&wgpu::TextureViewDescriptor {
                    base_mip_level: mip,
                    mip_level_count: Some(1),
                    ..Default::default()
                })
            })
            .collect();
        let sampler = wgpu::BindingResource::Sampler(&self.linear);
        let bloom_param = wgpu::BindingResource::Buffer(wgpu::BufferBinding {
            buffer: &self.bloom_params,
            offset: 0,
            size: core::num::NonZeroU64::new(DYNAMIC_STRIDE),
        });
        let mut taa_binds = Vec::new();
        let mut down = Vec::new();
        let mut composite = Vec::new();
        for parity in 0..2 {
            // At `parity`, history[parity] is next and history[1 - parity] is previous.
            let (prev, next) = history_pair(&self.history, parity);
            let (prev, next) = (&prev.1, &next.1);
            taa_binds.push(device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("post.taa"),
                layout: &self.taa_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: self.taa_params.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: view(hdr_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: view(depth_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: view(prev),
                    },
                    wgpu::BindGroupEntry {
                        binding: 4,
                        resource: sampler.clone(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 5,
                        resource: view(velocity),
                    },
                ],
            }));
            down.push(device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("post.bloom_down0"),
                layout: &self.bloom_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: bloom_param.clone(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: view(next),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: sampler.clone(),
                    },
                ],
            }));
            let bloom0 = self.bloom_views.first().ok_or("bloom mip 0")?;
            composite.push(device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("post.composite"),
                layout: &self.composite_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: self.composite_params.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: view(next),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: view(bloom0),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: view(&self.lut_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 4,
                        resource: sampler.clone(),
                    },
                ],
            }));
        }
        // Downsample level i reads level i - 1; upsample into level i reads level i + 1.
        for source in self.bloom_views.iter().take(BLOOM_MIPS as usize - 1) {
            down.push(device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("post.bloom_down"),
                layout: &self.bloom_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: bloom_param.clone(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: view(source),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: sampler.clone(),
                    },
                ],
            }));
        }
        let up = self
            .bloom_views
            .iter()
            .skip(1)
            .map(|source| {
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("post.bloom_up"),
                    layout: &self.bloom_layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: bloom_param.clone(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: view(source),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: sampler.clone(),
                        },
                    ],
                })
            })
            .collect();
        self.taa_binds = taa_binds;
        self.bloom_down_binds = down;
        self.bloom_up_binds = up;
        self.composite_binds = composite;
        Ok(())
    }

    /// The camera projection to render with this frame (jittered when TAA is on).
    pub fn frame_projection(&self, proj: Mat4, settings: &PostSettings) -> Mat4 {
        if settings.taa {
            jittered(proj, jitter(self.frame), self.width, self.height)
        } else {
            proj
        }
    }

    /// Per-frame parameters. `view_proj` is the unjittered camera matrix. Allocation-free.
    #[expect(clippy::cast_precision_loss)] // Pixel sizes are far below 2^24.
    pub fn prepare(&mut self, view_proj: Mat4, settings: &PostSettings, exposure: f32) {
        let (w, h) = (self.width.max(1) as f32, self.height.max(1) as f32);
        let j = if settings.taa {
            jitter(self.frame)
        } else {
            Vec2::ZERO
        };
        let reproject = self.prev_view_proj * view_proj.inverse();
        let valid = settings.taa && self.history_valid && reproject.is_finite();
        self.taa_cpu = TaaParams {
            reproject: reproject.to_cols_array_2d(),
            viewport: [w, h, 1.0 / w, 1.0 / h],
            settings: [
                settings.taa_current_weight.clamp(0.01, 1.0),
                if valid { 1.0 } else { 0.0 },
                j.x,
                j.y,
            ],
        };
        let mip = |k: usize| {
            let shift = u32::try_from(k).unwrap_or(31).min(31);
            [
                ((self.width / 2) >> shift).max(1) as f32,
                ((self.height / 2) >> shift).max(1) as f32,
            ]
        };
        for (k, p) in self.bloom_cpu.iter_mut().enumerate() {
            let source = if k == 0 { [w, h] } else { mip(k - 1) };
            let target = mip(k);
            *p = BloomParams {
                settings: [
                    settings.bloom_threshold.max(0.0),
                    settings.bloom_knee.clamp(0.001, 1.0),
                    if k == 0 { 1.0 } else { 0.0 },
                    0.0,
                ],
                texel: [1.0 / source[0], 1.0 / source[1], 1.0 / target[0], 1.0 / target[1]],
                pad: [[0.0; 4]; 14],
            };
        }
        self.composite_cpu = CompositeParams {
            settings: [
                exposure,
                settings.bloom_intensity.max(0.0),
                settings.sharpen.clamp(0.0, 1.0),
                0.0,
            ],
        };
        self.prev_view_proj = view_proj;
    }

    /// Uploads this frame's parameters.
    pub fn upload(&self, queue: &wgpu::Queue) {
        queue.write_buffer(&self.taa_params, 0, bytemuck::bytes_of(&self.taa_cpu));
        queue.write_buffer(&self.bloom_params, 0, bytemuck::cast_slice(&self.bloom_cpu));
        queue.write_buffer(&self.composite_params, 0, bytemuck::bytes_of(&self.composite_cpu));
    }

    /// The history textures for this frame: (previous, next).
    pub fn history_imports(
        &self,
    ) -> (
        &(wgpu::Texture, wgpu::TextureView),
        &(wgpu::Texture, wgpu::TextureView),
    ) {
        history_pair(&self.history, self.parity)
    }

    /// Ends the frame: the history just written becomes the previous one.
    pub fn finish_frame(&mut self) {
        self.parity = 1 - self.parity;
        self.history_valid = true;
        self.frame = self.frame.wrapping_add(1);
    }
}

fn color_pass<'e>(
    encoder: &'e mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
    load: wgpu::LoadOp<wgpu::Color>,
) -> wgpu::RenderPass<'e> {
    encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("post"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load,
                store: wgpu::StoreOp::Store,
            },
        })],
        ..Default::default()
    })
}

/// TAA resolve into the next history texture.
#[derive(Debug)]
pub struct TaaPass {
    /// The next-history handle.
    pub target: TextureHandle,
}

impl<S: HasPost> GraphPass<S> for TaaPass {
    fn execute(&mut self, ctx: &mut PassContext<'_>, s: &S) {
        let post = s.post();
        let Some(view) = ctx.texture_view(self.target).cloned() else {
            return;
        };
        let Some(bind) = post.taa_binds.get(post.parity) else {
            return;
        };
        let mut rp = color_pass(ctx.encoder, &view, wgpu::LoadOp::Clear(wgpu::Color::BLACK));
        rp.set_pipeline(&post.taa_pipeline);
        rp.set_bind_group(0, bind, &[]);
        rp.draw(0..3, 0..1);
    }
}

/// Bloom pyramid: downsample chain then additive upsample chain.
#[derive(Debug)]
pub struct BloomPass;

impl<S: HasPost> GraphPass<S> for BloomPass {
    fn execute(&mut self, ctx: &mut PassContext<'_>, s: &S) {
        let post = s.post();
        let stride = u32::try_from(DYNAMIC_STRIDE).unwrap_or(256);
        for (level, target) in post.bloom_views.iter().enumerate() {
            let bind = if level == 0 {
                post.bloom_down_binds.get(post.parity)
            } else {
                post.bloom_down_binds.get(level + 1)
            };
            let Some(bind) = bind else { continue };
            let mut rp = color_pass(ctx.encoder, target, wgpu::LoadOp::Clear(wgpu::Color::BLACK));
            rp.set_pipeline(&post.down_pipeline);
            rp.set_bind_group(0, bind, &[u32::try_from(level).unwrap_or(0) * stride]);
            rp.draw(0..3, 0..1);
        }
        for level in (0..post.bloom_views.len().saturating_sub(1)).rev() {
            let (Some(target), Some(bind)) = (post.bloom_views.get(level), post.bloom_up_binds.get(level))
            else {
                continue;
            };
            let mut rp = color_pass(ctx.encoder, target, wgpu::LoadOp::Load);
            rp.set_pipeline(&post.up_pipeline);
            // The source is level + 1, whose texel size is in its own parameter slot.
            rp.set_bind_group(0, bind, &[u32::try_from(level + 1).unwrap_or(0) * stride]);
            rp.draw(0..3, 0..1);
        }
    }
}

/// Final composite into the output.
#[derive(Debug)]
pub struct CompositePass {
    /// The output handle.
    pub output: TextureHandle,
}

impl<S: HasPost> GraphPass<S> for CompositePass {
    fn execute(&mut self, ctx: &mut PassContext<'_>, s: &S) {
        let post = s.post();
        let Some(view) = ctx.texture_view(self.output).cloned() else {
            return;
        };
        let Some(bind) = post.composite_binds.get(post.parity) else {
            return;
        };
        let mut rp = color_pass(ctx.encoder, &view, wgpu::LoadOp::Clear(wgpu::Color::BLACK));
        rp.set_pipeline(&post.composite_pipeline);
        rp.set_bind_group(0, bind, &[]);
        rp.draw(0..3, 0..1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn halton_jitter_is_subpixel_and_balanced() {
        let mut sum = Vec2::ZERO;
        for f in 0..8 {
            let j = jitter(f);
            assert!(j.x >= -0.5 && j.x < 0.5 && j.y >= -0.5 && j.y < 0.5, "{j:?}");
            sum += j;
        }
        assert!(sum.length() / 8.0 < 0.1, "roughly centered: {sum:?}");
        assert_eq!(jitter(3), jitter(11), "8-frame cycle");
        assert!((halton(1, 2) - 0.5).abs() < 1e-6 && (halton(3, 3) - 1.0 / 9.0).abs() < 1e-6);
    }

    #[test]
    fn jittered_projection_shifts_by_whole_pixel_fractions() {
        let proj = glam::camera::rh::proj::directx::perspective_infinite_reverse(1.0, 2.0, 0.1);
        let j = jittered(proj, Vec2::new(0.5, 0.25), 200, 100);
        for p in [
            glam::Vec4::new(0.0, 0.0, -5.0, 1.0),
            glam::Vec4::new(3.0, -1.0, -50.0, 1.0),
        ] {
            let (a, b) = (proj * p, j * p);
            let dx = (b.x / b.w - a.x / a.w) * 200.0 / 2.0;
            let dy = -(b.y / b.w - a.y / a.w) * 100.0 / 2.0;
            assert!((dx - 0.5).abs() < 1e-3 && (dy - 0.25).abs() < 1e-3, "{dx} {dy}");
            assert!((b.z / b.w - a.z / a.w).abs() < 1e-6, "depth unchanged");
        }
    }

    #[test]
    fn neutral_lut_is_the_identity() {
        let lut = bake_lut(&ColorGrading::NEUTRAL);
        assert_eq!(lut.len(), 32 * 32 * 32);
        let at = |r: usize, g: usize, b: usize| lut.get(r + g * 32 + b * 32 * 32).copied().unwrap_or([0; 4]);
        let f = |h: u16| mantis_formats::half::f16_to_f32(h);
        let texel = at(31, 0, 16);
        assert!(
            (f(texel[0]) - 1.0).abs() < 1e-3
                && f(texel[1]).abs() < 1e-3
                && (f(texel[2]) - 16.0 / 31.0).abs() < 1e-3
        );
    }
}
