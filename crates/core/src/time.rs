//! Time: ticks, tick rates, and the injected [`Clock`] (plan 6.4, principle 8).
//!
//! Simulation time is a tick counter. There is no wall clock anywhere in the
//! core; the `clippy.toml` of every simulation crate bans `Instant` and
//! `SystemTime`. Durations are tick counts (`u64`). Conversions to seconds
//! exist for presentation and for feeding `dt` into kinematics, and each one is
//! a single IEEE operation, so it produces the same bits on every target.

use core::fmt;
use core::num::NonZeroU32;

use crate::hash::{StableHasher, StateHash};

/// An absolute simulation tick.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Tick(pub u64);

impl Tick {
    /// Tick zero.
    pub const ZERO: Self = Self(0);

    /// The raw tick number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The following tick. Saturates at `u64::MAX`, which no real session reaches.
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }

    /// `self + n` ticks, or `None` on overflow.
    #[must_use]
    pub const fn checked_add(self, n: u64) -> Option<Self> {
        match self.0.checked_add(n) {
            Some(t) => Some(Self(t)),
            None => None,
        }
    }

    /// Ticks elapsed from `earlier` to `self`, or `None` if `earlier` is later.
    #[must_use]
    pub const fn checked_sub(self, earlier: Self) -> Option<u64> {
        self.0.checked_sub(earlier.0)
    }

    /// Ticks elapsed from `earlier` to `self`, or 0 if `earlier` is later.
    #[must_use]
    pub const fn saturating_sub(self, earlier: Self) -> u64 {
        self.0.saturating_sub(earlier.0)
    }
}

impl fmt::Debug for Tick {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Tick({})", self.0)
    }
}

impl fmt::Display for Tick {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "t{}", self.0)
    }
}

impl StateHash for Tick {
    fn state_hash(&self, h: &mut StableHasher) {
        h.write_u64(self.0);
    }
}

/// A validated, nonzero simulation rate in ticks per second.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct TickRate(NonZeroU32);

impl TickRate {
    /// 30 Hz, the default cell rate (plan 6.2).
    pub const HZ_30: Self = Self(NonZeroU32::MIN.saturating_add(29));
    /// 60 Hz, the arena rate (plan 6.2).
    pub const HZ_60: Self = Self(NonZeroU32::MIN.saturating_add(59));

    /// A rate of `hz` ticks per second, or `None` for zero.
    #[must_use]
    pub const fn new(hz: u32) -> Option<Self> {
        match NonZeroU32::new(hz) {
            Some(n) => Some(Self(n)),
            None => None,
        }
    }

    /// Ticks per second.
    #[must_use]
    pub const fn hz(self) -> u32 {
        self.0.get()
    }

    /// Seconds per tick as `f32`: exactly `1.0 / hz as f32`.
    ///
    /// One correctly rounded IEEE division, so every host feeds bit-identical
    /// `dt` into `Motion::step`.
    #[must_use]
    #[allow(clippy::cast_precision_loss)] // hz <= u32::MAX; the rounding is the defined result
    pub fn dt_seconds(self) -> f32 {
        1.0 / self.0.get() as f32
    }

    /// Seconds from tick zero to `t`: exactly `t as f64 / hz as f64`.
    ///
    /// For presentation and interpolation. Exact for every tick below 2^53.
    #[must_use]
    #[allow(clippy::cast_precision_loss)] // ticks beyond 2^53 are unreachable in practice
    pub fn seconds_at(self, t: Tick) -> f64 {
        t.0 as f64 / f64::from(self.0.get())
    }

    /// The number of whole ticks that covers `millis` milliseconds, rounded up.
    ///
    /// Integer arithmetic only, so content timings expressed in milliseconds
    /// convert identically on every host. Saturates at `u64::MAX`.
    #[must_use]
    #[allow(clippy::cast_possible_truncation)] // guarded by the comparison
    pub const fn ticks_from_millis(self, millis: u64) -> u64 {
        let num = (millis as u128) * (self.0.get() as u128);
        let ticks = num.div_ceil(1000);
        if ticks > u64::MAX as u128 {
            u64::MAX
        } else {
            ticks as u64
        }
    }
}

impl fmt::Display for TickRate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} Hz", self.0)
    }
}

/// The injected simulation clock: a tick counter and its rate. Nothing else.
pub trait Clock {
    /// The current tick.
    fn tick(&self) -> Tick;

    /// The tick rate.
    fn rate(&self) -> TickRate;

    /// Seconds from tick zero to `t` at this clock's rate.
    fn tick_to_seconds(&self, t: Tick) -> f64 {
        self.rate().seconds_at(t)
    }
}

/// The standard [`Clock`]: advanced explicitly by the host, once per tick.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TickClock {
    tick: Tick,
    rate: TickRate,
}

impl TickClock {
    /// A clock at `start` running at `rate`.
    #[must_use]
    pub const fn new(rate: TickRate, start: Tick) -> Self {
        Self { tick: start, rate }
    }

    /// Advances one tick and returns the new current tick.
    pub fn advance(&mut self) -> Tick {
        self.tick = self.tick.next();
        self.tick
    }
}

impl Clock for TickClock {
    fn tick(&self) -> Tick {
        self.tick
    }

    fn rate(&self) -> TickRate {
        self.rate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_arithmetic() {
        let t = Tick(10);
        assert_eq!(t.next(), Tick(11));
        assert_eq!(Tick(u64::MAX).next(), Tick(u64::MAX));
        assert_eq!(t.checked_add(5), Some(Tick(15)));
        assert_eq!(Tick(u64::MAX).checked_add(1), None);
        assert_eq!(t.checked_sub(Tick(4)), Some(6));
        assert_eq!(t.checked_sub(Tick(11)), None);
        assert_eq!(t.saturating_sub(Tick(11)), 0);
        assert_eq!(t.saturating_sub(Tick(3)), 7);
        assert!(Tick(3) < Tick(4));
        assert_eq!(format!("{t} {t:?}"), "t10 Tick(10)");
    }

    #[test]
    fn rate_validation() {
        assert_eq!(TickRate::new(0), None);
        assert_eq!(TickRate::new(30), Some(TickRate::HZ_30));
        assert_eq!(TickRate::new(60), Some(TickRate::HZ_60));
        assert_eq!(TickRate::HZ_30.hz(), 30);
        assert_eq!(TickRate::HZ_60.to_string(), "60 Hz");
    }

    /// The pinned bit patterns of dt. Client and server both use these exact
    /// values; a change here is a protocol change.
    #[test]
    fn dt_bits_are_pinned() {
        assert_eq!(TickRate::HZ_30.dt_seconds().to_bits(), 0x3D08_8889);
        assert_eq!(TickRate::HZ_60.dt_seconds().to_bits(), 0x3C88_8889);
        // Repeated calls give identical bits.
        for _ in 0..4 {
            assert_eq!(TickRate::HZ_30.dt_seconds().to_bits(), 0x3D08_8889);
        }
    }

    #[test]
    fn seconds_and_millis() {
        let r = TickRate::HZ_30;
        assert_eq!(r.seconds_at(Tick(0)), 0.0);
        assert_eq!(r.seconds_at(Tick(30)), 1.0);
        assert_eq!(r.seconds_at(Tick(45)), 1.5);
        assert_eq!(r.ticks_from_millis(0), 0);
        assert_eq!(r.ticks_from_millis(1000), 30);
        assert_eq!(r.ticks_from_millis(1), 1); // rounds up
        assert_eq!(r.ticks_from_millis(33), 1);
        assert_eq!(r.ticks_from_millis(34), 2);
        assert_eq!(r.ticks_from_millis(u64::MAX), 553_402_322_211_286_549);
        let fast = TickRate::new(4000).unwrap();
        assert_eq!(fast.ticks_from_millis(u64::MAX), u64::MAX, "saturates");
    }

    #[test]
    fn clock_advances() {
        let mut c = TickClock::new(TickRate::HZ_30, Tick(7));
        assert_eq!(c.tick(), Tick(7));
        assert_eq!(c.advance(), Tick(8));
        assert_eq!(c.tick(), Tick(8));
        assert_eq!(c.rate(), TickRate::HZ_30);
        assert_eq!(c.tick_to_seconds(Tick(60)), 2.0);
    }
}
