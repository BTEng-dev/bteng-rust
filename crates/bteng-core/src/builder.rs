//! Building and validating an arena from a nested description.
//!
//! Everything expensive and everything fallible happens here — allocation, arity
//! checking, port binding, type checking, name interning — so that the tick path
//! has none of it. That split is the same one a future [`TreeGraph`] patch will
//! use: build and validate off the tick thread, install with a bounded splice.

use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

use crate::blackboard::Blackboard;
use crate::builtins::{Builtin, MAX_PARALLEL_CHILDREN};
use crate::id::{NameId, NodeId, Uid};
use crate::manifest::{Arity, NodeManifest};
use crate::node::{Node, NodeKind};
use crate::port::{Binding, BindingSource, PortError, PortSpec};
use crate::tree::{NodeMeta, NodeStruct, TreeGraph};

/// How deep a tree may nest, by default.
///
/// Ticking and halting recurse, so depth is stack. Measured at ~250 bytes per
/// frame on x86-64: a default 8 MB main thread overflows somewhere between
/// 30 000 and 35 000 levels, and — this is the part that matters — a stack
/// overflow **aborts the process**. On a robot that is the worst failure mode
/// available.
///
/// 512 leaves ~128 KB of stack in play, which is safe even on a thread created
/// with a small stack, and is an order of magnitude deeper than any hand-written
/// behavior tree. Raise it with [`TreeBuilder::with_max_depth`] if a generator
/// genuinely produces deeper ones, having checked the stack it will run on.
pub const DEFAULT_MAX_DEPTH: usize = 512;

/// Why a tree could not be built.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BuildError {
    /// `build()` with nothing in it.
    Empty,
    /// More `begin()` than `end()`, or the reverse.
    UnbalancedScopes { open: usize },
    /// More than one node at the top level — a tree has exactly one root.
    MultipleRoots { found: usize },
    /// A node has the wrong number of children for its type.
    Arity {
        node: NodeId,
        name: String,
        type_name: &'static str,
        expected: Arity,
        found: u16,
    },
    /// A `Parallel` exceeded its completion bitset.
    TooManyParallelChildren { node: NodeId, found: u16 },
    /// A value was given for a port the node type does not declare.
    ///
    /// A typo in a port name is otherwise invisible: the value is simply never
    /// read, and the node runs on its default while looking configured.
    UnknownPort {
        node: NodeId,
        name: String,
        type_name: &'static str,
        port: String,
    },
    /// A readable port with no value and no default.
    MissingPort {
        node: NodeId,
        name: String,
        port: &'static str,
    },
    /// The value could not be bound — a bad literal, or a key of another type.
    PortBinding {
        node: NodeId,
        name: String,
        source: PortError,
    },
    /// A literal was given for an output-only port, which would write into
    /// nothing.
    LiteralOnOutput {
        node: NodeId,
        name: String,
        port: &'static str,
    },
    /// The same port was given a value twice.
    DuplicatePort {
        node: NodeId,
        name: String,
        port: String,
    },
    /// The tree nests deeper than ticking it could survive.
    ///
    /// Caught here because the alternative is a stack overflow during a tick,
    /// and that is a process abort rather than an error anyone can handle.
    TooDeep { depth: usize, max: usize },
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BuildError::Empty => f.write_str("cannot build an empty tree"),
            BuildError::UnbalancedScopes { open } => {
                write!(f, "{open} control scope(s) left open — missing end()")
            }
            BuildError::MultipleRoots { found } => {
                write!(f, "tree has {found} roots, expected exactly 1")
            }
            BuildError::Arity {
                name,
                type_name,
                expected,
                found,
                ..
            } => write!(f, "{type_name} {name:?} expects {expected} but has {found}"),
            BuildError::TooManyParallelChildren { found, .. } => write!(
                f,
                "Parallel has {found} children, over the {MAX_PARALLEL_CHILDREN} limit"
            ),
            BuildError::UnknownPort {
                name,
                type_name,
                port,
                ..
            } => write!(f, "{type_name} {name:?} has no port {port:?}"),
            BuildError::MissingPort { name, port, .. } => {
                write!(
                    f,
                    "node {name:?}: port {port:?} has no value and no default"
                )
            }
            BuildError::PortBinding { name, source, .. } => write!(f, "node {name:?}: {source}"),
            BuildError::LiteralOnOutput { name, port, .. } => write!(
                f,
                "node {name:?}: output port {port:?} was given a literal; it needs a {{key}}"
            ),
            BuildError::DuplicatePort { name, port, .. } => {
                write!(f, "node {name:?}: port {port:?} given twice")
            }
            BuildError::TooDeep { depth, max } => write!(
                f,
                "tree nests {depth} levels deep, over the limit of {max}; ticking recurses, \
                 so depth is stack"
            ),
        }
    }
}

impl core::error::Error for BuildError {}

/// A port value as written in the tree source, before binding.
#[derive(Clone, Debug)]
struct PortAssignment {
    port: String,
    value: String,
}

struct Pending {
    name: NameId,
    kind: NodeKind,
    manifest: &'static NodeManifest,
    children: Vec<NodeId>,
    parent: NodeId,
    assignments: Vec<PortAssignment>,
}

impl fmt::Debug for Pending {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pending")
            .field("type", &self.manifest.type_name)
            .field("children", &self.children.len())
            .field("ports", &self.assignments.len())
            .finish_non_exhaustive()
    }
}

/// `{key}` means "the blackboard key named key"; anything else is a literal.
///
/// One syntax, matching BehaviorTree.CPP and Python BTEng, so a tree written for
/// any of them reads the same way here.
fn as_key(value: &str) -> Option<&str> {
    let v = value.trim();
    v.strip_prefix('{')
        .and_then(|v| v.strip_suffix('}'))
        .map(str::trim)
        .filter(|k| !k.is_empty())
}

/// Fluent builder for an arena tree.
///
/// ```
/// use bteng_core::{Blackboard, Builtin, TreeBuilder};
///
/// let mut bb = Blackboard::new();
/// let tree = TreeBuilder::new()
///     .begin("root", Builtin::sequence())
///         .leaf("check", Builtin::condition(|_| true))
///         .leaf("wait", Builtin::sleep()).port("msec", "50")
///     .end()
///     .build(&mut bb)
///     .unwrap();
/// assert_eq!(tree.len(), 3);
/// ```
#[derive(Debug)]
pub struct TreeBuilder {
    nodes: Vec<Pending>,
    names: Vec<Box<str>>,
    /// Indices of nodes whose scope is still open.
    stack: Vec<usize>,
    /// The node `port()` applies to.
    last: Option<usize>,
    roots: Vec<NodeId>,
    next_uid: u64,
    /// Deepest scope nesting seen. Checked at build.
    max_depth_seen: usize,
    max_depth: usize,
}

impl Default for TreeBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl TreeBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            names: Vec::new(),
            stack: Vec::new(),
            last: None,
            roots: Vec::new(),
            next_uid: 0,
            max_depth_seen: 0,
            max_depth: DEFAULT_MAX_DEPTH,
        }
    }

    /// Change the nesting limit. See [`DEFAULT_MAX_DEPTH`] before raising it.
    #[must_use]
    pub fn with_max_depth(mut self, max_depth: usize) -> Self {
        self.max_depth = max_depth;
        self
    }

    fn intern(&mut self, name: &str) -> NameId {
        // Linear scan: build time only, and trees have tens of distinct names.
        if let Some(i) = self.names.iter().position(|n| &**n == name) {
            return NameId(i as u32);
        }
        self.names.push(name.into());
        NameId((self.names.len() - 1) as u32)
    }

    fn push(&mut self, name: &str, kind: NodeKind, manifest: &'static NodeManifest) -> NodeId {
        let name_id = self.intern(name);
        let id = NodeId(self.nodes.len() as u32);
        let parent = self.stack.last().map_or(id, |&p| NodeId(p as u32));
        self.nodes.push(Pending {
            name: name_id,
            kind,
            manifest,
            children: Vec::new(),
            parent,
            assignments: Vec::new(),
        });
        match self.stack.last() {
            Some(&p) => self.nodes[p].children.push(id),
            None => self.roots.push(id),
        }
        self.max_depth_seen = self.max_depth_seen.max(self.stack.len() + 1);
        self.last = Some(id.index());
        id
    }

    /// Add a built-in and open its scope. Every `begin` needs a matching
    /// [`end`](TreeBuilder::end).
    pub fn begin(&mut self, name: &str, kind: Builtin) -> &mut Self {
        let manifest = kind.manifest();
        let id = self.push(name, NodeKind::Builtin(kind), manifest);
        self.stack.push(id.index());
        self
    }

    /// Add a custom node and open its scope.
    pub fn begin_custom(
        &mut self,
        name: &str,
        manifest: &'static NodeManifest,
        node: Box<dyn Node>,
    ) -> &mut Self {
        let id = self.push(name, NodeKind::Custom(node), manifest);
        self.stack.push(id.index());
        self
    }

    /// Add a childless built-in.
    pub fn leaf(&mut self, name: &str, kind: Builtin) -> &mut Self {
        let manifest = kind.manifest();
        self.push(name, NodeKind::Builtin(kind), manifest);
        self
    }

    /// Add a node from a [`NodeKind`] and open its scope.
    ///
    /// What a registry-driven front end (XML, and later a patch) uses: the
    /// registry hands back a `NodeKind` that may be either a built-in or a user
    /// node, and neither the front end nor this method needs to care which.
    pub fn begin_kind(
        &mut self,
        name: &str,
        manifest: &'static NodeManifest,
        kind: NodeKind,
    ) -> &mut Self {
        let id = self.push(name, kind, manifest);
        self.stack.push(id.index());
        self
    }

    /// Add a childless node from a [`NodeKind`].
    pub fn leaf_kind(
        &mut self,
        name: &str,
        manifest: &'static NodeManifest,
        kind: NodeKind,
    ) -> &mut Self {
        self.push(name, kind, manifest);
        self
    }

    /// Add a childless custom node.
    pub fn leaf_custom(
        &mut self,
        name: &str,
        manifest: &'static NodeManifest,
        node: Box<dyn Node>,
    ) -> &mut Self {
        self.push(name, NodeKind::Custom(node), manifest);
        self
    }

    /// Give the most recently added node a port value.
    ///
    /// `"{key}"` binds to a blackboard key; anything else is a literal, parsed
    /// at [`build`](TreeBuilder::build) time into the port's declared type.
    pub fn port(&mut self, port: &str, value: &str) -> &mut Self {
        if let Some(i) = self.last {
            self.nodes[i].assignments.push(PortAssignment {
                port: port.to_string(),
                value: value.to_string(),
            });
        }
        self
    }

    /// Close the innermost open scope.
    pub fn end(&mut self) -> &mut Self {
        if let Some(i) = self.stack.pop() {
            // Ports now apply to the node whose scope just closed, not to its
            // last child — otherwise `.begin(x).leaf(y).end().port(..)` would
            // silently configure `y`.
            self.last = Some(i);
        }
        self
    }

    /// Validate, bind every port, and flatten into a [`TreeGraph`].
    ///
    /// Port binding needs the blackboard: `{key}` references resolve to slots
    /// there (created on first use), and literals are parsed into anonymous
    /// slots of their declared type. After this returns, the tree contains no
    /// port *names* at all — only slot indices.
    ///
    /// Consumes the builder's contents; the builder is left empty and reusable.
    ///
    /// # Errors
    /// See [`BuildError`].
    pub fn build(&mut self, bb: &mut Blackboard) -> Result<TreeGraph, BuildError> {
        if !self.stack.is_empty() {
            return Err(BuildError::UnbalancedScopes {
                open: self.stack.len(),
            });
        }
        if self.nodes.is_empty() {
            return Err(BuildError::Empty);
        }
        if self.roots.len() != 1 {
            return Err(BuildError::MultipleRoots {
                found: self.roots.len(),
            });
        }
        if self.max_depth_seen > self.max_depth {
            return Err(BuildError::TooDeep {
                depth: self.max_depth_seen,
                max: self.max_depth,
            });
        }

        let pending = core::mem::take(&mut self.nodes);
        let names = core::mem::take(&mut self.names);
        self.stack.clear();
        self.roots.clear();
        self.last = None;
        self.max_depth_seen = 0;
        let uid_base = self.next_uid;
        self.next_uid += pending.len() as u64;

        // ── arity ────────────────────────────────────────────────────────────
        // Checked once, here, so no control node has to defend itself against a
        // missing child on every tick.
        for (i, p) in pending.iter().enumerate() {
            let id = NodeId(i as u32);
            let found = p.children.len() as u16;
            if !p.manifest.arity.accepts(found) {
                return Err(BuildError::Arity {
                    node: id,
                    name: names[p.name.index()].to_string(),
                    type_name: p.manifest.type_name,
                    expected: p.manifest.arity,
                    found,
                });
            }
            if matches!(p.kind, NodeKind::Builtin(Builtin::Parallel { .. }))
                && found > MAX_PARALLEL_CHILDREN
            {
                return Err(BuildError::TooManyParallelChildren { node: id, found });
            }
        }

        // ── ports ────────────────────────────────────────────────────────────
        let mut all_bindings: Vec<Vec<Binding>> = Vec::with_capacity(pending.len());
        for (i, p) in pending.iter().enumerate() {
            let name = names[p.name.index()].to_string();
            all_bindings.push(bind_ports(NodeId(i as u32), p, &name, bb)?);
        }

        // ── flatten ──────────────────────────────────────────────────────────
        let total_children: usize = pending.iter().map(|p| p.children.len()).sum();
        let total_ports: usize = all_bindings.iter().map(Vec::len).sum();
        let mut structure = Vec::with_capacity(pending.len());
        let mut meta = Vec::with_capacity(pending.len());
        let mut children = Vec::with_capacity(total_children);
        let mut ports = Vec::with_capacity(total_ports);
        let mut behavior = Vec::with_capacity(pending.len());

        for (i, (p, bindings)) in pending.into_iter().zip(all_bindings).enumerate() {
            let first_child = children.len() as u32;
            let child_count = p.children.len() as u16;
            children.extend_from_slice(&p.children);

            let first_port = ports.len() as u32;
            let port_count = bindings.len() as u16;
            ports.extend_from_slice(&bindings);

            structure.push(NodeStruct {
                first_child,
                first_port,
                child_count,
                port_count,
            });
            meta.push(NodeMeta {
                // Monotonic across every tree this builder makes, never reused.
                // A patch that moves a node keeps its uid, so telemetry can tell
                // "same node, moved" from "new node".
                uid: Uid(uid_base + i as u64),
                name: p.name,
                parent: p.parent,
                manifest: p.manifest,
            });
            behavior.push(p.kind);
        }

        Ok(TreeGraph::from_parts(
            structure, meta, children, ports, behavior, names,
        ))
    }
}

/// Resolve one node's ports, in manifest order.
fn bind_ports(
    id: NodeId,
    p: &Pending,
    name: &str,
    bb: &mut Blackboard,
) -> Result<Vec<Binding>, BuildError> {
    for a in &p.assignments {
        if p.manifest.port_index(&a.port).is_none() {
            return Err(BuildError::UnknownPort {
                node: id,
                name: name.to_string(),
                type_name: p.manifest.type_name,
                port: a.port.clone(),
            });
        }
        if p.assignments.iter().filter(|b| b.port == a.port).count() > 1 {
            return Err(BuildError::DuplicatePort {
                node: id,
                name: name.to_string(),
                port: a.port.clone(),
            });
        }
    }

    let mut bindings = Vec::with_capacity(p.manifest.ports.len());
    for spec in p.manifest.ports {
        let given = p.assignments.iter().find(|a| a.port == spec.name);
        bindings.push(bind_one(
            id,
            name,
            spec,
            given.map(|a| a.value.as_str()),
            bb,
        )?);
    }
    Ok(bindings)
}

fn bind_one(
    id: NodeId,
    name: &str,
    spec: &PortSpec,
    given: Option<&str>,
    bb: &mut Blackboard,
) -> Result<Binding, BuildError> {
    let fail = |source: PortError| BuildError::PortBinding {
        node: id,
        name: name.to_string(),
        source,
    };

    if let Some(value) = given {
        return if let Some(key) = as_key(value) {
            spec.bind_key(bb, key)
                .map(|slot| Binding {
                    slot,
                    source: BindingSource::Key,
                })
                .map_err(fail)
        } else if spec.direction.writable() && !spec.direction.readable() {
            Err(BuildError::LiteralOnOutput {
                node: id,
                name: name.to_string(),
                port: spec.name,
            })
        } else {
            spec.bind_literal(bb, value)
                .map(|slot| Binding {
                    slot,
                    source: BindingSource::Literal,
                })
                .map_err(fail)
        };
    }

    if let Some(default) = spec.default {
        return spec
            .bind_literal(bb, default)
            .map(|slot| Binding {
                slot,
                source: BindingSource::Default,
            })
            .map_err(fail);
    }

    // An unwired output has to point somewhere, or every write would need a
    // branch on the tick path. It points at a slot nobody reads.
    if spec.direction.writable() {
        return Ok(Binding {
            slot: spec.bind_sink(bb),
            source: BindingSource::Sink,
        });
    }

    Err(BuildError::MissingPort {
        node: id,
        name: name.to_string(),
        port: spec.name,
    })
}
