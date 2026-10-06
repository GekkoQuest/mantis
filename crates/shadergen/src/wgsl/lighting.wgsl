// Forward lighting: cooked indirect light (probe volume or lightmap, sky fallback), SSAO,
// the sun with cascaded shadows, and clustered point lights. The direct-light response
// comes from the material (material_diffuse), so toon ramps apply to every light.

struct PointLight {
    position: vec3<f32>,
    range: f32,
    color: vec3<f32>,
    intensity: f32,
}

@group(0) @binding(2) var shadow_map: texture_depth_2d_array;
@group(0) @binding(3) var shadow_sampler: sampler_comparison;
@group(0) @binding(4) var<storage, read> cluster_grid: array<vec2<u32>>;
@group(0) @binding(5) var<storage, read> light_indices: array<u32>;
@group(0) @binding(6) var<storage, read> point_lights: array<PointLight>;
@group(0) @binding(7) var probe_red: texture_3d<f32>;
@group(0) @binding(8) var probe_green: texture_3d<f32>;
@group(0) @binding(9) var probe_blue: texture_3d<f32>;
@group(0) @binding(10) var linear_clamp: sampler;
// Lightmap pages: every resident sector's lightmap keyframes, one layer each.
@group(0) @binding(11) var lightmap: texture_2d_array<f32>;
@group(0) @binding(12) var ssao_texture: texture_2d<f32>;
// Per page slot: x first layer, y second layer, z blend weight, w unused.
@group(0) @binding(13) var<storage, read> lightmap_pages: array<vec4<f32>>;
@group(0) @binding(14) var<storage, read> probe_sectors: array<ProbeSector>;

// One slot of the toroidal probe sector table.
struct ProbeSector {
    // x and y sector coordinates (x, z), z 1 when this slot holds them, w unused.
    coord: vec4<i32>,
    // xyz world position of probe (0, 0, 0), w unused.
    origin: vec4<f32>,
    // xyz 1 / (spacing * (n - 1)) per axis (0 for a single probe), w unused.
    inv_extent: vec4<f32>,
    // xyz center of the brick's first texel in the atlas, in texels, w unused.
    brick: vec4<f32>,
    // xyz probes minus one per axis, w unused.
    span: vec4<f32>,
}

const SH_Y0: f32 = 0.282095;
const SH_Y1: f32 = 0.488603;
const SH_A0: f32 = 3.14159265;
const SH_A1: f32 = 2.09439510;
const INV_PI: f32 = 0.31830989;

// Irradiance divided by pi from one channel of L1 coefficients, for unit normal n.
fn sh_channel(c: vec4<f32>, n: vec3<f32>) -> f32 {
    return max((SH_A0 * SH_Y0 * c.x + SH_A1 * SH_Y1 * (c.y * n.y + c.z * n.z + c.w * n.x)) * INV_PI, 0.0);
}

fn sky_ambient(n: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(sh_channel(frame.sky_sh[0], n), sh_channel(frame.sky_sh[1], n), sh_channel(frame.sky_sh[2], n));
}

fn floor_mod(a: i32, n: i32) -> i32 {
    return ((a % n) + n) % n;
}

// Probe light at p: the sector containing p finds its brick through the toroidal table;
// outside every loaded sector, the sky.
fn indirect_ambient(p: vec3<f32>, n: vec3<f32>) -> vec3<f32> {
    let intensity = frame.probe_atlas.w;
    if (frame.probe_grid.z < 0.5) {
        return sky_ambient(n) * intensity;
    }
    let s = vec2<i32>(floor(p.xz * frame.probe_grid.x));
    let side = max(i32(frame.probe_grid.y), 1);
    let slot = u32(floor_mod(s.x, side) + floor_mod(s.y, side) * side);
    let rec = probe_sectors[slot];
    if (rec.coord.z == 0 || rec.coord.x != s.x || rec.coord.y != s.y) {
        return sky_ambient(n) * intensity;
    }
    let local = clamp((p - rec.origin.xyz) * rec.inv_extent.xyz, vec3<f32>(0.0), vec3<f32>(1.0));
    let uvw = (rec.brick.xyz + local * rec.span.xyz) * frame.probe_atlas.xyz;
    let r = textureSampleLevel(probe_red, linear_clamp, uvw, 0.0);
    let g = textureSampleLevel(probe_green, linear_clamp, uvw, 0.0);
    let b = textureSampleLevel(probe_blue, linear_clamp, uvw, 0.0);
    return vec3<f32>(sh_channel(r, n), sh_channel(g, n), sh_channel(b, n)) * intensity;
}

// Lightmap light: the page's two keyframe layers, blended.
fn lightmap_ambient(uv: vec2<f32>, page: u32) -> vec3<f32> {
    let blend = lightmap_pages[page];
    let a = textureSampleLevel(lightmap, linear_clamp, uv, i32(blend.x), 0.0).rgb;
    let b = textureSampleLevel(lightmap, linear_clamp, uv, i32(blend.y), 0.0).rgb;
    return mix(a, b, blend.z) * frame.probe_atlas.w;
}

// 1 lit, 0 shadowed: 3x3 PCF in the cascade that covers view_depth.
fn sun_shadow(p: vec3<f32>, n: vec3<f32>, view_depth: f32) -> f32 {
    if (frame.sun_color.w < 0.5) {
        return 1.0;
    }
    let count = min(u32(frame.cascade_params.x), 4u);
    var cascade = count;
    for (var i = 0u; i < count; i = i + 1u) {
        if (view_depth <= frame.cascade_splits[i]) {
            cascade = i;
            break;
        }
    }
    if (cascade >= count) {
        return 1.0;
    }
    let offset_p = p + n * frame.cascade_params.z;
    let clip = frame.cascade_view_proj[cascade] * vec4<f32>(offset_p, 1.0);
    let ndc = clip.xyz / clip.w;
    let uv = vec2<f32>(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5);
    let depth = ndc.z - frame.cascade_params.y;
    let texel = 1.0 / f32(textureDimensions(shadow_map).x);
    var lit = 0.0;
    for (var y = -1; y <= 1; y = y + 1) {
        for (var x = -1; x <= 1; x = x + 1) {
            let o = vec2<f32>(f32(x), f32(y)) * texel * frame.cascade_params.w;
            lit = lit + textureSampleCompareLevel(shadow_map, shadow_sampler, uv + o, i32(cascade), depth);
        }
    }
    return lit / 9.0;
}

fn cluster_index(frag: vec2<f32>, view_depth: f32) -> u32 {
    let dims = frame.cluster_dims;
    let tx = min(u32(frag.x / frame.cluster_depth.w), dims.x - 1u);
    let ty = min(u32(frag.y / frame.cluster_tile.x), dims.y - 1u);
    let d = max(view_depth, frame.cluster_depth.x);
    let slice = min(u32(max(log(d / frame.cluster_depth.x) * frame.cluster_depth.z, 0.0)), dims.z - 1u);
    return tx + ty * dims.x + slice * dims.x * dims.y;
}

// A smooth window to zero at the range, inverse-square inside it.
fn light_falloff(d: f32, range: f32) -> f32 {
    let x = d / range;
    let window = clamp(1.0 - x * x * x * x, 0.0, 1.0);
    return window * window / (d * d + 1.0);
}

fn shade(s: SurfaceOutput, mi: MaterialInput, frag: vec2<f32>, view_depth: f32) -> vec3<f32> {
    if (MATERIAL_UNLIT) {
        return s.base_color + s.emissive;
    }
    let p = mi.world_position;
    let n = mi.world_normal;
    let ao = textureSampleLevel(ssao_texture, linear_clamp, frag * frame.viewport.zw, 0.0).r;
    var ambient: vec3<f32>;
    if (LIGHTMAPPED) {
        ambient = lightmap_ambient(mi.lightmap_uv, mi.lightmap_page);
    } else {
        ambient = indirect_ambient(p, n);
    }
    var color = ambient * ao * s.base_color;
    // Lightmaps hold the sun with its baked shadows (keyframed with the time of day), so
    // lightmapped surfaces take no dynamic sun; everything else does.
    if (!LIGHTMAPPED) {
        let sun = dot(n, -frame.sun_direction.xyz) * sun_shadow(p, n, view_depth);
        color = color + material_diffuse(sun, frame.sun_color.rgb) * s.base_color;
    }
    if (frame.cluster_dims.w > 0u) {
        let range = cluster_grid[cluster_index(frag, view_depth)];
        for (var i = 0u; i < range.y; i = i + 1u) {
            let light = point_lights[light_indices[range.x + i]];
            let to_light = light.position - p;
            let d = length(to_light);
            let l = to_light / max(d, 0.0001);
            let radiance = light.color * light.intensity * light_falloff(d, light.range);
            color = color + material_diffuse(dot(n, l), radiance) * s.base_color;
        }
    }
    return color + material_rim(n, mi.view_direction) + s.emissive;
}
