//! Deterministic random streams (plan 6.4, principle 9).
//!
//! There is no global RNG. Every draw comes from a stream derived from
//! `(seed, tick, entity, salt)`:
//!
//! - the [`Seed`] belongs to the cell and is recorded in the unified log
//!   (decision 0007); it is never sent to clients (plan 8.2), and client
//!   prediction never depends on a roll;
//! - the [`Tick`] and [`EntityId`] make streams independent per tick and per
//!   entity, so system order and entity count never shift another entity's rolls;
//! - the [`Salt`] separates purposes ("crit", "loot", ...), so adding a roll
//!   to one system never changes another system's sequence.
//!
//! The derivation and the generator are fixed and documented, because their
//! outputs are part of replay:
//!
//! 1. `key = mix(mix(mix(mix(DOMAIN ^ seed) ^ tick) ^ entity.to_bits()) ^ salt)`,
//!    where `mix` is the `SplitMix64` finaliser and `DOMAIN` differs between
//!    entity streams and cell-wide streams;
//! 2. the four words of a xoshiro256** state are the first four `SplitMix64`
//!    outputs seeded with `key`.

use crate::ecs::EntityId;
use crate::time::Tick;

/// A cell's root random seed. Logged; never sent to clients.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Seed(pub u64);

/// A stream purpose tag. Declare one constant per purpose:
/// `const CRIT: Salt = Salt::named("combat.crit");`
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Salt(u64);

impl Salt {
    /// A salt from a raw value.
    #[must_use]
    pub const fn new(v: u64) -> Self {
        Self(v)
    }

    /// A salt from a name: the 64-bit FNV-1a hash of its UTF-8 bytes,
    /// computed at compile time when used in a `const`.
    #[must_use]
    pub const fn named(name: &str) -> Self {
        let mut bytes = name.as_bytes();
        let mut h: u64 = 0xCBF2_9CE4_8422_2325;
        while let [first, rest @ ..] = bytes {
            h ^= *first as u64;
            h = h.wrapping_mul(0x0100_0000_01B3);
            bytes = rest;
        }
        Self(h)
    }

    /// The raw value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

const DOMAIN_ENTITY: u64 = 0x6D61_6E74_6973_0001;
const DOMAIN_CELL: u64 = 0x6D61_6E74_6973_0002;
const GOLDEN: u64 = 0x9E37_79B9_7F4A_7C15;

/// The `SplitMix64` output finaliser (a bijection on `u64`).
const fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A deterministic random stream (xoshiro256**). Allocation-free and `Copy`
/// so it can be stored, but the intended use is: derive, draw, drop.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rng {
    s: [u64; 4],
}

impl Rng {
    /// The stream for `entity` at `tick` for purpose `salt`.
    #[must_use]
    pub const fn for_entity(seed: Seed, tick: Tick, entity: EntityId, salt: Salt) -> Self {
        let key = mix(mix(mix(mix(DOMAIN_ENTITY ^ seed.0) ^ tick.0) ^ entity.to_bits()) ^ salt.0);
        Self::from_key(key)
    }

    /// A cell-wide stream at `tick` for purpose `salt` (no entity involved,
    /// for example world events). Disjoint from every entity stream.
    #[must_use]
    pub const fn for_cell(seed: Seed, tick: Tick, salt: Salt) -> Self {
        let key = mix(mix(mix(DOMAIN_CELL ^ seed.0) ^ tick.0) ^ salt.0);
        Self::from_key(key)
    }

    const fn from_key(key: u64) -> Self {
        let mut s = [
            mix(key.wrapping_add(GOLDEN)),
            mix(key.wrapping_add(GOLDEN.wrapping_mul(2))),
            mix(key.wrapping_add(GOLDEN.wrapping_mul(3))),
            mix(key.wrapping_add(GOLDEN.wrapping_mul(4))),
        ];
        // xoshiro's only invalid state is all zeros. SplitMix64 cannot produce
        // four consecutive zeros, but fail safe rather than trust that.
        if s[0] | s[1] | s[2] | s[3] == 0 {
            s[0] = GOLDEN;
        }
        Self { s }
    }

    /// The next 64 random bits.
    pub const fn next_u64(&mut self) -> u64 {
        let result = self.s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = self.s[1] << 17;
        self.s[2] ^= self.s[0];
        self.s[3] ^= self.s[1];
        self.s[1] ^= self.s[2];
        self.s[0] ^= self.s[3];
        self.s[2] ^= t;
        self.s[3] = self.s[3].rotate_left(45);
        result
    }

    /// The next 32 random bits (the high half of [`Rng::next_u64`]).
    pub const fn next_u32(&mut self) -> u32 {
        (self.next_u64() >> 32) as u32
    }

    /// A uniform `f32` in `[0, 1)` with 24 random bits. Exact: every result is
    /// `k / 2^24` for an integer `k`.
    #[expect(clippy::cast_precision_loss)] // k < 2^24 is exactly representable
    pub fn next_f32(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 * (1.0 / 16_777_216.0)
    }

    /// A uniform `f64` in `[0, 1)` with 53 random bits. Exact.
    #[expect(clippy::cast_precision_loss)] // k < 2^53 is exactly representable
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / 9_007_199_254_740_992.0)
    }

    /// A uniform integer in `[0, bound)`, unbiased (Lemire's method with
    /// rejection). Returns 0 when `bound` is 0.
    #[expect(clippy::cast_possible_truncation)] // high 32 bits of a 64-bit product
    pub fn below(&mut self, bound: u32) -> u32 {
        if bound == 0 {
            return 0;
        }
        let bound64 = u64::from(bound);
        let mut m = u64::from(self.next_u32()) * bound64;
        let mut low = m as u32;
        if low < bound {
            let threshold = bound.wrapping_neg() % bound;
            while low < threshold {
                m = u64::from(self.next_u32()) * bound64;
                low = m as u32;
            }
        }
        (m >> 32) as u32
    }

    /// A uniform integer in `[lo, hi)`, or `None` if the range is empty.
    pub fn range(&mut self, lo: u32, hi: u32) -> Option<u32> {
        if lo >= hi {
            return None;
        }
        Some(lo + self.below(hi - lo))
    }

    /// True with probability exactly `numerator / denominator`, in integer
    /// arithmetic. Always false when `denominator` is 0 or `numerator` is 0;
    /// always true when `numerator >= denominator > 0`.
    pub fn chance(&mut self, numerator: u32, denominator: u32) -> bool {
        if denominator == 0 || numerator == 0 {
            return false;
        }
        if numerator >= denominator {
            return true;
        }
        self.below(denominator) < numerator
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SALT: Salt = Salt::named("test.purpose");

    #[test]
    fn splitmix_reference() {
        // SplitMix64 seeded with 0 produces these published outputs.
        let mut state = 0u64;
        let mut out = [0u64; 3];
        for o in &mut out {
            state = state.wrapping_add(GOLDEN);
            *o = mix(state);
        }
        assert_eq!(
            out,
            [
                0xE220_A839_7B1D_CDAF,
                0x6E78_9E6A_A1B9_65F4,
                0x06C4_5D18_8009_454F
            ]
        );
    }

    #[test]
    fn salt_named_is_fnv1a() {
        assert_eq!(Salt::named("").get(), 0xCBF2_9CE4_8422_2325);
        assert_eq!(Salt::named("a").get(), 0xAF63_DC4C_8601_EC8C);
        assert_ne!(Salt::named("combat.crit"), Salt::named("loot.drop"));
    }

    /// Pinned outputs. These values are part of replay; a change here breaks
    /// every recorded log and must be a decision.
    #[test]
    fn stream_outputs_are_pinned() {
        let mut r = Rng::for_entity(Seed(42), Tick(1000), EntityId::new(7, 3), SALT);
        let got = [r.next_u64(), r.next_u64(), r.next_u64()];
        assert_eq!(got, PINNED_ENTITY);
        let mut c = Rng::for_cell(Seed(42), Tick(1000), SALT);
        assert_eq!(c.next_u64(), PINNED_CELL);
    }

    // Cross-checked against an independent implementation of the documented
    // derivation and of xoshiro256**.
    const PINNED_ENTITY: [u64; 3] = [
        0x8AFD_740C_59EA_064B,
        0x380F_B62A_559F_35F0,
        0x4932_5CBC_C534_F4B4,
    ];
    const PINNED_CELL: u64 = 0x5608_DC0A_1EBB_A018;

    #[test]
    fn streams_are_pure_and_distinct() {
        let a = Rng::for_entity(Seed(1), Tick(5), EntityId::new(1, 0), SALT);
        let b = Rng::for_entity(Seed(1), Tick(5), EntityId::new(1, 0), SALT);
        assert_eq!(a, b);
        let variants = [
            Rng::for_entity(Seed(2), Tick(5), EntityId::new(1, 0), SALT),
            Rng::for_entity(Seed(1), Tick(6), EntityId::new(1, 0), SALT),
            Rng::for_entity(Seed(1), Tick(5), EntityId::new(2, 0), SALT),
            Rng::for_entity(Seed(1), Tick(5), EntityId::new(1, 1), SALT),
            Rng::for_entity(Seed(1), Tick(5), EntityId::new(1, 0), Salt::new(1)),
            Rng::for_cell(Seed(1), Tick(5), SALT),
        ];
        for v in variants {
            assert_ne!(a, v);
        }
    }

    #[test]
    fn floats_are_in_unit_interval() {
        let mut r = Rng::for_cell(Seed(9), Tick(0), SALT);
        for _ in 0..10_000 {
            let f = r.next_f32();
            assert!((0.0..1.0).contains(&f));
            let d = r.next_f64();
            assert!((0.0..1.0).contains(&d));
        }
    }

    #[test]
    fn below_is_bounded_and_roughly_uniform() {
        let mut r = Rng::for_cell(Seed(3), Tick(0), SALT);
        assert_eq!(r.below(0), 0);
        assert_eq!(r.below(1), 0);
        let mut counts = [0u32; 6];
        for _ in 0..60_000 {
            let v = r.below(6);
            counts[v as usize] += 1;
        }
        for c in counts {
            assert!((9_000..11_000).contains(&c), "{counts:?}");
        }
        assert!(r.below(u32::MAX) < u32::MAX);
    }

    #[test]
    fn range_and_chance_edges() {
        let mut r = Rng::for_cell(Seed(4), Tick(0), SALT);
        assert_eq!(r.range(5, 5), None);
        assert_eq!(r.range(6, 5), None);
        for _ in 0..1000 {
            let v = r.range(10, 13).unwrap();
            assert!((10..13).contains(&v));
        }
        assert!(!r.chance(0, 10));
        assert!(!r.chance(5, 0));
        assert!(r.chance(10, 10));
        assert!(r.chance(11, 10));
        let hits = (0..100_000).filter(|_| r.chance(1, 4)).count();
        assert!((24_000..26_000).contains(&hits), "{hits}");
    }
}
