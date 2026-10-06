// Depth prepass and shadow maps share this entry: the pass view supplies the camera or
// the cascade. The prepass writes velocity; shadow maps bind a fragment stage only for
// alpha-tested materials.
@vertex
fn vs_main(
    v: VertexIn,
    @builtin(vertex_index) vertex_index: u32,
    @builtin(instance_index) instance_index: u32,
) -> VertexOut {
    return transform_vertex(v, vertex_index, instance_index);
}

// The depth prepass: depth plus the velocity target, the screen-space motion of the
// surface since the previous frame in uv units (current minus previous), unjittered.
@fragment
fn fs_velocity(v: VertexOut, @builtin(front_facing) front_facing: bool) -> @location(0) vec2<f32> {
    if (ALPHA_TEST) {
        let s = material_surface(material_input(v, front_facing));
        if (s.alpha < ALPHA_CUTOFF) {
            discard;
        }
    }
    let current = v.motion_current.xy / v.motion_current.w;
    let previous = v.motion_previous.xy / v.motion_previous.w;
    return (current - previous) * vec2<f32>(0.5, -0.5);
}

@fragment
fn fs_main(v: VertexOut, @builtin(front_facing) front_facing: bool) {
    let s = material_surface(material_input(v, front_facing));
    if (ALPHA_TEST && s.alpha < ALPHA_CUTOFF) {
        discard;
    }
}
