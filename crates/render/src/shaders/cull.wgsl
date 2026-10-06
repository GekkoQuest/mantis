// Frustum culling: one invocation per instance. A visible instance increments its batch's
// indirect instance count and writes its index into the batch's slice of the visible list.
// Unused frustum planes are (0, 0, 0, +max) and never cull.

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

struct Cull {
    planes: array<vec4<f32>, 6>,
    instance_count: u32,
    pad0: u32,
    pad1: u32,
    pad2: u32,
}

@group(0) @binding(0) var<uniform> cull: Cull;
@group(0) @binding(1) var<storage, read> instances: array<Instance>;
@group(0) @binding(2) var<storage, read> batches: array<Batch>;
@group(0) @binding(3) var<storage, read_write> args: array<DrawArgs>;
@group(0) @binding(4) var<storage, read_write> visible: array<u32>;

@compute @workgroup_size(64)
fn cull_main(@builtin(global_invocation_id) id: vec3<u32>) {
    let i = id.x;
    if (i >= cull.instance_count) {
        return;
    }
    let s = instances[i].sphere;
    for (var p = 0u; p < 6u; p = p + 1u) {
        let plane = cull.planes[p];
        if (dot(plane.xyz, s.xyz) + plane.w < -s.w) {
            return;
        }
    }
    let b = instances[i].batch;
    let batch = batches[b];
    let slot = atomicAdd(&args[b].instance_count, 1u);
    if (slot < batch.capacity) {
        visible[batch.base + slot] = i;
    }
}
