//! Stable hashing for simulation state.
//!
//! [`StableHasher`] is XXH64 with seed 0, implemented in-crate. Its output is a
//! pure function of the bytes written, identical on every target, compiler
//! version, and run. `std`'s `DefaultHasher` makes no such promise and must
//! never be used for anything persisted, compared across hosts, or logged.
//!
//! [`StateHash`] is how a value feeds itself to the hasher. The encoding rules
//! are part of the replay contract (plan 6.8):
//!
//! - integers are written little-endian at their declared width; `usize` and
//!   `isize` have no implementation because their width differs by target;
//! - `bool` is one byte, 0 or 1;
//! - `f32`/`f64` are written as their IEEE bit patterns, except that every NaN
//!   is written as the canonical quiet NaN (`0x7FC0_0000` / `0x7FF8_0000_0000_0000`),
//!   because NaN sign and payload differ between `x86_64` and aarch64. Signed
//!   zeros are distinct, since IEEE arithmetic produces them identically everywhere;
//! - `Option<T>` writes a tag byte (0 for `None`, 1 for `Some`) and then the value;
//! - arrays and tuples write their elements in order, with no length prefix;
//! - slices write their length as `u64` and then their elements.
//!
//! Every ECS component implements [`StateHash`], so per-tick state hashes cover
//! all simulation state.

const P1: u64 = 0x9E37_79B1_85EB_CA87;
const P2: u64 = 0xC2B2_AE3D_27D4_EB4F;
const P3: u64 = 0x1656_67B1_9E37_79F9;
const P4: u64 = 0x85EB_CA77_C2B2_AE63;
const P5: u64 = 0x27D4_EB2F_1656_67C5;

const fn round(acc: u64, input: u64) -> u64 {
    acc.wrapping_add(input.wrapping_mul(P2))
        .rotate_left(31)
        .wrapping_mul(P1)
}

const fn merge_round(acc: u64, val: u64) -> u64 {
    (acc ^ round(0, val)).wrapping_mul(P1).wrapping_add(P4)
}

/// Streaming XXH64 (seed 0). Allocation-free; 32 bytes of internal buffer.
#[derive(Clone, Debug)]
pub struct StableHasher {
    v: [u64; 4],
    buf: [u8; 32],
    buf_len: usize,
    total_len: u64,
}

impl Default for StableHasher {
    fn default() -> Self {
        Self::new()
    }
}

impl StableHasher {
    /// A fresh hasher.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            v: [P1.wrapping_add(P2), P2, 0, 0u64.wrapping_sub(P1)],
            buf: [0; 32],
            buf_len: 0,
            total_len: 0,
        }
    }

    fn stripe(&mut self, block: &[u8; 32]) {
        let (lanes, _) = block.as_chunks::<8>();
        for (v, lane) in self.v.iter_mut().zip(lanes) {
            *v = round(*v, u64::from_le_bytes(*lane));
        }
    }

    /// Feeds raw bytes.
    pub fn write(&mut self, mut bytes: &[u8]) {
        self.total_len = self.total_len.wrapping_add(bytes.len() as u64);
        if self.buf_len > 0 {
            let take = (32 - self.buf_len).min(bytes.len());
            let (head, rest) = bytes.split_at(take);
            if let Some(dst) = self.buf.get_mut(self.buf_len..self.buf_len + take) {
                dst.copy_from_slice(head);
            }
            self.buf_len += take;
            bytes = rest;
            if self.buf_len < 32 {
                return;
            }
            let block = self.buf;
            self.stripe(&block);
            self.buf_len = 0;
        }
        let (blocks, tail) = bytes.as_chunks::<32>();
        for block in blocks {
            self.stripe(block);
        }
        if let Some(dst) = self.buf.get_mut(..tail.len()) {
            dst.copy_from_slice(tail);
        }
        self.buf_len = tail.len();
    }

    /// The digest of everything written so far. The hasher can keep accepting
    /// input afterwards.
    #[must_use]
    pub fn finish(&self) -> u64 {
        let mut h = if self.total_len >= 32 {
            let [v1, v2, v3, v4] = self.v;
            let mut h = v1
                .rotate_left(1)
                .wrapping_add(v2.rotate_left(7))
                .wrapping_add(v3.rotate_left(12))
                .wrapping_add(v4.rotate_left(18));
            for v in self.v {
                h = merge_round(h, v);
            }
            h
        } else {
            P5
        };
        h = h.wrapping_add(self.total_len);
        let tail = self.buf.get(..self.buf_len).unwrap_or(&[]);
        let (words, mut rest) = tail.as_chunks::<8>();
        for w in words {
            h ^= round(0, u64::from_le_bytes(*w));
            h = h.rotate_left(27).wrapping_mul(P1).wrapping_add(P4);
        }
        if let Some((w, r)) = rest.split_first_chunk::<4>() {
            h ^= u64::from(u32::from_le_bytes(*w)).wrapping_mul(P1);
            h = h.rotate_left(23).wrapping_mul(P2).wrapping_add(P3);
            rest = r;
        }
        for &b in rest {
            h ^= u64::from(b).wrapping_mul(P5);
            h = h.rotate_left(11).wrapping_mul(P1);
        }
        h ^= h >> 33;
        h = h.wrapping_mul(P2);
        h ^= h >> 29;
        h = h.wrapping_mul(P3);
        h ^= h >> 32;
        h
    }

    /// One-shot XXH64 of `bytes`.
    #[must_use]
    pub fn hash_bytes(bytes: &[u8]) -> u64 {
        let mut h = Self::new();
        h.write(bytes);
        h.finish()
    }

    /// Writes a `u8`.
    pub fn write_u8(&mut self, v: u8) {
        self.write(&[v]);
    }
    /// Writes a `u16` little-endian.
    pub fn write_u16(&mut self, v: u16) {
        self.write(&v.to_le_bytes());
    }
    /// Writes a `u32` little-endian.
    pub fn write_u32(&mut self, v: u32) {
        self.write(&v.to_le_bytes());
    }
    /// Writes a `u64` little-endian.
    pub fn write_u64(&mut self, v: u64) {
        self.write(&v.to_le_bytes());
    }
    /// Writes an `f32` by its bit pattern, with NaN canonicalised.
    pub fn write_f32(&mut self, v: f32) {
        self.write_u32(if v.is_nan() { 0x7FC0_0000 } else { v.to_bits() });
    }
    /// Writes an `f64` by its bit pattern, with NaN canonicalised.
    pub fn write_f64(&mut self, v: f64) {
        self.write_u64(if v.is_nan() {
            0x7FF8_0000_0000_0000
        } else {
            v.to_bits()
        });
    }
}

/// A value that feeds a stable, documented encoding of itself to a
/// [`StableHasher`]. See the module docs for the encoding rules.
pub trait StateHash {
    /// Writes this value's encoding into `h`.
    fn state_hash(&self, h: &mut StableHasher);

    /// Convenience: the XXH64 of this value alone.
    fn stable_hash(&self) -> u64 {
        let mut h = StableHasher::new();
        self.state_hash(&mut h);
        h.finish()
    }
}

macro_rules! int_state_hash {
    ($($t:ty),*) => {$(
        impl StateHash for $t {
            fn state_hash(&self, h: &mut StableHasher) {
                h.write(&self.to_le_bytes());
            }
        }
    )*};
}
int_state_hash!(u8, u16, u32, u64, u128, i8, i16, i32, i64, i128);

impl StateHash for bool {
    fn state_hash(&self, h: &mut StableHasher) {
        h.write_u8(u8::from(*self));
    }
}

impl StateHash for f32 {
    fn state_hash(&self, h: &mut StableHasher) {
        h.write_f32(*self);
    }
}

impl StateHash for f64 {
    fn state_hash(&self, h: &mut StableHasher) {
        h.write_f64(*self);
    }
}

impl StateHash for () {
    fn state_hash(&self, _h: &mut StableHasher) {}
}

impl<T: StateHash> StateHash for Option<T> {
    fn state_hash(&self, h: &mut StableHasher) {
        match self {
            None => h.write_u8(0),
            Some(v) => {
                h.write_u8(1);
                v.state_hash(h);
            }
        }
    }
}

impl<T: StateHash, const N: usize> StateHash for [T; N] {
    fn state_hash(&self, h: &mut StableHasher) {
        for v in self {
            v.state_hash(h);
        }
    }
}

impl<T: StateHash> StateHash for [T] {
    fn state_hash(&self, h: &mut StableHasher) {
        h.write_u64(self.len() as u64);
        for v in self {
            v.state_hash(h);
        }
    }
}

macro_rules! tuple_state_hash {
    ($($name:ident),+) => {
        impl<$($name: StateHash),+> StateHash for ($($name,)+) {
            #[expect(non_snake_case)]
            fn state_hash(&self, h: &mut StableHasher) {
                let ($($name,)+) = self;
                $($name.state_hash(h);)+
            }
        }
    };
}
tuple_state_hash!(A);
tuple_state_hash!(A, B);
tuple_state_hash!(A, B, C);
tuple_state_hash!(A, B, C, D);
tuple_state_hash!(A, B, C, D, E);
tuple_state_hash!(A, B, C, D, E, F);

/// Implements [`StateHash`] for a struct by hashing the listed fields in the
/// listed order. List every field (named fields or tuple indices); the order is
/// part of the state encoding.
///
/// ```
/// use mantis_core::hash::StateHash;
/// struct Health { current: u32, max: u32 }
/// mantis_core::impl_state_hash!(Health { current, max });
/// assert_ne!(Health { current: 1, max: 2 }.stable_hash(), Health { current: 2, max: 1 }.stable_hash());
/// ```
#[macro_export]
macro_rules! impl_state_hash {
    ($ty:ty { $($field:tt),* $(,)? }) => {
        impl $crate::hash::StateHash for $ty {
            fn state_hash(&self, h: &mut $crate::hash::StableHasher) {
                $( $crate::hash::StateHash::state_hash(&self.$field, h); )*
                let _ = h;
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Published XXH64 vectors (seed 0), plus two longer inputs that exercise
    /// the 32-byte stripe path, cross-checked against an independent
    /// implementation of the specification.
    #[test]
    fn xxh64_reference_vectors() {
        assert_eq!(StableHasher::hash_bytes(b""), 0xEF46_DB37_51D8_E999);
        assert_eq!(StableHasher::hash_bytes(b"a"), 0xD24E_C4F1_A98C_6E5B);
        assert_eq!(StableHasher::hash_bytes(b"abc"), 0x44BC_2CF5_AD77_0999);
        assert_eq!(StableHasher::hash_bytes(b"message digest"), 0x066E_D728_FCEE_B3BE);
        assert_eq!(
            StableHasher::hash_bytes(b"The quick brown fox jumps over the lazy dog"),
            0x0B24_2D36_1FDA_71BC
        );
        let hundred: Vec<u8> = (0u8..100).collect();
        assert_eq!(StableHasher::hash_bytes(&hundred), 0x6AC1_E580_3216_6597);
    }

    #[test]
    fn streaming_matches_one_shot_at_every_split() {
        let data: Vec<u8> = (0u8..=200).map(|b| b.wrapping_mul(31)).collect();
        let one_shot = StableHasher::hash_bytes(&data);
        for split_a in 0..data.len() {
            let split_b = (split_a * 7 + 3) % (data.len() + 1);
            let (lo, hi) = (split_a.min(split_b), split_a.max(split_b));
            let mut h = StableHasher::new();
            h.write(&data[..lo]);
            h.write(&data[lo..hi]);
            h.write(&data[hi..]);
            assert_eq!(h.finish(), one_shot, "split {lo}/{hi}");
        }
    }

    #[test]
    fn float_encoding_canonicalises_nan_only() {
        let nan_a = f32::from_bits(0x7FC0_0000);
        let nan_b = f32::from_bits(0xFFC0_0001);
        assert_eq!(nan_a.stable_hash(), nan_b.stable_hash());
        assert_ne!(0.0f32.stable_hash(), (-0.0f32).stable_hash());
        assert_eq!(1.5f32.stable_hash(), 0x3FC0_0000u32.stable_hash());
        assert_eq!(
            f64::from_bits(0xFFF8_0000_0000_0001).stable_hash(),
            f64::NAN.stable_hash()
        );
    }

    #[test]
    fn structural_encodings() {
        assert_ne!(None::<u8>.stable_hash(), Some(0u8).stable_hash());
        assert_eq!((1u8, 2u16).stable_hash(), {
            let mut h = StableHasher::new();
            h.write(&[1, 2, 0]);
            h.finish()
        });
        let s: &[u8] = &[9, 8];
        assert_eq!(s.stable_hash(), {
            let mut h = StableHasher::new();
            h.write_u64(2);
            h.write(&[9, 8]);
            h.finish()
        });
        assert_eq!([3u8, 4].stable_hash(), StableHasher::hash_bytes(&[3, 4]));
        assert_eq!(true.stable_hash(), StableHasher::hash_bytes(&[1]));
    }
}
