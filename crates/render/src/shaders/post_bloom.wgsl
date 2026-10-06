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
// Bloom
// ---------------------------------------------------------------------------------------

struct BloomParams {
    // x threshold, y soft knee, z 1 for the first (thresholded) downsample, w unused
    settings: vec4<f32>,
    // xy: 1 / size of the downsample source, zw: 1 / size of the downsample target. The
    // upsample into level i uses the slot of level i + 1, so xy is its target texel and zw
    // its source texel.
    texel: vec4<f32>,
}

@group(0) @binding(0) var<uniform> bloom: BloomParams;
@group(0) @binding(1) var bloom_source: texture_2d<f32>;
@group(0) @binding(2) var bloom_sampler: sampler;

fn threshold(c: vec3<f32>) -> vec3<f32> {
    let brightness = max(c.r, max(c.g, c.b));
    let knee = bloom.settings.x * bloom.settings.y;
    let soft = clamp(brightness - bloom.settings.x + knee, 0.0, 2.0 * knee);
    let contribution = max(soft * soft / (4.0 * knee + 0.00001), brightness - bloom.settings.x) / max(brightness, 0.00001);
    return c * max(contribution, 0.0);
}

// Four bilinear taps: a 4x4 box over the source.
@fragment
fn fs_bloom_down(in: FullscreenOut) -> @location(0) vec4<f32> {
    let uv = in.clip.xy * bloom.texel.zw;
    let o = bloom.texel.xy;
    var c = textureSampleLevel(bloom_source, bloom_sampler, uv + vec2<f32>(-o.x, -o.y), 0.0).rgb;
    c = c + textureSampleLevel(bloom_source, bloom_sampler, uv + vec2<f32>(o.x, -o.y), 0.0).rgb;
    c = c + textureSampleLevel(bloom_source, bloom_sampler, uv + vec2<f32>(-o.x, o.y), 0.0).rgb;
    c = c + textureSampleLevel(bloom_source, bloom_sampler, uv + vec2<f32>(o.x, o.y), 0.0).rgb;
    c = c * 0.25;
    if (bloom.settings.z > 0.5) {
        c = threshold(c);
    }
    return vec4<f32>(c, 1.0);
}

// 3x3 tent, added onto the next finer level (additive blending in the pipeline).
@fragment
fn fs_bloom_up(in: FullscreenOut) -> @location(0) vec4<f32> {
    let uv = in.clip.xy * bloom.texel.xy;
    let o = bloom.texel.zw;
    var c = vec3<f32>(0.0);
    for (var y = -1; y <= 1; y = y + 1) {
        for (var x = -1; x <= 1; x = x + 1) {
            let w = (2.0 - abs(f32(x))) * (2.0 - abs(f32(y)));
            c = c + w * textureSampleLevel(bloom_source, bloom_sampler, uv + vec2<f32>(f32(x), f32(y)) * o, 0.0).rgb;
        }
    }
    return vec4<f32>(c / 16.0, 1.0);
}

