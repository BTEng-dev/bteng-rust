//! The arena.
//!
//! Six parallel vectors indexed by [`NodeId`], split by access pattern rather
//! than by what "belongs together" conceptually:
//!
//! * **hot** — [`NodeStruct`] (child and port ranges), `behavior`, `state`.
//!   Touched on every node visit.
//! * **cold** — [`NodeMeta`] (uid, name, parent, manifest). Touched by
//!   validation, telemetry and diagnostics, never by a tick.
//!
//! M0 kept uid and name in the hot struct and measured 24 bytes of it being
//! walked linearly for no tick-time benefit. Splitting takes `NodeStruct` to 12.
//!
//! `structure`, `children` and `ports` are immutable for the whole tick, so
//! they can be shared freely while `behavior` and `state` are mutated — which is
//! what lets a control node tick children living in the same arena.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::mem;

use crate::id::{NameId, NodeId, SlotId, Uid};
use crate::manifest::NodeManifest;
use crate::node::NodeKind;
use crate::port::Binding;
use crate::status::{NodeState, Status};

/// A node's ranges into the flat child and port arrays. 12 bytes.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct NodeStruct {
    /// Offset into the graph's flat child array.
    pub first_child: u32,
    /// Offset into the graph's flat port-binding array.
    pub first_port: u32,
    pub child_count: u16,
    pub port_count: u16,
}

/// Everything about a node that a tick does not need.
#[derive(Copy, Clone, Debug)]
pub struct NodeMeta {
    /// Identity that survives structural edits. See [`Uid`].
    pub uid: Uid,
    pub name: NameId,
    pub parent: NodeId,
    /// What this node type declares about itself.
    pub manifest: &'static NodeManifest,
}

/// A built behavior tree in arena form.
#[derive(Debug)]
pub struct TreeGraph {
    structure: Vec<NodeStruct>,
    meta: Vec<NodeMeta>,
    children: Vec<NodeId>,
    ports: Vec<Binding>,
    behavior: Vec<NodeKind>,
    state: Vec<NodeState>,
    names: Vec<Box<str>>,
    version: crate::patch::TreeVersion,
}

impl TreeGraph {
    pub(crate) fn from_parts(
        structure: Vec<NodeStruct>,
        meta: Vec<NodeMeta>,
        children: Vec<NodeId>,
        ports: Vec<Binding>,
        behavior: Vec<NodeKind>,
        names: Vec<Box<str>>,
    ) -> Self {
        let state = alloc::vec![NodeState::Idle; structure.len()];
        Self {
            structure,
            meta,
            children,
            ports,
            behavior,
            state,
            names,
            version: crate::patch::TreeVersion(0),
        }
    }

    /// Rebuild from parts a patch has moved around.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_moved_parts(
        structure: Vec<NodeStruct>,
        meta: Vec<NodeMeta>,
        children: Vec<NodeId>,
        ports: Vec<Binding>,
        behavior: Vec<NodeKind>,
        state: Vec<NodeState>,
        names: Vec<Box<str>>,
        version: crate::patch::TreeVersion,
    ) -> Self {
        Self {
            structure,
            meta,
            children,
            ports,
            behavior,
            state,
            names,
            version,
        }
    }

    /// Take the arena apart. Only the patch machinery needs this.
    #[allow(clippy::type_complexity)]
    pub(crate) fn into_parts(
        self,
    ) -> (
        Vec<NodeStruct>,
        Vec<NodeMeta>,
        Vec<NodeId>,
        Vec<Binding>,
        Vec<NodeKind>,
        Vec<NodeState>,
        Vec<Box<str>>,
    ) {
        (
            self.structure,
            self.meta,
            self.children,
            self.ports,
            self.behavior,
            self.state,
            self.names,
        )
    }

    /// How many structural edits this graph has seen.
    #[must_use]
    pub fn version(&self) -> crate::patch::TreeVersion {
        self.version
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.structure.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.structure.is_empty()
    }

    // ── hot ──────────────────────────────────────────────────────────────────

    #[must_use]
    pub fn structure(&self, id: NodeId) -> &NodeStruct {
        &self.structure[id.index()]
    }

    #[must_use]
    pub fn state(&self, id: NodeId) -> NodeState {
        self.state[id.index()]
    }

    pub(crate) fn set_state(&mut self, id: NodeId, s: NodeState) {
        self.state[id.index()] = s;
    }

    #[must_use]
    pub fn child_count(&self, id: NodeId) -> u16 {
        self.structure[id.index()].child_count
    }

    /// The children of `id`, in order.
    #[must_use]
    pub fn children_of(&self, id: NodeId) -> &[NodeId] {
        let s = &self.structure[id.index()];
        let start = s.first_child as usize;
        &self.children[start..start + s.child_count as usize]
    }

    /// `i`-th child of `id`.
    ///
    /// # Panics
    /// If `i` is out of range — an arity bug in the builder, not a runtime
    /// condition, and one that should be loud.
    #[must_use]
    pub fn child(&self, id: NodeId, i: u16) -> NodeId {
        let s = &self.structure[id.index()];
        assert!(i < s.child_count, "child index {i} out of range");
        self.children[s.first_child as usize + i as usize]
    }

    /// The slot a node's `i`-th port resolved to, or `None` if it has no such
    /// port.
    #[must_use]
    pub fn port_slot(&self, id: NodeId, i: u16) -> Option<SlotId> {
        let s = &self.structure[id.index()];
        (i < s.port_count).then(|| self.ports[s.first_port as usize + i as usize].slot)
    }

    #[must_use]
    pub fn port_count(&self, id: NodeId) -> u16 {
        self.structure[id.index()].port_count
    }

    /// All bindings of a node, in declaration order. Tooling and telemetry.
    #[must_use]
    pub fn bindings_of(&self, id: NodeId) -> &[Binding] {
        let s = &self.structure[id.index()];
        let start = s.first_port as usize;
        &self.ports[start..start + s.port_count as usize]
    }

    // ── cold ─────────────────────────────────────────────────────────────────

    #[must_use]
    pub fn meta(&self, id: NodeId) -> &NodeMeta {
        &self.meta[id.index()]
    }

    /// Human-readable instance name, for tooling and telemetry only.
    #[must_use]
    pub fn name(&self, id: NodeId) -> &str {
        &self.names[self.meta[id.index()].name.index()]
    }

    #[must_use]
    pub fn manifest(&self, id: NodeId) -> &'static NodeManifest {
        self.meta[id.index()].manifest
    }

    #[must_use]
    pub fn uid(&self, id: NodeId) -> Uid {
        self.meta[id.index()].uid
    }

    #[must_use]
    pub fn parent(&self, id: NodeId) -> NodeId {
        self.meta[id.index()].parent
    }

    // ── behaviour ────────────────────────────────────────────────────────────

    /// Lift a node's behaviour out of the arena for the duration of its tick.
    ///
    /// This is the trick that makes the arena work with `&mut`: while a node is
    /// ticking, its behaviour is *not* in the arena, so the arena itself stays
    /// freely borrowable for the children it wants to tick. `Vacant` is left
    /// behind, and finding `Vacant` during a tick means the node is somehow
    /// ticking itself — a cycle.
    ///
    /// `mem::replace` of a small enum; no allocation.
    pub(crate) fn take_behavior(&mut self, id: NodeId) -> NodeKind {
        mem::replace(&mut self.behavior[id.index()], NodeKind::Vacant)
    }

    pub(crate) fn put_behavior(&mut self, id: NodeId, kind: NodeKind) {
        self.behavior[id.index()] = kind;
    }

    // ── regions ──────────────────────────────────────────────────────────────
    //
    // A `Region` node is a scheduling boundary: its subtree is ticked by the
    // scheduler on the region's own clock, not by the parent's traversal. The
    // parent reads the last published verdict. These three accessors are the
    // whole contract between the graph and `bteng-sched`.

    /// Is this node a scheduling boundary?
    #[must_use]
    pub fn is_region(&self, id: NodeId) -> bool {
        matches!(
            self.behavior[id.index()],
            NodeKind::Builtin(crate::builtins::Builtin::Region { .. })
        )
    }

    /// The verdict a region last published, or `None` if `id` is not a region.
    ///
    /// Returns `None` mid-tick, when the node's behaviour is lifted out of the
    /// arena — call it between ticks.
    #[must_use]
    pub fn region_status(&self, id: NodeId) -> Option<Status> {
        match &self.behavior[id.index()] {
            NodeKind::Builtin(crate::builtins::Builtin::Region { cached }) => Some(*cached),
            _ => None,
        }
    }

    /// Publish a region's verdict. Returns false if `id` is not a region.
    ///
    /// The scheduler calls this after ticking the region's subtree; nothing else
    /// should.
    pub fn set_region_status(&mut self, id: NodeId, status: Status) -> bool {
        match &mut self.behavior[id.index()] {
            NodeKind::Builtin(crate::builtins::Builtin::Region { cached }) => {
                *cached = status;
                true
            }
            _ => false,
        }
    }

    /// Every region boundary in the graph, in arena order.
    #[must_use]
    pub fn region_ids(&self) -> Vec<NodeId> {
        self.ids().filter(|id| self.is_region(*id)).collect()
    }

    /// Every node id in arena order (root first, parents before children).
    pub fn ids(&self) -> impl Iterator<Item = NodeId> + '_ {
        (0..self.structure.len() as u32).map(NodeId)
    }
}
