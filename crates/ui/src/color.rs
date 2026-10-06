//! Colors: linear-light, premultiplied RGBA.
//!
//! Layout files and themes write colors as sRGB hex (`#rgb`, `#rrggbb`,
//! `#rrggbbaa`). Parsing converts each color channel from sRGB to linear light
//! with the exact piecewise sRGB EOTF, keeps alpha linear as written, and
//! premultiplies. Everything downstream (draw list, reference rasterizer,
//! renderer) works in that space.

/// A linear-light, premultiplied RGBA color with `f32` channels in `0..=1`.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Color {
    /// Red, linear, premultiplied by alpha.
    pub r: f32,
    /// Green, linear, premultiplied by alpha.
    pub g: f32,
    /// Blue, linear, premultiplied by alpha.
    pub b: f32,
    /// Alpha (coverage), linear.
    pub a: f32,
}

/// The exact piecewise sRGB electro-optical transfer function: sRGB-encoded
/// `0..=1` to linear light.
#[must_use]
pub fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

impl Color {
    /// Fully transparent.
    pub const TRANSPARENT: Self = Self {
        r: 0.0,
        g: 0.0,
        b: 0.0,
        a: 0.0,
    };
    /// Opaque white.
    pub const WHITE: Self = Self {
        r: 1.0,
        g: 1.0,
        b: 1.0,
        a: 1.0,
    };
    /// Opaque black.
    pub const BLACK: Self = Self {
        r: 0.0,
        g: 0.0,
        b: 0.0,
        a: 1.0,
    };

    /// Builds a color from already linear, already premultiplied channels.
    #[must_use]
    pub const fn from_linear_premultiplied(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }

    /// Builds a color from 8-bit sRGB channels and an 8-bit linear alpha.
    #[must_use]
    pub fn from_srgba8(r: u8, g: u8, b: u8, a: u8) -> Self {
        let alpha = f32::from(a) / 255.0;
        let lin = |c: u8| srgb_to_linear(f32::from(c) / 255.0) * alpha;
        Self {
            r: lin(r),
            g: lin(g),
            b: lin(b),
            a: alpha,
        }
    }

    /// Parses `#rgb`, `#rrggbb`, or `#rrggbbaa` (sRGB, straight alpha).
    /// Returns `None` for anything else.
    #[must_use]
    pub fn from_hex(text: &str) -> Option<Self> {
        let hex = text.strip_prefix('#')?;
        let mut nibbles = [0_u8; 8];
        let mut count = 0_usize;
        for c in hex.chars() {
            let v = u8::try_from(c.to_digit(16)?).ok()?;
            *nibbles.get_mut(count)? = v;
            count += 1;
        }
        let byte = |i: usize| -> u8 {
            let hi = nibbles.get(i).copied().unwrap_or(0);
            let lo = nibbles.get(i + 1).copied().unwrap_or(0);
            (hi << 4) | lo
        };
        let short = |i: usize| -> u8 {
            let v = nibbles.get(i).copied().unwrap_or(0);
            (v << 4) | v
        };
        match count {
            3 => Some(Self::from_srgba8(short(0), short(1), short(2), 255)),
            6 => Some(Self::from_srgba8(byte(0), byte(2), byte(4), 255)),
            8 => Some(Self::from_srgba8(byte(0), byte(2), byte(4), byte(6))),
            _ => None,
        }
    }

    /// The channels as `[r, g, b, a]`.
    #[must_use]
    pub const fn to_array(self) -> [f32; 4] {
        [self.r, self.g, self.b, self.a]
    }

    /// True when the color contributes nothing when blended.
    #[must_use]
    pub fn is_transparent(self) -> bool {
        self.a <= 0.0 && self.r <= 0.0 && self.g <= 0.0 && self.b <= 0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_forms_parse() -> Result<(), Box<dyn std::error::Error>> {
        let white = Color::from_hex("#fff").ok_or("short form")?;
        assert_eq!(white, Color::WHITE);
        let black = Color::from_hex("#000000").ok_or("long form")?;
        assert_eq!(black, Color::BLACK);
        let half = Color::from_hex("#ffffff80").ok_or("alpha form")?;
        assert!((half.a - 128.0 / 255.0).abs() < 1e-6);
        assert!((half.r - half.a).abs() < 1e-6, "premultiplied white");
        assert!(Color::from_hex("#12345").is_none());
        assert!(Color::from_hex("#ggg").is_none());
        assert!(Color::from_hex("fff").is_none());
        assert!(Color::from_hex("#123456789").is_none());
        Ok(())
    }

    #[test]
    fn srgb_eotf_is_exact_piecewise() {
        // Mid grey 0x80 is about 0.2158 in linear light.
        let mid = srgb_to_linear(128.0 / 255.0);
        assert!((mid - 0.215_861).abs() < 1e-5);
        // Linear segment.
        assert!((srgb_to_linear(0.04) - 0.04 / 12.92).abs() < 1e-9);
        assert!((srgb_to_linear(1.0) - 1.0).abs() < 1e-6);
        assert_eq!(srgb_to_linear(0.0), 0.0);
    }
}
