// Linear-blend skinning. A second vertex stream carries up to four bone indices and
// weights; the instance's palette (three affine rows per bone) lives in `deform_pool`
// from `instance.deform.x`. Weights are renormalized (8-bit weights rarely sum to one).
struct VertexIn {
    @location(0) position: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) uv0: vec2<f32>,
    @location(3) uv1: vec2<f32>,
    @location(4) joints: vec4<u32>,
    @location(5) weights: vec4<f32>,
}

fn deform_local(v: VertexIn, vertex_index: u32, instance: Instance) -> LocalVertex {
    let base = instance.deform.x;
    let total = max(dot(v.weights, vec4<f32>(1.0)), 0.000001);
    var r0 = vec4<f32>(0.0);
    var r1 = vec4<f32>(0.0);
    var r2 = vec4<f32>(0.0);
    for (var i = 0u; i < 4u; i = i + 1u) {
        let w = v.weights[i] / total;
        let row = base + v.joints[i] * 3u;
        r0 = r0 + w * deform_pool[row];
        r1 = r1 + w * deform_pool[row + 1u];
        r2 = r2 + w * deform_pool[row + 2u];
    }
    let p = vec4<f32>(v.position, 1.0);
    let n = vec4<f32>(v.normal, 0.0);
    return LocalVertex(vec3<f32>(dot(r0, p), dot(r1, p), dot(r2, p)), vec3<f32>(dot(r0, n), dot(r1, n), dot(r2, n)));
}
