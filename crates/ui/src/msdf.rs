//! Multi-channel signed distance fields (MSDF) from glyph outlines.
//!
//! The generator follows Chlumsky's method:
//!
//! 1. **Outline.** A `ttf_parser::OutlineBuilder` collects contours of line
//!    and quadratic segments; cubic segments are split into four quadratics.
//! 2. **Edge coloring.** Corners are found where the outgoing and incoming
//!    tangents of consecutive edges turn by more than an angle threshold
//!    (`dot <= 0` or `|cross| > sin(threshold)`). Edges between corners get
//!    colors from {cyan, magenta, yellow} so the two edges meeting at a corner
//!    share exactly one channel; a smooth contour is white; a contour with a
//!    single corner is split into three colored parts ("teardrop").
//! 3. **Distance.** For every texel center and every channel, the edge of
//!    that channel with the smallest true distance (ties broken by the
//!    angle between the edge direction and the direction to the point) is
//!    selected, and the channel stores that edge's signed *pseudo*-distance
//!    (the distance to the edge extended along its end tangents). The
//!    alpha channel stores the true signed distance to the whole outline.
//! 4. **Sign.** Edge signs follow contour orientation (outer clockwise in the
//!    font's y-up space, holes counter-clockwise: positive inside). The true
//!    inside test uses the non-zero winding rule; if the outline's orientation
//!    is globally reversed the channel signs are flipped to agree with it.
//!    Texels whose channel median disagrees with the winding sign are
//!    replaced by the true distance (error correction), so inside/outside is
//!    exact at every texel center.
//!
//! Distances are in texels, positive inside, and stored in RGBA8 as
//! `round(clamp(d / range + 0.5, 0, 1) * 255)`. A shader reconstructs coverage with
//! `median(r, g, b)`; see [`crate::draw::UiQuad`] for the formula.

use rustybuzz::ttf_parser;

/// Red channel bit of an edge color.
const RED: u8 = 1;
/// Green channel bit of an edge color.
const GREEN: u8 = 2;
/// Blue channel bit of an edge color.
const BLUE: u8 = 4;
const WHITE: u8 = 7;
const CYAN: u8 = GREEN | BLUE;

/// Default corner angle threshold, radians (as in msdfgen).
pub const DEFAULT_ANGLE_THRESHOLD: f64 = 3.0;

#[derive(Clone, Copy, Debug, PartialEq, Default)]
struct V2 {
    x: f64,
    y: f64,
}

impl V2 {
    const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
    fn add(self, o: Self) -> Self {
        Self::new(self.x + o.x, self.y + o.y)
    }
    fn sub(self, o: Self) -> Self {
        Self::new(self.x - o.x, self.y - o.y)
    }
    fn mul(self, k: f64) -> Self {
        Self::new(self.x * k, self.y * k)
    }
    fn dot(self, o: Self) -> f64 {
        self.x * o.x + self.y * o.y
    }
    fn cross(self, o: Self) -> f64 {
        self.x * o.y - self.y * o.x
    }
    fn len(self) -> f64 {
        self.dot(self).sqrt()
    }
    fn normalize(self) -> Self {
        let l = self.len();
        if l == 0.0 {
            Self::new(0.0, 1.0)
        } else {
            self.mul(1.0 / l)
        }
    }
    fn lerp(self, o: Self, t: f64) -> Self {
        self.add(o.sub(self).mul(t))
    }
}

fn non_zero_sign(v: f64) -> f64 {
    if v > 0.0 { 1.0 } else { -1.0 }
}

/// A signed distance with the orthogonality tie-breaker.
#[derive(Clone, Copy, Debug)]
struct SignedDistance {
    distance: f64,
    dot: f64,
}

impl SignedDistance {
    const FAR: Self = Self {
        distance: -1e240,
        dot: 1.0,
    };
    fn less(self, o: Self) -> bool {
        let (a, b) = (self.distance.abs(), o.distance.abs());
        a < b || (a == b && self.dot < o.dot)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Segment {
    Line([V2; 2]),
    Quad([V2; 3]),
}

#[expect(clippy::many_single_char_names)] // geometry uses the conventional point names
impl Segment {
    fn start(self) -> V2 {
        match self {
            Self::Line([a, _]) | Self::Quad([a, _, _]) => a,
        }
    }

    fn end(self) -> V2 {
        match self {
            Self::Line([_, b]) | Self::Quad([_, _, b]) => b,
        }
    }

    fn point(self, t: f64) -> V2 {
        match self {
            Self::Line([a, b]) => a.lerp(b, t),
            Self::Quad([a, b, c]) => a.lerp(b, t).lerp(b.lerp(c, t), t),
        }
    }

    fn direction(self, t: f64) -> V2 {
        match self {
            Self::Line([a, b]) => b.sub(a),
            Self::Quad([a, b, c]) => {
                let d = b.sub(a).lerp(c.sub(b), t);
                if d.x == 0.0 && d.y == 0.0 { c.sub(a) } else { d }
            }
        }
    }

    fn split_in_thirds(self) -> [Self; 3] {
        match self {
            Self::Line([a, b]) => {
                let p1 = a.lerp(b, 1.0 / 3.0);
                let p2 = a.lerp(b, 2.0 / 3.0);
                [Self::Line([a, p1]), Self::Line([p1, p2]), Self::Line([p2, b])]
            }
            Self::Quad([a, b, c]) => {
                let p1 = self.point(1.0 / 3.0);
                let p2 = self.point(2.0 / 3.0);
                [
                    Self::Quad([a, a.lerp(b, 1.0 / 3.0), p1]),
                    Self::Quad([p1, a.lerp(b, 5.0 / 9.0).lerp(b.lerp(c, 4.0 / 9.0), 0.5), p2]),
                    Self::Quad([p2, b.lerp(c, 2.0 / 3.0), c]),
                ]
            }
        }
    }

    fn transform(self, k: f64, t: V2) -> Self {
        let f = |p: V2| p.mul(k).add(t);
        match self {
            Self::Line([a, b]) => Self::Line([f(a), f(b)]),
            Self::Quad([a, b, c]) => Self::Quad([f(a), f(b), f(c)]),
        }
    }

    /// Signed distance from `origin` and the parameter of the closest point.
    fn signed_distance(self, origin: V2) -> (SignedDistance, f64) {
        match self {
            Self::Line([p0, p1]) => {
                let aq = origin.sub(p0);
                let ab = p1.sub(p0);
                let param = aq.dot(ab) / ab.dot(ab);
                let eq = if param > 0.5 { p1 } else { p0 }.sub(origin);
                let endpoint_distance = eq.len();
                if param > 0.0 && param < 1.0 {
                    let ortho = V2::new(ab.y, -ab.x).normalize().dot(aq);
                    if ortho.abs() < endpoint_distance {
                        return (
                            SignedDistance {
                                distance: ortho,
                                dot: 0.0,
                            },
                            param,
                        );
                    }
                }
                (
                    SignedDistance {
                        distance: non_zero_sign(aq.cross(ab)) * endpoint_distance,
                        dot: ab.normalize().dot(eq.normalize()).abs(),
                    },
                    param,
                )
            }
            Self::Quad([p0, p1, p2]) => quad_signed_distance(p0, p1, p2, origin),
        }
    }

    /// Converts a distance to this edge into the pseudo-distance to the edge
    /// extended along its end tangents.
    fn to_pseudo(self, mut d: SignedDistance, origin: V2, param: f64) -> SignedDistance {
        if param < 0.0 {
            let dir = self.direction(0.0).normalize();
            let aq = origin.sub(self.start());
            if aq.dot(dir) < 0.0 {
                let pseudo = aq.cross(dir);
                if pseudo.abs() <= d.distance.abs() {
                    d = SignedDistance {
                        distance: pseudo,
                        dot: 0.0,
                    };
                }
            }
        } else if param > 1.0 {
            let dir = self.direction(1.0).normalize();
            let bq = origin.sub(self.end());
            if bq.dot(dir) > 0.0 {
                let pseudo = bq.cross(dir);
                if pseudo.abs() <= d.distance.abs() {
                    d = SignedDistance {
                        distance: pseudo,
                        dot: 0.0,
                    };
                }
            }
        }
        d
    }

    /// Appends this segment flattened into lines (for winding tests).
    fn flatten(self, out: &mut Vec<[V2; 2]>) {
        match self {
            Self::Line([a, b]) => out.push([a, b]),
            Self::Quad(_) => {
                let n = 16;
                let mut prev = self.start();
                for i in 1..=n {
                    let p = self.point(f64::from(i) / f64::from(n));
                    out.push([prev, p]);
                    prev = p;
                }
            }
        }
    }
}

#[expect(clippy::many_single_char_names)] // geometry uses the conventional point names
fn quad_signed_distance(p0: V2, p1: V2, p2: V2, origin: V2) -> (SignedDistance, f64) {
    let qa = p0.sub(origin);
    let ab = p1.sub(p0);
    let br = p2.sub(p1).sub(ab);
    let a = br.dot(br);
    let b = 3.0 * ab.dot(br);
    let c = 2.0 * ab.dot(ab) + qa.dot(br);
    let d = qa.dot(ab);
    let seg = Segment::Quad([p0, p1, p2]);
    let mut roots = [0.0; 3];
    let n = solve_cubic(&mut roots, a, b, c, d);

    let mut ep_dir = seg.direction(0.0);
    let mut min_distance = non_zero_sign(ep_dir.cross(qa)) * qa.len();
    let mut param = -qa.dot(ep_dir) / ep_dir.dot(ep_dir);
    {
        ep_dir = seg.direction(1.0);
        let to_end = p2.sub(origin);
        let distance = to_end.len();
        if distance < min_distance.abs() {
            min_distance = non_zero_sign(ep_dir.cross(to_end)) * distance;
            param = origin.sub(p1).dot(ep_dir) / ep_dir.dot(ep_dir);
        }
    }
    for &t in roots.iter().take(n) {
        if t > 0.0 && t < 1.0 {
            let qe = qa.add(ab.mul(2.0 * t)).add(br.mul(t * t));
            let distance = qe.len();
            if distance <= min_distance.abs() {
                min_distance = non_zero_sign(ab.add(br.mul(t)).cross(qe)) * distance;
                param = t;
            }
        }
    }
    let dot = if (0.0..=1.0).contains(&param) {
        0.0
    } else if param < 0.5 {
        seg.direction(0.0).normalize().dot(qa.normalize()).abs()
    } else {
        seg.direction(1.0)
            .normalize()
            .dot(p2.sub(origin).normalize())
            .abs()
    };
    (
        SignedDistance {
            distance: min_distance,
            dot,
        },
        param,
    )
}

#[expect(clippy::many_single_char_names)] // geometry uses the conventional point names
fn solve_quadratic(x: &mut [f64; 3], a: f64, b: f64, c: f64) -> usize {
    if a == 0.0 || b.abs() > 1e12 * a.abs() {
        if b == 0.0 {
            return 0;
        }
        x[0] = -c / b;
        return 1;
    }
    let dscr = b * b - 4.0 * a * c;
    if dscr > 0.0 {
        let s = dscr.sqrt();
        x[0] = (-b + s) / (2.0 * a);
        x[1] = (-b - s) / (2.0 * a);
        2
    } else if dscr == 0.0 {
        x[0] = -b / (2.0 * a);
        1
    } else {
        0
    }
}

#[expect(clippy::many_single_char_names)] // geometry uses the conventional point names
fn solve_cubic_normed(x: &mut [f64; 3], a: f64, b: f64, c: f64) -> usize {
    let a2 = a * a;
    let mut q = (a2 - 3.0 * b) / 9.0;
    let r = (a * (2.0 * a2 - 9.0 * b) + 27.0 * c) / 54.0;
    let r2 = r * r;
    let q3 = q * q * q;
    let a3 = a / 3.0;
    if r2 < q3 {
        let t = (r / q3.sqrt()).clamp(-1.0, 1.0).acos();
        q = -2.0 * q.sqrt();
        let tau = std::f64::consts::TAU;
        x[0] = q * (t / 3.0).cos() - a3;
        x[1] = q * ((t + tau) / 3.0).cos() - a3;
        x[2] = q * ((t - tau) / 3.0).cos() - a3;
        3
    } else {
        let sign = if r < 0.0 { 1.0 } else { -1.0 };
        let u = sign * (r.abs() + (r2 - q3).sqrt()).cbrt();
        let v = if u == 0.0 { 0.0 } else { q / u };
        x[0] = (u + v) - a3;
        if u == v || (u - v).abs() < 1e-12 * (u + v).abs() {
            x[1] = -0.5 * (u + v) - a3;
            return 2;
        }
        1
    }
}

#[expect(clippy::many_single_char_names)] // geometry uses the conventional point names
fn solve_cubic(x: &mut [f64; 3], a: f64, b: f64, c: f64, d: f64) -> usize {
    if a != 0.0 {
        let bn = b / a;
        if bn.abs() < 1e6 {
            return solve_cubic_normed(x, bn, c / a, d / a);
        }
    }
    solve_quadratic(x, b, c, d)
}

#[derive(Clone, Copy, Debug)]
struct Edge {
    seg: Segment,
    color: u8,
}

/// A glyph outline as contours of colored edges.
#[derive(Clone, Debug, Default)]
pub struct Shape {
    contours: Vec<Vec<Edge>>,
}

impl Shape {
    /// Number of contours.
    #[must_use]
    pub fn contour_count(&self) -> usize {
        self.contours.len()
    }

    /// Number of edges over all contours.
    #[must_use]
    pub fn edge_count(&self) -> usize {
        self.contours.iter().map(Vec::len).sum()
    }

    /// The color mask (bit 0 red, bit 1 green, bit 2 blue) of every edge,
    /// contour by contour.
    #[must_use]
    pub fn edge_colors(&self) -> Vec<Vec<u8>> {
        self.contours
            .iter()
            .map(|c| c.iter().map(|e| e.color).collect())
            .collect()
    }

    /// Scales by `k` then translates by `t`.
    fn transform(&mut self, k: f64, t: V2) {
        for e in self.contours.iter_mut().flatten() {
            e.seg = e.seg.transform(k, t);
        }
    }

    /// Assigns edge colors (Chlumsky's simple coloring, deterministic seed).
    pub fn color_edges(&mut self, angle_threshold: f64) {
        let cross_threshold = angle_threshold.sin();
        for contour in &mut self.contours {
            color_contour(contour, cross_threshold);
        }
    }

    fn flattened(&self) -> Vec<[V2; 2]> {
        let mut lines = Vec::new();
        for e in self.contours.iter().flatten() {
            e.seg.flatten(&mut lines);
        }
        lines
    }
}

/// Non-zero winding number of `p` against flattened outline lines.
fn winding(lines: &[[V2; 2]], p: V2) -> i32 {
    let mut w = 0;
    for [a, b] in lines {
        let is_left = (b.x - a.x) * (p.y - a.y) - (p.x - a.x) * (b.y - a.y);
        if a.y <= p.y {
            if b.y > p.y && is_left > 0.0 {
                w += 1;
            }
        } else if b.y <= p.y && is_left < 0.0 {
            w -= 1;
        }
    }
    w
}

fn switch_color(color: u8, banned: u8) -> u8 {
    let combined = color & banned;
    if combined == RED || combined == GREEN || combined == BLUE {
        return combined ^ WHITE;
    }
    if color == 0 || color == WHITE {
        return CYAN;
    }
    let shifted = color << 1;
    (shifted | (shifted >> 3)) & WHITE
}

fn is_corner(a: V2, b: V2, cross_threshold: f64) -> bool {
    let (a, b) = (a.normalize(), b.normalize());
    a.dot(b) <= 0.0 || a.cross(b).abs() > cross_threshold
}

#[expect(clippy::cast_possible_truncation)] // the trichotomy result is in -1..=1
fn symmetrical_trichotomy(position: usize, n: usize) -> i64 {
    #[expect(clippy::cast_precision_loss)] // edge counts are tiny
    let (p, n) = (position as f64, n as f64);
    (3.0 + 2.875 * p / (n - 1.0) - 1.4375 + 0.5).floor() as i64 - 3
}

fn color_contour(edges: &mut Vec<Edge>, cross_threshold: f64) {
    let m = edges.len();
    if m == 0 {
        return;
    }
    let mut corners: Vec<usize> = Vec::new();
    let mut prev_dir = edges.last().map_or(V2::default(), |e| e.seg.direction(1.0));
    for (i, e) in edges.iter().enumerate() {
        if is_corner(prev_dir, e.seg.direction(0.0), cross_threshold) {
            corners.push(i);
        }
        prev_dir = e.seg.direction(1.0);
    }
    match corners.as_slice() {
        [] => {
            for e in edges.iter_mut() {
                e.color = WHITE;
            }
        }
        [corner] => {
            let c0 = switch_color(WHITE, 0);
            let colors = [c0, WHITE, switch_color(c0, 0)];
            if m >= 3 {
                for i in 0..m {
                    let idx = 1 + symmetrical_trichotomy(i, m);
                    let color = usize::try_from(idx)
                        .ok()
                        .and_then(|k| colors.get(k))
                        .copied()
                        .unwrap_or(WHITE);
                    if let Some(e) = edges.get_mut((corner + i) % m) {
                        e.color = color;
                    }
                }
            } else {
                // One or two edges: split into thirds, then color the parts.
                let mut parts: Vec<Edge> = Vec::with_capacity(6);
                for i in 0..m {
                    if let Some(e) = edges.get((corner + i) % m) {
                        for s in e.seg.split_in_thirds() {
                            parts.push(Edge { seg: s, color: WHITE });
                        }
                    }
                }
                let n = parts.len();
                for (i, p) in parts.iter_mut().enumerate() {
                    let k = if n == 3 { i } else { i / 2 };
                    p.color = colors.get(k).copied().unwrap_or(WHITE);
                }
                *edges = parts;
            }
        }
        [first, ..] => {
            let corner_count = corners.len();
            let mut spline = 0_usize;
            let start = *first;
            let mut color = switch_color(WHITE, 0);
            let initial = color;
            for i in 0..m {
                let index = (start + i) % m;
                if spline + 1 < corner_count && corners.get(spline + 1) == Some(&index) {
                    spline += 1;
                    let banned = if spline == corner_count - 1 { initial } else { 0 };
                    color = switch_color(color, banned);
                }
                if let Some(e) = edges.get_mut(index) {
                    e.color = color;
                }
            }
        }
    }
}

/// Collects a glyph outline into a [`Shape`].
#[derive(Default)]
struct ShapeBuilder {
    shape: Shape,
    current: Vec<Edge>,
    start: V2,
    last: V2,
}

impl ShapeBuilder {
    fn push(&mut self, seg: Segment) {
        let degenerate = match seg {
            Segment::Line([a, b]) => a == b,
            Segment::Quad([a, b, c]) => a == b && b == c,
        };
        if !degenerate {
            self.current.push(Edge { seg, color: WHITE });
        }
        self.last = seg.end();
    }

    fn finish_contour(&mut self) {
        if self.last != self.start {
            self.push(Segment::Line([self.last, self.start]));
        }
        if !self.current.is_empty() {
            self.shape.contours.push(std::mem::take(&mut self.current));
        }
    }
}

#[expect(clippy::many_single_char_names)] // geometry uses the conventional point names
impl ttf_parser::OutlineBuilder for ShapeBuilder {
    fn move_to(&mut self, x: f32, y: f32) {
        self.finish_contour();
        self.start = V2::new(f64::from(x), f64::from(y));
        self.last = self.start;
    }

    fn line_to(&mut self, x: f32, y: f32) {
        let p = V2::new(f64::from(x), f64::from(y));
        self.push(Segment::Line([self.last, p]));
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let c = V2::new(f64::from(x1), f64::from(y1));
        let p = V2::new(f64::from(x), f64::from(y));
        self.push(Segment::Quad([self.last, c, p]));
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        // Split the cubic into four pieces and approximate each by a quadratic
        // whose control point is (3 (c1 + c2) - p0 - p3) / 4.
        let p0 = self.last;
        let c1 = V2::new(f64::from(x1), f64::from(y1));
        let c2 = V2::new(f64::from(x2), f64::from(y2));
        let p3 = V2::new(f64::from(x), f64::from(y));
        let cubic = |t: f64| {
            let a = p0.lerp(c1, t);
            let b = c1.lerp(c2, t);
            let c = c2.lerp(p3, t);
            a.lerp(b, t).lerp(b.lerp(c, t), t)
        };
        let deriv = |t: f64| {
            let a = c1.sub(p0);
            let b = c2.sub(c1);
            let c = p3.sub(c2);
            a.lerp(b, t).lerp(b.lerp(c, t), t).mul(3.0)
        };
        let n = 4;
        for i in 0..n {
            let t0 = f64::from(i) / f64::from(n);
            let t1 = f64::from(i + 1) / f64::from(n);
            let a = cubic(t0);
            let b = cubic(t1);
            let h = (t1 - t0) / 3.0;
            let q1 = a.add(deriv(t0).mul(h));
            let q2 = b.sub(deriv(t1).mul(h));
            let ctrl = q1.add(q2).mul(3.0).sub(a).sub(b).mul(0.25);
            self.push(Segment::Quad([a, ctrl, b]));
        }
    }

    fn close(&mut self) {
        self.finish_contour();
        self.last = self.start;
    }
}

/// An MSDF bitmap for one glyph.
#[derive(Clone, Debug, PartialEq)]
pub struct MsdfGlyph {
    /// Width in texels (padding included).
    pub width: u32,
    /// Height in texels (padding included).
    pub height: u32,
    /// RGBA8, row-major, top row first.
    pub pixels: Vec<u8>,
    /// Left edge of the bitmap relative to the pen, in pixels at the
    /// generation em size.
    pub left: f32,
    /// Top edge of the bitmap above the baseline, in pixels at the generation
    /// em size (y up).
    pub top: f32,
    /// Pixels per font unit at the generation em size.
    pub scale: f32,
    /// Distance range in texels.
    pub range: f32,
    /// Font-unit coordinates of the bitmap's bottom-left corner.
    pub origin: [f32; 2],
}

impl MsdfGlyph {
    /// Font-unit coordinates of a point given in texels from the bitmap's
    /// top-left corner (y down). Texel `(c, r)` has its center at
    /// `(c + 0.5, r + 0.5)`.
    #[must_use]
    #[expect(clippy::cast_precision_loss)] // bitmap sizes are tiny
    pub fn texel_to_font(&self, tx: f32, ty: f32) -> [f32; 2] {
        let h = self.height as f32;
        [
            self.origin[0] + tx / self.scale,
            self.origin[1] + (h - ty) / self.scale,
        ]
    }
}

/// Builds the colored [`Shape`] of a glyph in font units, or `None` when the
/// glyph has no outline.
#[must_use]
pub fn glyph_shape(face: &ttf_parser::Face<'_>, glyph: u16) -> Option<(Shape, ttf_parser::Rect)> {
    let mut builder = ShapeBuilder::default();
    let rect = face.outline_glyph(ttf_parser::GlyphId(glyph), &mut builder)?;
    builder.finish_contour();
    let mut shape = builder.shape;
    if shape.contours.is_empty() {
        return None;
    }
    shape.color_edges(DEFAULT_ANGLE_THRESHOLD);
    Some((shape, rect))
}

/// Generates the MSDF of `glyph` at `em_px` pixels per em with a distance
/// range of `range` texels and `padding` texels of border. Returns `None`
/// for glyphs without an outline (spaces) or absurd sizes.
#[must_use]
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)] // bitmap dimensions: positive, clamped to 4096
pub fn generate_glyph(
    face: &ttf_parser::Face<'_>,
    glyph: u16,
    em_px: f32,
    range: f32,
    padding: u32,
) -> Option<MsdfGlyph> {
    let (mut shape, rect) = glyph_shape(face, glyph)?;
    let upem = f32::from(face.units_per_em().max(1));
    let scale = em_px / upem;
    let (x_min, y_min) = (f32::from(rect.x_min), f32::from(rect.y_min));
    let w_units = f32::from(rect.x_max) - x_min;
    let h_units = f32::from(rect.y_max) - y_min;
    let pad = padding as f32;
    let width = ((w_units * scale).ceil() + 2.0 * pad).clamp(1.0, 4096.0) as u32;
    let height = ((h_units * scale).ceil() + 2.0 * pad).clamp(1.0, 4096.0) as u32;
    let k = f64::from(scale);
    shape.transform(
        k,
        V2::new(
            f64::from(pad) - f64::from(x_min) * k,
            f64::from(pad) - f64::from(y_min) * k,
        ),
    );
    let mut pixels = vec![0_u8; (width * height * 4) as usize];
    generate(&shape, width, height, range, &mut pixels);
    Some(MsdfGlyph {
        width,
        height,
        pixels,
        left: x_min * scale - pad,
        top: y_min * scale - pad + height as f32,
        scale,
        range,
        origin: [x_min - pad / scale, y_min - pad / scale],
    })
}

/// Median of three.
#[must_use]
pub fn median(a: f32, b: f32, c: f32) -> f32 {
    a.min(b).max(a.max(b).min(c))
}

fn encode(d: f64, range: f64) -> u8 {
    let v = (d / range + 0.5).clamp(0.0, 1.0) * 255.0;
    // In 0..=255 after the clamp.
    #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let byte = v.round() as u8;
    byte
}

/// Fills `out` (`width * height * 4` bytes, RGBA8, top row first) with the
/// MSDF of a shape already transformed into texel space (y up, texel
/// `(c, r)` centered at `(c + 0.5, height - r - 0.5)`).
fn generate(shape: &Shape, width: u32, height: u32, range: f32, out: &mut [u8]) {
    let range = f64::from(range.max(1e-3));
    let lines = shape.flattened();
    let mut texels: Vec<[f64; 4]> = Vec::with_capacity((width * height) as usize);
    let mut agree = 0_i64;
    for row in 0..height {
        for col in 0..width {
            let p = V2::new(f64::from(col) + 0.5, f64::from(height) - f64::from(row) - 0.5);
            let mut best = [(SignedDistance::FAR, None::<Edge>, 0.0); 3];
            let mut closest = SignedDistance::FAR;
            for e in shape.contours.iter().flatten() {
                let (sd, param) = e.seg.signed_distance(p);
                for (ch, slot) in best.iter_mut().enumerate() {
                    if e.color & (1 << ch) != 0 && sd.less(slot.0) {
                        *slot = (sd, Some(*e), param);
                    }
                }
                if sd.less(closest) {
                    closest = sd;
                }
            }
            let mut px = [0.0_f64; 4];
            for (ch, (sd, edge, param)) in best.iter().enumerate() {
                if let Some(slot) = px.get_mut(ch) {
                    *slot = edge.map_or(-1e9, |e| e.seg.to_pseudo(*sd, p, *param).distance);
                }
            }
            let inside = winding(&lines, p) != 0;
            let true_abs = closest.distance.abs();
            px[3] = if inside { true_abs } else { -true_abs };
            if closest.distance.abs() > 1e-9 {
                agree += if (closest.distance > 0.0) == inside { 1 } else { -1 };
            }
            texels.push(px);
        }
    }
    let flip = agree < 0;
    for (t, dst) in texels.iter_mut().zip(out.as_chunks_mut::<4>().0) {
        if flip {
            t[0] = -t[0];
            t[1] = -t[1];
            t[2] = -t[2];
        }
        let med = t[0].min(t[1]).max(t[0].max(t[1]).min(t[2]));
        if t[3] != 0.0 && (med > 0.0) != (t[3] > 0.0) {
            t[0] = t[3];
            t[1] = t[3];
            t[2] = t[3];
        }
        for (d, v) in dst.iter_mut().zip(t.iter()) {
            *d = encode(*v, range);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_font::{GlyphShape, TestFontBuilder};

    #[test]
    fn rectangle_is_four_colored_edges() -> Result<(), Box<dyn std::error::Error>> {
        let bytes = TestFontBuilder::new().glyph('a', GlyphShape::Rect).build();
        let face = ttf_parser::Face::parse(&bytes, 0)?;
        let gid = face.glyph_index('a').ok_or("gid")?;
        let (shape, _) = glyph_shape(&face, gid.0).ok_or("shape")?;
        assert_eq!(shape.contour_count(), 1);
        let colors = shape.edge_colors();
        let c = colors.first().ok_or("contour")?;
        assert_eq!(c.len(), 4);
        // Edges meeting at a corner share exactly one channel.
        for i in 0..4 {
            let a = c.get(i).copied().unwrap_or(0);
            let b = c.get((i + 1) % 4).copied().unwrap_or(0);
            assert_eq!((a & b).count_ones(), 1, "{c:?}");
        }
        Ok(())
    }

    #[test]
    fn circle_is_white_and_quadratic() -> Result<(), Box<dyn std::error::Error>> {
        let bytes = TestFontBuilder::new().glyph('a', GlyphShape::Circle).build();
        let face = ttf_parser::Face::parse(&bytes, 0)?;
        let gid = face.glyph_index('a').ok_or("gid")?;
        let (shape, _) = glyph_shape(&face, gid.0).ok_or("shape")?;
        assert!(shape.edge_colors().iter().flatten().all(|c| *c == WHITE));
        let g = generate_glyph(&face, gid.0, 32.0, 4.0, 5).ok_or("msdf")?;
        // Center texel is deep inside.
        let (cx, cy) = (g.width / 2, g.height / 2);
        let i = ((cy * g.width + cx) * 4) as usize;
        let px = g.pixels.get(i..i + 3).ok_or("px")?;
        assert!(px.iter().all(|v| *v > 200));
        Ok(())
    }

    #[test]
    fn teardrop_splits_into_three_colors() {
        // A single quadratic loop with one corner.
        let mut edges = vec![
            Edge {
                seg: Segment::Quad([V2::new(0.0, 0.0), V2::new(10.0, 10.0), V2::new(0.0, 10.0)]),
                color: WHITE,
            },
            Edge {
                seg: Segment::Quad([V2::new(0.0, 10.0), V2::new(-10.0, 10.0), V2::new(0.0, 0.0)]),
                color: WHITE,
            },
        ];
        color_contour(&mut edges, DEFAULT_ANGLE_THRESHOLD.sin());
        assert_eq!(edges.len(), 6);
        let distinct: std::collections::BTreeSet<u8> = edges.iter().map(|e| e.color).collect();
        assert_eq!(distinct.len(), 3);
    }

    #[test]
    fn cubics_become_quadratics_and_render() {
        use ttf_parser::OutlineBuilder as _;
        // A square with cubic "rounded" sides: four cubics, each split in four.
        let mut b = ShapeBuilder::default();
        b.move_to(0.0, 0.0);
        b.curve_to(-5.0, 10.0, -5.0, 20.0, 0.0, 30.0);
        b.curve_to(10.0, 35.0, 20.0, 35.0, 30.0, 30.0);
        b.curve_to(35.0, 20.0, 35.0, 10.0, 30.0, 0.0);
        b.curve_to(20.0, -5.0, 10.0, -5.0, 0.0, 0.0);
        b.close();
        let mut shape = b.shape;
        assert_eq!(shape.edge_count(), 16);
        assert!(
            shape
                .contours
                .iter()
                .flatten()
                .all(|e| matches!(e.seg, Segment::Quad(_)))
        );
        // Every quadratic piece stays close to the cubic it replaces.
        let first = shape.contours.first().and_then(|c| c.first()).map(|e| e.seg);
        let mid = first.map(|s| s.point(0.5)).unwrap_or_default();
        let cubic_at = |t: f64| {
            let u = 1.0 - t;
            let x = u * u * u * 0.0 + 3.0 * u * u * t * -5.0 + 3.0 * u * t * t * -5.0;
            let y = 3.0 * u * u * t * 10.0 + 3.0 * u * t * t * 20.0 + t * t * t * 30.0;
            V2::new(x, y)
        };
        assert!(mid.sub(cubic_at(0.125)).len() < 0.05, "{mid:?}");
        shape.color_edges(DEFAULT_ANGLE_THRESHOLD);
        shape.transform(1.0, V2::new(8.0, 8.0));
        let (w, h) = (48, 48);
        let mut px = vec![0_u8; w * h * 4];
        generate(&shape, 48, 48, 4.0, &mut px);
        // Center (texel 23, 24 from the top) is inside; a corner texel is outside.
        let at = |c: usize, r: usize| px.get((r * w + c) * 4..(r * w + c) * 4 + 3).map(<[u8]>::to_vec);
        assert!(at(23, 24).is_some_and(|v| v.iter().all(|x| *x > 200)));
        assert!(at(1, 1).is_some_and(|v| v.iter().all(|x| *x < 60)));
    }

    #[test]
    fn cubic_solver_finds_roots() {
        let mut x = [0.0; 3];
        // (t - 1)(t - 2)(t - 3) = t^3 - 6t^2 + 11t - 6
        let n = solve_cubic(&mut x, 1.0, -6.0, 11.0, -6.0);
        assert_eq!(n, 3);
        let mut r = x;
        r.sort_by(f64::total_cmp);
        assert!((r[0] - 1.0).abs() < 1e-9 && (r[1] - 2.0).abs() < 1e-9 && (r[2] - 3.0).abs() < 1e-9);
        assert_eq!(solve_cubic(&mut x, 0.0, 0.0, 2.0, -4.0), 1);
        assert!((x[0] - 2.0).abs() < 1e-12);
    }
}
