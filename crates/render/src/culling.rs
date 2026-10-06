//! GPU frustum culling (compute). The camera view adds two-phase occlusion culling on
//! top ([`crate::occlusion`]).
//!
//! [`SceneBuffers`] holds the GPU side of a [`FramePrep`]: instances, batches, indirect
//! args, the visible list, and the cull uniform, all sized once from the scene's
//! capacities. [`SceneBuffers::upload`] writes a frame's data; [`GpuCuller::record`]
//! dispatches the cull. Afterwards `args` holds one ready indirect draw per batch.

use bytemuck::{Pod, Zeroable};

use crate::math::Frustum;
use crate::scene::{DrawIndexedIndirect, FramePrep, GpuBatch, GpuInstance};

/// The cull compute shader.
pub const CULL_WGSL: &str = include_str!("shaders/cull.wgsl");

/// Workgroup size of the cull shader.
pub const CULL_WORKGROUP: u32 = 64;

/// The cull uniform.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Debug, Pod, Zeroable)]
pub struct CullUniform {
    /// Frustum planes (unused planes never cull).
    pub planes: [[f32; 4]; 6],
    /// Instances to test.
    pub instance_count: u32,
    /// Padding.
    pub pad: [u32; 3],
}

/// GPU buffers for the scene, sized for fixed capacities.
#[derive(Debug)]
pub struct SceneBuffers {
    /// Instance array (storage).
    pub instances: wgpu::Buffer,
    /// Batch table (storage).
    pub batches: wgpu::Buffer,
    /// Indirect args (indirect and storage).
    pub args: wgpu::Buffer,
    /// Visible instance indices (storage).
    pub visible: wgpu::Buffer,
    /// Cull uniform.
    pub cull: wgpu::Buffer,
    instance_capacity: u64,
    batch_capacity: u64,
}

fn size_of<T>(n: u64) -> u64 {
    (core::mem::size_of::<T>() as u64).saturating_mul(n.max(1))
}

impl SceneBuffers {
    /// Buffers for up to `instances` instances in up to `batches` batches. `extra_args`
    /// adds usages to the args buffer (for example `COPY_SRC` for readback in tests).
    pub fn new(device: &wgpu::Device, instances: u64, batches: u64, extra_args: wgpu::BufferUsages) -> Self {
        let storage = wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST;
        let mk = |label: &str, size: u64, usage: wgpu::BufferUsages| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size,
                usage,
                mapped_at_creation: false,
            })
        };
        Self {
            instances: mk("scene.instances", size_of::<GpuInstance>(instances), storage),
            batches: mk("scene.batches", size_of::<GpuBatch>(batches), storage),
            args: mk(
                "scene.args",
                size_of::<DrawIndexedIndirect>(batches),
                storage | wgpu::BufferUsages::INDIRECT | extra_args,
            ),
            visible: mk("scene.visible", size_of::<u32>(instances), storage | extra_args),
            cull: mk(
                "scene.cull",
                size_of::<CullUniform>(1),
                wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            ),
            instance_capacity: instances,
            batch_capacity: batches,
        }
    }

    /// Writes one frame's data. The args are written with zero instance counts, which the
    /// cull pass then fills. Returns the instance count to dispatch for.
    pub fn upload(&self, queue: &wgpu::Queue, prep: &FramePrep, frustum: &Frustum) -> u32 {
        let n = (prep.instances.len() as u64).min(self.instance_capacity);
        let b = (prep.batches.len() as u64).min(self.batch_capacity);
        #[allow(clippy::cast_possible_truncation)] // Bounded by capacities sized from usize.
        let (n_us, b_us) = (n as usize, b as usize);
        if let Some(inst) = prep.instances.get(..n_us) {
            queue.write_buffer(&self.instances, 0, bytemuck::cast_slice(inst));
        }
        if let (Some(batches), Some(args)) = (prep.batches.get(..b_us), prep.args.get(..b_us)) {
            queue.write_buffer(&self.batches, 0, bytemuck::cast_slice(batches));
            queue.write_buffer(&self.args, 0, bytemuck::cast_slice(args));
        }
        let count = u32::try_from(n).unwrap_or(u32::MAX);
        let uniform = CullUniform {
            planes: frustum.gpu_planes(),
            instance_count: count,
            pad: [0; 3],
        };
        queue.write_buffer(&self.cull, 0, bytemuck::bytes_of(&uniform));
        count
    }
}

/// The cull pipeline.
#[derive(Debug)]
pub struct GpuCuller {
    pipeline: wgpu::ComputePipeline,
    layout: wgpu::BindGroupLayout,
}

impl GpuCuller {
    /// Builds the pipeline.
    pub fn new(device: &wgpu::Device) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("cull"),
            source: wgpu::ShaderSource::Wgsl(CULL_WGSL.into()),
        });
        let storage = |binding: u32, read_only: bool| {
            crate::layouts::entry(
                binding,
                wgpu::ShaderStages::COMPUTE,
                wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only },
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
            )
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("cull"),
            entries: &[
                crate::layouts::entry(
                    0,
                    wgpu::ShaderStages::COMPUTE,
                    wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                ),
                storage(1, true),
                storage(2, true),
                storage(3, false),
                storage(4, false),
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("cull"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("cull"),
            layout: Some(&pipeline_layout),
            module: &module,
            entry_point: Some("cull_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            cache: None,
        });
        Self { pipeline, layout }
    }

    /// Creates the bind group for a set of scene buffers (once; buffers are persistent).
    pub fn bind_group(&self, device: &wgpu::Device, buffers: &SceneBuffers) -> wgpu::BindGroup {
        self.bind_group_parts(
            device,
            &buffers.cull,
            &buffers.instances,
            &buffers.batches,
            &buffers.args,
            &buffers.visible,
        )
    }

    /// Creates a bind group from individual buffers: one view's cull uniform, args, and
    /// visible list over the shared instance and batch tables.
    pub fn bind_group_parts(
        &self,
        device: &wgpu::Device,
        cull: &wgpu::Buffer,
        instances: &wgpu::Buffer,
        batches: &wgpu::Buffer,
        args: &wgpu::Buffer,
        visible: &wgpu::Buffer,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("cull"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: cull.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: instances.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: batches.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: args.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: visible.as_entire_binding(),
                },
            ],
        })
    }

    /// Records the cull dispatch for `instance_count` instances.
    pub fn record(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        bind_group: &wgpu::BindGroup,
        instance_count: u32,
    ) {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("cull"),
            timestamp_writes: None,
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, bind_group, &[]);
        pass.dispatch_workgroups(instance_count.div_ceil(CULL_WORKGROUP).max(1), 1, 1);
    }
}
