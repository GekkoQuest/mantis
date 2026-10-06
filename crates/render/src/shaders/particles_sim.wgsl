// GPU particle simulation (plan 8.5). One dispatch per live emitter instance, one
// invocation per slot of its contiguous pool range. The CPU decides how many particles
// spawn this frame and where in the range's ring they go; the shader initializes those
// slots from a stateless hash of (seed, emission index) and integrates every other live
// slot. No atomics: each slot is touched by exactly one invocation.
//
// `mantis_render::particles::simulate_reference` mirrors this file operation for
// operation; change both together.

struct Particle {
    position: vec3<f32>,
    age: f32,
    velocity: vec3<f32>,
    lifetime: f32,
}

struct Emitter {
    position: vec3<f32>,
    dt: f32,
    rotation: vec4<f32>,
    base: u32,
    capacity: u32,
    spawn_start: u32,
    spawn_count: u32,
    emit_base: u32,
    seed: u32,
    def_index: u32,
    flags: u32,
    acceleration: vec3<f32>,
    time: f32,
    // Uniform scale of the instance: spawn offsets, speeds, acceleration, and sizes.
    scale: f32,
    pad0: f32,
    pad1: f32,
    pad2: f32,
}

struct EmitterDef {
    drag: f32,
    shape: u32,
    space: u32,
    blend: u32,
    shape_a: f32,
    shape_b: f32,
    speed_min: f32,
    speed_max: f32,
    lifetime_min: f32,
    lifetime_max: f32,
    color_count: u32,
    size_count: u32,
    color_t: vec4<f32>,
    colors: array<vec4<f32>, 4>,
    size_t: vec4<f32>,
    sizes: vec4<f32>,
}

@group(0) @binding(0) var<uniform> emitter: Emitter;
@group(0) @binding(1) var<storage, read> defs: array<EmitterDef>;
@group(0) @binding(2) var<storage, read_write> particles: array<Particle>;

const SHAPE_SPHERE: u32 = 1u;
const SHAPE_CONE: u32 = 2u;
const SPACE_WORLD: u32 = 0u;
const FLAG_RESET: u32 = 1u;

// PCG hash: a permuted congruential step on one word.
fn pcg(v: u32) -> u32 {
    let state = v * 747796405u + 2891336453u;
    let word = ((state >> ((state >> 28u) + 4u)) ^ state) * 277803737u;
    return (word >> 22u) ^ word;
}

// A float in (0, 1) from the top 24 bits; exact on every device.
fn unit(h: u32) -> f32 {
    return (f32(h >> 8u) + 0.5) * (1.0 / 16777216.0);
}

// Sine and cosine of `turns` full turns, by quadrant reduction and short polynomials, so
// the CPU reference reproduces it without depending on the device's transcendentals.
fn sincos_turns(turns: f32) -> vec2<f32> {
    let x = turns * 4.0;
    let q = floor(x + 0.5);
    let a = (x - q) * 1.5707964;
    let a2 = a * a;
    let s = a * (1.0 + a2 * (-0.16666667 + a2 * (0.008333334 + a2 * (-0.0001984127 + a2 * 0.0000027557319))));
    let c = 1.0 + a2 * (-0.5 + a2 * (0.041666668 + a2 * (-0.0013888889 + a2 * 0.0000248015873)));
    let k = u32(q) & 3u;
    if (k == 0u) {
        return vec2<f32>(s, c);
    } else if (k == 1u) {
        return vec2<f32>(c, -s);
    } else if (k == 2u) {
        return vec2<f32>(-s, -c);
    }
    return vec2<f32>(-c, s);
}

// Rotates `v` by the unit quaternion `q` (xyz vector part, w scalar).
fn rotate(q: vec4<f32>, v: vec3<f32>) -> vec3<f32> {
    let t = 2.0 * cross(q.xyz, v);
    return v + q.w * t + cross(q.xyz, t);
}

fn spawn_particle(index: u32) -> Particle {
    let d = defs[emitter.def_index];
    var h = pcg(emitter.seed ^ pcg(index));
    h = pcg(h);
    let u0 = unit(h);
    h = pcg(h);
    let u1 = unit(h);
    h = pcg(h);
    let u2 = unit(h);
    h = pcg(h);
    let u3 = unit(h);
    h = pcg(h);
    let u4 = unit(h);
    h = pcg(h);
    let u5 = unit(h);
    var offset = vec3<f32>(0.0, 0.0, 0.0);
    var dir: vec3<f32>;
    if (d.shape == SHAPE_CONE) {
        // shape_a holds cos(half-angle): directions uniform over the spherical cap.
        let cos_t = 1.0 + (d.shape_a - 1.0) * u0;
        let sin_t = sqrt(max(1.0 - cos_t * cos_t, 0.0));
        let phi = sincos_turns(u1);
        dir = vec3<f32>(sin_t * phi.y, cos_t, sin_t * phi.x);
        let r = d.shape_b * sqrt(u2);
        let psi = sincos_turns(u3);
        offset = vec3<f32>(r * psi.y, 0.0, r * psi.x);
    } else {
        let y = 1.0 - 2.0 * u0;
        let s = sqrt(max(1.0 - y * y, 0.0));
        let phi = sincos_turns(u1);
        dir = vec3<f32>(s * phi.y, y, s * phi.x);
        if (d.shape == SHAPE_SPHERE) {
            offset = dir * (d.shape_a * pow(u2, 1.0 / 3.0));
        }
    }
    let speed = d.speed_min + (d.speed_max - d.speed_min) * u4;
    let lifetime = d.lifetime_min + (d.lifetime_max - d.lifetime_min) * u5;
    var position = offset * emitter.scale;
    var velocity = dir * speed * emitter.scale;
    if (d.space == SPACE_WORLD) {
        position = emitter.position + rotate(emitter.rotation, position);
        velocity = rotate(emitter.rotation, velocity);
    }
    return Particle(position, 0.0, velocity, lifetime);
}

@compute @workgroup_size(64)
fn simulate(@builtin(global_invocation_id) id: vec3<u32>) {
    let i = id.x;
    if (i >= emitter.capacity) {
        return;
    }
    let slot = emitter.base + i;
    if (slot >= arrayLength(&particles)) {
        return;
    }
    // Position of this slot in the range's ring, counted from this frame's first spawn.
    let k = (i + emitter.capacity - emitter.spawn_start) % emitter.capacity;
    if (k < emitter.spawn_count) {
        particles[slot] = spawn_particle(emitter.emit_base + k);
        return;
    }
    if ((emitter.flags & FLAG_RESET) != 0u) {
        // A fresh instance: clear whatever a previous owner of the range left behind.
        particles[slot] = Particle(vec3<f32>(0.0, 0.0, 0.0), 0.0, vec3<f32>(0.0, 0.0, 0.0), 0.0);
        return;
    }
    var p = particles[slot];
    if (!(p.age < p.lifetime)) {
        return;
    }
    let dt = emitter.dt;
    let drag = defs[emitter.def_index].drag;
    // Semi-implicit Euler with implicit drag.
    var v = p.velocity + emitter.acceleration * dt;
    v = v * (1.0 / (1.0 + drag * dt));
    p.position = p.position + v * dt;
    p.velocity = v;
    p.age = p.age + dt;
    particles[slot] = p;
}
