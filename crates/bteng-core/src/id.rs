//! Index newtypes.
//!
//! All of them are `u32`/`u64` on purpose: they are copied constantly on the
//! tick path, they must stay small inside [`crate::NodeStruct`], and they must
//! survive serialization for patches and telemetry without pointer fixups.

/// Index of a node within a [`crate::TreeGraph`]'s arena.
///
/// Positional, not stable across structural edits — use [`Uid`] when identity
/// has to survive a reconfiguration.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(pub u32);

impl NodeId {
    /// The root is always arena index 0. The builder guarantees it.
    pub const ROOT: NodeId = NodeId(0);

    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// Index of a blackboard slot. Resolved from a port name once, at build time.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SlotId(pub u32);

impl SlotId {
    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// Stable node identity, assigned at build time and preserved across patches.
///
/// This is what a future reconfiguration matches old nodes to new ones by, so
/// that a `Running` node whose position changed keeps running instead of being
/// restarted. Nothing in M0 uses it yet; it costs 8 bytes and cannot be
/// retrofitted onto trees that were already built and traced.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Uid(pub u64);

/// Index into the graph's interned name table.
///
/// Names are for humans, tooling and telemetry — never for the tick path, which
/// is why they are not `String` fields on the node.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NameId(pub u32);

impl NameId {
    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}
