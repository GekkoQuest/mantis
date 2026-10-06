//! Texture v1: cooked, GPU-ready 2D textures with a full or partial mip chain, block
//! compressed (BC1, BC4, BC5, BC7) or uncompressed RGBA8.
//!
//! Every texture records the **encoder version** that produced it. A change to an encoder
//! changes its version, so re-cooked textures change their bytes and their content hash
//! visibly, never silently.
//!
//! # Layout (little-endian)
//!
//! | offset | size | field |
//! |---|---|---|
//! | 0 | 4 | magic `"MTEX"` |
//! | 4 | 2 | version `u16` = 1 |
//! | 6 | 2 | flags `u16`: bit 0 sRGB color, bit 1 normal map (BC5 stores X and Y); other bits 0 |
//! | 8 | 1 | encoding `u8`: 1 BC1, 2 BC4, 3 BC5, 4 BC7, 5 RGBA8 |
//! | 9 | 3 | reserved, 0 |
//! | 12 | 4 | encoder version `u32` (0 only for RGBA8, which is not encoded) |
//! | 16 | 4 | width `u32`, 1 to [`MAX_SIZE`] |
//! | 20 | 4 | height `u32`, 1 to [`MAX_SIZE`] |
//! | 24 | 4 | mip count `u32`, 1 to the full chain length |
//! | 28 | 4 | reserved, 0 |
//! | 32 | | mip data, largest first, each exactly its size for the encoding (blocks of 4x4 texels, partial blocks padded; RGBA8 4 bytes per texel) |
//!
//! The sRGB flag is allowed with BC1, BC7, and RGBA8 only; the normal-map flag with BC5
//! only. The length must match exactly.

use crate::bytes::{FormatError, Reader, Writer};

/// Magic bytes.
pub const MAGIC: [u8; 4] = *b"MTEX";
/// Largest width or height.
pub const MAX_SIZE: u32 = 16_384;

/// Flag: color data in sRGB.
pub const FLAG_SRGB: u16 = 1;
/// Flag: a tangent-space normal map.
pub const FLAG_NORMAL_MAP: u16 = 2;

/// How texels are stored.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Encoding {
    /// 8 bytes per 4x4 block: RGB with 1-bit alpha.
    Bc1,
    /// 8 bytes per block: one channel.
    Bc4,
    /// 16 bytes per block: two channels.
    Bc5,
    /// 16 bytes per block: RGBA.
    Bc7,
    /// 4 bytes per texel.
    Rgba8,
}

impl Encoding {
    fn code(self) -> u8 {
        match self {
            Encoding::Bc1 => 1,
            Encoding::Bc4 => 2,
            Encoding::Bc5 => 3,
            Encoding::Bc7 => 4,
            Encoding::Rgba8 => 5,
        }
    }

    fn from_code(c: u8) -> Option<Self> {
        Some(match c {
            1 => Encoding::Bc1,
            2 => Encoding::Bc4,
            3 => Encoding::Bc5,
            4 => Encoding::Bc7,
            5 => Encoding::Rgba8,
            _ => return None,
        })
    }

    /// Bytes of one `width x height` image.
    pub fn image_bytes(self, width: u32, height: u32) -> u64 {
        let blocks = u64::from(width.div_ceil(4)) * u64::from(height.div_ceil(4));
        match self {
            Encoding::Bc1 | Encoding::Bc4 => blocks * 8,
            Encoding::Bc5 | Encoding::Bc7 => blocks * 16,
            Encoding::Rgba8 => u64::from(width) * u64::from(height) * 4,
        }
    }
}

/// Mip levels of a full chain for `width x height`.
pub fn full_chain(width: u32, height: u32) -> u32 {
    32 - width.max(height).max(1).leading_zeros()
}

/// Size of mip `level`.
pub fn mip_size(width: u32, height: u32, level: u32) -> (u32, u32) {
    ((width >> level.min(31)).max(1), (height >> level.min(31)).max(1))
}

/// A texture.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TextureAsset {
    /// Storage.
    pub encoding: Encoding,
    /// [`FLAG_SRGB`] and [`FLAG_NORMAL_MAP`].
    pub flags: u16,
    /// The encoder version that produced the data (0 for RGBA8).
    pub encoder_version: u32,
    /// Width of mip 0.
    pub width: u32,
    /// Height of mip 0.
    pub height: u32,
    /// Mip data, largest first.
    pub mips: Vec<Vec<u8>>,
}

impl TextureAsset {
    /// Checks every rule of the format.
    ///
    /// # Errors
    /// [`FormatError::Dimensions`], [`FormatError::Flags`], [`FormatError::Inconsistent`]
    /// (a mip of the wrong size, or an encoder version on RGBA8 or missing on BC).
    pub fn validate(&self) -> Result<(), FormatError> {
        if !(1..=MAX_SIZE).contains(&self.width) || !(1..=MAX_SIZE).contains(&self.height) {
            return Err(FormatError::Dimensions);
        }
        let count = u32::try_from(self.mips.len()).map_err(|_| FormatError::Dimensions)?;
        if count == 0 || count > full_chain(self.width, self.height) {
            return Err(FormatError::Dimensions);
        }
        if self.flags & !(FLAG_SRGB | FLAG_NORMAL_MAP) != 0 {
            return Err(FormatError::Flags(u32::from(self.flags)));
        }
        let srgb_ok = matches!(self.encoding, Encoding::Bc1 | Encoding::Bc7 | Encoding::Rgba8);
        let normal_ok = self.encoding == Encoding::Bc5;
        if (self.flags & FLAG_SRGB != 0 && !srgb_ok) || (self.flags & FLAG_NORMAL_MAP != 0 && !normal_ok) {
            return Err(FormatError::Flags(u32::from(self.flags)));
        }
        if (self.encoding == Encoding::Rgba8) != (self.encoder_version == 0) {
            return Err(FormatError::Inconsistent);
        }
        for (level, mip) in (0u32..).zip(&self.mips) {
            let (w, h) = mip_size(self.width, self.height, level);
            if mip.len() as u64 != self.encoding.image_bytes(w, h) {
                return Err(FormatError::Inconsistent);
            }
        }
        Ok(())
    }

    /// Parses and validates.
    ///
    /// # Errors
    /// [`FormatError`] describing the first problem found.
    pub fn parse(bytes: &[u8]) -> Result<TextureAsset, FormatError> {
        let mut r = Reader::new(bytes);
        if r.array::<4>()? != MAGIC {
            return Err(FormatError::Magic);
        }
        let version = r.u16()?;
        if version != 1 {
            return Err(FormatError::Version(version));
        }
        let flags = r.u16()?;
        let code = r.u8()?;
        let encoding = Encoding::from_code(code).ok_or(FormatError::Encoding(u32::from(code)))?;
        if r.array::<3>()? != [0; 3] {
            return Err(FormatError::Reserved);
        }
        let encoder_version = r.u32()?;
        let (width, height, count) = (r.u32()?, r.u32()?, r.u32()?);
        if r.u32()? != 0 {
            return Err(FormatError::Reserved);
        }
        if !(1..=MAX_SIZE).contains(&width)
            || !(1..=MAX_SIZE).contains(&height)
            || count == 0
            || count > full_chain(width, height)
        {
            return Err(FormatError::Dimensions);
        }
        let mut mips = Vec::with_capacity(count as usize);
        for level in 0..count {
            let (w, h) = mip_size(width, height, level);
            let n = usize::try_from(encoding.image_bytes(w, h)).map_err(|_| FormatError::Dimensions)?;
            mips.push(r.slice(n)?.to_vec());
        }
        r.finish()?;
        let t = TextureAsset {
            encoding,
            flags,
            encoder_version,
            width,
            height,
            mips,
        };
        t.validate()?;
        Ok(t)
    }

    /// Reference encoder.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.bytes(&MAGIC);
        w.u16(1);
        w.u16(self.flags);
        w.u8(self.encoding.code());
        w.bytes(&[0; 3]);
        w.u32(self.encoder_version);
        w.u32(self.width);
        w.u32(self.height);
        w.count(self.mips.len());
        w.u32(0);
        for m in &self.mips {
            w.bytes(m);
        }
        w.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn sample() -> TextureAsset {
        // 6x5 BC1: mips 6x5 (2x2 blocks), 3x2 (1 block), 1x1 (1 block).
        TextureAsset {
            encoding: Encoding::Bc1,
            flags: FLAG_SRGB,
            encoder_version: 3,
            width: 6,
            height: 5,
            mips: vec![vec![1; 32], vec![2; 8], vec![3; 8]],
        }
    }

    #[test]
    fn sizes_round_trip_and_chain_rules() -> TestResult {
        assert_eq!(full_chain(6, 5), 3);
        assert_eq!(full_chain(1, 1), 1);
        assert_eq!(full_chain(16_384, 1), 15);
        assert_eq!(Encoding::Bc7.image_bytes(5, 5), 64);
        assert_eq!(Encoding::Rgba8.image_bytes(3, 2), 24);
        let t = sample();
        let bytes = t.encode();
        assert_eq!(bytes.len(), 32 + 48);
        assert_eq!(TextureAsset::parse(&bytes)?, t);
        Ok(())
    }

    #[test]
    fn rules_reject() {
        let bad = |edit: fn(&mut TextureAsset)| {
            let mut t = sample();
            edit(&mut t);
            TextureAsset::parse(&t.encode()).err()
        };
        assert!(
            bad(|t| t.mips.push(vec![0; 8])).is_some(),
            "more mips than the chain"
        );
        assert_eq!(
            bad(|t| t.encoder_version = 0),
            Some(FormatError::Inconsistent),
            "BC data names its encoder"
        );
        assert_eq!(
            bad(|t| t.flags = FLAG_NORMAL_MAP),
            Some(FormatError::Flags(2)),
            "normal maps are BC5"
        );
        assert_eq!(
            bad(|t| {
                t.encoding = Encoding::Bc4;
                t.flags = FLAG_SRGB;
            }),
            Some(FormatError::Flags(1)),
            "single-channel data is linear"
        );
        let bytes = sample().encode();
        let corrupt = |at: usize, v: &[u8]| {
            let mut b = bytes.clone();
            if let Some(s) = b.get_mut(at..at + v.len()) {
                s.copy_from_slice(v);
            }
            TextureAsset::parse(&b).err()
        };
        assert_eq!(corrupt(0, b"MTEZ"), Some(FormatError::Magic));
        assert_eq!(corrupt(8, &[9]), Some(FormatError::Encoding(9)));
        assert_eq!(corrupt(9, &[1]), Some(FormatError::Reserved));
        assert_eq!(corrupt(16, &0u32.to_le_bytes()), Some(FormatError::Dimensions));
    }

    #[test]
    fn a_different_encoder_version_is_a_different_asset() {
        let mut t = sample();
        let before = mantis_core::content::ContentHash::of(&t.encode());
        t.encoder_version = 4;
        assert_ne!(mantis_core::content::ContentHash::of(&t.encode()), before);
    }

    #[test]
    fn no_corruption_or_truncation_panics() {
        let bytes = sample().encode();
        for cut in 0..bytes.len() {
            assert!(TextureAsset::parse(bytes.get(..cut).unwrap_or(&[])).is_err());
        }
        for i in 0..bytes.len() {
            let mut b = bytes.clone();
            if let Some(x) = b.get_mut(i) {
                *x ^= 0x81;
            }
            if let Ok(t) = TextureAsset::parse(&b) {
                assert_eq!(TextureAsset::parse(&t.encode()).ok(), Some(t));
            }
        }
    }
}
