//! Font library: owned font bytes, coverage, metrics, and fallback stacks.
//!
//! A [`FontLibrary`] owns every font's bytes (`Arc<[u8]>`) and hands out
//! [`FontId`]s. At load time it reads the font's metrics and builds a sorted
//! coverage table from the Unicode `cmap` subtables, so a per-character
//! coverage query is a binary search and never re-parses the font.
//!
//! A [`FontStack`] is an ordered fallback list. Stacks are named; styles refer
//! to them by name (`font = "ui"`). The stack named `default` always exists:
//! until it is defined explicitly it lists every loaded font in load order.

use std::fmt;
use std::sync::Arc;

use rustybuzz::ttf_parser;

/// Index of a font in a [`FontLibrary`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FontId(pub u16);

/// Index of a font stack in a [`FontLibrary`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StackId(pub u16);

/// Vertical metrics and design grid of a font, in font units.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FontMetrics {
    /// Units per em.
    pub units_per_em: u16,
    /// Distance from the baseline to the top of the line box (positive).
    pub ascender: i16,
    /// Distance from the baseline to the bottom of the line box (negative).
    pub descender: i16,
    /// Extra space between lines.
    pub line_gap: i16,
}

impl FontMetrics {
    /// Pixels per font unit at `size_px` per em.
    #[must_use]
    pub fn scale(&self, size_px: f32) -> f32 {
        size_px / f32::from(self.units_per_em.max(1))
    }
}

/// An ordered fallback list of fonts.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct FontStack {
    fonts: Vec<FontId>,
}

impl FontStack {
    /// The fonts in fallback order.
    #[must_use]
    pub fn fonts(&self) -> &[FontId] {
        &self.fonts
    }

    /// The first font of the stack.
    #[must_use]
    pub fn primary(&self) -> Option<FontId> {
        self.fonts.first().copied()
    }
}

/// A font failed to load or a stack is invalid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FontError {
    /// The bytes are not a parsable font.
    Parse(String),
    /// The face cannot be used by the shaper.
    Shaper,
    /// Too many fonts or stacks.
    Full,
    /// A stack referenced a font id that does not exist.
    UnknownFont(FontId),
    /// A stack was defined with no fonts.
    EmptyStack,
}

impl fmt::Display for FontError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(e) => write!(f, "font does not parse: {e}"),
            Self::Shaper => f.write_str("font is not usable by the shaper"),
            Self::Full => f.write_str("too many fonts or stacks"),
            Self::UnknownFont(id) => write!(f, "unknown font id {}", id.0),
            Self::EmptyStack => f.write_str("a font stack needs at least one font"),
        }
    }
}

impl std::error::Error for FontError {}

struct FontEntry {
    data: Arc<[u8]>,
    metrics: FontMetrics,
    /// Sorted, disjoint, inclusive codepoint ranges with a real glyph.
    coverage: Vec<(u32, u32)>,
}

/// Owns font bytes, coverage tables, and named fallback stacks.
pub struct FontLibrary {
    fonts: Vec<FontEntry>,
    stacks: Vec<(String, FontStack)>,
    default_explicit: bool,
}

impl Default for FontLibrary {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for FontLibrary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FontLibrary")
            .field("fonts", &self.fonts.len())
            .field("stacks", &self.stacks.iter().map(|s| &s.0).collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// Name of the stack that always exists.
pub const DEFAULT_STACK: &str = "default";

impl FontLibrary {
    /// An empty library with an empty `default` stack.
    #[must_use]
    pub fn new() -> Self {
        Self {
            fonts: Vec::new(),
            stacks: vec![(DEFAULT_STACK.to_owned(), FontStack::default())],
            default_explicit: false,
        }
    }

    /// Loads a font (face index 0) from owned bytes.
    ///
    /// # Errors
    /// [`FontError::Parse`] when the bytes are not a font, [`FontError::Shaper`]
    /// when the shaper rejects the face, [`FontError::Full`] past 65535 fonts.
    pub fn add_font(&mut self, data: impl Into<Arc<[u8]>>) -> Result<FontId, FontError> {
        let data: Arc<[u8]> = data.into();
        let face = ttf_parser::Face::parse(&data, 0).map_err(|e| FontError::Parse(e.to_string()))?;
        if rustybuzz::Face::from_slice(&data, 0).is_none() {
            return Err(FontError::Shaper);
        }
        let metrics = FontMetrics {
            units_per_em: face.units_per_em(),
            ascender: face.ascender(),
            descender: face.descender(),
            line_gap: face.line_gap(),
        };
        let coverage = coverage_of(&face);
        let id = FontId(u16::try_from(self.fonts.len()).map_err(|_| FontError::Full)?);
        self.fonts.push(FontEntry {
            data,
            metrics,
            coverage,
        });
        if !self.default_explicit
            && let Some((_, stack)) = self.stacks.first_mut()
        {
            stack.fonts.push(id);
        }
        Ok(id)
    }

    /// Defines (or redefines) the stack `name` as `fonts` in fallback order.
    ///
    /// # Errors
    /// [`FontError::EmptyStack`], [`FontError::UnknownFont`], or
    /// [`FontError::Full`] past 65535 stacks.
    pub fn define_stack(&mut self, name: &str, fonts: &[FontId]) -> Result<StackId, FontError> {
        if fonts.is_empty() {
            return Err(FontError::EmptyStack);
        }
        if let Some(bad) = fonts.iter().find(|f| usize::from(f.0) >= self.fonts.len()) {
            return Err(FontError::UnknownFont(*bad));
        }
        let stack = FontStack {
            fonts: fonts.to_vec(),
        };
        if name == DEFAULT_STACK {
            self.default_explicit = true;
        }
        if let Some(i) = self.stacks.iter().position(|(n, _)| n == name) {
            if let Some(slot) = self.stacks.get_mut(i) {
                slot.1 = stack;
            }
            return StackId::try_from_index(i);
        }
        let i = self.stacks.len();
        self.stacks.push((name.to_owned(), stack));
        StackId::try_from_index(i)
    }

    /// Looks a stack up by name.
    #[must_use]
    pub fn stack_id(&self, name: &str) -> Option<StackId> {
        self.stacks
            .iter()
            .position(|(n, _)| n == name)
            .and_then(|i| StackId::try_from_index(i).ok())
    }

    /// The `default` stack.
    #[must_use]
    pub fn default_stack(&self) -> StackId {
        StackId(0)
    }

    /// A stack by id.
    #[must_use]
    pub fn stack(&self, id: StackId) -> Option<&FontStack> {
        self.stacks.get(usize::from(id.0)).map(|(_, s)| s)
    }

    /// Number of loaded fonts.
    #[must_use]
    pub fn len(&self) -> usize {
        self.fonts.len()
    }

    /// True when no font is loaded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fonts.is_empty()
    }

    /// The font's bytes.
    #[must_use]
    pub fn data(&self, id: FontId) -> Option<&[u8]> {
        self.fonts.get(usize::from(id.0)).map(|e| &*e.data)
    }

    /// The font's metrics.
    #[must_use]
    pub fn metrics(&self, id: FontId) -> Option<FontMetrics> {
        self.fonts.get(usize::from(id.0)).map(|e| e.metrics)
    }

    /// A parsed view of the font (cheap: parses the table directory only).
    #[must_use]
    pub fn face(&self, id: FontId) -> Option<ttf_parser::Face<'_>> {
        ttf_parser::Face::parse(self.data(id)?, 0).ok()
    }

    /// True when the font maps `c` to a real (non-`.notdef`) glyph.
    #[must_use]
    pub fn covers(&self, id: FontId, c: char) -> bool {
        let Some(entry) = self.fonts.get(usize::from(id.0)) else {
            return false;
        };
        let cp = u32::from(c);
        let i = entry.coverage.partition_point(|&(_, end)| end < cp);
        entry.coverage.get(i).is_some_and(|&(start, _)| start <= cp)
    }

    /// True when the font covers every character of `cluster` that needs a
    /// glyph (controls and default-ignorable joiners and selectors are
    /// skipped).
    #[must_use]
    pub fn covers_cluster(&self, id: FontId, cluster: &str) -> bool {
        cluster
            .chars()
            .filter(|c| !is_ignorable(*c))
            .all(|c| self.covers(id, c))
    }

    /// Picks the font for a grapheme cluster: the first font of the stack that
    /// covers the whole cluster, else the first that covers its first
    /// character, else the stack's primary font (which renders `.notdef`).
    #[must_use]
    pub fn select(&self, stack: StackId, cluster: &str) -> Option<FontId> {
        let fonts = self.stack(stack)?.fonts();
        if let Some(f) = fonts.iter().find(|f| self.covers_cluster(**f, cluster)) {
            return Some(*f);
        }
        if let Some(first) = cluster.chars().find(|c| !is_ignorable(*c))
            && let Some(f) = fonts.iter().find(|f| self.covers(**f, first))
        {
            return Some(*f);
        }
        fonts.first().copied()
    }
}

impl StackId {
    fn try_from_index(i: usize) -> Result<Self, FontError> {
        u16::try_from(i).map(StackId).map_err(|_| FontError::Full)
    }
}

/// Characters that need no glyph of their own for coverage purposes.
#[must_use]
pub fn is_ignorable(c: char) -> bool {
    c.is_control()
        || matches!(c,
            '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{FE00}'..='\u{FE0F}'
            | '\u{FEFF}'
            | '\u{E0100}'..='\u{E01EF}')
}

fn coverage_of(face: &ttf_parser::Face<'_>) -> Vec<(u32, u32)> {
    let mut cps: Vec<u32> = Vec::new();
    if let Some(cmap) = face.tables().cmap {
        for sub in cmap.subtables {
            if !sub.is_unicode() {
                continue;
            }
            sub.codepoints(|cp| {
                if sub.glyph_index(cp).is_some_and(|g| g.0 != 0) {
                    cps.push(cp);
                }
            });
        }
    }
    cps.sort_unstable();
    cps.dedup();
    let mut ranges: Vec<(u32, u32)> = Vec::new();
    for cp in cps {
        match ranges.last_mut() {
            Some(last) if last.1.checked_add(1) == Some(cp) => last.1 = cp,
            _ => ranges.push((cp, cp)),
        }
    }
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_font;

    #[test]
    fn coverage_and_fallback() -> Result<(), Box<dyn std::error::Error>> {
        let mut lib = FontLibrary::new();
        let latin = lib.add_font(test_font::latin())?;
        let cjk = lib.add_font(test_font::cjk())?;
        let rtl = lib.add_font(test_font::rtl())?;
        assert!(lib.covers(latin, 'a'));
        assert!(!lib.covers(latin, '한'));
        assert!(lib.covers(cjk, '한'));
        assert!(lib.covers(rtl, 'א'));
        assert!(!lib.covers(rtl, 'a'));
        let ui = lib.define_stack("ui", &[latin, cjk, rtl])?;
        assert_eq!(lib.select(ui, "a"), Some(latin));
        assert_eq!(lib.select(ui, "中"), Some(cjk));
        assert_eq!(lib.select(ui, "ש"), Some(rtl));
        assert_eq!(
            lib.select(ui, "\u{0416}"),
            Some(latin),
            "uncovered falls back to primary"
        );
        // The implicit default stack lists every font in load order.
        let def = lib.stack(lib.default_stack()).ok_or("default stack")?;
        assert_eq!(def.fonts(), &[latin, cjk, rtl]);
        let m = lib.metrics(cjk).ok_or("metrics")?;
        assert_eq!(
            (m.units_per_em, m.ascender, m.descender, m.line_gap),
            (1000, 880, -120, 0)
        );
        assert!(lib.define_stack("bad", &[]).is_err());
        assert!(lib.define_stack("bad", &[FontId(9)]).is_err());
        assert!(lib.add_font(vec![1_u8, 2, 3]).is_err());
        Ok(())
    }
}
