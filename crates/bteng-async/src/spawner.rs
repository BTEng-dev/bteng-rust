//! The two backends that ship here.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker as StdWaker};
use std::thread::{self, JoinHandle};

use bteng_core::{Status, Waker};

use crate::task::{BoxFuture, CancelToken, Job, Outcome, Spawner, TaskHandle, TaskId, TaskSlot};

/// Shared plumbing: run a job, catching a panic, and settle its slot.
fn run_and_settle(
    job: Job,
    token: &CancelToken,
    slot: &Arc<TaskSlot>,
    waker: Option<&Arc<dyn Waker>>,
) {
    let outcome = match catch_unwind(AssertUnwindSafe(|| job(token))) {
        Ok(status) => {
            if token.is_cancelled() && status == Status::Running {
                Outcome::Cancelled
            } else {
                Outcome::Settled(status)
            }
        }
        // A panicking leaf must settle. Python BTEng learned this the hard way:
        // an escaped exception left the node RUNNING forever with nothing logged.
        Err(_) => Outcome::Panicked,
    };
    slot.settle(outcome);
    if let Some(w) = waker {
        w.wake();
    }
}

type Work = (Job, CancelToken, Arc<TaskSlot>, Option<Arc<dyn Waker>>);

/// A fixed pool of worker threads.
///
/// Threads live for the pool's lifetime, so nothing on the tick path ever pays
/// for thread creation.
pub struct ThreadSpawner {
    tx: Mutex<Option<Sender<Work>>>,
    workers: Mutex<Vec<JoinHandle<()>>>,
    next_id: AtomicU64,
    pending: Arc<AtomicUsize>,
}

impl std::fmt::Debug for ThreadSpawner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThreadSpawner")
            .field("pending", &self.pending())
            .finish_non_exhaustive()
    }
}

impl ThreadSpawner {
    /// # Panics
    /// If `threads` is zero.
    #[must_use]
    pub fn new(threads: usize) -> Self {
        assert!(threads > 0, "ThreadSpawner needs at least one worker");
        let (tx, rx) = channel::<Work>();
        let rx = Arc::new(Mutex::new(rx));
        let pending = Arc::new(AtomicUsize::new(0));

        let workers = (0..threads)
            .map(|i| {
                let rx: Arc<Mutex<Receiver<Work>>> = Arc::clone(&rx);
                let pending = Arc::clone(&pending);
                thread::Builder::new()
                    .name(format!("bteng-worker-{i}"))
                    .spawn(move || {
                        loop {
                            let next = {
                                let guard = rx.lock().unwrap_or_else(|p| p.into_inner());
                                guard.recv()
                            };
                            let Ok((job, token, slot, waker)) = next else {
                                break;
                            };
                            run_and_settle(job, &token, &slot, waker.as_ref());
                            pending.fetch_sub(1, Ordering::AcqRel);
                        }
                    })
                    .expect("spawn worker")
            })
            .collect();

        Self {
            tx: Mutex::new(Some(tx)),
            workers: Mutex::new(workers),
            next_id: AtomicU64::new(0),
            pending,
        }
    }

    /// Stop accepting work and join the workers.
    ///
    /// Call this at shutdown, never from a tick: it blocks by design, which is
    /// exactly why nothing on the tick path is allowed to reach it.
    pub fn shutdown(&self) {
        if let Ok(mut tx) = self.tx.lock() {
            tx.take();
        }
        if let Ok(mut workers) = self.workers.lock() {
            for w in workers.drain(..) {
                let _ = w.join();
            }
        }
    }
}

impl Drop for ThreadSpawner {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl Spawner for ThreadSpawner {
    fn spawn(&self, job: Job, waker: Option<Arc<dyn Waker>>) -> TaskHandle {
        let id = TaskId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let token = CancelToken::new();
        let slot = Arc::new(TaskSlot::new());
        self.pending.fetch_add(1, Ordering::AcqRel);

        let sent = self.tx.lock().ok().and_then(|tx| {
            tx.as_ref()
                .map(|tx| tx.send((job, token.clone(), Arc::clone(&slot), waker.clone())))
        });

        if !matches!(sent, Some(Ok(()))) {
            // The pool is shut down. Settling immediately beats handing back a
            // task that will never finish and a node that will never stop
            // polling it.
            self.pending.fetch_sub(1, Ordering::AcqRel);
            slot.settle(Outcome::Panicked);
            if let Some(w) = waker {
                w.wake();
            }
        }

        TaskHandle::new(id, token, slot)
    }

    fn spawn_future(&self, fut: BoxFuture, waker: Option<Arc<dyn Waker>>) -> Option<TaskHandle> {
        // A worker thread can drive a future by blocking on it — no runtime
        // needed for the single-future case. A backend with a real reactor
        // (tokio) implements this properly; this keeps node code identical
        // either way rather than making every node branch on the backend.
        Some(self.spawn(Box::new(move |_token| block_on(fut)), waker))
    }

    fn pending(&self) -> usize {
        self.pending.load(Ordering::Acquire)
    }
}

/// Minimal `block_on`: park the thread until the future is ready.
///
/// Enough for one future per worker thread, which is all
/// [`ThreadSpawner::spawn_future`] promises.
fn block_on(mut fut: BoxFuture) -> Status {
    struct ThreadWaker(thread::Thread);
    impl Wake for ThreadWaker {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.unpark();
        }
    }

    let waker = StdWaker::from(Arc::new(ThreadWaker(thread::current())));
    let mut cx = Context::from_waker(&waker);
    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(status) => return status,
            Poll::Pending => thread::park(),
        }
    }
}

/// Runs work on the calling thread, immediately.
///
/// Not a toy: it makes an async node's behaviour deterministic, which is what
/// lets scheduler and replay tests cover the async path at all. A task spawned
/// here is already settled when `spawn` returns, so a node using it goes
/// `Running` for exactly one tick.
#[derive(Debug, Default)]
pub struct InlineSpawner {
    next_id: AtomicU64,
}

impl InlineSpawner {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl Spawner for InlineSpawner {
    fn spawn(&self, job: Job, waker: Option<Arc<dyn Waker>>) -> TaskHandle {
        let id = TaskId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let token = CancelToken::new();
        let slot = Arc::new(TaskSlot::new());
        run_and_settle(job, &token, &slot, waker.as_ref());
        TaskHandle::new(id, token, slot)
    }

    fn spawn_future(&self, fut: BoxFuture, waker: Option<Arc<dyn Waker>>) -> Option<TaskHandle> {
        Some(self.spawn(Box::new(move |_| block_on(fut)), waker))
    }

    fn pending(&self) -> usize {
        0
    }
}
