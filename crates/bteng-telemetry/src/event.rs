//! The events themselves, and their JSON form.

use std::fmt::Write as _;

use bteng_core::{Blackboard, Nanos, NodeState, Status, TreeGraph};

/// One port binding, as a viewer needs to show it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortInfo {
    pub name: &'static str,
    pub direction: &'static str,
    pub type_name: &'static str,
    /// The slot it resolved to. Two ports with the same slot are wired together
    /// — which is the thing a viewer most wants to draw and cannot infer.
    pub slot: u32,
    /// Where the binding came from: `key`, `literal`, `default` or `sink`.
    pub source: &'static str,
    /// The blackboard key a `key` binding names (`{target}` in XML).
    pub key: Option<String>,
    /// The text a `literal` or `default` binding was given, as the port's type
    /// formats it — so a viewer can show `msec="250"` rather than a slot number.
    /// Not the *current* value: this is a snapshot of the tree's shape.
    pub value: Option<String>,
}

/// One node, as a viewer needs to show it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeInfo {
    pub uid: u64,
    pub node: u32,
    pub parent: u32,
    pub name: String,
    pub type_name: &'static str,
    pub category: &'static str,
    pub children: Vec<u32>,
    pub ports: Vec<PortInfo>,
}

/// The whole tree, once, at the head of a trace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Structure {
    pub schema: u32,
    pub nodes: Vec<NodeInfo>,
}

impl Structure {
    /// Snapshot a built graph. `bb` is the blackboard it was built against; it
    /// is only read, to turn slots back into key names and literal text.
    #[must_use]
    pub fn of(graph: &TreeGraph, bb: &Blackboard) -> Self {
        let keys: Vec<_> = bb.names().collect();
        let nodes = graph
            .ids()
            .map(|id| {
                let manifest = graph.manifest(id);
                let bindings = graph.bindings_of(id);
                NodeInfo {
                    uid: graph.uid(id).0,
                    node: id.0,
                    parent: graph.parent(id).0,
                    name: graph.name(id).to_string(),
                    type_name: manifest.type_name,
                    category: manifest.category.xml_tag(),
                    children: graph.children_of(id).iter().map(|c| c.0).collect(),
                    ports: manifest
                        .ports
                        .iter()
                        .enumerate()
                        .filter_map(|(i, spec)| {
                            bindings.get(i).map(|b| PortInfo {
                                key: match b.source {
                                    bteng_core::BindingSource::Key => keys
                                        .iter()
                                        .find(|(_, slot)| *slot == b.slot)
                                        .map(|(name, _)| (*name).to_string()),
                                    _ => None,
                                },
                                value: match b.source {
                                    bteng_core::BindingSource::Literal
                                    | bteng_core::BindingSource::Default => {
                                        spec.read_text(bb, b.slot).ok()
                                    }
                                    _ => None,
                                },
                                name: spec.name,
                                direction: match spec.direction {
                                    bteng_core::PortDirection::Input => "input",
                                    bteng_core::PortDirection::Output => "output",
                                    bteng_core::PortDirection::InOut => "inout",
                                },
                                type_name: spec.type_name(),
                                slot: b.slot.0,
                                source: match b.source {
                                    bteng_core::BindingSource::Key => "key",
                                    bteng_core::BindingSource::Literal => "literal",
                                    bteng_core::BindingSource::Default => "default",
                                    bteng_core::BindingSource::Sink => "sink",
                                },
                            })
                        })
                        .collect(),
                }
            })
            .collect();
        Self {
            schema: crate::SCHEMA_VERSION,
            nodes,
        }
    }
}

/// Something that happened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// Always first. Carries the node table a viewer draws from.
    Structure(Box<Structure>),
    /// A node changed state. The bulk of any trace.
    Node {
        at: Nanos,
        tick: u64,
        uid: u64,
        node: u32,
        from: NodeState,
        to: NodeState,
    },
    /// A region's subtree was ticked.
    Region {
        at: Nanos,
        region: usize,
        status: Status,
        lateness_ns: Nanos,
    },
    /// The root settled and the run is over.
    RunEnd { at: Nanos, status: Status },
    /// The tree changed shape.
    ///
    /// Carries the new structure outright rather than a diff: a viewer that
    /// joined late, or one reading a truncated file, must not have to replay
    /// every earlier patch to know what it is looking at.
    Patch {
        at: Nanos,
        version: u64,
        structure: Box<Structure>,
    },
    /// Events the recorder had to drop.
    ///
    /// Emitted rather than silently omitted: a gap you know about is a
    /// measurement, a gap you do not is a lie.
    Dropped { at: Nanos, count: u64 },
}

impl Event {
    pub(crate) fn write_json(&self, out: &mut String) {
        match self {
            Event::Structure(s) => {
                let _ = write!(out, r#"{{"t":"structure","schema":{},"nodes":["#, s.schema);
                for (i, n) in s.nodes.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_node(out, n);
                }
                out.push_str("]}");
            }
            Event::Node {
                at,
                tick,
                uid,
                node,
                from,
                to,
            } => {
                let _ = write!(
                    out,
                    r#"{{"t":"node","at":{at},"tick":{tick},"uid":{uid},"node":{node},"from":"{}","to":"{}"}}"#,
                    state_name(*from),
                    state_name(*to)
                );
            }
            Event::Region {
                at,
                region,
                status,
                lateness_ns,
            } => {
                let _ = write!(
                    out,
                    r#"{{"t":"region","at":{at},"region":{region},"status":"{}","lateness_ns":{lateness_ns}}}"#,
                    status_name(*status)
                );
            }
            Event::RunEnd { at, status } => {
                let _ = write!(
                    out,
                    r#"{{"t":"run_end","at":{at},"status":"{}"}}"#,
                    status_name(*status)
                );
            }
            Event::Patch {
                at,
                version,
                structure,
            } => {
                let _ = write!(
                    out,
                    r#"{{"t":"patch","at":{at},"version":{version},"nodes":["#
                );
                for (i, n) in structure.nodes.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_node(out, n);
                }
                out.push_str("]}");
            }
            Event::Dropped { at, count } => {
                let _ = write!(out, r#"{{"t":"dropped","at":{at},"count":{count}}}"#);
            }
        }
    }
}

fn write_node(out: &mut String, n: &NodeInfo) {
    let _ = write!(
        out,
        r#"{{"uid":{},"node":{},"parent":{},"name":"#,
        n.uid, n.node, n.parent
    );
    escape_json(out, &n.name);
    let _ = write!(
        out,
        r#","type":"{}","category":"{}","children":["#,
        n.type_name, n.category
    );
    for (i, c) in n.children.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "{c}");
    }
    out.push_str("],\"ports\":[");
    for (i, p) in n.ports.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(
            out,
            r#"{{"name":"{}","dir":"{}","type":"{}","slot":{},"src":"{}""#,
            p.name, p.direction, p.type_name, p.slot, p.source
        );
        if let Some(key) = &p.key {
            out.push_str(r#","key":"#);
            escape_json(out, key);
        }
        if let Some(value) = &p.value {
            out.push_str(r#","value":"#);
            escape_json(out, value);
        }
        out.push('}');
    }
    out.push_str("]}");
}

const fn state_name(s: NodeState) -> &'static str {
    match s {
        NodeState::Idle => "idle",
        NodeState::Running => "running",
        NodeState::Success => "success",
        NodeState::Failure => "failure",
    }
}

const fn status_name(s: Status) -> &'static str {
    match s {
        Status::Running => "running",
        Status::Success => "success",
        Status::Failure => "failure",
    }
}

/// Append `text` as a JSON string literal, quotes included.
pub fn escape_json(out: &mut String, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}
