//! Async leaves.
//!
//! # What went wrong in the Python engine
//!
//! BTEng has two async mechanisms bolted onto a synchronous core — a thread pool
//! and an asyncio bridge — and both are *poll-only*: a task that finished a
//! microsecond after a tick is not noticed until the next one, so completion
//! latency is one tick period no matter how fast the work was. Worse, halting is
//! blocking: `AsyncActionNode._on_halt` joins with a 5-second timeout, and
//! `CoroActionNode` waits a grace period and then needs an orphan guard because
//! a halt can be issued *from the very loop thread it is waiting on*.
//!
//! # The three properties this has instead
//!
//! 1. **Completion wakes the scheduler.** A finished task calls a
//!    [`Waker`](bteng_core::Waker), so the result is observed within scheduler
//!    latency rather than within a period.
//! 2. **Halt never blocks.** It sets a cancel flag and moves the handle aside.
//!    The tick thread does not join, does not sleep, and does not wait for a
//!    coroutine to unwind. A node refuses to start a second copy while the first
//!    is still unwinding — the orphan problem solved node-locally instead of
//!    with a global grace period.
//! 3. **One mechanism.** Blocking work and futures both go through [`Spawner`];
//!    a node's code is identical either way.
//!
//! # No runtime dependency
//!
//! [`Spawner`] is the seam, and this crate ships [`ThreadSpawner`] and
//! [`InlineSpawner`] only. A tokio backend is a small impl in whichever crate
//! already depends on tokio; vendoring one here would put a large dependency
//! tree in front of every aarch64 and QEMU build for the sake of an example.

mod node;
mod spawner;
mod task;

pub use node::{AsyncAction, AsyncWork, PORT_NONE};
pub use spawner::{InlineSpawner, ThreadSpawner};
pub use task::{BoxFuture, CancelToken, Job, Outcome, Spawner, TaskHandle, TaskId, TaskState};
