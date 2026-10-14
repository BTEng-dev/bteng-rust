//! The built-in node library.
//!
//! Semantics follow BehaviorTree.CPP (DR-8), including `Parallel` interleaving
//! its children on one thread rather than spawning any: real parallelism in this
//! engine lives at region boundaries (M2), never inside a control node. That
//! keeps `<Parallel>` deterministic, replayable, and portable to and from other
//! BT tooling.
//!
//! Two deliberate deviations, both documented at their node:
//!
//! * `Repeat` and `RetryUntilSuccessful` complete **at most one child cycle per
//!   tick**. BT.CPP loops inside a single tick, which makes the cost of one tick
//!   depend on how fast the child settles — unbounded work in a tick is exactly
//!   what DR-4 keeps off the table.
//! * `RunOnce` has no `Skipped` status to return, so `then_skip="false"` re-runs
//!   the child instead.
//!
//! Reactive control nodes are here and behave exactly as BT.CPP specifies:
//! re-tick every child from the first, every tick. What M4 adds is the
//! *optimisation* — skipping children whose inputs did not change — which needs
//! read sets to do precisely. Python BTEng approximated it with a single global
//! dirty flag; shipping that under a better name is what M0 declined to do, not
//! the semantics themselves.

mod control;
mod decorator;
mod leaf;

use core::fmt;

use crate::clock::Nanos;
use crate::manifest::{Arity, NodeCategory, NodeManifest};
use crate::node::{Ctx, Cursor, NodeKind};
use crate::port::{PortSpec, input};
use crate::status::Status;

pub use leaf::{ActionFn, ConditionFn};

/// A `Parallel` tracks child completion in a `u64` bitset.
///
/// 64 children under one `Parallel` is far past anything sane, and the builder
/// rejects more rather than letting the bitset silently lose track.
pub const MAX_PARALLEL_CHILDREN: u16 = 64;

// ── port declarations ────────────────────────────────────────────────────────

static NO_PORTS: &[PortSpec] = &[];

static PARALLEL_PORTS: &[PortSpec] = &[
    input::<i32>("success_count")
        .with_default("-1")
        .described("children that must succeed; -1 means all of them"),
    input::<i32>("failure_count")
        .with_default("1")
        .described("children that must fail; -1 means all of them"),
];

static REPEAT_PORTS: &[PortSpec] = &[input::<i32>("num_cycles")
    .with_default("1")
    .described("successful child cycles required; -1 repeats forever")];

static RETRY_PORTS: &[PortSpec] = &[input::<i32>("num_attempts")
    .with_default("1")
    .described("child failures tolerated; -1 retries forever")];

static RUN_ONCE_PORTS: &[PortSpec] = &[input::<bool>("then_skip")
    .with_default("true")
    .described("after the first run, return the cached status instead of re-running")];

static TIMEOUT_PORTS: &[PortSpec] =
    &[input::<u64>("msec").described("fail if the child is still running after this long")];

static DELAY_PORTS: &[PortSpec] =
    &[input::<u64>("delay_msec").described("wait this long before first ticking the child")];

static SLEEP_PORTS: &[PortSpec] =
    &[input::<u64>("msec").described("report Running for this long, then Success")];

static REGION_PORTS: &[PortSpec] = &[
    input::<f64>("hz")
        .with_default("10.0")
        .described("how often the scheduler ticks this region's subtree"),
    input::<alloc::string::String>("overrun")
        .with_default("skip")
        .described("skip | warn | catch_up | abort — what to do about a missed deadline"),
    input::<alloc::string::String>("mode")
        .with_default("periodic")
        .described("periodic | on_change — whether hz is a rate or a heartbeat"),
    input::<u64>("min_period_ms")
        .with_default("0")
        .described("in on_change mode, the shortest gap between two ticks (debounce)"),
];

/// Index of `Region`'s `hz` port. The scheduler reads it directly from the slot.
pub const REGION_PORT_HZ: u16 = 0;
/// Index of `Region`'s `overrun` port.
pub const REGION_PORT_OVERRUN: u16 = 1;
/// Index of `Region`'s `mode` port.
pub const REGION_PORT_MODE: u16 = 2;
/// Index of `Region`'s `min_period_ms` port.
pub const REGION_PORT_MIN_PERIOD: u16 = 3;

/// Port indices. Positions in the manifests above — the tick path uses these
/// rather than looking a port up by name.
pub(crate) mod ports {
    pub const PARALLEL_SUCCESS_COUNT: u16 = 0;
    pub const PARALLEL_FAILURE_COUNT: u16 = 1;
    pub const REPEAT_NUM_CYCLES: u16 = 0;
    pub const RETRY_NUM_ATTEMPTS: u16 = 0;
    pub const RUN_ONCE_THEN_SKIP: u16 = 0;
    pub const TIMEOUT_MSEC: u16 = 0;
    pub const DELAY_MSEC: u16 = 0;
    pub const SLEEP_MSEC: u16 = 0;
}

// ── manifests ────────────────────────────────────────────────────────────────

macro_rules! manifest {
    ($ident:ident, $name:literal, $cat:expr, $arity:expr, $ports:expr, $doc:literal) => {
        static $ident: NodeManifest =
            NodeManifest::new($name, $cat, $arity, $ports).described($doc);
    };
}

use NodeCategory::{Action, Condition, Control, Decorator, SubTree as SubTreeCat};

manifest!(
    M_SEQUENCE,
    "Sequence",
    Control,
    Arity::AtLeastOne,
    NO_PORTS,
    "Children left to right; remembers where it was. Stops at the first Failure or Running."
);
manifest!(
    M_SEQUENCE_MEM,
    "SequenceWithMemory",
    Control,
    Arity::AtLeastOne,
    NO_PORTS,
    "Like Sequence, but a Failure does not rewind — the next activation retries the failed child."
);
manifest!(
    M_REACTIVE_SEQUENCE,
    "ReactiveSequence",
    Control,
    Arity::AtLeastOne,
    NO_PORTS,
    "Re-ticks every child from the first, every tick. Earlier conditions can interrupt later work."
);
manifest!(
    M_FALLBACK,
    "Fallback",
    Control,
    Arity::AtLeastOne,
    NO_PORTS,
    "Children left to right; remembers where it was. Stops at the first Success or Running."
);
manifest!(
    M_REACTIVE_FALLBACK,
    "ReactiveFallback",
    Control,
    Arity::AtLeastOne,
    NO_PORTS,
    "Re-ticks every child from the first, every tick. A higher-priority child can preempt."
);
manifest!(
    M_PARALLEL,
    "Parallel",
    Control,
    Arity::AtLeastOne,
    PARALLEL_PORTS,
    "Ticks all unsettled children each tick, interleaved on this thread, with thresholds."
);
manifest!(
    M_IF_THEN_ELSE,
    "IfThenElse",
    Control,
    Arity::Between(2, 3),
    NO_PORTS,
    "children[0] chooses: Success runs [1], Failure runs [2] if present. The choice sticks while running."
);
manifest!(
    M_WHILE_DO_ELSE,
    "WhileDoElse",
    Control,
    Arity::Between(2, 3),
    NO_PORTS,
    "Like IfThenElse but re-evaluates the condition every tick and switches branch if it flips."
);

manifest!(
    M_INVERTER,
    "Inverter",
    Decorator,
    Arity::One,
    NO_PORTS,
    "Swaps Success and Failure; passes Running through."
);
manifest!(
    M_FORCE_SUCCESS,
    "ForceSuccess",
    Decorator,
    Arity::One,
    NO_PORTS,
    "Success unless the child is Running."
);
manifest!(
    M_FORCE_FAILURE,
    "ForceFailure",
    Decorator,
    Arity::One,
    NO_PORTS,
    "Failure unless the child is Running."
);
manifest!(
    M_KEEP_RUNNING,
    "KeepRunningUntilFailure",
    Decorator,
    Arity::One,
    NO_PORTS,
    "Running until the child fails."
);
manifest!(
    M_REPEAT,
    "Repeat",
    Decorator,
    Arity::One,
    REPEAT_PORTS,
    "Repeats the child until it has succeeded num_cycles times. One cycle per tick."
);
manifest!(
    M_RETRY,
    "RetryUntilSuccessful",
    Decorator,
    Arity::One,
    RETRY_PORTS,
    "Retries the child after a Failure, up to num_attempts times. One attempt per tick."
);
manifest!(
    M_RUN_ONCE,
    "RunOnce",
    Decorator,
    Arity::One,
    RUN_ONCE_PORTS,
    "Runs the child to completion once, then replays the cached status."
);
manifest!(
    M_TIMEOUT,
    "Timeout",
    Decorator,
    Arity::One,
    TIMEOUT_PORTS,
    "Fails and halts the child if it is still Running after msec."
);
manifest!(
    M_DELAY,
    "Delay",
    Decorator,
    Arity::One,
    DELAY_PORTS,
    "Waits delay_msec before first ticking the child."
);
manifest!(
    M_SUBTREE,
    "SubTree",
    SubTreeCat,
    Arity::One,
    NO_PORTS,
    "A marker around an inlined subtree. Transparent: passes the child's status through."
);
manifest!(
    M_REGION,
    "Region",
    Decorator,
    Arity::One,
    REGION_PORTS,
    "A scheduling boundary. Its subtree runs on its own clock; the parent reads the last verdict."
);

manifest!(
    M_ALWAYS_SUCCESS,
    "AlwaysSuccess",
    Action,
    Arity::Leaf,
    NO_PORTS,
    "Returns Success."
);
manifest!(
    M_ALWAYS_FAILURE,
    "AlwaysFailure",
    Action,
    Arity::Leaf,
    NO_PORTS,
    "Returns Failure."
);
manifest!(
    M_SLEEP,
    "Sleep",
    Action,
    Arity::Leaf,
    SLEEP_PORTS,
    "Running for msec, then Success. Does not block the tick thread."
);
manifest!(
    M_ACTION,
    "Action",
    Action,
    Arity::Leaf,
    NO_PORTS,
    "A Rust function used as a leaf. Not constructible from XML."
);
manifest!(
    M_CONDITION,
    "Condition",
    Condition,
    Arity::Leaf,
    NO_PORTS,
    "A Rust predicate used as a leaf. Not constructible from XML."
);

/// A node type a tree source can name: its declaration and a constructor.
pub(crate) type ConstructibleNode = (&'static NodeManifest, fn() -> NodeKind);

/// Every built-in that can be named in a tree source, with its manifest and a
/// constructor. `Action`/`Condition` are absent on purpose: they wrap a Rust
/// function pointer, which no XML document can supply.
///
/// The constructor yields a [`NodeKind`] rather than a `Builtin` so the registry
/// can hold built-ins and user nodes in one table, with one signature.
pub(crate) static CONSTRUCTIBLE: &[ConstructibleNode] = &[
    (&M_SEQUENCE, || NodeKind::Builtin(Builtin::sequence())),
    (&M_SEQUENCE_MEM, || {
        NodeKind::Builtin(Builtin::sequence_with_memory())
    }),
    (&M_REACTIVE_SEQUENCE, || {
        NodeKind::Builtin(Builtin::reactive_sequence())
    }),
    (&M_FALLBACK, || NodeKind::Builtin(Builtin::fallback())),
    (&M_REACTIVE_FALLBACK, || {
        NodeKind::Builtin(Builtin::reactive_fallback())
    }),
    (&M_PARALLEL, || NodeKind::Builtin(Builtin::parallel())),
    (&M_IF_THEN_ELSE, || {
        NodeKind::Builtin(Builtin::if_then_else())
    }),
    (&M_WHILE_DO_ELSE, || {
        NodeKind::Builtin(Builtin::while_do_else())
    }),
    (&M_INVERTER, || NodeKind::Builtin(Builtin::Inverter)),
    (&M_FORCE_SUCCESS, || {
        NodeKind::Builtin(Builtin::ForceSuccess)
    }),
    (&M_FORCE_FAILURE, || {
        NodeKind::Builtin(Builtin::ForceFailure)
    }),
    (&M_KEEP_RUNNING, || {
        NodeKind::Builtin(Builtin::KeepRunningUntilFailure)
    }),
    (&M_REPEAT, || NodeKind::Builtin(Builtin::repeat())),
    (&M_RETRY, || NodeKind::Builtin(Builtin::retry())),
    (&M_RUN_ONCE, || NodeKind::Builtin(Builtin::run_once())),
    (&M_TIMEOUT, || NodeKind::Builtin(Builtin::timeout())),
    (&M_DELAY, || NodeKind::Builtin(Builtin::delay())),
    (&M_SUBTREE, || NodeKind::Builtin(Builtin::SubTree)),
    (&M_REGION, || NodeKind::Builtin(Builtin::region())),
    (&M_ALWAYS_SUCCESS, || {
        NodeKind::Builtin(Builtin::AlwaysSuccess)
    }),
    (&M_ALWAYS_FAILURE, || {
        NodeKind::Builtin(Builtin::AlwaysFailure)
    }),
    (&M_SLEEP, || NodeKind::Builtin(Builtin::sleep())),
];

/// Names accepted as synonyms of a built-in, for compatibility with trees
/// written against BehaviorTree.CPP.
pub(crate) static ALIASES: &[(&str, &str)] = &[
    ("Selector", "Fallback"),
    ("ReactiveSelector", "ReactiveFallback"),
    ("SequenceStar", "SequenceWithMemory"),
    ("RetryNode", "RetryUntilSuccessful"),
];

// ── the enum ─────────────────────────────────────────────────────────────────

/// The built-in node kinds, dispatched by `match` rather than through a vtable.
pub enum Builtin {
    Sequence {
        cursor: u16,
    },
    SequenceWithMemory {
        cursor: u16,
    },
    ReactiveSequence,
    Fallback {
        cursor: u16,
    },
    ReactiveFallback,
    Parallel {
        settled: u64,
        succeeded: u64,
    },
    IfThenElse {
        branch: u8,
    },
    WhileDoElse {
        branch: u8,
    },

    Inverter,
    ForceSuccess,
    ForceFailure,
    KeepRunningUntilFailure,
    Repeat {
        completed: u32,
    },
    RetryUntilSuccessful {
        failures: u32,
    },
    RunOnce {
        done: bool,
        cached: Status,
    },
    Timeout {
        deadline: Nanos,
        armed: bool,
    },
    Delay {
        fire_at: Nanos,
        armed: bool,
    },
    SubTree,
    /// A scheduling boundary — see [`crate::Builtin::region`].
    Region {
        cached: Status,
    },

    AlwaysSuccess,
    AlwaysFailure,
    Sleep {
        fire_at: Nanos,
        armed: bool,
    },
    Action(ActionFn),
    Condition(ConditionFn),
}

impl fmt::Debug for Builtin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.type_name())
    }
}

impl Builtin {
    /// This node's declaration.
    #[must_use]
    pub fn manifest(&self) -> &'static NodeManifest {
        match self {
            Builtin::Sequence { .. } => &M_SEQUENCE,
            Builtin::SequenceWithMemory { .. } => &M_SEQUENCE_MEM,
            Builtin::ReactiveSequence => &M_REACTIVE_SEQUENCE,
            Builtin::Fallback { .. } => &M_FALLBACK,
            Builtin::ReactiveFallback => &M_REACTIVE_FALLBACK,
            Builtin::Parallel { .. } => &M_PARALLEL,
            Builtin::IfThenElse { .. } => &M_IF_THEN_ELSE,
            Builtin::WhileDoElse { .. } => &M_WHILE_DO_ELSE,
            Builtin::Inverter => &M_INVERTER,
            Builtin::ForceSuccess => &M_FORCE_SUCCESS,
            Builtin::ForceFailure => &M_FORCE_FAILURE,
            Builtin::KeepRunningUntilFailure => &M_KEEP_RUNNING,
            Builtin::Repeat { .. } => &M_REPEAT,
            Builtin::RetryUntilSuccessful { .. } => &M_RETRY,
            Builtin::RunOnce { .. } => &M_RUN_ONCE,
            Builtin::Timeout { .. } => &M_TIMEOUT,
            Builtin::Delay { .. } => &M_DELAY,
            Builtin::SubTree => &M_SUBTREE,
            Builtin::Region { .. } => &M_REGION,
            Builtin::AlwaysSuccess => &M_ALWAYS_SUCCESS,
            Builtin::AlwaysFailure => &M_ALWAYS_FAILURE,
            Builtin::Sleep { .. } => &M_SLEEP,
            Builtin::Action(_) => &M_ACTION,
            Builtin::Condition(_) => &M_CONDITION,
        }
    }

    #[must_use]
    pub fn type_name(&self) -> &'static str {
        self.manifest().type_name
    }

    #[must_use]
    pub fn arity(&self) -> Arity {
        self.manifest().arity
    }

    /// Construct a built-in by its type name, honouring [`ALIASES`].
    #[must_use]
    pub fn from_type_name(name: &str) -> Option<Builtin> {
        let canonical = ALIASES
            .iter()
            .find(|(alias, _)| *alias == name)
            .map_or(name, |(_, target)| *target);
        CONSTRUCTIBLE
            .iter()
            .find(|(m, _)| m.type_name == canonical)
            .and_then(|(_, ctor)| match ctor() {
                NodeKind::Builtin(b) => Some(b),
                _ => None,
            })
    }

    // ── constructors ─────────────────────────────────────────────────────────

    #[must_use]
    pub const fn sequence() -> Self {
        Builtin::Sequence { cursor: 0 }
    }
    #[must_use]
    pub const fn sequence_with_memory() -> Self {
        Builtin::SequenceWithMemory { cursor: 0 }
    }
    #[must_use]
    pub const fn reactive_sequence() -> Self {
        Builtin::ReactiveSequence
    }
    #[must_use]
    pub const fn fallback() -> Self {
        Builtin::Fallback { cursor: 0 }
    }
    #[must_use]
    pub const fn reactive_fallback() -> Self {
        Builtin::ReactiveFallback
    }
    /// Thresholds come from the `success_count`/`failure_count` ports.
    #[must_use]
    pub const fn parallel() -> Self {
        Builtin::Parallel {
            settled: 0,
            succeeded: 0,
        }
    }
    #[must_use]
    pub const fn if_then_else() -> Self {
        Builtin::IfThenElse { branch: 0 }
    }
    #[must_use]
    pub const fn while_do_else() -> Self {
        Builtin::WhileDoElse { branch: 0 }
    }
    #[must_use]
    pub const fn repeat() -> Self {
        Builtin::Repeat { completed: 0 }
    }
    #[must_use]
    pub const fn retry() -> Self {
        Builtin::RetryUntilSuccessful { failures: 0 }
    }
    #[must_use]
    pub const fn run_once() -> Self {
        Builtin::RunOnce {
            done: false,
            cached: Status::Success,
        }
    }
    #[must_use]
    pub const fn timeout() -> Self {
        Builtin::Timeout {
            deadline: 0,
            armed: false,
        }
    }
    #[must_use]
    pub const fn delay() -> Self {
        Builtin::Delay {
            fire_at: 0,
            armed: false,
        }
    }
    /// A scheduling boundary whose subtree runs on its own clock.
    ///
    /// The status starts as `Running` — "no verdict yet" — because a region that
    /// has not been scheduled yet has not decided anything, and `Success` is a
    /// decision a parent would act on.
    #[must_use]
    pub const fn region() -> Self {
        Builtin::Region {
            cached: Status::Running,
        }
    }

    #[must_use]
    pub const fn sleep() -> Self {
        Builtin::Sleep {
            fire_at: 0,
            armed: false,
        }
    }
    #[must_use]
    pub const fn action(f: ActionFn) -> Self {
        Builtin::Action(f)
    }
    #[must_use]
    pub const fn condition(f: ConditionFn) -> Self {
        Builtin::Condition(f)
    }

    // ── execution ────────────────────────────────────────────────────────────

    pub(crate) fn tick(&mut self, cur: &mut Cursor<'_, '_>) -> Status {
        match self {
            Builtin::Sequence { cursor } => control::sequence(cursor, cur),
            Builtin::SequenceWithMemory { cursor } => control::sequence_with_memory(cursor, cur),
            Builtin::ReactiveSequence => control::reactive_sequence(cur),
            Builtin::Fallback { cursor } => control::fallback(cursor, cur),
            Builtin::ReactiveFallback => control::reactive_fallback(cur),
            Builtin::Parallel { settled, succeeded } => control::parallel(settled, succeeded, cur),
            Builtin::IfThenElse { branch } => control::if_then_else(branch, cur),
            Builtin::WhileDoElse { branch } => control::while_do_else(branch, cur),

            Builtin::Inverter => cur.tick_child(0).inverted(),
            Builtin::ForceSuccess => decorator::force(cur, Status::Success),
            Builtin::ForceFailure => decorator::force(cur, Status::Failure),
            Builtin::KeepRunningUntilFailure => decorator::keep_running(cur),
            Builtin::Repeat { completed } => decorator::repeat(completed, cur),
            Builtin::RetryUntilSuccessful { failures } => decorator::retry(failures, cur),
            Builtin::RunOnce { done, cached } => decorator::run_once(done, cached, cur),
            Builtin::Timeout { deadline, armed } => decorator::timeout(deadline, armed, cur),
            Builtin::Delay { fire_at, armed } => decorator::delay(fire_at, armed, cur),
            // Deliberately does *not* tick its child: the scheduler does that,
            // on the region's own clock. Python BTEng's RateController ticks the
            // child inside the parent's tick, so the parent's rate silently caps
            // the child's — a 100 Hz subtree under a 10 Hz parent runs at 10 Hz.
            // Reading a published verdict here is what breaks that coupling.
            Builtin::SubTree => cur.tick_child(0),
            Builtin::Region { cached } => *cached,

            Builtin::AlwaysSuccess => Status::Success,
            Builtin::AlwaysFailure => Status::Failure,
            Builtin::Sleep { fire_at, armed } => leaf::sleep(fire_at, armed, cur),
            Builtin::Action(f) => f(cur.ctx()),
            Builtin::Condition(f) => {
                if f(cur.ctx()) {
                    Status::Success
                } else {
                    Status::Failure
                }
            }
        }
    }

    /// Clear this node's own progress.
    ///
    /// Children are halted by the engine; this only resets state that lives
    /// outside `NodeState`. Python BTEng shipped a bug of exactly this shape in
    /// `RateController` — cached progress that survived a reset and replayed a
    /// stale status without re-running the work — so every stateful built-in
    /// clears itself here.
    pub(crate) fn halt(&mut self, _cur: &mut Cursor<'_, '_>) {
        match self {
            Builtin::Sequence { cursor }
            | Builtin::SequenceWithMemory { cursor }
            | Builtin::Fallback { cursor } => *cursor = 0,
            Builtin::Parallel { settled, succeeded } => {
                *settled = 0;
                *succeeded = 0;
            }
            Builtin::IfThenElse { branch } | Builtin::WhileDoElse { branch } => *branch = 0,
            Builtin::Repeat { completed } => *completed = 0,
            Builtin::RetryUntilSuccessful { failures } => *failures = 0,
            Builtin::Timeout { armed, .. }
            | Builtin::Delay { armed, .. }
            | Builtin::Sleep { armed, .. } => *armed = false,
            // Back to "no verdict yet". A latched Success surviving a halt would
            // make the parent skip a region that never re-ran.
            Builtin::Region { cached } => *cached = Status::Running,
            // RunOnce deliberately keeps `done` across a halt — "once" would
            // mean nothing if a halt reset it.
            Builtin::RunOnce { .. }
            | Builtin::ReactiveSequence
            | Builtin::ReactiveFallback
            | Builtin::Inverter
            | Builtin::ForceSuccess
            | Builtin::ForceFailure
            | Builtin::KeepRunningUntilFailure
            | Builtin::SubTree
            | Builtin::AlwaysSuccess
            | Builtin::AlwaysFailure
            | Builtin::Action(_)
            | Builtin::Condition(_) => {}
        }
    }
}

/// Milliseconds to nanoseconds, saturating rather than wrapping.
pub(crate) fn msec_to_nanos(msec: u64) -> Nanos {
    msec.saturating_mul(1_000_000)
}

/// Read a count port where a negative value means "all children".
pub(crate) fn threshold(cur: &Cursor<'_, '_>, port: u16, fallback: i32, n: u16) -> u16 {
    let raw = cur.get_or::<i32>(port, fallback);
    if raw < 0 { n } else { (raw as u16).min(n) }
}

/// Shared by the leaf timers.
pub(crate) fn now_of(cur: &mut Cursor<'_, '_>) -> Nanos {
    let ctx: &mut Ctx<'_> = cur.ctx();
    ctx.now()
}
