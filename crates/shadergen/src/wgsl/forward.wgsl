@vertex
fn vs_main(
    v: VertexIn,
    @builtin(vertex_index) vertex_index: u32,
    @builtin(instance_index) instance_index: u32,
) -> VertexOut {
    return transform_vertex(v, vertex_index, instance_index);
}

@fragment
fn fs_main(v: VertexOut, @builtin(front_facing) front_facing: bool) -> @location(0) vec4<f32> {
    let mi = material_input(v, front_facing);
    let s = material_surface(mi);
    if (ALPHA_TEST && s.alpha < ALPHA_CUTOFF) {
        discard;
    }
    return vec4<f32>(shade(s, mi, v.clip.xy, v.view_depth), s.alpha);
}
