//! The async leaf node.

use std::sync::Arc;

use bteng_core::{Cursor, Node, Status, Waker};

use crate::task::{Job, Outcome, Spawner, TaskHandle, TaskState};

/// A node with no ports.
pub const PORT_NONE: &[bteng_core::PortSpec] = &[];

/// The domain half of an async leaf.
///
/// The split is the point: [`AsyncWork::start`] and [`AsyncWork::finish`] run on
/// the tick thread and may touch ports; the [`Job`] they hand over runs
/// elsewhere and may not. Nothing in between can accidentally reach the arena
/// from another thread, because the closure simply does not have it.
pub trait AsyncWork: Send + 'static {
    /// Read ports, capture what the work needs, and hand back the job.
    ///
    /// Runs on the tick thread, once per activation.
    fn start(&mut self, cur: &mut Cursor<'_, '_>) -> Job;

    /// Publish results and decide the node's status.
    ///
    /// Runs on the tick thread, once the task has settled.
    fn finish(&mut self, cur: &mut Cursor<'_, '_>, outcome: Outcome) -> Status {
        let _ = cur;
        match outcome {
            Outcome::Settled(s) => s,
            // Cancelled means the tree asked it to stop; reporting Failure is
            // the honest answer for a node that did not complete its work.
            Outcome::Cancelled | Outcome::Panicked => Status::Failure,
        }
    }

    fn type_name(&self) -> &'static str {
        "AsyncAction"
    }
}

/// Runs [`AsyncWork`] off the tick thread, reporting `Running` until it settles.
///
/// Halting is non-blocking by construction: it cancels and sets the handle
/// aside. The tick thread never joins a worker, so a wedged leaf costs the tree
/// nothing but a refusal to restart until it unwinds.
pub struct AsyncAction<W: AsyncWork> {
    work: W,
    spawner: Arc<dyn Spawner>,
    waker: Option<Arc<dyn Waker>>,
    running: Option<TaskHandle>,
    /// A cancelled task that has not unwound yet.
    ///
    /// Python BTEng needed the same guard and paid for it with a blocking grace
    /// period plus a special case for being halted from the loop thread it was
    /// waiting on. Keeping it node-local costs one `Option`.
    unwinding: Option<TaskHandle>,
}

impl<W: AsyncWork> std::fmt::Debug for AsyncAction<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AsyncAction")
            .field("type", &self.work.type_name())
            .field("running", &self.running.is_some())
            .field("unwinding", &self.unwinding.is_some())
            .finish_non_exhaustive()
    }
}

impl<W: AsyncWork> AsyncAction<W> {
    pub fn new(work: W, spawner: Arc<dyn Spawner>) -> Self {
        Self {
            work,
            spawner,
            waker: None,
            running: None,
            unwinding: None,
        }
    }

    /// Wake this waker when a task settles.
    ///
    /// Without it the node still works, but the result waits for the next
    /// scheduled tick — which is precisely Python BTEng's one-period completion
    /// latency, reintroduced.
    #[must_use]
    pub fn with_waker(mut self, waker: Arc<dyn Waker>) -> Self {
        self.waker = Some(waker);
        self
    }

    /// True while a previous activation is still unwinding.
    #[must_use]
    pub fn is_unwinding(&self) -> bool {
        self.unwinding.is_some()
    }
}

impl<W: AsyncWork> Node for AsyncAction<W> {
    fn tick(&mut self, cur: &mut Cursor<'_, '_>) -> Status {
        // Never two copies of the same work at once.
        if let Some(h) = &self.unwinding {
            if h.is_done() {
                self.unwinding = None;
            } else {
                return Status::Running;
            }
        }

        let Some(handle) = &self.running else {
            let job = self.work.start(cur);
            self.running = Some(self.spawner.spawn(job, self.waker.clone()));
            return Status::Running;
        };

        match handle.state() {
            TaskState::Pending => Status::Running,
            TaskState::Done(outcome) => {
                self.running = None;
                self.work.finish(cur, outcome)
            }
        }
    }

    fn halt(&mut self, _cur: &mut Cursor<'_, '_>) {
        let Some(handle) = self.running.take() else {
            return;
        };
        handle.cancel();
        if !handle.is_done() {
            // Set aside, not waited on. This single line is the whole difference
            // from a 5-second join on the tick thread.
            self.unwinding = Some(handle);
        }
    }

    fn type_name(&self) -> &'static str {
        self.work.type_name()
    }
}
