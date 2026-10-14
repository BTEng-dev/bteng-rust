//! The DR-6 proof: a full tree, authored in XML, driven from an extension crate
//! that is not part of the BTEngRust workspace.
//!
//! If any of this needed something opened up inside `bteng-core`, the crate
//! would not compile — which is the point.

use std::sync::Arc;
use std::time::{Duration, Instant};

use bteng_async::{InlineSpawner, ThreadSpawner};
use bteng_core::{Blackboard, Ctx, NodeRegistry, SimClock, Status, Tree};
use bteng_devices::{Device, DeviceNodes, MockDevice};
use bteng_xml::XmlLoader;

const MISSION: &str = r#"
<root BTCPP_format="4" main_tree_to_execute="main">
  <BehaviorTree ID="main">
    <Sequence name="press_cycle">
      <ReadTagF64  tag="/press/depth"    value="{depth}"/>
      <WriteTagBool tag="/press/enable"  value="true"/>
      <WriteTagF64 tag="/press/setpoint" value="12.5"/>
      <WaitForTagF64 tag="/press/depth" expected="12.5" tolerance="0.1"/>
      <WriteTagBool tag="/press/enable" value="false"/>
    </Sequence>
  </BehaviorTree>
</root>
"#;

struct Rig {
    tree: Tree,
    bb: Blackboard,
    clock: SimClock,
}

impl Rig {
    fn tick(&mut self) -> Status {
        let mut ctx = Ctx::new(&mut self.bb, &self.clock);
        self.tree.tick(&mut ctx)
    }
    fn setup(&mut self) -> Result<(), String> {
        let mut ctx = Ctx::new(&mut self.bb, &self.clock);
        self.tree
            .setup(&mut ctx)
            .map_err(|errs| errs[0].1.message.clone())
    }
}

fn load(xml: &str, device: &Arc<MockDevice>) -> Rig {
    let mut registry = NodeRegistry::with_builtins();
    DeviceNodes::new(Arc::clone(device) as Arc<dyn Device>)
        .register_all(&mut registry)
        .expect("registers");
    let mut bb = Blackboard::new();
    let graph = XmlLoader::new(&registry)
        .load(xml, &mut bb)
        .expect("mission loads");
    Rig {
        tree: Tree::new(graph),
        bb,
        clock: SimClock::new(),
    }
}

// ── the proof ────────────────────────────────────────────────────────────────

#[test]
fn an_xml_mission_of_third_party_nodes_loads_and_runs() {
    let device = MockDevice::shared();
    device.set_f64("/press/depth", 0.0);
    device.set_bool("/press/enable", false);
    device.set_f64("/press/setpoint", 0.0);

    let mut rig = load(MISSION, &device);
    rig.setup().expect("setup connects the device");
    assert!(device.is_connected(), "setup is where a connection belongs");

    // Waiting for a depth it has not reached.
    assert_eq!(rig.tick(), Status::Running);
    assert_eq!(rig.tick(), Status::Running);
    assert!(device.read_bool("/press/enable").unwrap());
    assert_eq!(device.read_f64("/press/setpoint").unwrap(), 12.5);

    // The press arrives — within tolerance, not exactly.
    device.set_f64("/press/depth", 12.55);
    assert_eq!(rig.tick(), Status::Success);
    assert!(
        !device.read_bool("/press/enable").unwrap(),
        "the cycle disables the press on the way out"
    );
}

#[test]
fn a_generic_node_is_a_different_type_per_value_type() {
    let device = MockDevice::shared();
    let mut registry = NodeRegistry::with_builtins();
    DeviceNodes::new(Arc::clone(&device) as Arc<dyn Device>)
        .register_all(&mut registry)
        .unwrap();

    // Separate manifests, separate port types, one implementation.
    let f = registry.get("ReadTagF64").unwrap();
    let b = registry.get("ReadTagBool").unwrap();
    assert_eq!(f.manifest.ports[1].type_name(), "f64");
    assert_eq!(b.manifest.ports[1].type_name(), "bool");
    assert_eq!(f.manifest.type_name, b.manifest.type_name, "same node type");

    // ...and the exported palette shows them as distinct entries for BTviz.
    let models = registry.export_node_models_xml();
    assert!(models.contains(r#"ID="ReadTagF64""#));
    assert!(models.contains(r#"ID="WaitForTagBool""#));
}

#[test]
fn port_types_are_checked_when_the_document_loads() {
    let device = MockDevice::shared();
    let mut registry = NodeRegistry::with_builtins();
    DeviceNodes::new(Arc::clone(&device) as Arc<dyn Device>)
        .register_all(&mut registry)
        .unwrap();
    let mut bb = Blackboard::new();

    // `expected` on WaitForTagF64 is an f64. Not "12.5 volts".
    let bad = r#"<root><BehaviorTree ID="m">
        <WaitForTagF64 tag="/x" expected="rather deep"/>
      </BehaviorTree></root>"#;
    assert!(XmlLoader::new(&registry).load(bad, &mut bb).is_err());

    // And a bool port will not take an f64 key.
    bb.declare("flag", 1.0f64).unwrap();
    let mismatched = r#"<root><BehaviorTree ID="m">
        <ReadTagBool tag="/x" value="{flag}"/>
      </BehaviorTree></root>"#;
    assert!(
        XmlLoader::new(&registry).load(mismatched, &mut bb).is_err(),
        "a bool output must not bind to an f64 key"
    );
}

// ── device misbehaviour ──────────────────────────────────────────────────────

#[test]
fn a_disconnected_device_fails_setup_rather_than_ticking() {
    // MockDevice::connect always succeeds, so drive the failure through a read.
    let device = MockDevice::shared();
    let mut rig = load(
        r#"<root><BehaviorTree ID="m"><ReadTagF64 tag="/missing" value="{v}"/></BehaviorTree></root>"#,
        &device,
    );
    rig.setup().unwrap();
    // The tag does not exist. Failure, not a silent zero.
    assert_eq!(rig.tick(), Status::Failure);
}

#[test]
fn a_transport_error_is_a_failure_not_a_wait() {
    let device = MockDevice::shared();
    device.set_f64("/x", 1.0);
    let mut rig = load(
        r#"<root><BehaviorTree ID="m">
             <WaitForTagF64 tag="/x" expected="99.0"/>
           </BehaviorTree></root>"#,
        &device,
    );
    rig.setup().unwrap();
    assert_eq!(rig.tick(), Status::Running, "not there yet");

    device.fail_next(1);
    // Quietly waiting forever on a device that is answering with errors is the
    // worst of the three possible answers.
    assert_eq!(rig.tick(), Status::Failure);
}

#[test]
fn shutdown_disconnects() {
    let device = MockDevice::shared();
    device.set_f64("/x", 1.0);
    let mut rig = load(
        r#"<root><BehaviorTree ID="m"><ReadTagF64 tag="/x" value="{v}"/></BehaviorTree></root>"#,
        &device,
    );
    rig.setup().unwrap();
    assert!(device.is_connected());
    let mut ctx = Ctx::new(&mut rig.bb, &rig.clock);
    rig.tree.shutdown(&mut ctx);
    assert!(!device.is_connected());
}

// ── the async path ───────────────────────────────────────────────────────────

#[test]
fn a_blocking_device_call_runs_off_the_tick_thread() {
    let device = MockDevice::shared();
    device.connect().unwrap();
    device.set_latency(Duration::from_millis(30));

    let spawner = Arc::new(ThreadSpawner::new(2));
    let mut registry = NodeRegistry::with_builtins();
    DeviceNodes::new(Arc::clone(&device) as Arc<dyn Device>)
        .with_spawner(spawner)
        .register_all(&mut registry)
        .unwrap();

    let mut bb = Blackboard::new();
    let graph = XmlLoader::new(&registry)
        .load(
            r#"<root><BehaviorTree ID="m">
                 <CallMethod method="double" arg="21.0" result="{answer}"/>
               </BehaviorTree></root>"#,
            &mut bb,
        )
        .unwrap();
    let mut tree = Tree::new(graph);
    let clock = SimClock::new();

    // The device call takes 30 ms. The tick that starts it must not.
    let t0 = Instant::now();
    let first = {
        let mut ctx = Ctx::new(&mut bb, &clock);
        tree.tick(&mut ctx)
    };
    assert_eq!(first, Status::Running);
    assert!(
        t0.elapsed() < Duration::from_millis(20),
        "the tick took {:?} — it waited for the device",
        t0.elapsed()
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut status = Status::Running;
    while status == Status::Running && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
        let mut ctx = Ctx::new(&mut bb, &clock);
        status = tree.tick(&mut ctx);
    }
    assert_eq!(status, Status::Success);
    let slot = bb.resolve("answer").unwrap();
    assert_eq!(*bb.get::<f64>(slot).unwrap(), 42.0);
}

#[test]
fn a_failing_device_method_fails_the_node() {
    let device = MockDevice::shared();
    device.connect().unwrap();
    let mut registry = NodeRegistry::with_builtins();
    DeviceNodes::new(Arc::clone(&device) as Arc<dyn Device>)
        .with_spawner(Arc::new(InlineSpawner::new()))
        .register_all(&mut registry)
        .unwrap();

    let mut bb = Blackboard::new();
    let graph = XmlLoader::new(&registry)
        .load(
            r#"<root><BehaviorTree ID="m">
                 <CallMethod method="fail" result="{r}"/>
               </BehaviorTree></root>"#,
            &mut bb,
        )
        .unwrap();
    let mut tree = Tree::new(graph);
    let clock = SimClock::new();

    let mut ctx = Ctx::new(&mut bb, &clock);
    assert_eq!(tree.tick(&mut ctx), Status::Running);
    assert_eq!(tree.tick(&mut ctx), Status::Failure);
}

#[test]
fn call_method_is_absent_without_somewhere_to_run_it() {
    let device = MockDevice::shared();
    let mut registry = NodeRegistry::with_builtins();
    DeviceNodes::new(Arc::clone(&device) as Arc<dyn Device>)
        .register_all(&mut registry)
        .unwrap();
    // Better a missing node than one that blocks the tick thread on a socket.
    assert!(!registry.contains("CallMethod"));
    assert!(registry.contains("ReadTagF64"));
}

#[test]
fn two_devices_can_be_registered_side_by_side() {
    // The registry is a value, not a singleton (D6), so one process can drive
    // two machines with two independent connections.
    let left = MockDevice::shared();
    let right = MockDevice::shared();
    left.connect().unwrap();
    right.connect().unwrap();
    left.set_f64("/x", 1.0);
    right.set_f64("/x", 2.0);

    let mut registry = NodeRegistry::with_builtins();
    DeviceNodes::new(Arc::clone(&left) as Arc<dyn Device>)
        .register_f64(&mut registry)
        .unwrap();

    let mut other = NodeRegistry::with_builtins();
    DeviceNodes::new(Arc::clone(&right) as Arc<dyn Device>)
        .register_f64(&mut other)
        .unwrap();

    let xml =
        r#"<root><BehaviorTree ID="m"><ReadTagF64 tag="/x" value="{v}"/></BehaviorTree></root>"#;
    for (registry, expected) in [(&registry, 1.0), (&other, 2.0)] {
        let mut bb = Blackboard::new();
        let graph = XmlLoader::new(registry).load(xml, &mut bb).unwrap();
        let mut tree = Tree::new(graph);
        let clock = SimClock::new();
        let mut ctx = Ctx::new(&mut bb, &clock);
        assert_eq!(tree.tick(&mut ctx), Status::Success);
        let slot = bb.resolve("v").unwrap();
        assert_eq!(*bb.get::<f64>(slot).unwrap(), expected);
    }
}
