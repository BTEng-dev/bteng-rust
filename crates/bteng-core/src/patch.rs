//! Structural edits to a live tree.
//!
//! # Why this lives in core
//!
//! The plan put patching in its own crate. It is arena surgery — it has to move
//! `NodeKind` values out of one graph and into another — and exposing the raw
//! parallel vectors to do that from outside would make the arena's layout public
//! API forever. It lives next to the arena instead.
//!
//! # How a patch works
//!
//! **Build outside, install inside.** The replacement subtree is built and
//! validated by [`TreeBuilder`](crate::TreeBuilder) like any other tree, off the
//! tick thread. [`TreePatch::validate`] then checks it against the live graph —
//! target exists, arity still holds, nothing structural is impossible — and
//! yields a [`PatchPlan`]. Only [`apply`] touches the tree, and by then nothing
//! can fail.
//!
//! Python BTEng's equivalent probes `hasattr(parent, "_children")`, mutates the
//! private list, and then halts the parent to avoid a stale child cursor — so a
//! running branch is *restarted* by any edit near it, and nothing about the
//! incoming subtree is checked at all.
//!
//! # State survives
//!
//! Surviving nodes are **moved**, not rebuilt. A `Sequence` three children deep
//! into its run keeps its cursor, an async leaf keeps its task handle, and a
//! `Running` node stays `Running` — because the value itself is carried across,
//! not reconstructed from a description of it. Identity is [`Uid`], which is why
//! it exists.
//!
//! # Rollback
//!
//! [`apply`] hands back the subtree it removed *and* the inverse patch. Applying
//! one to the other restores the previous tree exactly, live state included.

use alloc::boxed::Box;
use alloc::collections::BTreeSet;
use alloc::string::ToString as _;
use alloc::vec::Vec;
use core::fmt;

use crate::id::{NameId, NodeId, Uid};
use crate::manifest::NodeManifest;
use crate::node::NodeKind;
use crate::port::Binding;
use crate::status::NodeState;
use crate::tree::{NodeMeta, NodeStruct, TreeGraph};

/// How many times a graph has been structurally edited.
///
/// Stamped into telemetry so a recorded run can be read against the shape the
/// tree actually had at each moment, rather than the shape it started with.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct TreeVersion(pub u64);

/// One structural edit.
///
/// Exactly one per patch. Batching several is possible but every extra op
/// multiplies the ways validation can be subtly wrong about what the tree looks
/// like halfway through, and nothing needs it yet.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TreePatch {
    /// Swap out the subtree rooted at `target`, keeping its position.
    Replace { target: Uid },
    /// Add a child to `parent` at `index`.
    InsertChild { parent: Uid, index: u16 },
    /// Take the child at `index` away from `parent`.
    RemoveChild { parent: Uid, index: u16 },
}

/// Why a patch could not be applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PatchError {
    /// No node in the graph has that uid.
    NoSuchNode { uid: Uid },
    /// The op needs a replacement subtree and none was given, or the reverse.
    IncomingMismatch { expected: bool },
    /// The replacement subtree is empty.
    EmptyIncoming,
    /// `index` is outside the parent's children.
    ChildIndexOutOfRange { parent: Uid, index: u16, count: u16 },
    /// The edit would leave a node with a child count its type forbids.
    ///
    /// Checked here, before anything moves, rather than discovered by a control
    /// node mid-tick.
    Arity {
        parent: Uid,
        type_name: &'static str,
        expected: crate::manifest::Arity,
        would_be: u16,
    },
    /// Replacing the root is not an edit, it is a new tree. Say so explicitly.
    CannotReplaceRoot,
    /// The edit would nest the tree deeper than ticking it could survive.
    ///
    /// The builder refuses a too-deep tree; a patch has to refuse to *make* one,
    /// or the limit is only enforced against trees nobody edits.
    TooDeep { depth: usize, max: usize },
}

impl fmt::Display for PatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PatchError::NoSuchNode { uid } => write!(f, "no node with {uid:?}"),
            PatchError::IncomingMismatch { expected } => {
                if *expected {
                    f.write_str("this patch needs a replacement subtree")
                } else {
                    f.write_str("this patch takes no replacement subtree")
                }
            }
            PatchError::EmptyIncoming => f.write_str("the replacement subtree is empty"),
            PatchError::ChildIndexOutOfRange {
                parent,
                index,
                count,
            } => write!(f, "{parent:?} has {count} children, no index {index}"),
            PatchError::Arity {
                type_name,
                expected,
                would_be,
                ..
            } => write!(
                f,
                "{type_name} requires {expected}; the edit would leave {would_be}"
            ),
            PatchError::CannotReplaceRoot => {
                f.write_str("replacing the root is a new tree, not a patch")
            }
            PatchError::TooDeep { depth, max } => write!(
                f,
                "the edit would nest {depth} levels deep, over the limit of {max}"
            ),
        }
    }
}

impl core::error::Error for PatchError {}

/// A validated patch. Applying one cannot fail.
///
/// The type is the guarantee: everything that could go wrong was decided on the
/// builder thread, and what reaches the tick boundary is a bounded splice.
#[derive(Copy, Clone, Debug)]
pub struct PatchPlan {
    patch: TreePatch,
    /// The node being replaced or removed, resolved to a position.
    target: NodeId,
    /// The parent whose child list changes.
    parent: NodeId,
    index: u16,
    needs_incoming: bool,
}

impl PatchPlan {
    #[must_use]
    pub const fn patch(&self) -> TreePatch {
        self.patch
    }

    /// The node this patch removes, if it removes one.
    #[must_use]
    pub const fn target(&self) -> Option<NodeId> {
        match self.patch {
            TreePatch::InsertChild { .. } => None,
            _ => Some(self.target),
        }
    }

    #[must_use]
    pub const fn needs_incoming(&self) -> bool {
        self.needs_incoming
    }
}

impl TreePatch {
    /// Check this patch against a live graph.
    ///
    /// # Errors
    /// See [`PatchError`].
    pub fn validate(
        self,
        graph: &TreeGraph,
        incoming: Option<&TreeGraph>,
    ) -> Result<PatchPlan, PatchError> {
        let needs_incoming = !matches!(self, TreePatch::RemoveChild { .. });
        if needs_incoming != incoming.is_some() {
            return Err(PatchError::IncomingMismatch {
                expected: needs_incoming,
            });
        }
        if let Some(g) = incoming
            && g.is_empty()
        {
            return Err(PatchError::EmptyIncoming);
        }

        // Depth is stack: ticking recurses, and a stack overflow is a process
        // abort rather than an error. The builder refuses a too-deep tree, so a
        // patch has to refuse to make one.
        if let Some(sub) = incoming {
            let anchor = match self {
                TreePatch::Replace { target } => depth_of(graph, find(graph, target)?) - 1,
                TreePatch::InsertChild { parent, .. } => depth_of(graph, find(graph, parent)?),
                TreePatch::RemoveChild { .. } => 0,
            };
            let would_be = anchor + height(sub, NodeId::ROOT);
            if would_be > crate::builder::DEFAULT_MAX_DEPTH {
                return Err(PatchError::TooDeep {
                    depth: would_be,
                    max: crate::builder::DEFAULT_MAX_DEPTH,
                });
            }
        }

        match self {
            TreePatch::Replace { target } => {
                let target_id = find(graph, target)?;
                if target_id == NodeId::ROOT {
                    return Err(PatchError::CannotReplaceRoot);
                }
                let parent = graph.parent(target_id);
                let index = child_index(graph, parent, target_id);
                Ok(PatchPlan {
                    patch: self,
                    target: target_id,
                    parent,
                    index,
                    needs_incoming,
                })
            }
            TreePatch::InsertChild { parent, index } => {
                let parent_id = find(graph, parent)?;
                let count = graph.child_count(parent_id);
                if index > count {
                    return Err(PatchError::ChildIndexOutOfRange {
                        parent,
                        index,
                        count,
                    });
                }
                check_arity(graph, parent_id, parent, count + 1)?;
                Ok(PatchPlan {
                    patch: self,
                    target: parent_id,
                    parent: parent_id,
                    index,
                    needs_incoming,
                })
            }
            TreePatch::RemoveChild { parent, index } => {
                let parent_id = find(graph, parent)?;
                let count = graph.child_count(parent_id);
                if index >= count {
                    return Err(PatchError::ChildIndexOutOfRange {
                        parent,
                        index,
                        count,
                    });
                }
                check_arity(graph, parent_id, parent, count - 1)?;
                Ok(PatchPlan {
                    patch: self,
                    target: graph.child(parent_id, index),
                    parent: parent_id,
                    index,
                    needs_incoming,
                })
            }
        }
    }
}

fn find(graph: &TreeGraph, uid: Uid) -> Result<NodeId, PatchError> {
    graph
        .ids()
        .find(|id| graph.uid(*id) == uid)
        .ok_or(PatchError::NoSuchNode { uid })
}

/// Levels from the root down to `id`, root inclusive.
fn depth_of(graph: &TreeGraph, id: NodeId) -> usize {
    let mut depth = 1;
    let mut cur = id;
    while cur != NodeId::ROOT {
        cur = graph.parent(cur);
        depth += 1;
    }
    depth
}

/// Levels in the subtree rooted at `id`. Iterative: measuring the depth limit
/// by recursing to it would be its own bug.
fn height(graph: &TreeGraph, id: NodeId) -> usize {
    let mut best = 0;
    let mut stack = alloc::vec![(id, 1usize)];
    while let Some((node, depth)) = stack.pop() {
        best = best.max(depth);
        for &child in graph.children_of(node) {
            stack.push((child, depth + 1));
        }
    }
    best
}

fn child_index(graph: &TreeGraph, parent: NodeId, child: NodeId) -> u16 {
    graph
        .children_of(parent)
        .iter()
        .position(|c| *c == child)
        .unwrap_or(0) as u16
}

fn check_arity(
    graph: &TreeGraph,
    parent_id: NodeId,
    parent_uid: Uid,
    would_be: u16,
) -> Result<(), PatchError> {
    let manifest: &NodeManifest = graph.manifest(parent_id);
    if manifest.arity.accepts(would_be) {
        Ok(())
    } else {
        Err(PatchError::Arity {
            parent: parent_uid,
            type_name: manifest.type_name,
            expected: manifest.arity,
            would_be,
        })
    }
}

/// What a patch produced.
#[derive(Debug)]
pub struct Applied {
    /// The tree after the edit.
    pub graph: TreeGraph,
    /// The subtree that was taken out, with its live state intact.
    ///
    /// Keep it and you can undo: `inverse.validate(&graph, removed.as_ref())`.
    pub removed: Option<TreeGraph>,
    /// The patch that puts things back.
    pub inverse: TreePatch,
}

/// Apply a validated patch.
///
/// Cannot fail: [`TreePatch::validate`] already decided everything.
///
/// Surviving nodes are *moved*, so their behaviour and state come along
/// untouched — a `Running` node stays running, a `Sequence` keeps its cursor.
/// The removed subtree is moved out whole rather than dropped, so it can go
/// back.
#[must_use]
pub fn apply(graph: TreeGraph, incoming: Option<TreeGraph>, plan: PatchPlan) -> Applied {
    let version = TreeVersion(graph.version().0 + 1);
    let next_uid = graph.ids().map(|id| graph.uid(id).0).max().unwrap_or(0) + 1;
    let mut used: BTreeSet<u64> = graph.ids().map(|id| graph.uid(id).0).collect();

    let mut old = Scratch::of(graph);

    // The subtree that is leaving, carried out whole so it can come back.
    let removed = match plan.patch {
        TreePatch::InsertChild { .. } => None,
        _ => Some(extract(&mut old, plan.target)),
    };
    // Its uids leave with it, which is what frees them for a rollback to
    // reclaim — see `emit_incoming`.
    if let Some(rem) = &removed {
        for id in rem.ids() {
            used.remove(&rem.uid(id).0);
        }
    }

    let mut new_sub = incoming.map(Scratch::of);
    let mut out = Emit::new(next_uid, version, used);
    let root = emit(&mut out, &mut old, new_sub.as_mut(), NodeId::ROOT, &plan);
    debug_assert_eq!(root, NodeId::ROOT, "the root must stay at index 0");

    let inverse = match plan.patch {
        // Replacing again, this time targeting whatever the new subtree's root
        // uid turned out to be, puts the original back.
        TreePatch::Replace { .. } => TreePatch::Replace {
            target: out.first_incoming_uid.unwrap_or(Uid(0)),
        },
        TreePatch::InsertChild { parent, index } => TreePatch::RemoveChild { parent, index },
        TreePatch::RemoveChild { parent, index } => TreePatch::InsertChild { parent, index },
    };

    Applied {
        graph: out.finish(),
        removed,
        inverse,
    }
}

// ── the machinery ────────────────────────────────────────────────────────────

/// A graph taken apart so its nodes can be moved out one at a time.
struct Scratch {
    structure: Vec<NodeStruct>,
    meta: Vec<NodeMeta>,
    children: Vec<NodeId>,
    ports: Vec<Binding>,
    behavior: Vec<NodeKind>,
    state: Vec<NodeState>,
    names: Vec<Box<str>>,
}

impl Scratch {
    fn of(graph: TreeGraph) -> Self {
        let (structure, meta, children, ports, behavior, state, names) = graph.into_parts();
        Self {
            structure,
            meta,
            children,
            ports,
            behavior,
            state,
            names,
        }
    }

    fn take_behavior(&mut self, id: NodeId) -> NodeKind {
        core::mem::replace(&mut self.behavior[id.index()], NodeKind::Vacant)
    }

    fn children_of(&self, id: NodeId) -> &[NodeId] {
        let s = &self.structure[id.index()];
        let start = s.first_child as usize;
        &self.children[start..start + s.child_count as usize]
    }

    fn bindings_of(&self, id: NodeId) -> &[Binding] {
        let s = &self.structure[id.index()];
        let start = s.first_port as usize;
        &self.ports[start..start + s.port_count as usize]
    }

    fn name_of(&self, id: NodeId) -> &str {
        &self.names[self.meta[id.index()].name.index()]
    }
}

/// The graph being built up.
struct Emit {
    structure: Vec<NodeStruct>,
    meta: Vec<NodeMeta>,
    children: Vec<NodeId>,
    ports: Vec<Binding>,
    behavior: Vec<NodeKind>,
    state: Vec<NodeState>,
    names: Vec<Box<str>>,
    next_uid: u64,
    version: TreeVersion,
    /// Uids already spoken for in the graph being built.
    used: BTreeSet<u64>,
    /// The uid the incoming subtree's root ended up with — the inverse patch's
    /// target.
    first_incoming_uid: Option<Uid>,
}

impl Emit {
    fn new(next_uid: u64, version: TreeVersion, used: BTreeSet<u64>) -> Self {
        Self {
            structure: Vec::new(),
            meta: Vec::new(),
            children: Vec::new(),
            ports: Vec::new(),
            behavior: Vec::new(),
            state: Vec::new(),
            names: Vec::new(),
            next_uid,
            version,
            used,
            first_incoming_uid: None,
        }
    }

    /// Keep an incoming node's uid if it is free, else mint a fresh one.
    ///
    /// This one rule covers both directions. A subtree straight out of a
    /// `TreeBuilder` numbers from zero and collides with the live tree, so it
    /// gets fresh uids. A subtree that a previous patch *removed* took its uids
    /// with it, so they are free and a rollback reclaims them — which is what
    /// makes an undo restore the original identities, and keeps an earlier trace
    /// still lining up with the tree.
    fn uid_for(&mut self, wanted: u64) -> Uid {
        if self.used.insert(wanted) {
            return Uid(wanted);
        }
        while !self.used.insert(self.next_uid) {
            self.next_uid += 1;
        }
        let uid = Uid(self.next_uid);
        self.next_uid += 1;
        uid
    }

    fn intern(&mut self, name: &str) -> NameId {
        if let Some(i) = self.names.iter().position(|n| &**n == name) {
            return NameId(i as u32);
        }
        self.names.push(name.into());
        NameId((self.names.len() - 1) as u32)
    }

    fn finish(self) -> TreeGraph {
        TreeGraph::from_moved_parts(
            self.structure,
            self.meta,
            self.children,
            self.ports,
            self.behavior,
            self.state,
            self.names,
            self.version,
        )
    }
}

/// Copy `src`'s node at `id` into `out`, recursing, substituting per `plan`.
fn emit(
    out: &mut Emit,
    old: &mut Scratch,
    mut incoming: Option<&mut Scratch>,
    id: NodeId,
    plan: &PatchPlan,
) -> NodeId {
    // Reserve this node's slot before its children, so the root lands at 0 and
    // parents keep coming before children.
    let me = NodeId(out.structure.len() as u32);
    out.structure.push(NodeStruct {
        first_child: 0,
        first_port: 0,
        child_count: 0,
        port_count: 0,
    });
    out.meta.push(NodeMeta {
        uid: old.meta[id.index()].uid,
        name: NameId(0),
        parent: me,
        manifest: old.meta[id.index()].manifest,
    });
    out.behavior.push(NodeKind::Vacant);
    out.state.push(NodeState::Idle);

    let name = old.name_of(id).to_string();
    let name_id = out.intern(&name);
    out.meta[me.index()].name = name_id;
    out.behavior[me.index()] = old.take_behavior(id);
    out.state[me.index()] = old.state[id.index()];

    let first_port = out.ports.len() as u32;
    let bindings: Vec<Binding> = old.bindings_of(id).to_vec();
    out.ports.extend_from_slice(&bindings);
    out.structure[me.index()].first_port = first_port;
    out.structure[me.index()].port_count = bindings.len() as u16;

    // Children, with the substitution applied where it belongs.
    let old_children: Vec<NodeId> = old.children_of(id).to_vec();
    let mut new_children: Vec<NodeId> = Vec::with_capacity(old_children.len() + 1);

    let inserting_here = matches!(plan.patch, TreePatch::InsertChild { .. }) && id == plan.parent;

    for (i, child) in old_children.iter().enumerate() {
        if inserting_here && i == plan.index as usize {
            let sub = incoming.as_mut().expect("validated");
            new_children.push(emit_incoming(out, sub, NodeId::ROOT, me));
        }
        match plan.patch {
            TreePatch::Replace { .. } if *child == plan.target => {
                let sub = incoming.as_mut().expect("validated");
                new_children.push(emit_incoming(out, sub, NodeId::ROOT, me));
            }
            TreePatch::RemoveChild { .. } if *child == plan.target => {}
            _ => {
                let c = emit(out, old, incoming.as_deref_mut(), *child, plan);
                out.meta[c.index()].parent = me;
                new_children.push(c);
            }
        }
    }
    if inserting_here && plan.index as usize >= old_children.len() {
        let sub = incoming.as_mut().expect("validated");
        new_children.push(emit_incoming(out, sub, NodeId::ROOT, me));
    }

    let first_child = out.children.len() as u32;
    out.children.extend_from_slice(&new_children);
    out.structure[me.index()].first_child = first_child;
    out.structure[me.index()].child_count = new_children.len() as u16;
    me
}

/// Copy a node of the incoming subtree, giving it a uid that cannot collide.
fn emit_incoming(out: &mut Emit, sub: &mut Scratch, id: NodeId, parent: NodeId) -> NodeId {
    let me = NodeId(out.structure.len() as u32);
    let uid = out.uid_for(sub.meta[id.index()].uid.0);
    if id == NodeId::ROOT && out.first_incoming_uid.is_none() {
        out.first_incoming_uid = Some(uid);
    }

    out.structure.push(NodeStruct {
        first_child: 0,
        first_port: 0,
        child_count: 0,
        port_count: 0,
    });
    out.meta.push(NodeMeta {
        uid,
        name: NameId(0),
        parent,
        manifest: sub.meta[id.index()].manifest,
    });
    out.behavior.push(NodeKind::Vacant);
    out.state.push(NodeState::Idle);

    let name = sub.name_of(id).to_string();
    let name_id = out.intern(&name);
    out.meta[me.index()].name = name_id;
    out.behavior[me.index()] = sub.take_behavior(id);
    out.state[me.index()] = sub.state[id.index()];

    let first_port = out.ports.len() as u32;
    let bindings: Vec<Binding> = sub.bindings_of(id).to_vec();
    out.ports.extend_from_slice(&bindings);
    out.structure[me.index()].first_port = first_port;
    out.structure[me.index()].port_count = bindings.len() as u16;

    let kids: Vec<NodeId> = sub.children_of(id).to_vec();
    let mut new_children = Vec::with_capacity(kids.len());
    for child in kids {
        new_children.push(emit_incoming(out, sub, child, me));
    }
    let first_child = out.children.len() as u32;
    out.children.extend_from_slice(&new_children);
    out.structure[me.index()].first_child = first_child;
    out.structure[me.index()].child_count = new_children.len() as u16;
    me
}

/// Lift a subtree out of a scratch graph into a graph of its own.
fn extract(old: &mut Scratch, root: NodeId) -> TreeGraph {
    // Version 0: a detached subtree has no edit history of its own.
    let mut out = Emit::new(0, TreeVersion(0), BTreeSet::new());
    extract_into(&mut out, old, root, NodeId::ROOT);
    out.finish()
}

fn extract_into(out: &mut Emit, old: &mut Scratch, id: NodeId, parent: NodeId) -> NodeId {
    let me = NodeId(out.structure.len() as u32);
    out.structure.push(NodeStruct {
        first_child: 0,
        first_port: 0,
        child_count: 0,
        port_count: 0,
    });
    out.meta.push(NodeMeta {
        // Uids are kept, so a re-insert restores the original identities and any
        // recorded trace still lines up.
        uid: old.meta[id.index()].uid,
        name: NameId(0),
        parent,
        manifest: old.meta[id.index()].manifest,
    });
    out.behavior.push(NodeKind::Vacant);
    out.state.push(NodeState::Idle);

    let name = old.name_of(id).to_string();
    let name_id = out.intern(&name);
    out.meta[me.index()].name = name_id;
    out.behavior[me.index()] = old.take_behavior(id);
    out.state[me.index()] = old.state[id.index()];

    let first_port = out.ports.len() as u32;
    let bindings: Vec<Binding> = old.bindings_of(id).to_vec();
    out.ports.extend_from_slice(&bindings);
    out.structure[me.index()].first_port = first_port;
    out.structure[me.index()].port_count = bindings.len() as u16;

    let kids: Vec<NodeId> = old.children_of(id).to_vec();
    let mut new_children = Vec::with_capacity(kids.len());
    for child in kids {
        new_children.push(extract_into(out, old, child, me));
    }
    let first_child = out.children.len() as u32;
    out.children.extend_from_slice(&new_children);
    out.structure[me.index()].first_child = first_child;
    out.structure[me.index()].child_count = new_children.len() as u16;
    me
}
