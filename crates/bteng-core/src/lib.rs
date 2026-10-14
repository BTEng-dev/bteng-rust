//! BTEng-Rust core — M0 representation spike.
//!
//! This crate exists to answer one question before anything is built on top of
//! it: *is the arena tree + slot blackboard representation fast and
//! allocation-free in the steady-state tick?*
//!
//! Two representation choices carry the whole design:
//!
//! 1. **The tree is an arena, not a pointer graph.**  Nodes are [`NodeId`]
//!    indices into flat vectors; edges live in one contiguous child array.  That
//!    gives cache-friendly traversal, stable identity across structural edits,
//!    and — the reason it is not negotiable — no `&`-borrow that a live
//!    reconfiguration could invalidate.  There are no references to dangle, only
//!    indices.
//!
//! 2. **The blackboard is a slot arena with version counters.**  A port binds to
//!    a [`SlotId`] once, at build time, where its type is checked.  Ticking never
//!    hashes a string, never walks a scope chain, never takes a lock.  The
//!    per-slot `version` is what later turns into precise read sets for
//!    event-driven scheduling — reactivity becomes "compare the versions I read"
//!    instead of a callback storm.
//!
//! Not in M0, by design: XML, the multi-rate scheduler, async leaves, reactive
//! control nodes, reconfiguration, telemetry.

#![cfg_attr(not(feature = "std"), no_std)]
// DR-3: the core stays alloc-first. Reaching for `std` where `core` or `alloc`
// would do is the exact mistake that closes the no_std door without anyone
// noticing, so it is a lint here — and only here; `bteng-xml` is hosted by
// design and would only be nagged by it.
#![warn(
    clippy::alloc_instead_of_core,
    clippy::std_instead_of_alloc,
    clippy::std_instead_of_core
)]

extern crate alloc;

use alloc::vec::Vec;

mod blackboard;
mod builder;
mod builtins;
mod clock;
mod id;
mod manifest;
mod node;
mod observe;
mod patch;
mod port;
mod registry;
mod status;
mod tree;

pub use blackboard::{BbError, Blackboard};
pub use builder::{BuildError, DEFAULT_MAX_DEPTH, TreeBuilder};
pub use builtins::{
    ActionFn, Builtin, ConditionFn, MAX_PARALLEL_CHILDREN, REGION_PORT_HZ, REGION_PORT_MIN_PERIOD,
    REGION_PORT_MODE, REGION_PORT_OVERRUN,
};
#[cfg(feature = "std")]
pub use clock::SteadyClock;
pub use clock::{Clock, Nanos, SimClock};
pub use id::{NameId, NodeId, SlotId, Uid};
pub use manifest::{Arity, NodeCategory, NodeManifest};
pub use node::{
    Ctx, Cursor, Node, NodeKind, SetupError, halt_node, setup_all, shutdown_all, tick_node,
};
pub use observe::{NoWaker, NodeEvent, TraceSink, Waker};
pub use patch::{Applied, PatchError, PatchPlan, TreePatch, TreeVersion, apply};
pub use port::{
    Binding, BindingSource, ParseError, PortDirection, PortError, PortSpec, PortValue, inout,
    input, output,
};
pub use registry::{NodeCtor, NodeRegistry, Registration, RegistryError};
pub use status::{NodeState, Status};
pub use tree::{NodeMeta, NodeStruct, TreeGraph};

/// A built behavior tree: the arena plus the ergonomics to drive it.
///
/// Deliberately thin. The scheduler (M2) will drive [`TreeGraph`] directly for
/// each region, so nothing important may accumulate here.
#[derive(Debug)]
pub struct Tree {
    graph: TreeGraph,
}

impl Tree {
    /// Wrap a built graph.
    #[must_use]
    pub fn new(graph: TreeGraph) -> Self {
        Self { graph }
    }

    /// Tick the root once.
    pub fn tick(&mut self, ctx: &mut Ctx<'_>) -> Status {
        let status = tick_node(&mut self.graph, NodeId::ROOT, ctx);
        ctx.tick_index += 1;
        status
    }

    /// Halt every running node, returning the tree to `Idle`.
    pub fn halt(&mut self, ctx: &mut Ctx<'_>) {
        halt_node(&mut self.graph, NodeId::ROOT, ctx);
    }

    /// Run every node's one-time initialisation.
    ///
    /// Returns *every* failure rather than the first: a startup that reports one
    /// missing device per run turns bring-up into a guessing game.
    ///
    /// # Errors
    /// The failing nodes, paired with what they reported.
    pub fn setup(&mut self, ctx: &mut Ctx<'_>) -> Result<(), Vec<(NodeId, SetupError)>> {
        let errors = setup_all(&mut self.graph, ctx);
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// Release what `setup` acquired. Safe to call without a prior setup.
    pub fn shutdown(&mut self, ctx: &mut Ctx<'_>) {
        shutdown_all(&mut self.graph, ctx);
    }

    #[must_use]
    pub fn graph(&self) -> &TreeGraph {
        &self.graph
    }

    #[must_use]
    pub fn graph_mut(&mut self) -> &mut TreeGraph {
        &mut self.graph
    }

    /// Give the arena up — what a patch consumes.
    #[must_use]
    pub fn into_graph(self) -> TreeGraph {
        self.graph
    }
}
