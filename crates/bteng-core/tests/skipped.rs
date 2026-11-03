//! `Skipped` through every built-in, written against the builder API.
//!
//! The cross-engine shared cases (spec section 6) are in
//! `bteng-xml/tests/preconditions.rs`; this file pins what each built-in does
//! with a skipped child, plus the builder-side precondition API.

use bteng_core::{
    Blackboard, BuildError, Builtin, Ctx, ExprError, NodeId, NodeState, Precondition, SimClock,
    Status, Tree, TreeBuilder,
};

struct H {
    tree: Tree,
    bb: Blackboard,
    clock: SimClock,
}

impl H {
    fn build(f: impl FnOnce(&mut TreeBuilder)) -> Self {
        let mut bb = Blackboard::new();
        bb.declare("skip", true).unwrap();
        bb.declare("cond", true).unwrap();
        let mut b = TreeBuilder::new();
        f(&mut b);
        let graph = b.build(&mut bb).unwrap_or_else(|e| panic!("{e}"));
        Self {
            tree: Tree::new(graph),
            bb,
            clock: SimClock::new(),
        }
    }
    fn tick(&mut self) -> Status {
        let mut ctx = Ctx::new(&mut self.bb, &self.clock);
        self.tree.tick(&mut ctx)
    }
    fn set(&mut self, key: &str, v: bool) {
        let s = self.bb.resolve(key).unwrap();
        self.bb.set(s, v).unwrap();
    }
}

/// A leaf that is always skipped.
fn skipped(b: &mut TreeBuilder, kind: Builtin) {
    b.leaf("skipped", kind)
        .precondition(Precondition::SkipIf, "skip");
}

fn running(_c: &mut Ctx<'_>) -> Status {
    Status::Running
}

// ── control nodes ────────────────────────────────────────────────────────────

/// `parent` over (skipped, `other`) gives `one`; over (skipped, skipped) gives
/// Skipped.
fn composite(parent: fn() -> Builtin, other: Builtin, one: Status) {
    let name = parent().type_name();
    let mut h = H::build(|b| {
        b.begin("p", parent());
        skipped(b, Builtin::AlwaysSuccess);
        b.leaf("other", other);
        b.end();
    });
    assert_eq!(h.tick(), one, "{name}: one child skipped");

    let mut h = H::build(|b| {
        b.begin("p", parent());
        skipped(b, Builtin::AlwaysSuccess);
        skipped(b, Builtin::AlwaysFailure);
        b.end();
    });
    assert_eq!(h.tick(), Status::Skipped, "{name}: all children skipped");
}

#[test]
fn sequences_pass_over_a_skipped_child() {
    for p in [
        Builtin::sequence as fn() -> Builtin,
        Builtin::sequence_with_memory,
        Builtin::reactive_sequence,
    ] {
        composite(p, Builtin::AlwaysSuccess, Status::Success);
        composite(p, Builtin::AlwaysFailure, Status::Failure);
    }
}

#[test]
fn fallbacks_pass_over_a_skipped_child() {
    for p in [
        Builtin::fallback as fn() -> Builtin,
        Builtin::reactive_fallback,
    ] {
        composite(p, Builtin::AlwaysSuccess, Status::Success);
        composite(p, Builtin::AlwaysFailure, Status::Failure);
    }
}

#[test]
fn parallel_leaves_skipped_children_out_of_the_count() {
    composite(Builtin::parallel, Builtin::AlwaysSuccess, Status::Success);
    composite(Builtin::parallel, Builtin::AlwaysFailure, Status::Failure);
}

#[test]
fn a_sequence_counts_children_skipped_on_earlier_ticks() {
    // child 0 skipped on tick 1, child 1 running; then child 1 is skipped by
    // `_while` too. Every child ended up skipped → the sequence is skipped.
    let mut h = H::build(|b| {
        b.begin("seq", Builtin::sequence());
        skipped(b, Builtin::AlwaysSuccess);
        b.leaf("work", Builtin::action(running))
            .precondition(Precondition::While, "cond");
        b.end();
    });
    assert_eq!(h.tick(), Status::Running);
    h.set("cond", false);
    assert_eq!(h.tick(), Status::Skipped);
}

#[test]
fn if_then_else_with_a_skipped_condition_or_branch_is_skipped() {
    // Skipped condition: no branch runs.
    let mut h = H::build(|b| {
        b.begin("ite", Builtin::if_then_else());
        skipped(b, Builtin::AlwaysSuccess);
        b.leaf("then", Builtin::AlwaysSuccess);
        b.leaf("else", Builtin::AlwaysFailure);
        b.end();
    });
    assert_eq!(h.tick(), Status::Skipped);

    // Skipped branch: passed through.
    let mut h = H::build(|b| {
        b.begin("ite", Builtin::if_then_else());
        b.leaf("cond", Builtin::AlwaysSuccess);
        skipped(b, Builtin::AlwaysSuccess);
        b.end();
    });
    assert_eq!(h.tick(), Status::Skipped);
}

#[test]
fn while_do_else_with_a_skipped_condition_stops_its_branch() {
    let mut h = H::build(|b| {
        b.begin("wde", Builtin::while_do_else());
        b.leaf("cond", Builtin::AlwaysSuccess)
            .precondition(Precondition::SkipIf, "!cond");
        b.leaf("do", Builtin::action(running));
        b.leaf("else", Builtin::AlwaysFailure);
        b.end();
    });
    assert_eq!(h.tick(), Status::Running);
    h.set("cond", false);
    assert_eq!(h.tick(), Status::Skipped);
    let g = h.tree.graph();
    assert_eq!(
        g.state(g.child(NodeId::ROOT, 1)),
        NodeState::Idle,
        "branch halted"
    );

    let mut h = H::build(|b| {
        b.begin("wde", Builtin::while_do_else());
        b.leaf("cond", Builtin::AlwaysSuccess);
        skipped(b, Builtin::AlwaysSuccess);
        b.end();
    });
    assert_eq!(h.tick(), Status::Skipped, "skipped branch passes through");
}

// ── decorators ───────────────────────────────────────────────────────────────

#[test]
fn every_decorator_passes_skipped_through() {
    let decorators: [(Builtin, Option<(&str, &str)>); 10] = [
        (Builtin::Inverter, None),
        (Builtin::ForceSuccess, None),
        (Builtin::ForceFailure, None),
        (Builtin::KeepRunningUntilFailure, None),
        (Builtin::repeat(), Some(("num_cycles", "3"))),
        (Builtin::retry(), Some(("num_attempts", "3"))),
        (Builtin::run_once(), None),
        (Builtin::timeout(), Some(("msec", "100"))),
        (Builtin::delay(), Some(("delay_msec", "0"))),
        (Builtin::SubTree, None),
    ];
    for (d, port) in decorators {
        let name = d.type_name();
        let mut h = H::build(|b| {
            b.begin("d", d);
            skipped(b, Builtin::AlwaysSuccess);
            b.end();
            if let Some((p, v)) = port {
                b.port(p, v);
            }
        });
        assert_eq!(h.tick(), Status::Skipped, "{name}");
        // And again: nothing latched.
        assert_eq!(h.tick(), Status::Skipped, "{name}, second tick");
    }
}

#[test]
fn repeat_starts_over_after_its_child_is_skipped() {
    let mut h = H::build(|b| {
        b.begin("rep", Builtin::repeat()).port("num_cycles", "2");
        b.leaf("x", Builtin::AlwaysSuccess)
            .precondition(Precondition::SkipIf, "skip");
        b.end();
    });
    h.set("skip", false);
    assert_eq!(h.tick(), Status::Running, "1 of 2 cycles");
    h.set("skip", true);
    assert_eq!(h.tick(), Status::Skipped);
    h.set("skip", false);
    assert_eq!(
        h.tick(),
        Status::Running,
        "the count restarted, 1 of 2 again"
    );
    assert_eq!(h.tick(), Status::Success);
}

#[test]
fn run_once_does_not_count_a_skipped_child_as_its_run() {
    let mut h = H::build(|b| {
        b.begin("once", Builtin::run_once());
        b.leaf("x", Builtin::AlwaysFailure)
            .precondition(Precondition::SkipIf, "skip");
        b.end().port("then_skip", "false");
    });
    assert_eq!(h.tick(), Status::Skipped);
    h.set("skip", false);
    assert_eq!(h.tick(), Status::Failure, "the real run");
    h.set("skip", true);
    assert_eq!(h.tick(), Status::Failure, "cached, child not consulted");
}

// ── preconditions on control nodes and leaves ────────────────────────────────

#[test]
fn a_precondition_on_a_control_node_skips_its_whole_subtree() {
    let mut h = H::build(|b| {
        b.begin("root", Builtin::sequence());
        b.begin("branch", Builtin::sequence())
            .leaf("a", Builtin::action(running))
            .end()
            .precondition(Precondition::SkipIf, "skip");
        b.leaf("b", Builtin::AlwaysSuccess);
        b.end();
    });
    assert_eq!(h.tick(), Status::Success);
    let g = h.tree.graph();
    let branch = g.child(NodeId::ROOT, 0);
    assert_eq!(g.state(g.child(branch, 0)), NodeState::Idle, "never ticked");
}

#[test]
fn success_if_and_failure_if_decide_without_ticking() {
    let mut h = H::build(|b| {
        b.leaf("x", Builtin::action(running))
            .precondition(Precondition::SuccessIf, "cond");
    });
    assert_eq!(h.tick(), Status::Success);
    let mut h = H::build(|b| {
        b.leaf("x", Builtin::action(running))
            .precondition(Precondition::FailureIf, "cond && skip");
    });
    assert_eq!(h.tick(), Status::Failure);
}

#[test]
fn a_precondition_keeps_its_source_text() {
    let mut h = H::build(|b| {
        b.leaf("x", Builtin::AlwaysSuccess)
            .precondition(Precondition::While, "false");
    });
    assert_eq!(h.tick(), Status::Skipped);
    let g = h.tree.graph();
    let pre = g.preconditions(NodeId::ROOT).unwrap();
    assert_eq!(pre.source(Precondition::While), Some("false"));
}

#[test]
fn the_builder_rejects_bad_preconditions() {
    let build = |f: &dyn Fn(&mut TreeBuilder)| {
        let mut bb = Blackboard::new();
        bb.declare("n", 1u8).unwrap();
        let mut b = TreeBuilder::new();
        f(&mut b);
        b.build(&mut bb).map(|_| ())
    };
    assert!(matches!(
        build(&|b| {
            b.leaf("x", Builtin::AlwaysSuccess)
                .precondition(Precondition::SkipIf, "n > 0")
                .precondition(Precondition::SkipIf, "n > 1");
        }),
        Err(BuildError::DuplicatePrecondition {
            attribute: "_skipIf",
            ..
        })
    ));
    assert!(matches!(
        build(&|b| {
            b.leaf("x", Builtin::AlwaysSuccess)
                .precondition(Precondition::While, "ghost");
        }),
        Err(BuildError::Precondition {
            source: ExprError::UnknownKey { .. },
            ..
        })
    ));
    assert!(matches!(
        build(&|b| {
            b.leaf("x", Builtin::AlwaysSuccess)
                .precondition(Precondition::While, "len(n) > 1");
        }),
        Err(BuildError::Precondition {
            source: ExprError::Forbidden { .. },
            ..
        })
    ));
}

// ── the leaf contract ────────────────────────────────────────────────────────

fn rogue(_c: &mut Ctx<'_>) -> Status {
    Status::Skipped
}

/// A leaf has no precondition of its own to report and no child to pass one up
/// from, so returning Skipped from its tick is a bug in the leaf: loud in
/// debug, Failure in release.
#[test]
#[cfg_attr(debug_assertions, should_panic(expected = "returned Skipped"))]
fn a_leaf_returning_skipped_is_a_contract_violation() {
    let mut h = H::build(|b| {
        b.leaf("rogue", Builtin::action(rogue));
    });
    assert_eq!(h.tick(), Status::Failure);
}
