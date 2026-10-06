// UI: one instanced pass of quads (plan 8.6). Each instance is a `mantis_ui::UiQuad`:
// rounded rectangles with borders (kind 0) and MSDF glyphs (kind 1), clipped per quad,
// blended premultiplied over the composited frame. Coordinates are physical pixels,
// origin top-left, y down.

struct UiView {
    // Target width, height in pixels; 1 / width, 1 / height.
    size: vec4<f32>,
}

@group(0) @binding(0) var<uniform> ui_view: UiView;
@group(0) @binding(1) var ui_atlas: texture_2d<f32>;
@group(0) @binding(2) var ui_sampler: sampler;

struct QuadIn {
    @location(0) rect: vec4<f32>,
    @location(1) uv: vec4<f32>,
    @location(2) color: vec4<f32>,
    @location(3) border_color: vec4<f32>,
    @location(4) params: vec4<f32>,
    @location(5) clip: vec4<f32>,
}

struct QuadOut {
    @builtin(position) position: vec4<f32>,
    // Pixel position of this fragment.
    @location(0) pixel: vec2<f32>,
    @location(1) @interpolate(flat) uv: vec2<f32>,
    @location(7) @interpolate(flat) uv_max: vec2<f32>,
    @location(2) @interpolate(flat) rect: vec4<f32>,
    @location(3) @interpolate(flat) color: vec4<f32>,
    @location(4) @interpolate(flat) border_color: vec4<f32>,
    @location(5) @interpolate(flat) params: vec4<f32>,
    @location(6) @interpolate(flat) clip: vec4<f32>,
}

@vertex
fn vs_ui(q: QuadIn, @builtin(vertex_index) vertex: u32) -> QuadOut {
    // Two triangles: corners (0,0) (1,0) (0,1), (0,1) (1,0) (1,1).
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0),
    );
    let c = corners[vertex % 6u];
    // Exactly the quad's rect (the UI lays out antialiasing margins itself).
    let pixel = q.rect.xy + c * q.rect.zw;
    var out: QuadOut;
    out.position = vec4<f32>(pixel.x * 2.0 * ui_view.size.z - 1.0, 1.0 - pixel.y * 2.0 * ui_view.size.w, 0.0, 1.0);
    out.pixel = pixel;
    out.uv = q.uv.xy;
    out.uv_max = q.uv.zw;
    out.rect = q.rect;
    out.color = q.color;
    out.border_color = q.border_color;
    out.params = q.params;
    out.clip = q.clip;
    return out;
}

fn median3(v: vec3<f32>) -> f32 {
    return max(min(v.r, v.g), min(max(v.r, v.g), v.b));
}

// Signed distance (pixels, negative inside) to a rounded rectangle.
fn rounded_rect(p: vec2<f32>, rect: vec4<f32>, radius: f32) -> f32 {
    let half = rect.zw * 0.5;
    let r = min(radius, min(half.x, half.y));
    let q = abs(p - (rect.xy + half)) - half + vec2<f32>(r);
    return length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - r;
}

@fragment
fn fs_ui(v: QuadOut) -> @location(0) vec4<f32> {
    if (v.pixel.x < v.clip.x || v.pixel.y < v.clip.y || v.pixel.x >= v.clip.z || v.pixel.y >= v.clip.w) {
        discard;
    }
    if (v.params.z > 0.5) {
        // MSDF glyph: coverage from the median distance, scaled to screen pixels.
        let t = (v.pixel - v.rect.xy) / v.rect.zw;
        let sample = textureSampleLevel(ui_atlas, ui_sampler, mix(v.uv, v.uv_max, t), 0.0).rgb;
        let alpha = clamp((median3(sample) - 0.5) * v.params.w + 0.5, 0.0, 1.0);
        return v.color * alpha;
    }
    // Rounded rectangle with an inner border; one-pixel antialiasing (the UI crate's
    // `draw::reference` rasterizer defines this math).
    let d = rounded_rect(v.pixel, v.rect, v.params.x);
    let cover = clamp(0.5 - d, 0.0, 1.0);
    let b = select(0.0, clamp(d + v.params.y + 0.5, 0.0, 1.0), v.params.y > 0.0);
    return mix(v.color, v.border_color, b) * cover;
}
