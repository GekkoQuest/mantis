//! Fixture builder: minimal valid TrueType fonts generated in code.
//!
//! This module exists for tests and tools. It writes a complete `sfnt` file
//! with the tables `head`, `hhea`, `maxp` (version 1.0), `hmtx`, `cmap`
//! (format 4 for the Basic Multilingual Plane, plus format 12 when a covered
//! codepoint lies above it), `loca` (long offsets), `glyf`, and `post`
//! (version 3). Every covered codepoint maps to its own glyph whose outline is
//! one of a few simple [`GlyphShape`]s, so the shaper, the fallback logic, and
//! the MSDF generator can be exercised without font files in the repository
//! and without system fonts.
//!
//! Contours follow the TrueType convention: outer contours run clockwise in
//! the font's y-up space and holes run counter-clockwise, so the non-zero
//! winding rule fills exactly the analytic shape reported by
//! [`GlyphShape::contains`].
//!
//! Glyph 0 is `.notdef` (an empty outline). Whitespace codepoints get an empty
//! outline and the normal advance.

/// The outline drawn for a covered codepoint. Coordinates are in font units
/// and scale with the builder's units per em; see [`GlyphShape::contours`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GlyphShape {
    /// A filled axis-aligned rectangle (four sharp corners).
    Rect,
    /// A filled triangle (three sharp corners, two of them acute).
    Triangle,
    /// A square with a square hole (an outer contour and a hole contour).
    SquareWithHole,
    /// A disc drawn with quadratic curves (no corners).
    Circle,
    /// No outline (used for whitespace).
    Empty,
}

/// One outline point: font units, and whether it lies on the curve.
pub type OutlinePoint = (i16, i16, bool);

/// Scales a coordinate designed for 1000 units per em.
fn su(v: i32, upem: u16) -> i16 {
    let scaled = v * i32::from(upem) / 1000;
    i16::try_from(scaled).unwrap_or(if scaled < 0 { i16::MIN } else { i16::MAX })
}

impl GlyphShape {
    /// The contours of this shape for a font with `units_per_em`, designed on
    /// a 1000-unit em and scaled. Outer contours are clockwise (y up), holes
    /// counter-clockwise.
    #[must_use]
    pub fn contours(self, units_per_em: u16) -> Vec<Vec<OutlinePoint>> {
        let s = |v: i32| su(v, units_per_em);
        match self {
            Self::Rect => vec![vec![
                (s(100), s(0), true),
                (s(100), s(700), true),
                (s(500), s(700), true),
                (s(500), s(0), true),
            ]],
            Self::Triangle => vec![vec![
                (s(100), s(0), true),
                (s(300), s(700), true),
                (s(500), s(0), true),
            ]],
            Self::SquareWithHole => vec![
                vec![
                    (s(50), s(0), true),
                    (s(50), s(500), true),
                    (s(550), s(500), true),
                    (s(550), s(0), true),
                ],
                vec![
                    (s(200), s(150), true),
                    (s(400), s(150), true),
                    (s(400), s(350), true),
                    (s(200), s(350), true),
                ],
            ],
            Self::Circle => {
                // Center (300, 300), radius 250: eight quadratic arcs with
                // off-curve control points at the octagon corners.
                let (cx, cy, r) = (300, 300, 250);
                // tan(22.5 deg) * r, rounded.
                let t = 104;
                vec![vec![
                    (s(cx - r), s(cy), true),
                    (s(cx - r), s(cy + t), false),
                    (s(cx - 177), s(cy + 177), true),
                    (s(cx - t), s(cy + r), false),
                    (s(cx), s(cy + r), true),
                    (s(cx + t), s(cy + r), false),
                    (s(cx + 177), s(cy + 177), true),
                    (s(cx + r), s(cy + t), false),
                    (s(cx + r), s(cy), true),
                    (s(cx + r), s(cy - t), false),
                    (s(cx + 177), s(cy - 177), true),
                    (s(cx + t), s(cy - r), false),
                    (s(cx), s(cy - r), true),
                    (s(cx - t), s(cy - r), false),
                    (s(cx - 177), s(cy - 177), true),
                    (s(cx - r), s(cy - t), false),
                ]]
            }
            Self::Empty => Vec::new(),
        }
    }

    /// Analytic inside test in font units (non-zero rule), for the
    /// straight-edged shapes. `Circle` uses the ideal circle the curves
    /// approximate; `Empty` contains nothing.
    #[must_use]
    pub fn contains(self, units_per_em: u16, x: f32, y: f32) -> bool {
        let k = f32::from(units_per_em) / 1000.0;
        let (x, y) = (x / k, y / k);
        match self {
            Self::Rect => (100.0..=500.0).contains(&x) && (0.0..=700.0).contains(&y),
            Self::Triangle => {
                if !(0.0..=700.0).contains(&y) {
                    return false;
                }
                // Left edge from (100,0) to (300,700); right edge from (300,700) to (500,0).
                let half = 200.0 * (1.0 - y / 700.0);
                (x - 300.0).abs() <= half
            }
            Self::SquareWithHole => {
                let outer = (50.0..=550.0).contains(&x) && (0.0..=500.0).contains(&y);
                let hole = (200.0..=400.0).contains(&x) && (150.0..=350.0).contains(&y);
                outer && !hole
            }
            Self::Circle => {
                let (dx, dy) = (x - 300.0, y - 300.0);
                dx * dx + dy * dy <= 250.0 * 250.0
            }
            Self::Empty => false,
        }
    }
}

/// Builds a minimal TrueType font. See the module documentation.
#[derive(Clone, Debug)]
pub struct TestFontBuilder {
    units_per_em: u16,
    advance: u16,
    ascender: i16,
    descender: i16,
    line_gap: i16,
    glyphs: Vec<(char, GlyphShape)>,
}

impl Default for TestFontBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl TestFontBuilder {
    /// A builder with 1000 units per em, advance 600, ascender 800,
    /// descender -200, line gap 0, and no codepoints.
    #[must_use]
    pub fn new() -> Self {
        Self {
            units_per_em: 1000,
            advance: 600,
            ascender: 800,
            descender: -200,
            line_gap: 0,
            glyphs: Vec::new(),
        }
    }

    /// Sets units per em (16..=16384). Shapes and metrics given later are not
    /// rescaled; shapes always scale with this value.
    #[must_use]
    pub fn units_per_em(mut self, upem: u16) -> Self {
        self.units_per_em = upem.clamp(16, 16384);
        self
    }

    /// Sets the advance width of every glyph, in font units.
    #[must_use]
    pub fn advance(mut self, advance: u16) -> Self {
        self.advance = advance;
        self
    }

    /// Sets ascender, descender (negative below the baseline), and line gap.
    #[must_use]
    pub fn metrics(mut self, ascender: i16, descender: i16, line_gap: i16) -> Self {
        self.ascender = ascender;
        self.descender = descender;
        self.line_gap = line_gap;
        self
    }

    /// Covers one codepoint with `shape` (replacing an earlier entry).
    #[must_use]
    pub fn glyph(mut self, c: char, shape: GlyphShape) -> Self {
        self.glyphs.retain(|(g, _)| *g != c);
        self.glyphs.push((c, shape));
        self
    }

    /// Covers every codepoint of `chars` with `shape`; whitespace always gets
    /// [`GlyphShape::Empty`].
    #[must_use]
    pub fn glyphs(mut self, chars: impl IntoIterator<Item = char>, shape: GlyphShape) -> Self {
        for c in chars {
            let s = if c.is_whitespace() {
                GlyphShape::Empty
            } else {
                shape
            };
            self = self.glyph(c, s);
        }
        self
    }

    /// Writes the font file.
    #[must_use]
    #[allow(clippy::too_many_lines)] // declarative table layout, field by field
    pub fn build(&self) -> Vec<u8> {
        let mut entries = self.glyphs.clone();
        entries.sort_by_key(|(c, _)| *c);
        // Glyph 0 is .notdef; covered codepoints follow in codepoint order.
        let mut glyf = Vec::new();
        let mut loca: Vec<u32> = vec![0];
        let mut max_points = 0_usize;
        let mut max_contours = 0_usize;
        let mut bbox = (i16::MAX, i16::MAX, i16::MIN, i16::MIN);
        let shapes = std::iter::once(GlyphShape::Empty).chain(entries.iter().map(|(_, s)| *s));
        let mut lsbs = Vec::new();
        for shape in shapes {
            let contours = shape.contours(self.units_per_em);
            let (data, b) = encode_glyph(&contours);
            if let Some(b) = b {
                bbox = (bbox.0.min(b.0), bbox.1.min(b.1), bbox.2.max(b.2), bbox.3.max(b.3));
                lsbs.push(b.0);
            } else {
                lsbs.push(0);
            }
            max_points = max_points.max(contours.iter().map(Vec::len).sum());
            max_contours = max_contours.max(contours.len());
            glyf.extend_from_slice(&data);
            while glyf.len() % 4 != 0 {
                glyf.push(0);
            }
            loca.push(u32::try_from(glyf.len()).unwrap_or(u32::MAX));
        }
        if bbox.0 > bbox.2 {
            bbox = (0, 0, 0, 0);
        }
        let num_glyphs = u16::try_from(entries.len() + 1).unwrap_or(u16::MAX);
        let mapping: Vec<(u32, u16)> = entries
            .iter()
            .enumerate()
            .map(|(i, (c, _))| (u32::from(*c), u16::try_from(i + 1).unwrap_or(0)))
            .collect();

        let mut head = Vec::new();
        put_u32(&mut head, 0x0001_0000);
        put_u32(&mut head, 0x0001_0000);
        put_u32(&mut head, 0); // checksum adjustment, patched below
        put_u32(&mut head, 0x5F0F_3CF5);
        put_u16(&mut head, 0x000B); // baseline at y=0, lsb at x=0, integer scaling
        put_u16(&mut head, self.units_per_em);
        put_u64(&mut head, 0);
        put_u64(&mut head, 0);
        put_i16(&mut head, bbox.0);
        put_i16(&mut head, bbox.1);
        put_i16(&mut head, bbox.2);
        put_i16(&mut head, bbox.3);
        put_u16(&mut head, 0);
        put_u16(&mut head, 8);
        put_i16(&mut head, 2);
        put_i16(&mut head, 1); // long loca
        put_i16(&mut head, 0);

        let mut hhea = Vec::new();
        put_u32(&mut hhea, 0x0001_0000);
        put_i16(&mut hhea, self.ascender);
        put_i16(&mut hhea, self.descender);
        put_i16(&mut hhea, self.line_gap);
        put_u16(&mut hhea, self.advance);
        put_i16(&mut hhea, bbox.0);
        put_i16(&mut hhea, 0);
        put_i16(&mut hhea, bbox.2);
        put_i16(&mut hhea, 1);
        put_i16(&mut hhea, 0);
        put_i16(&mut hhea, 0);
        for _ in 0..4 {
            put_i16(&mut hhea, 0);
        }
        put_i16(&mut hhea, 0);
        put_u16(&mut hhea, num_glyphs);

        let mut maxp = Vec::new();
        put_u32(&mut maxp, 0x0001_0000);
        put_u16(&mut maxp, num_glyphs);
        put_u16(&mut maxp, u16::try_from(max_points).unwrap_or(u16::MAX));
        put_u16(&mut maxp, u16::try_from(max_contours).unwrap_or(u16::MAX));
        put_u16(&mut maxp, 0);
        put_u16(&mut maxp, 0);
        put_u16(&mut maxp, 2);
        for _ in 0..8 {
            put_u16(&mut maxp, 0);
        }

        let mut hmtx = Vec::new();
        for lsb in &lsbs {
            put_u16(&mut hmtx, self.advance);
            put_i16(&mut hmtx, *lsb);
        }

        let mut loca_bytes = Vec::new();
        for off in &loca {
            put_u32(&mut loca_bytes, *off);
        }

        let mut post = Vec::new();
        put_u32(&mut post, 0x0003_0000);
        put_u32(&mut post, 0);
        put_i16(&mut post, -100);
        put_i16(&mut post, 50);
        for _ in 0..5 {
            put_u32(&mut post, 0);
        }

        let cmap = encode_cmap(&mapping);
        let tables: [(&[u8; 4], Vec<u8>); 8] = [
            (b"cmap", cmap),
            (b"glyf", glyf),
            (b"head", head),
            (b"hhea", hhea),
            (b"hmtx", hmtx),
            (b"loca", loca_bytes),
            (b"maxp", maxp),
            (b"post", post),
        ];
        assemble(&tables)
    }
}

/// A font covering printable ASCII (space included) with rectangles, plus
/// `o` as a square with a hole, `A` as a triangle and `O` as a circle.
#[must_use]
pub fn latin() -> Vec<u8> {
    TestFontBuilder::new()
        .glyphs((' '..='~').collect::<Vec<_>>(), GlyphShape::Rect)
        .glyph('o', GlyphShape::SquareWithHole)
        .glyph('A', GlyphShape::Triangle)
        .glyph('O', GlyphShape::Circle)
        .build()
}

/// Hangul syllables covered by [`cjk`].
pub const CJK_HANGUL: &[char] = &['가', '나', '다', '한', '글'];
/// Han ideographs covered by [`cjk`].
pub const CJK_HAN: &[char] = &['中', '文', '字', '日'];

/// A font covering a few Hangul syllables and Han ideographs (full-width
/// advance) with squares with holes. It does not cover ASCII.
#[must_use]
pub fn cjk() -> Vec<u8> {
    TestFontBuilder::new()
        .advance(1000)
        .metrics(880, -120, 0)
        .glyphs(CJK_HANGUL.iter().copied(), GlyphShape::SquareWithHole)
        .glyphs(CJK_HAN.iter().copied(), GlyphShape::Rect)
        .build()
}

/// A font covering the Hebrew letters U+05D0..=U+05EA with triangles. It
/// does not cover ASCII.
#[must_use]
pub fn rtl() -> Vec<u8> {
    TestFontBuilder::new()
        .advance(500)
        .glyphs(
            ('\u{05D0}'..='\u{05EA}').collect::<Vec<_>>(),
            GlyphShape::Triangle,
        )
        .build()
}

fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn put_i16(out: &mut Vec<u8>, v: i16) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_be_bytes());
}

type Bbox = (i16, i16, i16, i16);

/// Encodes one simple glyph. Returns the bytes and the bounding box, or empty
/// bytes for a glyph without contours.
fn encode_glyph(contours: &[Vec<OutlinePoint>]) -> (Vec<u8>, Option<Bbox>) {
    let points: Vec<OutlinePoint> = contours.iter().flatten().copied().collect();
    if points.is_empty() {
        return (Vec::new(), None);
    }
    let bbox = points
        .iter()
        .fold((i16::MAX, i16::MAX, i16::MIN, i16::MIN), |b, &(x, y, _)| {
            (b.0.min(x), b.1.min(y), b.2.max(x), b.3.max(y))
        });
    let mut out = Vec::new();
    put_i16(&mut out, i16::try_from(contours.len()).unwrap_or(i16::MAX));
    put_i16(&mut out, bbox.0);
    put_i16(&mut out, bbox.1);
    put_i16(&mut out, bbox.2);
    put_i16(&mut out, bbox.3);
    let mut end = 0_usize;
    for c in contours {
        end += c.len();
        put_u16(&mut out, u16::try_from(end.saturating_sub(1)).unwrap_or(u16::MAX));
    }
    put_u16(&mut out, 0); // no instructions
    for &(_, _, on) in &points {
        out.push(u8::from(on));
    }
    // Coordinates as 16-bit signed deltas (no short or repeat flags).
    let mut prev = 0_i32;
    for &(x, _, _) in &points {
        let d = i32::from(x) - prev;
        put_i16(&mut out, i16::try_from(d).unwrap_or(0));
        prev = i32::from(x);
    }
    prev = 0;
    for &(_, y, _) in &points {
        let d = i32::from(y) - prev;
        put_i16(&mut out, i16::try_from(d).unwrap_or(0));
        prev = i32::from(y);
    }
    (out, Some(bbox))
}

/// Encodes a `cmap` table with a format 4 subtable (3, 1) for the BMP and a
/// format 12 subtable (3, 10) when any codepoint lies above it. `mapping` is
/// sorted by codepoint.
fn encode_cmap(mapping: &[(u32, u16)]) -> Vec<u8> {
    // Group consecutive codepoints with consecutive glyph ids.
    let mut groups: Vec<(u32, u32, u16)> = Vec::new();
    for &(cp, gid) in mapping {
        if let Some(last) = groups.last_mut() {
            let next_gid = u32::from(last.2) + (last.1 - last.0) + 1;
            if cp == last.1 + 1 && u32::from(gid) == next_gid {
                last.1 = cp;
                continue;
            }
        }
        groups.push((cp, cp, gid));
    }
    let needs_12 = mapping.iter().any(|&(cp, _)| cp > 0xFFFF);

    // Format 4 segments: BMP groups plus the terminating 0xFFFF segment.
    let mut segs: Vec<(u16, u16, u16)> = groups
        .iter()
        .filter(|g| g.1 <= 0xFFFE)
        .map(|&(s, e, gid)| {
            let s16 = u16::try_from(s).unwrap_or(0);
            let e16 = u16::try_from(e).unwrap_or(0);
            (s16, e16, gid.wrapping_sub(s16))
        })
        .collect();
    segs.push((0xFFFF, 0xFFFF, 1));
    let seg_count = u16::try_from(segs.len()).unwrap_or(u16::MAX);
    let mut f4 = Vec::new();
    put_u16(&mut f4, 4);
    put_u16(&mut f4, 0); // length, patched below
    put_u16(&mut f4, 0);
    put_u16(&mut f4, seg_count.saturating_mul(2));
    let mut pow = 1_u16;
    let mut sel = 0_u16;
    while pow.saturating_mul(2) <= seg_count {
        pow = pow.saturating_mul(2);
        sel += 1;
    }
    put_u16(&mut f4, pow.saturating_mul(2));
    put_u16(&mut f4, sel);
    put_u16(
        &mut f4,
        seg_count.saturating_mul(2).saturating_sub(pow.saturating_mul(2)),
    );
    for s in &segs {
        put_u16(&mut f4, s.1);
    }
    put_u16(&mut f4, 0);
    for s in &segs {
        put_u16(&mut f4, s.0);
    }
    for s in &segs {
        put_u16(&mut f4, s.2);
    }
    for _ in &segs {
        put_u16(&mut f4, 0);
    }
    let f4_len = u16::try_from(f4.len()).unwrap_or(u16::MAX).to_be_bytes();
    if let Some(slot) = f4.get_mut(2..4) {
        slot.copy_from_slice(&f4_len);
    }

    let mut f12 = Vec::new();
    if needs_12 {
        put_u16(&mut f12, 12);
        put_u16(&mut f12, 0);
        put_u32(
            &mut f12,
            u32::try_from(16 + groups.len() * 12).unwrap_or(u32::MAX),
        );
        put_u32(&mut f12, 0);
        put_u32(&mut f12, u32::try_from(groups.len()).unwrap_or(u32::MAX));
        for &(s, e, gid) in &groups {
            put_u32(&mut f12, s);
            put_u32(&mut f12, e);
            put_u32(&mut f12, u32::from(gid));
        }
    }

    let num_tables: u16 = if needs_12 { 2 } else { 1 };
    let header_len = 4 + 8 * u32::from(num_tables);
    let mut out = Vec::new();
    put_u16(&mut out, 0);
    put_u16(&mut out, num_tables);
    put_u16(&mut out, 3);
    put_u16(&mut out, 1);
    put_u32(&mut out, header_len);
    if needs_12 {
        put_u16(&mut out, 3);
        put_u16(&mut out, 10);
        put_u32(&mut out, header_len + u32::try_from(f4.len()).unwrap_or(0));
    }
    out.extend_from_slice(&f4);
    out.extend_from_slice(&f12);
    out
}

fn checksum(data: &[u8]) -> u32 {
    let mut sum = 0_u32;
    let (chunks, rest) = data.as_chunks::<4>();
    for c in chunks {
        sum = sum.wrapping_add(u32::from_be_bytes(*c));
    }
    if !rest.is_empty() {
        let mut last = [0_u8; 4];
        for (d, s) in last.iter_mut().zip(rest) {
            *d = *s;
        }
        sum = sum.wrapping_add(u32::from_be_bytes(last));
    }
    sum
}

/// Writes the table directory and the tables (sorted by tag, 4-byte aligned),
/// then patches `head.checkSumAdjustment`.
fn assemble(tables: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
    let n = u16::try_from(tables.len()).unwrap_or(0);
    let mut pow = 1_u16;
    let mut sel = 0_u16;
    while pow * 2 <= n {
        pow *= 2;
        sel += 1;
    }
    let mut out = Vec::new();
    put_u32(&mut out, 0x0001_0000);
    put_u16(&mut out, n);
    put_u16(&mut out, pow * 16);
    put_u16(&mut out, sel);
    put_u16(&mut out, n * 16 - pow * 16);
    let mut offset = 12 + 16 * u32::from(n);
    let mut head_offset = None;
    for (tag, data) in tables {
        out.extend_from_slice(*tag);
        put_u32(&mut out, checksum(data));
        put_u32(&mut out, offset);
        put_u32(&mut out, u32::try_from(data.len()).unwrap_or(0));
        if *tag == b"head" {
            head_offset = Some(offset);
        }
        let padded = data.len().div_ceil(4) * 4;
        offset += u32::try_from(padded).unwrap_or(0);
    }
    for (_, data) in tables {
        out.extend_from_slice(data);
        while out.len() % 4 != 0 {
            out.push(0);
        }
    }
    if let Some(h) = head_offset.and_then(|h| usize::try_from(h).ok()) {
        let adjust = 0xB1B0_AFBA_u32.wrapping_sub(checksum(&out));
        if let Some(slot) = out.get_mut(h + 8..h + 12) {
            slot.copy_from_slice(&adjust.to_be_bytes());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustybuzz::ttf_parser;

    #[test]
    fn built_fonts_parse_and_map() -> Result<(), Box<dyn std::error::Error>> {
        for (bytes, probe) in [(latin(), 'a'), (cjk(), '한'), (rtl(), 'ש')] {
            let face = ttf_parser::Face::parse(&bytes, 0)?;
            let gid = face.glyph_index(probe).ok_or("unmapped probe")?;
            assert_ne!(gid.0, 0);
            assert!(face.glyph_index('\u{0400}').is_none());
            assert_eq!(face.units_per_em(), 1000);
            let mut sink = Sink(0);
            let rect = face.outline_glyph(gid, &mut sink).ok_or("no outline")?;
            assert!(rect.width() > 0 && sink.0 > 0);
            assert!(rustybuzz::Face::from_slice(&bytes, 0).is_some());
        }
        Ok(())
    }

    #[test]
    fn supplementary_plane_uses_format_12() -> Result<(), Box<dyn std::error::Error>> {
        let bytes = TestFontBuilder::new()
            .glyph('a', GlyphShape::Rect)
            .glyph('\u{20000}', GlyphShape::Rect)
            .build();
        let face = ttf_parser::Face::parse(&bytes, 0)?;
        assert!(face.glyph_index('\u{20000}').is_some());
        assert!(face.glyph_index('a').is_some());
        Ok(())
    }

    #[test]
    fn shapes_match_contains() {
        assert!(GlyphShape::Rect.contains(1000, 300.0, 350.0));
        assert!(!GlyphShape::Rect.contains(1000, 50.0, 350.0));
        assert!(GlyphShape::SquareWithHole.contains(1000, 100.0, 100.0));
        assert!(!GlyphShape::SquareWithHole.contains(1000, 300.0, 250.0));
        assert!(GlyphShape::Triangle.contains(1000, 300.0, 600.0));
        assert!(!GlyphShape::Triangle.contains(1000, 150.0, 600.0));
        assert!(GlyphShape::Circle.contains(1000, 300.0, 300.0));
        assert!(!GlyphShape::Empty.contains(1000, 300.0, 300.0));
    }

    struct Sink(usize);
    impl ttf_parser::OutlineBuilder for Sink {
        fn move_to(&mut self, _: f32, _: f32) {
            self.0 += 1;
        }
        fn line_to(&mut self, _: f32, _: f32) {
            self.0 += 1;
        }
        fn quad_to(&mut self, _: f32, _: f32, _: f32, _: f32) {
            self.0 += 1;
        }
        fn curve_to(&mut self, _: f32, _: f32, _: f32, _: f32, _: f32, _: f32) {
            self.0 += 1;
        }
        fn close(&mut self) {}
    }
}
