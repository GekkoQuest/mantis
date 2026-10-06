//! Correction-magnitude histogram for the Predictive budget row (plan 17: correction
//! magnitude p99 under 10 cm at 100 ms RTT and 2% loss).
//!
//! Fixed buckets, no allocation after construction, quantiles by bucket upper bound.

/// Bucket width in world units (0.5 cm when one unit is one meter).
pub const BUCKET_WIDTH: f32 = 0.005;
/// Number of finite buckets; magnitudes past `BUCKETS * BUCKET_WIDTH` land in overflow.
pub const BUCKETS: usize = 400;

/// Histogram of correction magnitudes.
#[derive(Clone, Debug)]
pub struct CorrectionHistogram {
    counts: Box<[u64; BUCKETS]>,
    overflow: u64,
    total: u64,
    max: f32,
}

impl Default for CorrectionHistogram {
    fn default() -> Self {
        Self::new()
    }
}

impl CorrectionHistogram {
    /// An empty histogram.
    pub fn new() -> Self {
        Self {
            counts: Box::new([0; BUCKETS]),
            overflow: 0,
            total: 0,
            max: 0.0,
        }
    }

    /// Records one correction magnitude. Every reconciliation should record, including
    /// zero corrections, so the quantile reflects all snapshots.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // Range-checked.
    pub fn record(&mut self, magnitude: f32) {
        let m = if magnitude.is_finite() {
            magnitude.max(0.0)
        } else {
            f32::INFINITY
        };
        self.total = self.total.saturating_add(1);
        if m > self.max {
            self.max = m;
        }
        let b = (m / BUCKET_WIDTH) as usize;
        match self.counts.get_mut(b) {
            Some(c) if m.is_finite() => *c = c.saturating_add(1),
            _ => self.overflow = self.overflow.saturating_add(1),
        }
    }

    /// Samples recorded.
    pub fn total(&self) -> u64 {
        self.total
    }

    /// Largest magnitude recorded.
    pub fn max(&self) -> f32 {
        self.max
    }

    /// Upper bound of the bucket holding quantile `q` in [0, 1]; `None` if empty, and
    /// `f32::INFINITY` if the quantile falls in overflow. The quantile is the sample of
    /// rank `ceil(q * total)` (nearest-rank method).
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    pub fn quantile(&self, q: f64) -> Option<f32> {
        if self.total == 0 {
            return None;
        }
        let q = if q.is_finite() { q.clamp(0.0, 1.0) } else { 1.0 };
        // Rank of the sample at quantile q, 1-based, at least 1.
        let rank = ((q * self.total as f64).ceil() as u64).max(1);
        let mut seen = 0u64;
        for (i, c) in self.counts.iter().enumerate() {
            seen = seen.saturating_add(*c);
            if seen >= rank {
                return Some((i + 1) as f32 * BUCKET_WIDTH);
            }
        }
        Some(f32::INFINITY)
    }

    /// Clears all samples.
    pub fn reset(&mut self) {
        *self.counts = [0; BUCKETS];
        self.overflow = 0;
        self.total = 0;
        self.max = 0.0;
    }
}
