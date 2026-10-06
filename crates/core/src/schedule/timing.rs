//! Per-system run times for the Ops inspector (plan 13): diagnostics only,
//! never simulation state. The time source is injected ([`Stopwatch`]); the
//! simulation crates have no clock of their own (principle 8). Recording is
//! allocation-free: each system owns a fixed ring of its last
//! [`TIMING_WINDOW`] durations, sized when the system is added.

/// A monotonic time source for diagnostics, in nanoseconds from any origin.
/// The host supplies one (an OS clock in a tooling crate); without one,
/// nothing is timed.
pub trait Stopwatch: Send + Sync {
    /// Nanoseconds since an arbitrary fixed origin; never decreases.
    fn now_nanos(&self) -> u64;
}

/// Durations each system keeps (its percentile window).
pub const TIMING_WINDOW: usize = 256;

/// The recent run times of one system.
#[derive(Clone, Debug)]
pub struct Timing {
    ring: [u32; TIMING_WINDOW],
    next: usize,
    filled: usize,
    runs: u64,
    last: u32,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            ring: [0; TIMING_WINDOW],
            next: 0,
            filled: 0,
            runs: 0,
            last: 0,
        }
    }
}

impl Timing {
    /// Records one run of `nanos` (saturating at about four seconds).
    /// Allocation-free.
    pub fn record(&mut self, nanos: u64) {
        let n = u32::try_from(nanos).unwrap_or(u32::MAX);
        if let Some(slot) = self.ring.get_mut(self.next) {
            *slot = n;
        }
        self.next = (self.next + 1) % TIMING_WINDOW;
        self.filled = (self.filled + 1).min(TIMING_WINDOW);
        self.runs = self.runs.saturating_add(1);
        self.last = n;
    }

    /// Runs recorded in all.
    #[must_use]
    pub fn runs(&self) -> u64 {
        self.runs
    }

    /// The last run, in nanoseconds.
    #[must_use]
    pub fn last_nanos(&self) -> u32 {
        self.last
    }

    /// The `pct`th percentile (0 to 100) of the window, in nanoseconds; 0
    /// before the first run. Allocation-free (sorts a stack copy).
    #[must_use]
    pub fn percentile_nanos(&self, pct: u32) -> u32 {
        let n = self.filled;
        if n == 0 {
            return 0;
        }
        let mut copy = self.ring;
        let window = copy.get_mut(..n).unwrap_or_default();
        window.sort_unstable();
        // Nearest rank: ceil(pct/100 * n), at least 1.
        let rank = (usize::try_from(pct.min(100)).unwrap_or(100) * n)
            .div_ceil(100)
            .max(1);
        window.get(rank - 1).copied().unwrap_or(0)
    }
}
