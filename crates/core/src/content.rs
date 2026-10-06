//! Content addressing: [`ContentHash`] and the one hash function behind it.
//!
//! Every content hash in Mantis (gameplay and presentation bundles, world
//! sectors, assets, the log header) is BLAKE3-256, computed through
//! [`ContentHasher`]. No other crate names the underlying implementation, so
//! the function can be swapped in exactly one place.

use core::fmt;
use core::str::FromStr;

/// A 256-bit content hash.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct ContentHash([u8; 32]);

impl ContentHash {
    /// The all-zero hash, a placeholder that never matches real content.
    pub const ZERO: Self = Self([0; 32]);

    /// A hash from its bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The hash bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// The hash of `bytes`.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        let mut h = ContentHasher::new();
        h.update(bytes);
        h.finalize()
    }
}

impl fmt::Display for ContentHash {
    /// Lowercase hex, 64 characters. Stable; used in logs and file names.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in &self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ContentHash({self})")
    }
}

/// A hex string that is not a content hash.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ParseContentHashError;

impl fmt::Display for ParseContentHashError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("expected 64 hex digits")
    }
}

impl std::error::Error for ParseContentHashError {}

impl FromStr for ContentHash {
    type Err = ParseContentHashError;

    /// Parses 64 hex digits (either case).
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let digits = s.as_bytes();
        if digits.len() != 64 {
            return Err(ParseContentHashError);
        }
        let nibble = |c: u8| -> Result<u8, ParseContentHashError> {
            match c {
                b'0'..=b'9' => Ok(c - b'0'),
                b'a'..=b'f' => Ok(c - b'a' + 10),
                b'A'..=b'F' => Ok(c - b'A' + 10),
                _ => Err(ParseContentHashError),
            }
        };
        let mut out = [0u8; 32];
        for (byte, pair) in out.iter_mut().zip(digits.as_chunks::<2>().0) {
            *byte = (nibble(pair[0])? << 4) | nibble(pair[1])?;
        }
        Ok(Self(out))
    }
}

/// Incremental content hasher (BLAKE3-256).
#[derive(Clone, Default)]
pub struct ContentHasher(blake3::Hasher);

impl fmt::Debug for ContentHasher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ContentHasher(..)")
    }
}

impl ContentHasher {
    /// A fresh hasher.
    #[must_use]
    pub fn new() -> Self {
        Self(blake3::Hasher::new())
    }

    /// Feeds bytes.
    pub fn update(&mut self, bytes: &[u8]) -> &mut Self {
        self.0.update(bytes);
        self
    }

    /// The hash of everything fed so far. The hasher can keep accepting input.
    #[must_use]
    pub fn finalize(&self) -> ContentHash {
        ContentHash(*self.0.finalize().as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Published BLAKE3 vectors.
    #[test]
    fn blake3_reference_vectors() {
        assert_eq!(
            ContentHash::of(b"").to_string(),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
        assert_eq!(
            ContentHash::of(b"abc").to_string(),
            "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85"
        );
    }

    #[test]
    fn incremental_matches_one_shot() {
        let data: Vec<u8> = (0..=255u8).cycle().take(5000).collect();
        let mut h = ContentHasher::new();
        for chunk in data.chunks(333) {
            h.update(chunk);
        }
        assert_eq!(h.finalize(), ContentHash::of(&data));
    }

    #[test]
    fn hex_round_trip_and_rejects() {
        let h = ContentHash::of(b"mantis");
        let s = h.to_string();
        assert_eq!(s.len(), 64);
        assert_eq!(s.parse::<ContentHash>(), Ok(h));
        assert_eq!(s.to_uppercase().parse::<ContentHash>(), Ok(h));
        assert_eq!("".parse::<ContentHash>(), Err(ParseContentHashError));
        assert_eq!("zz".repeat(32).parse::<ContentHash>(), Err(ParseContentHashError));
        assert_eq!(ContentHash::from_bytes(*h.as_bytes()), h);
        assert_eq!(
            format!("{:?}", ContentHash::ZERO),
            format!("ContentHash({})", "0".repeat(64))
        );
    }
}
