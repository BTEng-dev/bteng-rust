//! The tick machinery: context, cursor, dispatch, lifecycle.
//!
//! Python BTEng pushes the tracer, inspector and thread pool into every node by
//! walking the tree and assigning private attributes, then re-walks after every
//! structural change. Here that state lives in [`Ctx`], which is passed down the
//! tick — nothing to inject, nothing to re-inject after a reconfiguration.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use crate::blackboard::Blackboard;
use crate::builtins::Builtin;
use crate::clock::{Clock, Nanos};
use crate::id::NodeId;
use crate::manifest::{Arity, NodeManifest};
use crate::observe::{NodeEvent, TraceSink};
use crate::port::{PortError, PortValue};
use crate::status::{NodeState, Status};
use crate::tree::TreeGraph;

/// Everything a node may reach outside its own subtree during a tick.
///
/// Grows over the milestones (trace sink, spawner, event bus, read-set
/// recorder). Nodes take it by `&mut`, so adding to it never changes a
/// signature.
pub struct Ctx<'a> {
    pub bb: &'a mut Blackboard,
    pub clock: &'a dyn Clock,
    /// Ticks executed by the owning driver so far. Bumped by [`crate::Tree::tick`].
    pub tick_index: u64,
    /// Where node transitions go, if anyone is listening.
    ///
    /// `None` is the common case and costs one branch per node visit. Recording
    /// transitions rather than ticks is what keeps a trace small enough to keep.
    pub trace: Option<&'a mut dyn TraceSink>,
}

impl<'a> Ctx<'a> {
    /// The usual construction. Fields stay public so the M2 scheduler can build
    /// one per region without going through a builder.
    pub fn new(bb: &'a mut Blackboard, clock: &'a dyn Clock) -> Self {
        Self {
            bb,
            clock,
            tick_index: 0,
            trace: None,
        }
    }

    /// Attach a trace sink for the lifetime of this context.
    #[must_use]
    pub fn with_trace(mut self, sink: &'a mut dyn TraceSink) -> Self {
        self.trace = Some(sink);
        self
    }

    #[must_use]
    pub fn now(&self) -> Nanos {
        self.clock.now()
    }
}

impl fmt::Debug for Ctx<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ctx")
            .field("tick_index", &self.tick_index)
            .field("slots", &self.bb.len())
            .finish_non_exhaustive()
    }
}

/// Setup failed. Carries a message rather than a code because it is reported
/// once, at startup, to a human.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetupError {
    pub message: String,
}

impl SetupError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for SetupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl core::error::Error for SetupError {}

/// A node's handle on its children and its ports during its tick.
///
/// A node never sees [`NodeId`]s or the arena — only "my child *i*" and "my port
/// *i*". That keeps node implementations position-independent, which is what
/// makes moving a subtree during a reconfiguration a graph edit rather than a
/// rewrite.
pub struct Cursor<'a, 'c> {
    graph: &'a mut TreeGraph,
    node: NodeId,
    ctx: &'a mut Ctx<'c>,
}

impl fmt::Debug for Cursor<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cursor")
            .field("node", &self.node)
            .field("children", &self.child_count())
            .field("ports", &self.graph.port_count(self.node))
            .finish_non_exhaustive()
    }
}

impl<'c> Cursor<'_, 'c> {
    /// This node's own arena id. Tooling and diagnostics only.
    #[must_use]
    pub fn node_id(&self) -> NodeId {
        self.node
    }

    /// This node type's declaration. Useful for naming a port in an error.
    #[must_use]
    pub fn manifest(&self) -> &'static NodeManifest {
        self.graph.manifest(self.node)
    }

    #[must_use]
    pub fn child_count(&self) -> u16 {
        self.graph.child_count(self.node)
    }

    /// Access to the shared tick context.
    pub fn ctx(&mut self) -> &mut Ctx<'c> {
        self.ctx
    }

    // ── children ─────────────────────────────────────────────────────────────

    /// Tick child `i` and return its status.
    pub fn tick_child(&mut self, i: u16) -> Status {
        let child = self.graph.child(self.node, i);
        tick_node(&mut *self.graph, child, &mut *self.ctx)
    }

    /// Last recorded state of child `i`, without ticking it.
    #[must_use]
    pub fn child_state(&self, i: u16) -> NodeState {
        self.graph.state(self.graph.child(self.node, i))
    }

    /// Halt child `i` and everything below it.
    pub fn halt_child(&mut self, i: u16) {
        let child = self.graph.child(self.node, i);
        halt_node(&mut *self.graph, child, &mut *self.ctx);
    }

    /// Halt children `from..`. The common "a sibling settled, stop the rest" move.
    pub fn halt_children_from(&mut self, from: u16) {
        for i in from..self.child_count() {
            self.halt_child(i);
        }
    }

    /// Halt every child.
    pub fn halt_all_children(&mut self) {
        self.halt_children_from(0);
    }

    // ── ports ────────────────────────────────────────────────────────────────
    //
    // Indices, not names. The index is the port's position in the node type's
    // manifest, fixed at compile time — so a port read is an array index and a
    // `TypeId` compare, with no string anywhere on the path.

    fn port_name(&self, i: u16) -> &'static str {
        self.manifest().port(i).map_or("?", |p| p.name)
    }

    fn slot_of(&self, i: u16) -> Result<crate::id::SlotId, PortError> {
        self.graph
            .port_slot(self.node, i)
            .ok_or(PortError::NoSuchPort {
                index: i,
                count: self.graph.port_count(self.node),
            })
    }

    /// Borrow port `i`'s value.
    ///
    /// Holds the context borrow, so ticking a child while it is live will not
    /// compile. Use [`Cursor::get`] when you need both.
    ///
    /// # Errors
    /// [`PortError::NoSuchPort`] or [`PortError::TypeMismatch`].
    pub fn input<T: PortValue>(&self, i: u16) -> Result<&T, PortError> {
        let slot = self.slot_of(i)?;
        self.ctx
            .bb
            .get::<T>(slot)
            .map_err(|_| PortError::TypeMismatch {
                port: self.port_name(i),
                expected: core::any::type_name::<T>(),
                slot,
            })
    }

    /// Copy port `i`'s value out, releasing the borrow.
    ///
    /// # Errors
    /// As [`Cursor::input`].
    pub fn get<T: PortValue>(&self, i: u16) -> Result<T, PortError> {
        self.input::<T>(i).cloned()
    }

    /// Copy port `i`'s value out, falling back to `fallback` on any error.
    ///
    /// Used by built-ins whose ports all have defaults: a misconfigured port
    /// must not turn into a status the tree would read as a decision.
    pub fn get_or<T: PortValue>(&self, i: u16, fallback: T) -> T {
        self.get::<T>(i).unwrap_or(fallback)
    }

    /// Write port `i`.
    ///
    /// # Errors
    /// [`PortError::NoSuchPort`] or [`PortError::TypeMismatch`].
    pub fn set<T: PortValue>(&mut self, i: u16, value: T) -> Result<(), PortError> {
        let slot = self.slot_of(i)?;
        let name = self.port_name(i);
        self.ctx
            .bb
            .set::<T>(slot, value)
            .map_err(|_| PortError::TypeMismatch {
                port: name,
                expected: core::any::type_name::<T>(),
                slot,
            })
    }
}

/// A user-defined node.
///
/// This trait is the extension surface promised to OPC UA, ROS 2 and whatever
/// follows (DR-6), so it is deliberately narrow: a node gets its children, its
/// ports and the context, and returns a status. It cannot reach the arena,
/// cannot hold a reference across a tick, and cannot see its own position.
pub trait Node: Send {
    fn tick(&mut self, cur: &mut Cursor<'_, '_>) -> Status;

    /// Cleanup when this node is halted while `Running`. Children are halted by
    /// the engine; this hook is for the node's *own* resources.
    ///
    /// Must not block. In M3 this becomes the contract that keeps a wedged
    /// async leaf from stalling the tick thread the way Python's 5-second
    /// `JOIN_TIMEOUT` does.
    fn halt(&mut self, _cur: &mut Cursor<'_, '_>) {}

    /// One-time initialisation, before the first tick. Open sockets here.
    ///
    /// # Errors
    /// Any reason the node cannot run.
    fn setup(&mut self, _ctx: &mut Ctx<'_>) -> Result<(), SetupError> {
        Ok(())
    }

    /// Release what [`Node::setup`] acquired. Runs even if setup failed.
    fn shutdown(&mut self, _ctx: &mut Ctx<'_>) {}

    /// For diagnostics and telemetry.
    fn type_name(&self) -> &'static str {
        "Custom"
    }
}

/// What sits in an arena slot.
///
/// Built-ins go through a `match` — no vtable, inlinable, and the hot path of
/// every real tree. User nodes pay one indirect call, which is the right place
/// to pay it.
pub enum NodeKind {
    /// Placeholder left behind while a node is mid-tick. See
    /// [`TreeGraph::take_behavior`].
    Vacant,
    Builtin(Builtin),
    Custom(Box<dyn Node>),
}

impl fmt::Debug for NodeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NodeKind::Vacant => f.write_str("Vacant"),
            NodeKind::Builtin(b) => write!(f, "Builtin({})", b.type_name()),
            NodeKind::Custom(n) => write!(f, "Custom({})", n.type_name()),
        }
    }
}

impl NodeKind {
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        match self {
            NodeKind::Vacant => "Vacant",
            NodeKind::Builtin(b) => b.type_name(),
            NodeKind::Custom(n) => n.type_name(),
        }
    }

    /// The declaration this kind should be validated against.
    ///
    /// Built-ins carry their own; a custom node's comes from the registry, which
    /// is why the builder has to be told one when it installs a `Custom`.
    #[must_use]
    pub fn builtin_manifest(&self) -> Option<&'static NodeManifest> {
        match self {
            NodeKind::Builtin(b) => Some(b.manifest()),
            NodeKind::Vacant | NodeKind::Custom(_) => None,
        }
    }

    #[must_use]
    pub fn arity(&self, fallback: &NodeManifest) -> Arity {
        self.builtin_manifest().map_or(fallback.arity, |m| m.arity)
    }
}

/// Tick one node.
///
/// The whole dance is: lift the behaviour out of the arena, run it against a
/// cursor that owns the arena for the duration, put it back, record the state.
/// Allocation-free, and the borrow checker proves the node cannot reach itself.
pub fn tick_node(graph: &mut TreeGraph, id: NodeId, ctx: &mut Ctx<'_>) -> Status {
    let mut kind = graph.take_behavior(id);

    let status = match &mut kind {
        NodeKind::Vacant => {
            // The node is already on the stack: something made a cycle. Failing
            // beats recursing until the stack dies, and beats panicking on a
            // path that a real-time profile may compile with `panic = "abort"`.
            debug_assert!(
                false,
                "cycle detected: node {id:?} ticked while already ticking"
            );
            Status::Failure
        }
        NodeKind::Builtin(b) => {
            let mut cur = Cursor {
                graph: &mut *graph,
                node: id,
                ctx: &mut *ctx,
            };
            b.tick(&mut cur)
        }
        NodeKind::Custom(n) => {
            let mut cur = Cursor {
                graph: &mut *graph,
                node: id,
                ctx: &mut *ctx,
            };
            n.tick(&mut cur)
        }
    };

    graph.put_behavior(id, kind);

    let to: NodeState = status.into();
    if let Some(sink) = ctx.trace.as_deref_mut() {
        let from = graph.state(id);
        if from != to {
            sink.node_event(NodeEvent {
                node: id,
                uid: graph.uid(id),
                from,
                to,
                at: ctx.clock.now(),
                tick: ctx.tick_index,
            });
        }
    }
    graph.set_state(id, to);
    status
}

/// Halt a node and its subtree, returning them to `Idle`.
///
/// Cleanup only fires for nodes that were actually `Running` — halting an idle
/// branch must be free, because control nodes do it constantly.
pub fn halt_node(graph: &mut TreeGraph, id: NodeId, ctx: &mut Ctx<'_>) {
    if graph.state(id) == NodeState::Idle {
        return;
    }

    let mut kind = graph.take_behavior(id);
    if graph.state(id).is_running() {
        let mut cur = Cursor {
            graph: &mut *graph,
            node: id,
            ctx: &mut *ctx,
        };
        match &mut kind {
            NodeKind::Vacant => {}
            NodeKind::Builtin(b) => b.halt(&mut cur),
            NodeKind::Custom(n) => n.halt(&mut cur),
        }
    }
    graph.put_behavior(id, kind);

    for i in 0..graph.child_count(id) {
        let child = graph.child(id, i);
        halt_node(graph, child, ctx);
    }

    // A halt is not a tick, so nothing else would report it. Without this a node
    // stopped by a Timeout or a reactive sibling stays Running in every live
    // view forever — the exact stale-active-node bug Python BTEng had to patch.
    if let Some(sink) = ctx.trace.as_deref_mut() {
        let from = graph.state(id);
        if from != NodeState::Idle {
            sink.node_event(NodeEvent {
                node: id,
                uid: graph.uid(id),
                from,
                to: NodeState::Idle,
                at: ctx.clock.now(),
                tick: ctx.tick_index,
            });
        }
    }
    graph.set_state(id, NodeState::Idle);
}

/// Run [`Node::setup`] on every node, collecting every failure.
///
/// All of them, not the first: a startup that reports one missing device per run
/// turns bring-up into a guessing game.
pub fn setup_all(graph: &mut TreeGraph, ctx: &mut Ctx<'_>) -> Vec<(NodeId, SetupError)> {
    let mut errors = Vec::new();
    let ids: Vec<NodeId> = graph.ids().collect();
    for id in ids {
        let mut kind = graph.take_behavior(id);
        if let NodeKind::Custom(n) = &mut kind
            && let Err(e) = n.setup(ctx)
        {
            errors.push((id, e));
        }
        graph.put_behavior(id, kind);
    }
    errors
}

/// Run [`Node::shutdown`] on every node, deepest first.
///
/// Reverse arena order is children before parents, since the builder emits
/// parents first.
pub fn shutdown_all(graph: &mut TreeGraph, ctx: &mut Ctx<'_>) {
    let ids: Vec<NodeId> = graph.ids().collect();
    for id in ids.into_iter().rev() {
        let mut kind = graph.take_behavior(id);
        if let NodeKind::Custom(n) = &mut kind {
            n.shutdown(ctx);
        }
        graph.put_behavior(id, kind);
    }
}
