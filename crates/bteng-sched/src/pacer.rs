//! Where time comes from, and how the scheduler waits for it.
//!
//! Two implementations that differ in exactly one thing: whether waiting costs
//! wall-clock time.
//!
//! * [`RealTime`] sleeps. Its whole job is to resume as close to an absolute
//!   deadline as the host allows, and to report honestly how close that was.
//! * [`VirtualTime`] jumps the clock forward instead. A run under it is
//!   deterministic and instantaneous, which is what makes scheduler behaviour
//!   testable and, later (M6), replayable tick-for-tick.
//!
//! Both hand the scheduler a `&dyn Clock`, so nothing downstream knows which one
//! it is running under.

use core::time::Duration;

use std::sync::Arc;

use bteng_core::{Clock, Nanos, SimClock};

use crate::wake::WakeBus;

/// A time source the scheduler can wait on.
pub trait Pacer {
    /// The clock the tree sees. Must agree with [`Pacer::now`].
    fn clock(&self) -> &dyn Clock;

    fn now(&self) -> Nanos {
        self.clock().now()
    }

    /// Wait until `deadline`, returning the time actually resumed at.
    ///
    /// Returning the *actual* resume time rather than the deadline is what lets
    /// the scheduler measure its own lateness instead of assuming it was on
    /// time.
    fn wait_until(&self, deadline: Nanos) -> Nanos;

    /// Wait until `deadline`, or until something wakes the scheduler.
    ///
    /// Defaults to [`Pacer::wait_until`] — a pacer with no wake source simply
    /// cannot be interrupted, and event-driven regions then fall back to their
    /// heartbeat. That degrades latency, never correctness.
    fn wait_or_wake(&self, deadline: Nanos) -> Nanos {
        self.wait_until(deadline)
    }

    /// True when waiting costs no wall-clock time, so a run is deterministic.
    fn is_virtual(&self) -> bool {
        false
    }
}

/// How [`RealTime`] gives back the CPU while it waits.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SleepStrategy {
    /// Hand the whole wait to the OS. Cheapest; the OS decides how late you are,
    /// and on a stock Linux kernel that is typically tens to hundreds of µs.
    Yield,
    /// Sleep to `deadline - slack`, then busy-wait the rest.
    ///
    /// Burns one core for `slack`, and buys back the OS's wake-up granularity.
    /// The right knob when a region's period is close to that granularity.
    SpinTail { slack: Duration },
    /// Busy-wait the entire time. For a pinned thread on a dedicated core, and
    /// nothing else.
    Spin,
}

impl SleepStrategy {
    /// A reasonable default: sleep most of the way, spin the last 200 µs.
    #[must_use]
    pub const fn hybrid() -> Self {
        SleepStrategy::SpinTail {
            slack: Duration::from_micros(200),
        }
    }
}

/// Wall-clock pacing.
#[derive(Debug)]
pub struct RealTime<C: Clock> {
    clock: C,
    strategy: SleepStrategy,
    wake: Option<Arc<WakeBus>>,
}

impl<C: Clock> RealTime<C> {
    pub const fn new(clock: C, strategy: SleepStrategy) -> Self {
        Self {
            clock,
            strategy,
            wake: None,
        }
    }

    /// Let this bus interrupt a wait.
    ///
    /// Without one, an event-driven region can only react as fast as its
    /// heartbeat: the scheduler is asleep and nothing can tell it otherwise.
    #[must_use]
    pub fn with_wake_bus(mut self, bus: Arc<WakeBus>) -> Self {
        self.wake = Some(bus);
        self
    }

    pub fn strategy(&self) -> SleepStrategy {
        self.strategy
    }

    #[must_use]
    pub fn wake_bus(&self) -> Option<&Arc<WakeBus>> {
        self.wake.as_ref()
    }
}

impl<C: Clock> Pacer for RealTime<C> {
    fn clock(&self) -> &dyn Clock {
        &self.clock
    }

    fn wait_or_wake(&self, deadline: Nanos) -> Nanos {
        let Some(bus) = &self.wake else {
            return self.wait_until(deadline);
        };
        // Take the generation before waiting: a wake that lands between now and
        // the wait must not be lost.
        let since = bus.generation();
        if bus.wait_until(&self.clock, deadline, since) {
            return self.clock.now();
        }
        // Timed out. Spin out the residue the same way an uninterrupted wait
        // would, so an event-driven region is no less punctual than a periodic
        // one when nothing happens.
        let mut now = self.clock.now();
        while now < deadline {
            core::hint::spin_loop();
            now = self.clock.now();
        }
        now
    }

    fn wait_until(&self, deadline: Nanos) -> Nanos {
        let now = self.clock.now();
        if now >= deadline {
            // Already late. Returning immediately — rather than sleeping a
            // period — is what lets the overrun policy see the lateness and
            // decide, instead of the pacer quietly papering over it.
            return now;
        }

        match self.strategy {
            SleepStrategy::Yield => sleep_for(deadline - now),
            SleepStrategy::Spin => {}
            SleepStrategy::SpinTail { slack } => {
                let slack = slack.as_nanos() as u64;
                let remaining = deadline - now;
                if remaining > slack {
                    sleep_for(remaining - slack);
                }
            }
        }

        // Spin out whatever is left. `Yield` normally leaves nothing, but an
        // early wake-up is allowed and this is where it gets absorbed.
        let mut now = self.clock.now();
        while now < deadline {
            core::hint::spin_loop();
            now = self.clock.now();
        }
        now
    }
}

fn sleep_for(nanos: Nanos) {
    std::thread::sleep(Duration::from_nanos(nanos));
}

/// Simulated pacing: waiting jumps the clock instead of costing time.
///
/// A scheduler run under this is deterministic — same inputs, same sequence of
/// region ticks, same timestamps, every time and on every architecture. That is
/// what scheduler tests assert on, and what M6's replay will need.
#[derive(Debug, Default)]
pub struct VirtualTime {
    clock: SimClock,
}

impl VirtualTime {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            clock: SimClock::new(),
        }
    }

    #[must_use]
    pub const fn starting_at(now: Nanos) -> Self {
        Self {
            clock: SimClock::starting_at(now),
        }
    }

    /// Move time on without ticking anything — for tests that want to create a
    /// deliberate overrun.
    pub fn advance(&self, delta: Nanos) {
        self.clock.advance(delta);
    }

    #[must_use]
    pub fn sim_clock(&self) -> &SimClock {
        &self.clock
    }
}

impl Pacer for VirtualTime {
    fn clock(&self) -> &dyn Clock {
        &self.clock
    }

    fn wait_until(&self, deadline: Nanos) -> Nanos {
        let now = self.clock.now();
        if deadline > now {
            self.clock.set(deadline);
            return deadline;
        }
        now
    }

    fn is_virtual(&self) -> bool {
        true
    }
}
