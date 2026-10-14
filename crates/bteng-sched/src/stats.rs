//! Per-region timing statistics.
//!
//! Everything here is fixed-size and updated in place: the scheduler loop must
//! not allocate, and a histogram that reallocates as it fills would be the one
//! thing in the loop that does (DR-4).
//!
//! Lateness is the number that matters. A region asked for 100 Hz did not get
//! 100 Hz if every tick starts 3 ms after its deadline — the *average period*
//! can be perfect while the phase drifts all over the place, which is why this
//! measures distance from each absolute deadline rather than the gap between
//! consecutive ticks.

use bteng_core::Nanos;

/// Log2 buckets, so one fixed array covers 1 ns to ~1 s without tuning.
const BUCKETS: usize = 32;

/// A fixed-size log2 histogram.
#[derive(Copy, Clone, Debug)]
pub struct Histogram {
    counts: [u32; BUCKETS],
    total: u64,
}

impl Default for Histogram {
    fn default() -> Self {
        Self::new()
    }
}

impl Histogram {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            counts: [0; BUCKETS],
            total: 0,
        }
    }

    pub fn record(&mut self, value: Nanos) {
        let bucket = if value == 0 {
            0
        } else {
            (64 - value.leading_zeros()) as usize
        };
        let bucket = bucket.min(BUCKETS - 1);
        self.counts[bucket] = self.counts[bucket].saturating_add(1);
        self.total += 1;
    }

    #[must_use]
    pub const fn count(&self) -> u64 {
        self.total
    }

    /// Upper bound of the bucket the `q`-quantile falls in.
    ///
    /// Bucketed, so this is an over-estimate by up to 2×, never an under-estimate
    /// — the direction that cannot make a timing claim look better than it is.
    #[must_use]
    pub fn quantile(&self, q: f64) -> Nanos {
        if self.total == 0 {
            return 0;
        }
        let target = (self.total as f64 * q).ceil() as u64;
        let mut seen = 0u64;
        for (i, c) in self.counts.iter().enumerate() {
            seen += u64::from(*c);
            if seen >= target {
                return if i == 0 { 0 } else { 1u64 << i };
            }
        }
        1u64 << (BUCKETS - 1)
    }
}

/// What one region did.
#[derive(Copy, Clone, Debug, Default)]
pub struct RegionStats {
    /// Subtree ticks performed.
    pub ticks: u64,
    /// Deadlines missed by a whole period or more — ticks that were lost.
    ///
    /// Sub-period lateness is not an overrun: counting it made the metric read
    /// 100% on a run whose worst lateness was 158 ns. What that finer lateness
    /// looks like is [`RegionStats::lateness`]'s job.
    pub overruns: u64,
    /// Scheduled ticks dropped by [`crate::OverrunPolicy::Skip`] or `Warn`.
    pub skipped: u64,
    /// Missed ticks fired back-to-back by [`crate::OverrunPolicy::CatchUp`].
    pub caught_up: u64,
    /// Worst lateness seen, in nanoseconds.
    pub max_lateness: Nanos,
    /// Sum of lateness, for the mean.
    pub total_lateness: Nanos,
    /// Longest single subtree tick.
    pub max_tick_duration: Nanos,
    pub total_tick_duration: Nanos,
    /// Distribution of lateness.
    pub lateness: Histogram,
    /// Distribution of subtree tick duration.
    pub duration: Histogram,
}

impl RegionStats {
    pub fn record(&mut self, lateness: Nanos, tick_duration: Nanos) {
        self.ticks += 1;
        self.total_lateness += lateness;
        self.max_lateness = self.max_lateness.max(lateness);
        self.lateness.record(lateness);
        self.total_tick_duration += tick_duration;
        self.max_tick_duration = self.max_tick_duration.max(tick_duration);
        self.duration.record(tick_duration);
    }

    #[must_use]
    pub fn mean_lateness(&self) -> f64 {
        if self.ticks == 0 {
            0.0
        } else {
            self.total_lateness as f64 / self.ticks as f64
        }
    }

    #[must_use]
    pub fn mean_tick_duration(&self) -> f64 {
        if self.ticks == 0 {
            0.0
        } else {
            self.total_tick_duration as f64 / self.ticks as f64
        }
    }

    /// Fraction of deadlines that were missed by a whole period or more.
    #[must_use]
    pub fn overrun_rate(&self) -> f64 {
        if self.ticks == 0 {
            0.0
        } else {
            self.overruns as f64 / self.ticks as f64
        }
    }
}
