// Inverted hull: the mesh pushed out along its normal by a constant width in pixels, drawn
// with front faces culled so only the silhouette rim shows.
@vertex
fn vs_main(
    v: VertexIn,
    @builtin(vertex_index) vertex_index: u32,
    @builtin(instance_index) instance_index: u32,
) -> VertexOut {
    var out = transform_vertex(v, vertex_index, instance_index);
    let n_clip = (pass_view.view_proj * vec4<f32>(out.world_normal, 0.0)).xy;
    let len = length(n_clip);
    if (len > 0.00001) {
        let offset = n_clip / len * OUTLINE_WIDTH_PX * 2.0 * frame.viewport.zw * out.clip.w;
        out.clip = vec4<f32>(out.clip.xy + offset, out.clip.zw);
    }
    return out;
}

@fragment
fn fs_main(v: VertexOut, @builtin(front_facing) front_facing: bool) -> @location(0) vec4<f32> {
    let s = material_surface(material_input(v, front_facing));
    if (ALPHA_TEST && s.alpha < ALPHA_CUTOFF) {
        discard;
    }
    return vec4<f32>(OUTLINE_COLOR, 1.0);
}
