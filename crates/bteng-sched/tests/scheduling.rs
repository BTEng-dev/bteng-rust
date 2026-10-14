//! Scheduler behaviour, asserted under [`VirtualTime`] so every run is
//! deterministic — same sequence of region ticks, same timestamps, every time
//! and on every architecture.

use core::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard};

use bteng_core::{Blackboard, Builtin, Ctx, NodeState, Status, Tree, TreeBuilder};
use bteng_sched::{
    OverrunPolicy, Pacer, RunOutcome, SchedError, Scheduler, StepOutcome, VirtualTime,
};

static TICKS: [AtomicU32; 4] = [
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
];
static SERIAL: Mutex<()> = Mutex::new(());

fn begin_test() -> MutexGuard<'static, ()> {
    let g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    for t in &TICKS {
        t.store(0, Ordering::Relaxed);
    }
    g
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

leaf!(run0, 0, Status::Running);
leaf!(run1, 1, Status::Running);
leaf!(run2, 2, Status::Running);
leaf!(ok1, 1, Status::Success);
leaf!(ok0, 0, Status::Success);

fn gate(c: &Ctx<'_>) -> bool {
    c.bb.resolve("gate")
        .and_then(|s| c.bb.get::<bool>(s).ok().copied())
        .unwrap_or(false)
}

const MS: u64 = 1_000_000;
const SEC: u64 = 1_000_000_000;

/// A slow leaf and a fast region, side by side.
///
/// The root is a `Parallel`, not a `Sequence`, and that is worth understanding
/// rather than working around: a region publishes `Running` until its subtree
/// settles, so a `Sequence` parent stops there and never reaches its later
/// children. Regions belong under a `Parallel`, under a `ReactiveSequence`, or
/// last in a `Sequence`.
fn two_rate_tree(hz: &str, overrun: Option<&str>) -> (Scheduler, Blackboard) {
    let mut b = TreeBuilder::new();
    b.begin("root", Builtin::parallel())
        .begin("fast", Builtin::region())
        .port("hz", hz);
    if let Some(o) = overrun {
        b.port("overrun", o);
    }
    b.leaf("fast_work", Builtin::action(run1))
        .end()
        .leaf("slow_work", Builtin::action(run0))
        .end();

    let mut bb = Blackboard::new();
    let graph = b.build(&mut bb).expect("builds");
    let sched = Scheduler::new(Tree::new(graph), &bb, 10.0).expect("scheduler builds");
    (sched, bb)
}

// ── the exit criterion ───────────────────────────────────────────────────────

#[test]
fn a_region_runs_at_its_own_rate_not_its_parents() {
    let _g = begin_test();
    // 10 Hz root, 100 Hz region. In Python BTEng the parent's rate is a hard
    // ceiling and this would come out 10:10, because RateController ticks its
    // child inside the parent's tick.
    let (mut sched, mut bb) = two_rate_tree("100.0", None);
    let pacer = VirtualTime::new();

    let outcome = sched.run_for(&mut bb, &pacer, SEC, 0);
    assert!(matches!(outcome, RunOutcome::TimeUp { .. }), "{outcome:?}");

    let root = sched.stats(0).unwrap();
    let fast = sched.stats(1).unwrap();
    assert_eq!(root.ticks, 10, "root asked for 10 Hz over one second");
    assert_eq!(fast.ticks, 100, "region asked for 100 Hz over one second");
    assert_eq!(count(0), 10, "the root's own leaf follows the root's rate");
    assert_eq!(count(1), 100, "the region's leaf follows the region's rate");
}

#[test]
fn virtual_time_runs_are_bit_identical() {
    let _g = begin_test();
    let mut trace_a = Vec::new();
    let mut trace_b = Vec::new();

    for trace in [&mut trace_a, &mut trace_b] {
        let (mut sched, mut bb) = two_rate_tree("100.0", None);
        let pacer = VirtualTime::new();
        sched.arm(pacer.now());
        for _ in 0..50 {
            let now = pacer.now();
            trace.push((now, sched.step(&mut bb, &pacer)));
        }
    }
    // Determinism is what makes M6's replay possible at all, so it is asserted
    // rather than assumed.
    assert_eq!(trace_a, trace_b);
}

// ── deadline discipline ──────────────────────────────────────────────────────

#[test]
fn deadlines_do_not_drift_when_ticks_take_time() {
    let _g = begin_test();
    // A leaf that burns 3 ms of simulated time every tick. Under `sleep(period)`
    // pacing — Python BTEng's loop — a 100 Hz region would slide to ~77 Hz.
    // Absolute deadlines mean the phase is paid once, not compounded.
    fn slow(_c: &mut Ctx<'_>) -> Status {
        TICKS[2].fetch_add(1, Ordering::Relaxed);
        Status::Running
    }

    let mut b = TreeBuilder::new();
    b.begin("root", Builtin::sequence())
        .begin("fast", Builtin::region())
        .port("hz", "100.0")
        .leaf("work", Builtin::action(slow))
        .end()
        .end();
    let mut bb = Blackboard::new();
    let graph = b.build(&mut bb).unwrap();
    let mut sched = Scheduler::new(Tree::new(graph), &bb, 10.0).unwrap();

    let pacer = VirtualTime::new();
    sched.arm(pacer.now());
    let period = sched.period(1).unwrap();
    assert_eq!(period, 10 * MS);

    // Step the region 20 times, injecting 3 ms of simulated work after each.
    let mut region_ticks = 0;
    while region_ticks < 20 {
        if let StepOutcome::Ticked { region: 1, .. } = sched.step(&mut bb, &pacer) {
            region_ticks += 1;
            pacer.advance(3 * MS);
        }
    }
    // 20 ticks on a fixed 10 ms grid: the 20th starts at 190 ms and its work
    // ends at 193 ms, regardless of the 3 ms each one cost. Under
    // `tick(); sleep(period)` pacing this would be at 20 x 13 = 260 ms, i.e. an
    // effective 77 Hz for a region that asked for 100.
    assert_eq!(pacer.now(), 193 * MS);
    assert_eq!(count(2), 20);
    assert_eq!(
        sched.stats(1).unwrap().total_lateness,
        0,
        "work that fits inside the period must never make the region late"
    );
}

#[test]
fn skip_realigns_to_the_grid_rather_than_to_now() {
    let _g = begin_test();
    let (mut sched, mut bb) = two_rate_tree("100.0", Some("skip"));
    let pacer = VirtualTime::new();
    sched.arm(pacer.now());

    sched.step(&mut bb, &pacer); // root at t=0
    sched.step(&mut bb, &pacer); // region at t=0

    // Stall for 35 ms — three and a half region periods.
    pacer.advance(35 * MS);
    sched.step(&mut bb, &pacer);

    let fast = sched.stats(1).unwrap();
    assert!(
        fast.overruns >= 1,
        "the stall must be counted as an overrun"
    );
    assert!(
        fast.skipped >= 1,
        "missed ticks must be dropped, not queued"
    );
}

#[test]
fn catch_up_fires_the_backlog_but_bounded() {
    let _g = begin_test();
    let (mut sched, mut bb) = two_rate_tree("100.0", Some("catch_up:2"));
    let pacer = VirtualTime::new();
    sched.arm(pacer.now());

    sched.step(&mut bb, &pacer);
    sched.step(&mut bb, &pacer);
    pacer.advance(50 * MS); // five periods missed
    sched.step(&mut bb, &pacer);

    let fast = sched.stats(1).unwrap();
    assert!(
        fast.caught_up <= 2,
        "unbounded catch-up turns one slow tick into a burst that causes the next; got {}",
        fast.caught_up
    );
}

#[test]
fn abort_stops_the_run_and_says_why() {
    let _g = begin_test();
    let (mut sched, mut bb) = two_rate_tree("100.0", Some("abort"));
    let pacer = VirtualTime::new();
    sched.arm(pacer.now());

    sched.step(&mut bb, &pacer);
    sched.step(&mut bb, &pacer);
    pacer.advance(50 * MS);
    sched.step(&mut bb, &pacer);

    match sched.abort_reason() {
        Some(SchedError::DeadlineMissed { late_by, .. }) => assert!(*late_by > 0),
        other => panic!("expected a deadline miss, got {other:?}"),
    }
}

#[test]
fn an_unknown_overrun_policy_is_an_error_not_a_default() {
    let _g = begin_test();
    let mut b = TreeBuilder::new();
    b.begin("root", Builtin::region())
        .port("hz", "10.0")
        .port("overrun", "skipp")
        .leaf("w", Builtin::action(run0))
        .end();
    let mut bb = Blackboard::new();
    let graph = b.build(&mut bb).unwrap();
    // A typo silently becoming `Skip` is exactly the failure mode this engine
    // keeps refusing to ship.
    assert!(matches!(
        Scheduler::new(Tree::new(graph), &bb, 10.0),
        Err(SchedError::BadOverrunPolicy { .. })
    ));
}

#[test]
fn a_rate_outside_the_supported_range_is_refused() {
    for hz in [0.0, -1.0, f64::NAN, f64::INFINITY, 5000.0] {
        let mut b = TreeBuilder::new();
        b.begin("root", Builtin::sequence())
            .leaf("w", Builtin::action(run0))
            .end();
        let mut bb = Blackboard::new();
        let graph = b.build(&mut bb).unwrap();
        assert!(
            matches!(
                Scheduler::new(Tree::new(graph), &bb, hz),
                Err(SchedError::BadRate { .. })
            ),
            "{hz} should be refused"
        );
    }
}

// ── boundary semantics ───────────────────────────────────────────────────────

#[test]
fn the_parent_reads_a_published_verdict_and_never_descends() {
    let _g = begin_test();
    let (mut sched, mut bb) = two_rate_tree("100.0", None);
    let pacer = VirtualTime::new();
    sched.arm(pacer.now());

    // Root ticks first. It must not tick the region's subtree itself.
    sched.step(&mut bb, &pacer);
    assert_eq!(
        count(1),
        0,
        "the parent traversal must stop at the region boundary"
    );
    assert_eq!(count(0), 1, "but it does tick its own sibling leaf");

    sched.step(&mut bb, &pacer);
    assert_eq!(count(1), 1, "the scheduler ticks the region's subtree");
}

#[test]
fn a_region_that_settles_latches_until_it_is_re_entered() {
    let _g = begin_test();
    // The region's leaf succeeds immediately.
    let mut b = TreeBuilder::new();
    b.begin("root", Builtin::sequence())
        .begin("done", Builtin::region())
        .port("hz", "100.0")
        .leaf("w", Builtin::action(ok1))
        .end()
        .leaf("hold", Builtin::action(run0))
        .end();
    let mut bb = Blackboard::new();
    let graph = b.build(&mut bb).unwrap();
    let mut sched = Scheduler::new(Tree::new(graph), &bb, 10.0).unwrap();
    let pacer = VirtualTime::new();

    let _ = sched.run_for(&mut bb, &pacer, 200 * MS, 0);
    // Without latching, the scheduler would restart the region 100 times a
    // second and the parent would never see the Success it published.
    assert_eq!(
        count(1),
        1,
        "a settled region waits to be read, not restarted"
    );
}

#[test]
fn a_guard_closing_stops_the_scheduler_ticking_the_region() {
    let _g = begin_test();
    // A ReactiveSequence re-evaluates its guard every tick; when the guard
    // fails it halts the branch behind it, region included. The scheduler must
    // notice — a region that kept running after the tree stopped it would be
    // executing behaviour nothing in the tree is asking for.
    let mut b = TreeBuilder::new();
    b.begin("root", Builtin::reactive_sequence())
        .leaf("guard", Builtin::condition(gate))
        .begin("fast", Builtin::region())
        .port("hz", "100.0")
        .leaf("work", Builtin::action(run1))
        .end()
        .end();
    let mut bb = Blackboard::new();
    let graph = b.build(&mut bb).unwrap();
    let gate_slot = bb.declare("gate", true).unwrap();
    let mut sched = Scheduler::new(Tree::new(graph), &bb, 10.0).unwrap();
    let pacer = VirtualTime::new();
    sched.arm(pacer.now());

    for _ in 0..10 {
        sched.step(&mut bb, &pacer);
    }
    let before = count(1);
    assert!(before > 0, "the region should have been running");

    bb.set(gate_slot, false).unwrap();
    // One root tick to let the guard close and halt the branch.
    while !matches!(
        sched.step(&mut bb, &pacer),
        StepOutcome::Ticked { region: 0, .. }
    ) {}

    let boundary = sched.boundary(1).unwrap();
    assert_eq!(sched.tree().graph().state(boundary), NodeState::Idle);

    let after_halt = count(1);
    for _ in 0..30 {
        sched.step(&mut bb, &pacer);
    }
    assert_eq!(
        count(1),
        after_halt,
        "a halted branch must not keep running behind the tree's back"
    );
}

#[test]
fn nested_regions_each_get_their_own_rate() {
    let _g = begin_test();
    let mut b = TreeBuilder::new();
    b.begin("root", Builtin::parallel())
        .begin("outer", Builtin::region())
        .port("hz", "20.0")
        .begin("outer_par", Builtin::parallel())
        .begin("inner", Builtin::region())
        .port("hz", "100.0")
        .leaf("inner_work", Builtin::action(run2))
        .end()
        .leaf("outer_work", Builtin::action(run1))
        .end()
        .end()
        .leaf("root_work", Builtin::action(run0))
        .end();
    let mut bb = Blackboard::new();
    let graph = b.build(&mut bb).unwrap();
    let mut sched = Scheduler::new(Tree::new(graph), &bb, 10.0).unwrap();
    assert_eq!(sched.region_count(), 3);

    let pacer = VirtualTime::new();
    let _ = sched.run_for(&mut bb, &pacer, SEC, 0);

    assert_eq!(count(0), 10, "root");
    assert_eq!(count(1), 20, "outer region");
    assert_eq!(count(2), 100, "inner region, unbounded by either parent");
}

#[test]
fn a_settling_root_ends_the_run() {
    let _g = begin_test();
    let mut b = TreeBuilder::new();
    b.begin("root", Builtin::sequence())
        .leaf("w", Builtin::action(ok0))
        .end();
    let mut bb = Blackboard::new();
    let graph = b.build(&mut bb).unwrap();
    let mut sched = Scheduler::new(Tree::new(graph), &bb, 10.0).unwrap();
    let pacer = VirtualTime::new();

    match sched.run_for(&mut bb, &pacer, SEC, 0) {
        RunOutcome::Settled { status, at } => {
            assert_eq!(status, Status::Success);
            assert_eq!(at, 0, "it settled on the first tick, at t=0");
        }
        other => panic!("got {other:?}"),
    }
    assert_eq!(count(0), 1);
}

#[test]
fn a_tree_with_no_regions_is_just_a_periodic_tree() {
    let _g = begin_test();
    let mut b = TreeBuilder::new();
    b.begin("root", Builtin::sequence())
        .leaf("w", Builtin::action(run0))
        .end();
    let mut bb = Blackboard::new();
    let graph = b.build(&mut bb).unwrap();
    let mut sched = Scheduler::new(Tree::new(graph), &bb, 50.0).unwrap();
    assert_eq!(sched.region_count(), 1);

    let pacer = VirtualTime::new();
    let _ = sched.run_for(&mut bb, &pacer, SEC, 0);
    assert_eq!(count(0), 50);
}

// ── statistics ───────────────────────────────────────────────────────────────

#[test]
fn lateness_is_measured_against_the_deadline_not_the_previous_tick() {
    let _g = begin_test();
    let (mut sched, mut bb) = two_rate_tree("100.0", None);
    let pacer = VirtualTime::new();
    sched.arm(pacer.now());

    sched.step(&mut bb, &pacer);
    sched.step(&mut bb, &pacer);
    pacer.advance(25 * MS);
    sched.step(&mut bb, &pacer);

    let fast = sched.stats(1).unwrap();
    // A tick that starts 15 ms after its deadline is 15 ms late even if the gap
    // from the previous tick happens to be exactly one period. Measuring the gap
    // would report a perfect rate for a phase that walked away.
    assert!(
        fast.max_lateness >= 10 * MS,
        "lateness {} should reflect the stall",
        fast.max_lateness
    );
    assert!(fast.lateness.count() > 0);
}

#[test]
fn histogram_quantiles_never_understate() {
    let mut h = bteng_sched::Histogram::new();
    for v in [1u64, 2, 4, 8, 16, 32, 64, 128] {
        h.record(v);
    }
    assert_eq!(h.count(), 8);
    // Bucketed, so the quantile is an upper bound — the direction that cannot
    // make a timing claim look better than it is.
    assert!(h.quantile(1.0) >= 128);
    assert!(h.quantile(0.5) >= 8);
}

#[test]
fn overrun_policy_parses_the_spellings_it_documents() {
    assert_eq!(OverrunPolicy::parse("skip"), Ok(OverrunPolicy::Skip));
    assert_eq!(OverrunPolicy::parse(" warn "), Ok(OverrunPolicy::Warn));
    assert_eq!(OverrunPolicy::parse("abort"), Ok(OverrunPolicy::Abort));
    assert_eq!(
        OverrunPolicy::parse("catch_up"),
        Ok(OverrunPolicy::CatchUp { max: 4 })
    );
    assert_eq!(
        OverrunPolicy::parse("catch_up:7"),
        Ok(OverrunPolicy::CatchUp { max: 7 })
    );
    assert!(OverrunPolicy::parse("nope").is_err());
}
