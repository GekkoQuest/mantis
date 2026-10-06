//! Glyph atlas: on-demand MSDF glyphs packed on shelves into one RGBA8 image.
//!
//! Glyphs are rasterized the first time they are needed, at a fixed em size
//! ([`AtlasConfig::em_px`], 32 px by default) with a padding of at least the
//! distance range, and keyed by `(FontId, glyph id)`. Placement uses a shelf
//! packer with a one-texel gap between cells. When nothing fits, the atlas
//! doubles in both dimensions (copying its pixels, so placed glyphs keep their
//! texel rectangles) up to [`AtlasConfig::max_size`]; past that it refuses the
//! glyph, counts it in [`AtlasStats::refused`], and never panics.
//!
//! The renderer uploads [`GlyphAtlas::pixels`] (RGBA8, row-major,
//! `width * 4` bytes per row) and then only the rectangle returned by
//! [`GlyphAtlas::take_dirty`]. When [`GlyphAtlas::generation`] changes the
//! atlas has grown: the renderer recreates the texture at the new size and
//! uploads everything (the dirty rectangle then covers the whole atlas).

use std::collections::HashMap;

use crate::font::{FontId, FontLibrary};
use crate::msdf;

/// A rectangle of texels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct AtlasRect {
    /// Left texel.
    pub x: u32,
    /// Top texel.
    pub y: u32,
    /// Width in texels.
    pub w: u32,
    /// Height in texels.
    pub h: u32,
}

impl AtlasRect {
    /// The smallest rectangle containing both.
    #[must_use]
    pub fn union(self, o: Self) -> Self {
        let x0 = self.x.min(o.x);
        let y0 = self.y.min(o.y);
        let x1 = (self.x + self.w).max(o.x + o.w);
        let y1 = (self.y + self.h).max(o.y + o.h);
        Self {
            x: x0,
            y: y0,
            w: x1 - x0,
            h: y1 - y0,
        }
    }
}

/// Atlas parameters.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AtlasConfig {
    /// Em size glyphs are rasterized at, px.
    pub em_px: f32,
    /// MSDF distance range, texels.
    pub range: f32,
    /// Initial width and height, texels.
    pub initial_size: u32,
    /// Largest width and height the atlas grows to, texels.
    pub max_size: u32,
}

impl Default for AtlasConfig {
    fn default() -> Self {
        Self {
            em_px: 32.0,
            range: 4.0,
            initial_size: 256,
            max_size: 4096,
        }
    }
}

/// Where a glyph lives in the atlas and how its cell sits on the baseline.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AtlasGlyph {
    /// Texel rectangle of the cell (padding included).
    pub rect: AtlasRect,
    /// Left edge of the cell relative to the pen, in pixels at the atlas em
    /// size.
    pub left: f32,
    /// Top edge of the cell above the baseline, in pixels at the atlas em
    /// size.
    pub top: f32,
}

/// Counters.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct AtlasStats {
    /// Glyphs placed.
    pub glyphs: u32,
    /// Glyphs without an outline (nothing to place).
    pub empty: u32,
    /// Glyphs refused because the atlas is at its maximum size and full.
    pub refused: u32,
    /// Times the atlas grew.
    pub grows: u32,
}

#[derive(Clone, Copy, Debug)]
enum Slot {
    Placed(AtlasGlyph),
    Empty,
    Refused,
}

#[derive(Clone, Copy, Debug)]
struct Shelf {
    y: u32,
    h: u32,
    x: u32,
}

/// The MSDF glyph atlas. See the module documentation.
#[derive(Debug)]
pub struct GlyphAtlas {
    config: AtlasConfig,
    padding: u32,
    size: [u32; 2],
    pixels: Vec<u8>,
    shelves: Vec<Shelf>,
    cache: HashMap<(FontId, u16), Slot>,
    dirty: Option<AtlasRect>,
    generation: u64,
    stats: AtlasStats,
}

impl Default for GlyphAtlas {
    fn default() -> Self {
        Self::new(AtlasConfig::default())
    }
}

/// One texel gap between cells.
const GAP: u32 = 1;

impl GlyphAtlas {
    /// An empty atlas. Sizes are clamped to `1..=16384` and the initial size
    /// to the maximum.
    #[must_use]
    #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // range is small and positive
    pub fn new(config: AtlasConfig) -> Self {
        let max = config.max_size.clamp(1, 16384);
        let initial = config.initial_size.clamp(1, max);
        let config = AtlasConfig {
            em_px: if config.em_px > 0.0 { config.em_px } else { 32.0 },
            range: if config.range > 0.0 { config.range } else { 4.0 },
            initial_size: initial,
            max_size: max,
        };
        let padding = config.range.ceil() as u32 + 1;
        Self {
            config,
            padding,
            size: [initial, initial],
            pixels: vec![0; texel_bytes(initial, initial)],
            shelves: Vec::new(),
            cache: HashMap::new(),
            dirty: Some(AtlasRect {
                x: 0,
                y: 0,
                w: initial,
                h: initial,
            }),
            generation: 0,
            stats: AtlasStats::default(),
        }
    }

    /// The configuration in effect.
    #[must_use]
    pub fn config(&self) -> AtlasConfig {
        self.config
    }

    /// Width and height in texels.
    #[must_use]
    pub fn size(&self) -> [u32; 2] {
        self.size
    }

    /// RGBA8 pixels, row-major, `width * 4` bytes per row.
    #[must_use]
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// The rectangle changed since the last call, if any.
    pub fn take_dirty(&mut self) -> Option<AtlasRect> {
        self.dirty.take()
    }

    /// Incremented every time the atlas grows (the texture must be recreated).
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// MSDF distance range in atlas texels.
    #[must_use]
    pub fn msdf_range(&self) -> f32 {
        self.config.range
    }

    /// Em size glyphs are rasterized at, px.
    #[must_use]
    pub fn em_px(&self) -> f32 {
        self.config.em_px
    }

    /// Padding around each glyph, texels (at least the range).
    #[must_use]
    pub fn padding(&self) -> u32 {
        self.padding
    }

    /// Counters.
    #[must_use]
    pub fn stats(&self) -> AtlasStats {
        self.stats
    }

    /// A placed glyph, without rasterizing. Never allocates.
    #[must_use]
    pub fn get(&self, font: FontId, glyph: u16) -> Option<AtlasGlyph> {
        match self.cache.get(&(font, glyph)) {
            Some(Slot::Placed(g)) => Some(*g),
            _ => None,
        }
    }

    /// True when the glyph has been looked at before (placed, empty, or
    /// refused).
    #[must_use]
    pub fn contains(&self, font: FontId, glyph: u16) -> bool {
        self.cache.contains_key(&(font, glyph))
    }

    /// Drops every glyph and resets the atlas to its initial size (the
    /// generation advances).
    pub fn clear(&mut self) {
        let s = self.config.initial_size;
        self.size = [s, s];
        self.pixels.clear();
        self.pixels.resize(texel_bytes(s, s), 0);
        self.shelves.clear();
        self.cache.clear();
        self.generation += 1;
        self.dirty = Some(AtlasRect {
            x: 0,
            y: 0,
            w: s,
            h: s,
        });
        self.stats = AtlasStats::default();
    }

    /// Returns the glyph's placement, rasterizing and packing it on first use.
    /// `None` for glyphs without an outline and for refused glyphs.
    pub fn ensure(&mut self, fonts: &FontLibrary, font: FontId, glyph: u16) -> Option<AtlasGlyph> {
        if let Some(slot) = self.cache.get(&(font, glyph)) {
            return match slot {
                Slot::Placed(g) => Some(*g),
                Slot::Empty | Slot::Refused => None,
            };
        }
        let bitmap = fonts.face(font).and_then(|face| {
            msdf::generate_glyph(&face, glyph, self.config.em_px, self.config.range, self.padding)
        });
        let Some(bitmap) = bitmap else {
            self.cache.insert((font, glyph), Slot::Empty);
            self.stats.empty += 1;
            return None;
        };
        let Some((x, y)) = self.allocate(bitmap.width, bitmap.height) else {
            self.cache.insert((font, glyph), Slot::Refused);
            self.stats.refused += 1;
            return None;
        };
        let rect = AtlasRect {
            x,
            y,
            w: bitmap.width,
            h: bitmap.height,
        };
        self.blit(rect, &bitmap.pixels);
        let placed = AtlasGlyph {
            rect,
            left: bitmap.left,
            top: bitmap.top,
        };
        self.cache.insert((font, glyph), Slot::Placed(placed));
        self.stats.glyphs += 1;
        Some(placed)
    }

    fn blit(&mut self, rect: AtlasRect, src: &[u8]) {
        let row_bytes = rect.w as usize * 4;
        let stride = self.size[0] as usize * 4;
        for (r, src_row) in src.chunks_exact(row_bytes).enumerate() {
            let start = (rect.y as usize + r) * stride + rect.x as usize * 4;
            if let Some(dst) = self.pixels.get_mut(start..start + row_bytes) {
                dst.copy_from_slice(src_row);
            }
        }
        self.dirty = Some(self.dirty.map_or(rect, |d| d.union(rect)));
    }

    /// Finds room for a `w` x `h` cell, growing when needed.
    fn allocate(&mut self, w: u32, h: u32) -> Option<(u32, u32)> {
        let (cw, ch) = (w + GAP, h + GAP);
        if cw > self.config.max_size || ch > self.config.max_size {
            return None;
        }
        loop {
            if let Some(pos) = self.try_place(cw, ch) {
                return Some(pos);
            }
            if !self.grow() {
                return None;
            }
        }
    }

    fn try_place(&mut self, cw: u32, ch: u32) -> Option<(u32, u32)> {
        let width = self.size[0];
        let best = self
            .shelves
            .iter_mut()
            .filter(|s| s.h >= ch && s.x + cw <= width && s.h <= ch + ch / 2 + 2)
            .min_by_key(|s| s.h - ch);
        if let Some(shelf) = best {
            let pos = (shelf.x, shelf.y);
            shelf.x += cw;
            return Some(pos);
        }
        let y = self.shelves.last().map_or(0, |s| s.y + s.h);
        if y + ch <= self.size[1] && cw <= width {
            self.shelves.push(Shelf { y, h: ch, x: cw });
            return Some((0, y));
        }
        None
    }

    /// Doubles both dimensions, keeping texel positions. False at max size.
    fn grow(&mut self) -> bool {
        let [w, h] = self.size;
        if w >= self.config.max_size && h >= self.config.max_size {
            return false;
        }
        let nw = (w * 2).min(self.config.max_size);
        let nh = (h * 2).min(self.config.max_size);
        let mut next = vec![0_u8; texel_bytes(nw, nh)];
        let old_stride = w as usize * 4;
        let new_stride = nw as usize * 4;
        for (r, row) in self.pixels.chunks_exact(old_stride).enumerate() {
            let start = r * new_stride;
            if let Some(dst) = next.get_mut(start..start + old_stride) {
                dst.copy_from_slice(row);
            }
        }
        self.pixels = next;
        self.size = [nw, nh];
        self.generation += 1;
        self.stats.grows += 1;
        self.dirty = Some(AtlasRect {
            x: 0,
            y: 0,
            w: nw,
            h: nh,
        });
        true
    }
}

fn texel_bytes(w: u32, h: u32) -> usize {
    w as usize * h as usize * 4
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_font::{self, GlyphShape, TestFontBuilder};

    #[test]
    fn places_glyphs_and_tracks_dirty() -> Result<(), Box<dyn std::error::Error>> {
        let mut fonts = FontLibrary::new();
        let latin = fonts.add_font(test_font::latin())?;
        let face = fonts.face(latin).ok_or("face")?;
        let a = face.glyph_index('a').ok_or("a")?.0;
        let space = face.glyph_index(' ').ok_or("space")?.0;
        let mut atlas = GlyphAtlas::default();
        assert!(atlas.take_dirty().is_some(), "a new atlas starts dirty");
        assert!(atlas.take_dirty().is_none());
        let g = atlas.ensure(&fonts, latin, a).ok_or("placed")?;
        assert!(g.rect.w > 2 * atlas.padding());
        let dirty = atlas.take_dirty().ok_or("dirty")?;
        assert_eq!(dirty, g.rect);
        // Second request is a cache hit: no new dirt.
        assert_eq!(atlas.ensure(&fonts, latin, a), Some(g));
        assert!(atlas.take_dirty().is_none());
        assert!(atlas.ensure(&fonts, latin, space).is_none());
        assert_eq!(atlas.stats().empty, 1);
        assert_eq!(atlas.get(latin, a), Some(g));
        Ok(())
    }

    #[test]
    fn grows_then_refuses_without_panicking() -> Result<(), Box<dyn std::error::Error>> {
        let chars: Vec<char> = ('a'..='z').chain('A'..='Z').collect();
        let bytes = TestFontBuilder::new()
            .glyphs(chars.iter().copied(), GlyphShape::Rect)
            .build();
        let mut fonts = FontLibrary::new();
        let f = fonts.add_font(bytes)?;
        let face = fonts.face(f).ok_or("face")?;
        let gids: Vec<u16> = chars
            .iter()
            .filter_map(|c| face.glyph_index(*c).map(|g| g.0))
            .collect();
        let mut atlas = GlyphAtlas::new(AtlasConfig {
            initial_size: 32,
            max_size: 128,
            ..AtlasConfig::default()
        });
        let gen0 = atlas.generation();
        for g in &gids {
            let _ = atlas.ensure(&fonts, f, *g);
        }
        let stats = atlas.stats();
        assert_eq!(atlas.size(), [128, 128]);
        assert!(atlas.generation() > gen0);
        assert!(stats.grows >= 2);
        assert!(stats.refused > 0, "{stats:?}");
        assert_eq!(stats.glyphs + stats.refused, u32::try_from(gids.len())?);
        assert_eq!(atlas.pixels().len(), 128 * 128 * 4);
        // Refused glyphs stay refused and are counted once.
        for g in &gids {
            let _ = atlas.ensure(&fonts, f, *g);
        }
        assert_eq!(atlas.stats().refused, stats.refused);
        // Placed cells never overlap.
        let placed: Vec<AtlasRect> = gids
            .iter()
            .filter_map(|g| atlas.get(f, *g))
            .map(|g| g.rect)
            .collect();
        for (i, a) in placed.iter().enumerate() {
            for b in placed.iter().skip(i + 1) {
                let overlap = a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h;
                assert!(!overlap, "{a:?} overlaps {b:?}");
            }
            assert!(a.x + a.w <= 128 && a.y + a.h <= 128);
        }
        Ok(())
    }
}
