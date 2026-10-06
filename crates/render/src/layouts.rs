//! Bind group and pipeline layouts matching `mantis_shadergen`'s binding scheme.

use core::num::NonZeroU32;

use mantis_shadergen::layout;

use crate::gpu_types::{DYNAMIC_STRIDE, GpuFrame, GpuMaterialParams};

const VF: wgpu::ShaderStages = wgpu::ShaderStages::VERTEX_FRAGMENT;
const FRAG: wgpu::ShaderStages = wgpu::ShaderStages::FRAGMENT;

/// One bind group layout entry (never an array): the one constructor every pass of the
/// renderer builds its layouts with.
pub(crate) fn entry(
    binding: u32,
    visibility: wgpu::ShaderStages,
    ty: wgpu::BindingType,
) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty,
        count: None,
    }
}

fn uniform(binding: u32, size: u64, dynamic: bool) -> wgpu::BindGroupLayoutEntry {
    entry(
        binding,
        VF,
        wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: dynamic,
            min_binding_size: core::num::NonZeroU64::new(size),
        },
    )
}

fn storage(binding: u32, visibility: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
    entry(
        binding,
        visibility,
        wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only: true },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
    )
}

fn texture(
    binding: u32,
    dim: wgpu::TextureViewDimension,
    sample_type: wgpu::TextureSampleType,
) -> wgpu::BindGroupLayoutEntry {
    entry(
        binding,
        FRAG,
        wgpu::BindingType::Texture {
            sample_type,
            view_dimension: dim,
            multisampled: false,
        },
    )
}

fn sampler(binding: u32, kind: wgpu::SamplerBindingType) -> wgpu::BindGroupLayoutEntry {
    entry(binding, FRAG, wgpu::BindingType::Sampler(kind))
}

const FLOAT: wgpu::TextureSampleType = wgpu::TextureSampleType::Float { filterable: true };

/// Every layout the material pipelines use.
#[derive(Debug)]
pub struct Layouts {
    /// Group 0 for the forward pass: frame, pass view, lighting resources.
    pub frame_forward: wgpu::BindGroupLayout,
    /// Group 0 for depth, shadow, and outline passes: frame and pass view.
    pub frame_basic: wgpu::BindGroupLayout,
    /// Group 1: the GPU-driven scene.
    pub scene: wgpu::BindGroupLayout,
    /// Group 2: material parameters, sampler, and four fixed texture slots.
    pub material_fixed: wgpu::BindGroupLayout,
    /// Group 2 in bindless mode: parameters and sampler only.
    pub material_params: wgpu::BindGroupLayout,
    /// Group 3 in bindless mode: the global texture table (when the device supports it).
    pub texture_table: Option<wgpu::BindGroupLayout>,
    /// Forward pipeline layouts: fixed, bindless.
    pub forward: [Option<wgpu::PipelineLayout>; 2],
    /// Depth, shadow, and outline pipeline layouts: fixed, bindless.
    pub basic: [Option<wgpu::PipelineLayout>; 2],
}

impl Layouts {
    /// Creates the layouts; bindless ones only when `bindless` is supported.
    #[allow(clippy::too_many_lines)] // Declarative layout tables, one entry per binding.
    pub fn new(device: &wgpu::Device, bindless: bool) -> Self {
        let frame_size = core::mem::size_of::<GpuFrame>() as u64;
        let mk = |label: &str, entries: &[wgpu::BindGroupLayoutEntry]| {
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some(label),
                entries,
            })
        };
        let frame_basic = mk(
            "layout.frame_basic",
            &[uniform(0, frame_size, false), uniform(1, DYNAMIC_STRIDE, true)],
        );
        let frame_forward = mk(
            "layout.frame_forward",
            &[
                uniform(0, frame_size, false),
                uniform(1, DYNAMIC_STRIDE, true),
                texture(
                    2,
                    wgpu::TextureViewDimension::D2Array,
                    wgpu::TextureSampleType::Depth,
                ),
                sampler(3, wgpu::SamplerBindingType::Comparison),
                storage(4, FRAG),
                storage(5, FRAG),
                storage(6, FRAG),
                texture(7, wgpu::TextureViewDimension::D3, FLOAT),
                texture(8, wgpu::TextureViewDimension::D3, FLOAT),
                texture(9, wgpu::TextureViewDimension::D3, FLOAT),
                sampler(10, wgpu::SamplerBindingType::Filtering),
                texture(11, wgpu::TextureViewDimension::D2Array, FLOAT),
                texture(layout::SSAO_BINDING, wgpu::TextureViewDimension::D2, FLOAT),
                storage(layout::LIGHTMAP_PAGES_BINDING, FRAG),
                storage(layout::PROBE_SECTORS_BINDING, FRAG),
            ],
        );
        let scene = mk(
            "layout.scene",
            &[
                storage(0, wgpu::ShaderStages::VERTEX),
                storage(1, wgpu::ShaderStages::VERTEX),
                uniform(2, DYNAMIC_STRIDE, true),
                storage(layout::DEFORM_POOL_BINDING, wgpu::ShaderStages::VERTEX),
                storage(layout::PREVIOUS_MODELS_BINDING, wgpu::ShaderStages::VERTEX),
            ],
        );
        let params_size = core::mem::size_of::<GpuMaterialParams>() as u64;
        let first = layout::MATERIAL_TEXTURE_BINDING;
        let material_fixed = mk(
            "layout.material_fixed",
            &[
                uniform(0, params_size, false),
                sampler(1, wgpu::SamplerBindingType::Filtering),
                texture(first, wgpu::TextureViewDimension::D2, FLOAT),
                texture(first + 1, wgpu::TextureViewDimension::D2, FLOAT),
                texture(first + 2, wgpu::TextureViewDimension::D2, FLOAT),
                texture(first + 3, wgpu::TextureViewDimension::D2, FLOAT),
            ],
        );
        // Bindless: the material group holds only parameters and the sampler; the texture
        // table is its own global group (wgpu forbids a binding array next to a uniform).
        let material_params = mk(
            "layout.material_params",
            &[
                uniform(0, params_size, false),
                sampler(1, wgpu::SamplerBindingType::Filtering),
            ],
        );
        let texture_table = bindless.then(|| {
            let mut array = texture(0, wgpu::TextureViewDimension::D2, FLOAT);
            array.count = NonZeroU32::new(layout::BINDLESS_TEXTURES);
            mk("layout.texture_table", &[array])
        });
        let pipeline = |label: &str, groups: &[Option<&wgpu::BindGroupLayout>]| {
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some(label),
                bind_group_layouts: groups,
                immediate_size: 0,
            })
        };
        let forward = [
            Some(pipeline(
                "pipeline.forward",
                &[Some(&frame_forward), Some(&scene), Some(&material_fixed)],
            )),
            texture_table.as_ref().map(|t| {
                pipeline(
                    "pipeline.forward.bindless",
                    &[
                        Some(&frame_forward),
                        Some(&scene),
                        Some(&material_params),
                        Some(t),
                    ],
                )
            }),
        ];
        let basic = [
            Some(pipeline(
                "pipeline.basic",
                &[Some(&frame_basic), Some(&scene), Some(&material_fixed)],
            )),
            texture_table.as_ref().map(|t| {
                pipeline(
                    "pipeline.basic.bindless",
                    &[Some(&frame_basic), Some(&scene), Some(&material_params), Some(t)],
                )
            }),
        ];
        Self {
            frame_forward,
            frame_basic,
            scene,
            material_fixed,
            material_params,
            texture_table,
            forward,
            basic,
        }
    }
}
