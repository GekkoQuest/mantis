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
#[expect(clippy::cast_possible_truncation)] // Saturated to u64 explicitly.
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
    #[expect(clippy::cast_precision_loss)] // Sub-nanosecond precision is irrelevant here.
    pub fn seconds_since(self, earlier: HostInstant) -> f64 {
        if self.0 >= earlier.0 {
            (self.0 - earlier.0) as f64 * 1e-9
        } else {
            -((earlier.0 - self.0) as f64 * 1e-9)
        }
    }

    /// `self + d`, saturating at the end of the timeline.
    #[must_use]
    #[expect(clippy::cast_possible_truncation)] // Saturated explicitly.
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
    #[expect(clippy::cast_possible_truncation)] // Saturated explicitly.
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
    #[expect(clippy::cast_possible_truncation)] // Saturated explicitly.
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
    #[expect(clippy::cast_possible_truncation)] // Saturated explicitly.
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
        #[expect(clippy::cast_possible_truncation)] // count <= max_catchup, a u32.
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

/// Recent arrivals compared against the earlier ones to detect a sustained shift
/// (about a second at 30 Hz).
pub const SUSTAIN_WINDOW: usize = 32;
/// Earlier arrivals the recent ones are compared against.
pub const SUSTAIN_HISTORY: usize = 64;
/// Ticks without an arrival after which a later arrival re-anchors the timeline at once.
pub const RESUME_GAP_TICKS: u64 = 6;

const SHIFT_RING: usize = SUSTAIN_WINDOW + SUSTAIN_HISTORY;

/// The `pct`th percentile (nearest rank) of `values`, sorted.
fn percentile(sorted: &[i128], pct: usize) -> Option<i128> {
    let rank = (sorted.len() * pct).div_ceil(100).max(1) - 1;
    sorted.get(rank).copied()
}

/// Maps server ticks to host time from snapshot arrivals.
///
/// Each arrival yields an offset sample `arrival - server_tick_time`. Network delay only
/// ever adds to a sample, so the smallest recent sample is the best estimate of the true
/// offset: a lower sample is adopted at once, a higher one is approached slowly
/// (`1 / 2^rise_shift` of the difference per sample) so the estimate tracks clock drift
/// without following jitter.
///
/// **Adaptive rise.** A sustained latency increase is absorbed sooner than that. The
/// timeline keeps the raw offsets of the last [`SUSTAIN_WINDOW`] +
/// [`SUSTAIN_HISTORY`] arrivals; when even the 10th percentile of the recent
/// [`SUSTAIN_WINDOW`] is later than the 90th percentile of the [`SUSTAIN_HISTORY`] before
/// them by more than the threshold, the whole link got slower: the estimate rises to that
/// recent 10th percentile over the next few samples (a quarter of what remains per
/// sample) and the history starts over. Percentiles, not minimums, make it robust both to jitter and
/// to arrivals quantized by a polling loop; a burst shorter than about nine tenths of the
/// recent window never moves the percentile, so a transient only ever gets the slow rise.
///
/// **Resume rule.** The first arrival after a silence of more than [`RESUME_GAP_TICKS`]
/// ticks that is later than the estimate (a server stall, or an outage that came back
/// slower) re-anchors the estimate on it at once.
#[derive(Clone, Debug)]
pub struct ServerTimeline {
    rate: TickRate,
    offset_nanos: Option<i128>,
    rise_shift: u32,
    adaptive: bool,
    threshold_nanos: i128,
    /// Raw offsets of recent arrivals, oldest first.
    raw: [i128; SHIFT_RING],
    raw_len: usize,
    /// A detected shift not yet added to the estimate.
    pending: i128,
    fast_rises: u64,
    /// The last arrival, for the resume rule.
    last_arrival: Option<HostInstant>,
    /// Re-anchors on a resume after a gap.
    resumes: u64,
}

impl ServerTimeline {
    /// A timeline for a server running at `rate`. `rise_shift` sets how slowly the
    /// estimate rises toward later samples: each sample moves it by `1 / 2^rise_shift`
    /// of the difference. The adaptive rise is on, with a 5 ms threshold.
    pub fn new(rate: TickRate, rise_shift: u32) -> Self {
        Self {
            rate,
            offset_nanos: None,
            rise_shift: rise_shift.min(30),
            adaptive: true,
            threshold_nanos: 5_000_000,
            raw: [0; SHIFT_RING],
            raw_len: 0,
            pending: 0,
            fast_rises: 0,
            last_arrival: None,
            resumes: 0,
        }
    }

    /// Turns the adaptive rise on or off and sets the shift it must exceed.
    pub fn set_adaptive_rise(&mut self, on: bool, threshold: core::time::Duration) {
        self.adaptive = on;
        self.threshold_nanos = i128::try_from(threshold.as_nanos()).unwrap_or(i128::MAX);
        self.raw_len = 0;
        self.pending = 0;
    }

    fn remember(&mut self, sample: i128) {
        if self.raw_len == SHIFT_RING {
            self.raw.copy_within(1.., 0);
            self.raw_len -= 1;
        }
        if let Some(slot) = self.raw.get_mut(self.raw_len) {
            *slot = sample;
        }
        self.raw_len += 1;
    }

    /// Where the offset now sits (the recent arrivals' 10th percentile), if even that is
    /// later than the earlier arrivals' 90th percentile by more than the threshold: the
    /// whole link got slower, not just some arrivals.
    fn sustained_shift(&self) -> Option<i128> {
        if !self.adaptive || self.raw_len < SHIFT_RING {
            return None;
        }
        let mut prior = [0i128; SUSTAIN_HISTORY];
        let mut recent = [0i128; SUSTAIN_WINDOW];
        prior.copy_from_slice(self.raw.get(..SUSTAIN_HISTORY)?);
        recent.copy_from_slice(self.raw.get(SUSTAIN_HISTORY..)?);
        prior.sort_unstable();
        recent.sort_unstable();
        let now = percentile(&recent, 10)?;
        (now - percentile(&prior, 90)? > self.threshold_nanos).then_some(now)
    }

    /// Records that `server_tick` arrived at host instant `arrival`. Returns how late it
    /// was: how far it trails the estimate as it stood (zero when it set the estimate).
    #[expect(clippy::cast_possible_wrap)] // tick_start_nanos is far below i128::MAX.
    pub fn observe(&mut self, server_tick: Tick, arrival: HostInstant) -> core::time::Duration {
        let sample = i128::from(arrival.0) - tick_start_nanos(self.rate, server_tick) as i128;
        let gap = self.last_arrival.map(|l| arrival.saturating_since(l));
        self.last_arrival = Some(arrival);
        self.remember(sample);
        let target = self.sustained_shift();
        let Some(cur) = self.offset_nanos else {
            self.offset_nanos = Some(sample);
            return core::time::Duration::ZERO;
        };
        // The resume rule: the first arrival after a silence of more than `RESUME_GAP_TICKS`
        // ticks that is later than the estimate (a server stall or a link outage that came
        // back slower) re-anchors at once. Adopting a slightly late sample is safe: the next
        // faster one lowers the estimate immediately.
        let silence = core::time::Duration::from_nanos(
            u64::try_from(tick_start_nanos(self.rate, Tick(RESUME_GAP_TICKS))).unwrap_or(u64::MAX),
        );
        if self.adaptive && gap.is_some_and(|g| g > silence) && sample - cur > self.threshold_nanos {
            self.offset_nanos = Some(sample);
            self.pending = 0;
            self.raw_len = 0;
            self.resumes += 1;
            return core::time::Duration::ZERO;
        }
        if let Some(target) = target {
            self.pending = (target - cur).max(0);
            self.fast_rises += 1;
            self.raw_len = 0;
        }
        if sample <= cur {
            self.offset_nanos = Some(sample);
            self.pending = 0;
            return core::time::Duration::ZERO;
        }
        let excess = sample - cur;
        let rise = if self.pending > 0 {
            let step = (self.pending >> 2).max(1_000_000).min(self.pending).min(excess);
            self.pending -= step;
            step
        } else {
            (excess >> self.rise_shift).max(1)
        };
        self.offset_nanos = Some(cur + rise);
        core::time::Duration::from_nanos(u64::try_from(excess).unwrap_or(u64::MAX))
    }

    /// The current offset estimate (host time minus server time), in nanoseconds.
    pub fn offset_nanos(&self) -> Option<i64> {
        self.offset_nanos
            .map(|o| i64::try_from(o).unwrap_or(if o < 0 { i64::MIN } else { i64::MAX }))
    }

    /// Sustained shifts detected (each added to the estimate over a few samples).
    pub fn fast_rises(&self) -> u64 {
        self.fast_rises
    }

    /// Re-anchors at the first arrival after a silence (a server stall or an outage).
    pub fn resumes(&self) -> u64 {
        self.resumes
    }

    /// Every re-anchor so far (sustained shifts and resumes): the estimate jumped rather
    /// than drifted.
    pub fn reanchors(&self) -> u64 {
        self.fast_rises + self.resumes
    }

    /// Estimated host instant at which the server produced `server_tick`.
    #[expect(
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
        // Arrivals a second apart: the resume rule would re-anchor; this is the slow rise.
        tl.set_adaptive_rise(false, core::time::Duration::from_millis(5));
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

    #[test]
    fn a_sustained_shift_is_absorbed_sooner_than_a_transient() -> TestResult {
        let r = rate(30)?;
        let at = |tick: u64, extra_ms: u64| {
            HostInstant::from_nanos(
                u64::try_from(tick_start_nanos(r, Tick(tick))).unwrap_or(0) + extra_ms * 1_000_000,
            )
        };
        let offset_ms = |tl: &ServerTimeline| tl.offset_nanos().unwrap_or(0) / 1_000_000;
        let feed = |tl: &mut ServerTimeline, ticks: core::ops::Range<u64>, late: &dyn Fn(u64) -> u64| {
            for t in ticks {
                let _ = tl.observe(Tick(t), at(t, 100 + late(t)));
            }
        };
        // A transient: twenty arrivals 50 ms late among on-time ones (0 to 9 ms jitter)
        // only get the slow rise, then the estimate comes back down.
        let mut tl = ServerTimeline::new(r, 6);
        feed(&mut tl, 0..100, &|t| t % 10);
        feed(&mut tl, 100..120, &|t| 50 + t % 10);
        feed(&mut tl, 120..220, &|t| t % 10);
        assert_eq!(tl.fast_rises(), 0);
        assert_eq!(offset_ms(&tl), 100);
        // Arrivals quantized by a polling loop: nine in ten a whole tick late. Not a shift.
        let mut tl = ServerTimeline::new(r, 6);
        feed(&mut tl, 0..300, &|t| if t % 11 == 0 { 0 } else { 33 });
        assert_eq!(tl.fast_rises(), 0);
        // A sustained +50 ms shift with jitter: detected once and absorbed about a second
        // after it began.
        let mut tl = ServerTimeline::new(r, 6);
        feed(&mut tl, 0..100, &|t| t % 10);
        feed(&mut tl, 100..145, &|t| 50 + t % 10);
        assert_eq!(tl.fast_rises(), 1);
        assert!(offset_ms(&tl) >= 145, "{} ms", offset_ms(&tl));
        // Without the adaptive rise the same shift is far from absorbed by then.
        let mut slow = ServerTimeline::new(r, 6);
        slow.set_adaptive_rise(false, core::time::Duration::from_millis(5));
        feed(&mut slow, 0..100, &|t| t % 10);
        feed(&mut slow, 100..145, &|t| 50 + t % 10);
        assert!(offset_ms(&slow) < 130, "{} ms", offset_ms(&slow));
        Ok(())
    }

    #[test]
    fn a_resume_after_silence_reanchors_at_once() -> TestResult {
        let r = rate(30)?;
        let at = |_tick: u64, host_tick: u64| -> Result<HostInstant, std::num::TryFromIntError> {
            Ok(HostInstant::from_nanos(
                u64::try_from(tick_start_nanos(r, Tick(host_tick)))? + 50_000_000,
            ))
        };
        let mut tl = ServerTimeline::new(r, 6);
        for t in 0..100 {
            let _ = tl.observe(Tick(t), at(t, t)?);
        }
        let before = tl.offset_nanos().unwrap_or(0);
        // The server stalls 15 ticks: tick 100 arrives 15 ticks late, after a silence.
        let _ = tl.observe(Tick(100), at(100, 115)?);
        assert_eq!(tl.resumes(), 1);
        assert_eq!(tl.offset_nanos().unwrap_or(0) - before, 500_000_000);
        // Steady arrivals afterwards keep it there; no fast rise is needed.
        for t in 101..200 {
            let _ = tl.observe(Tick(t), at(t, t + 15)?);
        }
        assert_eq!(tl.offset_nanos().unwrap_or(0) - before, 500_000_000);
        assert_eq!(tl.reanchors(), 1);
        // Without the adaptive rise there is no resume rule either.
        let mut slow = ServerTimeline::new(r, 6);
        slow.set_adaptive_rise(false, core::time::Duration::from_millis(5));
        for t in 0..100 {
            let _ = slow.observe(Tick(t), at(t, t)?);
        }
        let _ = slow.observe(Tick(100), at(100, 115)?);
        assert_eq!(slow.resumes(), 0);
        Ok(())
    }
}
