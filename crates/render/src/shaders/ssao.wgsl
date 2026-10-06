// Screen-space ambient occlusion from the depth prepass (reverse Z, infinite far plane).
// Normals come from depth derivatives; a hemisphere kernel is rotated per pixel by a hash
// so banding becomes noise. Output: 1 unoccluded, toward 0 occluded.

struct SsaoParams {
    proj: mat4x4<f32>,
    inv_proj: mat4x4<f32>,
    // width, height, 1 / width, 1 / height
    viewport: vec4<f32>,
    // radius (view units), strength, bias (view units), sample count
    settings: vec4<f32>,
    kernel: array<vec4<f32>, 16>,
}

@group(0) @binding(0) var<uniform> params: SsaoParams;
@group(0) @binding(1) var depth_texture: texture_depth_2d;

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

fn view_position(pixel: vec2<f32>, depth: f32) -> vec3<f32> {
    let uv = pixel * params.viewport.zw;
    let ndc = vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, depth, 1.0);
    let v = params.inv_proj * ndc;
    return v.xyz / v.w;
}

fn hash(p: vec2<f32>) -> f32 {
    return fract(sin(dot(p, vec2<f32>(12.9898, 78.233))) * 43758.5453);
}

@fragment
fn fs_ssao(in: FullscreenOut) -> @location(0) vec4<f32> {
    let size = vec2<i32>(textureDimensions(depth_texture));
    let pixel = vec2<i32>(in.clip.xy);
    let depth = textureLoad(depth_texture, clamp(pixel, vec2<i32>(0), size - 1), 0);
    let p = view_position(in.clip.xy, max(depth, 0.000001));
    // Derivatives before any branch: they need uniform control flow.
    var n = normalize(cross(dpdx(p), dpdy(p)));
    if (dot(n, p) > 0.0) {
        n = -n;
    }
    if (depth <= 0.0) {
        return vec4<f32>(1.0);
    }
    let angle = hash(in.clip.xy) * 6.2831853;
    let helper = select(vec3<f32>(1.0, 0.0, 0.0), vec3<f32>(0.0, 1.0, 0.0), abs(n.x) > 0.9);
    let t0 = normalize(cross(helper, n));
    let b0 = cross(n, t0);
    let t = t0 * cos(angle) + b0 * sin(angle);
    let b = cross(n, t);
    let radius = params.settings.x;
    let count = u32(params.settings.w);
    var occlusion = 0.0;
    for (var i = 0u; i < count; i = i + 1u) {
        let k = params.kernel[i].xyz;
        let s = p + (t * k.x + b * k.y + n * k.z) * radius;
        let clip = params.proj * vec4<f32>(s, 1.0);
        if (clip.w <= 0.0) {
            continue;
        }
        let ndc = clip.xy / clip.w;
        let sample_pixel = vec2<i32>(vec2<f32>(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5) * params.viewport.xy);
        if (any(sample_pixel < vec2<i32>(0)) || any(sample_pixel >= size)) {
            continue;
        }
        let scene_depth = textureLoad(depth_texture, sample_pixel, 0);
        if (scene_depth <= 0.0) {
            continue;
        }
        let scene = view_position(vec2<f32>(sample_pixel) + 0.5, scene_depth);
        // Left-handed view space (decision 0019) looks along +z: a scene point in front of
        // the sample has smaller z.
        let in_range = smoothstep(0.0, 1.0, radius / max(abs(p.z - scene.z), 0.0001));
        if (scene.z <= s.z - params.settings.z) {
            occlusion = occlusion + in_range;
        }
    }
    let ao = 1.0 - occlusion / max(f32(count), 1.0) * params.settings.y;
    return vec4<f32>(clamp(ao, 0.0, 1.0));
}
