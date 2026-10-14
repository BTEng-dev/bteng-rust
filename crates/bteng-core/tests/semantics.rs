//! Node semantics, pinned against BehaviorTree.CPP behaviour (DR-8).
//!
//! Written as behaviour, not as coverage: these are the contracts the XML front
//! end and any future node additions have to keep.

use core::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard};

use bteng_core::{
    Blackboard, Builtin, Ctx, NodeId, NodeState, SimClock, Status, Tree, TreeBuilder,
};

/// Tick counters per leaf slot. Statics rather than closures because the
/// function-pointer leaves have nowhere to hang per-instance state.
static TICKS: [AtomicU32; 4] = [
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
];

/// `cargo test` runs tests on parallel threads and the counters are global, so
/// every test that reads one takes this lock first.
static SERIAL: Mutex<()> = Mutex::new(());

fn begin_test() -> MutexGuard<'static, ()> {
    let guard = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for t in &TICKS {
        t.store(0, Ordering::Relaxed);
    }
    guard
}

fn count(i: usize) -> u32 {
    TICKS[i].load(Ordering::Relaxed)
}

macro_rules! leaf {
    ($name:ident, $slot:expr, $status:expr) => {
        fn $name(_c: &mut Ctx<'_>) -> Status {
            TICKS[$slot].fetch_add(1, Ordering::Relaxed);
            $status
        }
    };
}

leaf!(ok0, 0, Status::Success);
leaf!(ok1, 1, Status::Success);
leaf!(fail1, 1, Status::Failure);
leaf!(run1, 1, Status::Running);
leaf!(ok2, 2, Status::Success);
leaf!(run2, 2, Status::Running);

fn yes(_c: &Ctx<'_>) -> bool {
    true
}
fn no(_c: &Ctx<'_>) -> bool {
    false
}

/// True while the blackboard key `gate` is set. Lets a test flip a condition
/// under a running branch, which is what the reactive nodes exist for.
fn gate(c: &Ctx<'_>) -> bool {
    c.bb.resolve("gate")
        .and_then(|s| c.bb.get::<bool>(s).ok().copied())
        .unwrap_or(false)
}

struct Harness {
    tree: Tree,
    bb: Blackboard,
    clock: SimClock,
}

impl Harness {
    fn build(f: impl FnOnce(&mut TreeBuilder)) -> Self {
        let mut b = TreeBuilder::new();
        f(&mut b);
        let mut bb = Blackboard::new();
        let graph = b.build(&mut bb).expect("tree builds");
        Self {
            tree: Tree::new(graph),
            bb,
            clock: SimClock::new(),
        }
    }

    /// Declare the `gate` key the reactive tests flip.
    fn with_gate(mut self, open: bool) -> Self {
        self.bb.declare("gate", open).unwrap();
        self
    }

    fn tick(&mut self) -> Status {
        let mut ctx = Ctx::new(&mut self.bb, &self.clock);
        self.tree.tick(&mut ctx)
    }

    fn halt(&mut self) {
        let mut ctx = Ctx::new(&mut self.bb, &self.clock);
        self.tree.halt(&mut ctx);
    }

    fn set_gate(&mut self, open: bool) {
        let slot = self.bb.resolve("gate").expect("gate declared");
        self.bb.set(slot, open).unwrap();
    }

    fn state_of_child(&self, i: u16) -> NodeState {
        self.tree
            .graph()
            .state(self.tree.graph().child(NodeId::ROOT, i))
    }
}

// ── Sequence family ──────────────────────────────────────────────────────────

#[test]
fn sequence_stops_at_first_failure() {
    let _guard = begin_test();
    let mut h = Harness::build(|b| {
        b.begin("root", Builtin::sequence())
            .leaf("a", Builtin::action(ok0))
            .leaf("b", Builtin::action(fail1))
            .leaf("c", Builtin::action(ok2))
            .end();
    });

    assert_eq!(h.tick(), Status::Failure);
    assert_eq!(count(0), 1);
    assert_eq!(count(1), 1);
    assert_eq!(count(2), 0, "child after a failure must not be ticked");
}

#[test]
fn sequence_resumes_at_the_running_child() {
    let _guard = begin_test();
    let mut h = Harness::build(|b| {
        b.begin("root", Builtin::sequence())
            .leaf("a", Builtin::action(ok0))
            .leaf("b", Builtin::action(run1))
            .end();
    });

    for _ in 0..3 {
        assert_eq!(h.tick(), Status::Running);
    }
    assert_eq!(
        count(0),
        1,
        "a Sequence must not re-tick a child that already succeeded"
    );
    assert_eq!(count(1), 3);
}

#[test]
fn sequence_with_memory_retries_the_failed_child_not_the_whole_sequence() {
    let _guard = begin_test();
    let mut h = Harness::build(|b| {
        b.begin("root", Builtin::sequence_with_memory())
            .leaf("a", Builtin::action(ok0))
            .leaf("b", Builtin::action(fail1))
            .end();
    });

    assert_eq!(h.tick(), Status::Failure);
    assert_eq!(h.tick(), Status::Failure);
    assert_eq!(
        count(0),
        1,
        "the whole point of SequenceWithMemory: a Failure does not rewind"
    );
    assert_eq!(count(1), 2);
}

#[test]
fn reactive_sequence_reevaluates_earlier_children_every_tick() {
    let _guard = begin_test();
    let mut h = Harness::build(|b| {
        b.begin("root", Builtin::reactive_sequence())
            .leaf("guard", Builtin::condition(gate))
            .leaf("work", Builtin::action(run1))
            .end();
    })
    .with_gate(true);

    assert_eq!(h.tick(), Status::Running);
    assert_eq!(h.tick(), Status::Running);
    assert_eq!(count(1), 2);

    // The guard closing must interrupt the running action — the entire reason
    // the reactive variant exists.
    h.set_gate(false);
    assert_eq!(h.tick(), Status::Failure);
    assert_eq!(
        count(1),
        2,
        "the action must not be ticked after the guard failed"
    );
}

// ── Fallback family ──────────────────────────────────────────────────────────

#[test]
fn fallback_stops_at_first_success() {
    let _guard = begin_test();
    let mut h = Harness::build(|b| {
        b.begin("root", Builtin::fallback())
            .leaf("a", Builtin::condition(no))
            .leaf("b", Builtin::action(ok1))
            .leaf("c", Builtin::action(ok2))
            .end();
    });

    assert_eq!(h.tick(), Status::Success);
    assert_eq!(count(1), 1);
    assert_eq!(count(2), 0);
}

#[test]
fn reactive_fallback_lets_a_higher_priority_child_preempt() {
    let _guard = begin_test();
    let mut h = Harness::build(|b| {
        b.begin("root", Builtin::reactive_fallback())
            .leaf("high", Builtin::condition(gate))
            .leaf("low", Builtin::action(run2))
            .end();
    })
    .with_gate(false);

    assert_eq!(h.tick(), Status::Running);
    assert_eq!(count(2), 1);

    h.set_gate(true);
    assert_eq!(h.tick(), Status::Success);
    assert_eq!(count(2), 1, "the low-priority branch must be preempted");
    assert_eq!(
        h.state_of_child(1),
        NodeState::Idle,
        "the preempted child must be halted, not left Running"
    );
}

// ── Parallel ─────────────────────────────────────────────────────────────────

#[test]
fn parallel_succeeds_at_its_success_threshold() {
    let _guard = begin_test();
    let mut h = Harness::build(|b| {
        b.begin("par", Builtin::parallel())
            .port("success_count", "2")
            .leaf("a", Builtin::action(ok0))
            .leaf("b", Builtin::action(run1))
            .leaf("c", Builtin::action(ok2))
            .end();
    });

    assert_eq!(h.tick(), Status::Success);
    assert_eq!(count(0), 1);
    assert_eq!(count(2), 1);
}

#[test]
fn parallel_does_not_retick_a_settled_child() {
    let _guard = begin_test();
    let mut h = Harness::build(|b| {
        b.begin("par", Builtin::parallel())
            .port("success_count", "2")
            .leaf("a", Builtin::action(ok0))
            .leaf("b", Builtin::action(run1))
            .end();
    });

    assert_eq!(h.tick(), Status::Running);
    assert_eq!(h.tick(), Status::Running);
    assert_eq!(
        count(0),
        1,
        "a settled child must not be re-ticked this activation"
    );
    assert_eq!(count(1), 2);
}

#[test]
fn parallel_defaults_to_requiring_every_child() {
    let _guard = begin_test();
    let mut h = Harness::build(|b| {
        b.begin("par", Builtin::parallel())
            .leaf("a", Builtin::action(ok0))
            .leaf("b", Builtin::action(ok1))
            .end();
    });
    assert_eq!(h.tick(), Status::Success);
}

#[test]
fn parallel_fails_at_its_failure_threshold() {
    let _guard = begin_test();
    let mut h = Harness::build(|b| {
        b.begin("par", Builtin::parallel())
            .leaf("a", Builtin::action(fail1))
            .leaf("b", Builtin::action(run2))
            .end();
    });
    assert_eq!(h.tick(), Status::Failure, "failure_count defaults to 1");
}

// ── branching control ────────────────────────────────────────────────────────

#[test]
fn if_then_else_picks_a_branch_and_keeps_it_while_running() {
    let _guard = begin_test();
    let mut h = Harness::build(|b| {
        b.begin("ite", Builtin::if_then_else())
            .leaf("cond", Builtin::condition(gate))
            .leaf("then", Builtin::action(run1))
            .leaf("else", Builtin::action(ok2))
            .end();
    })
    .with_gate(true);

    assert_eq!(h.tick(), Status::Running);
    // The condition flips, but IfThenElse has already committed.
    h.set_gate(false);
    assert_eq!(h.tick(), Status::Running);
    assert_eq!(count(1), 2);
    assert_eq!(
        count(2),
        0,
        "IfThenElse must not re-evaluate a committed branch"
    );
}

#[test]
fn while_do_else_switches_branch_when_the_condition_flips() {
    let _guard = begin_test();
    let mut h = Harness::build(|b| {
        b.begin("wde", Builtin::while_do_else())
            .leaf("cond", Builtin::condition(gate))
            .leaf("do", Builtin::action(run1))
            .leaf("else", Builtin::action(run2))
            .end();
    })
    .with_gate(true);

    assert_eq!(h.tick(), Status::Running);
    assert_eq!(count(1), 1);

    h.set_gate(false);
    assert_eq!(h.tick(), Status::Running);
    assert_eq!(
        count(1),
        1,
        "the do-branch must stop when the condition flips"
    );
    assert_eq!(count(2), 1);
    assert_eq!(
        h.state_of_child(1),
        NodeState::Idle,
        "the abandoned branch must be halted, or two branches are live at once"
    );
}

#[test]
fn if_then_else_without_an_else_fails_when_the_condition_fails() {
    let _guard = begin_test();
    let mut h = Harness::build(|b| {
        b.begin("ite", Builtin::if_then_else())
            .leaf("cond", Builtin::condition(no))
            .leaf("then", Builtin::action(ok1))
            .end();
    });
    assert_eq!(h.tick(), Status::Failure);
    assert_eq!(count(1), 0);
}

// ── decorators ───────────────────────────────────────────────────────────────

#[test]
fn inverter_swaps_settled_statuses_and_passes_running() {
    let _guard = begin_test();
    for (inner, expected) in [
        (Builtin::condition(yes), Status::Failure),
        (Builtin::condition(no), Status::Success),
        (Builtin::action(run1), Status::Running),
    ] {
        let mut h = Harness::build(|b| {
            b.begin("inv", Builtin::Inverter).leaf("child", inner).end();
        });
        assert_eq!(h.tick(), expected);
    }
}

#[test]
fn force_decorators_pass_running_through() {
    let _guard = begin_test();
    let mut h = Harness::build(|b| {
        b.begin("f", Builtin::ForceSuccess)
            .leaf("child", Builtin::action(fail1))
            .end();
    });
    assert_eq!(h.tick(), Status::Success);

    let mut h = Harness::build(|b| {
        b.begin("f", Builtin::ForceFailure)
            .leaf("child", Builtin::action(run1))
            .end();
    });
    assert_eq!(
        h.tick(),
        Status::Running,
        "Running is not a settled status to force"
    );
}

#[test]
fn repeat_completes_one_cycle_per_tick() {
    let _guard = begin_test();
    let mut h = Harness::build(|b| {
        b.begin("rep", Builtin::repeat())
            .port("num_cycles", "3")
            .leaf("child", Builtin::action(ok1))
            .end();
    });

    // Deliberate deviation from BT.CPP, which loops inside one tick: unbounded
    // work in a tick is what DR-4 rules out.
    assert_eq!(h.tick(), Status::Running);
    assert_eq!(h.tick(), Status::Running);
    assert_eq!(h.tick(), Status::Success);
    assert_eq!(count(1), 3);
}

#[test]
fn repeat_fails_immediately_when_the_child_fails() {
    let _guard = begin_test();
    let mut h = Harness::build(|b| {
        b.begin("rep", Builtin::repeat())
            .port("num_cycles", "5")
            .leaf("child", Builtin::action(fail1))
            .end();
    });
    assert_eq!(h.tick(), Status::Failure);
    assert_eq!(count(1), 1);
}

#[test]
fn retry_gives_the_child_the_declared_number_of_attempts() {
    let _guard = begin_test();
    let mut h = Harness::build(|b| {
        b.begin("retry", Builtin::retry())
            .port("num_attempts", "3")
            .leaf("child", Builtin::action(fail1))
            .end();
    });

    assert_eq!(h.tick(), Status::Running);
    assert_eq!(h.tick(), Status::Running);
    assert_eq!(h.tick(), Status::Failure);
    assert_eq!(count(1), 3);
}

#[test]
fn keep_running_until_failure() {
    let _guard = begin_test();
    let mut h = Harness::build(|b| {
        b.begin("k", Builtin::KeepRunningUntilFailure)
            .leaf("child", Builtin::action(ok1))
            .end();
    });
    assert_eq!(h.tick(), Status::Running);
    assert_eq!(h.tick(), Status::Running);
}

#[test]
fn run_once_replays_its_cached_status() {
    let _guard = begin_test();
    let mut h = Harness::build(|b| {
        b.begin("once", Builtin::run_once())
            .leaf("child", Builtin::action(ok1))
            .end();
    });

    for _ in 0..3 {
        assert_eq!(h.tick(), Status::Success);
    }
    assert_eq!(count(1), 1, "RunOnce must run the child exactly once");

    // A halt must not reset it — "once" would mean nothing if it did.
    h.halt();
    assert_eq!(h.tick(), Status::Success);
    assert_eq!(count(1), 1);
}

#[test]
fn timeout_fails_a_child_that_overstays() {
    let _guard = begin_test();
    let mut h = Harness::build(|b| {
        b.begin("t", Builtin::timeout())
            .port("msec", "100")
            .leaf("child", Builtin::action(run1))
            .end();
    });

    assert_eq!(h.tick(), Status::Running);
    h.clock.advance(50_000_000); // 50 ms
    assert_eq!(h.tick(), Status::Running);
    h.clock.advance(60_000_000); // past 100 ms
    assert_eq!(h.tick(), Status::Failure);
    assert_eq!(
        h.state_of_child(0),
        NodeState::Idle,
        "a timed-out child must be halted, not abandoned Running"
    );
}

#[test]
fn timeout_leaves_a_child_that_finishes_in_time_alone() {
    let _guard = begin_test();
    let mut h = Harness::build(|b| {
        b.begin("t", Builtin::timeout())
            .port("msec", "100")
            .leaf("child", Builtin::action(ok1))
            .end();
    });
    assert_eq!(h.tick(), Status::Success);
}

#[test]
fn delay_waits_before_first_ticking_the_child() {
    let _guard = begin_test();
    let mut h = Harness::build(|b| {
        b.begin("d", Builtin::delay())
            .port("delay_msec", "20")
            .leaf("child", Builtin::action(ok1))
            .end();
    });

    assert_eq!(h.tick(), Status::Running);
    assert_eq!(
        count(1),
        0,
        "the child must not run before the delay elapses"
    );
    h.clock.advance(25_000_000);
    assert_eq!(h.tick(), Status::Success);
    assert_eq!(count(1), 1);
}

#[test]
fn sleep_is_running_until_its_deadline() {
    let _guard = begin_test();
    let mut h = Harness::build(|b| {
        b.begin("root", Builtin::sequence())
            .leaf("nap", Builtin::sleep())
            .port("msec", "10")
            .end();
    });

    assert_eq!(h.tick(), Status::Running);
    h.clock.advance(9_000_000);
    assert_eq!(h.tick(), Status::Running);
    h.clock.advance(2_000_000);
    assert_eq!(h.tick(), Status::Success);
}

#[test]
fn always_nodes_do_what_they_say() {
    let _guard = begin_test();
    let mut h = Harness::build(|b| {
        b.begin("root", Builtin::fallback())
            .leaf("a", Builtin::AlwaysFailure)
            .leaf("b", Builtin::AlwaysSuccess)
            .end();
    });
    assert_eq!(h.tick(), Status::Success);
}

#[test]
fn subtree_is_transparent() {
    let _guard = begin_test();
    let mut h = Harness::build(|b| {
        b.begin("st", Builtin::SubTree)
            .leaf("inner", Builtin::action(fail1))
            .end();
    });
    assert_eq!(h.tick(), Status::Failure);
}

// ── halt and structure ───────────────────────────────────────────────────────

#[test]
fn halt_clears_control_state_so_the_next_activation_restarts() {
    let _guard = begin_test();
    let mut h = Harness::build(|b| {
        b.begin("root", Builtin::sequence())
            .leaf("a", Builtin::action(ok0))
            .leaf("b", Builtin::action(run1))
            .end();
    });

    assert_eq!(h.tick(), Status::Running);
    assert_eq!(count(0), 1);

    h.halt();
    assert_eq!(h.tree.graph().state(NodeId::ROOT), NodeState::Idle);

    // Python BTEng shipped this bug in RateController: cached progress that
    // survived a reset and replayed a stale status without re-running the work.
    assert_eq!(h.tick(), Status::Running);
    assert_eq!(count(0), 2, "halt must reset the sequence cursor");
}

#[test]
fn halting_an_idle_tree_is_a_no_op() {
    let _guard = begin_test();
    let mut h = Harness::build(|b| {
        b.begin("root", Builtin::sequence())
            .leaf("a", Builtin::action(ok0))
            .end();
    });
    h.halt();
    assert_eq!(h.tree.graph().state(NodeId::ROOT), NodeState::Idle);
}

#[test]
fn halting_one_branch_leaves_its_sibling_alone() {
    let _guard = begin_test();
    let mut h = Harness::build(|b| {
        b.begin("root", Builtin::parallel())
            .begin("left", Builtin::sequence())
            .leaf("l", Builtin::action(run1))
            .end()
            .begin("right", Builtin::sequence())
            .leaf("r", Builtin::action(ok2))
            .end()
            .end();
    });

    assert_eq!(h.tick(), Status::Running);
    let right = h.tree.graph().child(NodeId::ROOT, 1);
    let left = h.tree.graph().child(NodeId::ROOT, 0);
    assert_eq!(h.tree.graph().state(right), NodeState::Success);

    let mut ctx = Ctx::new(&mut h.bb, &h.clock);
    bteng_core::halt_node(h.tree.graph_mut(), left, &mut ctx);

    assert_eq!(h.tree.graph().state(left), NodeState::Idle);
    assert_eq!(
        h.tree.graph().state(right),
        NodeState::Success,
        "halting one branch must not touch its sibling"
    );
}

#[test]
fn deep_nesting_ticks_without_the_arena_fighting_itself() {
    let _guard = begin_test();
    // The take/put-behaviour trick is what allows a control node to tick
    // children living in the same arena. Depth is where a subtle mistake shows.
    let mut h = Harness::build(|b| {
        for _ in 0..64 {
            b.begin("seq", Builtin::sequence());
        }
        b.leaf("leaf", Builtin::action(ok0));
        for _ in 0..64 {
            b.end();
        }
    });
    assert_eq!(h.tick(), Status::Success);
    assert_eq!(count(0), 1);
}

#[test]
fn hot_types_stay_small() {
    // `take_behavior`/`put_behavior` move a `NodeKind` twice per node visit, and
    // `NodeStruct` is walked linearly by every traversal. Both sizes are part of
    // the tick cost, so they get a guard rather than a hope.
    use core::mem::size_of;
    assert!(
        size_of::<bteng_core::NodeKind>() <= 32,
        "NodeKind grew to {} bytes — it is moved twice per node visit",
        size_of::<bteng_core::NodeKind>()
    );
    assert!(
        size_of::<bteng_core::NodeStruct>() <= 16,
        "NodeStruct grew to {} bytes — it is walked by every traversal",
        size_of::<bteng_core::NodeStruct>()
    );
}
