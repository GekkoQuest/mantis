// One level of the depth pyramid from the level above (see pyramid_base.wgsl).

@group(0) @binding(0) var source_level: texture_2d<f32>;
@group(0) @binding(1) var next_level: texture_storage_2d<r32float, write>;

// The minimum (farthest) depth of the source texels this texel covers: 2x2, widened to
// 3 along an axis whose source size is odd so no source texel is skipped.
@compute @workgroup_size(8, 8)
fn pyramid_reduce(@builtin(global_invocation_id) id: vec3<u32>) {
    let size = textureDimensions(next_level);
    if (id.x >= size.x || id.y >= size.y) {
        return;
    }
    let src = vec2<i32>(textureDimensions(source_level));
    let base = vec2<i32>(id.xy) * 2;
    let span = vec2<i32>(select(2, 3, (src.x & 1) == 1 && i32(id.x) == i32(size.x) - 1), select(2, 3, (src.y & 1) == 1 && i32(id.y) == i32(size.y) - 1));
    var farthest = 1.0;
    for (var y = 0; y < span.y; y = y + 1) {
        for (var x = 0; x < span.x; x = x + 1) {
            let p = min(base + vec2<i32>(x, y), src - 1);
            farthest = min(farthest, textureLoad(source_level, p, 0).r);
        }
    }
    textureStore(next_level, vec2<i32>(id.xy), vec4<f32>(farthest, 0.0, 0.0, 0.0));
}
