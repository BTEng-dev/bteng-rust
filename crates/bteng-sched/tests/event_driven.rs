//! Event-driven regions: read sets, version diffing, debounce, wake-ups.
//!
//! What this replaces is Python BTEng's single global dirty flag, where *any*
//! write to *any* reachable blackboard invalidates *every* reactive node. Here a
//! region watches exactly the slots its own subtree reads, and finds out by
//! comparing version counters — no callbacks, no locks, no allocation.

use core::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use bteng_core::{
    Arity, Blackboard, Builtin, Clock, Cursor, Node, NodeCategory, NodeManifest, PortSpec, SlotId,
    Status, SteadyClock, Tree, TreeBuilder, Waker, input,
};
use bteng_sched::{
    Pacer, RealTime, SchedError, Scheduler, SleepStrategy, StepOutcome, TickPolicy, VirtualTime,
    WakeBus,
};

static SERIAL: Mutex<()> = Mutex::new(());
static TICKS: [AtomicU32; 3] = [AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0)];

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

static WATCH_PORTS: &[PortSpec] = &[input::<i64>("value").with_default("0")];
static WATCHER: NodeManifest =
    NodeManifest::new("Watch", NodeCategory::Action, Arity::Leaf, WATCH_PORTS);

/// Reads one port, stays Running, counts its ticks.
struct Watcher(usize);

impl Node for Watcher {
    fn tick(&mut self, cur: &mut Cursor<'_, '_>) -> Status {
        let _ = cur.get::<i64>(0);
        TICKS[self.0].fetch_add(1, Ordering::Relaxed);
        Status::Running
    }
    fn type_name(&self) -> &'static str {
        "Watch"
    }
}

const MS: u64 = 1_000_000;
const SEC: u64 = 1_000_000_000;

struct Fixture {
    sched: Scheduler,
    bb: Blackboard,
    watched: SlotId,
    other: SlotId,
}

/// Root at 100 Hz beside an event-driven region watching `{watched}`, plus a
/// second periodic region watching `{other}` so the read sets can be told apart.
fn fixture(min_period_ms: &str, heartbeat_hz: &str) -> Fixture {
    let mut b = TreeBuilder::new();
    b.begin("root", Builtin::parallel())
        .begin("reactive", Builtin::region())
        .port("hz", heartbeat_hz)
        .port("mode", "on_change")
        .port("min_period_ms", min_period_ms)
        .leaf_custom("watch", &WATCHER, Box::new(Watcher(1)))
        .port("value", "{watched}")
        .end()
        .begin("other", Builtin::region())
        .port("hz", "5.0")
        .leaf_custom("other_watch", &WATCHER, Box::new(Watcher(2)))
        .port("value", "{other}")
        .end()
        .end();

    let mut bb = Blackboard::new();
    let graph = b.build(&mut bb).expect("builds");
    let watched = bb.resolve("watched").unwrap();
    let other = bb.resolve("other").unwrap();
    let sched = Scheduler::new(Tree::new(graph), &bb, 100.0).expect("scheduler builds");
    Fixture {
        sched,
        bb,
        watched,
        other,
    }
}

// ── read sets ────────────────────────────────────────────────────────────────

#[test]
fn a_region_watches_exactly_what_its_subtree_reads() {
    let _g = begin_test();
    let f = fixture("0", "1.0");
    // The reactive region reads one slot: its watcher's `value` port. Not the
    // Region node's own `hz`, and not the sibling region's slots.
    assert_eq!(f.sched.read_set_len(1), Some(1));
    assert_eq!(f.sched.read_set_len(2), Some(1));
    assert!(
        f.sched.policy(1).unwrap().is_event_driven(),
        "mode=on_change should produce an event-driven policy"
    );
    assert!(!f.sched.policy(2).unwrap().is_event_driven());
}

#[test]
fn a_write_to_another_regions_slot_does_not_wake_this_one() {
    let _g = begin_test();
    let mut f = fixture("0", "1.0");
    let pacer = VirtualTime::new();
    f.sched.arm(pacer.now());

    for _ in 0..40 {
        f.sched.step(&mut f.bb, &pacer);
    }
    let before = count(1);

    // Poke the *other* region's slot, repeatedly.
    for i in 0..50i64 {
        f.bb.set(f.other, i).unwrap();
        f.sched.step(&mut f.bb, &pacer);
    }

    // Python BTEng's global dirty flag would have re-evaluated everything here.
    assert_eq!(
        count(1),
        before,
        "a precise read set means an unrelated write is not this region's business"
    );
}

#[test]
fn a_write_to_a_watched_slot_makes_the_region_due() {
    let _g = begin_test();
    let mut f = fixture("0", "1.0"); // heartbeat 1 s, no debounce
    let pacer = VirtualTime::new();
    f.sched.arm(pacer.now());

    // Nothing changes: over 100 ms the region should not tick at all, because
    // its heartbeat is a second away.
    for _ in 0..20 {
        f.sched.step(&mut f.bb, &pacer);
    }
    let idle_ticks = count(1);

    f.bb.set(f.watched, 7i64).unwrap();
    // The next step must find it due.
    let mut ticked = false;
    for _ in 0..4 {
        if let StepOutcome::Ticked { region: 1, .. } = f.sched.step(&mut f.bb, &pacer) {
            ticked = true;
            break;
        }
    }
    assert!(ticked, "a write to a watched slot must make the region due");
    assert_eq!(count(1), idle_ticks + 1);
}

#[test]
fn an_idle_event_driven_region_only_ticks_on_its_heartbeat() {
    let _g = begin_test();
    let mut f = fixture("0", "2.0"); // heartbeat 500 ms
    let pacer = VirtualTime::new();

    let _ = f.sched.run_for(&mut f.bb, &pacer, SEC, 0);

    // Two heartbeats in a second, against the root's hundred ticks. This is the
    // "burns no CPU while nothing happens" property, measured.
    assert_eq!(count(1), 2, "heartbeat only");
    assert_eq!(f.sched.stats(0).unwrap().ticks, 100, "root still periodic");
}

// ── debounce ─────────────────────────────────────────────────────────────────

#[test]
fn debounce_caps_how_often_a_chatty_slot_can_tick_a_region() {
    let _g = begin_test();
    // 20 ms floor, 1 s heartbeat.
    let mut f = fixture("20", "1.0");
    let pacer = VirtualTime::new();
    f.sched.arm(pacer.now());

    // Write on every root tick — 100 Hz of churn against a region that has said
    // it will not run faster than 50 Hz.
    for i in 0..100i64 {
        f.bb.set(f.watched, i).unwrap();
        f.sched.step(&mut f.bb, &pacer);
        f.sched.step(&mut f.bb, &pacer);
    }

    let elapsed = pacer.now();
    let max_allowed = elapsed / (20 * MS) + 2;
    assert!(
        u64::from(count(1)) <= max_allowed,
        "region ticked {} times in {} ms with a 20 ms floor",
        count(1),
        elapsed / MS
    );
    assert!(count(1) > 0, "but it must still be reacting");
}

#[test]
fn without_a_floor_a_region_reacts_to_every_change() {
    let _g = begin_test();
    let mut f = fixture("0", "1.0");
    let pacer = VirtualTime::new();
    f.sched.arm(pacer.now());

    for i in 0..10i64 {
        f.bb.set(f.watched, i).unwrap();
        // Step until the region has taken its turn.
        for _ in 0..4 {
            if let StepOutcome::Ticked { region: 1, .. } = f.sched.step(&mut f.bb, &pacer) {
                break;
            }
        }
    }
    assert_eq!(count(1), 10);
}

// ── wake-ups ─────────────────────────────────────────────────────────────────

#[test]
fn a_wake_cuts_a_real_wait_short() {
    let _g = begin_test();
    let bus = Arc::new(WakeBus::new());
    let clock = SteadyClock::new();
    let pacer = RealTime::new(clock, SleepStrategy::Yield).with_wake_bus(Arc::clone(&bus));

    let waker: Arc<dyn Waker> = Arc::clone(&bus) as Arc<dyn Waker>;
    let t0 = Instant::now();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        waker.wake();
    });

    // Ask to sleep for a second; the wake should land in ~20 ms.
    let deadline = pacer.now() + SEC;
    pacer.wait_or_wake(deadline);
    let elapsed = t0.elapsed();

    assert!(
        elapsed < Duration::from_millis(500),
        "the wait should have been interrupted, took {elapsed:?}"
    );
    assert!(
        elapsed >= Duration::from_millis(15),
        "and not returned before the wake, took {elapsed:?}"
    );
}

#[test]
fn a_wake_that_lands_before_the_wait_is_not_lost() {
    let bus = WakeBus::new();
    let clock = SteadyClock::new();

    // The classic lost-wakeup: signal, then wait. Taking the generation first is
    // what makes this safe.
    let since = bus.generation();
    bus.wake();

    let t0 = Instant::now();
    let woken = bus.wait_until(&clock, clock.now() + SEC, since);
    assert!(woken);
    assert!(
        t0.elapsed() < Duration::from_millis(100),
        "a wake before the wait must return immediately"
    );
}

#[test]
fn without_a_wake_bus_an_event_driven_region_falls_back_to_its_heartbeat() {
    // Degrading latency rather than correctness is the intended failure mode of
    // forgetting to wire the bus up.
    let _g = begin_test();
    let mut f = fixture("0", "4.0");
    let pacer = VirtualTime::new(); // no wake source at all
    let _ = f.sched.run_for(&mut f.bb, &pacer, SEC, 0);
    assert_eq!(count(1), 4, "four heartbeats, no more");
}

// ── configuration ────────────────────────────────────────────────────────────

#[test]
fn an_unknown_mode_is_an_error() {
    let _g = begin_test();
    let mut b = TreeBuilder::new();
    b.begin("root", Builtin::region())
        .port("hz", "10.0")
        .port("mode", "reactive")
        .leaf_custom("w", &WATCHER, Box::new(Watcher(0)))
        .end();
    let mut bb = Blackboard::new();
    let graph = b.build(&mut bb).unwrap();
    assert!(matches!(
        Scheduler::new(Tree::new(graph), &bb, 10.0),
        Err(SchedError::BadMode { .. })
    ));
}

#[test]
fn the_policy_types_describe_themselves() {
    let p = TickPolicy::periodic_hz(50.0).unwrap();
    assert_eq!(p.heartbeat(), 20 * MS);
    assert!(!p.is_event_driven());

    let e = TickPolicy::on_change(2.0, 5 * MS).unwrap();
    assert_eq!(e.heartbeat(), 500 * MS);
    assert!(e.is_event_driven());
    assert!(TickPolicy::on_change(0.0, 0).is_err());
}
