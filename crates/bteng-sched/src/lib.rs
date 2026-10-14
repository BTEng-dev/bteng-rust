//! Multi-rate scheduling for BTEng.
//!
//! # The problem this solves
//!
//! Python BTEng has one `tick_interval` for the whole tree, and fakes per-subtree
//! rates with a `RateController` decorator that caches a status and ticks its
//! child less often *inside the parent's tick*. That is not a second rate: the
//! parent's rate is a hard ceiling, so a 100 Hz subtree under a 10 Hz parent runs
//! at 10 Hz. Its loop is also `do_tick(); sleep(interval)`, so the period drifts
//! by however long the tick took, every tick, forever.
//!
//! # The shape here
//!
//! A **region** is a subtree with its own rate. It is marked in the tree by a
//! [`Region`](bteng_core::Builtin::region) node, which is a scheduling boundary:
//! the parent's traversal stops there and reads the last verdict the region
//! published, while the scheduler ticks the region's subtree directly, on the
//! region's own clock. Nesting works — an outer region's traversal stops at an
//! inner region's boundary the same way.
//!
//! Deadlines are **absolute**. The next deadline is computed from the previous
//! deadline, never from "now", so a slow tick costs that one tick's phase and
//! nothing after it. That single choice is the difference between a rate that
//! holds for hours and one that walks away from you.
//!
//! ```text
//!   root  ──── 10 Hz ────────●───────────────●───────────────●
//!     └─ Region(100 Hz) ─●─●─●─●─●─●─●─●─●─●─●─●─●─●─●─●─●─●─●
//! ```
//!
//! # What is not here yet
//!
//! **Regions do not run on separate threads.** One scheduler thread interleaves
//! them, which makes a run deterministic and lets every region share one
//! `Blackboard` with no locking at all. Real cross-core parallelism needs
//! per-region blackboards plus an explicit wait-free link between them, and the
//! memory-ordering work that implies is not something to do the week before
//! there is ARM hardware to test it on (DR-2 §3.1). The region boundary is
//! already the right seam for it.
//!
//! **Event-driven wake-ups are M4.** [`TickPolicy`] is periodic-only for now;
//! `OnChange`/`OnEvent` need the read sets that the blackboard's version
//! counters exist to provide.

mod pacer;
mod scheduler;
mod stats;
mod wake;

pub use pacer::{Pacer, RealTime, SleepStrategy, VirtualTime};
pub use scheduler::{
    MAX_HZ, OverrunPolicy, RunOutcome, SchedError, Scheduler, StepOutcome, TickPolicy,
};
pub use stats::{Histogram, RegionStats};
pub use wake::WakeBus;
