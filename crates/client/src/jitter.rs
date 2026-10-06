//! The jitter buffer: the interpolation delay remote entities are shown behind render
//! time, derived from how snapshots actually arrive instead of a fixed setting.
//!
//! Every applied snapshot reports its **lateness**: how far its arrival trails the
//! [`crate::time::ServerTimeline`] estimate (the fastest recent arrival). Every tick the
//! buffer also sees the timeline's estimate, and from it the estimate's **drift**: how far it
//! rose over the last second (a route change, or the estimate catching up with one). The target
//! delay is
//!
//! ```text
//! target = 2 snapshot intervals + p95(lateness over the last 64 snapshots) + drift + margin
//! ```
//!
//! clamped to `[floor, ceiling]`. Two snapshot intervals cover the spacing of samples and
//! the sim thread's publish cadence; the lateness percentile covers jitter; the drift term
//! keeps the delay up while the timeline is still moving. A re-anchor of the timeline (a sustained
//! shift or a resume absorbed at once) clears the drift history: that jump is not drift.
//! The delay never exceeds what the remote histories cover
//! ([`crate::sim::REMOTE_HISTORY`] samples), whatever the ceiling.
//!
//! **Hysteresis.** The delay moves toward the target at a bounded slew (so remote motion
//! is never visibly rewound or skipped): up at [`DelayConfig::rise_per_tick`] as soon as
//! the target is above it, down at [`DelayConfig::fall_per_tick`] only after the target
//! has stayed below it by more than [`DelayConfig::hysteresis`] for
//! [`DelayConfig::settle`]; once falling, it continues down to the target. A burst
//! raises the delay quickly; it comes back down slowly and only once the link is calm.
//!
//! All state is fixed-size; nothing allocates after construction.

use std::time::Duration;

/// Snapshots the lateness percentile is taken over.
pub const LATENESS_WINDOW: usize = 64;
/// Ticks the drift is measured over (about a second at 30 Hz).
pub const DRIFT_WINDOW: usize = 32;

/// How the interpolation delay is chosen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DelayConfig {
    /// Adapt the delay to the link (`false`: always [`DelayConfig::floor`]).
    pub adaptive: bool,
    /// The smallest delay (the delay on a clean link).
    pub floor: Duration,
    /// The largest delay.
    pub ceiling: Duration,
    /// Added on top of the measured need.
    pub margin: Duration,
    /// How far the delay may rise in one tick.
    pub rise_per_tick: Duration,
    /// How far the delay may fall in one tick.
    pub fall_per_tick: Duration,
    /// How far below the delay the target must be before it may fall.
    pub hysteresis: Duration,
    /// How long the target must stay that far below before the delay falls.
    pub settle: Duration,
}

impl DelayConfig {
    /// A fixed delay of `d` (no adaptation).
    pub const fn fixed(d: Duration) -> Self {
        Self {
            adaptive: false,
            floor: d,
            ceiling: d,
            margin: Duration::ZERO,
            rise_per_tick: Duration::ZERO,
            fall_per_tick: Duration::ZERO,
            hysteresis: Duration::ZERO,
            settle: Duration::ZERO,
        }
    }

    /// An adaptive delay between `floor` and `ceiling` with `margin`, rising at up to
    /// 4 ms per tick and falling at 1 ms per tick after 2 s below by more than 15 ms.
    pub const fn adaptive(floor: Duration, ceiling: Duration, margin: Duration) -> Self {
        Self {
            adaptive: true,
            floor,
            ceiling,
            margin,
            rise_per_tick: Duration::from_millis(4),
            fall_per_tick: Duration::from_millis(1),
            hysteresis: Duration::from_millis(15),
            settle: Duration::from_secs(2),
        }
    }
}

impl Default for DelayConfig {
    /// Adaptive between 100 ms and 400 ms with a 10 ms margin.
    fn default() -> Self {
        Self::adaptive(
            Duration::from_millis(100),
            Duration::from_millis(400),
            Duration::from_millis(10),
        )
    }
}

/// What the buffer measured and chose, for diagnostics, metrics, and tests.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct JitterStats {
    /// The delay in use.
    pub delay: Duration,
    /// The largest delay used so far.
    pub max_delay: Duration,
    /// The delay the measurements ask for (before slew and hysteresis).
    pub target: Duration,
    /// 95th percentile lateness over the window.
    pub lateness_p95: Duration,
    /// How far the timeline estimate moved over the drift window.
    pub drift: Duration,
    /// Ticks the delay rose.
    pub rises: u64,
    /// Ticks the delay fell.
    pub falls: u64,
}

/// The jitter buffer (sim thread).
#[derive(Clone, Debug)]
pub struct JitterBuffer {
    config: DelayConfig,
    interval_nanos: u64,
    lateness: [u32; LATENESS_WINDOW],
    lateness_len: usize,
    lateness_next: usize,
    offsets: [i64; DRIFT_WINDOW],
    offsets_len: usize,
    offsets_next: usize,
    delay_nanos: u64,
    below_nanos: u64,
    /// The deepest delay the remote histories can serve.
    cover_nanos: u64,
    stats: JitterStats,
}

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

impl JitterBuffer {
    /// A buffer for snapshots arriving every `interval` (one server tick), starting at
    /// the floor. `cover` is the deepest delay the remote histories can serve (the delay
    /// never exceeds it, whatever the ceiling: past it, remotes would freeze at their
    /// oldest sample).
    pub fn new(config: DelayConfig, interval: Duration, cover: Duration) -> Self {
        let floor = nanos(config.floor);
        Self {
            config,
            interval_nanos: nanos(interval),
            lateness: [0; LATENESS_WINDOW],
            lateness_len: 0,
            lateness_next: 0,
            offsets: [0; DRIFT_WINDOW],
            offsets_len: 0,
            offsets_next: 0,
            delay_nanos: floor,
            below_nanos: 0,
            cover_nanos: nanos(cover).max(floor),
            stats: JitterStats {
                delay: config.floor,
                max_delay: config.floor,
                ..JitterStats::default()
            },
        }
    }

    /// The configuration.
    pub fn config(&self) -> &DelayConfig {
        &self.config
    }

    /// The delay in use.
    pub fn delay(&self) -> Duration {
        Duration::from_nanos(self.delay_nanos)
    }

    /// Measurements and choices so far.
    pub fn stats(&self) -> JitterStats {
        self.stats
    }

    /// The timeline jumped (a sustained shift or a resume was absorbed at once): its
    /// movement so far is not drift the delay has to cover.
    pub fn reanchored(&mut self) {
        self.offsets_len = 0;
        self.offsets_next = 0;
    }

    /// One applied snapshot arrived `lateness` after the timeline's estimate of its
    /// fastest possible arrival (0 when it set the estimate).
    pub fn observe_lateness(&mut self, lateness: Duration) {
        let micros = u32::try_from(lateness.as_micros()).unwrap_or(u32::MAX);
        if let Some(slot) = self.lateness.get_mut(self.lateness_next) {
            *slot = micros;
        }
        self.lateness_next = (self.lateness_next + 1) % LATENESS_WINDOW;
        self.lateness_len = (self.lateness_len + 1).min(LATENESS_WINDOW);
    }

    fn lateness_p95(&self) -> u64 {
        let mut sorted = [0u32; LATENESS_WINDOW];
        let n = self.lateness_len;
        let (Some(dst), Some(src)) = (sorted.get_mut(..n), self.lateness.get(..n)) else {
            return 0;
        };
        dst.copy_from_slice(src);
        dst.sort_unstable();
        // Nearest-rank 95th percentile.
        let rank = (n * 95).div_ceil(100).max(1) - 1;
        dst.get(rank).map_or(0, |m| u64::from(*m) * 1000)
    }

    /// How far the estimate rose over the window (newest minus oldest; a fall or a
    /// round trip counts as none).
    fn drift(&self) -> u64 {
        if self.offsets_len == 0 {
            return 0;
        }
        let oldest = if self.offsets_len < DRIFT_WINDOW {
            0
        } else {
            self.offsets_next
        };
        let newest = (self.offsets_next + DRIFT_WINDOW - 1) % DRIFT_WINDOW;
        match (self.offsets.get(oldest), self.offsets.get(newest)) {
            (Some(lo), Some(hi)) => u64::try_from(hi.saturating_sub(*lo)).unwrap_or(0),
            _ => 0,
        }
    }

    /// One tick: records the timeline's offset estimate (`None` before the first
    /// snapshot), recomputes the target, and moves the delay toward it. Returns the
    /// delay to use this tick.
    pub fn tick(&mut self, timeline_offset_nanos: Option<i64>) -> Duration {
        if let Some(off) = timeline_offset_nanos {
            if let Some(slot) = self.offsets.get_mut(self.offsets_next) {
                *slot = off;
            }
            self.offsets_next = (self.offsets_next + 1) % DRIFT_WINDOW;
            self.offsets_len = (self.offsets_len + 1).min(DRIFT_WINDOW);
        }
        let c = self.config;
        let floor = nanos(c.floor);
        let ceiling = nanos(c.ceiling).max(floor).min(self.cover_nanos);
        if !c.adaptive {
            self.delay_nanos = floor;
            self.stats.delay = c.floor;
            return c.floor;
        }
        let p95 = self.lateness_p95();
        let drift = self.drift();
        let target = self
            .interval_nanos
            .saturating_mul(2)
            .saturating_add(p95)
            .saturating_add(drift)
            .saturating_add(nanos(c.margin))
            .clamp(floor, ceiling);
        let current = self.delay_nanos;
        if target > current {
            self.below_nanos = 0;
            self.delay_nanos = current.saturating_add(nanos(c.rise_per_tick).max(1)).min(target);
            self.stats.rises += 1;
        } else if current - target > nanos(c.hysteresis)
            || (current > target && self.below_nanos >= nanos(c.settle))
        {
            self.below_nanos = self.below_nanos.saturating_add(self.interval_nanos);
            if self.below_nanos >= nanos(c.settle) {
                self.delay_nanos = current.saturating_sub(nanos(c.fall_per_tick).max(1)).max(target);
                self.stats.falls += 1;
            }
        } else {
            self.below_nanos = 0;
        }
        let delay = Duration::from_nanos(self.delay_nanos);
        self.stats.delay = delay;
        self.stats.max_delay = self.stats.max_delay.max(delay);
        self.stats.target = Duration::from_nanos(target);
        self.stats.lateness_p95 = Duration::from_nanos(p95);
        self.stats.drift = Duration::from_nanos(drift);
        delay
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TICK: Duration = Duration::from_nanos(33_333_333);

    fn ms(d: Duration) -> u64 {
        u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
    }

    #[test]
    fn a_clean_link_stays_at_the_floor() {
        let mut j = JitterBuffer::new(DelayConfig::default(), TICK, Duration::from_secs(1));
        for _ in 0..300 {
            j.observe_lateness(Duration::ZERO);
            assert_eq!(j.tick(Some(1_000)), Duration::from_millis(100));
        }
        assert_eq!(j.stats().rises, 0);
    }

    #[test]
    fn jitter_raises_the_delay_at_a_bounded_slew_and_it_falls_back_slowly() {
        let mut j = JitterBuffer::new(DelayConfig::default(), TICK, Duration::from_secs(1));
        let mut previous = j.delay();
        // 80 ms of lateness on every tenth snapshot: p95 asks for about 80 ms more.
        for i in 0..200u32 {
            j.observe_lateness(Duration::from_millis(if i % 10 == 0 { 80 } else { 0 }));
            let d = j.tick(Some(0));
            assert!(d.saturating_sub(previous) <= Duration::from_millis(4), "slew");
            previous = d;
        }
        let high = ms(j.delay());
        assert!((150..=160).contains(&high), "{high} ms");
        // The link calms: nothing falls until the window has forgotten the bursts and
        // the settle time has passed.
        for _ in 0..LATENESS_WINDOW {
            j.observe_lateness(Duration::ZERO);
            let _ = j.tick(Some(0));
        }
        assert_eq!(ms(j.delay()), high, "held through the settle time");
        for _ in 0..90 {
            j.observe_lateness(Duration::ZERO);
            previous = j.tick(Some(0));
        }
        let falling = ms(j.delay());
        assert!(falling < high, "{falling} after {high}");
        for _ in 0..600 {
            j.observe_lateness(Duration::ZERO);
            let d = j.tick(Some(0));
            assert!(
                previous.saturating_sub(d) <= Duration::from_millis(1),
                "fall slew"
            );
            previous = d;
        }
        assert_eq!(j.delay(), Duration::from_millis(100));
    }

    #[test]
    fn drift_holds_the_delay_up_and_the_ceiling_caps_it() {
        let mut j = JitterBuffer::new(DelayConfig::default(), TICK, Duration::from_secs(1));
        for t in 0..100i64 {
            j.observe_lateness(Duration::ZERO);
            let _ = j.tick(Some(t * 5_000_000)); // 5 ms per tick
        }
        // 32 ticks of 5 ms drift: 155 ms above the floor's need, capped at 400 ms.
        assert!(ms(j.stats().drift) >= 150);
        assert!(ms(j.delay()) > 200);
        assert!(j.delay() <= Duration::from_millis(400));
    }

    #[test]
    fn a_reanchor_is_not_drift_and_the_delay_stays_within_cover() {
        let mut j = JitterBuffer::new(DelayConfig::default(), TICK, Duration::from_millis(250));
        let _ = j.tick(Some(0));
        j.reanchored();
        for _ in 0..100 {
            j.observe_lateness(Duration::ZERO);
            let _ = j.tick(Some(500_000_000));
        }
        assert_eq!(j.stats().drift, Duration::ZERO, "a 500 ms jump after a re-anchor");
        assert_eq!(j.delay(), Duration::from_millis(100));
        // Heavy jitter asks for more than the histories cover: capped at the cover.
        for _ in 0..200 {
            j.observe_lateness(Duration::from_millis(300));
            let _ = j.tick(Some(500_000_000));
        }
        assert_eq!(j.delay(), Duration::from_millis(250));
    }

    #[test]
    fn a_fixed_delay_never_moves() {
        let mut j = JitterBuffer::new(
            DelayConfig::fixed(Duration::from_millis(100)),
            TICK,
            Duration::from_secs(1),
        );
        for _ in 0..100 {
            j.observe_lateness(Duration::from_millis(300));
            assert_eq!(j.tick(Some(0)), Duration::from_millis(100));
        }
    }
}
