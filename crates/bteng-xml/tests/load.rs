//! Loading BehaviorTree.CPP-shaped XML.
//!
//! The M1 exit criterion lives here: a BT.CPP-shaped document loads, validates,
//! and ticks.

use bteng_core::{
    Arity, Blackboard, Ctx, Cursor, Node, NodeCategory, NodeKind, NodeManifest, NodeRegistry,
    PortSpec, SimClock, Status, Tree, input, output,
};
use bteng_xml::{XmlError, XmlLoader, unknown_node_types};

// ── a domain node, registered the way an extension crate would ───────────────

static NAV_PORTS: &[PortSpec] = &[
    input::<String>("goal").described("where to go"),
    input::<f64>("speed").with_default("1.0"),
    output::<bool>("arrived"),
];

static NAVIGATE: NodeManifest =
    NodeManifest::new("NavigateTo", NodeCategory::Action, Arity::Leaf, NAV_PORTS);

#[derive(Default)]
struct NavigateTo;

impl Node for NavigateTo {
    fn tick(&mut self, cur: &mut Cursor<'_, '_>) -> Status {
        let Ok(goal) = cur.get::<String>(0) else {
            return Status::Failure;
        };
        if goal.is_empty() {
            return Status::Failure;
        }
        let _ = cur.set(2, true);
        Status::Success
    }
    fn type_name(&self) -> &'static str {
        "NavigateTo"
    }
}

static COUNT_PORTS: &[PortSpec] = &[
    input::<i64>("step").with_default("1"),
    output::<i64>("total"),
];
static COUNTER: NodeManifest =
    NodeManifest::new("Count", NodeCategory::Action, Arity::Leaf, COUNT_PORTS);

#[derive(Default)]
struct Count {
    total: i64,
}

impl Node for Count {
    fn tick(&mut self, cur: &mut Cursor<'_, '_>) -> Status {
        let step = cur.get::<i64>(0).unwrap_or(0);
        self.total += step;
        let total = self.total;
        let _ = cur.set(1, total);
        Status::Success
    }
    fn type_name(&self) -> &'static str {
        "Count"
    }
}

fn registry() -> NodeRegistry {
    let mut reg = NodeRegistry::with_builtins();
    reg.register(&NAVIGATE, || NodeKind::Custom(Box::new(NavigateTo)))
        .unwrap();
    reg.register(&COUNTER, || NodeKind::Custom(Box::new(Count::default())))
        .unwrap();
    reg
}

fn load(xml: &str) -> Result<(Tree, Blackboard), XmlError> {
    let reg = registry();
    let mut bb = Blackboard::new();
    let graph = XmlLoader::new(&reg).load(xml, &mut bb)?;
    Ok((Tree::new(graph), bb))
}

fn tick(tree: &mut Tree, bb: &mut Blackboard, clock: &SimClock) -> Status {
    let mut ctx = Ctx::new(bb, clock);
    tree.tick(&mut ctx)
}

// ── the exit criterion ───────────────────────────────────────────────────────

const MISSION: &str = r#"
<root BTCPP_format="4" main_tree_to_execute="main">
  <BehaviorTree ID="main">
    <Sequence name="mission">
      <!-- comments and whitespace must not confuse the loader -->
      <NavigateTo name="go" goal="{target}" speed="2.5" arrived="{at_goal}"/>
      <Timeout msec="5000">
        <NavigateTo goal="dock" arrived="{at_dock}"/>
      </Timeout>
    </Sequence>
  </BehaviorTree>
</root>
"#;

#[test]
fn a_btcpp_shaped_document_loads_validates_and_ticks() {
    let (mut tree, mut bb) = load(MISSION).expect("loads");
    let clock = SimClock::new();

    let target = bb.resolve("target").expect("key created by binding");
    bb.set(target, "kitchen".to_string()).unwrap();

    assert_eq!(tick(&mut tree, &mut bb, &clock), Status::Success);

    let at_goal = bb.resolve("at_goal").unwrap();
    assert!(*bb.get::<bool>(at_goal).unwrap());
    let at_dock = bb.resolve("at_dock").unwrap();
    assert!(*bb.get::<bool>(at_dock).unwrap());
}

#[test]
fn a_literal_port_is_typed_by_its_declaration() {
    let (tree, bb) = load(MISSION).expect("loads");
    // speed="2.5" was declared f64, so it is stored as one — not as the string
    // "2.5" that the document actually contained.
    let go = tree
        .graph()
        .ids()
        .find(|id| tree.graph().name(*id) == "go")
        .unwrap();
    let slot = tree.graph().port_slot(go, 1).unwrap();
    assert!((*bb.get::<f64>(slot).unwrap() - 2.5).abs() < f64::EPSILON);
}

// ── dialects ─────────────────────────────────────────────────────────────────

#[test]
fn the_v3_category_tag_spelling_also_loads() {
    // `<Action ID="X"/>` is BT.CPP v3 and Python BTEng; `<X/>` is v4. Trees
    // written either way exist, so both are read.
    let xml = r#"
    <root main_tree_to_execute="main">
      <BehaviorTree ID="main">
        <Sequence>
          <Action ID="NavigateTo" goal="home"/>
        </Sequence>
      </BehaviorTree>
    </root>"#;
    let (mut tree, mut bb) = load(xml).expect("loads");
    let clock = SimClock::new();
    assert_eq!(tick(&mut tree, &mut bb, &clock), Status::Success);
}

#[test]
fn the_python_bteng_root_and_tree_tags_also_load() {
    let xml = r#"
    <BTEng format_version="1.0" main_tree_to_execute="main">
      <Tree ID="main">
        <Sequence>
          <NavigateTo goal="home"/>
        </Sequence>
      </Tree>
    </BTEng>"#;
    let (mut tree, mut bb) = load(xml).expect("loads");
    let clock = SimClock::new();
    assert_eq!(tick(&mut tree, &mut bb, &clock), Status::Success);
}

#[test]
fn a_btcpp_node_alias_resolves() {
    let xml = r#"
    <root>
      <BehaviorTree ID="main">
        <Selector>
          <NavigateTo goal=""/>
          <AlwaysSuccess/>
        </Selector>
      </BehaviorTree>
    </root>"#;
    let (mut tree, mut bb) = load(xml).expect("loads");
    let clock = SimClock::new();
    assert_eq!(tick(&mut tree, &mut bb, &clock), Status::Success);
}

#[test]
fn a_single_tree_document_needs_no_main_tree_attribute() {
    let xml = r#"
    <root>
      <BehaviorTree ID="only">
        <AlwaysSuccess/>
      </BehaviorTree>
    </root>"#;
    assert!(load(xml).is_ok());
}

// ── subtrees ─────────────────────────────────────────────────────────────────

const WITH_SUBTREE: &str = r#"
<root main_tree_to_execute="main">
  <BehaviorTree ID="main">
    <Sequence>
      <SubTree ID="counter" total="{outer_total}" step="5"/>
      <SubTree ID="counter"/>
    </Sequence>
  </BehaviorTree>
  <BehaviorTree ID="counter">
    <Count step="{step}" total="{total}"/>
  </BehaviorTree>
</root>
"#;

#[test]
fn a_subtree_is_inlined_behind_a_marker_node() {
    let (tree, _bb) = load(WITH_SUBTREE).expect("loads");
    let g = tree.graph();
    let markers = g
        .ids()
        .filter(|id| g.manifest(*id).type_name == "SubTree")
        .count();
    assert_eq!(markers, 2, "the boundary must survive inlining, for BTviz");
    // root Sequence + 2 markers + 2 Count nodes
    assert_eq!(g.len(), 5);
}

#[test]
fn subtree_ports_remap_outward_and_namespace_by_default() {
    let (mut tree, mut bb) = load(WITH_SUBTREE).expect("loads");
    let clock = SimClock::new();
    assert_eq!(tick(&mut tree, &mut bb, &clock), Status::Success);

    // First instance: `total` was remapped to the parent's key, `step` given a
    // literal.
    let outer = bb.resolve("outer_total").expect("remapped key exists");
    assert_eq!(*bb.get::<i64>(outer).unwrap(), 5);

    // Second instance: nothing remapped, so its keys live in its own namespace
    // and cannot have touched the first instance's.
    let own: Vec<_> = bb
        .names()
        .filter(|(n, _)| n.starts_with("counter#2/"))
        .collect();
    assert_eq!(
        own.len(),
        2,
        "an unmapped subtree instance keeps its keys to itself: {:?}",
        bb.names().map(|(n, _)| n.to_string()).collect::<Vec<_>>()
    );
}

#[test]
fn autoremap_passes_unmapped_keys_through() {
    let xml = r#"
    <root main_tree_to_execute="main">
      <BehaviorTree ID="main">
        <SubTree ID="counter" _autoremap="true"/>
      </BehaviorTree>
      <BehaviorTree ID="counter">
        <Count step="2" total="{total}"/>
      </BehaviorTree>
    </root>"#;
    let (mut tree, mut bb) = load(xml).expect("loads");
    let clock = SimClock::new();
    assert_eq!(tick(&mut tree, &mut bb, &clock), Status::Success);
    let total = bb.resolve("total").expect("key passed through unprefixed");
    assert_eq!(*bb.get::<i64>(total).unwrap(), 2);
}

#[test]
fn a_self_including_subtree_is_rejected() {
    let xml = r#"
    <root main_tree_to_execute="main">
      <BehaviorTree ID="main">
        <SubTree ID="main"/>
      </BehaviorTree>
    </root>"#;
    assert!(matches!(
        load(xml).unwrap_err(),
        XmlError::SubTreeCycle { .. }
    ));
}

#[test]
fn mutually_including_subtrees_are_rejected() {
    let xml = r#"
    <root main_tree_to_execute="a">
      <BehaviorTree ID="a"><SubTree ID="b"/></BehaviorTree>
      <BehaviorTree ID="b"><SubTree ID="a"/></BehaviorTree>
    </root>"#;
    assert!(matches!(
        load(xml).unwrap_err(),
        XmlError::SubTreeCycle { .. }
    ));
}

// ── diagnostics ──────────────────────────────────────────────────────────────

#[test]
fn an_unregistered_node_type_names_itself() {
    let xml = r#"
    <root><BehaviorTree ID="main"><Sequence><Frobnicate/></Sequence></BehaviorTree></root>"#;
    match load(xml).unwrap_err() {
        XmlError::UnknownNodeType { type_name, .. } => assert_eq!(type_name, "Frobnicate"),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn unknown_types_can_be_listed_without_building() {
    let xml = r#"
    <root><BehaviorTree ID="main">
      <Sequence><Frobnicate/><Wibble/><NavigateTo goal="x"/></Sequence>
    </BehaviorTree></root>"#;
    let missing = unknown_node_types(xml, &registry()).unwrap();
    assert_eq!(missing, vec!["Frobnicate", "Wibble"]);
}

#[test]
fn a_bad_port_value_fails_at_load_not_at_tick() {
    let xml = r#"
    <root><BehaviorTree ID="main">
      <NavigateTo goal="x" speed="quite fast"/>
    </BehaviorTree></root>"#;
    // The whole point of build-time binding: this is a document error, and it is
    // reported when the document is read.
    assert!(matches!(load(xml).unwrap_err(), XmlError::Build(_)));
}

#[test]
fn a_misspelled_port_fails_at_load() {
    let xml = r#"
    <root><BehaviorTree ID="main"><NavigateTo goal="x" sped="2.0"/></BehaviorTree></root>"#;
    assert!(matches!(load(xml).unwrap_err(), XmlError::Build(_)));
}

#[test]
fn a_wrong_arity_document_fails_at_load() {
    let xml = r#"
    <root><BehaviorTree ID="main">
      <Inverter><AlwaysSuccess/><AlwaysFailure/></Inverter>
    </BehaviorTree></root>"#;
    assert!(matches!(load(xml).unwrap_err(), XmlError::Build(_)));
}

#[test]
fn structural_document_errors_are_named_precisely() {
    type Case = (&'static str, fn(&XmlError) -> bool);
    let cases: Vec<Case> = vec![
        ("<notroot/>", |e| matches!(e, XmlError::BadRoot { .. })),
        ("<root></root>", |e| matches!(e, XmlError::NoTrees)),
        (
            r#"<root><BehaviorTree ID="a"><AlwaysSuccess/></BehaviorTree>
                     <BehaviorTree ID="a"><AlwaysSuccess/></BehaviorTree></root>"#,
            |e| matches!(e, XmlError::DuplicateTree { .. }),
        ),
        (
            r#"<root main_tree_to_execute="nope">
                 <BehaviorTree ID="a"><AlwaysSuccess/></BehaviorTree></root>"#,
            |e| matches!(e, XmlError::NoSuchTree { .. }),
        ),
        (
            r#"<root><BehaviorTree ID="a"><AlwaysSuccess/><AlwaysFailure/></BehaviorTree></root>"#,
            |e| matches!(e, XmlError::TreeNotSingleRooted { .. }),
        ),
        (
            r#"<root><BehaviorTree ID="a"><SubTree ID="ghost"/></BehaviorTree></root>"#,
            |e| matches!(e, XmlError::BadSubTree { .. }),
        ),
        ("<root>", |e| matches!(e, XmlError::Malformed(_))),
    ];

    for (xml, expected) in cases {
        let err = load(xml).unwrap_err();
        assert!(expected(&err), "for {xml:?} got {err:?}");
    }
}

#[test]
fn load_tree_can_pick_a_non_main_tree() {
    let reg = registry();
    let mut bb = Blackboard::new();
    let graph = XmlLoader::new(&reg)
        .load_tree(WITH_SUBTREE, "counter", &mut bb)
        .expect("loads the named tree");
    assert_eq!(graph.len(), 1);
}

#[test]
fn tree_ids_lists_the_document() {
    let reg = registry();
    let ids = XmlLoader::new(&reg).tree_ids(WITH_SUBTREE).unwrap();
    assert_eq!(ids, vec!["main", "counter"]);
}
