//! Host time: an injected monotonic clock, the fixed-step scheduler that decouples tick
//! rate from frame rate, and the mapping from server ticks to host time.
//!
//! Simulation code never reads time from here: it sees only [`Tick`]s. The host uses
//! [`HostClock`] to decide *when* a tick runs and the render thread uses it to decide
//! *what instant* a frame shows. Both read the same clock, so render time and tick time
//! share one timeline.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::core_api::{Tick, TickRate};

/// Exact nanoseconds from tick zero to the start of `tick` at `rate`, rounded down.
///
/// Computed in 128-bit integers so the mapping never drifts, whatever the tick.
pub fn tick_start_nanos(rate: TickRate, tick: Tick) -> u128 {
    u128::from(tick.0) * 1_000_000_000 / u128::from(rate.hz())
}

/// The largest tick whose start ([`tick_start_nanos`]) is at or before `nanos`.
///
/// `tick_start_nanos(t) = floor(t * 1e9 / hz) <= n` holds exactly when
/// `t * 1e9 < (n + 1) * hz`, so the answer is `floor(((n + 1) * hz - 1) / 1e9)`.
#[allow(clippy::cast_possible_truncation)] // Saturated to u64 explicitly.
pub fn tick_at_nanos(rate: TickRate, nanos: u128) -> Tick {
    let t = (nanos.saturating_add(1).saturating_mul(u128::from(rate.hz())) - 1) / 1_000_000_000;
    if t > u128::from(u64::MAX) {
        Tick(u64::MAX)
    } else {
        Tick(t as u64)
    }
}

/// A point on the host's monotonic timeline, in nanoseconds since the clock's origin.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct HostInstant(u64);

impl HostInstant {
    /// The clock origin.
    pub const ZERO: HostInstant = HostInstant(0);

    /// An instant `nanos` after the origin.
    pub const fn from_nanos(nanos: u64) -> Self {
        Self(nanos)
    }

    /// Nanoseconds since the origin.
    pub const fn as_nanos(self) -> u64 {
        self.0
    }

    /// Time from `earlier` to `self`, or zero if `earlier` is later.
    pub const fn saturating_since(self, earlier: HostInstant) -> Duration {
        Duration::from_nanos(self.0.saturating_sub(earlier.0))
    }

    /// Signed seconds from `earlier` to `self` (negative when `earlier` is later).
    #[allow(clippy::cast_precision_loss)] // Sub-nanosecond precision is irrelevant here.
    pub fn seconds_since(self, earlier: HostInstant) -> f64 {
        if self.0 >= earlier.0 {
            (self.0 - earlier.0) as f64 * 1e-9
        } else {
            -((earlier.0 - self.0) as f64 * 1e-9)
        }
    }

    /// `self + d`, saturating at the end of the timeline.
    #[must_use]
    #[allow(clippy::cast_possible_truncation)] // Saturated explicitly.
    pub fn saturating_add(self, d: Duration) -> HostInstant {
        let n = d.as_nanos();
        let n = if n > u128::from(u64::MAX) {
            u64::MAX
        } else {
            n as u64
        };
        HostInstant(self.0.saturating_add(n))
    }

    /// `self - d`, saturating at the origin.
    #[must_use]
    #[allow(clippy::cast_possible_truncation)] // Saturated explicitly.
    pub fn saturating_sub(self, d: Duration) -> HostInstant {
        let n = d.as_nanos();
        let n = if n > u128::from(u64::MAX) {
            u64::MAX
        } else {
            n as u64
        };
        HostInstant(self.0.saturating_sub(n))
    }
}

/// The injected monotonic clock every client thread reads.
pub trait HostClock: Send + Sync {
    /// The current instant. Never decreases.
    fn now(&self) -> HostInstant;
}

impl<C: HostClock + ?Sized> HostClock for Arc<C> {
    fn now(&self) -> HostInstant {
        (**self).now()
    }
}

/// A clock advanced by hand, for tests and for deterministic offline runs.
#[derive(Debug, Default)]
pub struct ManualClock {
    nanos: AtomicU64,
}

impl ManualClock {
    /// A clock at its origin.
    pub fn new() -> Self {
        Self::default()
    }

    /// Moves the clock forward by `d`.
    #[allow(clippy::cast_possible_truncation)] // Saturated explicitly.
    pub fn advance(&self, d: Duration) {
        let n = d.as_nanos();
        let n = if n > u128::from(u64::MAX) {
            u64::MAX
        } else {
            n as u64
        };
        // fetch_update cannot fail with a closure that always returns Some.
        let _ = self
            .nanos
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| Some(v.saturating_add(n)));
    }

    /// Sets the clock to `t` if `t` is not earlier than the current instant.
    pub fn set(&self, t: HostInstant) {
        self.nanos.fetch_max(t.0, Ordering::AcqRel);
    }
}

impl HostClock for ManualClock {
    fn now(&self) -> HostInstant {
        HostInstant(self.nanos.load(Ordering::Acquire))
    }
}

/// A batch of ticks due now, returned by [`FixedStepper::poll`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StepBatch {
    /// First tick to run.
    pub first: Tick,
    /// Number of consecutive ticks to run.
    pub count: u32,
    /// Ticks of host time that were given up because the host fell further behind than
    /// the catch-up limit. Tick numbers stay contiguous; the simulation timeline slips
    /// relative to host time instead.
    pub slipped: u64,
}

impl StepBatch {
    /// The ticks in this batch, in order.
    pub fn ticks(self) -> impl Iterator<Item = Tick> {
        (0..u64::from(self.count)).map_while(move |i| self.first.checked_add(i))
    }
}

/// Fixed-step scheduler: decides which ticks are due at a host instant.
///
/// Tick `t` is due once host time reaches `tick_time(t)`. The mapping is exact integer
/// arithmetic from an epoch, so it never drifts. If the host falls more than
/// `max_catchup` ticks behind (a debugger pause, a long hitch), the extra ticks are not
/// run in a burst; the epoch slips forward instead and the slip is reported.
#[derive(Clone, Debug)]
pub struct FixedStepper {
    rate: TickRate,
    epoch_tick: Tick,
    epoch_time: HostInstant,
    next: Tick,
    max_catchup: u32,
    slipped_total: u64,
}

impl FixedStepper {
    /// A stepper whose tick `first` is due at `start`.
    pub fn new(rate: TickRate, first: Tick, start: HostInstant, max_catchup: u32) -> Self {
        Self {
            rate,
            epoch_tick: first,
            epoch_time: start,
            next: first,
            max_catchup: max_catchup.max(1),
            slipped_total: 0,
        }
    }

    /// The tick rate.
    pub fn rate(&self) -> TickRate {
        self.rate
    }

    /// The next tick that has not been handed out.
    pub fn next_tick(&self) -> Tick {
        self.next
    }

    /// Total ticks slipped since creation.
    pub fn slipped_total(&self) -> u64 {
        self.slipped_total
    }

    /// Host instant at which `tick` is due. Ticks before the epoch map to the epoch.
    #[allow(clippy::cast_possible_truncation)] // Saturated explicitly.
    pub fn tick_time(&self, tick: Tick) -> HostInstant {
        let rel = Tick(tick.saturating_sub(self.epoch_tick));
        let n = tick_start_nanos(self.rate, rel);
        let n = if n > u128::from(u64::MAX) {
            u64::MAX
        } else {
            n as u64
        };
        HostInstant(self.epoch_time.0.saturating_add(n))
    }

    /// Hands out every tick due at `now`, bounded by the catch-up limit.
    pub fn poll(&mut self, now: HostInstant) -> StepBatch {
        if now < self.epoch_time {
            return StepBatch {
                first: self.next,
                count: 0,
                slipped: 0,
            };
        }
        let since_epoch = u128::from(now.0 - self.epoch_time.0);
        // Last due tick, relative to the epoch.
        let last_due_rel = tick_at_nanos(self.rate, since_epoch);
        let last_due = Tick(self.epoch_tick.0.saturating_add(last_due_rel.0));
        let due = match last_due.checked_sub(self.next) {
            Some(d) => d.saturating_add(1),
            None => {
                return StepBatch {
                    first: self.next,
                    count: 0,
                    slipped: 0,
                };
            }
        };
        let limit = u64::from(self.max_catchup);
        let (count, slipped) = if due > limit {
            (limit, due - limit)
        } else {
            (due, 0)
        };
        let first = self.next;
        if slipped > 0 {
            // Re-anchor so that `first` is due where `first + slipped` would have been.
            self.epoch_time = self.tick_time(Tick(first.0.saturating_add(slipped)));
            self.epoch_tick = first;
            self.slipped_total = self.slipped_total.saturating_add(slipped);
        }
        self.next = Tick(first.0.saturating_add(count));
        #[allow(clippy::cast_possible_truncation)] // count <= max_catchup, a u32.
        StepBatch {
            first,
            count: count as u32,
            slipped,
        }
    }

    /// Time from `now` until the next tick is due (zero if it is already due).
    pub fn until_next(&self, now: HostInstant) -> Duration {
        self.tick_time(self.next).saturating_since(now)
    }
}

/// Maps server ticks to host time from snapshot arrivals.
///
/// Each arrival yields an offset sample `arrival - server_tick_time`. Network delay only
/// ever adds to a sample, so the smallest recent sample is the best estimate of the true
/// offset: a lower sample is adopted at once, a higher one is approached slowly so the
/// estimate tracks clock drift and route changes without following jitter.
#[derive(Clone, Debug)]
pub struct ServerTimeline {
    rate: TickRate,
    offset_nanos: Option<i128>,
    rise_shift: u32,
}

impl ServerTimeline {
    /// A timeline for a server running at `rate`. `rise_shift` sets how slowly the
    /// estimate rises toward later samples: each sample moves it by `1 / 2^rise_shift`
    /// of the difference.
    pub fn new(rate: TickRate, rise_shift: u32) -> Self {
        Self {
            rate,
            offset_nanos: None,
            rise_shift: rise_shift.min(30),
        }
    }

    /// Records that `server_tick` arrived at host instant `arrival`.
    #[allow(clippy::cast_possible_wrap)] // tick_start_nanos is far below i128::MAX.
    pub fn observe(&mut self, server_tick: Tick, arrival: HostInstant) {
        let sample = i128::from(arrival.0) - tick_start_nanos(self.rate, server_tick) as i128;
        self.offset_nanos = Some(match self.offset_nanos {
            None => sample,
            Some(cur) if sample <= cur => sample,
            Some(cur) => cur + ((sample - cur) >> self.rise_shift).max(1),
        });
    }

    /// Estimated host instant at which the server produced `server_tick`.
    #[allow(
        clippy::cast_possible_wrap,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub fn host_time(&self, server_tick: Tick) -> Option<HostInstant> {
        let off = self.offset_nanos?;
        let t = tick_start_nanos(self.rate, server_tick) as i128 + off;
        Some(HostInstant(t.clamp(0, i128::from(u64::MAX)) as u64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{TestResult, rate};

    #[test]
    fn tick_nanos_mapping_is_exact_and_consistent() -> TestResult {
        let r = rate(30)?;
        assert_eq!(tick_start_nanos(r, Tick(30)), 1_000_000_000);
        assert_eq!(tick_start_nanos(r, Tick(1)), 33_333_333);
        // Large ticks do not drift: tick 30 * 10^9 starts exactly 10^9 seconds in.
        assert_eq!(
            tick_start_nanos(r, Tick(30_000_000_000)),
            1_000_000_000 * 1_000_000_000
        );
        for hz in [1, 7, 30, 60, 144] {
            let r = rate(hz)?;
            for t in [0u64, 1, 2, 29, 30, 31, 12_345, 1 << 40] {
                let start = tick_start_nanos(r, Tick(t));
                assert_eq!(tick_at_nanos(r, start), Tick(t), "hz {hz} tick {t}");
                if start > 0 {
                    assert_eq!(
                        tick_at_nanos(r, start - 1),
                        Tick(t.saturating_sub(1)),
                        "hz {hz} tick {t}"
                    );
                }
            }
        }
        Ok(())
    }

    #[test]
    fn manual_clock_advances_and_never_goes_back() {
        let c = ManualClock::new();
        c.advance(Duration::from_millis(5));
        assert_eq!(c.now(), HostInstant::from_nanos(5_000_000));
        c.set(HostInstant::from_nanos(1));
        assert_eq!(c.now(), HostInstant::from_nanos(5_000_000));
    }

    #[test]
    fn stepper_hands_out_due_ticks_exactly_once() -> TestResult {
        let mut s = FixedStepper::new(rate(30)?, Tick(0), HostInstant::ZERO, 8);
        let b = s.poll(HostInstant::ZERO);
        assert_eq!((b.first, b.count), (Tick(0), 1));
        assert_eq!(s.poll(HostInstant::ZERO).count, 0);
        // Tick 1 is due at 33_333_333 ns.
        assert_eq!(s.poll(HostInstant::from_nanos(33_333_332)).count, 0);
        let b = s.poll(HostInstant::from_nanos(33_333_333));
        assert_eq!((b.first, b.count), (Tick(1), 1));
        // One full second later: ticks 2..=30 are due.
        let b = s.poll(HostInstant::from_nanos(1_000_000_000));
        assert_eq!((b.first, b.count, b.slipped), (Tick(2), 8, 21));
        assert_eq!(s.next_tick(), Tick(10));
        assert_eq!(s.slipped_total(), 21);
        // After slipping, the epoch maps tick 2 to old tick 23's instant, so tick 10 is
        // due where old tick 31 was (each term floored to whole nanoseconds).
        assert_eq!(
            s.tick_time(Tick(10)),
            HostInstant::from_nanos(766_666_666 + 266_666_666)
        );
        assert_eq!(s.poll(HostInstant::from_nanos(1_033_333_331)).count, 0);
        assert_eq!(s.poll(HostInstant::from_nanos(1_033_333_332)).count, 1);
        Ok(())
    }

    #[test]
    fn stepper_tick_count_is_independent_of_poll_frequency() -> TestResult {
        // Poll at an irregular "frame rate"; the number of ticks over a span depends only
        // on the span. This is the frame-rate / tick-rate decoupling.
        let mut fast = FixedStepper::new(rate(30)?, Tick(0), HostInstant::ZERO, 1000);
        let mut slow = FixedStepper::new(rate(30)?, Tick(0), HostInstant::ZERO, 1000);
        let mut n_fast = 0u64;
        let mut t = 0u64;
        while t <= 2_000_000_000 {
            n_fast += u64::from(fast.poll(HostInstant::from_nanos(t)).count);
            t += 6_944_444 + (t % 3_000_000); // ~144 Hz with jitter
        }
        let n_slow = u64::from(slow.poll(HostInstant::from_nanos(2_000_000_000)).count);
        assert_eq!(n_slow, 61);
        // The fast loop's last poll may land just before 2 s, so it saw 60 or 61 ticks.
        assert!(n_fast == 60 || n_fast == 61, "{n_fast}");
        let rest = u64::from(fast.poll(HostInstant::from_nanos(2_000_000_000)).count);
        assert_eq!(n_fast + rest, 61);
        Ok(())
    }

    #[test]
    fn until_next_reports_wait() -> TestResult {
        let mut s = FixedStepper::new(rate(10)?, Tick(0), HostInstant::from_nanos(1_000), 4);
        assert_eq!(s.until_next(HostInstant::ZERO), Duration::from_nanos(1_000));
        let _ = s.poll(HostInstant::from_nanos(1_000));
        assert_eq!(
            s.until_next(HostInstant::from_nanos(1_000)),
            Duration::from_millis(100)
        );
        Ok(())
    }

    #[test]
    fn server_timeline_takes_min_offset_and_rises_slowly() -> TestResult {
        let mut tl = ServerTimeline::new(rate(30)?, 4);
        assert!(tl.host_time(Tick(0)).is_none());
        // Server tick 30 (1 s) arrives at host 5 s: offset 4 s.
        tl.observe(Tick(30), HostInstant::from_nanos(5_000_000_000));
        assert_eq!(
            tl.host_time(Tick(30)),
            Some(HostInstant::from_nanos(5_000_000_000))
        );
        // A faster arrival lowers the estimate immediately.
        tl.observe(Tick(60), HostInstant::from_nanos(5_900_000_000));
        assert_eq!(
            tl.host_time(Tick(60)),
            Some(HostInstant::from_nanos(5_900_000_000))
        );
        // A delayed arrival raises it by only 1/16 of the difference.
        tl.observe(Tick(90), HostInstant::from_nanos(7_060_000_000));
        assert_eq!(
            tl.host_time(Tick(90)),
            Some(HostInstant::from_nanos(6_910_000_000))
        );
        Ok(())
    }
}
