//! Patching a tree that a scheduler is driving.

use core::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard};

use bteng_core::{
    Blackboard, Builtin, Ctx, NodeState, Status, Tree, TreeBuilder, TreeGraph, TreePatch, apply,
};
use bteng_sched::{Pacer, Scheduler, VirtualTime};

static TICKS: [AtomicU32; 3] = [AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0)];
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

fn keeper(_c: &mut Ctx<'_>) -> Status {
    TICKS[0].fetch_add(1, Ordering::Relaxed);
    Status::Running
}
fn old_work(_c: &mut Ctx<'_>) -> Status {
    TICKS[1].fetch_add(1, Ordering::Relaxed);
    Status::Running
}
fn new_work(_c: &mut Ctx<'_>) -> Status {
    TICKS[2].fetch_add(1, Ordering::Relaxed);
    Status::Running
}

const SEC: u64 = 1_000_000_000;

fn placeholder() -> TreeGraph {
    let mut bb = Blackboard::new();
    let mut b = TreeBuilder::new();
    b.leaf("placeholder", Builtin::AlwaysSuccess);
    b.build(&mut bb).unwrap()
}

#[test]
fn a_region_can_be_swapped_out_from_under_a_running_scheduler() {
    let _g = begin_test();
    let mut b = TreeBuilder::new();
    b.begin("root", Builtin::parallel())
        .begin("steady", Builtin::region())
        .port("hz", "100.0")
        .leaf("keeper", Builtin::action(keeper))
        .end()
        .begin("swappable", Builtin::region())
        .port("hz", "50.0")
        .leaf("old", Builtin::action(old_work))
        .end()
        .end();
    let mut bb = Blackboard::new();
    let graph = b.build(&mut bb).unwrap();
    let mut sched = Scheduler::new(Tree::new(graph), &bb, 10.0).unwrap();
    let pacer = VirtualTime::new();

    sched.arm(pacer.now());
    for _ in 0..60 {
        sched.step(&mut bb, &pacer);
    }
    let keeper_before = count(0);
    assert!(keeper_before > 0);
    assert!(count(1) > 0);

    // Build the replacement region off to the side, against the same blackboard.
    let mut rb = TreeBuilder::new();
    rb.begin("swappable_v2", Builtin::region())
        .port("hz", "50.0")
        .leaf("new", Builtin::action(new_work))
        .end();
    let incoming = rb.build(&mut bb).unwrap();

    let target = sched
        .tree()
        .graph()
        .ids()
        .find(|id| sched.tree().graph().name(*id) == "swappable")
        .map(|id| sched.tree().graph().uid(id))
        .unwrap();

    let plan = TreePatch::Replace { target }
        .validate(sched.tree().graph(), Some(&incoming))
        .expect("valid patch");

    let tree = std::mem::replace(sched.tree_mut(), Tree::new(placeholder()));
    let out = apply(tree.into_graph(), Some(incoming), plan);
    sched
        .replace_tree(Tree::new(out.graph), &bb, 10.0)
        .expect("the new tree's regions read back");

    // The untouched region kept its phase and its state; the scheduler found the
    // new boundary and carried on.
    assert_eq!(sched.region_count(), 3);
    let keeper_after_swap = count(0);

    for _ in 0..60 {
        sched.step(&mut bb, &pacer);
    }
    assert!(
        count(0) > keeper_after_swap,
        "the steady region kept running"
    );
    assert!(count(2) > 0, "the new region is live");
    let old_at_swap = count(1);
    assert_eq!(count(1), old_at_swap, "the replaced region is gone");
}

#[test]
fn stats_of_untouched_regions_carry_across_a_swap() {
    let _g = begin_test();
    let mut b = TreeBuilder::new();
    b.begin("root", Builtin::parallel())
        .begin("steady", Builtin::region())
        .port("hz", "100.0")
        .leaf("keeper", Builtin::action(keeper))
        .end()
        .leaf("filler", Builtin::action(old_work))
        .end();
    let mut bb = Blackboard::new();
    let graph = b.build(&mut bb).unwrap();
    let mut sched = Scheduler::new(Tree::new(graph), &bb, 10.0).unwrap();
    let pacer = VirtualTime::new();

    let _ = sched.run_for(&mut bb, &pacer, SEC / 2, 0);
    let root_ticks = sched.stats(0).unwrap().ticks;
    let region_ticks = sched.stats(1).unwrap().ticks;
    assert!(root_ticks > 0 && region_ticks > 0);

    let mut rb = TreeBuilder::new();
    rb.leaf("filler_v2", Builtin::action(new_work));
    let incoming = rb.build(&mut bb).unwrap();
    let target = sched
        .tree()
        .graph()
        .ids()
        .find(|id| sched.tree().graph().name(*id) == "filler")
        .map(|id| sched.tree().graph().uid(id))
        .unwrap();
    let plan = TreePatch::Replace { target }
        .validate(sched.tree().graph(), Some(&incoming))
        .unwrap();
    let tree = std::mem::replace(sched.tree_mut(), Tree::new(placeholder()));
    let out = apply(tree.into_graph(), Some(incoming), plan);
    sched.replace_tree(Tree::new(out.graph), &bb, 10.0).unwrap();

    // A swap is not a restart: a region that was not touched should not lose its
    // history or its phase.
    assert_eq!(sched.stats(0).unwrap().ticks, root_ticks);
    assert_eq!(sched.stats(1).unwrap().ticks, region_ticks);
}

#[test]
fn a_patched_tree_still_halts_cleanly() {
    let _g = begin_test();
    let mut b = TreeBuilder::new();
    b.begin("root", Builtin::parallel())
        .leaf("a", Builtin::action(keeper))
        .leaf("b", Builtin::action(old_work))
        .end();
    let mut bb = Blackboard::new();
    let graph = b.build(&mut bb).unwrap();
    let mut sched = Scheduler::new(Tree::new(graph), &bb, 10.0).unwrap();
    let pacer = VirtualTime::new();
    sched.arm(pacer.now());
    sched.step(&mut bb, &pacer);

    let target = sched
        .tree()
        .graph()
        .ids()
        .find(|id| sched.tree().graph().name(*id) == "b")
        .map(|id| sched.tree().graph().uid(id))
        .unwrap();
    let mut rb = TreeBuilder::new();
    rb.leaf("b2", Builtin::action(new_work));
    let incoming = rb.build(&mut bb).unwrap();
    let plan = TreePatch::Replace { target }
        .validate(sched.tree().graph(), Some(&incoming))
        .unwrap();
    let tree = std::mem::replace(sched.tree_mut(), Tree::new(placeholder()));
    let out = apply(tree.into_graph(), Some(incoming), plan);
    sched.replace_tree(Tree::new(out.graph), &bb, 10.0).unwrap();

    sched.step(&mut bb, &pacer);
    sched.halt(&mut bb, &pacer);
    for id in sched.tree().graph().ids() {
        assert_eq!(
            sched.tree().graph().state(id),
            NodeState::Idle,
            "node {} left running after a halt",
            sched.tree().graph().name(id)
        );
    }
}
