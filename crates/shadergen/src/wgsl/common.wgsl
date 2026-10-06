// Shared declarations for every material permutation. Group 0: frame and pass view.
// Group 1: the GPU-driven scene. Group 2: the material.
//
// `VertexIn` and `deform_local` are generated per deformation (static, skinned, vertex
// animation) by shadergen; see `codegen::deform`.

struct Frame {
    view: mat4x4<f32>,
    proj: mat4x4<f32>,
    view_proj: mat4x4<f32>,
    inv_view_proj: mat4x4<f32>,
    // The previous frame's unjittered view projection (motion vectors).
    prev_view_proj: mat4x4<f32>,
    // xyz camera position, w seconds since the session started.
    camera_position: vec4<f32>,
    // width, height, 1 / width, 1 / height (pixels).
    viewport: vec4<f32>,
    // xyz direction sunlight travels, w unused.
    sun_direction: vec4<f32>,
    // rgb color times intensity, w 1 when shadows are enabled.
    sun_color: vec4<f32>,
    // Sky ambient as L1 SH: red, green, blue, each [L00, L1-1, L10, L11].
    sky_sh: array<vec4<f32>, 3>,
    cascade_view_proj: array<mat4x4<f32>, 4>,
    // Far view depth of each cascade.
    cascade_splits: vec4<f32>,
    // x cascade count, y depth bias, z normal offset in world units, w PCF radius in texels.
    cascade_params: vec4<f32>,
    // Tiles across, tiles down, depth slices, light count.
    cluster_dims: vec4<u32>,
    // near, far, slices / ln(far / near), tile width in pixels.
    cluster_depth: vec4<f32>,
    // x tile height in pixels.
    cluster_tile: vec4<f32>,
    // x 1 / sector size, y side of the probe sector table, z 1 when any sector's probes
    // are loaded, w unused.
    probe_grid: vec4<f32>,
    // xyz 1 / probe atlas size in texels, w ambient intensity.
    probe_atlas: vec4<f32>,
    // Reserved.
    indirect_params: vec4<f32>,
}

// Per pass: the camera, or one shadow cascade. Bound with a dynamic offset.
struct PassView {
    view_proj: mat4x4<f32>,
    // xyz eye position, w unused.
    position: vec4<f32>,
    // Reserved for pass-specific values.
    params: vec4<f32>,
}

struct Instance {
    model: mat4x4<f32>,
    sphere: vec4<f32>,
    batch: u32,
    // Lightmapped instances: the slot of their page in the lightmap page table.
    lightmap_page: u32,
    pad1: u32,
    pad2: u32,
    // Deformation data. Skinned: x palette base (in vec4s of `deform_pool`). Vertex
    // animation: x first texel minus the mesh's base vertex (wrapping), y vertices per
    // frame, z frame position (f32 bits; the fraction blends to the next frame), w frame
    // count. Static: unused.
    deform: vec4<u32>,
    // Lightmapped instances: lightmap uv = uv1 * xy + zw, in page coordinates.
    lightmap_rect: vec4<f32>,
}

// Per indirect draw: where this batch's slice of the visible list starts.
struct DrawParams {
    base: u32,
    pad0: u32,
    pad1: u32,
    pad2: u32,
}

struct MaterialParams {
    scalars: array<vec4<f32>, 4>,
    colors: array<vec4<f32>, 8>,
    texture_index: vec4<u32>,
}

@group(0) @binding(0) var<uniform> frame: Frame;
@group(0) @binding(1) var<uniform> pass_view: PassView;
@group(1) @binding(0) var<storage, read> instances: array<Instance>;
@group(1) @binding(1) var<storage, read> visible: array<u32>;
@group(1) @binding(2) var<uniform> draw: DrawParams;
// Bone palettes (three vec4 rows per bone) and baked vertex animations (position then
// normal per texel), shared by every deformed instance.
@group(1) @binding(3) var<storage, read> deform_pool: array<vec4<f32>>;
// Each instance's model matrix in the previous frame (its current one when it is new).
@group(1) @binding(4) var<storage, read> previous_models: array<mat4x4<f32>>;
@group(2) @binding(0) var<uniform> material: MaterialParams;
@group(2) @binding(1) var material_sampler: sampler;

// A vertex in model space after deformation.
struct LocalVertex {
    position: vec3<f32>,
    normal: vec3<f32>,
}

// The clip position is invariant: the depth prepass and the forward pass compile it in
// different modules and must produce bit-identical depth for the forward pass's
// depth test to accept the prepass's values.
struct VertexOut {
    @builtin(position) @invariant clip: vec4<f32>,
    @location(0) world_position: vec3<f32>,
    @location(1) world_normal: vec3<f32>,
    @location(2) uv0: vec2<f32>,
    @location(3) uv1: vec2<f32>,
    @location(4) view_depth: f32,
    @location(5) lightmap_uv: vec2<f32>,
    @location(6) @interpolate(flat) lightmap_page: u32,
    // Unjittered clip positions this frame and last frame (motion vectors). Rigid
    // motion only: the previous position reuses this frame's deformation.
    @location(7) motion_current: vec4<f32>,
    @location(8) motion_previous: vec4<f32>,
}

struct MaterialInput {
    uv0: vec2<f32>,
    uv1: vec2<f32>,
    world_position: vec3<f32>,
    world_normal: vec3<f32>,
    view_direction: vec3<f32>,
    time: f32,
    // Lightmapped permutations: page coordinates and page slot.
    lightmap_uv: vec2<f32>,
    lightmap_page: u32,
}

struct SurfaceOutput {
    base_color: vec3<f32>,
    alpha: f32,
    emissive: vec3<f32>,
}

fn instance_slot(instance_index: u32) -> u32 {
    return visible[draw.base + instance_index];
}

fn instance_of(instance_index: u32) -> Instance {
    return instances[instance_slot(instance_index)];
}

// Normals use the model matrix directly: instance transforms are similarities (scene and
// sector placement validation reject anything else). Non-uniform scale would need the
// inverse transpose and a format version bump.
fn transform_vertex(v: VertexIn, vertex_index: u32, instance_index: u32) -> VertexOut {
    let instance = instance_of(instance_index);
    let model = instance.model;
    let local = deform_local(v, vertex_index, instance);
    let world = model * vec4<f32>(local.position, 1.0);
    var out: VertexOut;
    out.clip = pass_view.view_proj * world;
    out.world_position = world.xyz;
    out.world_normal = normalize((model * vec4<f32>(local.normal, 0.0)).xyz);
    out.uv0 = v.uv0;
    out.uv1 = v.uv1;
    out.lightmap_uv = v.uv1 * instance.lightmap_rect.xy + instance.lightmap_rect.zw;
    out.lightmap_page = instance.lightmap_page;
    let previous = previous_models[instance_slot(instance_index)] * vec4<f32>(local.position, 1.0);
    out.motion_current = frame.view_proj * world;
    out.motion_previous = frame.prev_view_proj * previous;
    // Left-handed view space (decision 0019) looks along +z.
    out.view_depth = (frame.view * world).z;
    return out;
}

fn material_input(v: VertexOut, front_facing: bool) -> MaterialInput {
    var mi: MaterialInput;
    mi.uv0 = v.uv0;
    mi.uv1 = v.uv1;
    mi.world_position = v.world_position;
    let n = normalize(v.world_normal);
    mi.world_normal = select(-n, n, front_facing);
    mi.view_direction = normalize(frame.camera_position.xyz - v.world_position);
    mi.time = frame.camera_position.w;
    mi.lightmap_uv = v.lightmap_uv;
    mi.lightmap_page = v.lightmap_page;
    return mi;
}
