//! Time source abstraction.
//!
//! Time is `u64` nanoseconds, not `Duration`/`Instant`, so the core stays
//! `no_std`-able (DR-3) and so a recorded trace can be replayed bit-identically
//! under [`SimClock`] (DR-4: the choice of timing class stays open, but the
//! *ability* to make time deterministic cannot be retrofitted).

use core::sync::atomic::{AtomicU64, Ordering};

/// Nanoseconds since an arbitrary, monotonic origin.
pub type Nanos = u64;

/// A monotonic time source.
///
/// `Send + Sync` because M2 will share one clock across regions running on
/// different threads.
pub trait Clock: Send + Sync {
    fn now(&self) -> Nanos;
}

/// Wall-clock time from `std::time::Instant`.
#[cfg(feature = "std")]
#[derive(Debug)]
pub struct SteadyClock {
    origin: std::time::Instant,
}

#[cfg(feature = "std")]
impl SteadyClock {
    #[must_use]
    pub fn new() -> Self {
        Self {
            origin: std::time::Instant::now(),
        }
    }
}

#[cfg(feature = "std")]
impl Default for SteadyClock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "std")]
impl Clock for SteadyClock {
    fn now(&self) -> Nanos {
        // u64 nanos overflows ~584 years after process start.
        self.origin.elapsed().as_nanos() as u64
    }
}

/// A clock the test or the replay drives by hand.
///
/// Orderings are `Relaxed` on purpose and that is a *decision*, not an
/// oversight: this counter carries no happens-before relationship for any other
/// data. Every atomic in this codebase gets that justification written down,
/// because aarch64 will not forgive the ones that needed `Acquire`/`Release`
/// and got `Relaxed` by habit (DR-2, §3.1).
#[derive(Debug, Default)]
pub struct SimClock {
    now: AtomicU64,
}

impl SimClock {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            now: AtomicU64::new(0),
        }
    }

    #[must_use]
    pub const fn starting_at(now: Nanos) -> Self {
        Self {
            now: AtomicU64::new(now),
        }
    }

    /// Move time forward by `delta` nanoseconds.
    pub fn advance(&self, delta: Nanos) {
        self.now.fetch_add(delta, Ordering::Relaxed);
    }

    /// Jump to an absolute time.
    pub fn set(&self, now: Nanos) {
        self.now.store(now, Ordering::Relaxed);
    }
}

impl Clock for SimClock {
    fn now(&self) -> Nanos {
        self.now.load(Ordering::Relaxed)
    }
}
