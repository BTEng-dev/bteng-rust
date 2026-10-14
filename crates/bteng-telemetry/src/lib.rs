//! The trace schema — what a running tree says about itself.
//!
//! This is the contract with BTviz (DR-7), so it is **versioned** and written
//! down rather than being whatever the engine happened to log. [`SCHEMA_VERSION`]
//! goes up when a consumer would have to change.
//!
//! # Shape
//!
//! A trace is a stream of [`Event`]s. The first is always [`Event::Structure`],
//! which carries the whole node table — uid, parent, name, type, category, port
//! bindings — so a viewer can draw the tree without parsing the XML or having
//! the registry. Everything after it refers to nodes by [`Uid`], which is stable
//! across a structural edit (M7) in a way `NodeId` is not.
//!
//! # Transitions, not ticks
//!
//! A node emits an event when its state *changes*, not when it is ticked. A tree
//! at 100 Hz whose state is unchanged produces nothing. That is the difference
//! between a trace you can keep for a whole run and one you throw away after ten
//! seconds — and it is why the interesting thing, `halt`, is reported too:
//! Python BTEng only hooked ticks, so a node stopped by a `Timeout` stayed
//! "running" in every live view forever.
//!
//! # On the tick path
//!
//! [`Recorder`] is bounded and allocation-free once constructed: it fills a
//! preallocated buffer and *counts* what it had to drop rather than growing.
//! Reporting a gap honestly beats a malloc in a 1 kHz region. [`to_jsonl`]
//! allocates freely — it runs offline, after.

mod event;
mod recorder;
mod timeline;

pub use event::{Event, NodeInfo, PortInfo, Structure, escape_json};
pub use recorder::{Recorder, RegionReport};
pub use timeline::Timeline;

/// Bumped when a consumer of the JSON would have to change.
///
/// * **3** — a port may carry `key` (the blackboard key a `key` binding names) and
///   `value` (the text of a `literal` or `default` binding). Additive, but a consumer
///   that wants to show ports has to read them.
/// * **2** — added `patch`, emitted when a tree changes shape at runtime.
/// * **1** — initial.
pub const SCHEMA_VERSION: u32 = 3;

/// Render a slice of events as JSON Lines — one object per line.
///
/// Line-delimited on purpose: a viewer can tail a growing file, and a truncated
/// trace from a process that died still parses up to its last complete line.
#[must_use]
pub fn to_jsonl(events: &[Event]) -> String {
    let mut out = String::new();
    for e in events {
        e.write_json(&mut out);
        out.push('\n');
    }
    out
}
