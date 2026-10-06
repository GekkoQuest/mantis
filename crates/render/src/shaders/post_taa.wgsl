// Post-processing: temporal anti-aliasing, bloom, and the final composite (sharpening,
// bloom, exposure, ACES tone mapping, color grading through a 3D lookup table).
// Every pass is a fullscreen triangle.

struct FullscreenOut {
    @builtin(position) clip: vec4<f32>,
}

@vertex
fn vs_fullscreen(@builtin(vertex_index) i: u32) -> FullscreenOut {
    let uv = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    var out: FullscreenOut;
    out.clip = vec4<f32>(uv * 2.0 - 1.0, 0.0, 1.0);
    return out;
}

// ---------------------------------------------------------------------------------------
// TAA
// ---------------------------------------------------------------------------------------

struct TaaParams {
    // Current unjittered clip space to the previous frame's clip space.
    reproject: mat4x4<f32>,
    // width, height, 1 / width, 1 / height
    viewport: vec4<f32>,
    // x weight of the current frame, y 1 when history is valid, z/w jitter in pixels
    settings: vec4<f32>,
}

@group(0) @binding(0) var<uniform> taa: TaaParams;
@group(0) @binding(1) var taa_current: texture_2d<f32>;
@group(0) @binding(2) var taa_depth: texture_depth_2d;
@group(0) @binding(3) var taa_history: texture_2d<f32>;
@group(0) @binding(4) var taa_sampler: sampler;
// Per-object motion in uv units (current minus previous); x above 50 where nothing was
// drawn.
@group(0) @binding(5) var taa_velocity: texture_2d<f32>;

fn to_ycocg(c: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(0.25 * c.r + 0.5 * c.g + 0.25 * c.b, 0.5 * c.r - 0.5 * c.b, -0.25 * c.r + 0.5 * c.g - 0.25 * c.b);
}

fn from_ycocg(c: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(c.x + c.y - c.z, c.x + c.z, c.x - c.y - c.z);
}

@fragment
fn fs_taa(in: FullscreenOut) -> @location(0) vec4<f32> {
    let size = vec2<i32>(textureDimensions(taa_current));
    let p = vec2<i32>(in.clip.xy);
    let current = textureLoad(taa_current, p, 0).rgb;
    // Neighborhood bounds of the current frame, in YCoCg.
    var lo = vec3<f32>(1e30);
    var hi = vec3<f32>(-1e30);
    for (var y = -1; y <= 1; y = y + 1) {
        for (var x = -1; x <= 1; x = x + 1) {
            let q = clamp(p + vec2<i32>(x, y), vec2<i32>(0), size - 1);
            let c = to_ycocg(textureLoad(taa_current, q, 0).rgb);
            lo = min(lo, c);
            hi = max(hi, c);
        }
    }
    // Where a surface was drawn, its own motion (objects and camera); elsewhere (the sky)
    // reproject through depth and the camera alone.
    let uv = (in.clip.xy - taa.settings.zw) * taa.viewport.zw;
    let velocity = textureLoad(taa_velocity, p, 0).xy;
    var prev_uv = uv - velocity;
    var ahead = true;
    if (velocity.x > 50.0) {
        let depth = textureLoad(taa_depth, p, 0);
        let ndc = vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, depth, 1.0);
        let prev = taa.reproject * ndc;
        prev_uv = vec2<f32>(prev.x / prev.w * 0.5 + 0.5, 0.5 - prev.y / prev.w * 0.5);
        ahead = prev.w > 0.0;
    }
    let inside = all(prev_uv >= vec2<f32>(0.0)) && all(prev_uv <= vec2<f32>(1.0)) && ahead;
    let history = textureSampleLevel(taa_history, taa_sampler, clamp(prev_uv, vec2<f32>(0.0), vec2<f32>(1.0)), 0.0).rgb;
    let clamped = from_ycocg(clamp(to_ycocg(history), lo, hi));
    if (taa.settings.y < 0.5 || !inside) {
        return vec4<f32>(current, 1.0);
    }
    return vec4<f32>(mix(clamped, current, taa.settings.x), 1.0);
}

