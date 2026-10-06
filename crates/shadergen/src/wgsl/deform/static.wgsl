// Static geometry: the mesh's own vertices.
struct VertexIn {
    @location(0) position: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) uv0: vec2<f32>,
    @location(3) uv1: vec2<f32>,
}

fn deform_local(v: VertexIn, vertex_index: u32, instance: Instance) -> LocalVertex {
    return LocalVertex(v.position, v.normal);
}
