//! Text shaping and layout.
//!
//! The pipeline has two cached stages, so a node pays only for what changed:
//!
//! 1. [`Shaper::shape`] turns a string and a [`TextStyle`] into a
//!    [`ShapedText`]: it resolves bidi levels (`unicode-bidi`), walks grapheme
//!    clusters (`unicode-segmentation`) choosing a font per cluster from the
//!    style's fallback stack by coverage, classifies scripts, splits the text
//!    into runs of equal (bidi level, font, script), and shapes each run with
//!    `rustybuzz` in the direction of its level. It also records the line break
//!    opportunities (`unicode-linebreak`) and a per-byte advance prefix sum.
//!    The `rustybuzz` buffer is reused across calls through the
//!    `UnicodeBuffer` / `GlyphBuffer` round trip.
//! 2. [`ShapedText::layout`] breaks lines greedily at a maximum width (falling
//!    back to grapheme boundaries for a word wider than the line), reorders the
//!    runs of each line visually with `unicode-bidi`'s reordering, and writes
//!    positioned glyphs and line metrics into a reused [`TextLayout`].
//!
//! Layout is in logical pixels; the y axis points down and each glyph's `y`
//! is its baseline. Control characters (newlines, tabs) produce no glyph.

use std::ops::Range;

use rustybuzz::{Direction, UnicodeBuffer};
use unicode_bidi::{BidiInfo, Level};
use unicode_linebreak::BreakOpportunity;
use unicode_segmentation::UnicodeSegmentation;

use crate::font::{FontId, FontLibrary, StackId, is_ignorable};

/// What a piece of text is shaped with.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextStyle {
    /// Font fallback stack.
    pub stack: StackId,
    /// Font size in logical pixels per em.
    pub size: f32,
}

/// Horizontal alignment of lines inside a box.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TextAlign {
    /// Left edge.
    #[default]
    Start,
    /// Centered.
    Center,
    /// Right edge.
    End,
}

/// Script classes used to split runs. `Common` characters (spaces,
/// punctuation, digits, combining marks) join the run around them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Script {
    /// Shared between scripts; inherits the surrounding run's script.
    Common,
    /// A letter of a script this table does not name; the engine guesses.
    Unknown,
    /// Latin.
    Latin,
    /// Greek.
    Greek,
    /// Cyrillic.
    Cyrillic,
    /// Armenian.
    Armenian,
    /// Hebrew.
    Hebrew,
    /// Arabic.
    Arabic,
    /// Syriac.
    Syriac,
    /// Thaana.
    Thaana,
    /// Devanagari.
    Devanagari,
    /// Bengali.
    Bengali,
    /// Thai.
    Thai,
    /// Lao.
    Lao,
    /// Tibetan.
    Tibetan,
    /// Myanmar.
    Myanmar,
    /// Georgian.
    Georgian,
    /// Hangul.
    Hangul,
    /// Ethiopic.
    Ethiopic,
    /// Khmer.
    Khmer,
    /// Hiragana.
    Hiragana,
    /// Katakana.
    Katakana,
    /// Bopomofo.
    Bopomofo,
    /// Han ideographs.
    Han,
}

/// Script table: inclusive ranges, sorted.
const SCRIPT_RANGES: &[(u32, u32, Script)] = &[
    (0x0041, 0x005A, Script::Latin),
    (0x0061, 0x007A, Script::Latin),
    (0x00AA, 0x00AA, Script::Latin),
    (0x00BA, 0x00BA, Script::Latin),
    (0x00C0, 0x00D6, Script::Latin),
    (0x00D8, 0x00F6, Script::Latin),
    (0x00F8, 0x024F, Script::Latin),
    (0x0250, 0x02AF, Script::Latin),
    (0x0300, 0x036F, Script::Common),
    (0x0370, 0x03FF, Script::Greek),
    (0x0400, 0x052F, Script::Cyrillic),
    (0x0530, 0x058F, Script::Armenian),
    (0x0590, 0x05FF, Script::Hebrew),
    (0x0600, 0x06FF, Script::Arabic),
    (0x0700, 0x074F, Script::Syriac),
    (0x0750, 0x077F, Script::Arabic),
    (0x0780, 0x07BF, Script::Thaana),
    (0x08A0, 0x08FF, Script::Arabic),
    (0x0900, 0x097F, Script::Devanagari),
    (0x0980, 0x09FF, Script::Bengali),
    (0x0E00, 0x0E7F, Script::Thai),
    (0x0E80, 0x0EFF, Script::Lao),
    (0x0F00, 0x0FFF, Script::Tibetan),
    (0x1000, 0x109F, Script::Myanmar),
    (0x10A0, 0x10FF, Script::Georgian),
    (0x1100, 0x11FF, Script::Hangul),
    (0x1200, 0x139F, Script::Ethiopic),
    (0x1780, 0x17FF, Script::Khmer),
    (0x1AB0, 0x1AFF, Script::Common),
    (0x1DC0, 0x1DFF, Script::Common),
    (0x1E00, 0x1EFF, Script::Latin),
    (0x1F00, 0x1FFF, Script::Greek),
    (0x20D0, 0x20FF, Script::Common),
    (0x2C60, 0x2C7F, Script::Latin),
    (0x2DE0, 0x2DFF, Script::Cyrillic),
    (0x2E80, 0x2FDF, Script::Han),
    (0x3005, 0x3005, Script::Han),
    (0x3007, 0x3007, Script::Han),
    (0x3021, 0x3029, Script::Han),
    (0x3038, 0x303B, Script::Han),
    (0x3041, 0x309F, Script::Hiragana),
    (0x30A0, 0x30FF, Script::Katakana),
    (0x3100, 0x312F, Script::Bopomofo),
    (0x3130, 0x318F, Script::Hangul),
    (0x31A0, 0x31BF, Script::Bopomofo),
    (0x31F0, 0x31FF, Script::Katakana),
    (0x3400, 0x4DBF, Script::Han),
    (0x4E00, 0x9FFF, Script::Han),
    (0xA640, 0xA69F, Script::Cyrillic),
    (0xA720, 0xA7FF, Script::Latin),
    (0xA960, 0xA97F, Script::Hangul),
    (0xAC00, 0xD7FF, Script::Hangul),
    (0xF900, 0xFAFF, Script::Han),
    (0xFB00, 0xFB06, Script::Latin),
    (0xFB1D, 0xFB4F, Script::Hebrew),
    (0xFB50, 0xFDFF, Script::Arabic),
    (0xFE20, 0xFE2F, Script::Common),
    (0xFE70, 0xFEFC, Script::Arabic),
    (0xFF21, 0xFF3A, Script::Latin),
    (0xFF41, 0xFF5A, Script::Latin),
    (0xFF66, 0xFF9F, Script::Katakana),
    (0x20000, 0x3FFFF, Script::Han),
];

impl Script {
    /// Classifies one character.
    #[must_use]
    pub fn of(c: char) -> Self {
        let cp = u32::from(c);
        let i = SCRIPT_RANGES.partition_point(|&(_, end, _)| end < cp);
        if let Some(&(start, _, s)) = SCRIPT_RANGES.get(i)
            && start <= cp
        {
            return s;
        }
        if c.is_alphabetic() {
            Self::Unknown
        } else {
            Self::Common
        }
    }

    fn to_hb(self) -> Option<rustybuzz::Script> {
        use rustybuzz::script as s;
        Some(match self {
            Self::Common | Self::Unknown => return None,
            Self::Latin => s::LATIN,
            Self::Greek => s::GREEK,
            Self::Cyrillic => s::CYRILLIC,
            Self::Armenian => s::ARMENIAN,
            Self::Hebrew => s::HEBREW,
            Self::Arabic => s::ARABIC,
            Self::Syriac => s::SYRIAC,
            Self::Thaana => s::THAANA,
            Self::Devanagari => s::DEVANAGARI,
            Self::Bengali => s::BENGALI,
            Self::Thai => s::THAI,
            Self::Lao => s::LAO,
            Self::Tibetan => s::TIBETAN,
            Self::Myanmar => s::MYANMAR,
            Self::Georgian => s::GEORGIAN,
            Self::Hangul => s::HANGUL,
            Self::Ethiopic => s::ETHIOPIC,
            Self::Khmer => s::KHMER,
            Self::Hiragana => s::HIRAGANA,
            Self::Katakana => s::KATAKANA,
            Self::Bopomofo => s::BOPOMOFO,
            Self::Han => s::HAN,
        })
    }
}

/// One shaped glyph in logical run order (visual order inside an RTL run, as
/// the engine returns it).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShapedGlyph {
    /// Glyph id in the run's font.
    pub glyph: u16,
    /// Byte index in the text of the cluster this glyph belongs to.
    pub cluster: u32,
    /// Horizontal advance, logical px.
    pub advance: f32,
    /// Offset from the pen position (x right, y up), logical px.
    pub offset: [f32; 2],
}

/// A run of text with one bidi level, font, and script.
#[derive(Clone, Debug, PartialEq)]
pub struct ShapedRun {
    /// Byte range in the text.
    pub range: Range<usize>,
    /// Bidi embedding level (odd = right to left).
    pub level: u8,
    /// The font every cluster of the run was assigned.
    pub font: FontId,
    /// The run's script.
    pub script: Script,
    /// Range of this run's glyphs in [`ShapedText::glyphs`].
    pub glyphs: Range<usize>,
    /// Ascent above the baseline at the style size, px.
    pub ascent: f32,
    /// Descent below the baseline at the style size, px (positive).
    pub descent: f32,
    /// Line gap at the style size, px.
    pub line_gap: f32,
}

impl ShapedRun {
    /// True for a right-to-left run.
    #[must_use]
    pub fn is_rtl(&self) -> bool {
        self.level % 2 == 1
    }
}

/// The wrap-independent result of shaping one string.
#[derive(Clone, Debug, Default)]
pub struct ShapedText {
    text: String,
    style: Option<TextStyle>,
    levels: Vec<u8>,
    runs: Vec<ShapedRun>,
    glyphs: Vec<ShapedGlyph>,
    breaks: Vec<(usize, bool)>,
    prefix: Vec<f32>,
    base_metrics: [f32; 3],
}

impl ShapedText {
    /// The shaped string.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The style it was shaped with (`None` before the first shape).
    #[must_use]
    pub fn style(&self) -> Option<TextStyle> {
        self.style
    }

    /// Runs in logical order.
    #[must_use]
    pub fn runs(&self) -> &[ShapedRun] {
        &self.runs
    }

    /// All glyphs, run after run.
    #[must_use]
    pub fn glyphs(&self) -> &[ShapedGlyph] {
        &self.glyphs
    }

    /// Bidi level of the byte at `index`.
    #[must_use]
    pub fn level_at(&self, index: usize) -> Option<u8> {
        self.levels.get(index).copied()
    }

    /// Total advance of the clusters starting in `range`, px.
    #[must_use]
    pub fn width_of(&self, range: Range<usize>) -> f32 {
        let a = self.prefix.get(range.start).copied().unwrap_or(0.0);
        let b = self.prefix.get(range.end).copied().unwrap_or(a);
        b - a
    }

    /// Strips trailing whitespace and control characters from `range`.
    fn trim_end(&self, range: Range<usize>) -> usize {
        let Some(s) = self.text.get(range.clone()) else {
            return range.start;
        };
        range.start
            + s.trim_end_matches(|c: char| c.is_whitespace() || c.is_control())
                .len()
    }

    /// Lays the text out with lines no wider than `max_width` (unbounded when
    /// `None`), reusing `out`'s storage.
    pub fn layout(&self, max_width: Option<f32>, out: &mut TextLayout) {
        out.glyphs.clear();
        out.lines.clear();
        out.size = [0.0, 0.0];
        out.font_size = self.style.map_or(0.0, |s| s.size);
        let len = self.text.len();
        let mut line_start = 0_usize;
        let mut last_fit: Option<usize> = None;
        let mut bi = 0_usize;
        while let Some(&(b, mandatory)) = self.breaks.get(bi) {
            if b <= line_start {
                bi += 1;
                continue;
            }
            if let Some(max) = max_width {
                let w = self.width_of(line_start..self.trim_end(line_start..b));
                if w > max {
                    if let Some(lf) = last_fit {
                        self.push_line(line_start..lf, out);
                        line_start = lf;
                        last_fit = None;
                        continue;
                    }
                    let cut = self.grapheme_cut(line_start..b, max);
                    if cut > line_start && cut < b {
                        self.push_line(line_start..cut, out);
                        line_start = cut;
                        continue;
                    }
                }
            }
            last_fit = Some(b);
            if mandatory {
                self.push_line(line_start..b, out);
                line_start = b;
                last_fit = None;
            }
            bi += 1;
        }
        if line_start < len || out.lines.is_empty() || self.text.ends_with('\n') {
            self.push_line(line_start..len, out);
        }
        out.size[1] = out.lines.last().map_or(0.0, |l| l.top + l.height);
    }

    /// Largest grapheme boundary in `range` whose prefix fits `max`, keeping
    /// at least one grapheme.
    fn grapheme_cut(&self, range: Range<usize>, max: f32) -> usize {
        let Some(s) = self.text.get(range.clone()) else {
            return range.end;
        };
        let mut cut = range.end;
        for (i, g) in s.grapheme_indices(true) {
            let end = range.start + i + g.len();
            if i > 0 && self.width_of(range.start..end) > max {
                cut = range.start + i;
                break;
            }
        }
        cut
    }

    fn push_line(&self, range: Range<usize>, out: &mut TextLayout) {
        let visible_end = self.trim_end(range.clone());
        let [mut ascent, mut descent, mut gap] = self.base_metrics;
        out.run_levels.clear();
        out.run_indices.clear();
        for (i, run) in self.runs.iter().enumerate() {
            if run.range.start < visible_end && run.range.end > range.start {
                out.run_indices.push(i);
                out.run_levels
                    .push(Level::new(run.level).unwrap_or_else(|_| Level::ltr()));
                ascent = ascent.max(run.ascent);
                descent = descent.max(run.descent);
                gap = gap.max(run.line_gap);
            }
        }
        let top = out.lines.last().map_or(0.0, |l| l.top + l.height);
        let baseline = top + ascent;
        let glyph_start = out.glyphs.len();
        let order = BidiInfo::reorder_visual(&out.run_levels);
        let mut x = 0.0_f32;
        for vi in order {
            let Some(run) = out.run_indices.get(vi).and_then(|&ri| self.runs.get(ri)) else {
                continue;
            };
            let rtl = run.is_rtl();
            let Some(glyphs) = self.glyphs.get(run.glyphs.clone()) else {
                continue;
            };
            for g in glyphs {
                let c = g.cluster as usize;
                if c < range.start || c >= visible_end {
                    continue;
                }
                out.glyphs.push(PositionedGlyph {
                    font: run.font,
                    glyph: g.glyph,
                    x: x + g.offset[0],
                    y: baseline - g.offset[1],
                    advance: g.advance,
                    cluster: g.cluster,
                    rtl,
                });
                x += g.advance;
            }
        }
        out.size[0] = out.size[0].max(x);
        out.lines.push(LineMetrics {
            range: range.start..visible_end,
            glyphs: glyph_start..out.glyphs.len(),
            top,
            baseline,
            height: ascent + descent + gap,
            width: x,
            rtl: self.levels.get(range.start).is_some_and(|l| l % 2 == 1),
        });
    }
}

/// A glyph placed in a [`TextLayout`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PositionedGlyph {
    /// Font of the glyph.
    pub font: FontId,
    /// Glyph id in that font.
    pub glyph: u16,
    /// Pen x plus the glyph's x offset, relative to the line start, px.
    pub x: f32,
    /// Baseline y (minus the glyph's y offset), relative to the layout top, px.
    pub y: f32,
    /// Horizontal advance, px.
    pub advance: f32,
    /// Byte index of the glyph's cluster in the text.
    pub cluster: u32,
    /// True when the glyph belongs to a right-to-left run.
    pub rtl: bool,
}

/// Metrics of one laid-out line.
#[derive(Clone, Debug, PartialEq)]
pub struct LineMetrics {
    /// Byte range of the visible line (trailing whitespace excluded).
    pub range: Range<usize>,
    /// Range of the line's glyphs in [`TextLayout::glyphs`], visual order.
    pub glyphs: Range<usize>,
    /// Top of the line box, px.
    pub top: f32,
    /// Baseline, px.
    pub baseline: f32,
    /// Line box height, px.
    pub height: f32,
    /// Advance width of the visible line, px.
    pub width: f32,
    /// True when the line's first character is right to left.
    pub rtl: bool,
}

/// Positioned glyphs and line metrics for one string at one wrap width.
#[derive(Clone, Debug, Default)]
pub struct TextLayout {
    /// Glyphs, line after line, each line in visual (left to right) order.
    pub glyphs: Vec<PositionedGlyph>,
    /// Lines, top to bottom.
    pub lines: Vec<LineMetrics>,
    /// Width of the widest line and total height, px.
    pub size: [f32; 2],
    /// Font size the layout was made at, px per em.
    pub font_size: f32,
    run_levels: Vec<Level>,
    run_indices: Vec<usize>,
}

impl TextLayout {
    /// Horizontal offset of `line` inside a box of `box_width` for `align`.
    #[must_use]
    pub fn line_offset(&self, line: &LineMetrics, align: TextAlign, box_width: f32) -> f32 {
        let free = (box_width - line.width).max(0.0);
        match align {
            TextAlign::Start => 0.0,
            TextAlign::Center => free * 0.5,
            TextAlign::End => free,
        }
    }

    /// Index of the line that holds the caret at byte `index`.
    #[must_use]
    pub fn line_of(&self, index: usize) -> usize {
        let i = self
            .lines
            .iter()
            .rposition(|l| l.range.start <= index)
            .unwrap_or(0);
        i.min(self.lines.len().saturating_sub(1))
    }

    /// Caret x (relative to the line start) for byte `index`, and its line.
    #[must_use]
    pub fn caret_x(&self, index: usize) -> (usize, f32) {
        let li = self.line_of(index);
        let Some(line) = self.lines.get(li) else {
            return (0, 0.0);
        };
        let glyphs = self.glyphs.get(line.glyphs.clone()).unwrap_or(&[]);
        if let Some(g) = glyphs.iter().find(|g| g.cluster as usize == index) {
            return (li, if g.rtl { g.x + g.advance } else { g.x });
        }
        // After the last cluster that starts before `index`.
        let before = glyphs
            .iter()
            .filter(|g| (g.cluster as usize) < index)
            .max_by_key(|g| g.cluster);
        match before {
            Some(g) if g.rtl => (li, g.x),
            Some(g) => (li, g.x + g.advance),
            None if line.rtl => (li, line.width),
            None => (li, 0.0),
        }
    }

    /// Byte index nearest to the point (`x` relative to the line start, `y`
    /// relative to the layout top).
    #[must_use]
    pub fn hit_test(&self, x: f32, y: f32) -> usize {
        let li = self
            .lines
            .iter()
            .position(|l| y < l.top + l.height)
            .unwrap_or(self.lines.len().saturating_sub(1));
        let Some(line) = self.lines.get(li) else {
            return 0;
        };
        let glyphs = self.glyphs.get(line.glyphs.clone()).unwrap_or(&[]);
        let next_cluster = |c: u32| {
            glyphs
                .iter()
                .map(|g| g.cluster)
                .filter(|&k| k > c)
                .min()
                .map_or(line.range.end, |k| k as usize)
        };
        for g in glyphs {
            if x < g.x + g.advance {
                let left_half = x < g.x + g.advance * 0.5;
                return if left_half == g.rtl {
                    next_cluster(g.cluster)
                } else {
                    g.cluster as usize
                };
            }
        }
        if line.rtl {
            line.range.start
        } else {
            line.range.end
        }
    }
}

/// Shapes strings, reusing the engine's buffers between calls.
#[derive(Default)]
pub struct Shaper {
    buffer: Option<UnicodeBuffer>,
    items: Vec<Item>,
}

impl std::fmt::Debug for Shaper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shaper").finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug)]
struct Item {
    start: usize,
    end: usize,
    level: u8,
    font: FontId,
    script: Script,
}

impl Shaper {
    /// A engine with empty buffers.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Shapes `text` with `style` into `out`, reusing `out`'s storage.
    pub fn shape(&mut self, fonts: &FontLibrary, text: &str, style: TextStyle, out: &mut ShapedText) {
        out.text.clear();
        out.text.push_str(text);
        out.style = Some(style);
        out.levels.clear();
        out.runs.clear();
        out.glyphs.clear();
        out.breaks.clear();
        out.prefix.clear();
        out.base_metrics = fonts
            .stack(style.stack)
            .and_then(crate::font::FontStack::primary)
            .map_or([style.size * 0.8, style.size * 0.2, 0.0], |f| {
                run_metrics(fonts, f, style.size)
            });

        let bidi = BidiInfo::new(text, None);
        out.levels.extend(bidi.levels.iter().map(Level::number));
        self.itemize(fonts, text, style.stack, &out.levels);
        for item in &self.items {
            self.buffer = Some(shape_item(
                fonts,
                text,
                *item,
                style.size,
                self.buffer.take().unwrap_or_default(),
                out,
            ));
        }
        out.breaks.extend(
            unicode_linebreak::linebreaks(text).map(|(i, o)| (i, matches!(o, BreakOpportunity::Mandatory))),
        );
        out.prefix.resize(text.len() + 1, 0.0);
        for g in &out.glyphs {
            if let Some(slot) = out.prefix.get_mut(g.cluster as usize + 1) {
                *slot += g.advance;
            }
        }
        let mut acc = 0.0_f32;
        for p in &mut out.prefix {
            acc += *p;
            *p = acc;
        }
    }

    /// Splits `text` into items of equal (level, font, script), skipping
    /// control characters.
    fn itemize(&mut self, fonts: &FontLibrary, text: &str, stack: StackId, levels: &[u8]) {
        self.items.clear();
        let mut cur: Option<Item> = None;
        for (i, g) in text.grapheme_indices(true) {
            if g.chars().all(char::is_control) {
                if let Some(c) = cur.take() {
                    self.items.push(c);
                }
                continue;
            }
            let level = levels.get(i).copied().unwrap_or(0);
            let script = g
                .chars()
                .map(Script::of)
                .find(|s| *s != Script::Common)
                .unwrap_or(Script::Common);
            let keep_font = cur.filter(|c| {
                c.level == level
                    && (script == Script::Common || g.chars().all(is_ignorable))
                    && fonts.covers_cluster(c.font, g)
            });
            let font = match keep_font {
                Some(c) => c.font,
                None => match fonts.select(stack, g) {
                    Some(f) => f,
                    None => continue,
                },
            };
            if let Some(c) = cur.as_mut() {
                let script_ok = script == Script::Common || c.script == Script::Common || c.script == script;
                if c.level == level && c.font == font && script_ok && c.end == i {
                    c.end = i + g.len();
                    if c.script == Script::Common {
                        c.script = script;
                    }
                    continue;
                }
                self.items.push(*c);
            }
            cur = Some(Item {
                start: i,
                end: i + g.len(),
                level,
                font,
                script,
            });
        }
        if let Some(c) = cur {
            self.items.push(c);
        }
    }
}

/// Ascent, descent (positive), and line gap of a font at `size`, px.
fn run_metrics(fonts: &FontLibrary, font: FontId, size: f32) -> [f32; 3] {
    fonts.metrics(font).map_or([size * 0.8, size * 0.2, 0.0], |m| {
        let k = m.scale(size);
        [
            f32::from(m.ascender) * k,
            -f32::from(m.descender) * k,
            f32::from(m.line_gap) * k,
        ]
    })
}

/// Shapes one item, appends its run and glyphs to `out`, and returns the
/// cleared buffer for reuse.
#[allow(clippy::cast_precision_loss)] // glyph positions are font units, far below 2^24
fn shape_item(
    fonts: &FontLibrary,
    text: &str,
    item: Item,
    size: f32,
    mut buffer: UnicodeBuffer,
    out: &mut ShapedText,
) -> UnicodeBuffer {
    let glyph_start = out.glyphs.len();
    let [ascent, descent, line_gap] = run_metrics(fonts, item.font, size);
    let mut run = ShapedRun {
        range: item.start..item.end,
        level: item.level,
        font: item.font,
        script: item.script,
        glyphs: glyph_start..glyph_start,
        ascent,
        descent,
        line_gap,
    };
    let (Some(data), Some(slice)) = (fonts.data(item.font), text.get(item.start..item.end)) else {
        out.runs.push(run);
        return buffer;
    };
    let Some(face) = rustybuzz::Face::from_slice(data, 0) else {
        out.runs.push(run);
        return buffer;
    };
    buffer.clear();
    for (i, c) in slice.char_indices() {
        let cluster = u32::try_from(item.start + i).unwrap_or(u32::MAX);
        buffer.add(c, cluster);
    }
    if let Some(pre) = text.get(..item.start) {
        buffer.set_pre_context(pre);
    }
    if let Some(post) = text.get(item.end..) {
        buffer.set_post_context(post);
    }
    match item.script.to_hb() {
        Some(s) => buffer.set_script(s),
        None => buffer.guess_segment_properties(),
    }
    buffer.set_direction(if item.level % 2 == 1 {
        Direction::RightToLeft
    } else {
        Direction::LeftToRight
    });
    let shaped = rustybuzz::shape(&face, &[], buffer);
    let k = fonts.metrics(item.font).map_or(1.0, |m| m.scale(size));
    for (info, pos) in shaped.glyph_infos().iter().zip(shaped.glyph_positions()) {
        out.glyphs.push(ShapedGlyph {
            glyph: u16::try_from(info.glyph_id).unwrap_or(0),
            cluster: info.cluster,
            advance: pos.x_advance as f32 * k,
            offset: [pos.x_offset as f32 * k, pos.y_offset as f32 * k],
        });
    }
    run.glyphs = glyph_start..out.glyphs.len();
    out.runs.push(run);
    shaped.clear()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_font;

    fn library() -> Result<(FontLibrary, [FontId; 3], StackId), Box<dyn std::error::Error>> {
        let mut lib = FontLibrary::new();
        let latin = lib.add_font(test_font::latin())?;
        let cjk = lib.add_font(test_font::cjk())?;
        let rtl = lib.add_font(test_font::rtl())?;
        let stack = lib.define_stack("ui", &[latin, cjk, rtl])?;
        Ok((lib, [latin, cjk, rtl], stack))
    }

    #[test]
    fn latin_wraps_at_width() -> Result<(), Box<dyn std::error::Error>> {
        let (lib, _, stack) = library()?;
        let mut engine = Shaper::new();
        let mut shaped = ShapedText::default();
        let style = TextStyle { stack, size: 10.0 };
        // Every Latin glyph advances 6 px at size 10.
        engine.shape(&lib, "aaa bbb ccc", style, &mut shaped);
        assert!((shaped.width_of(0..11) - 66.0).abs() < 1e-3);
        let mut layout = TextLayout::default();
        shaped.layout(None, &mut layout);
        assert_eq!(layout.lines.len(), 1);
        shaped.layout(Some(45.0), &mut layout);
        assert_eq!(layout.lines.len(), 2, "{:?}", layout.lines);
        assert_eq!(layout.lines.first().map(|l| l.range.clone()), Some(0..7));
        assert!(layout.size[0] <= 45.0);
        // A word wider than the line breaks at grapheme boundaries.
        engine.shape(&lib, "abcdefghij", style, &mut shaped);
        shaped.layout(Some(20.0), &mut layout);
        assert_eq!(layout.lines.len(), 4);
        // Hard breaks.
        engine.shape(&lib, "a\nb", style, &mut shaped);
        shaped.layout(None, &mut layout);
        assert_eq!(layout.lines.len(), 2);
        assert_eq!(layout.glyphs.len(), 2, "newline draws nothing");
        Ok(())
    }

    #[test]
    fn caret_and_hit_test_round_trip() -> Result<(), Box<dyn std::error::Error>> {
        let (lib, _, stack) = library()?;
        let mut engine = Shaper::new();
        let mut shaped = ShapedText::default();
        engine.shape(&lib, "abc", TextStyle { stack, size: 10.0 }, &mut shaped);
        let mut layout = TextLayout::default();
        shaped.layout(None, &mut layout);
        assert_eq!(layout.caret_x(0), (0, 0.0));
        assert_eq!(layout.caret_x(1), (0, 6.0));
        assert_eq!(layout.caret_x(3), (0, 18.0));
        assert_eq!(layout.hit_test(7.0, 1.0), 1);
        assert_eq!(layout.hit_test(10.0, 1.0), 2);
        assert_eq!(layout.hit_test(100.0, 1.0), 3);
        Ok(())
    }

    #[test]
    fn empty_text_has_one_line() -> Result<(), Box<dyn std::error::Error>> {
        let (lib, _, stack) = library()?;
        let mut engine = Shaper::new();
        let mut shaped = ShapedText::default();
        engine.shape(&lib, "", TextStyle { stack, size: 10.0 }, &mut shaped);
        let mut layout = TextLayout::default();
        shaped.layout(Some(10.0), &mut layout);
        assert_eq!(layout.lines.len(), 1);
        assert!(layout.size[1] > 0.0);
        Ok(())
    }
}
