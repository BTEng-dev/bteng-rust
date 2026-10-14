//! The trace schema, and what it is for.

use bteng_core::{Blackboard, Builtin, Ctx, NodeState, SimClock, Status, Tree, TreeBuilder};
use bteng_sched::{Pacer, RunOutcome, Scheduler, StepOutcome, VirtualTime};
use bteng_telemetry::{Event, Recorder, RegionReport, SCHEMA_VERSION, Timeline, to_jsonl};

const MS: u64 = 1_000_000;

fn tree() -> (Tree, Blackboard) {
    let mut b = TreeBuilder::new();
    b.begin("root", Builtin::sequence())
        .leaf("check", Builtin::condition(|_| true))
        .begin("guard", Builtin::timeout())
        .port("msec", "50")
        .leaf("work", Builtin::action(|_| Status::Running))
        .end()
        .end();
    let mut bb = Blackboard::new();
    let graph = b.build(&mut bb).unwrap();
    (Tree::new(graph), bb)
}

#[test]
fn the_structure_event_carries_everything_a_viewer_needs() {
    let (tree, bb) = tree();
    let mut rec = Recorder::with_capacity(64);
    rec.structure(tree.graph(), &bb);
    let events = rec.finish();

    let Event::Structure(s) = &events[0] else {
        panic!("structure must come first")
    };
    assert_eq!(s.schema, SCHEMA_VERSION);
    assert_eq!(s.nodes.len(), 4);

    let timeout = s.nodes.iter().find(|n| n.name == "guard").unwrap();
    assert_eq!(timeout.type_name, "Timeout");
    assert_eq!(timeout.category, "Decorator");
    assert_eq!(timeout.children.len(), 1);
    // A viewer draws wiring from slots, which it cannot infer any other way.
    let msec = timeout.ports.iter().find(|p| p.name == "msec").unwrap();
    assert_eq!(msec.direction, "input");
    assert_eq!(msec.source, "literal");
}

#[test]
fn only_transitions_are_recorded_not_every_tick() {
    let (mut tree, mut bb) = tree();
    let clock = SimClock::new();
    let mut rec = Recorder::with_capacity(1024);
    rec.structure(tree.graph(), &bb);

    for _ in 0..100 {
        let mut ctx = Ctx::new(&mut bb, &clock).with_trace(&mut rec);
        tree.tick(&mut ctx);
    }

    let (node_events, _, _) = Timeline::new(rec.events()).unwrap().counts();
    // 100 ticks of an unchanging tree. Recording each one would be 400 events;
    // recording changes is what makes a trace keepable.
    assert!(
        node_events < 10,
        "an unchanging tree produced {node_events} events"
    );
    assert!(node_events > 0, "the first tick did change things");
}

#[test]
fn a_halt_is_recorded_even_though_it_is_not_a_tick() {
    let (mut tree, mut bb) = tree();
    let clock = SimClock::new();
    let mut rec = Recorder::with_capacity(256);
    rec.structure(tree.graph(), &bb);

    {
        let mut ctx = Ctx::new(&mut bb, &clock).with_trace(&mut rec);
        tree.tick(&mut ctx);
    }
    {
        let mut ctx = Ctx::new(&mut bb, &clock).with_trace(&mut rec);
        tree.halt(&mut ctx);
    }

    // Python BTEng only hooked ticks, so a node stopped by a Timeout or a
    // reactive sibling stayed "running" in every live view forever.
    let to_idle = rec
        .events()
        .iter()
        .filter(|e| {
            matches!(
                e,
                Event::Node {
                    to: NodeState::Idle,
                    ..
                }
            )
        })
        .count();
    assert!(to_idle > 0, "a halt must reach the trace");
}

#[test]
fn a_full_recorder_reports_the_gap_instead_of_growing() {
    let (mut tree, mut bb) = tree();
    let clock = SimClock::new();
    let mut rec = Recorder::with_capacity(3);
    rec.structure(tree.graph(), &bb);
    for _ in 0..50 {
        let mut ctx = Ctx::new(&mut bb, &clock).with_trace(&mut rec);
        tree.tick(&mut ctx);
        tree.halt(&mut ctx);
    }
    assert!(rec.dropped() > 0);
    let events = rec.finish();
    // A gap you know about is a measurement; a gap you do not is a lie.
    assert!(matches!(events.last(), Some(Event::Dropped { .. })));
    assert_eq!(events.len(), 3);
}

#[test]
fn a_timeline_answers_what_was_it_doing() {
    let (mut tree, mut bb) = tree();
    let clock = SimClock::new();
    let mut rec = Recorder::with_capacity(256);
    rec.structure(tree.graph(), &bb);
    {
        let mut ctx = Ctx::new(&mut bb, &clock).with_trace(&mut rec);
        tree.tick(&mut ctx);
    }
    let events = rec.finish();
    let tl = Timeline::new(&events).unwrap();

    assert_eq!(tl.node_count(), 4);
    let leaf = tl.active_leaf_at(0).expect("something was running");
    assert_eq!(leaf.name, "work");
    assert!(!tl.change_times().is_empty());
}

#[test]
fn the_json_is_line_delimited_and_escapes_names() {
    let mut b = TreeBuilder::new();
    b.begin("root \"quoted\"\n", Builtin::sequence())
        .leaf("ok", Builtin::AlwaysSuccess)
        .end();
    let mut bb = Blackboard::new();
    let graph = b.build(&mut bb).unwrap();
    let mut rec = Recorder::with_capacity(8);
    rec.structure(&graph, &bb);
    let json = to_jsonl(&rec.finish());

    assert_eq!(json.lines().count(), 1, "one object per line");
    assert!(json.contains(r#"\"quoted\""#), "quotes escaped: {json}");
    assert!(json.contains(r"\n"), "newlines escaped");
    assert!(json.contains(r#""t":"structure""#));
    assert!(json.contains(&format!(r#""schema":{SCHEMA_VERSION}"#)));
}

#[test]
fn a_scheduled_run_replays_identically() {
    // Determinism is the property M6's replay rests on, so it is asserted from
    // the trace rather than assumed from the design.
    let record = || {
        let mut b = TreeBuilder::new();
        b.begin("root", Builtin::parallel())
            .begin("fast", Builtin::region())
            .port("hz", "100.0")
            .leaf("w", Builtin::action(|_| Status::Running))
            .end()
            .leaf("slow", Builtin::action(|_| Status::Running))
            .end();
        let mut bb = Blackboard::new();
        let graph = b.build(&mut bb).unwrap();
        let mut sched = Scheduler::new(Tree::new(graph), &bb, 10.0).unwrap();
        let pacer = VirtualTime::new();
        let mut rec = Recorder::with_capacity(4096);
        rec.structure(sched.tree().graph(), &bb);

        sched.arm(pacer.now());
        for _ in 0..200 {
            let at = pacer.now();
            if let StepOutcome::Ticked {
                region,
                status,
                lateness,
            } = sched.step_traced(&mut bb, &pacer, Some(&mut rec))
            {
                rec.region(RegionReport {
                    at,
                    region,
                    status,
                    lateness_ns: lateness,
                });
            }
        }
        to_jsonl(&rec.finish())
    };

    assert_eq!(record(), record(), "two runs must produce the same trace");
}

#[test]
fn a_region_report_lands_in_the_trace() {
    let mut b = TreeBuilder::new();
    b.begin("root", Builtin::sequence())
        .leaf("w", Builtin::action(|_| Status::Success))
        .end();
    let mut bb = Blackboard::new();
    let graph = b.build(&mut bb).unwrap();
    let mut sched = Scheduler::new(Tree::new(graph), &bb, 10.0).unwrap();
    let pacer = VirtualTime::new();
    let mut rec = Recorder::with_capacity(64);
    rec.structure(sched.tree().graph(), &bb);

    sched.arm(pacer.now());
    let at = pacer.now();
    if let StepOutcome::Ticked {
        region,
        status,
        lateness,
    } = sched.step_traced(&mut bb, &pacer, Some(&mut rec))
    {
        rec.region(RegionReport {
            at,
            region,
            status,
            lateness_ns: lateness,
        });
    }
    rec.run_end(pacer.now(), Status::Success);

    let events = rec.finish();
    let (_, regions, _) = Timeline::new(&events).unwrap().counts();
    assert_eq!(regions, 1);
    assert!(matches!(events.last(), Some(Event::RunEnd { .. })));
    let _ = RunOutcome::TimeUp { at: 0 };
    let _ = MS;
}

// ── port bindings: what a viewer needs to draw `msec="250"` and `{key}` ──────

fn ports_of(build: impl FnOnce(&mut TreeBuilder)) -> (Vec<bteng_telemetry::PortInfo>, String) {
    let mut b = TreeBuilder::new();
    build(&mut b);
    let mut bb = Blackboard::new();
    let graph = b.build(&mut bb).unwrap();
    let mut rec = Recorder::with_capacity(8);
    rec.structure(&graph, &bb);
    let events = rec.finish();
    let Event::Structure(s) = &events[0] else {
        panic!("first event must be the structure");
    };
    let ports = s.nodes[0].ports.clone();
    (ports, to_jsonl(&events))
}

#[test]
fn a_literal_port_reports_its_text() {
    let (ports, json) = ports_of(|b| {
        b.leaf("nap", Builtin::sleep()).port("msec", "250");
    });
    assert_eq!(ports[0].source, "literal");
    assert_eq!(ports[0].value.as_deref(), Some("250"));
    assert_eq!(ports[0].key, None);
    assert!(json.contains(r#""src":"literal","value":"250""#), "{json}");
}

#[test]
fn a_key_port_reports_the_key_name_not_a_slot_number() {
    let (ports, json) = ports_of(|b| {
        b.leaf("nap", Builtin::sleep()).port("msec", "{wait_ms}");
    });
    assert_eq!(ports[0].source, "key");
    assert_eq!(ports[0].key.as_deref(), Some("wait_ms"));
    assert_eq!(ports[0].value, None);
    assert!(json.contains(r#""key":"wait_ms""#), "{json}");
    assert!(
        !json.contains(r#""value""#),
        "a key binding has no literal text: {json}"
    );
}

#[test]
fn a_defaulted_port_reports_the_default_text() {
    let (ports, _) = ports_of(|b| {
        b.begin("again", Builtin::repeat())
            .leaf("x", Builtin::AlwaysSuccess)
            .end();
    });
    assert_eq!(ports[0].source, "default");
    assert!(ports[0].value.is_some(), "{ports:?}");
}

#[test]
fn the_schema_version_names_the_port_fields() {
    assert_eq!(SCHEMA_VERSION, 3);
    let (_, json) = ports_of(|b| {
        b.leaf("nap", Builtin::sleep()).port("msec", "1");
    });
    assert!(
        json.contains(&format!(r#""schema":{SCHEMA_VERSION}"#)),
        "{json}"
    );
}

#[test]
fn every_port_line_is_valid_json_with_awkward_text() {
    // A key containing a quote and a backslash must stay one well-formed string.
    let (_, json) = ports_of(|b| {
        b.leaf("nap", Builtin::sleep()).port("msec", "{a\"b\\c}");
    });
    assert!(json.contains(r#""key":"a\"b\\c""#), "{json}");
}
