//! The CPU mirror of `particles_sim.wgsl`: the same hash, the same polynomials, the same
//! integration order, in `f32`. Tests compare the GPU pool against it; nothing at runtime
//! depends on it.

use mantis_formats::particle_effect::{BlendMode, EmitterDef, EmitterShape, MAX_KEYS, SimulationSpace};

use super::{FLAG_RESET, GpuEmitter, GpuEmitterDef, GpuParticle};

/// Shape codes shared with the shaders.
pub(crate) const SHAPE_POINT: u32 = 0;
pub(crate) const SHAPE_SPHERE: u32 = 1;
pub(crate) const SHAPE_CONE: u32 = 2;

/// The PCG hash the shaders use.
pub fn pcg_hash(v: u32) -> u32 {
    let state = v.wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
    let word = ((state >> ((state >> 28) + 4)) ^ state).wrapping_mul(277_803_737);
    (word >> 22) ^ word
}

/// A float in (0, 1) from the top 24 bits of `h`.
#[allow(clippy::cast_precision_loss)] // At most 2^24, exact in f32.
pub fn unit_float(h: u32) -> f32 {
    ((h >> 8) as f32 + 0.5) * (1.0 / 16_777_216.0)
}

/// Sine and cosine of `turns` full turns: the shaders' quadrant reduction and
/// polynomials (absolute error below 1e-6).
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // Quadrant index of a value in [0, 4].
pub fn sincos_turns(turns: f32) -> (f32, f32) {
    let quarters = turns * 4.0;
    let quadrant = (quarters + 0.5).floor();
    let a = (quarters - quadrant) * 1.570_796_4;
    let a2 = a * a;
    let sin = a
        * (1.0
            + a2 * (-0.166_666_67
                + a2 * (0.008_333_334 + a2 * (-0.000_198_412_7 + a2 * 0.000_002_755_731_9))));
    let cos = 1.0 + a2 * (-0.5 + a2 * (0.041_666_668 + a2 * (-0.001_388_888_9 + a2 * 0.000_024_801_587)));
    match (quadrant as i64 as u32) & 3 {
        0 => (sin, cos),
        1 => (cos, -sin),
        2 => (-sin, -cos),
        _ => (-cos, sin),
    }
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    let [ax, ay, az] = a;
    let [bx, by, bz] = b;
    [ay * bz - az * by, az * bx - ax * bz, ax * by - ay * bx]
}

fn add(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    let [ax, ay, az] = a;
    let [bx, by, bz] = b;
    [ax + bx, ay + by, az + bz]
}

fn scale(a: [f32; 3], s: f32) -> [f32; 3] {
    a.map(|c| c * s)
}

/// Rotates `v` by the unit quaternion `q` (`[x, y, z, w]`), as the shaders do.
pub fn rotate(q: [f32; 4], v: [f32; 3]) -> [f32; 3] {
    let [qx, qy, qz, qw] = q;
    let axis = [qx, qy, qz];
    let t = scale(cross(axis, v), 2.0);
    add(add(v, scale(t, qw)), cross(axis, t))
}

/// Bakes one emitter definition into its GPU table entry.
pub fn bake_def(def: &EmitterDef) -> GpuEmitterDef {
    let (shape, shape_a, shape_b) = match def.shape {
        EmitterShape::Point => (SHAPE_POINT, 0.0, 0.0),
        EmitterShape::Sphere { radius } => (SHAPE_SPHERE, radius, 0.0),
        EmitterShape::Cone { angle, radius } => (SHAPE_CONE, angle.cos(), radius),
    };
    let mut color_t = [1.0f32; 4];
    let mut colors = [[0.0f32; 4]; 4];
    for ((t, c), k) in color_t
        .iter_mut()
        .zip(colors.iter_mut())
        .zip(def.color.iter().take(MAX_KEYS))
    {
        *t = k.t;
        *c = k.value;
    }
    let mut size_t = [1.0f32; 4];
    let mut sizes = [0.0f32; 4];
    for ((t, s), k) in size_t
        .iter_mut()
        .zip(sizes.iter_mut())
        .zip(def.size.iter().take(MAX_KEYS))
    {
        *t = k.t;
        *s = k.value[0];
    }
    let count = |n: usize| u32::try_from(n.min(MAX_KEYS)).unwrap_or(1).max(1);
    GpuEmitterDef {
        drag: def.drag,
        shape,
        space: match def.space {
            SimulationSpace::World => 0,
            SimulationSpace::Local => 1,
        },
        blend: match def.blend {
            BlendMode::Additive => 0,
            BlendMode::Alpha => 1,
        },
        shape_a,
        shape_b,
        speed_min: def.speed_min,
        speed_max: def.speed_max,
        lifetime_min: def.lifetime_min,
        lifetime_max: def.lifetime_max,
        color_count: count(def.color.len()),
        size_count: count(def.size.len()),
        color_t,
        colors,
        size_t,
        sizes,
    }
}

/// The particle the shader spawns for emission `index` of `emitter`.
pub fn spawn_reference(emitter: &GpuEmitter, def: &GpuEmitterDef, index: u32) -> GpuParticle {
    let mut h = pcg_hash(emitter.seed ^ pcg_hash(index));
    let mut next = || {
        h = pcg_hash(h);
        unit_float(h)
    };
    let (u0, u1, u2, u3, u4, u5) = (next(), next(), next(), next(), next(), next());
    let mut offset = [0.0f32; 3];
    let dir;
    if def.shape == SHAPE_CONE {
        let cos_t = 1.0 + (def.shape_a - 1.0) * u0;
        let sin_t = (1.0 - cos_t * cos_t).max(0.0).sqrt();
        let (around_sin, around_cos) = sincos_turns(u1);
        dir = [sin_t * around_cos, cos_t, sin_t * around_sin];
        let r = def.shape_b * u2.sqrt();
        let (disc_sin, disc_cos) = sincos_turns(u3);
        offset = [r * disc_cos, 0.0, r * disc_sin];
    } else {
        let y = 1.0 - 2.0 * u0;
        let s = (1.0 - y * y).max(0.0).sqrt();
        let (around_sin, around_cos) = sincos_turns(u1);
        dir = [s * around_cos, y, s * around_sin];
        if def.shape == SHAPE_SPHERE {
            offset = scale(dir, def.shape_a * u2.powf(1.0 / 3.0));
        }
    }
    let speed = def.speed_min + (def.speed_max - def.speed_min) * u4;
    let lifetime = def.lifetime_min + (def.lifetime_max - def.lifetime_min) * u5;
    let mut position = scale(offset, emitter.scale);
    let mut velocity = scale(dir, speed * emitter.scale);
    if def.space == 0 {
        position = add(emitter.position, rotate(emitter.rotation, position));
        velocity = rotate(emitter.rotation, velocity);
    }
    GpuParticle {
        position,
        age: 0.0,
        velocity,
        lifetime,
    }
}

/// Runs the simulation shader for one emitter instance over `pool` (the whole particle
/// pool, indexed like the GPU buffer). Slots outside the pool are skipped, as on the GPU.
pub fn simulate_reference(pool: &mut [GpuParticle], emitter: &GpuEmitter, def: &GpuEmitterDef) {
    let capacity = emitter.capacity;
    for i in 0..capacity {
        let Some(slot) = emitter
            .base
            .checked_add(i)
            .and_then(|s| usize::try_from(s).ok())
            .and_then(|s| pool.get_mut(s))
        else {
            return;
        };
        let k = i.wrapping_add(capacity).wrapping_sub(emitter.spawn_start) % capacity;
        if k < emitter.spawn_count {
            *slot = spawn_reference(emitter, def, emitter.emit_base.wrapping_add(k));
            continue;
        }
        if emitter.flags & FLAG_RESET != 0 {
            *slot = GpuParticle::default();
            continue;
        }
        if slot.age.partial_cmp(&slot.lifetime) != Some(core::cmp::Ordering::Less) {
            continue;
        }
        let dt = emitter.dt;
        let mut v = add(slot.velocity, scale(emitter.acceleration, dt));
        v = scale(v, 1.0 / (1.0 + def.drag * dt));
        slot.position = add(slot.position, scale(v, dt));
        slot.velocity = v;
        slot.age += dt;
    }
}
