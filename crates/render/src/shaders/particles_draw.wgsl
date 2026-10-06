// GPU particle drawing (plan 8.5): camera-facing billboards, six vertices per pool slot of
// one emitter instance, a procedural soft round sprite, color and size from the emitter's
// curves by normalized age. Dead slots collapse outside the clip volume. Output is linear
// HDR, premultiplied: the additive pipeline blends (One, One), the alpha pipeline
// (One, OneMinusSrcAlpha). Depth is tested against the reverse-Z scene depth, not written.

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

struct ParticleCamera {
    view_proj: mat4x4<f32>,
    right: vec4<f32>,
    up: vec4<f32>,
}

@group(0) @binding(0) var<uniform> camera: ParticleCamera;
@group(0) @binding(1) var<uniform> emitter: Emitter;
@group(0) @binding(2) var<storage, read> defs: array<EmitterDef>;
@group(0) @binding(3) var<storage, read> particles: array<Particle>;

const SPACE_LOCAL: u32 = 1u;

struct VertexOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) uv: vec2<f32>,
}

fn rotate(q: vec4<f32>, v: vec3<f32>) -> vec3<f32> {
    let t = 2.0 * cross(q.xyz, v);
    return v + q.w * t + cross(q.xyz, t);
}

// Piecewise-linear color over normalized age, clamped outside the keys.
fn color_at(index: u32, t: f32) -> vec4<f32> {
    let n = clamp(defs[index].color_count, 1u, 4u);
    if (t <= defs[index].color_t[0]) {
        return defs[index].colors[0];
    }
    for (var k = 1u; k < n; k = k + 1u) {
        let t0 = defs[index].color_t[k - 1u];
        let t1 = defs[index].color_t[k];
        if (t <= t1) {
            let f = select(1.0, (t - t0) / (t1 - t0), t1 > t0);
            let a = defs[index].colors[k - 1u];
            return a + (defs[index].colors[k] - a) * f;
        }
    }
    return defs[index].colors[n - 1u];
}

// Piecewise-linear size over normalized age, clamped outside the keys.
fn size_at(index: u32, t: f32) -> f32 {
    let n = clamp(defs[index].size_count, 1u, 4u);
    if (t <= defs[index].size_t[0]) {
        return defs[index].sizes[0];
    }
    for (var k = 1u; k < n; k = k + 1u) {
        let t0 = defs[index].size_t[k - 1u];
        let t1 = defs[index].size_t[k];
        if (t <= t1) {
            let f = select(1.0, (t - t0) / (t1 - t0), t1 > t0);
            let a = defs[index].sizes[k - 1u];
            return a + (defs[index].sizes[k] - a) * f;
        }
    }
    return defs[index].sizes[n - 1u];
}

@vertex
fn vs_main(@builtin(vertex_index) vertex: u32) -> VertexOut {
    var out: VertexOut;
    // Outside the clip volume: the whole triangle is discarded.
    out.clip = vec4<f32>(2.0, 2.0, 2.0, 1.0);
    out.color = vec4<f32>(0.0, 0.0, 0.0, 0.0);
    out.uv = vec2<f32>(0.0, 0.0);
    let local = vertex / 6u;
    let slot = emitter.base + local;
    if (local >= emitter.capacity || slot >= arrayLength(&particles)) {
        return out;
    }
    let p = particles[slot];
    if (!(p.age < p.lifetime)) {
        return out;
    }
    // Two triangles over quad corners 0 1 2 and 0 2 3, counter-clockwise from bottom left.
    var quad = array<u32, 6>(0u, 1u, 2u, 0u, 2u, 3u);
    let c = quad[vertex % 6u];
    let corner = vec2<f32>(select(-1.0, 1.0, c == 1u || c == 2u), select(-1.0, 1.0, c >= 2u));
    let index = emitter.def_index;
    let t = clamp(p.age / p.lifetime, 0.0, 1.0);
    let half_size = 0.5 * max(size_at(index, t), 0.0) * emitter.scale;
    var center = p.position;
    if (defs[index].space == SPACE_LOCAL) {
        center = emitter.position + rotate(emitter.rotation, p.position);
    }
    let world = center + (camera.right.xyz * corner.x + camera.up.xyz * corner.y) * half_size;
    out.clip = camera.view_proj * vec4<f32>(world, 1.0);
    out.color = color_at(index, t);
    out.uv = corner;
    return out;
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    // Soft round sprite: quadratic falloff to zero at the inscribed circle.
    let falloff = clamp(1.0 - dot(in.uv, in.uv), 0.0, 1.0);
    let alpha = clamp(in.color.a, 0.0, 1.0) * falloff * falloff;
    return vec4<f32>(max(in.color.rgb, vec3<f32>(0.0, 0.0, 0.0)) * alpha, alpha);
}
