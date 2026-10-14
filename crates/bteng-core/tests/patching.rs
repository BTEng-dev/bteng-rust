//! Live structural edits.
//!
//! The claim under test is the one Python BTEng cannot make: a subtree can be
//! swapped while a sibling is `Running`, and the sibling does not notice.

use core::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard};

use bteng_core::{
    Blackboard, BuildError, Builtin, Ctx, NodeId, NodeState, SimClock, Status, TreeBuilder,
    TreeGraph, TreePatch, Uid, apply, tick_node,
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
leaf!(ok2, 2, Status::Success);
leaf!(run3, 3, Status::Running);

struct World {
    graph: TreeGraph,
    bb: Blackboard,
    clock: SimClock,
}

impl World {
    fn tick(&mut self) -> Status {
        let mut ctx = Ctx::new(&mut self.bb, &self.clock);
        tick_node(&mut self.graph, NodeId::ROOT, &mut ctx)
    }

    fn uid_of(&self, name: &str) -> Uid {
        self.graph
            .ids()
            .find(|id| self.graph.name(*id) == name)
            .map(|id| self.graph.uid(id))
            .unwrap_or_else(|| panic!("no node named {name}"))
    }

    fn id_of(&self, name: &str) -> NodeId {
        self.graph
            .ids()
            .find(|id| self.graph.name(*id) == name)
            .unwrap_or_else(|| panic!("no node named {name}"))
    }

    fn state_of(&self, name: &str) -> NodeState {
        self.graph.state(self.id_of(name))
    }
}

/// A `Parallel` of two long-running branches, so one can be edited while the
/// other is mid-run.
fn world() -> World {
    let mut b = TreeBuilder::new();
    b.begin("root", Builtin::parallel())
        .begin("keeper", Builtin::sequence())
        .leaf("keeper_a", Builtin::action(ok2))
        .leaf("keeper_b", Builtin::action(run0))
        .end()
        .begin("victim", Builtin::sequence())
        .leaf("victim_work", Builtin::action(run1))
        .end()
        .end();
    let mut bb = Blackboard::new();
    let graph = b.build(&mut bb).expect("builds");
    World {
        graph,
        bb,
        clock: SimClock::new(),
    }
}

/// A replacement subtree, built against the same blackboard.
fn replacement(bb: &mut Blackboard) -> TreeGraph {
    let mut b = TreeBuilder::new();
    b.begin("newcomer", Builtin::sequence())
        .leaf("new_work", Builtin::action(run3))
        .end();
    b.build(bb).expect("replacement builds")
}

// ── the claim ────────────────────────────────────────────────────────────────

#[test]
fn a_subtree_is_swapped_without_disturbing_a_running_sibling() {
    let _g = begin_test();
    let mut w = world();

    assert_eq!(w.tick(), Status::Running);
    assert_eq!(count(2), 1, "keeper_a ran once and succeeded");
    assert_eq!(count(0), 1);
    assert_eq!(count(1), 1);
    assert_eq!(w.state_of("keeper_b"), NodeState::Running);

    let victim = w.uid_of("victim");
    let keeper_uid = w.uid_of("keeper");
    let incoming = replacement(&mut w.bb);
    let plan = TreePatch::Replace { target: victim }
        .validate(&w.graph, Some(&incoming))
        .expect("valid");

    let out = apply(
        core::mem::replace(&mut w.graph, incoming_placeholder()),
        Some(incoming),
        plan,
    );
    w.graph = out.graph;

    // The sibling kept its identity, its state, and — the part that matters —
    // its Sequence cursor. Python BTEng halts the parent on any edit, which
    // restarts every branch under it.
    assert_eq!(w.uid_of("keeper"), keeper_uid);
    assert_eq!(w.state_of("keeper_b"), NodeState::Running);

    assert_eq!(w.tick(), Status::Running);
    assert_eq!(
        count(2),
        1,
        "keeper_a must NOT have re-run: the sequence kept its cursor across the patch"
    );
    assert_eq!(count(3), 1, "the new subtree is live");
    assert_eq!(count(1), 1, "the replaced subtree is gone");
}

#[test]
fn rollback_restores_the_previous_subtree_with_its_state() {
    let _g = begin_test();
    let mut w = world();
    w.tick();
    w.tick();
    assert_eq!(count(1), 2, "victim_work has been running a while");

    let victim = w.uid_of("victim");
    let incoming = replacement(&mut w.bb);
    let plan = TreePatch::Replace { target: victim }
        .validate(&w.graph, Some(&incoming))
        .unwrap();
    let out = apply(
        core::mem::replace(&mut w.graph, incoming_placeholder()),
        Some(incoming),
        plan,
    );
    w.graph = out.graph;
    let removed = out.removed.expect("Replace hands back what it took");

    assert_eq!(w.graph.version().0, 1);
    w.tick();
    assert_eq!(count(3), 1);

    // Undo. The inverse targets whatever uid the newcomer was given.
    let back = out
        .inverse
        .validate(&w.graph, Some(&removed))
        .expect("the inverse is valid against the patched tree");
    let out2 = apply(
        core::mem::replace(&mut w.graph, incoming_placeholder()),
        Some(removed),
        back,
    );
    w.graph = out2.graph;

    assert_eq!(w.uid_of("victim"), victim, "the original uid is back");
    assert_eq!(
        w.state_of("victim_work"),
        NodeState::Running,
        "and so is the state it had when it left"
    );
    assert_eq!(w.graph.version().0, 2, "versions count edits, not undos");

    w.tick();
    assert_eq!(count(1), 3, "the restored subtree carries on");
    assert_eq!(count(3), 1, "the newcomer is gone");
}

// ── the other two ops ────────────────────────────────────────────────────────

#[test]
fn a_child_can_be_inserted_at_a_position() {
    let _g = begin_test();
    let mut w = world();
    w.tick();

    let root = w.uid_of("root");
    let incoming = replacement(&mut w.bb);
    let plan = TreePatch::InsertChild {
        parent: root,
        index: 0,
    }
    .validate(&w.graph, Some(&incoming))
    .unwrap();
    let out = apply(
        core::mem::replace(&mut w.graph, incoming_placeholder()),
        Some(incoming),
        plan,
    );
    w.graph = out.graph;

    assert_eq!(w.graph.child_count(NodeId::ROOT), 3);
    assert_eq!(
        w.graph.name(w.graph.child(NodeId::ROOT, 0)),
        "newcomer",
        "inserted where it was asked to go"
    );
    assert!(out.removed.is_none());
    w.tick();
    assert_eq!(count(3), 1);
}

#[test]
fn a_child_can_be_removed_and_put_back() {
    let _g = begin_test();
    let mut w = world();
    w.tick();

    let root = w.uid_of("root");
    let plan = TreePatch::RemoveChild {
        parent: root,
        index: 1,
    }
    .validate(&w.graph, None)
    .unwrap();
    let out = apply(
        core::mem::replace(&mut w.graph, incoming_placeholder()),
        None,
        plan,
    );
    w.graph = out.graph;
    let removed = out.removed.expect("RemoveChild hands back what it took");

    assert_eq!(w.graph.child_count(NodeId::ROOT), 1);
    w.tick();
    assert_eq!(count(1), 1, "the removed branch does not tick");

    let back = out.inverse.validate(&w.graph, Some(&removed)).unwrap();
    let out2 = apply(
        core::mem::replace(&mut w.graph, incoming_placeholder()),
        Some(removed),
        back,
    );
    w.graph = out2.graph;
    assert_eq!(w.graph.child_count(NodeId::ROOT), 2);
    w.tick();
    assert_eq!(count(1), 2, "and now it does again");
}

// ── validation happens before anything moves ─────────────────────────────────

#[test]
fn an_edit_that_would_break_arity_is_refused() {
    let _g = begin_test();
    let mut b = TreeBuilder::new();
    b.begin("root", Builtin::sequence())
        .begin("inv", Builtin::Inverter)
        .leaf("only", Builtin::action(run0))
        .end()
        .end();
    let mut bb = Blackboard::new();
    let graph = b.build(&mut bb).unwrap();
    let inv = graph
        .ids()
        .find(|id| graph.name(*id) == "inv")
        .map(|id| graph.uid(id))
        .unwrap();

    let incoming = replacement(&mut bb);
    // A decorator takes exactly one child. Caught here, on the builder thread,
    // rather than by a control node discovering it mid-tick.
    let err = TreePatch::InsertChild {
        parent: inv,
        index: 1,
    }
    .validate(&graph, Some(&incoming))
    .unwrap_err();
    assert!(
        matches!(err, bteng_core::PatchError::Arity { would_be: 2, .. }),
        "got {err:?}"
    );

    let err = TreePatch::RemoveChild {
        parent: inv,
        index: 0,
    }
    .validate(&graph, None)
    .unwrap_err();
    assert!(matches!(
        err,
        bteng_core::PatchError::Arity { would_be: 0, .. }
    ));
}

#[test]
fn structurally_impossible_patches_are_named_precisely() {
    let _g = begin_test();
    let mut w = world();
    let incoming = replacement(&mut w.bb);
    let root = w.uid_of("root");

    use bteng_core::PatchError as E;
    assert!(matches!(
        TreePatch::Replace { target: Uid(9999) }
            .validate(&w.graph, Some(&incoming))
            .unwrap_err(),
        E::NoSuchNode { .. }
    ));
    assert!(matches!(
        TreePatch::Replace { target: root }
            .validate(&w.graph, Some(&incoming))
            .unwrap_err(),
        E::CannotReplaceRoot
    ));
    assert!(matches!(
        TreePatch::Replace {
            target: w.uid_of("victim")
        }
        .validate(&w.graph, None)
        .unwrap_err(),
        E::IncomingMismatch { expected: true }
    ));
    assert!(matches!(
        TreePatch::RemoveChild {
            parent: root,
            index: 0
        }
        .validate(&w.graph, Some(&incoming))
        .unwrap_err(),
        E::IncomingMismatch { expected: false }
    ));
    assert!(matches!(
        TreePatch::RemoveChild {
            parent: root,
            index: 7
        }
        .validate(&w.graph, None)
        .unwrap_err(),
        E::ChildIndexOutOfRange { .. }
    ));
}

// ── invariants the arena depends on ──────────────────────────────────────────

#[test]
fn a_patched_graph_keeps_the_arena_invariants() {
    let _g = begin_test();
    let mut w = world();
    w.tick();
    let victim = w.uid_of("victim");
    let incoming = replacement(&mut w.bb);
    let plan = TreePatch::Replace { target: victim }
        .validate(&w.graph, Some(&incoming))
        .unwrap();
    let out = apply(
        core::mem::replace(&mut w.graph, incoming_placeholder()),
        Some(incoming),
        plan,
    );
    w.graph = out.graph;

    // Root at index 0, parents before children, every uid distinct, every
    // parent link pointing at a node that actually claims the child.
    assert_eq!(w.graph.parent(NodeId::ROOT), NodeId::ROOT);
    let mut uids: Vec<u64> = w.graph.ids().map(|id| w.graph.uid(id).0).collect();
    let before = uids.len();
    uids.sort_unstable();
    uids.dedup();
    assert_eq!(uids.len(), before, "uids must stay unique after a splice");

    for id in w.graph.ids() {
        for &child in w.graph.children_of(id) {
            assert!(child.0 > id.0, "parents must come before children");
            assert_eq!(w.graph.parent(child), id, "parent links must agree");
        }
    }
}

#[test]
fn deep_and_wide_trees_survive_a_splice() {
    let _g = begin_test();
    let mut b = TreeBuilder::new();
    b.begin("root", Builtin::parallel());
    for i in 0..8 {
        b.begin("branch", Builtin::sequence());
        for _ in 0..4 {
            b.begin("nest", Builtin::sequence());
        }
        b.leaf(
            if i == 3 { "target_leaf" } else { "leaf" },
            Builtin::action(run0),
        );
        for _ in 0..4 {
            b.end();
        }
        b.end();
    }
    b.end();
    let mut bb = Blackboard::new();
    let graph = b.build(&mut bb).unwrap();
    let n_before = graph.len();

    let target = graph
        .ids()
        .find(|id| graph.name(*id) == "target_leaf")
        .map(|id| graph.uid(id))
        .unwrap();
    let incoming = replacement(&mut bb);
    let incoming_len = incoming.len();
    let plan = TreePatch::Replace { target }
        .validate(&graph, Some(&incoming))
        .unwrap();
    let out = apply(graph, Some(incoming), plan);

    assert_eq!(out.graph.len(), n_before - 1 + incoming_len);
    assert!(out.graph.ids().any(|id| out.graph.name(id) == "new_work"));
    assert!(
        !out.graph
            .ids()
            .any(|id| out.graph.name(id) == "target_leaf")
    );
}

#[test]
fn a_replacement_subtree_must_not_be_empty() {
    let _g = begin_test();
    let w = world();
    let mut bb = Blackboard::new();
    let mut b = TreeBuilder::new();
    assert!(matches!(b.build(&mut bb), Err(BuildError::Empty)));
    // ...which is why an empty graph cannot even be constructed to hand over.
    let _ = &w;
}

/// A stand-in so a graph can be moved out of the struct it lives in.
fn incoming_placeholder() -> TreeGraph {
    let mut bb = Blackboard::new();
    let mut b = TreeBuilder::new();
    b.leaf("placeholder", Builtin::AlwaysSuccess);
    b.build(&mut bb).expect("placeholder builds")
}

// ── depth is stack ───────────────────────────────────────────────────────────

#[test]
fn a_tree_deeper_than_the_stack_can_take_is_refused_at_build_time() {
    let _g = begin_test();
    let mut b = TreeBuilder::new();
    for _ in 0..(bteng_core::DEFAULT_MAX_DEPTH + 1) {
        b.begin("s", Builtin::sequence());
    }
    b.leaf("l", Builtin::action(run0));
    for _ in 0..(bteng_core::DEFAULT_MAX_DEPTH + 1) {
        b.end();
    }
    let mut bb = Blackboard::new();
    // Measured: ~250 bytes of stack per level, so a default 8 MB thread aborts
    // between 30k and 35k. A stack overflow is a *process abort*, not an error,
    // which makes it the one failure this engine must refuse to reach.
    assert!(matches!(
        b.build(&mut bb).unwrap_err(),
        BuildError::TooDeep { .. }
    ));
}

#[test]
fn the_depth_limit_is_adjustable_for_generated_trees() {
    let _g = begin_test();
    let deep = bteng_core::DEFAULT_MAX_DEPTH + 50;
    let mut b = TreeBuilder::new().with_max_depth(deep + 10);
    for _ in 0..deep {
        b.begin("s", Builtin::sequence());
    }
    b.leaf("l", Builtin::action(run0));
    for _ in 0..deep {
        b.end();
    }
    let mut bb = Blackboard::new();
    assert!(
        b.build(&mut bb).is_ok(),
        "the limit is a default, not a law"
    );
}

#[test]
fn a_patch_cannot_nest_a_tree_past_the_limit() {
    let _g = begin_test();
    // A tree already near the limit, plus a subtree that would push it over.
    let near = bteng_core::DEFAULT_MAX_DEPTH - 4;
    let mut b = TreeBuilder::new();
    for _ in 0..near {
        b.begin("s", Builtin::sequence());
    }
    b.leaf("tip", Builtin::action(run0));
    for _ in 0..near {
        b.end();
    }
    let mut bb = Blackboard::new();
    let graph = b.build(&mut bb).expect("builds");
    let tip = graph
        .ids()
        .find(|id| graph.name(*id) == "tip")
        .map(|id| graph.uid(id))
        .unwrap();

    let mut rb = TreeBuilder::new();
    for _ in 0..10 {
        rb.begin("deep", Builtin::sequence());
    }
    rb.leaf("w", Builtin::action(run3));
    for _ in 0..10 {
        rb.end();
    }
    let incoming = rb.build(&mut bb).expect("subtree builds on its own");

    // Enforcing the limit only against trees nobody edits would not be enforcing
    // it.
    assert!(matches!(
        TreePatch::Replace { target: tip }
            .validate(&graph, Some(&incoming))
            .unwrap_err(),
        bteng_core::PatchError::TooDeep { .. }
    ));
}
