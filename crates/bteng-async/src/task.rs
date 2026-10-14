//! Task handles, cancellation, and the spawner contract.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use bteng_core::{Status, Waker};

/// A unit of blocking work. Gets a cancel token; must return a [`Status`].
pub type Job = Box<dyn FnOnce(&CancelToken) -> Status + Send + 'static>;

/// A unit of async work, for spawners that can drive a future.
pub type BoxFuture = Pin<Box<dyn Future<Output = Status> + Send + 'static>>;

/// Identifies a spawned task. Monotonic per spawner; never reused.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TaskId(pub u64);

/// Cooperative cancellation.
///
/// Dropping a future is Rust's own cancellation, and it is abrupt: cleanup that
/// needs to run does not. A flag the work polls lets it unwind properly, and
/// costs one relaxed load per check.
#[derive(Clone, Debug, Default)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
}

impl CancelToken {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask the work to stop. Idempotent, never blocks.
    pub fn cancel(&self) {
        // Release: everything this thread did before deciding to cancel must be
        // visible to the worker that observes the flag. On x86 this is free; on
        // aarch64 it is the difference between correct and "works on my laptop"
        // (DR-2 §3.1).
        self.flag.store(true, Ordering::Release);
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }
}

// State encoding for the result slot. A single atomic rather than a mutex, so
// polling from the tick thread is wait-free.
const PENDING: u8 = 0;
const SUCCESS: u8 = 1;
const FAILURE: u8 = 2;
const CANCELLED: u8 = 3;
const PANICKED: u8 = 4;

/// How a task ended.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Settled(Status),
    /// The work observed its cancel token and stopped.
    Cancelled,
    /// The work panicked. Reported rather than swallowed: a leaf that panics and
    /// then sits `Running` forever is a far worse failure than a `Failure`.
    Panicked,
}

/// What a handle currently knows.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TaskState {
    Pending,
    Done(Outcome),
}

#[derive(Debug)]
pub(crate) struct TaskSlot {
    state: AtomicU8,
}

impl TaskSlot {
    pub(crate) fn new() -> Self {
        Self {
            state: AtomicU8::new(PENDING),
        }
    }

    pub(crate) fn settle(&self, outcome: Outcome) {
        let code = match outcome {
            Outcome::Settled(Status::Success) => SUCCESS,
            Outcome::Settled(Status::Failure) => FAILURE,
            // A worker that returns `Running` has not finished anything; treat it
            // as a failure rather than letting the node poll forever.
            Outcome::Settled(Status::Running) => FAILURE,
            Outcome::Cancelled => CANCELLED,
            Outcome::Panicked => PANICKED,
        };
        // Release pairs with the Acquire in `state()`: the reader must see every
        // write the work made before it settled, including anything it published
        // to the blackboard through a captured handle.
        self.state.store(code, Ordering::Release);
    }

    pub(crate) fn state(&self) -> TaskState {
        match self.state.load(Ordering::Acquire) {
            PENDING => TaskState::Pending,
            SUCCESS => TaskState::Done(Outcome::Settled(Status::Success)),
            FAILURE => TaskState::Done(Outcome::Settled(Status::Failure)),
            CANCELLED => TaskState::Done(Outcome::Cancelled),
            _ => TaskState::Done(Outcome::Panicked),
        }
    }
}

/// A running task.
///
/// Dropping a handle does **not** cancel the task — cancellation is explicit, so
/// that a node halting can hand the handle aside and let the work unwind on its
/// own time instead of the tick thread's.
#[derive(Debug, Clone)]
pub struct TaskHandle {
    id: TaskId,
    cancel: CancelToken,
    slot: Arc<TaskSlot>,
}

impl TaskHandle {
    pub(crate) fn new(id: TaskId, cancel: CancelToken, slot: Arc<TaskSlot>) -> Self {
        Self { id, cancel, slot }
    }

    #[must_use]
    pub fn id(&self) -> TaskId {
        self.id
    }

    /// Poll without blocking. This is the only thing the tick thread ever does
    /// to a task.
    #[must_use]
    pub fn state(&self) -> TaskState {
        self.slot.state()
    }

    #[must_use]
    pub fn is_done(&self) -> bool {
        matches!(self.state(), TaskState::Done(_))
    }

    /// Ask the work to stop. Returns immediately.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    #[must_use]
    pub fn token(&self) -> &CancelToken {
        &self.cancel
    }
}

/// Somewhere to run work that must not run on the tick thread.
pub trait Spawner: Send + Sync {
    /// Run `job` off the tick thread.
    ///
    /// `waker` is called once, when the job settles. That call is what lets a
    /// finished task be noticed within scheduler latency instead of within a
    /// tick period.
    fn spawn(&self, job: Job, waker: Option<Arc<dyn Waker>>) -> TaskHandle;

    /// Run a future, for backends that can drive one.
    ///
    /// Returns `None` when this spawner cannot — the caller then falls back to
    /// [`Spawner::spawn`] with a blocking closure. Having the fallback in the
    /// trait rather than in every node is what keeps node code identical across
    /// backends.
    fn spawn_future(&self, fut: BoxFuture, waker: Option<Arc<dyn Waker>>) -> Option<TaskHandle> {
        let _ = (fut, waker);
        None
    }

    /// Tasks that have not settled yet. Diagnostics and shutdown.
    fn pending(&self) -> usize;
}
