//! Inline, bounded collections for wire messages. Never allocate.

use core::fmt;

use super::{DecodeError, Decoder, Encoder, FuzzSample, Wire};
use crate::mem::CapacityError;
use crate::rng::Rng;

/// Up to `N` `Copy` values stored inline. Decoding refuses a length over `N`
/// before reading any element, so a hostile length costs nothing. Unused
/// slots hold `None`, so element types need no placeholder value (an
/// `EntityId` has no "null"). `N` must not exceed `u16::MAX` (checked at
/// compile time on use).
#[derive(Clone, Copy)]
pub struct BoundedArray<T: Copy, const N: usize> {
    len: u16,
    items: [Option<T>; N],
}

impl<T: Copy, const N: usize> BoundedArray<T, N> {
    const BOUND_FITS_U16: () = assert!(N <= u16::MAX as usize, "BoundedArray bound exceeds u16");

    /// An empty array.
    #[must_use]
    pub fn new() -> Self {
        let () = Self::BOUND_FITS_U16;
        Self {
            len: 0,
            items: [None; N],
        }
    }

    /// An array holding a copy of `values`, or `None` if they do not fit.
    #[must_use]
    pub fn from_slice(values: &[T]) -> Option<Self> {
        let mut out = Self::new();
        for v in values {
            out.push(*v).ok()?;
        }
        Some(out)
    }

    /// Appends `value`, or returns it if full.
    ///
    /// # Errors
    /// [`CapacityError`] carrying `value` when full.
    pub fn push(&mut self, value: T) -> Result<(), CapacityError<T>> {
        let Some(slot) = self.items.get_mut(usize::from(self.len)) else {
            return Err(CapacityError(value));
        };
        *slot = Some(value);
        self.len += 1;
        Ok(())
    }

    /// Removes every value.
    pub fn clear(&mut self) {
        self.items = [None; N];
        self.len = 0;
    }

    /// Number of values.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len as usize
    }

    /// True when empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The bound.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        N
    }

    /// The value at `index`.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&T> {
        if index < self.len() {
            self.items.get(index).and_then(Option::as_ref)
        } else {
            None
        }
    }

    /// The values in order.
    pub fn iter(&self) -> impl Iterator<Item = &T> + '_ {
        self.items.iter().take(self.len()).filter_map(Option::as_ref)
    }
}

impl<T: Copy, const N: usize> Default for BoundedArray<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Copy + PartialEq, const N: usize> PartialEq for BoundedArray<T, N> {
    fn eq(&self, other: &Self) -> bool {
        self.len == other.len && self.iter().eq(other.iter())
    }
}

impl<T: Copy + Eq, const N: usize> Eq for BoundedArray<T, N> {}

impl<T: Copy + fmt::Debug, const N: usize> fmt::Debug for BoundedArray<T, N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

/// A `u16` count, then the elements.
impl<T: Copy + Wire, const N: usize> Wire for BoundedArray<T, N> {
    fn encode(&self, e: &mut Encoder<'_>) {
        e.u16(self.len);
        for v in self.iter() {
            v.encode(e);
        }
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        let n = d.len(N)?;
        let mut out = Self::new();
        for _ in 0..n {
            out.push(T::decode(d)?)
                .map_err(|_| DecodeError::Invalid("length over bound"))?;
        }
        Ok(out)
    }
}

impl<T: Copy + FuzzSample, const N: usize> FuzzSample for BoundedArray<T, N> {
    fn fuzz_sample(rng: &mut Rng) -> Self {
        let cap = u32::try_from(N).unwrap_or(u32::MAX);
        // Mostly short, sometimes full.
        let n = if rng.chance(1, 8) {
            cap
        } else {
            rng.below(cap.min(8) + 1)
        };
        let mut out = Self::new();
        for _ in 0..n {
            if out.push(T::fuzz_sample(rng)).is_err() {
                break;
            }
        }
        out
    }
}

/// UTF-8 text of at most `N` bytes, stored inline.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct WireString<const N: usize> {
    len: u16,
    bytes: [u8; N],
}

impl<const N: usize> WireString<N> {
    const BOUND_FITS_U16: () = assert!(N <= u16::MAX as usize, "WireString bound exceeds u16");

    /// The empty string.
    #[must_use]
    pub const fn empty() -> Self {
        let () = Self::BOUND_FITS_U16;
        Self {
            len: 0,
            bytes: [0; N],
        }
    }

    /// The text, or `None` if it does not fit.
    #[must_use]
    pub fn new(text: &str) -> Option<Self> {
        let src = text.as_bytes();
        let mut out = Self::empty();
        out.bytes.get_mut(..src.len())?.copy_from_slice(src);
        out.len = u16::try_from(src.len()).ok()?;
        Some(out)
    }

    /// The text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        // Construction and decoding both guarantee UTF-8.
        core::str::from_utf8(self.bytes.get(..usize::from(self.len)).unwrap_or(&[])).unwrap_or("")
    }
}

impl<const N: usize> Default for WireString<N> {
    fn default() -> Self {
        Self::empty()
    }
}

impl<const N: usize> fmt::Debug for WireString<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

/// A `u16` byte length, then UTF-8 bytes (validated).
impl<const N: usize> Wire for WireString<N> {
    fn encode(&self, e: &mut Encoder<'_>) {
        e.u16(self.len);
        e.bytes(self.as_str().as_bytes());
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        let n = d.len(N)?;
        let bytes = d.take(n)?;
        let text = core::str::from_utf8(bytes).map_err(|_| DecodeError::Invalid("utf-8"))?;
        Self::new(text).ok_or(DecodeError::Invalid("length over bound"))
    }
}

impl<const N: usize> FuzzSample for WireString<N> {
    #[allow(clippy::cast_possible_truncation)] // below(26) fits u8
    fn fuzz_sample(rng: &mut Rng) -> Self {
        let cap = u32::try_from(N).unwrap_or(u32::MAX);
        let n = rng.below(cap.min(12) + 1);
        let mut out = Self::empty();
        for (i, slot) in out.bytes.iter_mut().enumerate().take(n as usize) {
            *slot = b'a' + rng.below(26) as u8;
            out.len = u16::try_from(i + 1).unwrap_or(0);
        }
        out
    }
}
