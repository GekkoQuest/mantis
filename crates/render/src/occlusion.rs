//! Two-phase GPU occlusion culling for the camera view (plan 8.3).
//!
//! 1. **Early cull** ([`OcclusionCuller::record_early`]): instances visible last frame
//!    (one bit each in the visibility buffer) and inside the frustum go to the early list.
//! 2. The depth prepass draws the early list; [`DepthPyramid`] is built from that depth
//!    (each level the farthest, minimum reverse-Z depth of the texels it covers).
//! 3. **Late cull** ([`OcclusionCuller::record_late`]): every instance inside the frustum
//!    is tested against the pyramid. The result becomes its bit for the next frame, and
//!    visible instances that were not in the early list go to the late list, which the
//!    prepass draws next.
//!
//! Every instance visible this frame is in one list or the other, so nothing pops in when
//! the camera moves or an occluder disappears; occlusion only removes work. Shaders:
//! `shaders/occlusion.wgsl`, `shaders/pyramid_base.wgsl`, `shaders/pyramid_reduce.wgsl`.

use bytemuck::{Pod, Zeroable};

use crate::layouts::entry;

const COMPUTE: wgpu::ShaderStages = wgpu::ShaderStages::COMPUTE;

/// The occlusion cull shader.
pub const OCCLUSION_WGSL: &str = include_str!("shaders/occlusion.wgsl");
/// The pyramid's first level from the depth buffer.
pub const PYRAMID_BASE_WGSL: &str = include_str!("shaders/pyramid_base.wgsl");
/// One pyramid level from the one above.
pub const PYRAMID_REDUCE_WGSL: &str = include_str!("shaders/pyramid_reduce.wgsl");
/// Pyramid texel format.
pub const PYRAMID_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::R32Float;

/// The occlusion cull uniform (192 bytes).
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Debug, Pod, Zeroable)]
pub struct OcclusionUniform {
    /// Camera frustum planes.
    pub planes: [[f32; 4]; 6],
    /// Unjittered camera world to clip.
    pub view_proj: [[f32; 4]; 4],
    /// Pyramid width, height, levels, and 1 when valid.
    pub pyramid: [f32; 4],
    /// Instances to test.
    pub instance_count: u32,
    /// Padding.
    pub pad: [u32; 3],
}

/// Levels of a full pyramid for `width x height`.
pub fn pyramid_levels(width: u32, height: u32) -> u32 {
    32 - width.max(height).max(1).leading_zeros()
}

/// The depth pyramid: an `R32Float` texture with a full mip chain, plus the bind groups
/// that build it from a depth view.
#[derive(Debug)]
pub struct DepthPyramid {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    width: u32,
    height: u32,
    levels: u32,
    base_bind: Option<wgpu::BindGroup>,
    reduce_binds: Vec<wgpu::BindGroup>,
}

impl DepthPyramid {
    /// The whole chain, for sampling.
    pub fn view(&self) -> &wgpu::TextureView {
        &self.view
    }

    /// The texture.
    pub fn texture(&self) -> &wgpu::Texture {
        &self.texture
    }

    /// `[width, height, levels, 1]` for [`OcclusionUniform::pyramid`].
    #[expect(clippy::cast_precision_loss)] // Pixel sizes are far below 2^24.
    pub fn params(&self) -> [f32; 4] {
        [self.width as f32, self.height as f32, self.levels as f32, 1.0]
    }
}

/// The occlusion pipelines.
#[derive(Debug)]
pub struct OcclusionCuller {
    early: wgpu::ComputePipeline,
    late: wgpu::ComputePipeline,
    cull_layout: wgpu::BindGroupLayout,
    base: wgpu::ComputePipeline,
    base_layout: wgpu::BindGroupLayout,
    reduce: wgpu::ComputePipeline,
    reduce_layout: wgpu::BindGroupLayout,
}

fn storage(read_only: bool) -> wgpu::BindingType {
    wgpu::BindingType::Buffer {
        ty: wgpu::BufferBindingType::Storage { read_only },
        has_dynamic_offset: false,
        min_binding_size: None,
    }
}

fn storage_texture() -> wgpu::BindingType {
    wgpu::BindingType::StorageTexture {
        access: wgpu::StorageTextureAccess::WriteOnly,
        format: PYRAMID_FORMAT,
        view_dimension: wgpu::TextureViewDimension::D2,
    }
}

fn float_texture() -> wgpu::BindingType {
    wgpu::BindingType::Texture {
        sample_type: wgpu::TextureSampleType::Float { filterable: false },
        view_dimension: wgpu::TextureViewDimension::D2,
        multisampled: false,
    }
}

fn buffer(binding: u32, b: &wgpu::Buffer) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry {
        binding,
        resource: b.as_entire_binding(),
    }
}

fn pipeline(
    device: &wgpu::Device,
    label: &str,
    source: &str,
    entry_point: &str,
    layout: &wgpu::BindGroupLayout,
) -> wgpu::ComputePipeline {
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(label),
        source: wgpu::ShaderSource::Wgsl(source.into()),
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(label),
        bind_group_layouts: &[Some(layout)],
        immediate_size: 0,
    });
    device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some(label),
        layout: Some(&pipeline_layout),
        module: &module,
        entry_point: Some(entry_point),
        compilation_options: wgpu::PipelineCompilationOptions::default(),
        cache: None,
    })
}

/// The buffers one occlusion cull reads and writes.
#[derive(Clone, Copy, Debug)]
pub struct CullBuffers<'a> {
    /// [`OcclusionUniform`].
    pub uniform: &'a wgpu::Buffer,
    /// Instances.
    pub instances: &'a wgpu::Buffer,
    /// Batches.
    pub batches: &'a wgpu::Buffer,
    /// Indirect args of the list this phase fills.
    pub args: &'a wgpu::Buffer,
    /// The visible list this phase fills.
    pub visible: &'a wgpu::Buffer,
    /// One `u32` visibility bit per instance, persistent across frames.
    pub visibility: &'a wgpu::Buffer,
}

impl OcclusionCuller {
    /// Builds the pipelines.
    pub fn new(device: &wgpu::Device) -> Self {
        let cull_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("occlusion.cull"),
            entries: &[
                entry(
                    0,
                    COMPUTE,
                    wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                ),
                entry(1, COMPUTE, storage(true)),
                entry(2, COMPUTE, storage(true)),
                entry(3, COMPUTE, storage(false)),
                entry(4, COMPUTE, storage(false)),
                entry(5, COMPUTE, storage(false)),
                entry(6, COMPUTE, float_texture()),
            ],
        });
        let base_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("occlusion.pyramid_base"),
            entries: &[
                entry(
                    0,
                    COMPUTE,
                    wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Depth,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                ),
                entry(1, COMPUTE, storage_texture()),
            ],
        });
        let reduce_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("occlusion.pyramid_reduce"),
            entries: &[
                entry(0, COMPUTE, float_texture()),
                entry(1, COMPUTE, storage_texture()),
            ],
        });
        Self {
            early: pipeline(
                device,
                "occlusion.early",
                OCCLUSION_WGSL,
                "cull_early",
                &cull_layout,
            ),
            late: pipeline(
                device,
                "occlusion.late",
                OCCLUSION_WGSL,
                "cull_late",
                &cull_layout,
            ),
            base: pipeline(
                device,
                "occlusion.pyramid_base",
                PYRAMID_BASE_WGSL,
                "pyramid_base",
                &base_layout,
            ),
            reduce: pipeline(
                device,
                "occlusion.pyramid_reduce",
                PYRAMID_REDUCE_WGSL,
                "pyramid_reduce",
                &reduce_layout,
            ),
            cull_layout,
            base_layout,
            reduce_layout,
        }
    }

    /// A pyramid for a `width x height` depth buffer.
    pub fn pyramid(&self, device: &wgpu::Device, width: u32, height: u32) -> DepthPyramid {
        let (width, height) = (width.max(1), height.max(1));
        let levels = pyramid_levels(width, height);
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("occlusion.pyramid"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: levels,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: PYRAMID_FORMAT,
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::STORAGE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let level = |l: u32| {
            texture.create_view(&wgpu::TextureViewDescriptor {
                base_mip_level: l,
                mip_level_count: Some(1),
                ..Default::default()
            })
        };
        let reduce_binds = (1..levels)
            .map(|l| {
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("occlusion.pyramid_reduce"),
                    layout: &self.reduce_layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(&level(l - 1)),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(&level(l)),
                        },
                    ],
                })
            })
            .collect();
        DepthPyramid {
            texture,
            view,
            width,
            height,
            levels,
            base_bind: None,
            reduce_binds,
        }
    }

    /// Points the pyramid's first level at `depth` (a depth view the size of the pyramid).
    pub fn bind_depth(&self, device: &wgpu::Device, pyramid: &mut DepthPyramid, depth: &wgpu::TextureView) {
        let level0 = pyramid.texture.create_view(&wgpu::TextureViewDescriptor {
            base_mip_level: 0,
            mip_level_count: Some(1),
            ..Default::default()
        });
        pyramid.base_bind = Some(device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("occlusion.pyramid_base"),
            layout: &self.base_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(depth),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&level0),
                },
            ],
        }));
    }

    /// A cull bind group over `buffers` and `pyramid`.
    pub fn bind_group(
        &self,
        device: &wgpu::Device,
        buffers: CullBuffers<'_>,
        pyramid: &DepthPyramid,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("occlusion.cull"),
            layout: &self.cull_layout,
            entries: &[
                buffer(0, buffers.uniform),
                buffer(1, buffers.instances),
                buffer(2, buffers.batches),
                buffer(3, buffers.args),
                buffer(4, buffers.visible),
                buffer(5, buffers.visibility),
                wgpu::BindGroupEntry {
                    binding: 6,
                    resource: wgpu::BindingResource::TextureView(pyramid.view()),
                },
            ],
        })
    }

    fn dispatch(
        encoder: &mut wgpu::CommandEncoder,
        label: &str,
        pipeline: &wgpu::ComputePipeline,
        bind: &wgpu::BindGroup,
        groups: (u32, u32),
    ) {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some(label),
            timestamp_writes: None,
        });
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, bind, &[]);
        pass.dispatch_workgroups(groups.0.max(1), groups.1.max(1), 1);
    }

    /// Records the early cull.
    pub fn record_early(&self, encoder: &mut wgpu::CommandEncoder, bind: &wgpu::BindGroup, instances: u32) {
        Self::dispatch(
            encoder,
            "occlusion.early",
            &self.early,
            bind,
            (instances.div_ceil(crate::culling::CULL_WORKGROUP), 1),
        );
    }

    /// Records the late cull.
    pub fn record_late(&self, encoder: &mut wgpu::CommandEncoder, bind: &wgpu::BindGroup, instances: u32) {
        Self::dispatch(
            encoder,
            "occlusion.late",
            &self.late,
            bind,
            (instances.div_ceil(crate::culling::CULL_WORKGROUP), 1),
        );
    }

    /// Records the pyramid build: level 0 from the bound depth, then each level from the
    /// one above (one compute pass per level). Nothing when no depth is bound.
    pub fn record_pyramid(&self, encoder: &mut wgpu::CommandEncoder, pyramid: &DepthPyramid) {
        let Some(base) = &pyramid.base_bind else {
            return;
        };
        Self::dispatch(
            encoder,
            "occlusion.pyramid_base",
            &self.base,
            base,
            (pyramid.width.div_ceil(8), pyramid.height.div_ceil(8)),
        );
        for (level, bind) in (1u32..).zip(&pyramid.reduce_binds) {
            let (w, h) = ((pyramid.width >> level).max(1), (pyramid.height >> level).max(1));
            Self::dispatch(
                encoder,
                "occlusion.pyramid_reduce",
                &self.reduce,
                bind,
                (w.div_ceil(8), h.div_ceil(8)),
            );
        }
    }
}
