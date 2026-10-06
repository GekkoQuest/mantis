//! GPU-simulated particles from data (plan 8.5), defined by
//! `mantis_formats::particle_effect`.
//!
//! **Pool.** One storage buffer of [`GpuParticle`] slots. Spawning an effect starts one
//! emitter instance per emitter definition; each instance owns a contiguous slot range of
//! its definition's capacity from a first-fit, coalescing [`RangeAllocator`]. A full pool
//! refuses with [`ParticleError::PoolFull`]; nothing panics.
//!
//! **CPU per frame ([`ParticleSystem::prepare`]).** Advances each instance's clock, counts
//! this frame's spawns (continuous rate with a fractional carry, plus bursts crossed this
//! frame, capped by capacity), and writes one [`GpuEmitter`] uniform per instance: slot
//! range, the spawn window in the range's ring (`emitted mod capacity`, count), seed,
//! transform, `dt`. Stopped instances free themselves once every particle they emitted
//! must have died. Allocation-free after warm-up.
//!
//! **GPU simulation ([`ParticleSystem::record_simulate`]).** One dispatch per instance,
//! one invocation per slot: slots in the spawn window are initialized from a stateless
//! PCG hash of (seed, emission index); other live slots integrate with semi-implicit
//! Euler (`v += a dt; v /= 1 + drag dt; p += v dt`) and age. Deterministic, no atomics.
//! [`simulate_reference`] is the operation-for-operation CPU mirror.
//!
//! **Drawing ([`ParticleSystem::record_draw`]).** Six vertices per slot of each instance's
//! range, camera-facing billboards with a procedural soft round sprite, color and size from
//! the definition's curves by normalized age. Alpha-blended instances draw first, sorted
//! far to near by emitter position (particles within an instance are not sorted), then
//! additive instances. Depth is tested against the reverse-Z scene depth
//! (`GreaterEqual`) and never written. Local-space particles are stored relative to the
//! emitter and transformed at draw time.

mod alloc;
mod reference;

use bytemuck::{Pod, Zeroable};
use glam::{Mat4, Quat, Vec3};
use mantis_formats::FormatError;
use mantis_formats::particle_effect::{
    BlendMode, Burst, MAX_BURSTS, MAX_EMITTERS, ParticleEffect, SimulationSpace,
};

pub use alloc::RangeAllocator;
pub use reference::{
    bake_def, pcg_hash, rotate, simulate_reference, sincos_turns, spawn_reference, unit_float,
};

/// The simulation compute shader.
pub const SIM_WGSL: &str = include_str!("shaders/particles_sim.wgsl");
/// The billboard draw shader.
pub const DRAW_WGSL: &str = include_str!("shaders/particles_draw.wgsl");
/// Workgroup size of the simulation shader.
pub const SIM_WORKGROUP: u32 = 64;
/// Dynamic-offset stride of the per-instance emitter uniforms.
pub const EMITTER_STRIDE: u64 = 256;
/// Largest pool the system creates (a 128 MiB storage binding).
pub const MAX_POOL: u32 = 1 << 22;

/// Definition table entries reserved per registered effect.
#[allow(clippy::cast_possible_truncation)] // Eight.
const PER_EFFECT: u32 = MAX_EMITTERS as u32;

/// `GpuEmitter::flags` bit: clear every non-spawning slot of the range this frame.
pub const FLAG_RESET: u32 = 1;

/// One pool slot. Alive while `age < lifetime`; a zeroed slot is dead.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Debug, Default, Pod, Zeroable)]
pub struct GpuParticle {
    /// Position: world space, or emitter-local for local-space emitters.
    pub position: [f32; 3],
    /// Seconds since birth.
    pub age: f32,
    /// Velocity in the same frame as `position`.
    pub velocity: [f32; 3],
    /// Seconds the particle lives.
    pub lifetime: f32,
}

impl GpuParticle {
    /// Whether the slot holds a live particle.
    pub fn is_alive(&self) -> bool {
        self.age < self.lifetime
    }
}

/// Per-instance, per-frame uniform (a prefix of each [`EMITTER_STRIDE`] slot). The
/// default is all zero except `scale`, which is 1.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Debug, Pod, Zeroable)]
pub struct GpuEmitter {
    /// Emitter world position.
    pub position: [f32; 3],
    /// Seconds simulated this frame.
    pub dt: f32,
    /// Emitter rotation quaternion `[x, y, z, w]`.
    pub rotation: [f32; 4],
    /// First pool slot of the instance's range.
    pub base: u32,
    /// Slots in the range (the definition's capacity).
    pub capacity: u32,
    /// Ring index of this frame's first spawn.
    pub spawn_start: u32,
    /// Particles spawned this frame.
    pub spawn_count: u32,
    /// Emission index (low 32 bits) of this frame's first spawn.
    pub emit_base: u32,
    /// Instance seed.
    pub seed: u32,
    /// Index into the definition table.
    pub def_index: u32,
    /// [`FLAG_RESET`] and future bits.
    pub flags: u32,
    /// Acceleration in the particles' frame (world, or emitter-local for local space).
    pub acceleration: [f32; 3],
    /// Seconds into the current loop.
    pub time: f32,
    /// Uniform scale of the instance (spawn offsets, speeds, acceleration, sizes).
    pub scale: f32,
    /// Padding.
    pub pad: [f32; 3],
}

impl Default for GpuEmitter {
    fn default() -> Self {
        Self {
            scale: 1.0,
            ..bytemuck::Zeroable::zeroed()
        }
    }
}

/// One baked emitter definition (storage table entry).
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Debug, Default, Pod, Zeroable)]
pub struct GpuEmitterDef {
    /// Drag per second.
    pub drag: f32,
    /// 0 point, 1 sphere, 2 cone.
    pub shape: u32,
    /// 0 world, 1 local.
    pub space: u32,
    /// 0 additive, 1 alpha.
    pub blend: u32,
    /// Sphere radius, or the cosine of the cone half-angle.
    pub shape_a: f32,
    /// Cone base radius.
    pub shape_b: f32,
    /// Slowest initial speed.
    pub speed_min: f32,
    /// Fastest initial speed.
    pub speed_max: f32,
    /// Shortest lifetime.
    pub lifetime_min: f32,
    /// Longest lifetime.
    pub lifetime_max: f32,
    /// Color keys in use (1 to 4).
    pub color_count: u32,
    /// Size keys in use (1 to 4).
    pub size_count: u32,
    /// Color key times.
    pub color_t: [f32; 4],
    /// Color key values, straight linear RGBA.
    pub colors: [[f32; 4]; 4],
    /// Size key times.
    pub size_t: [f32; 4],
    /// Size key values (billboard edge, meters).
    pub sizes: [f32; 4],
}

/// The draw camera uniform.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Debug, Default, Pod, Zeroable)]
pub struct GpuParticleCamera {
    /// World to clip, as the scene renders this frame.
    pub view_proj: [[f32; 4]; 4],
    /// Camera right in world space (w unused).
    pub right: [f32; 4],
    /// Camera up in world space (w unused).
    pub up: [f32; 4],
}

/// Fixed capacities, chosen once.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ParticleConfig {
    /// Pool slots shared by every live instance (1 to [`MAX_POOL`]).
    pub max_particles: u32,
    /// Effects that can be registered.
    pub max_effects: u32,
    /// Emitter instances live at once (each spawned effect uses one per emitter).
    pub max_emitters: u32,
}

/// Where an effect instance is.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct EmitterTransform {
    /// World position.
    pub position: Vec3,
    /// Orientation (unit quaternion); the cone axis is local +Y.
    pub rotation: Quat,
    /// Uniform scale (positive): spawn offsets, speeds, acceleration, and particle sizes
    /// scale with it, so a scaled effect looks like the same effect seen nearer.
    pub scale: f32,
}

impl EmitterTransform {
    /// At `position`, unrotated, unscaled.
    pub fn at(position: Vec3) -> Self {
        Self {
            position,
            rotation: Quat::IDENTITY,
            scale: 1.0,
        }
    }

    /// This transform with `scale`.
    #[must_use]
    pub fn scaled(self, scale: f32) -> Self {
        Self { scale, ..self }
    }

    fn is_finite(&self) -> bool {
        self.position.is_finite() && self.rotation.is_finite() && self.scale.is_finite() && self.scale > 0.0
    }
}

/// A registered effect.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct EffectId(pub u32);

/// A spawned effect instance (generational: stale after it is killed or finishes).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct EmitterHandle {
    index: u32,
    generation: u32,
}

/// Why a particle call was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ParticleError {
    /// No free slot range is large enough for one of the effect's emitters.
    PoolFull,
    /// Every emitter instance slot is in use.
    TooManyEmitters,
    /// Every effect slot is registered.
    TooManyEffects,
    /// The effect needs more slots than the whole pool.
    EffectTooLarge,
    /// The effect id was never registered.
    UnknownEffect,
    /// The handle's instance was killed or finished.
    StaleHandle,
    /// A transform component is not finite.
    InvalidTransform,
    /// The effect fails validation.
    InvalidEffect(FormatError),
}

impl core::fmt::Display for ParticleError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "particles: {self:?}")
    }
}

impl std::error::Error for ParticleError {}

/// What one [`ParticleSystem::prepare`] did.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ParticleStats {
    /// Emitter instances simulated and drawn this frame.
    pub live_emitters: u32,
    /// Effect instances alive after this frame's bookkeeping.
    pub live_effects: u32,
    /// Particles spawned this frame.
    pub spawned: u32,
    /// Pool slots dispatched (sum of live instance capacities).
    pub slots: u32,
    /// Emitter instances that finished and were freed this frame.
    pub freed: u32,
}

/// The CPU-side schedule of one emitter definition.
#[derive(Clone, Copy, Debug)]
struct Schedule {
    capacity: u32,
    duration: f32,
    looping: bool,
    rate: f32,
    bursts: [Burst; MAX_BURSTS],
    burst_count: usize,
    lifetime_max: f32,
    acceleration: Vec3,
    local: bool,
    alpha: bool,
}

impl Schedule {
    fn bursts_in(&self, from: f32, to: f32) -> u64 {
        self.bursts
            .iter()
            .take(self.burst_count)
            .filter(|b| b.time >= from && b.time < to)
            .map(|b| u64::from(b.count))
            .sum()
    }
}

/// This frame's spawn window in an instance's ring.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct SpawnWindow {
    start: u32,
    count: u32,
    base: u32,
}

/// An emitter instance's clock and counters.
#[derive(Clone, Copy, Debug, Default)]
struct Emission {
    /// Seconds into the current loop.
    time: f32,
    /// Fractional particles owed by the continuous rate.
    carry: f32,
    /// Particles emitted so far.
    emitted: u64,
    emitting: bool,
    /// Seconds since the last frame that spawned (the youngest particle's age).
    since_spawn: f32,
}

impl Emission {
    fn started() -> Self {
        Self {
            emitting: true,
            ..Self::default()
        }
    }

    /// Advances by `dt`. `None` when the instance is finished: it stopped emitting and
    /// every particle it emitted is dead after this frame.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // Floor of a non-negative carry; ring indices below capacity.
    fn advance(&mut self, s: &Schedule, dt: f32) -> Option<SpawnWindow> {
        if !self.emitting {
            self.since_spawn += dt;
            return (self.since_spawn < s.lifetime_max).then_some(SpawnWindow::default());
        }
        let t0 = self.time;
        let mut new = 0u64;
        let span = if s.looping {
            // At most one wrap per frame: a frame longer than a loop emits one loop.
            let step = dt.min(s.duration);
            let end = t0 + step;
            if end >= s.duration {
                let wrapped = (end - s.duration).clamp(0.0, t0);
                new += s.bursts_in(t0, s.duration) + s.bursts_in(0.0, wrapped);
                self.time = wrapped;
            } else {
                new += s.bursts_in(t0, end);
                self.time = end;
            }
            step
        } else {
            let end = (t0 + dt).min(s.duration);
            new += s.bursts_in(t0, end);
            self.time = end;
            if end >= s.duration {
                self.emitting = false;
            }
            (end - t0).max(0.0)
        };
        self.carry += s.rate * span;
        let whole = self.carry.floor();
        self.carry -= whole;
        new = new.saturating_add(whole as u64);
        let capacity = u64::from(s.capacity.max(1));
        let count = new.min(capacity) as u32;
        let window = SpawnWindow {
            start: (self.emitted % capacity) as u32,
            count,
            base: self.emitted as u32,
        };
        self.emitted = self.emitted.wrapping_add(u64::from(count));
        if count > 0 {
            self.since_spawn = 0.0;
        } else {
            self.since_spawn += dt;
        }
        Some(window)
    }
}

#[derive(Clone, Copy, Debug)]
struct EmitterSlot {
    live: bool,
    instance: u32,
    effect: u32,
    def: u32,
    base: u32,
    seed: u32,
    reset: bool,
    emission: Emission,
    depth: f32,
}

impl EmitterSlot {
    const EMPTY: EmitterSlot = EmitterSlot {
        live: false,
        instance: 0,
        effect: 0,
        def: 0,
        base: 0,
        seed: 0,
        reset: false,
        emission: Emission {
            time: 0.0,
            carry: 0.0,
            emitted: 0,
            emitting: false,
            since_spawn: 0.0,
        },
        depth: 0.0,
    };
}

#[derive(Clone, Copy, Debug)]
struct InstanceSlot {
    live: bool,
    generation: u32,
    transform: EmitterTransform,
    emitters: [u32; MAX_EMITTERS],
    live_count: u32,
    total: u32,
}

#[derive(Clone, Debug)]
struct RegisteredEffect {
    schedules: Vec<Schedule>,
}

/// GPU particle system: pool, emitter instances, pipelines.
#[derive(Debug)]
pub struct ParticleSystem {
    config: ParticleConfig,
    allocator: RangeAllocator,
    effects: Vec<RegisteredEffect>,
    defs_cpu: Vec<GpuEmitterDef>,
    slots: Vec<EmitterSlot>,
    free_slots: Vec<u32>,
    instances: Vec<InstanceSlot>,
    free_instances: Vec<u32>,
    emitters_cpu: Vec<GpuEmitter>,
    staging: Vec<u8>,
    upload_len: usize,
    simulate_list: Vec<u32>,
    alpha_list: Vec<u32>,
    additive_list: Vec<u32>,
    camera: GpuParticleCamera,
    particles: wgpu::Buffer,
    defs: wgpu::Buffer,
    emitters: wgpu::Buffer,
    camera_buffer: wgpu::Buffer,
    sim_pipeline: wgpu::ComputePipeline,
    sim_bind: wgpu::BindGroup,
    additive_pipeline: wgpu::RenderPipeline,
    alpha_pipeline: wgpu::RenderPipeline,
    draw_bind: wgpu::BindGroup,
}

fn buffer_entry(
    binding: u32,
    visibility: wgpu::ShaderStages,
    ty: wgpu::BufferBindingType,
    dynamic: bool,
) -> wgpu::BindGroupLayoutEntry {
    crate::layouts::entry(
        binding,
        visibility,
        wgpu::BindingType::Buffer {
            ty,
            has_dynamic_offset: dynamic,
            min_binding_size: None,
        },
    )
}

fn emitter_binding(buffer: &wgpu::Buffer) -> wgpu::BindingResource<'_> {
    wgpu::BindingResource::Buffer(wgpu::BufferBinding {
        buffer,
        offset: 0,
        size: core::num::NonZeroU64::new(core::mem::size_of::<GpuEmitter>() as u64),
    })
}

fn draw_pipeline(
    device: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    module: &wgpu::ShaderModule,
    hdr_format: wgpu::TextureFormat,
    depth_format: wgpu::TextureFormat,
    blend: wgpu::BlendState,
    label: &str,
) -> wgpu::RenderPipeline {
    let targets = [Some(wgpu::ColorTargetState {
        format: hdr_format,
        blend: Some(blend),
        write_mask: wgpu::ColorWrites::ALL,
    })];
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module,
            entry_point: Some("vs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: &[],
        },
        primitive: wgpu::PrimitiveState {
            cull_mode: None,
            front_face: crate::gpu_types::FRONT_FACE,
            ..Default::default()
        },
        // Reverse Z: nearer is greater. Particles test against scene depth, never write it.
        depth_stencil: Some(wgpu::DepthStencilState {
            format: depth_format,
            depth_write_enabled: Some(false),
            depth_compare: Some(wgpu::CompareFunction::GreaterEqual),
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState::default(),
        }),
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module,
            entry_point: Some("fs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: &targets,
        }),
        multiview_mask: None,
        cache: None,
    })
}

/// Additive: light adds; destination alpha is kept.
const ADDITIVE: wgpu::BlendState = wgpu::BlendState {
    color: wgpu::BlendComponent {
        src_factor: wgpu::BlendFactor::One,
        dst_factor: wgpu::BlendFactor::One,
        operation: wgpu::BlendOperation::Add,
    },
    alpha: wgpu::BlendComponent {
        src_factor: wgpu::BlendFactor::Zero,
        dst_factor: wgpu::BlendFactor::One,
        operation: wgpu::BlendOperation::Add,
    },
};

impl ParticleSystem {
    /// Creates the pool, tables, and pipelines. Capacities are clamped to at least 1 and
    /// the pool to [`MAX_POOL`]. `hdr_format` and `depth_format` are the formats of the
    /// color and depth attachments [`ParticleSystem::record_draw`] will be called with.
    #[allow(clippy::too_many_lines)] // Buffers, layouts, and three pipelines, declared once.
    pub fn new(
        device: &wgpu::Device,
        config: ParticleConfig,
        hdr_format: wgpu::TextureFormat,
        depth_format: wgpu::TextureFormat,
    ) -> Self {
        let config = ParticleConfig {
            max_particles: config.max_particles.clamp(1, MAX_POOL),
            max_effects: config.max_effects.max(1),
            max_emitters: config.max_emitters.max(1),
        };
        let emitters_n = config.max_emitters as usize;
        let defs_n = config.max_effects as usize * MAX_EMITTERS;
        let buffer = |label: &str, size: u64, usage: wgpu::BufferUsages| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size,
                usage,
                mapped_at_creation: false,
            })
        };
        let storage = wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST;
        let particles = buffer(
            "particles.pool",
            u64::from(config.max_particles) * core::mem::size_of::<GpuParticle>() as u64,
            storage | wgpu::BufferUsages::COPY_SRC,
        );
        let defs = buffer(
            "particles.defs",
            defs_n as u64 * core::mem::size_of::<GpuEmitterDef>() as u64,
            storage,
        );
        let emitters = buffer(
            "particles.emitters",
            u64::from(config.max_emitters) * EMITTER_STRIDE,
            wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        );
        let camera_buffer = buffer(
            "particles.camera",
            core::mem::size_of::<GpuParticleCamera>() as u64,
            wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        );

        let compute = wgpu::ShaderStages::COMPUTE;
        let sim_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("particles.sim"),
            entries: &[
                buffer_entry(0, compute, wgpu::BufferBindingType::Uniform, true),
                buffer_entry(
                    1,
                    compute,
                    wgpu::BufferBindingType::Storage { read_only: true },
                    false,
                ),
                buffer_entry(
                    2,
                    compute,
                    wgpu::BufferBindingType::Storage { read_only: false },
                    false,
                ),
            ],
        });
        let sim_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("particles.sim"),
            source: wgpu::ShaderSource::Wgsl(SIM_WGSL.into()),
        });
        let sim_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("particles.sim"),
            bind_group_layouts: &[Some(&sim_layout)],
            immediate_size: 0,
        });
        let sim_pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("particles.sim"),
            layout: Some(&sim_pipeline_layout),
            module: &sim_module,
            entry_point: Some("simulate"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            cache: None,
        });
        let sim_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("particles.sim"),
            layout: &sim_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: emitter_binding(&emitters),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: defs.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: particles.as_entire_binding(),
                },
            ],
        });

        let vertex = wgpu::ShaderStages::VERTEX;
        let draw_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("particles.draw"),
            entries: &[
                buffer_entry(0, vertex, wgpu::BufferBindingType::Uniform, false),
                buffer_entry(1, vertex, wgpu::BufferBindingType::Uniform, true),
                buffer_entry(
                    2,
                    vertex,
                    wgpu::BufferBindingType::Storage { read_only: true },
                    false,
                ),
                buffer_entry(
                    3,
                    vertex,
                    wgpu::BufferBindingType::Storage { read_only: true },
                    false,
                ),
            ],
        });
        let draw_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("particles.draw"),
            source: wgpu::ShaderSource::Wgsl(DRAW_WGSL.into()),
        });
        let draw_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("particles.draw"),
            bind_group_layouts: &[Some(&draw_layout)],
            immediate_size: 0,
        });
        let additive_pipeline = draw_pipeline(
            device,
            &draw_pipeline_layout,
            &draw_module,
            hdr_format,
            depth_format,
            ADDITIVE,
            "particles.additive",
        );
        let alpha_pipeline = draw_pipeline(
            device,
            &draw_pipeline_layout,
            &draw_module,
            hdr_format,
            depth_format,
            wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING,
            "particles.alpha",
        );
        let draw_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("particles.draw"),
            layout: &draw_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: camera_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: emitter_binding(&emitters),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: defs.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: particles.as_entire_binding(),
                },
            ],
        });

        let stride = usize::try_from(EMITTER_STRIDE).unwrap_or(256);
        Self {
            config,
            allocator: RangeAllocator::new(config.max_particles, emitters_n),
            effects: Vec::with_capacity(config.max_effects as usize),
            defs_cpu: vec![GpuEmitterDef::default(); defs_n],
            slots: vec![EmitterSlot::EMPTY; emitters_n],
            free_slots: (0..config.max_emitters).rev().collect(),
            instances: vec![
                InstanceSlot {
                    live: false,
                    generation: 0,
                    transform: EmitterTransform::at(Vec3::ZERO),
                    emitters: [0; MAX_EMITTERS],
                    live_count: 0,
                    total: 0,
                };
                emitters_n
            ],
            free_instances: (0..config.max_emitters).rev().collect(),
            emitters_cpu: vec![GpuEmitter::default(); emitters_n],
            staging: vec![0; emitters_n * stride],
            upload_len: 0,
            simulate_list: Vec::with_capacity(emitters_n),
            alpha_list: Vec::with_capacity(emitters_n),
            additive_list: Vec::with_capacity(emitters_n),
            camera: GpuParticleCamera::default(),
            particles,
            defs,
            emitters,
            camera_buffer,
            sim_pipeline,
            sim_bind,
            additive_pipeline,
            alpha_pipeline,
            draw_bind,
        }
    }

    /// The configuration in effect (after clamping).
    pub fn config(&self) -> ParticleConfig {
        self.config
    }

    /// Registers an effect and uploads its baked definitions.
    ///
    /// # Errors
    /// [`ParticleError::InvalidEffect`] when the effect fails validation,
    /// [`ParticleError::EffectTooLarge`] when its total capacity exceeds the pool, and
    /// [`ParticleError::TooManyEffects`] when every effect slot is taken.
    pub fn register(
        &mut self,
        queue: &wgpu::Queue,
        effect: &ParticleEffect,
    ) -> Result<EffectId, ParticleError> {
        effect.validate().map_err(ParticleError::InvalidEffect)?;
        if effect.capacity_total() > self.config.max_particles {
            return Err(ParticleError::EffectTooLarge);
        }
        let id = u32::try_from(self.effects.len()).map_err(|_| ParticleError::TooManyEffects)?;
        if id >= self.config.max_effects {
            return Err(ParticleError::TooManyEffects);
        }
        let first = id as usize * MAX_EMITTERS;
        let mut schedules = Vec::with_capacity(effect.emitters.len());
        for (i, def) in effect.emitters.iter().enumerate() {
            let baked = bake_def(def);
            if let Some(slot) = self.defs_cpu.get_mut(first + i) {
                *slot = baked;
            }
            let mut bursts = [Burst { time: 0.0, count: 0 }; MAX_BURSTS];
            for (b, src) in bursts.iter_mut().zip(&def.bursts) {
                *b = *src;
            }
            schedules.push(Schedule {
                capacity: def.capacity,
                duration: def.duration,
                looping: def.looping,
                rate: def.rate,
                bursts,
                burst_count: def.bursts.len().min(MAX_BURSTS),
                lifetime_max: def.lifetime_max,
                acceleration: Vec3::from_array(def.acceleration),
                local: def.space == SimulationSpace::Local,
                alpha: def.blend == BlendMode::Alpha,
            });
        }
        if let Some(baked) = self.defs_cpu.get(first..first + effect.emitters.len()) {
            let offset = first as u64 * core::mem::size_of::<GpuEmitterDef>() as u64;
            queue.write_buffer(&self.defs, offset, bytemuck::cast_slice(baked));
        }
        self.effects.push(RegisteredEffect { schedules });
        Ok(EffectId(id))
    }

    /// Starts an instance of `effect`: one emitter instance per emitter definition, each
    /// with its own pool range. `seed` makes the instance's particles reproducible.
    ///
    /// # Errors
    /// [`ParticleError::UnknownEffect`], [`ParticleError::InvalidTransform`],
    /// [`ParticleError::TooManyEmitters`] when instance slots run out, and
    /// [`ParticleError::PoolFull`] when a range does not fit (nothing is kept on failure).
    pub fn spawn(
        &mut self,
        effect: EffectId,
        transform: EmitterTransform,
        seed: u32,
    ) -> Result<EmitterHandle, ParticleError> {
        let schedules = &self
            .effects
            .get(effect.0 as usize)
            .ok_or(ParticleError::UnknownEffect)?
            .schedules;
        if !transform.is_finite() {
            return Err(ParticleError::InvalidTransform);
        }
        if self.free_slots.len() < schedules.len() || self.free_instances.is_empty() {
            return Err(ParticleError::TooManyEmitters);
        }
        // Reserve every range first so a failure leaves nothing behind.
        let mut ranges: [core::ops::Range<u32>; MAX_EMITTERS] = core::array::from_fn(|_| 0..0);
        for (i, s) in schedules.iter().enumerate() {
            let got = self.allocator.allocate(s.capacity);
            match (got, ranges.get_mut(i)) {
                (Some(r), Some(slot)) => *slot = r,
                (got, _) => {
                    if let Some(r) = got {
                        let _ = self.allocator.release(r);
                    }
                    for r in ranges.iter().take(i) {
                        let _ = self.allocator.release(r.clone());
                    }
                    return Err(ParticleError::PoolFull);
                }
            }
        }
        let instance = self.free_instances.pop().ok_or(ParticleError::TooManyEmitters)?;
        let mut emitters = [0u32; MAX_EMITTERS];
        let count = schedules.len().min(MAX_EMITTERS);
        for (i, (range, out)) in ranges.iter().zip(emitters.iter_mut()).take(count).enumerate() {
            let slot_index = self.free_slots.pop().ok_or(ParticleError::TooManyEmitters)?;
            *out = slot_index;
            let def_in_effect = u32::try_from(i).unwrap_or(0);
            if let Some(slot) = self.slots.get_mut(slot_index as usize) {
                *slot = EmitterSlot {
                    live: true,
                    instance,
                    effect: effect.0,
                    def: effect.0.saturating_mul(PER_EFFECT).saturating_add(def_in_effect),
                    base: range.start,
                    seed: pcg_hash(seed ^ pcg_hash(def_in_effect.wrapping_add(0x9e37_79b9))),
                    reset: true,
                    emission: Emission::started(),
                    depth: 0.0,
                };
            }
        }
        let slot = self
            .instances
            .get_mut(instance as usize)
            .ok_or(ParticleError::TooManyEmitters)?;
        let count = u32::try_from(count).unwrap_or(0);
        slot.live = true;
        slot.transform = transform;
        slot.emitters = emitters;
        slot.live_count = count;
        slot.total = count;
        Ok(EmitterHandle {
            index: instance,
            generation: slot.generation,
        })
    }

    fn instance(&self, h: EmitterHandle) -> Result<&InstanceSlot, ParticleError> {
        self.instances
            .get(h.index as usize)
            .filter(|i| i.live && i.generation == h.generation)
            .ok_or(ParticleError::StaleHandle)
    }

    /// Moves an instance. Local-space particles follow; world-space particles stay.
    ///
    /// # Errors
    /// [`ParticleError::StaleHandle`] or [`ParticleError::InvalidTransform`].
    pub fn set_transform(&mut self, h: EmitterHandle, t: EmitterTransform) -> Result<(), ParticleError> {
        let _ = self.instance(h)?;
        if !t.is_finite() {
            return Err(ParticleError::InvalidTransform);
        }
        if let Some(i) = self.instances.get_mut(h.index as usize) {
            i.transform = t;
        }
        Ok(())
    }

    /// Stops emitting. Live particles finish their lives; the instance frees itself once
    /// the youngest particle it emitted has outlived the longest lifetime.
    ///
    /// # Errors
    /// [`ParticleError::StaleHandle`].
    pub fn stop(&mut self, h: EmitterHandle) -> Result<(), ParticleError> {
        let instance = *self.instance(h)?;
        for &e in instance.emitters.iter().take(instance.total as usize) {
            if let Some(slot) = self
                .slots
                .get_mut(e as usize)
                .filter(|s| s.live && s.instance == h.index)
            {
                slot.emission.emitting = false;
                slot.emission.carry = 0.0;
            }
        }
        Ok(())
    }

    /// Removes an instance and its particles immediately.
    ///
    /// # Errors
    /// [`ParticleError::StaleHandle`].
    pub fn kill(&mut self, h: EmitterHandle) -> Result<(), ParticleError> {
        let instance = *self.instance(h)?;
        for &e in instance.emitters.iter().take(instance.total as usize) {
            // A slot freed earlier may already belong to another instance.
            if self
                .slots
                .get(e as usize)
                .is_some_and(|s| s.live && s.instance == h.index)
            {
                self.free_emitter(e);
            }
        }
        Ok(())
    }

    /// Releases an emitter slot's range, and its instance when it was the last one.
    fn free_emitter(&mut self, e: u32) {
        let Some(slot) = self.slots.get_mut(e as usize).filter(|s| s.live) else {
            return;
        };
        slot.live = false;
        let (base, effect, def, instance) = (slot.base, slot.effect, slot.def, slot.instance);
        let capacity = self
            .effects
            .get(effect as usize)
            .and_then(|r| r.schedules.get((def % PER_EFFECT) as usize))
            .map_or(0, |s| s.capacity);
        let _ = self.allocator.release(base..base + capacity);
        self.free_slots.push(e);
        if let Some(inst) = self.instances.get_mut(instance as usize) {
            inst.live_count = inst.live_count.saturating_sub(1);
            if inst.live_count == 0 && inst.live {
                inst.live = false;
                inst.generation = inst.generation.wrapping_add(1);
                self.free_instances.push(instance);
            }
        }
    }

    /// Emitter instances alive.
    pub fn live_emitters(&self) -> u32 {
        u32::try_from(self.slots.iter().filter(|s| s.live).count()).unwrap_or(u32::MAX)
    }

    /// Whether `h` still refers to a live instance.
    pub fn is_live(&self, h: EmitterHandle) -> bool {
        self.instance(h).is_ok()
    }

    /// The pool range of emitter `emitter` (definition order) of a live instance.
    pub fn emitter_range(&self, h: EmitterHandle, emitter: usize) -> Option<core::ops::Range<u32>> {
        let instance = self.instance(h).ok()?;
        if emitter >= instance.total as usize {
            return None;
        }
        let slot = self
            .slots
            .get(*instance.emitters.get(emitter)? as usize)
            .filter(|s| s.live && s.instance == h.index)?;
        let capacity = self.schedule(slot)?.capacity;
        Some(slot.base..slot.base + capacity)
    }

    /// The range allocator (free space diagnostics).
    pub fn allocator(&self) -> &RangeAllocator {
        &self.allocator
    }

    /// The particle pool (storage; `COPY_SRC` for readback).
    pub fn particle_buffer(&self) -> &wgpu::Buffer {
        &self.particles
    }

    fn schedule(&self, slot: &EmitterSlot) -> Option<&Schedule> {
        self.effects
            .get(slot.effect as usize)?
            .schedules
            .get((slot.def % PER_EFFECT) as usize)
    }

    /// CPU frame preparation: advances every instance by `dt` seconds, frees finished
    /// ones, and fills this frame's emitter uniforms and draw lists. `view_proj` is the
    /// matrix the scene renders with this frame (jittered when the scene is), and
    /// `camera_right` and `camera_up` are the camera's world-space axes. Allocation-free
    /// after warm-up. Run [`ParticleSystem::upload`], [`ParticleSystem::record_simulate`],
    /// and [`ParticleSystem::record_draw`] once after each call.
    #[allow(clippy::cast_possible_truncation)] // Slot indices are below `max_emitters` (u32).
    pub fn prepare(
        &mut self,
        dt: f32,
        view_proj: Mat4,
        camera_right: Vec3,
        camera_up: Vec3,
    ) -> ParticleStats {
        let dt = if dt.is_finite() { dt.max(0.0) } else { 0.0 };
        self.camera = GpuParticleCamera {
            view_proj: view_proj.to_cols_array_2d(),
            right: camera_right.extend(0.0).to_array(),
            up: camera_up.extend(0.0).to_array(),
        };
        self.simulate_list.clear();
        self.alpha_list.clear();
        self.additive_list.clear();
        let mut stats = ParticleStats::default();
        let mut highest = 0usize;
        for index in 0..self.slots.len() {
            let Some(slot) = self.slots.get(index).copied().filter(|s| s.live) else {
                continue;
            };
            let Some(schedule) = self.schedule(&slot).copied() else {
                continue;
            };
            let mut emission = slot.emission;
            let Some(window) = emission.advance(&schedule, dt) else {
                self.free_emitter(index as u32);
                stats.freed += 1;
                continue;
            };
            let transform = self
                .instances
                .get(slot.instance as usize)
                .map_or(EmitterTransform::at(Vec3::ZERO), |i| i.transform);
            let rotation = transform.rotation.to_array();
            let scaled = (schedule.acceleration * transform.scale).to_array();
            let acceleration = if schedule.local {
                let [x, y, z, w] = rotation;
                rotate([-x, -y, -z, w], scaled)
            } else {
                scaled
            };
            let gpu = GpuEmitter {
                position: transform.position.to_array(),
                dt,
                rotation,
                base: slot.base,
                capacity: schedule.capacity,
                spawn_start: window.start,
                spawn_count: window.count,
                emit_base: window.base,
                seed: slot.seed,
                def_index: slot.def,
                flags: if slot.reset { FLAG_RESET } else { 0 },
                acceleration,
                time: emission.time,
                scale: transform.scale,
                pad: [0.0; 3],
            };
            let depth = (view_proj * transform.position.extend(1.0)).w;
            if let Some(s) = self.slots.get_mut(index) {
                s.emission = emission;
                s.reset = false;
                s.depth = if depth.is_finite() { depth } else { 0.0 };
            }
            if let Some(out) = self.emitters_cpu.get_mut(index) {
                *out = gpu;
            }
            let stride = EMITTER_STRIDE as usize;
            if let Some(bytes) = self
                .staging
                .get_mut(index * stride..index * stride + core::mem::size_of::<GpuEmitter>())
            {
                bytes.copy_from_slice(bytemuck::bytes_of(&gpu));
            }
            highest = index + 1;
            self.simulate_list.push(index as u32);
            if schedule.alpha {
                self.alpha_list.push(index as u32);
            } else {
                self.additive_list.push(index as u32);
            }
            stats.live_emitters += 1;
            stats.spawned += window.count;
            stats.slots += schedule.capacity;
        }
        // Alpha-blended instances draw far to near.
        let slots = &self.slots;
        let depth_of = |i: &u32| slots.get(*i as usize).map_or(0.0, |s| s.depth);
        self.alpha_list
            .sort_unstable_by(|a, b| depth_of(b).total_cmp(&depth_of(a)));
        self.upload_len = highest * EMITTER_STRIDE as usize;
        stats.live_effects = u32::try_from(self.instances.iter().filter(|i| i.live).count()).unwrap_or(0);
        stats
    }

    /// Writes this frame's camera and emitter uniforms.
    pub fn upload(&self, queue: &wgpu::Queue) {
        queue.write_buffer(&self.camera_buffer, 0, bytemuck::bytes_of(&self.camera));
        if let Some(bytes) = self.staging.get(..self.upload_len).filter(|b| !b.is_empty()) {
            queue.write_buffer(&self.emitters, 0, bytes);
        }
    }

    /// Records the simulation into a compute pass: one dispatch per live instance.
    pub fn record_simulate(&self, pass: &mut wgpu::ComputePass<'_>) {
        if self.simulate_list.is_empty() {
            return;
        }
        pass.set_pipeline(&self.sim_pipeline);
        for &index in &self.simulate_list {
            let Some(e) = self.emitters_cpu.get(index as usize) else {
                continue;
            };
            pass.set_bind_group(0, &self.sim_bind, &[Self::offset(index)]);
            pass.dispatch_workgroups(e.capacity.div_ceil(SIM_WORKGROUP).max(1), 1, 1);
        }
    }

    /// Records the billboards into the caller's render pass, whose color attachment is the
    /// HDR target (load, do not clear) and whose depth attachment is the scene depth
    /// (read-only use; particles never write depth). Alpha instances draw first, far to
    /// near, then additive instances.
    pub fn record_draw(&self, pass: &mut wgpu::RenderPass<'_>) {
        for (pipeline, list) in [
            (&self.alpha_pipeline, &self.alpha_list),
            (&self.additive_pipeline, &self.additive_list),
        ] {
            if list.is_empty() {
                continue;
            }
            pass.set_pipeline(pipeline);
            for &index in list {
                let Some(e) = self.emitters_cpu.get(index as usize) else {
                    continue;
                };
                pass.set_bind_group(0, &self.draw_bind, &[Self::offset(index)]);
                pass.draw(0..e.capacity.saturating_mul(6), 0..1);
            }
        }
    }

    #[allow(clippy::cast_possible_truncation)] // Uniform buffers are far below 4 GiB.
    fn offset(index: u32) -> u32 {
        index.saturating_mul(EMITTER_STRIDE as u32)
    }

    /// Runs [`simulate_reference`] over `pool` for every instance this frame simulates,
    /// with exactly the uniforms [`ParticleSystem::upload`] sends.
    pub fn simulate_reference_frame(&self, pool: &mut [GpuParticle]) {
        for &index in &self.simulate_list {
            let Some(e) = self.emitters_cpu.get(index as usize) else {
                continue;
            };
            let Some(def) = self.defs_cpu.get(e.def_index as usize) else {
                continue;
            };
            simulate_reference(pool, e, def);
        }
    }
}

#[cfg(test)]
mod tests;
