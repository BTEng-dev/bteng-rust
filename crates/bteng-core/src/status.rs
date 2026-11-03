//! Tick results and resting states — deliberately two different types.
//!
//! Python BTEng puts `IDLE` in the same enum as the tick results and then has
//! to reject it at runtime in every tick, because every control node misreads
//! it: a Sequence advances past it, a Parallel never completes on it, a
//! decorator propagates it upward. Splitting the types makes returning it from
//! `tick()` not a runtime error but an unrepresentable one.
//!
//! # `Skipped`
//!
//! Both types carry `Skipped`, added in the same place in each so the
//! `From<Status>` mapping stays one-to-one. A node is skipped when a
//! precondition (`_skipIf`, or `_while` turning false) stops it from running;
//! control nodes pass over a skipped child, counting it as neither success nor
//! failure. Only two things produce it: the precondition check in
//! [`crate::tick_node`], and a control node or decorator passing it up — plus
//! `RunOnce` after its one run, as BehaviorTree.CPP does. A tree that uses no
//! precondition and no `RunOnce` never sees it.

/// What a `tick()` may return. There is no "idle" — a node that is being ticked
/// is, by definition, not resting.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Status {
    Running,
    Success,
    Failure,
    /// A precondition stopped the node from running. Neither success nor
    /// failure: control nodes pass over it.
    Skipped,
}

impl Status {
    /// True for anything but `Running` — the node is done for this activation.
    ///
    /// Includes `Skipped`. Use [`Status::is_completed`] when the question is
    /// whether the node actually produced an outcome.
    #[must_use]
    pub const fn is_settled(self) -> bool {
        !matches!(self, Status::Running)
    }

    /// True for `Success`/`Failure` only — the node ran and has a verdict.
    #[must_use]
    pub const fn is_completed(self) -> bool {
        matches!(self, Status::Success | Status::Failure)
    }

    #[must_use]
    pub const fn is_skipped(self) -> bool {
        matches!(self, Status::Skipped)
    }

    #[must_use]
    pub const fn is_running(self) -> bool {
        matches!(self, Status::Running)
    }

    /// `Success` ↔ `Failure`; `Running` and `Skipped` unchanged. Used by
    /// `Inverter`: a child that did not run has no outcome to invert.
    #[must_use]
    pub const fn inverted(self) -> Status {
        match self {
            Status::Running => Status::Running,
            Status::Skipped => Status::Skipped,
            Status::Success => Status::Failure,
            Status::Failure => Status::Success,
        }
    }
}

/// The state a node rests in between ticks — a superset of [`Status`].
///
/// Stored per node in the arena. `halt()` only fires cleanup for `Running`
/// nodes, so this is load-bearing, not just introspection.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum NodeState {
    #[default]
    Idle,
    Running,
    Success,
    Failure,
    /// The last tick was decided `Skipped`. Not running, so `halt()` does not
    /// fire for it, and preconditions are evaluated again on the next tick.
    Skipped,
}

impl NodeState {
    #[must_use]
    pub const fn is_running(self) -> bool {
        matches!(self, NodeState::Running)
    }
}

impl From<Status> for NodeState {
    fn from(s: Status) -> Self {
        match s {
            Status::Running => NodeState::Running,
            Status::Success => NodeState::Success,
            Status::Failure => NodeState::Failure,
            Status::Skipped => NodeState::Skipped,
        }
    }
}
