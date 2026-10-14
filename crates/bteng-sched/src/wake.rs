//! Interrupting a wait.
//!
//! The scheduler's default state is asleep until the next deadline. Three things
//! must be able to cut that short: an async task settling, an external event, and
//! a write to a slot some region reads. All three go through here.
//!
//! The bus carries **no payload** on purpose. A wake means "look again"; *what*
//! changed is answered by comparing slot versions, which is cheaper than routing
//! identities through a queue and — more importantly — cannot go stale or be
//! dropped. A missed wake costs latency; a missed version bump would cost
//! correctness.

use std::sync::{Condvar, Mutex};

use bteng_core::{Clock, Nanos, Waker};

/// A wake signal shared between the scheduler and whatever wants its attention.
#[derive(Debug, Default)]
pub struct WakeBus {
    /// Bumped by every wake. A counter rather than a flag so a wake that lands
    /// between the caller's check and its wait is not lost.
    generation: Mutex<u64>,
    cv: Condvar,
}

impl WakeBus {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Current generation. Take this *before* checking whatever you are about to
    /// wait on, then pass it to [`WakeBus::wait_until`].
    #[must_use]
    pub fn generation(&self) -> u64 {
        *self.generation.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Block until `deadline`, or until a wake arrives after `since`.
    ///
    /// Returns true if a wake arrived. Taking the generation before the caller's
    /// own check is what closes the lost-wakeup window: a wake between the check
    /// and the wait is already reflected in the counter.
    pub fn wait_until(&self, clock: &dyn Clock, deadline: Nanos, since: u64) -> bool {
        let mut guard = self.generation.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if *guard != since {
                return true;
            }
            let now = clock.now();
            if now >= deadline {
                return false;
            }
            let (g, timeout) = self
                .cv
                .wait_timeout(guard, core::time::Duration::from_nanos(deadline - now))
                .unwrap_or_else(|p| p.into_inner());
            guard = g;
            if timeout.timed_out() && *guard == since {
                return false;
            }
        }
    }
}

impl Waker for WakeBus {
    fn wake(&self) {
        let mut g = self.generation.lock().unwrap_or_else(|p| p.into_inner());
        *g = g.wrapping_add(1);
        // Notify while holding the lock: a waiter that has just re-acquired it
        // and is about to re-check must not miss this bump.
        self.cv.notify_all();
    }
}
