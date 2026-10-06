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
// Composite
// ---------------------------------------------------------------------------------------

struct CompositeParams {
    // x exposure, y bloom intensity, z sharpening 0 to 1, w unused
    settings: vec4<f32>,
}

@group(0) @binding(0) var<uniform> composite: CompositeParams;
@group(0) @binding(1) var composite_color: texture_2d<f32>;
@group(0) @binding(2) var composite_bloom: texture_2d<f32>;
@group(0) @binding(3) var grading_lut: texture_3d<f32>;
@group(0) @binding(4) var composite_sampler: sampler;

fn aces(x: vec3<f32>) -> vec3<f32> {
    return clamp((x * (2.51 * x + 0.03)) / (x * (2.43 * x + 0.59) + 0.14), vec3<f32>(0.0), vec3<f32>(1.0));
}

@fragment
fn fs_composite(in: FullscreenOut) -> @location(0) vec4<f32> {
    let size = vec2<i32>(textureDimensions(composite_color));
    let p = vec2<i32>(in.clip.xy);
    let center = textureLoad(composite_color, p, 0).rgb;
    let n = textureLoad(composite_color, clamp(p + vec2<i32>(0, -1), vec2<i32>(0), size - 1), 0).rgb;
    let s = textureLoad(composite_color, clamp(p + vec2<i32>(0, 1), vec2<i32>(0), size - 1), 0).rgb;
    let e = textureLoad(composite_color, clamp(p + vec2<i32>(1, 0), vec2<i32>(0), size - 1), 0).rgb;
    let w = textureLoad(composite_color, clamp(p + vec2<i32>(-1, 0), vec2<i32>(0), size - 1), 0).rgb;
    // Contrast-adaptive sharpening: a negative-lobe filter scaled down where local
    // contrast is already high, so edges do not ring.
    let lo = min(center, min(min(n, s), min(e, w)));
    let hi = max(center, max(max(n, s), max(e, w)));
    let amount = sqrt(clamp(min(lo, 2.0 - hi) / max(hi, vec3<f32>(0.00001)), vec3<f32>(0.0), vec3<f32>(1.0)));
    let lobe = -amount * mix(0.0, 0.2, composite.settings.z);
    let sharpened = max((center + (n + s + e + w) * lobe) / (1.0 + 4.0 * lobe), vec3<f32>(0.0));
    let uv = in.clip.xy / vec2<f32>(size);
    let bloomed = sharpened + textureSampleLevel(composite_bloom, composite_sampler, uv, 0.0).rgb * composite.settings.y;
    let ldr = aces(bloomed * composite.settings.x);
    let lut_size = f32(textureDimensions(grading_lut).x);
    let lut_uv = ldr * ((lut_size - 1.0) / lut_size) + 0.5 / lut_size;
    return vec4<f32>(textureSampleLevel(grading_lut, composite_sampler, lut_uv, 0.0).rgb, 1.0);
}
