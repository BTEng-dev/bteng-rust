//! Tick results and resting states — deliberately two different types.
//!
//! Python BTEng puts `IDLE` in the same enum as the tick results and then has
//! to reject it at runtime in every tick, because every control node misreads
//! it: a Sequence advances past it, a Parallel never completes on it, a
//! decorator propagates it upward. Splitting the types makes returning it from
//! `tick()` not a runtime error but an unrepresentable one.

/// What a `tick()` may return. There is no "idle" — a node that is being ticked
/// is, by definition, not resting.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Status {
    Running,
    Success,
    Failure,
}

impl Status {
    /// True for `Success`/`Failure` — the node has a final answer this tick.
    #[must_use]
    pub const fn is_settled(self) -> bool {
        !matches!(self, Status::Running)
    }

    #[must_use]
    pub const fn is_running(self) -> bool {
        matches!(self, Status::Running)
    }

    /// `Success` ↔ `Failure`, `Running` unchanged. Used by `Inverter`.
    #[must_use]
    pub const fn inverted(self) -> Status {
        match self {
            Status::Running => Status::Running,
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
        }
    }
}
