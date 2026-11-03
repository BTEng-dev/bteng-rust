//! `Skipped` and the four preconditions, driven from XML.
//!
//! These are the shared cases of the cross-engine spec
//! (`bteng/docs/design/skipped-status.md`, section 6), numbered as there. The
//! Python engine implements the same list; a case here and its twin there must
//! agree. Propagation through every built-in, written against the builder API,
//! lives in `bteng-core/tests/skipped.rs`.

use core::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard};

use bteng_core::{
    Arity, Blackboard, BuildError, Ctx, Cursor, ExprError, Node, NodeCategory, NodeEvent, NodeId,
    NodeKind, NodeManifest, NodeRegistry, NodeState, Precondition, SimClock, Status, TraceSink,
    Tree,
};
use bteng_xml::{XmlError, XmlLoader};

// ── test nodes ───────────────────────────────────────────────────────────────

static BUSY_TICKS: AtomicU32 = AtomicU32::new(0);
static BUSY_HALTS: AtomicU32 = AtomicU32::new(0);
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    let g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    BUSY_TICKS.store(0, Ordering::Relaxed);
    BUSY_HALTS.store(0, Ordering::Relaxed);
    g
}

static BUSY: NodeManifest = NodeManifest::new("Busy", NodeCategory::Action, Arity::Leaf, &[]);

/// Runs forever, counting ticks and halts.
struct Busy;

impl Node for Busy {
    fn tick(&mut self, _cur: &mut Cursor<'_, '_>) -> Status {
        BUSY_TICKS.fetch_add(1, Ordering::Relaxed);
        Status::Running
    }
    fn halt(&mut self, _cur: &mut Cursor<'_, '_>) {
        BUSY_HALTS.fetch_add(1, Ordering::Relaxed);
    }
    fn type_name(&self) -> &'static str {
        "Busy"
    }
}

fn registry() -> NodeRegistry {
    let mut reg = NodeRegistry::with_builtins();
    reg.register(&BUSY, || NodeKind::Custom(Box::new(Busy)))
        .unwrap();
    reg
}

/// Every transition, for the "no Skipped ever" and "by" checks.
#[derive(Default)]
struct Log(Vec<NodeEvent>);

impl TraceSink for Log {
    fn node_event(&mut self, e: NodeEvent) {
        self.0.push(e);
    }
}

struct H {
    tree: Tree,
    bb: Blackboard,
    clock: SimClock,
    log: Log,
}

impl H {
    fn tick(&mut self) -> Status {
        let mut ctx = Ctx::new(&mut self.bb, &self.clock).with_trace(&mut self.log);
        self.tree.tick(&mut ctx)
    }
    fn halt(&mut self) {
        let mut ctx = Ctx::new(&mut self.bb, &self.clock);
        self.tree.halt(&mut ctx);
    }
    fn set<T: Send + Sync + 'static>(&mut self, key: &str, v: T) {
        let slot = self.bb.resolve(key).expect("key declared");
        self.bb.set(slot, v).unwrap();
    }
}

fn wrap(body: &str) -> String {
    format!(r#"<root BTCPP_format="4"><BehaviorTree ID="main">{body}</BehaviorTree></root>"#)
}

/// Load `body` against a blackboard prepared by `prep`.
fn try_load(xml: &str, prep: impl FnOnce(&mut Blackboard)) -> Result<H, XmlError> {
    let reg = registry();
    let mut bb = Blackboard::new();
    prep(&mut bb);
    let graph = XmlLoader::new(&reg).load(xml, &mut bb)?;
    Ok(H {
        tree: Tree::new(graph),
        bb,
        clock: SimClock::new(),
        log: Log::default(),
    })
}

fn load(body: &str) -> H {
    try_load(&wrap(body), |bb| {
        bb.declare("skip", true).unwrap();
        bb.declare("go", true).unwrap();
        bb.declare("battery", 50i32).unwrap();
    })
    .unwrap_or_else(|e| panic!("{e}\n{body}"))
}

fn once(body: &str) -> Status {
    load(body).tick()
}

// ── 1. every row of the propagation table ────────────────────────────────────

#[test]
fn case1_sequence_and_fallback_pass_over_a_skipped_child() {
    let _g = serial();
    // One skipped: passed over, the rest decides.
    assert_eq!(
        once(r#"<Sequence><AlwaysFailure _skipIf="skip"/><AlwaysSuccess/></Sequence>"#),
        Status::Success
    );
    assert_eq!(
        once(r#"<Sequence><AlwaysSuccess _skipIf="skip"/><AlwaysFailure/></Sequence>"#),
        Status::Failure
    );
    assert_eq!(
        once(r#"<Fallback><AlwaysSuccess _skipIf="skip"/><AlwaysFailure/></Fallback>"#),
        Status::Failure,
        "a skipped child must not count as a success"
    );
    assert_eq!(
        once(r#"<Fallback><AlwaysFailure _skipIf="skip"/><AlwaysSuccess/></Fallback>"#),
        Status::Success
    );
    // All skipped: the parent is skipped.
    for tag in ["Sequence", "Fallback", "SequenceWithMemory"] {
        assert_eq!(
            once(&format!(
                r#"<{tag}><AlwaysSuccess _skipIf="skip"/><AlwaysFailure _skipIf="skip"/></{tag}>"#
            )),
            Status::Skipped,
            "{tag}"
        );
    }
}

#[test]
fn case1_reactive_nodes_pass_over_a_skipped_child() {
    let _g = serial();
    assert_eq!(
        once(
            r#"<ReactiveSequence><AlwaysFailure _skipIf="skip"/><AlwaysSuccess/></ReactiveSequence>"#
        ),
        Status::Success
    );
    assert_eq!(
        once(
            r#"<ReactiveFallback><AlwaysSuccess _skipIf="skip"/><AlwaysFailure/></ReactiveFallback>"#
        ),
        Status::Failure
    );
    for tag in ["ReactiveSequence", "ReactiveFallback"] {
        assert_eq!(
            once(&format!(
                r#"<{tag}><AlwaysSuccess _skipIf="skip"/><AlwaysFailure _skipIf="skip"/></{tag}>"#
            )),
            Status::Skipped,
            "{tag}"
        );
    }
}

#[test]
fn case1_parallel_removes_skipped_children_from_the_count() {
    let _g = serial();
    // Default thresholds (all must succeed, one failure fails): the skipped
    // failure is out of the count, so the remaining child decides.
    assert_eq!(
        once(r#"<Parallel><AlwaysSuccess/><AlwaysFailure _skipIf="skip"/></Parallel>"#),
        Status::Success
    );
    // An explicit threshold is clamped to the children that count.
    assert_eq!(
        once(
            r#"<Parallel success_count="3"><AlwaysSuccess/><AlwaysSuccess/><AlwaysSuccess _skipIf="skip"/></Parallel>"#
        ),
        Status::Success
    );
    // All skipped → Skipped (BT.CPP would say Success here; deliberate).
    assert_eq!(
        once(
            r#"<Parallel><AlwaysSuccess _skipIf="skip"/><AlwaysSuccess _skipIf="skip"/></Parallel>"#
        ),
        Status::Skipped
    );
}

#[test]
fn case1_parallel_reticks_a_skipped_child_so_it_can_join_in() {
    let _g = serial();
    let mut h = load(r#"<Parallel><Busy/><AlwaysSuccess _skipIf="skip"/></Parallel>"#);
    assert_eq!(h.tick(), Status::Running);
    h.set("skip", false);
    assert_eq!(h.tick(), Status::Running);
    let par = h.tree.graph().child(NodeId::ROOT, 1);
    assert_eq!(h.tree.graph().state(par), NodeState::Success);
}

#[test]
fn case1_decorators_and_subtree_pass_skipped_through() {
    let _g = serial();
    for tag in [
        "Inverter",
        "ForceSuccess",
        "ForceFailure",
        "RetryUntilSuccessful",
        "Repeat",
        "KeepRunningUntilFailure",
        "RunOnce",
    ] {
        assert_eq!(
            once(&format!(
                r#"<{tag}><AlwaysSuccess _skipIf="skip"/></{tag}>"#
            )),
            Status::Skipped,
            "{tag}"
        );
    }
    assert_eq!(
        once(r#"<Timeout msec="100"><AlwaysSuccess _skipIf="skip"/></Timeout>"#),
        Status::Skipped
    );
    assert_eq!(
        once(r#"<Delay delay_msec="0"><AlwaysSuccess _skipIf="skip"/></Delay>"#),
        Status::Skipped
    );

    let xml = r#"<root BTCPP_format="4" main_tree_to_execute="main">
        <BehaviorTree ID="main"><SubTree ID="inner"/></BehaviorTree>
        <BehaviorTree ID="inner"><AlwaysSuccess _skipIf="true"/></BehaviorTree>
    </root>"#;
    let mut h = try_load(xml, |_| {}).unwrap();
    assert_eq!(h.tick(), Status::Skipped, "SubTree is transparent");
}

// ── 2. precondition order ────────────────────────────────────────────────────

#[test]
fn case2_failure_if_beats_success_if_beats_skip_if() {
    let _g = serial();
    let xml = wrap(r#"<Busy _failureIf="f" _successIf="s" _skipIf="k"/>"#);
    let run = |f: bool, s: bool, k: bool| {
        let mut h = try_load(&xml, |bb| {
            bb.declare("f", f).unwrap();
            bb.declare("s", s).unwrap();
            bb.declare("k", k).unwrap();
        })
        .unwrap();
        h.tick()
    };
    assert_eq!(run(true, true, true), Status::Failure);
    assert_eq!(run(false, true, true), Status::Success);
    assert_eq!(run(false, false, true), Status::Skipped);
    assert_eq!(run(false, false, false), Status::Running);
    assert_eq!(
        BUSY_TICKS.load(Ordering::Relaxed),
        1,
        "tick() runs only when no precondition decides"
    );
}

#[test]
fn case2_a_decided_result_is_traced_with_its_precondition() {
    let _g = serial();
    let mut h = load(r#"<Busy _skipIf="battery > 20"/>"#);
    assert_eq!(h.tick(), Status::Skipped);
    let e = h.log.0.last().expect("a transition was traced");
    assert_eq!((e.from, e.to), (NodeState::Idle, NodeState::Skipped));
    assert_eq!(e.by, Some(Precondition::SkipIf));
}

// ── 3. _skipIf is not re-checked while Running ───────────────────────────────

#[test]
fn case3_skip_if_is_ignored_while_running() {
    let _g = serial();
    let mut h = load(r#"<Busy _skipIf="!go"/>"#);
    assert_eq!(h.tick(), Status::Running);
    h.set("go", false);
    assert_eq!(
        h.tick(),
        Status::Running,
        "a running node is not interrupted"
    );
    assert_eq!(BUSY_TICKS.load(Ordering::Relaxed), 2);

    // Once it is no longer running, the precondition applies again.
    h.halt();
    assert_eq!(h.tick(), Status::Skipped);
    assert_eq!(BUSY_TICKS.load(Ordering::Relaxed), 2);
}

// ── 4. _while turning false halts once, returns Skipped ──────────────────────

#[test]
fn case4_while_false_halts_a_running_node_once() {
    let _g = serial();
    let mut h = load(r#"<Sequence><Busy _while="go"/><AlwaysSuccess/></Sequence>"#);
    assert_eq!(h.tick(), Status::Running);
    assert_eq!(BUSY_HALTS.load(Ordering::Relaxed), 0);

    h.set("go", false);
    // Busy is halted and skipped; the sequence passes over it.
    assert_eq!(h.tick(), Status::Success);
    assert_eq!(BUSY_HALTS.load(Ordering::Relaxed), 1, "halted exactly once");
    assert_eq!(
        BUSY_TICKS.load(Ordering::Relaxed),
        1,
        "not ticked once skipped"
    );

    assert_eq!(h.tick(), Status::Success);
    assert_eq!(
        BUSY_HALTS.load(Ordering::Relaxed),
        1,
        "a skipped node is not running, so it is not halted again"
    );

    // The trace shows the halt, then the skip.
    let busy = h.tree.graph().uid(h.tree.graph().child(NodeId::ROOT, 0));
    let mine: Vec<_> = h.log.0.iter().filter(|e| e.uid == busy).collect();
    assert_eq!(mine[1].to, NodeState::Idle);
    assert_eq!(mine[2].to, NodeState::Skipped);
    assert_eq!(mine[2].by, Some(Precondition::While));
}

#[test]
fn case4_while_false_on_an_idle_node_skips_it() {
    let _g = serial();
    let mut h = load(r#"<Busy _while="!go"/>"#);
    assert_eq!(h.tick(), Status::Skipped);
    assert_eq!(BUSY_TICKS.load(Ordering::Relaxed), 0);
    assert_eq!(BUSY_HALTS.load(Ordering::Relaxed), 0);
}

// ── 5. ForceSuccess / ForceFailure ───────────────────────────────────────────

#[test]
fn case5_force_nodes_pass_skipped_through() {
    let _g = serial();
    assert_eq!(
        once(r#"<ForceSuccess><AlwaysFailure _skipIf="skip"/></ForceSuccess>"#),
        Status::Skipped
    );
    assert_eq!(
        once(r#"<ForceFailure><AlwaysSuccess _skipIf="skip"/></ForceFailure>"#),
        Status::Skipped
    );
    // And "all skipped" reaches the parent through them.
    assert_eq!(
        once(
            r#"<Sequence><ForceSuccess><AlwaysFailure _skipIf="skip"/></ForceSuccess></Sequence>"#
        ),
        Status::Skipped
    );
}

// ── 6. missing key ───────────────────────────────────────────────────────────

#[test]
fn case6_an_unknown_key_fails_the_build_and_names_it() {
    let _g = serial();
    let err = try_load(
        &wrap(r#"<AlwaysSuccess name="dock" _skipIf="batery &lt; 20"/>"#),
        |bb| {
            bb.declare("battery", 50i32).unwrap();
        },
    )
    .err()
    .expect("a typo in a key must not load");
    match &err {
        XmlError::Build(BuildError::Precondition {
            name,
            attribute,
            source: ExprError::UnknownKey { key },
            ..
        }) => {
            assert_eq!(name, "dock");
            assert_eq!(*attribute, "_skipIf");
            assert_eq!(key, "batery");
        }
        other => panic!("wrong error: {other:?}"),
    }
    let text = err.to_string();
    assert!(text.contains("batery") && text.contains("_skipIf") && text.contains("dock"));
}

#[test]
fn case6_a_key_bound_only_by_a_port_counts_as_existing() {
    let _g = serial();
    // `n` exists only because Repeat's port binds it; the precondition on the
    // earlier sibling must still resolve it.
    let mut h = try_load(
        &wrap(
            r#"<Sequence><AlwaysSuccess _skipIf="n == 3"/>
               <Repeat num_cycles="{n}"><AlwaysSuccess/></Repeat></Sequence>"#,
        ),
        |_| {},
    )
    .unwrap();
    // n defaults to 0: not skipped, and Repeat with 0 cycles is done at once.
    assert_eq!(h.tick(), Status::Success);
}

// ── 7. both operator spellings ───────────────────────────────────────────────

#[test]
fn case7_c_and_python_spellings_agree() {
    let _g = serial();
    let pairs = [
        ("a && b", "a and b"),
        ("a || b", "a or b"),
        ("!a", "not a"),
        ("!(a && b) || n > 2", "not (a and b) or n > 2"),
        ("a && !b || !a && b", "a and not b or not a and b"),
    ];
    for a in [false, true] {
        for b in [false, true] {
            for n in [0i64, 5] {
                for (c, py) in pairs {
                    let eval = |expr: &str| {
                        let mut h = try_load(
                            &wrap(&format!(
                                r#"<AlwaysSuccess _skipIf="{}"/>"#,
                                expr.replace('&', "&amp;")
                            )),
                            |bb| {
                                bb.declare("a", a).unwrap();
                                bb.declare("b", b).unwrap();
                                bb.declare("n", n).unwrap();
                            },
                        )
                        .unwrap();
                        h.tick()
                    };
                    assert_eq!(eval(c), eval(py), "{c} vs {py} at a={a} b={b} n={n}");
                }
            }
        }
    }
}

// ── 8. forbidden expressions fail at load ────────────────────────────────────

#[test]
fn case8_calls_and_attribute_access_fail_at_load() {
    let _g = serial();
    for expr in [
        "max(battery, 3) > 1",
        "battery.real > 1",
        "items[0] == 1",
        "battery = 3",
    ] {
        let err = try_load(
            &wrap(&format!(r#"<AlwaysSuccess _skipIf="{expr}"/>"#)),
            |bb| {
                bb.declare("battery", 1i32).unwrap();
            },
        )
        .err()
        .unwrap_or_else(|| panic!("{expr} should not load"));
        assert!(
            matches!(
                err,
                XmlError::BadPrecondition {
                    source: ExprError::Forbidden { .. },
                    ..
                }
            ),
            "{expr}: {err:?}"
        );
    }
}

#[test]
fn case8_other_underscore_attributes_fail_at_load() {
    let _g = serial();
    for attr in ["_onSuccess", "_post", "_skipif", "_whatever"] {
        let err = try_load(&wrap(&format!(r#"<AlwaysSuccess {attr}="true"/>"#)), |_| {})
            .err()
            .unwrap_or_else(|| panic!("{attr} should not load"));
        assert!(
            matches!(&err, XmlError::UnknownSpecialAttribute { attribute, .. } if attribute == attr),
            "{attr}: {err:?}"
        );
    }
    // The reserved ones still load.
    assert!(
        try_load(
            &wrap(r#"<AlwaysSuccess _description="d" _uid="7" name="n"/>"#),
            |_| {}
        )
        .is_ok()
    );
}

#[test]
fn case8_type_errors_fail_at_build() {
    let _g = serial();
    for expr in ["battery", "battery &amp;&amp; skip", "'low' &lt; 3"] {
        let err = try_load(
            &wrap(&format!(r#"<AlwaysSuccess _skipIf="{expr}"/>"#)),
            |bb| {
                bb.declare("battery", 1i32).unwrap();
                bb.declare("skip", true).unwrap();
            },
        )
        .err()
        .unwrap_or_else(|| panic!("{expr} should not build"));
        assert!(
            matches!(
                err,
                XmlError::Build(BuildError::Precondition {
                    source: ExprError::Type { .. },
                    ..
                })
            ),
            "{expr}: {err:?}"
        );
    }
}

// ── 9. no preconditions, no change ───────────────────────────────────────────

#[test]
fn case9_a_tree_without_preconditions_never_sees_skipped() {
    let _g = serial();
    let mut h = load(
        r#"<Sequence>
             <Fallback><AlwaysFailure/><Inverter><AlwaysFailure/></Inverter></Fallback>
             <ReactiveSequence><AlwaysSuccess/><ForceFailure><AlwaysSuccess/></ForceFailure></ReactiveSequence>
           </Sequence>"#,
    );
    // Same statuses as before the feature existed: the reactive sequence fails
    // on its ForceFailure, so the root fails, every tick.
    for _ in 0..3 {
        assert_eq!(h.tick(), Status::Failure);
    }
    let g = h.tree.graph();
    assert!(g.ids().all(|id| g.preconditions(id).is_none()));
    assert!(
        h.log
            .0
            .iter()
            .all(|e| e.to != NodeState::Skipped && e.by.is_none())
    );
}

// ── subtree namespaces ───────────────────────────────────────────────────────

#[test]
fn names_inside_a_subtree_go_through_its_remapping() {
    let _g = serial();
    let doc = |call: &str| {
        format!(
            r#"<root BTCPP_format="4" main_tree_to_execute="main">
                 <BehaviorTree ID="main">{call}</BehaviorTree>
                 <BehaviorTree ID="inner"><AlwaysSuccess _skipIf="lvl &lt; 20"/></BehaviorTree>
               </root>"#
        )
    };
    let with = |call: &str, battery: i32| {
        let mut h = try_load(&doc(call), |bb| {
            bb.declare("battery", battery).unwrap();
        })
        .unwrap_or_else(|e| panic!("{e}"));
        h.tick()
    };
    // A key remap: inner's `lvl` is main's `battery`.
    assert_eq!(
        with(r#"<SubTree ID="inner" lvl="{battery}"/>"#, 10),
        Status::Skipped
    );
    assert_eq!(
        with(r#"<SubTree ID="inner" lvl="{battery}"/>"#, 90),
        Status::Success
    );
    // A literal remap.
    assert_eq!(
        with(r#"<SubTree ID="inner" lvl="5"/>"#, 90),
        Status::Skipped
    );
    // Unmapped, the name lives in the subtree's own namespace — which has no
    // `lvl`, and the error says which key it looked for.
    let err = try_load(&doc(r#"<SubTree ID="inner"/>"#), |bb| {
        bb.declare("battery", 1i32).unwrap();
    })
    .err()
    .unwrap();
    assert!(err.to_string().contains("inner#1/lvl"), "{err}");
}

#[test]
fn a_precondition_on_the_subtree_element_reads_the_calling_tree() {
    let _g = serial();
    let xml = r#"<root BTCPP_format="4" main_tree_to_execute="main">
        <BehaviorTree ID="main"><SubTree ID="inner" _skipIf="battery &lt; 20"/></BehaviorTree>
        <BehaviorTree ID="inner"><Busy/></BehaviorTree>
    </root>"#;
    let mut h = try_load(xml, |bb| {
        bb.declare("battery", 10i32).unwrap();
    })
    .unwrap();
    assert_eq!(h.tick(), Status::Skipped);
    assert_eq!(BUSY_TICKS.load(Ordering::Relaxed), 0);
    // And it guards the SubTree marker, not a remap called `_skipIf`.
    let g = h.tree.graph();
    assert!(g.preconditions(NodeId::ROOT).is_some());
}
