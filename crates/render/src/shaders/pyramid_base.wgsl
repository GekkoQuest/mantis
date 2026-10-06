// The depth pyramid of the occlusion culling: level 0 is the depth prepass's depth, each
// next level the minimum (farthest, reverse Z) of the texels it covers. One dispatch per
// level; `pyramid_base` and `pyramid_reduce` are separate pipelines with their own
// bindings.

@group(0) @binding(0) var source_depth: texture_depth_2d;
@group(0) @binding(1) var level0: texture_storage_2d<r32float, write>;

@compute @workgroup_size(8, 8)
fn pyramid_base(@builtin(global_invocation_id) id: vec3<u32>) {
    let size = textureDimensions(level0);
    if (id.x >= size.x || id.y >= size.y) {
        return;
    }
    textureStore(level0, vec2<i32>(id.xy), vec4<f32>(textureLoad(source_depth, vec2<i32>(id.xy), 0), 0.0, 0.0, 0.0));
}

