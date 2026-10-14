//! The bounded, allocation-free sink.

use bteng_core::{Blackboard, Nanos, NodeEvent, Status, TraceSink, TreeGraph};

use crate::event::{Event, Structure};

/// A region tick worth recording.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct RegionReport {
    pub at: Nanos,
    pub region: usize,
    pub status: Status,
    pub lateness_ns: Nanos,
}

/// Collects events into a preallocated buffer.
///
/// **Bounded on purpose.** Once the buffer is full the recorder counts drops
/// instead of growing, because a `Vec` doubling inside a 1 kHz region is exactly
/// the unbounded pause DR-4 exists to keep out. The drop count is emitted as an
/// [`Event::Dropped`], so a gap is a measurement rather than a silence.
#[derive(Debug)]
pub struct Recorder {
    events: Vec<Event>,
    capacity: usize,
    dropped: u64,
    last_at: Nanos,
}

impl Recorder {
    /// Room for `capacity` events. Nothing after construction allocates.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            events: Vec::with_capacity(capacity),
            capacity,
            dropped: 0,
            last_at: 0,
        }
    }

    /// Record the tree's shape. Call once, before the first tick.
    pub fn structure(&mut self, graph: &TreeGraph, bb: &Blackboard) {
        self.push(Event::Structure(Box::new(Structure::of(graph, bb))));
    }

    pub fn region(&mut self, report: RegionReport) {
        self.last_at = report.at;
        self.push(Event::Region {
            at: report.at,
            region: report.region,
            status: report.status,
            lateness_ns: report.lateness_ns,
        });
    }

    /// Record that the tree changed shape.
    ///
    /// Without this a trace silently describes the wrong tree from the patch
    /// onward — the one failure mode a recorded run must not have.
    pub fn patch(&mut self, at: Nanos, graph: &TreeGraph, bb: &Blackboard) {
        self.last_at = at;
        self.push(Event::Patch {
            at,
            version: graph.version().0,
            structure: Box::new(Structure::of(graph, bb)),
        });
    }

    pub fn run_end(&mut self, at: Nanos, status: Status) {
        self.push(Event::RunEnd { at, status });
    }

    fn push(&mut self, e: Event) {
        if self.events.len() < self.capacity {
            self.events.push(e);
        } else {
            self.dropped += 1;
        }
    }

    /// Everything recorded, with a trailing [`Event::Dropped`] if anything was.
    #[must_use]
    pub fn finish(mut self) -> Vec<Event> {
        if self.dropped > 0 {
            let at = self.last_at;
            let count = self.dropped;
            // Overwrite the last slot rather than growing: the gap matters more
            // than the event it displaces.
            if self.events.len() == self.capacity {
                self.events.pop();
            }
            self.events.push(Event::Dropped { at, count });
        }
        self.events
    }

    #[must_use]
    pub fn events(&self) -> &[Event] {
        &self.events
    }

    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.events.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

impl TraceSink for Recorder {
    fn node_event(&mut self, event: NodeEvent) {
        self.last_at = event.at;
        self.push(Event::Node {
            at: event.at,
            tick: event.tick,
            uid: event.uid.0,
            node: event.node.0,
            from: event.from,
            to: event.to,
        });
    }
}
