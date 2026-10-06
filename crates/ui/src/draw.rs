//! The draw list: one instanced quad pass in painter's order.
//!
//! [`DrawList::quads`] is the whole frame: backgrounds, borders, selection,
//! glyphs, IME underlines, and carets, back to front (slice order is
//! painter's order). It is cleared and refilled every frame without
//! allocating once its capacity has grown. The [`reference`](mod@reference) module is the
//! CPU rasterizer that defines the exact math the renderer's shader
//! implements.

use crate::atlas::GlyphAtlas;
use crate::color::Color;
use crate::layout::{Rect, clip_intersect, display_caret};
use crate::text::TextAlign;
use crate::tree::{Node, NodeId, NodeKind, StateStyle, Tree};

/// Quad kind: rounded rectangle with optional border.
pub const KIND_RECT: f32 = 0.0;
/// Quad kind: MSDF glyph.
pub const KIND_GLYPH: f32 = 1.0;

/// One instanced quad. Physical pixels, origin top-left, y down.
///
/// The renderer draws each quad as exactly its `rect` (two triangles, no
/// expansion), with premultiplied-alpha blending
/// (`dst = src + dst * (1 - src.a)`), in slice order. For every fragment at
/// pixel center `p`:
///
/// * **Clip.** Discard unless `clip.x <= p.x < clip.z` and
///   `clip.y <= p.y < clip.w`.
/// * **Kind 0, rounded rectangle.**
///   ```text
///   half   = rect.zw * 0.5
///   center = rect.xy + half
///   r      = min(params.x, min(half.x, half.y))
///   q      = abs(p - center) - (half - r)
///   d      = length(max(q, 0)) + min(max(q.x, q.y), 0) - r   // < 0 inside
///   cover  = clamp(0.5 - d, 0, 1)
///   b      = params.y > 0 ? clamp(d + params.y + 0.5, 0, 1) : 0
///   out    = mix(color, border_color, b) * cover
///   ```
/// * **Kind 1, MSDF glyph.**
///   ```text
///   t      = (p - rect.xy) / rect.zw
///   uv     = mix(uv.xy, uv.zw, t)
///   s      = textureSample(atlas, bilinear clamp-to-edge sampler, uv).rgb
///   md     = max(min(s.r, s.g), min(max(s.r, s.g), s.b))        // median
///   alpha  = clamp((md - 0.5) * params.w + 0.5, 0, 1)
///   out    = color * alpha
///   ```
///   `params.w` is the screen-pixel range: the atlas distance range in texels
///   ([`GlyphAtlas::msdf_range`]) times screen pixels per atlas texel at this
///   glyph's size. The atlas is RGBA8 with linear (not sRGB) encoding:
///   sample it as `Rgba8Unorm`.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct UiQuad {
    /// x, y, width, height.
    pub rect: [f32; 4],
    /// Atlas texture coordinates u0, v0, u1, v1 (normalized) for glyphs; zero otherwise.
    pub uv: [f32; 4],
    /// Fill color, linear premultiplied.
    pub color: [f32; 4],
    /// Border color, linear premultiplied.
    pub border_color: [f32; 4],
    /// x corner radius px, y border width px, z kind (0 = rounded rect, 1 = MSDF glyph),
    /// w MSDF screen-pixel range (distance range in screen pixels at this glyph's size).
    pub params: [f32; 4],
    /// Clip rectangle x0, y0, x1, y1 in physical pixels; fragments outside are discarded.
    pub clip: [f32; 4],
}

/// The quads of one frame.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DrawList {
    /// Quads in painter's order.
    pub quads: Vec<UiQuad>,
}

impl DrawList {
    /// Removes every quad, keeping the capacity.
    pub fn clear(&mut self) {
        self.quads.clear();
    }

    /// Number of quads.
    #[must_use]
    pub fn len(&self) -> usize {
        self.quads.len()
    }

    /// True when there is nothing to draw.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.quads.is_empty()
    }

    /// The quads as bytes, ready for an instance buffer upload.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        bytemuck::cast_slice(&self.quads)
    }

    /// Appends a rounded rectangle unless it is invisible or fully clipped.
    pub fn push_rect(
        &mut self,
        rect: Rect,
        fill: Color,
        border: f32,
        border_color: Color,
        radius: f32,
        clip: [f32; 4],
    ) {
        let has_border = border > 0.0 && !border_color.is_transparent();
        if fill.is_transparent() && !has_border {
            return;
        }
        if !overlaps(rect, clip) {
            return;
        }
        self.quads.push(UiQuad {
            rect: [rect.x, rect.y, rect.w, rect.h],
            uv: [0.0; 4],
            color: fill.to_array(),
            border_color: border_color.to_array(),
            params: [radius, if has_border { border } else { 0.0 }, KIND_RECT, 0.0],
            clip,
        });
    }
}

fn overlaps(r: Rect, clip: [f32; 4]) -> bool {
    r.w > 0.0 && r.h > 0.0 && r.x < clip[2] && r.y < clip[3] && r.x + r.w > clip[0] && r.y + r.h > clip[1]
}

/// Interaction state that changes colors.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Interaction {
    /// Node under the pointer.
    pub hover: Option<NodeId>,
    /// Node being pressed.
    pub pressed: Option<NodeId>,
    /// Node with keyboard focus.
    pub focus: Option<NodeId>,
}

/// Selection highlight color (linear, premultiplied).
const SELECTION: Color = Color::from_linear_premultiplied(0.05, 0.12, 0.35, 0.5);

struct Emit<'a> {
    atlas: &'a GlyphAtlas,
    out: &'a mut DrawList,
    scale: f32,
    inter: Interaction,
}

fn scale_clip(c: [f32; 4], k: f32) -> [f32; 4] {
    [c[0] * k, c[1] * k, c[2] * k, c[3] * k]
}

/// Fills `out` with the quads of the visible tree.
pub fn build(tree: &Tree, atlas: &GlyphAtlas, out: &mut DrawList, scale: f32, inter: Interaction) {
    out.clear();
    let mut e = Emit {
        atlas,
        out,
        scale,
        inter,
    };
    emit_node(tree, tree.root(), &mut e, true);
}

/// Alpha factor of the built-in disabled style.
pub const DISABLED_FADE: f32 = 0.4;

fn fade(c: Color) -> Color {
    Color::from_linear_premultiplied(
        c.r * DISABLED_FADE,
        c.g * DISABLED_FADE,
        c.b * DISABLED_FADE,
        c.a * DISABLED_FADE,
    )
}

/// The colors a node is drawn with. Disabled wins over focus, hover, and
/// pressed; a disabled color the theme does not set is the normal color at
/// 40% alpha.
fn effective(node: &Node, id: NodeId, inter: Interaction, enabled: bool) -> StateStyle {
    let st = &node.style;
    if !enabled {
        let d = &node.disabled;
        return StateStyle {
            background: Some(d.background.unwrap_or_else(|| fade(st.background))),
            border: Some(d.border.unwrap_or(st.border)),
            border_color: Some(d.border_color.unwrap_or_else(|| fade(st.border_color))),
            color: Some(d.color.unwrap_or_else(|| fade(st.color))),
        };
    }
    let mut s = StateStyle {
        background: Some(st.background),
        border: Some(st.border),
        border_color: Some(st.border_color),
        color: Some(st.color),
    };
    let mut over = |o: &StateStyle| {
        s.background = o.background.or(s.background);
        s.border = o.border.or(s.border);
        s.border_color = o.border_color.or(s.border_color);
        s.color = o.color.or(s.color);
    };
    if inter.focus == Some(id) {
        over(&node.focus);
    }
    if inter.hover == Some(id) {
        over(&node.hover);
    }
    if inter.pressed == Some(id) && inter.hover == Some(id) {
        over(&node.pressed);
    }
    s
}

fn emit_node(tree: &Tree, id: NodeId, e: &mut Emit<'_>, parent_enabled: bool) {
    let Some(node) = tree.get(id) else { return };
    if !node.visible {
        return;
    }
    let enabled = parent_enabled && node.enabled;
    let k = e.scale;
    let s = effective(node, id, e.inter, enabled);
    let parent_clip = node
        .parent
        .and_then(|p| tree.get(p))
        .map_or(node.clip, |p| p.clip);
    let clip = scale_clip(parent_clip, k);
    e.out.push_rect(
        node.rect.scaled(k),
        s.background.unwrap_or(Color::TRANSPARENT),
        s.border.unwrap_or(0.0) * k,
        s.border_color.unwrap_or(Color::TRANSPARENT),
        node.style.radius * k,
        clip,
    );
    if matches!(node.kind, NodeKind::Text | NodeKind::Button | NodeKind::Input) {
        emit_text(node, id, s.color.unwrap_or(Color::WHITE), e);
    }
    for c in &node.children {
        emit_node(tree, *c, e, enabled);
    }
}

#[allow(clippy::too_many_lines, clippy::many_single_char_names)] // one straight pass of pixel math
fn emit_text(node: &Node, id: NodeId, color: Color, e: &mut Emit<'_>) {
    let Some(tb) = node.text.as_ref() else { return };
    let layout = tb.layout();
    let k = e.scale;
    let inner = node.rect.inset(node.style.padding[0], node.style.padding[1]);
    let mut clip = node.clip;
    let mut origin = [inner.x, inner.y];
    let mut align = node.style.text_align;
    let mut color = color;
    match node.kind {
        NodeKind::Button => origin[1] += ((inner.h - layout.size[1]) * 0.5).max(0.0),
        NodeKind::Input => {
            clip = clip_intersect(clip, inner.to_clip());
            origin[0] -= node.input.as_ref().map_or(0.0, |i| i.scroll_x);
            origin[1] += ((inner.h - layout.size[1]) * 0.5).max(0.0);
            align = TextAlign::Start;
            if node.showing_placeholder {
                color = Color::from_linear_premultiplied(
                    color.r * 0.45,
                    color.g * 0.45,
                    color.b * 0.45,
                    color.a * 0.45,
                );
            }
        }
        _ => {}
    }
    let pclip = scale_clip(clip, k);
    let focused = e.inter.focus == Some(id);
    let input = node.input.as_ref().filter(|_| !node.showing_placeholder);
    let first_line = layout.lines.first();

    // Selection highlight behind the text.
    if let (Some(input), Some(line), true) = (input, first_line, focused) {
        let sel = input.selection();
        if !sel.is_empty() && input.preedit.is_empty() {
            let (_, x0) = layout.caret_x(sel.start);
            let (_, x1) = layout.caret_x(sel.end);
            let r = Rect::new(
                origin[0] + x0.min(x1),
                origin[1] + line.top,
                (x1 - x0).abs(),
                line.height,
            );
            e.out
                .push_rect(r.scaled(k), SELECTION, 0.0, Color::TRANSPARENT, 0.0, pclip);
        }
    }

    // Glyphs.
    let [aw, ah] = e.atlas.size();
    #[allow(clippy::cast_precision_loss)] // atlas sizes are at most 16384
    let (aw, ah) = (aw.max(1) as f32, ah.max(1) as f32);
    let gk = layout.font_size * k / e.atlas.em_px();
    let screen_range = e.atlas.msdf_range() * gk;
    for line in &layout.lines {
        let off = layout.line_offset(line, align, inner.w);
        let Some(glyphs) = layout.glyphs.get(line.glyphs.clone()) else {
            continue;
        };
        for g in glyphs {
            let Some(ag) = e.atlas.get(g.font, g.glyph) else {
                continue;
            };
            #[allow(clippy::cast_precision_loss)] // texel coordinates are at most 16384
            let (rx, ry, rw, rh) = (
                ag.rect.x as f32,
                ag.rect.y as f32,
                ag.rect.w as f32,
                ag.rect.h as f32,
            );
            let x = (origin[0] + off + g.x) * k + ag.left * gk;
            let y = (origin[1] + g.y) * k - ag.top * gk;
            let r = Rect::new(x, y, rw * gk, rh * gk);
            if !overlaps(r, pclip) {
                continue;
            }
            e.out.quads.push(UiQuad {
                rect: [r.x, r.y, r.w, r.h],
                uv: [rx / aw, ry / ah, (rx + rw) / aw, (ry + rh) / ah],
                color: color.to_array(),
                border_color: [0.0; 4],
                params: [0.0, 0.0, KIND_GLYPH, screen_range],
                clip: pclip,
            });
        }
    }

    let Some(input) = input.or(node.input.as_ref().filter(|_| focused)) else {
        return;
    };
    let Some(line) = first_line else { return };
    let thickness = 1.0_f32.max(1.0 / k.max(1e-3));
    // IME preedit underline.
    if !input.preedit.is_empty() && !node.showing_placeholder {
        let (_, x0) = layout.caret_x(input.caret);
        let (_, x1) = layout.caret_x(input.caret + input.preedit.len());
        let y = origin[1] + line.baseline + (layout.font_size * 0.08).max(1.0);
        let r = Rect::new(origin[0] + x0.min(x1), y, (x1 - x0).abs(), thickness);
        e.out
            .push_rect(r.scaled(k), color, 0.0, Color::TRANSPARENT, 0.0, pclip);
    }
    // Caret.
    if focused {
        let caret = if node.showing_placeholder {
            0.0
        } else {
            layout.caret_x(display_caret(input)).1
        };
        let r = Rect::new(origin[0] + caret, origin[1] + line.top, thickness, line.height);
        let caret_color = node.style.color;
        e.out
            .push_rect(r.scaled(k), caret_color, 0.0, Color::TRANSPARENT, 0.0, pclip);
    }
}

/// The CPU reference rasterizer: the exact math the renderer's shader must
/// implement (see [`UiQuad`]).
pub mod reference {
    use super::{DrawList, KIND_GLYPH, UiQuad};
    use crate::msdf::median;

    /// An RGBA image, linear premultiplied `f32`, row-major.
    #[derive(Clone, Debug, PartialEq)]
    pub struct Image {
        /// Width in pixels.
        pub width: u32,
        /// Height in pixels.
        pub height: u32,
        /// Pixels, `width * height`, top row first.
        pub pixels: Vec<[f32; 4]>,
    }

    impl Image {
        /// A pixel (transparent outside the image).
        #[must_use]
        pub fn get(&self, x: u32, y: u32) -> [f32; 4] {
            if x >= self.width || y >= self.height {
                return [0.0; 4];
            }
            self.pixels
                .get(y as usize * self.width as usize + x as usize)
                .copied()
                .unwrap_or([0.0; 4])
        }
    }

    /// Bilinear sample of an RGBA8 texture with clamp-to-edge addressing;
    /// texel `i` is centered at `(i + 0.5) / size`. Channels in `0..=1`.
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )] // texture coordinates are small and clamped to be non-negative
    pub fn sample_bilinear(pixels: &[u8], size: [u32; 2], uv: [f32; 2]) -> [f32; 4] {
        let (w, h) = (size[0].max(1), size[1].max(1));
        let sx = uv[0] * w as f32 - 0.5;
        let sy = uv[1] * h as f32 - 0.5;
        let (fx, fy) = (sx.floor(), sy.floor());
        let (ax, ay) = (sx - fx, sy - fy);
        let texel = |x: f32, y: f32| -> [f32; 4] {
            let xi = (x.max(0.0) as u32).min(w - 1) as usize;
            let yi = (y.max(0.0) as u32).min(h - 1) as usize;
            let i = (yi * w as usize + xi) * 4;
            let mut out = [0.0; 4];
            for (k, o) in out.iter_mut().enumerate() {
                *o = f32::from(pixels.get(i + k).copied().unwrap_or(0)) / 255.0;
            }
            out
        };
        let t00 = texel(fx, fy);
        let t10 = texel(fx + 1.0, fy);
        let t01 = texel(fx, fy + 1.0);
        let t11 = texel(fx + 1.0, fy + 1.0);
        let mut out = [0.0; 4];
        for ((((o, a), b), c), d) in out.iter_mut().zip(t00).zip(t10).zip(t01).zip(t11) {
            let top = a + (b - a) * ax;
            let bottom = c + (d - c) * ax;
            *o = top + (bottom - top) * ay;
        }
        out
    }

    /// The fragment function: the premultiplied color of quad `q` at pixel
    /// center `p`, or `None` when the fragment is clipped.
    #[must_use]
    #[allow(clippy::many_single_char_names)] // the shader's variable names
    pub fn shade(q: &UiQuad, p: [f32; 2], atlas: &[u8], atlas_size: [u32; 2]) -> Option<[f32; 4]> {
        let [cx0, cy0, cx1, cy1] = q.clip;
        if p[0] < cx0 || p[1] < cy0 || p[0] >= cx1 || p[1] >= cy1 {
            return None;
        }
        let [x, y, w, h] = q.rect;
        if q.params[2] == KIND_GLYPH {
            let t = [(p[0] - x) / w, (p[1] - y) / h];
            let uv = [
                q.uv[0] + (q.uv[2] - q.uv[0]) * t[0],
                q.uv[1] + (q.uv[3] - q.uv[1]) * t[1],
            ];
            let s = sample_bilinear(atlas, atlas_size, uv);
            let md = median(s[0], s[1], s[2]);
            let alpha = ((md - 0.5) * q.params[3] + 0.5).clamp(0.0, 1.0);
            return Some(q.color.map(|c| c * alpha));
        }
        let half = [w * 0.5, h * 0.5];
        let center = [x + half[0], y + half[1]];
        let r = q.params[0].min(half[0].min(half[1])).max(0.0);
        let qx = (p[0] - center[0]).abs() - (half[0] - r);
        let qy = (p[1] - center[1]).abs() - (half[1] - r);
        let d = qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - r;
        let cover = (0.5 - d).clamp(0.0, 1.0);
        let bw = q.params[1];
        let b = if bw > 0.0 {
            (d + bw + 0.5).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let mut out = [0.0; 4];
        for (k, o) in out.iter_mut().enumerate() {
            let fill = q.color.get(k).copied().unwrap_or(0.0);
            let border = q.border_color.get(k).copied().unwrap_or(0.0);
            *o = (fill + (border - fill) * b) * cover;
        }
        Some(out)
    }

    /// Rasterizes a draw list over a transparent `width` x `height` image.
    /// Each quad covers the pixels whose centers lie inside its `rect`.
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::many_single_char_names
    )] // pixel math
    pub fn rasterize(list: &DrawList, atlas: &[u8], atlas_size: [u32; 2], width: u32, height: u32) -> Image {
        let mut img = Image {
            width,
            height,
            pixels: vec![[0.0; 4]; width as usize * height as usize],
        };
        for q in &list.quads {
            let [x, y, w, h] = q.rect;
            let x0 = (x - 0.5).ceil().max(0.0) as u32;
            let y0 = (y - 0.5).ceil().max(0.0) as u32;
            let x1 = ((x + w - 0.5).ceil().max(0.0) as u32).min(width);
            let y1 = ((y + h - 0.5).ceil().max(0.0) as u32).min(height);
            for py in y0..y1 {
                for px in x0..x1 {
                    let p = [px as f32 + 0.5, py as f32 + 0.5];
                    let Some(src) = shade(q, p, atlas, atlas_size) else {
                        continue;
                    };
                    let i = py as usize * width as usize + px as usize;
                    if let Some(dst) = img.pixels.get_mut(i) {
                        let keep = 1.0 - src[3];
                        for (d, s) in dst.iter_mut().zip(src) {
                            *d = s + *d * keep;
                        }
                    }
                }
            }
        }
        img
    }
}

#[cfg(test)]
mod tests {
    use super::reference::{rasterize, shade};
    use super::*;

    #[test]
    fn quad_is_pod_and_96_bytes() {
        assert_eq!(std::mem::size_of::<UiQuad>(), 96);
        let list = DrawList {
            quads: vec![UiQuad::zeroed_quad()],
        };
        assert_eq!(list.as_bytes().len(), 96);
    }

    impl UiQuad {
        fn zeroed_quad() -> Self {
            bytemuck::Zeroable::zeroed()
        }
    }

    #[test]
    fn rounded_rect_corner_is_transparent_and_border_shows() {
        let mut list = DrawList::default();
        let clip = [0.0, 0.0, 100.0, 100.0];
        list.push_rect(
            Rect::new(10.0, 10.0, 40.0, 20.0),
            Color::WHITE,
            2.0,
            Color::BLACK,
            8.0,
            clip,
        );
        let q = list.quads.first().copied().unwrap_or(UiQuad::zeroed_quad());
        // Center: fill.
        assert_eq!(shade(&q, [30.5, 20.5], &[], [1, 1]), Some([1.0, 1.0, 1.0, 1.0]));
        // Inside the border band: border color.
        let edge = shade(&q, [30.5, 10.5], &[], [1, 1]).unwrap_or([9.0; 4]);
        assert!(edge[0] < 0.01 && (edge[3] - 1.0).abs() < 1e-6, "{edge:?}");
        // The extreme corner is outside the radius.
        let corner = shade(&q, [10.5, 10.5], &[], [1, 1]).unwrap_or([9.0; 4]);
        assert!(corner[3] < 0.01, "{corner:?}");
        // Clipped fragments are discarded.
        let mut clipped = q;
        clipped.clip = [0.0, 0.0, 20.0, 100.0];
        assert_eq!(shade(&clipped, [30.5, 20.5], &[], [1, 1]), None);
        let img = rasterize(&list, &[], [1, 1], 64, 64);
        assert_eq!(img.get(30, 20), [1.0, 1.0, 1.0, 1.0]);
        assert_eq!(img.get(5, 5), [0.0; 4]);
    }
}
