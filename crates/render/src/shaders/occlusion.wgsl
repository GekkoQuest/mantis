// Two-phase GPU occlusion culling for the camera view (plan 8.3).
//
// Early: instances that were visible last frame (their bit in `visibility`) and inside
// the frustum go to the early list; the depth prepass draws them and the depth pyramid
// is built from that depth. Late: every instance inside the frustum is tested against
// the pyramid; the result becomes its visibility bit for the next frame, and visible
// instances not already in the early list go to the late list, which the prepass draws
// next. Everything visible is drawn by one list or the other in the same frame, so
// nothing pops in when the camera moves; occlusion only saves work.
//
// Depth is reverse Z (nearer is greater). The pyramid holds the farthest (minimum) depth
// of each region, so a bound whose nearest point is farther than that is hidden.

struct Instance {
    model: mat4x4<f32>,
    sphere: vec4<f32>,
    batch: u32,
    lightmap_page: u32,
    pad1: u32,
    pad2: u32,
    deform: vec4<u32>,
    lightmap_rect: vec4<f32>,
}

struct Batch {
    base: u32,
    capacity: u32,
    pad0: u32,
    pad1: u32,
}

struct DrawArgs {
    index_count: u32,
    instance_count: atomic<u32>,
    first_index: u32,
    base_vertex: i32,
    first_instance: u32,
}

struct Occlusion {
    planes: array<vec4<f32>, 6>,
    // Unjittered camera world to clip.
    view_proj: mat4x4<f32>,
    // x pyramid width, y height, z mip levels, w 1 when the pyramid is valid.
    pyramid: vec4<f32>,
    instance_count: u32,
    pad0: u32,
    pad1: u32,
    pad2: u32,
}

@group(0) @binding(0) var<uniform> occ: Occlusion;
@group(0) @binding(1) var<storage, read> instances: array<Instance>;
@group(0) @binding(2) var<storage, read> batches: array<Batch>;
@group(0) @binding(3) var<storage, read_write> args: array<DrawArgs>;
@group(0) @binding(4) var<storage, read_write> visible: array<u32>;
@group(0) @binding(5) var<storage, read_write> visibility: array<u32>;
@group(0) @binding(6) var pyramid: texture_2d<f32>;

fn in_frustum(s: vec4<f32>) -> bool {
    for (var p = 0u; p < 6u; p = p + 1u) {
        let plane = occ.planes[p];
        if (dot(plane.xyz, s.xyz) + plane.w < -s.w) {
            return false;
        }
    }
    return true;
}

fn append(i: u32) {
    let b = instances[i].batch;
    let batch = batches[b];
    let slot = atomicAdd(&args[b].instance_count, 1u);
    if (slot < batch.capacity) {
        visible[batch.base + slot] = i;
    }
}

// True when the sphere is certainly hidden behind the pyramid's depth.
fn occluded(s: vec4<f32>) -> bool {
    if (occ.pyramid.w < 0.5) {
        return false;
    }
    var lo = vec2<f32>(1e30);
    var hi = vec2<f32>(-1e30);
    var nearest = 0.0;
    for (var c = 0u; c < 8u; c = c + 1u) {
        let corner = s.xyz + vec3<f32>(
            select(-s.w, s.w, (c & 1u) != 0u),
            select(-s.w, s.w, (c & 2u) != 0u),
            select(-s.w, s.w, (c & 4u) != 0u),
        );
        let clip = occ.view_proj * vec4<f32>(corner, 1.0);
        // A corner at or behind the eye: the bound crosses the near plane.
        if (clip.w <= 1e-4) {
            return false;
        }
        let ndc = clip.xyz / clip.w;
        let uv = vec2<f32>(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5);
        lo = min(lo, uv);
        hi = max(hi, uv);
        nearest = max(nearest, ndc.z);
    }
    let size = occ.pyramid.xy;
    // One texel of margin for the projection jitter.
    let px_lo = clamp(lo * size - 1.0, vec2<f32>(0.0), size - 1.0);
    let px_hi = clamp(hi * size + 1.0, vec2<f32>(0.0), size - 1.0);
    let extent = max(px_hi.x - px_lo.x, px_hi.y - px_lo.y);
    let levels = u32(occ.pyramid.z);
    let level = min(u32(ceil(log2(max(extent, 1.0)))), levels - 1u);
    let dims = vec2<i32>(textureDimensions(pyramid, level));
    let a = clamp(vec2<i32>(px_lo) >> vec2<u32>(level), vec2<i32>(0), dims - 1);
    let b = clamp(vec2<i32>(px_hi) >> vec2<u32>(level), vec2<i32>(0), dims - 1);
    // At this level the rectangle spans at most two texels per axis.
    var farthest = 1.0;
    for (var y = a.y; y <= b.y; y = y + 1) {
        for (var x = a.x; x <= b.x; x = x + 1) {
            farthest = min(farthest, textureLoad(pyramid, vec2<i32>(x, y), i32(level)).r);
        }
    }
    return nearest < farthest;
}

@compute @workgroup_size(64)
fn cull_early(@builtin(global_invocation_id) id: vec3<u32>) {
    let i = id.x;
    if (i >= occ.instance_count) {
        return;
    }
    if (visibility[i] != 0u && in_frustum(instances[i].sphere)) {
        append(i);
    }
}

@compute @workgroup_size(64)
fn cull_late(@builtin(global_invocation_id) id: vec3<u32>) {
    let i = id.x;
    if (i >= occ.instance_count) {
        return;
    }
    let s = instances[i].sphere;
    let was = visibility[i] != 0u;
    let vis = in_frustum(s) && !occluded(s);
    visibility[i] = select(0u, 1u, vis);
    if (vis && !was) {
        append(i);
    }
}
