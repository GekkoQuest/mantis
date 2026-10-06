// Vertex animation: positions and normals baked per frame (frame-major, two vec4 per
// texel in `deform_pool`), blended between the two frames around the instance's frame
// position. The mesh's own position and normal are unused.
struct VertexIn {
    @location(0) position: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) uv0: vec2<f32>,
    @location(3) uv1: vec2<f32>,
}

fn deform_local(v: VertexIn, vertex_index: u32, instance: Instance) -> LocalVertex {
    let d = instance.deform;
    let frame = bitcast<f32>(d.z);
    let whole = floor(frame);
    let t = frame - whole;
    let f0 = u32(whole);
    let f1 = select(f0 + 1u, 0u, f0 + 1u >= d.w);
    let local = d.x + vertex_index;
    let a = (local + f0 * d.y) * 2u;
    let b = (local + f1 * d.y) * 2u;
    let position = mix(deform_pool[a].xyz, deform_pool[b].xyz, t);
    let normal = mix(deform_pool[a + 1u].xyz, deform_pool[b + 1u].xyz, t);
    return LocalVertex(position, normal);
}
