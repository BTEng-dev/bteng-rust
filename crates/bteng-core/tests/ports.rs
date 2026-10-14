//! Ports, the registry, and the node lifecycle.
//!
//! Doubles as the dogfood for DR-6: everything below is written against the
//! public API only, exactly as an out-of-tree OPC UA or ROS 2 crate would.

use bteng_core::{
    Arity, Blackboard, BuildError, Builtin, Ctx, Cursor, Node, NodeCategory, NodeKind,
    NodeManifest, NodeRegistry, PortSpec, RegistryError, SetupError, SimClock, Status, Tree,
    TreeBuilder, input, output,
};

// ── a custom node, declared the way an extension crate would ─────────────────

static ACCUMULATE_PORTS: &[PortSpec] = &[
    input::<i64>("step")
        .with_default("1")
        .described("added to the running total each tick"),
    output::<i64>("total").described("the running total"),
];

static ACCUMULATE: NodeManifest = NodeManifest::new(
    "Accumulate",
    NodeCategory::Action,
    Arity::Leaf,
    ACCUMULATE_PORTS,
)
.described("Adds `step` to a running total and publishes it.");

const PORT_STEP: u16 = 0;
const PORT_TOTAL: u16 = 1;

#[derive(Default)]
struct Accumulate {
    total: i64,
    setup_calls: u32,
}

impl Node for Accumulate {
    fn tick(&mut self, cur: &mut Cursor<'_, '_>) -> Status {
        let Ok(step) = cur.get::<i64>(PORT_STEP) else {
            return Status::Failure;
        };
        self.total += step;
        let total = self.total;
        if cur.set(PORT_TOTAL, total).is_err() {
            return Status::Failure;
        }
        Status::Success
    }

    fn setup(&mut self, _ctx: &mut Ctx<'_>) -> Result<(), SetupError> {
        self.setup_calls += 1;
        Ok(())
    }

    fn type_name(&self) -> &'static str {
        "Accumulate"
    }
}

static BROKEN: NodeManifest = NodeManifest::new("Broken", NodeCategory::Action, Arity::Leaf, &[]);

#[derive(Default)]
struct Broken;

impl Node for Broken {
    fn tick(&mut self, _cur: &mut Cursor<'_, '_>) -> Status {
        Status::Failure
    }
    fn setup(&mut self, _ctx: &mut Ctx<'_>) -> Result<(), SetupError> {
        Err(SetupError::new("no device at /dev/null"))
    }
    fn type_name(&self) -> &'static str {
        "Broken"
    }
}

fn run(tree: &mut Tree, bb: &mut Blackboard, clock: &SimClock) -> Status {
    let mut ctx = Ctx::new(bb, clock);
    tree.tick(&mut ctx)
}

// ── binding ──────────────────────────────────────────────────────────────────

#[test]
fn a_key_binding_reaches_the_blackboard() {
    let mut bb = Blackboard::new();
    let mut b = TreeBuilder::new();
    b.leaf_custom("acc", &ACCUMULATE, Box::new(Accumulate::default()))
        .port("step", "5")
        .port("total", "{sum}");
    let graph = b.build(&mut bb).unwrap();
    let mut tree = Tree::new(graph);
    let clock = SimClock::new();

    let sum = bb
        .resolve("sum")
        .expect("output key was declared by binding");
    assert_eq!(run(&mut tree, &mut bb, &clock), Status::Success);
    assert_eq!(*bb.get::<i64>(sum).unwrap(), 5);
    assert_eq!(run(&mut tree, &mut bb, &clock), Status::Success);
    assert_eq!(*bb.get::<i64>(sum).unwrap(), 10);
}

#[test]
fn a_literal_is_parsed_once_at_build_time() {
    let mut bb = Blackboard::new();
    let before = bb.len();
    let mut b = TreeBuilder::new();
    b.leaf_custom("acc", &ACCUMULATE, Box::new(Accumulate::default()))
        .port("step", "7");
    let graph = b.build(&mut bb).unwrap();

    // The literal became a slot, and the unwired output became a sink slot.
    assert_eq!(bb.len(), before + 2);
    let slot = graph
        .port_slot(bteng_core::NodeId::ROOT, PORT_STEP)
        .unwrap();
    assert_eq!(*bb.get::<i64>(slot).unwrap(), 7);
}

#[test]
fn a_declared_default_applies_when_the_source_says_nothing() {
    let mut bb = Blackboard::new();
    let mut b = TreeBuilder::new();
    b.leaf_custom("acc", &ACCUMULATE, Box::new(Accumulate::default()));
    let graph = b.build(&mut bb).unwrap();
    let mut tree = Tree::new(graph);
    let clock = SimClock::new();

    assert_eq!(run(&mut tree, &mut bb, &clock), Status::Success);
    // step defaulted to 1; total went to an unwired sink, and the node still ran.
}

#[test]
fn a_bad_literal_fails_the_build_not_the_tick() {
    let mut bb = Blackboard::new();
    let mut b = TreeBuilder::new();
    b.leaf_custom("acc", &ACCUMULATE, Box::new(Accumulate::default()))
        .port("step", "not-a-number");
    let err = b.build(&mut bb).unwrap_err();
    assert!(
        matches!(err, BuildError::PortBinding { .. }),
        "expected a binding error, got {err:?}"
    );
}

#[test]
fn a_key_of_the_wrong_type_fails_the_build() {
    let mut bb = Blackboard::new();
    bb.declare("sum", 1.5f64).unwrap();

    let mut b = TreeBuilder::new();
    b.leaf_custom("acc", &ACCUMULATE, Box::new(Accumulate::default()))
        .port("total", "{sum}");
    let err = b.build(&mut bb).unwrap_err();
    assert!(
        matches!(err, BuildError::PortBinding { .. }),
        "an i64 port must not silently bind to an f64 key; got {err:?}"
    );
}

#[test]
fn a_misspelled_port_fails_the_build() {
    let mut bb = Blackboard::new();
    let mut b = TreeBuilder::new();
    b.leaf_custom("acc", &ACCUMULATE, Box::new(Accumulate::default()))
        .port("stepp", "2");
    let err = b.build(&mut bb).unwrap_err();
    // The failure mode this prevents: the node runs on its default while the
    // tree source looks configured.
    assert!(
        matches!(&err, BuildError::UnknownPort { port, .. } if port == "stepp"),
        "got {err:?}"
    );
}

#[test]
fn the_same_port_twice_fails_the_build() {
    let mut bb = Blackboard::new();
    let mut b = TreeBuilder::new();
    b.leaf_custom("acc", &ACCUMULATE, Box::new(Accumulate::default()))
        .port("step", "1")
        .port("step", "2");
    assert!(matches!(
        b.build(&mut bb).unwrap_err(),
        BuildError::DuplicatePort { .. }
    ));
}

#[test]
fn a_literal_on_an_output_port_fails_the_build() {
    let mut bb = Blackboard::new();
    let mut b = TreeBuilder::new();
    b.leaf_custom("acc", &ACCUMULATE, Box::new(Accumulate::default()))
        .port("total", "12");
    assert!(matches!(
        b.build(&mut bb).unwrap_err(),
        BuildError::LiteralOnOutput { .. }
    ));
}

#[test]
fn a_required_input_with_no_value_fails_the_build() {
    // Timeout's `msec` has no default on purpose: silently defaulting a timeout
    // is worse than refusing to build.
    let mut bb = Blackboard::new();
    let mut b = TreeBuilder::new();
    b.begin("t", Builtin::timeout())
        .leaf("child", Builtin::AlwaysSuccess)
        .end();
    assert!(matches!(
        b.build(&mut bb).unwrap_err(),
        BuildError::MissingPort { port: "msec", .. }
    ));
}

#[test]
fn ports_attach_to_the_node_whose_scope_just_closed() {
    // `.begin(x).leaf(y).end().port(..)` must configure x, not y.
    let mut bb = Blackboard::new();
    let mut b = TreeBuilder::new();
    b.begin("t", Builtin::timeout())
        .leaf("child", Builtin::AlwaysSuccess)
        .end()
        .port("msec", "500");
    let graph = b.build(&mut bb).unwrap();
    let slot = graph.port_slot(bteng_core::NodeId::ROOT, 0).unwrap();
    assert_eq!(*bb.get::<u64>(slot).unwrap(), 500);
}

#[test]
fn a_key_written_by_one_node_is_read_by_another() {
    let mut bb = Blackboard::new();
    let mut b = TreeBuilder::new();
    b.begin("root", Builtin::sequence())
        .leaf_custom("a", &ACCUMULATE, Box::new(Accumulate::default()))
        .port("step", "3")
        .port("total", "{shared}")
        .leaf_custom("b", &ACCUMULATE, Box::new(Accumulate::default()))
        .port("step", "{shared}")
        .port("total", "{result}")
        .end();
    let graph = b.build(&mut bb).unwrap();
    let mut tree = Tree::new(graph);
    let clock = SimClock::new();

    let result = bb.resolve("result").unwrap();
    assert_eq!(run(&mut tree, &mut bb, &clock), Status::Success);
    assert_eq!(
        *bb.get::<i64>(result).unwrap(),
        3,
        "b should have read the 3 that a published this same tick"
    );
}

#[test]
fn bool_ports_accept_the_spellings_xml_actually_uses() {
    for text in ["true", "TRUE", "1", "yes", "on"] {
        let mut bb = Blackboard::new();
        let mut b = TreeBuilder::new();
        b.begin("once", Builtin::run_once())
            .leaf("child", Builtin::AlwaysSuccess)
            .end()
            .port("then_skip", text);
        assert!(b.build(&mut bb).is_ok(), "{text:?} should parse as a bool");
    }
}

// ── registry ─────────────────────────────────────────────────────────────────

#[test]
fn the_builtin_registry_knows_every_constructible_node() {
    let reg = NodeRegistry::with_builtins();
    for name in [
        "Sequence",
        "ReactiveSequence",
        "SequenceWithMemory",
        "Fallback",
        "ReactiveFallback",
        "Parallel",
        "IfThenElse",
        "WhileDoElse",
        "Inverter",
        "ForceSuccess",
        "ForceFailure",
        "KeepRunningUntilFailure",
        "Repeat",
        "RetryUntilSuccessful",
        "RunOnce",
        "Timeout",
        "Delay",
        "SubTree",
        "AlwaysSuccess",
        "AlwaysFailure",
        "Sleep",
    ] {
        assert!(reg.contains(name), "{name} should be registered");
        let reg_entry = reg.get(name).unwrap();
        assert!(matches!((reg_entry.ctor)(), NodeKind::Builtin(_)));
    }
}

#[test]
fn behaviortree_cpp_aliases_resolve() {
    let reg = NodeRegistry::with_builtins();
    assert_eq!(
        reg.get("Selector").unwrap().manifest.type_name,
        "Fallback",
        "a tree written for BT.CPP should load unchanged"
    );
    assert_eq!(
        reg.get("SequenceStar").unwrap().manifest.type_name,
        "SequenceWithMemory"
    );
}

#[test]
fn registering_a_taken_name_is_refused() {
    let mut reg = NodeRegistry::with_builtins();
    reg.register(&ACCUMULATE, || {
        NodeKind::Custom(Box::new(Accumulate::default()))
    })
    .unwrap();

    // Silently replacing would make a tree's meaning depend on registration
    // order, which is not a thing anyone can debug.
    let err = reg
        .register(&ACCUMULATE, || {
            NodeKind::Custom(Box::new(Accumulate::default()))
        })
        .unwrap_err();
    assert!(matches!(err, RegistryError::Duplicate { .. }));
}

#[test]
fn the_exported_node_model_describes_ports() {
    let mut reg = NodeRegistry::with_builtins();
    reg.register(&ACCUMULATE, || {
        NodeKind::Custom(Box::new(Accumulate::default()))
    })
    .unwrap();

    let xml = reg.export_node_models_xml();
    assert!(xml.contains("<TreeNodesModel>"));
    assert!(xml.contains(r#"<Action ID="Accumulate">"#));
    assert!(xml.contains(r#"<input_port name="step" default="1""#));
    assert!(xml.contains(r#"<output_port name="total""#));
    // Aliases would otherwise list one node type twice in a palette.
    assert!(!xml.contains(r#"ID="Selector""#));
}

// ── lifecycle ────────────────────────────────────────────────────────────────

#[test]
fn setup_runs_once_per_node_and_reports_every_failure() {
    let mut bb = Blackboard::new();
    let mut b = TreeBuilder::new();
    b.begin("root", Builtin::sequence())
        .leaf_custom("ok", &ACCUMULATE, Box::new(Accumulate::default()))
        .leaf_custom("bad1", &BROKEN, Box::new(Broken))
        .leaf_custom("bad2", &BROKEN, Box::new(Broken))
        .end();
    let graph = b.build(&mut bb).unwrap();
    let mut tree = Tree::new(graph);
    let clock = SimClock::new();

    let mut ctx = Ctx::new(&mut bb, &clock);
    let errors = tree.setup(&mut ctx).unwrap_err();
    assert_eq!(
        errors.len(),
        2,
        "all failures at once — reporting one per run turns bring-up into guessing"
    );
    assert_eq!(errors[0].1.message, "no device at /dev/null");
}

#[test]
fn shutdown_is_safe_without_a_prior_setup() {
    let mut bb = Blackboard::new();
    let mut b = TreeBuilder::new();
    b.leaf_custom("acc", &ACCUMULATE, Box::new(Accumulate::default()));
    let graph = b.build(&mut bb).unwrap();
    let mut tree = Tree::new(graph);
    let clock = SimClock::new();
    let mut ctx = Ctx::new(&mut bb, &clock);
    tree.shutdown(&mut ctx);
}

#[test]
fn a_custom_nodes_arity_is_checked_against_its_manifest() {
    let mut bb = Blackboard::new();
    let mut b = TreeBuilder::new();
    b.begin_custom("acc", &ACCUMULATE, Box::new(Accumulate::default()))
        .leaf("surprise", Builtin::AlwaysSuccess)
        .end();
    let err = b.build(&mut bb).unwrap_err();
    assert!(
        matches!(err, BuildError::Arity { found: 1, .. }),
        "a Leaf manifest must reject children; got {err:?}"
    );
}

#[test]
fn exported_port_types_carry_no_module_path() {
    // `type_name` paths are a std implementation detail; tooling must not see them.
    let xml = NodeRegistry::with_builtins().export_node_models_xml();
    let types: Vec<&str> = xml
        .split("type=\"")
        .skip(1)
        .filter_map(|s| s.split('"').next())
        .collect();
    assert!(!types.is_empty(), "builtins declare typed ports");
    for t in &types {
        let head = t.split('<').next().unwrap();
        assert!(!head.contains("::"), "module path leaked: {t}");
    }
    assert!(
        types.contains(&"String"),
        "String ports must render as `String`: {types:?}"
    );
}
