//! The two hooks the outside world gets into a tick.
//!
//! Both are `core`-only traits with no allocation and no `String`, because they
//! sit on the tick path and everything on the tick path has to survive DR-4.
//! Anything richer — formatting, buffering, JSON — belongs in the crate that
//! implements them.

use crate::clock::Nanos;
use crate::id::{NodeId, Uid};
use crate::status::NodeState;

/// Something that wants the scheduler to stop waiting.
///
/// Deliberately payload-free. An async task finishing, an external event, a
/// blackboard write from another thread — all the scheduler needs to know is
/// "look again"; *what* changed is answered by comparing slot versions, which
/// is cheaper and cannot go stale.
///
/// Lives in core so `bteng-async` and `bteng-sched` can meet here without
/// depending on each other.
pub trait Waker: Send + Sync {
    fn wake(&self);
}

/// A no-op waker, for a tree with nothing to wake for.
#[derive(Copy, Clone, Debug, Default)]
pub struct NoWaker;

impl Waker for NoWaker {
    fn wake(&self) {}
}

/// One node changing state.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct NodeEvent {
    pub node: NodeId,
    /// Stable identity — survives a structural edit, unlike [`NodeId`].
    pub uid: Uid,
    pub from: NodeState,
    pub to: NodeState,
    pub at: Nanos,
    pub tick: u64,
}

/// Where tick events go.
///
/// Only *transitions* are reported, not every tick: a tree at 100 Hz whose
/// state is unchanged produces no events, which is the difference between a
/// trace you can keep and one you throw away after ten seconds.
pub trait TraceSink {
    fn node_event(&mut self, event: NodeEvent);
}
