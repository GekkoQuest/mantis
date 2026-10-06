//! Little-endian reading and writing shared by every format, and the common error type.

/// Why an asset was rejected. Every format rejects the whole asset on the first problem.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FormatError {
    /// Wrong magic bytes.
    Magic,
    /// Unsupported version.
    Version(u16),
    /// Unknown or reserved flag bits set.
    Flags(u32),
    /// Unsupported encoding.
    Encoding(u32),
    /// A dimension or count is out of range.
    Dimensions,
    /// Origin, spacing, bounds, or a transform is not finite or not valid.
    Geometry,
    /// Keyframe count or times invalid.
    Keyframes,
    /// Reserved or padding bytes not zero.
    Reserved,
    /// The data ended early, or bytes remain after the last field.
    Length {
        /// Bytes expected (at least).
        expected: u64,
        /// Bytes present.
        actual: u64,
    },
    /// A value is not finite.
    NonFinite,
    /// A validity byte is neither 0 nor 1.
    Validity,
    /// A chunk id this version does not know.
    UnknownChunk([u8; 4]),
    /// A chunk that may appear once appears again.
    DuplicateChunk([u8; 4]),
    /// A required chunk is missing.
    MissingChunk([u8; 4]),
    /// Two fields disagree (for example a placement's lightmap is not in the atlas list).
    Inconsistent,
    /// An id that must be unique repeats.
    DuplicateId(u32),
}

impl core::fmt::Display for FormatError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "invalid asset: {self:?}")
    }
}

impl std::error::Error for FormatError {}

/// A bounds-checked little-endian reader.
#[derive(Debug)]
pub struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    /// A reader at the start of `bytes`.
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    /// Bytes consumed so far.
    pub fn position(&self) -> usize {
        self.at
    }

    /// Bytes left.
    pub fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.at)
    }

    fn short(&self, need: usize) -> FormatError {
        FormatError::Length {
            expected: (self.at + need) as u64,
            actual: self.bytes.len() as u64,
        }
    }

    /// The next `n` bytes.
    ///
    /// # Errors
    /// [`FormatError::Length`] if fewer remain.
    pub fn slice(&mut self, n: usize) -> Result<&'a [u8], FormatError> {
        let end = self.at.checked_add(n).ok_or(self.short(n))?;
        let s = self.bytes.get(self.at..end).ok_or(self.short(n))?;
        self.at = end;
        Ok(s)
    }

    /// The next `N` bytes as an array.
    ///
    /// # Errors
    /// [`FormatError::Length`] if fewer remain.
    pub fn array<const N: usize>(&mut self) -> Result<[u8; N], FormatError> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.slice(N)?);
        Ok(out)
    }

    /// A `u8`.
    ///
    /// # Errors
    /// [`FormatError::Length`].
    pub fn u8(&mut self) -> Result<u8, FormatError> {
        Ok(u8::from_le_bytes(self.array()?))
    }

    /// A `u16`.
    ///
    /// # Errors
    /// [`FormatError::Length`].
    pub fn u16(&mut self) -> Result<u16, FormatError> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    /// A `u32`.
    ///
    /// # Errors
    /// [`FormatError::Length`].
    pub fn u32(&mut self) -> Result<u32, FormatError> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    /// An `i32`.
    ///
    /// # Errors
    /// [`FormatError::Length`].
    pub fn i32(&mut self) -> Result<i32, FormatError> {
        Ok(i32::from_le_bytes(self.array()?))
    }

    /// An `f32`, which must be finite.
    ///
    /// # Errors
    /// [`FormatError::Length`] or [`FormatError::NonFinite`].
    pub fn f32(&mut self) -> Result<f32, FormatError> {
        let v = f32::from_le_bytes(self.array()?);
        if v.is_finite() {
            Ok(v)
        } else {
            Err(FormatError::NonFinite)
        }
    }

    /// An `f32` read without the finiteness check (for fields with their own rules).
    ///
    /// # Errors
    /// [`FormatError::Length`].
    pub fn f32_raw(&mut self) -> Result<f32, FormatError> {
        Ok(f32::from_le_bytes(self.array()?))
    }

    /// Three finite `f32`s.
    ///
    /// # Errors
    /// [`FormatError::Length`] or [`FormatError::NonFinite`].
    pub fn vec3(&mut self) -> Result<[f32; 3], FormatError> {
        Ok([self.f32()?, self.f32()?, self.f32()?])
    }

    /// Fails unless every byte was consumed.
    ///
    /// # Errors
    /// [`FormatError::Length`] when bytes remain.
    pub fn finish(&self) -> Result<(), FormatError> {
        if self.at == self.bytes.len() {
            Ok(())
        } else {
            Err(FormatError::Length {
                expected: self.at as u64,
                actual: self.bytes.len() as u64,
            })
        }
    }
}

/// A little-endian writer.
#[derive(Debug, Default)]
pub struct Writer {
    bytes: Vec<u8>,
}

impl Writer {
    /// An empty writer.
    pub fn new() -> Self {
        Self::default()
    }

    /// The bytes written.
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// Bytes written so far.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// True when nothing was written.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Raw bytes.
    pub fn bytes(&mut self, b: &[u8]) {
        self.bytes.extend_from_slice(b);
    }

    /// A `u8`.
    pub fn u8(&mut self, v: u8) {
        self.bytes.push(v);
    }

    /// A `u16`.
    pub fn u16(&mut self, v: u16) {
        self.bytes(&v.to_le_bytes());
    }

    /// A `u32`.
    pub fn u32(&mut self, v: u32) {
        self.bytes(&v.to_le_bytes());
    }

    /// An `i32`.
    pub fn i32(&mut self, v: i32) {
        self.bytes(&v.to_le_bytes());
    }

    /// An `f32`.
    pub fn f32(&mut self, v: f32) {
        self.bytes(&v.to_le_bytes());
    }

    /// Three `f32`s.
    pub fn vec3(&mut self, v: [f32; 3]) {
        for c in v {
            self.f32(c);
        }
    }

    /// A count that must fit `u32`.
    pub fn count(&mut self, n: usize) {
        self.u32(u32::try_from(n).unwrap_or(u32::MAX));
    }
}
