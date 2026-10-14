//! Async leaf behaviour — the three properties the Python engine lacks.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use bteng_async::{AsyncAction, AsyncWork, InlineSpawner, Job, Outcome, Spawner, ThreadSpawner};
use bteng_core::{
    Arity, Blackboard, Builtin, Ctx, Cursor, NodeCategory, NodeManifest, PortSpec, SimClock,
    Status, Tree, TreeBuilder, Waker, input, output,
};

static WORK_PORTS: &[PortSpec] = &[
    input::<i64>("input").with_default("0"),
    output::<i64>("result"),
];

static WORK: NodeManifest =
    NodeManifest::new("Work", NodeCategory::Action, Arity::Leaf, WORK_PORTS);

/// Doubles its input, on a worker thread, after `gate` opens.
struct Doubler {
    gate: Arc<AtomicBool>,
    started: Arc<AtomicU32>,
    result: Arc<AtomicU32>,
}

impl AsyncWork for Doubler {
    fn start(&mut self, cur: &mut Cursor<'_, '_>) -> Job {
        // Ports are read here, on the tick thread. The closure gets values, not
        // the arena — which is why nothing can reach the tree from a worker.
        let n = cur.get::<i64>(0).unwrap_or(0);
        let gate = Arc::clone(&self.gate);
        let started = Arc::clone(&self.started);
        let result = Arc::clone(&self.result);
        Box::new(move |token| {
            started.fetch_add(1, Ordering::SeqCst);
            while !gate.load(Ordering::Acquire) {
                if token.is_cancelled() {
                    return Status::Running; // settles as Cancelled
                }
                std::thread::sleep(Duration::from_micros(50));
            }
            result.store((n * 2) as u32, Ordering::SeqCst);
            Status::Success
        })
    }

    fn finish(&mut self, cur: &mut Cursor<'_, '_>, outcome: Outcome) -> Status {
        if let Outcome::Settled(Status::Success) = outcome {
            let v = i64::from(self.result.load(Ordering::SeqCst));
            let _ = cur.set(1, v);
            return Status::Success;
        }
        Status::Failure
    }

    fn type_name(&self) -> &'static str {
        "Work"
    }
}

/// Counts how often it was woken.
#[derive(Debug, Default)]
struct CountingWaker(AtomicU32);

impl Waker for CountingWaker {
    fn wake(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

struct Harness {
    tree: Tree,
    bb: Blackboard,
    clock: SimClock,
}

impl Harness {
    fn tick(&mut self) -> Status {
        let mut ctx = Ctx::new(&mut self.bb, &self.clock);
        self.tree.tick(&mut ctx)
    }
    fn halt(&mut self) {
        let mut ctx = Ctx::new(&mut self.bb, &self.clock);
        self.tree.halt(&mut ctx);
    }
}

fn build(work: Doubler, spawner: Arc<dyn Spawner>, waker: Option<Arc<dyn Waker>>) -> Harness {
    let mut node = AsyncAction::new(work, spawner);
    if let Some(w) = waker {
        node = node.with_waker(w);
    }
    let mut b = TreeBuilder::new();
    b.begin("root", Builtin::sequence())
        .leaf_custom("work", &WORK, Box::new(node))
        .port("input", "21")
        .port("result", "{answer}")
        .end();
    let mut bb = Blackboard::new();
    let graph = b.build(&mut bb).unwrap();
    Harness {
        tree: Tree::new(graph),
        bb,
        clock: SimClock::new(),
    }
}

fn doubler() -> (Doubler, Arc<AtomicBool>, Arc<AtomicU32>) {
    let gate = Arc::new(AtomicBool::new(false));
    let started = Arc::new(AtomicU32::new(0));
    (
        Doubler {
            gate: Arc::clone(&gate),
            started: Arc::clone(&started),
            result: Arc::new(AtomicU32::new(0)),
        },
        gate,
        started,
    )
}

// ── the three properties ─────────────────────────────────────────────────────

#[test]
fn a_task_settling_wakes_the_scheduler() {
    let spawner = Arc::new(ThreadSpawner::new(2));
    let waker = Arc::new(CountingWaker::default());
    let (work, gate, started) = doubler();
    let mut h = build(work, spawner, Some(waker.clone() as Arc<dyn Waker>));

    assert_eq!(h.tick(), Status::Running);
    while started.load(Ordering::SeqCst) == 0 {
        std::thread::yield_now();
    }
    assert_eq!(
        waker.0.load(Ordering::SeqCst),
        0,
        "no wake while the work is still running"
    );

    gate.store(true, Ordering::Release);
    let deadline = Instant::now() + Duration::from_secs(5);
    while waker.0.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
        std::thread::yield_now();
    }
    // This is the fix for Python BTEng's one-period completion latency: the
    // result announces itself instead of waiting to be polled.
    assert_eq!(
        waker.0.load(Ordering::SeqCst),
        1,
        "completion must wake once"
    );

    assert_eq!(h.tick(), Status::Success);
    let slot = h.bb.resolve("answer").unwrap();
    assert_eq!(*h.bb.get::<i64>(slot).unwrap(), 42);
}

#[test]
fn halting_a_wedged_task_does_not_block_the_tick_thread() {
    let spawner = Arc::new(ThreadSpawner::new(2));
    let (work, gate, started) = doubler();
    let mut h = build(work, spawner, None);

    assert_eq!(h.tick(), Status::Running);
    while started.load(Ordering::SeqCst) == 0 {
        std::thread::yield_now();
    }

    // The work is sitting in its loop and will not stop for 50 µs at a time.
    // Python BTEng would join it here for up to five seconds.
    let t0 = Instant::now();
    h.halt();
    let halt_time = t0.elapsed();
    assert!(
        halt_time < Duration::from_millis(50),
        "halt took {halt_time:?} — it must not wait for the worker"
    );

    gate.store(true, Ordering::Release);
}

/// Opens a gate when dropped, so a failing assertion cannot leave a worker
/// parked on it: `ThreadSpawner::drop` joins its workers, and a join on a task
/// nobody will release turns a test failure into a silent hang.
struct OpenOnDrop(Arc<AtomicBool>);

impl Drop for OpenOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[test]
fn a_node_refuses_to_start_a_second_copy_while_the_first_unwinds() {
    let spawner = Arc::new(ThreadSpawner::new(2));
    let (work, gate, started) = doubler();
    let _release = OpenOnDrop(Arc::clone(&gate));
    let mut h = build(work, spawner, None);

    h.tick();
    let deadline = Instant::now() + Duration::from_secs(5);
    while started.load(Ordering::SeqCst) == 0 {
        assert!(
            Instant::now() < deadline,
            "the worker never started the job"
        );
        std::thread::yield_now();
    }
    h.halt();

    // The cancelled worker is still in its sleep loop. Re-entering must not
    // launch a second copy of the same side effect.
    assert_eq!(h.tick(), Status::Running);
    assert_eq!(
        started.load(Ordering::SeqCst),
        1,
        "the halted work must not be started again until it has unwound"
    );

    gate.store(true, Ordering::Release);
    let deadline = Instant::now() + Duration::from_secs(5);
    while started.load(Ordering::SeqCst) < 2 && Instant::now() < deadline {
        h.tick();
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        started.load(Ordering::SeqCst),
        2,
        "once it has unwound, the node restarts normally"
    );
}

// ── settling under every ending ──────────────────────────────────────────────

#[test]
fn the_inline_spawner_makes_the_async_path_deterministic() {
    let spawner = Arc::new(InlineSpawner::new());
    let (work, gate, _) = doubler();
    gate.store(true, Ordering::Release); // completes immediately
    let mut h = build(work, spawner, None);

    // Spawned and settled before `spawn` returned, so exactly one Running tick.
    assert_eq!(h.tick(), Status::Running);
    assert_eq!(h.tick(), Status::Success);
}

#[test]
fn a_panicking_task_settles_as_failure() {
    struct Exploding;
    impl AsyncWork for Exploding {
        fn start(&mut self, _cur: &mut Cursor<'_, '_>) -> Job {
            Box::new(|_| panic!("boom"))
        }
    }

    let spawner = Arc::new(InlineSpawner::new());
    let mut b = TreeBuilder::new();
    b.begin("root", Builtin::sequence())
        .leaf_custom(
            "boom",
            &WORK,
            Box::new(AsyncAction::new(Exploding, spawner)),
        )
        .port("result", "{r}")
        .end();
    let mut bb = Blackboard::new();
    let graph = b.build(&mut bb).unwrap();
    let mut h = Harness {
        tree: Tree::new(graph),
        bb,
        clock: SimClock::new(),
    };

    assert_eq!(h.tick(), Status::Running);
    // A leaf that panicked and then sat Running forever is a far worse failure
    // than a Failure.
    assert_eq!(h.tick(), Status::Failure);
}

#[test]
fn a_task_that_returns_running_is_treated_as_failure() {
    struct NeverSettles;
    impl AsyncWork for NeverSettles {
        fn start(&mut self, _cur: &mut Cursor<'_, '_>) -> Job {
            Box::new(|_| Status::Running)
        }
    }
    let spawner = Arc::new(InlineSpawner::new());
    let mut b = TreeBuilder::new();
    b.begin("root", Builtin::sequence())
        .leaf_custom(
            "w",
            &WORK,
            Box::new(AsyncAction::new(NeverSettles, spawner)),
        )
        .port("result", "{r}")
        .end();
    let mut bb = Blackboard::new();
    let graph = b.build(&mut bb).unwrap();
    let mut h = Harness {
        tree: Tree::new(graph),
        bb,
        clock: SimClock::new(),
    };
    h.tick();
    // "Running" from finished work means nothing; polling it forever is the one
    // outcome that must not happen.
    assert_eq!(h.tick(), Status::Failure);
}

#[test]
fn futures_go_through_the_same_node_code() {
    struct Awaiting;
    impl AsyncWork for Awaiting {
        fn start(&mut self, _cur: &mut Cursor<'_, '_>) -> Job {
            unreachable!("this test drives spawn_future directly")
        }
    }
    let _ = Awaiting;

    let spawner = ThreadSpawner::new(1);
    let barrier = Arc::new(Barrier::new(1));
    let _ = barrier;
    let handle = spawner
        .spawn_future(Box::pin(async { Status::Success }), None)
        .expect("ThreadSpawner drives futures");

    let deadline = Instant::now() + Duration::from_secs(5);
    while !handle.is_done() && Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert_eq!(
        handle.state(),
        bteng_async::TaskState::Done(Outcome::Settled(Status::Success))
    );
}

#[test]
fn a_spawner_that_cannot_drive_futures_says_so() {
    // The fallback lives in the trait, not in every node — which is what keeps
    // node code identical across backends.
    struct BlockingOnly;
    impl Spawner for BlockingOnly {
        fn spawn(&self, _job: Job, _w: Option<Arc<dyn Waker>>) -> bteng_async::TaskHandle {
            unreachable!()
        }
        fn pending(&self) -> usize {
            0
        }
    }
    assert!(
        BlockingOnly
            .spawn_future(Box::pin(async { Status::Success }), None)
            .is_none()
    );
}
