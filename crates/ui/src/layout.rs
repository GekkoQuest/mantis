//! Flex-lite layout.
//!
//! Containers lay children along a main axis (`direction`) with `padding`
//! and `gap`. Each child's size on an axis is fixed pixels, `fit` (its
//! content), or `grow:N` (a weighted share of the free main-axis space; on the
//! cross axis `grow` fills like `stretch`), clamped by `min_*` and `max_*`.
//! `justify` distributes leftover main-axis space (`start`, `center`, `end`,
//! `space-between`); `align` places children on the cross axis (`start`,
//! `center`, `end`, `stretch`). Text is measured with wrapping at the
//! available width. Lists stack their items in a column, clip them, and
//! scroll.
//!
//! Layout is in logical pixels. It runs only when something marked it dirty
//! (a bound value, visibility, a list's length, the viewport, a reload, or
//! editing); the scale factor to physical pixels is applied when the draw
//! list is built.

use crate::font::FontLibrary;
use crate::markup::{Align, Flow, Justify, SizeSpec};
use crate::text::Shaper;
use crate::tree::{NodeId, NodeKind, Tree};

/// An axis-aligned rectangle.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Rect {
    /// Left.
    pub x: f32,
    /// Top.
    pub y: f32,
    /// Width.
    pub w: f32,
    /// Height.
    pub h: f32,
}

impl Rect {
    /// Builds a rectangle.
    #[must_use]
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    /// True when the point lies inside (right and bottom edges excluded).
    #[must_use]
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.w && y < self.y + self.h
    }

    /// Shrinks by `dx` on the left and right and `dy` on the top and bottom.
    #[must_use]
    pub fn inset(&self, dx: f32, dy: f32) -> Self {
        Self {
            x: self.x + dx,
            y: self.y + dy,
            w: (self.w - 2.0 * dx).max(0.0),
            h: (self.h - 2.0 * dy).max(0.0),
        }
    }

    /// As a clip rectangle `[x0, y0, x1, y1]`.
    #[must_use]
    pub fn to_clip(&self) -> [f32; 4] {
        [self.x, self.y, self.x + self.w, self.y + self.h]
    }

    /// Scaled by `k`.
    #[must_use]
    pub fn scaled(&self, k: f32) -> Self {
        Self {
            x: self.x * k,
            y: self.y * k,
            w: self.w * k,
            h: self.h * k,
        }
    }
}

/// Intersection of two clip rectangles `[x0, y0, x1, y1]`.
#[must_use]
pub fn clip_intersect(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    let x0 = a[0].max(b[0]);
    let y0 = a[1].max(b[1]);
    [x0, y0, a[2].min(b[2]).max(x0), a[3].min(b[3]).max(y0)]
}

/// Shared layout state.
pub struct Layouter<'a> {
    /// Fonts.
    pub fonts: &'a FontLibrary,
    /// Shaper (buffers reused).
    pub shaper: &'a mut Shaper,
}

fn clamp(v: f32, min: f32, max: f32) -> f32 {
    v.min(max).max(min)
}

fn axis(flow: Flow) -> usize {
    match flow {
        Flow::Row => 0,
        Flow::Column => 1,
    }
}

fn spec(style: &crate::tree::ResolvedStyle, a: usize) -> SizeSpec {
    if a == 0 { style.width } else { style.height }
}

fn get(v: [f32; 2], a: usize) -> f32 {
    if a == 0 { v[0] } else { v[1] }
}

fn set(v: &mut [f32; 2], a: usize, x: f32) {
    if a == 0 {
        v[0] = x;
    } else {
        v[1] = x;
    }
}

impl Layouter<'_> {
    /// Lays out the whole tree in a viewport of `viewport` logical px.
    pub fn layout(&mut self, tree: &mut Tree, viewport: [f32; 2]) {
        let root = tree.root();
        let Some(style) = tree.get(root).map(|n| n.style) else {
            return;
        };
        let want = self.measure(tree, root, viewport[0]);
        let mut size = [0.0; 2];
        for a in 0..2 {
            let v = match spec(&style, a) {
                SizeSpec::Px(v) => v,
                SizeSpec::Grow(_) => get(viewport, a),
                SizeSpec::Fit => get(want, a),
            };
            set(&mut size, a, clamp(v, get(style.min, a), get(style.max, a)));
        }
        let rect = Rect::new(0.0, 0.0, size[0], size[1]);
        self.arrange(tree, root, rect, [0.0, 0.0, viewport[0], viewport[1]]);
    }

    /// The outer size `id` wants when at most `max_w` wide.
    pub fn measure(&mut self, tree: &mut Tree, id: NodeId, max_w: f32) -> [f32; 2] {
        let Some(node) = tree.get(id) else {
            return [0.0, 0.0];
        };
        if !node.visible {
            return [0.0, 0.0];
        }
        let st = node.style;
        let kind = node.kind;
        let [px, py] = st.padding;
        let inner_max = match st.width {
            SizeSpec::Px(w) => (w - 2.0 * px).max(0.0),
            _ => (max_w.min(st.max[0]) - 2.0 * px).max(0.0),
        };
        let content = match kind {
            NodeKind::Text | NodeKind::Button => self.text_size(tree, id, Some(inner_max)),
            NodeKind::Input => {
                let s = self.text_size(tree, id, None);
                [s[0], s[1]]
            }
            NodeKind::Spacer => [0.0, 0.0],
            NodeKind::Panel | NodeKind::List => self.measure_children(tree, id, inner_max),
        };
        let mut out = [0.0; 2];
        for a in 0..2 {
            let pad = if a == 0 { px } else { py };
            let v = match spec(&st, a) {
                SizeSpec::Px(v) => v,
                SizeSpec::Fit | SizeSpec::Grow(_) => get(content, a) + 2.0 * pad,
            };
            set(&mut out, a, clamp(v, get(st.min, a), get(st.max, a)));
        }
        out
    }

    fn text_size(&mut self, tree: &mut Tree, id: NodeId, max_w: Option<f32>) -> [f32; 2] {
        let Some(node) = tree.get_mut(id) else {
            return [0.0, 0.0];
        };
        let style = node.text_style();
        node.text.as_mut().map_or([0.0, 0.0], |tb| {
            tb.ensure(self.fonts, self.shaper, style, max_w).size
        })
    }

    fn child(tree: &Tree, id: NodeId, i: usize) -> Option<NodeId> {
        tree.get(id).and_then(|n| n.children.get(i).copied())
    }

    fn child_count(tree: &Tree, id: NodeId) -> usize {
        tree.get(id).map_or(0, |n| n.children.len())
    }

    fn measure_children(&mut self, tree: &mut Tree, id: NodeId, inner_max: f32) -> [f32; 2] {
        let Some(st) = tree.get(id).map(|n| n.style) else {
            return [0.0, 0.0];
        };
        let main = axis(st.direction);
        let mut out = [0.0_f32; 2];
        let mut visible = 0.0_f32;
        for i in 0..Self::child_count(tree, id) {
            let Some(c) = Self::child(tree, id, i) else {
                continue;
            };
            if !tree.get(c).is_some_and(|n| n.visible) {
                continue;
            }
            visible += 1.0;
            let s = self.measure(tree, c, inner_max);
            let cross = 1 - main;
            let (m, c) = (get(out, main) + get(s, main), get(out, cross).max(get(s, cross)));
            set(&mut out, main, m);
            set(&mut out, cross, c);
        }
        let gaps = st.gap * (visible - 1.0).max(0.0);
        let m = get(out, main) + gaps;
        set(&mut out, main, m);
        out
    }

    /// Places `id` at `rect` and lays out its content.
    pub fn arrange(&mut self, tree: &mut Tree, id: NodeId, rect: Rect, parent_clip: [f32; 4]) {
        let Some(node) = tree.get_mut(id) else {
            return;
        };
        node.rect = rect;
        node.clip = if node.style.clip {
            clip_intersect(parent_clip, rect.to_clip())
        } else {
            parent_clip
        };
        let clip = node.clip;
        if !node.visible {
            return;
        }
        let st = node.style;
        let inner = rect.inset(st.padding[0], st.padding[1]);
        match node.kind {
            NodeKind::Text | NodeKind::Button => {
                self.text_size(tree, id, Some(inner.w));
            }
            NodeKind::Input => {
                self.text_size(tree, id, None);
                scroll_input_to_caret(tree, id, inner.w);
            }
            NodeKind::Spacer => {}
            NodeKind::Panel | NodeKind::List => self.arrange_children(tree, id, inner, clip),
        }
    }

    /// Main and cross sizes of every visible child, stored in the child's
    /// rectangle before positioning. Returns the summed main size, the grow
    /// weight, and the visible count.
    fn size_children(&mut self, tree: &mut Tree, id: NodeId, inner: Rect, main: usize) -> (f32, f32, f32) {
        let inner_size = [inner.w, inner.h];
        let align = tree.get(id).map_or(Align::Start, |n| n.style.align);
        let (mut total, mut weight, mut visible) = (0.0_f32, 0.0_f32, 0.0_f32);
        for i in 0..Self::child_count(tree, id) {
            let Some(c) = Self::child(tree, id, i) else {
                continue;
            };
            let Some(cs) = tree.get(c).filter(|n| n.visible).map(|n| n.style) else {
                continue;
            };
            visible += 1.0;
            let mut size = [0.0_f32; 2];
            // Width first: wrapping text needs it to know its height.
            for a in [0, 1] {
                let v = if a == main {
                    match spec(&cs, a) {
                        SizeSpec::Px(v) => v,
                        SizeSpec::Fit => {
                            let w = if a == 0 { inner.w } else { size[0] };
                            get(self.measure(tree, c, w), a)
                        }
                        SizeSpec::Grow(k) => {
                            weight += k;
                            0.0
                        }
                    }
                } else {
                    match spec(&cs, a) {
                        SizeSpec::Px(v) => v,
                        SizeSpec::Grow(_) => get(inner_size, a),
                        SizeSpec::Fit if align == Align::Stretch => get(inner_size, a),
                        SizeSpec::Fit => {
                            let w = if a == 0 { inner.w } else { size[0] };
                            let m = get(self.measure(tree, c, w), a);
                            if a == 0 { m.min(inner.w.max(cs.min[0])) } else { m }
                        }
                    }
                };
                set(&mut size, a, clamp(v, get(cs.min, a), get(cs.max, a)));
            }
            total += get(size, main);
            if let Some(n) = tree.get_mut(c) {
                n.rect.w = size[0];
                n.rect.h = size[1];
            }
        }
        (total, weight, visible)
    }

    fn arrange_children(&mut self, tree: &mut Tree, id: NodeId, inner: Rect, clip: [f32; 4]) {
        let Some(node) = tree.get(id) else { return };
        let st = node.style;
        let is_list = node.kind == NodeKind::List;
        let main = axis(st.direction);
        let cross = 1 - main;
        let inner_main = if main == 0 { inner.w } else { inner.h };
        let inner_cross = if main == 0 { inner.h } else { inner.w };
        let (mut total, weight, visible) = self.size_children(tree, id, inner, main);
        let gaps = st.gap * (visible - 1.0).max(0.0);
        let free = inner_main - total - gaps;
        if free > 0.0 && weight > 0.0 {
            total = 0.0;
            for i in 0..Self::child_count(tree, id) {
                let Some(c) = Self::child(tree, id, i) else {
                    continue;
                };
                let Some(n) = tree.get_mut(c).filter(|n| n.visible) else {
                    continue;
                };
                let mut size = [n.rect.w, n.rect.h];
                if let SizeSpec::Grow(k) = spec(&n.style, main) {
                    let v = get(size, main) + free * k / weight;
                    set(
                        &mut size,
                        main,
                        clamp(v, get(n.style.min, main), get(n.style.max, main)),
                    );
                }
                n.rect.w = size[0];
                n.rect.h = size[1];
                total += get(size, main);
            }
            // Re-measure the height of grown-width children that fit their height.
            if main == 0 {
                for i in 0..Self::child_count(tree, id) {
                    let Some(c) = Self::child(tree, id, i) else {
                        continue;
                    };
                    let Some((w, cs)) = tree.get(c).filter(|n| n.visible).map(|n| (n.rect.w, n.style)) else {
                        continue;
                    };
                    if matches!(cs.width, SizeSpec::Grow(_))
                        && cs.height == SizeSpec::Fit
                        && st.align != Align::Stretch
                    {
                        let h = clamp(self.measure(tree, c, w)[1], cs.min[1], cs.max[1]);
                        if let Some(n) = tree.get_mut(c) {
                            n.rect.h = h;
                        }
                    }
                }
            }
        }
        let leftover = inner_main - total - gaps;
        let (mut offset, mut extra) = (0.0_f32, 0.0_f32);
        if leftover > 0.0 {
            match st.justify {
                Justify::Center => offset = leftover * 0.5,
                Justify::End => offset = leftover,
                Justify::SpaceBetween if visible > 1.0 => extra = leftover / (visible - 1.0),
                Justify::Start | Justify::SpaceBetween => {}
            }
        }
        if is_list && let Some(l) = tree.get_mut(id).and_then(|n| n.list.as_mut()) {
            let content = total + gaps;
            l.content = content;
            l.scroll = l.scroll.clamp(0.0, (content - inner_main).max(0.0));
            offset -= l.scroll;
        }
        let origin = [inner.x, inner.y];
        let mut cursor = get(origin, main) + offset;
        for i in 0..Self::child_count(tree, id) {
            let Some(c) = Self::child(tree, id, i) else {
                continue;
            };
            let Some(n) = tree.get(c) else { continue };
            if !n.visible {
                let r = Rect::new(inner.x, inner.y, 0.0, 0.0);
                self.arrange(tree, c, r, clip);
                continue;
            }
            let size = [n.rect.w, n.rect.h];
            let c_cross = get(size, cross);
            let cross_off = match st.align {
                Align::Start | Align::Stretch => 0.0,
                Align::Center => (inner_cross - c_cross) * 0.5,
                Align::End => inner_cross - c_cross,
            };
            let mut pos = [0.0_f32; 2];
            set(&mut pos, main, cursor);
            set(&mut pos, cross, get(origin, cross) + cross_off);
            let r = Rect::new(pos[0], pos[1], size[0], size[1]);
            self.arrange(tree, c, r, clip);
            cursor += get(size, main) + st.gap + extra;
        }
    }
}

/// Keeps an input's caret inside its visible width.
fn scroll_input_to_caret(tree: &mut Tree, id: NodeId, width: f32) {
    let Some(node) = tree.get_mut(id) else { return };
    let Some(input) = node.input.as_mut() else { return };
    let Some(tb) = node.text.as_ref() else { return };
    if node.showing_placeholder {
        input.scroll_x = 0.0;
        return;
    }
    let caret = display_caret(input);
    let (_, x) = tb.layout().caret_x(caret);
    if x - input.scroll_x > width {
        input.scroll_x = x - width;
    }
    if x < input.scroll_x {
        input.scroll_x = x;
    }
    let max_scroll = (tb.layout().size[0] - width).max(0.0);
    input.scroll_x = input.scroll_x.clamp(0.0, max_scroll.max(x - width).max(0.0));
}

/// Caret byte index in an input's display text (committed text with the
/// preedit spliced in at the caret).
#[must_use]
pub fn display_caret(input: &crate::tree::InputState) -> usize {
    if input.preedit.is_empty() {
        input.caret
    } else {
        let inside = input
            .preedit_cursor
            .map_or(input.preedit.len(), |c| c.1.min(input.preedit.len()));
        input.caret + inside
    }
}
