//! Reconstructing what the tree looked like at a moment.

use std::collections::BTreeMap;

use bteng_core::{Nanos, NodeState};

use crate::event::{Event, NodeInfo};

/// Replays a trace to answer "what was every node doing at time *t*?".
///
/// This is what makes the recording worth keeping: without it a trace is a log,
/// and with it a trace is a debugger. It works purely from the event stream —
/// no tree, no registry, no engine — which is the same position BTviz is in.
#[derive(Debug)]
pub struct Timeline<'a> {
    events: &'a [Event],
    nodes: BTreeMap<u64, &'a NodeInfo>,
}

impl<'a> Timeline<'a> {
    /// # Errors
    /// `None` when the stream has no [`Event::Structure`] — without the node
    /// table there is nothing to reconstruct against.
    #[must_use]
    pub fn new(events: &'a [Event]) -> Option<Self> {
        // The *last* structure wins: after a patch, that is the tree the later
        // events are describing.
        let structure = events.iter().rev().find_map(|e| match e {
            Event::Structure(s) | Event::Patch { structure: s, .. } => Some(s),
            _ => None,
        })?;
        let nodes = structure.nodes.iter().map(|n| (n.uid, n)).collect();
        Some(Self { events, nodes })
    }

    #[must_use]
    pub fn node(&self, uid: u64) -> Option<&'a NodeInfo> {
        self.nodes.get(&uid).copied()
    }

    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Every node's state as of `at`, nodes that never changed included as
    /// `Idle`.
    #[must_use]
    pub fn states_at(&self, at: Nanos) -> BTreeMap<u64, NodeState> {
        let mut states: BTreeMap<u64, NodeState> = self
            .nodes
            .keys()
            .map(|uid| (*uid, NodeState::Idle))
            .collect();
        for e in self.events {
            if let Event::Node { at: t, uid, to, .. } = e {
                if *t > at {
                    break;
                }
                states.insert(*uid, *to);
            }
        }
        states
    }

    /// The deepest node that was `Running` at `at` — "what was it *doing*".
    ///
    /// Answered from the node table's parent links, so it needs no tree.
    #[must_use]
    pub fn active_leaf_at(&self, at: Nanos) -> Option<&'a NodeInfo> {
        let states = self.states_at(at);
        self.nodes
            .values()
            .filter(|n| states.get(&n.uid) == Some(&NodeState::Running))
            .filter(|n| n.children.is_empty())
            .copied()
            .next()
    }

    /// Timestamps at which anything changed. The scrubber's tick marks.
    #[must_use]
    pub fn change_times(&self) -> Vec<Nanos> {
        let mut out: Vec<Nanos> = self
            .events
            .iter()
            .filter_map(|e| match e {
                Event::Node { at, .. } | Event::Region { at, .. } => Some(*at),
                _ => None,
            })
            .collect();
        out.dedup();
        out
    }

    /// How many events of each kind, for a quick sanity read on a trace.
    #[must_use]
    pub fn counts(&self) -> (usize, usize, u64) {
        let mut node = 0;
        let mut region = 0;
        let mut dropped = 0;
        for e in self.events {
            match e {
                Event::Node { .. } => node += 1,
                Event::Region { .. } => region += 1,
                Event::Dropped { count, .. } => dropped += count,
                _ => {}
            }
        }
        (node, region, dropped)
    }
}
