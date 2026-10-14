//! The scheduler proper.

use std::fmt;

use bteng_core::{
    Blackboard, Ctx, Nanos, NodeId, NodeState, SlotId, Status, TraceSink, Tree, halt_node,
    tick_node,
};

use crate::pacer::Pacer;
use crate::stats::RegionStats;

/// The fastest rate a region may ask for.
///
/// 1 kHz is the working assumption for the timing class (DR-4 leaves it open);
/// it is a validation bound, not a design constant — nothing is sized by it.
pub const MAX_HZ: f64 = 1000.0;

/// How a region decides it is time to tick.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum TickPolicy {
    /// Every `period`, on a fixed grid.
    Periodic { period: Nanos },
    /// When something the region reads changes — with a floor on how often that
    /// may happen, and a heartbeat so timeouts and watchdogs still fire.
    ///
    /// The heartbeat is not optional in practice. A region that only ever wakes
    /// on data would never notice its own `Timeout` expiring.
    OnChange {
        /// Shortest gap between two ticks. Debounce: a 1 kHz sensor writing a
        /// slot must not tick a 10 Hz region a thousand times.
        min_period: Nanos,
        /// Tick at least this often even with nothing new.
        heartbeat: Nanos,
    },
}

impl TickPolicy {
    /// # Errors
    /// [`SchedError::BadRate`] for a non-finite, non-positive, or out-of-range rate.
    pub fn periodic_hz(hz: f64) -> Result<Self, SchedError> {
        Ok(TickPolicy::Periodic {
            period: period_from_hz(hz)?,
        })
    }

    /// # Errors
    /// [`SchedError::BadRate`] for a bad heartbeat rate.
    pub fn on_change(heartbeat_hz: f64, min_period: Nanos) -> Result<Self, SchedError> {
        Ok(TickPolicy::OnChange {
            min_period,
            heartbeat: period_from_hz(heartbeat_hz)?,
        })
    }

    /// The longest this policy will ever go without ticking.
    #[must_use]
    pub const fn heartbeat(self) -> Nanos {
        match self {
            TickPolicy::Periodic { period } => period,
            TickPolicy::OnChange { heartbeat, .. } => heartbeat,
        }
    }

    #[must_use]
    pub const fn is_event_driven(self) -> bool {
        matches!(self, TickPolicy::OnChange { .. })
    }
}

fn period_from_hz(hz: f64) -> Result<Nanos, SchedError> {
    if !hz.is_finite() || hz <= 0.0 || hz > MAX_HZ {
        return Err(SchedError::BadRate { hz });
    }
    Ok(((1e9 / hz).round() as u64).max(1))
}

/// What to do when the scheduler reaches a deadline late.
///
/// A real-time system has to state this. Python BTEng states none, which in
/// practice means unbounded catch-up: a slow tick silently becomes a burst.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum OverrunPolicy {
    /// Drop the missed ticks, re-align to the grid. Right for control — a stale
    /// command is worse than none.
    Skip,
    /// As [`OverrunPolicy::Skip`], counted separately so a run can be audited.
    Warn,
    /// Fire up to `max` missed ticks back-to-back, then re-align.
    CatchUp { max: u32 },
    /// Stop the run.
    Abort,
}

impl OverrunPolicy {
    /// Parse the `overrun` port.
    ///
    /// Unknown spellings are an error, not a default — a typo silently changing
    /// a real-time policy is exactly the class of failure this engine keeps
    /// refusing to ship.
    ///
    /// # Errors
    /// [`SchedError::BadOverrunPolicy`].
    pub fn parse(text: &str) -> Result<Self, SchedError> {
        match text.trim() {
            "skip" => Ok(OverrunPolicy::Skip),
            "warn" => Ok(OverrunPolicy::Warn),
            "abort" => Ok(OverrunPolicy::Abort),
            "catch_up" => Ok(OverrunPolicy::CatchUp { max: 4 }),
            other => {
                if let Some(n) = other.strip_prefix("catch_up:")
                    && let Ok(max) = n.trim().parse::<u32>()
                {
                    return Ok(OverrunPolicy::CatchUp { max });
                }
                Err(SchedError::BadOverrunPolicy {
                    text: other.to_string(),
                })
            }
        }
    }
}

/// Why a scheduler could not be built or could not continue.
#[derive(Clone, Debug, PartialEq)]
pub enum SchedError {
    BadRate {
        hz: f64,
    },
    BadOverrunPolicy {
        text: String,
    },
    /// An unrecognised `mode` port.
    BadMode {
        text: String,
    },
    /// A `Region` node whose ports could not be read.
    UnreadablePort {
        region: NodeId,
        port: &'static str,
    },
    /// A region under [`OverrunPolicy::Abort`] missed its deadline.
    DeadlineMissed {
        region: NodeId,
        late_by: Nanos,
        deadline: Nanos,
    },
}

impl fmt::Display for SchedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SchedError::BadRate { hz } => write!(f, "rate {hz} is not in (0, {MAX_HZ}]"),
            SchedError::BadOverrunPolicy { text } => write!(
                f,
                "overrun policy {text:?} is not one of skip | warn | catch_up[:N] | abort"
            ),
            SchedError::BadMode { text } => {
                write!(f, "region mode {text:?} is not one of periodic | on_change")
            }
            SchedError::UnreadablePort { region, port } => {
                write!(f, "region {region:?} has an unreadable {port} port")
            }
            SchedError::DeadlineMissed {
                region,
                late_by,
                deadline,
            } => write!(
                f,
                "region {region:?} missed deadline {deadline} ns by {late_by} ns"
            ),
        }
    }
}

impl std::error::Error for SchedError {}

/// One scheduled region.
struct RegionState {
    /// The boundary node, or `None` for the root region.
    boundary: Option<NodeId>,
    /// Where ticking starts — the boundary's child, or the tree root.
    entry: NodeId,
    policy: TickPolicy,
    overrun: OverrunPolicy,
    /// Next absolute deadline (or heartbeat). Never derived from "now".
    deadline: Nanos,
    /// When this region last ticked. Only the debounce reads it.
    last_tick: Nanos,
    /// Slots any node in this region reads, with the versions seen at the last
    /// tick. This is what "did anything change?" means, precisely, without a
    /// single callback.
    read_set: Vec<SlotId>,
    versions: Vec<u64>,
    stats: RegionStats,
}

impl fmt::Debug for RegionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegionState")
            .field("boundary", &self.boundary)
            .field("policy", &self.policy)
            .field("reads", &self.read_set.len())
            .finish_non_exhaustive()
    }
}

impl RegionState {
    /// Have any of the slots this region reads changed since its last tick?
    ///
    /// O(read set), no locks, no allocation. Python BTEng's equivalent is a
    /// callback on every blackboard write setting one global dirty flag, which
    /// makes *any* write invalidate *every* reactive node.
    fn is_dirty(&self, bb: &Blackboard) -> bool {
        self.read_set
            .iter()
            .zip(&self.versions)
            .any(|(slot, seen)| bb.version(*slot).is_some_and(|v| v != *seen))
    }

    fn snapshot_versions(&mut self, bb: &Blackboard) {
        for (i, slot) in self.read_set.iter().enumerate() {
            self.versions[i] = bb.version(*slot).unwrap_or(0);
        }
    }

    /// The earliest time this region could possibly want to run.
    fn wake_at(&self, dirty: bool) -> Nanos {
        match self.policy {
            TickPolicy::Periodic { .. } => self.deadline,
            TickPolicy::OnChange { min_period, .. } => {
                if dirty {
                    self.deadline.min(self.last_tick.saturating_add(min_period))
                } else {
                    self.deadline
                }
            }
        }
    }

    fn is_due(&self, now: Nanos, dirty: bool) -> bool {
        match self.policy {
            TickPolicy::Periodic { .. } => now >= self.deadline,
            TickPolicy::OnChange { min_period, .. } => {
                now >= self.deadline || (dirty && now >= self.last_tick.saturating_add(min_period))
            }
        }
    }
}

/// What one [`Scheduler::step`] did.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum StepOutcome {
    Ticked {
        region: usize,
        status: Status,
        lateness: Nanos,
    },
    /// A region's time came round but the tree is not asking it to run — its
    /// boundary is idle, or it has a verdict waiting to be read.
    Idle {
        region: usize,
    },
    /// Something woke the scheduler but nothing turned out to be due.
    Woken,
    Finished {
        status: Status,
    },
}

/// Why a run ended.
#[derive(Clone, Debug, PartialEq)]
pub enum RunOutcome {
    Settled { status: Status, at: Nanos },
    TimeUp { at: Nanos },
    StepsUp { at: Nanos },
    Aborted(SchedError),
}

/// Drives one tree's regions off a shared clock.
pub struct Scheduler {
    tree: Tree,
    regions: Vec<RegionState>,
    settled: Option<Status>,
    aborted: Option<SchedError>,
    /// Scratch for the dirty scan, allocated once so `step` never does.
    dirty_scratch: Vec<bool>,
}

impl fmt::Debug for Scheduler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Scheduler")
            .field("regions", &self.regions.len())
            .field("settled", &self.settled)
            .finish_non_exhaustive()
    }
}

impl Scheduler {
    /// Build a scheduler for `tree`, running its root at `root_hz`.
    ///
    /// Every `Region` node becomes a scheduled region, taking its rate, mode,
    /// debounce and overrun policy from its ports, and its read set from the
    /// input ports of everything in its subtree.
    ///
    /// # Errors
    /// See [`SchedError`].
    pub fn new(tree: Tree, bb: &Blackboard, root_hz: f64) -> Result<Self, SchedError> {
        let mut regions = vec![RegionState {
            boundary: None,
            entry: NodeId::ROOT,
            policy: TickPolicy::periodic_hz(root_hz)?,
            overrun: OverrunPolicy::Skip,
            deadline: 0,
            last_tick: 0,
            read_set: Vec::new(),
            versions: Vec::new(),
            stats: RegionStats::default(),
        }];

        for id in tree.graph().region_ids() {
            regions.push(build_region(tree.graph(), bb, id)?);
        }

        for r in &mut regions {
            r.read_set = read_set_of(tree.graph(), r.entry);
            r.versions = vec![0; r.read_set.len()];
            r.snapshot_versions(bb);
        }

        let dirty_scratch = vec![false; regions.len()];
        Ok(Self {
            tree,
            regions,
            settled: None,
            aborted: None,
            dirty_scratch,
        })
    }

    #[must_use]
    pub fn region_count(&self) -> usize {
        self.regions.len()
    }

    #[must_use]
    pub fn stats(&self, region: usize) -> Option<&RegionStats> {
        self.regions.get(region).map(|r| &r.stats)
    }

    #[must_use]
    pub fn boundary(&self, region: usize) -> Option<NodeId> {
        self.regions.get(region).and_then(|r| r.boundary)
    }

    #[must_use]
    pub fn policy(&self, region: usize) -> Option<TickPolicy> {
        self.regions.get(region).map(|r| r.policy)
    }

    /// The period, or the heartbeat for an event-driven region.
    #[must_use]
    pub fn period(&self, region: usize) -> Option<Nanos> {
        self.regions.get(region).map(|r| r.policy.heartbeat())
    }

    /// How many slots a region watches. Zero means nothing can wake it early.
    #[must_use]
    pub fn read_set_len(&self, region: usize) -> Option<usize> {
        self.regions.get(region).map(|r| r.read_set.len())
    }

    #[must_use]
    pub fn tree(&self) -> &Tree {
        &self.tree
    }

    #[must_use]
    pub fn tree_mut(&mut self) -> &mut Tree {
        &mut self.tree
    }

    #[must_use]
    pub fn settled(&self) -> Option<Status> {
        self.settled
    }

    #[must_use]
    pub fn abort_reason(&self) -> Option<&SchedError> {
        self.aborted.as_ref()
    }

    /// Set every region's first deadline relative to `now`.
    pub fn arm(&mut self, now: Nanos) {
        for r in &mut self.regions {
            r.deadline = now;
            r.last_tick = now;
        }
    }

    /// The earliest time any region could want to run.
    #[must_use]
    pub fn next_wake(&mut self, bb: &Blackboard) -> Option<Nanos> {
        self.refresh_dirty(bb);
        self.regions
            .iter()
            .enumerate()
            .map(|(i, r)| r.wake_at(self.dirty_scratch[i]))
            .min()
    }

    fn refresh_dirty(&mut self, bb: &Blackboard) {
        for (i, r) in self.regions.iter().enumerate() {
            self.dirty_scratch[i] = !r.read_set.is_empty() && r.is_dirty(bb);
        }
    }

    /// Wait for the next thing to happen and tick whatever is due.
    ///
    /// Allocation-free: the region table and the dirty scratch are sized at
    /// construction and never grow.
    pub fn step(&mut self, bb: &mut Blackboard, pacer: &impl Pacer) -> StepOutcome {
        self.step_traced(bb, pacer, None)
    }

    /// [`Scheduler::step`], reporting node transitions to `trace`.
    pub fn step_traced(
        &mut self,
        bb: &mut Blackboard,
        pacer: &impl Pacer,
        trace: Option<&mut dyn TraceSink>,
    ) -> StepOutcome {
        if self.aborted.is_some() || self.regions.is_empty() {
            return StepOutcome::Finished {
                status: self.settled.unwrap_or(Status::Running),
            };
        }

        let Some(target) = self.next_wake(bb) else {
            return StepOutcome::Finished {
                status: self.settled.unwrap_or(Status::Running),
            };
        };

        let now = pacer.wait_or_wake(target);
        self.refresh_dirty(bb);

        // Lowest index among the due regions — deterministic, so a run under
        // VirtualTime is reproducible whatever the wake order was.
        let Some(idx) =
            (0..self.regions.len()).find(|&i| self.regions[i].is_due(now, self.dirty_scratch[i]))
        else {
            // A wake landed but nothing turned out to want it. Cheap and
            // expected: a write to a slot inside a debounce window looks exactly
            // like this.
            return StepOutcome::Woken;
        };

        let lateness = now.saturating_sub(self.regions[idx].deadline);

        let outcome = if self.region_is_due(idx) {
            let t0 = pacer.now();
            let status = {
                let mut ctx = Ctx::new(bb, pacer.clock());
                ctx.tick_index = self.regions[idx].stats.ticks;
                if let Some(sink) = trace {
                    ctx.trace = Some(sink);
                }
                tick_node(self.tree.graph_mut(), self.regions[idx].entry, &mut ctx)
            };
            let duration = pacer.now().saturating_sub(t0);

            match self.regions[idx].boundary {
                Some(node) => {
                    self.tree.graph_mut().set_region_status(node, status);
                }
                None => {
                    if status.is_settled() {
                        self.settled = Some(status);
                    }
                }
            }

            self.regions[idx].stats.record(lateness, duration);
            StepOutcome::Ticked {
                region: idx,
                status,
                lateness,
            }
        } else {
            StepOutcome::Idle { region: idx }
        };

        // Whether it ticked or not, the region has been considered at `now`:
        // record what it saw and move its deadline on, or it would spin.
        self.regions[idx].last_tick = now;
        self.regions[idx].snapshot_versions(bb);
        if let Err(e) = self.reschedule(idx, now, lateness) {
            self.aborted = Some(e);
            return StepOutcome::Finished {
                status: Status::Failure,
            };
        }

        outcome
    }

    /// Is region `idx` live enough to tick?
    fn region_is_due(&self, idx: usize) -> bool {
        let Some(node) = self.regions[idx].boundary else {
            return self.settled.is_none();
        };
        let g = self.tree.graph();
        // Idle means an ancestor halted this branch; the scheduler must not keep
        // it alive behind the tree's back.
        if g.state(node) == NodeState::Idle {
            return false;
        }
        // A published verdict waits to be read, not overwritten.
        g.region_status(node) == Some(Status::Running)
    }

    /// Advance a region's deadline, applying its overrun policy.
    fn reschedule(&mut self, idx: usize, now: Nanos, lateness: Nanos) -> Result<(), SchedError> {
        let period = self.regions[idx].policy.heartbeat();
        let r = &mut self.regions[idx];

        // The next deadline comes from the last deadline, not from `now`. This
        // is the whole drift-compensation story: a tick that ran long costs its
        // own phase and nothing after it.
        let mut next = r.deadline.saturating_add(period);

        // An *overrun* is a lost tick, not merely a late one. Counting any
        // lateness made the metric read 100% on a run whose worst lateness was
        // 158 ns — true, and useless.
        if lateness >= period {
            r.stats.overruns += 1;
            match r.overrun {
                OverrunPolicy::Abort => {
                    return Err(SchedError::DeadlineMissed {
                        region: r.boundary.unwrap_or(NodeId::ROOT),
                        late_by: lateness,
                        deadline: r.deadline,
                    });
                }
                OverrunPolicy::CatchUp { max } => {
                    // Bounded: unbounded catch-up turns one slow tick into a
                    // burst that causes the next one.
                    let missed = (lateness / period).min(u64::from(max));
                    r.stats.caught_up += missed;
                    next = r.deadline.saturating_add(period.saturating_mul(missed + 1));
                }
                OverrunPolicy::Skip | OverrunPolicy::Warn => {}
            }
        }

        if next <= now {
            // Re-align to the grid rather than to `now`, so a region that
            // recovers lands back on its original phase.
            let skip = (now - next) / period + 1;
            if lateness >= period {
                r.stats.skipped += skip;
            }
            next = next.saturating_add(period.saturating_mul(skip));
        }

        r.deadline = next;
        Ok(())
    }

    /// Run until the root region settles, `budget` nanoseconds elapse, or
    /// `max_steps` steps have been taken. `0` means "no limit".
    pub fn run_for(
        &mut self,
        bb: &mut Blackboard,
        pacer: &impl Pacer,
        budget: Nanos,
        max_steps: u64,
    ) -> RunOutcome {
        let start = pacer.now();
        self.arm(start);
        let limit = if budget == 0 {
            Nanos::MAX
        } else {
            start.saturating_add(budget)
        };

        let mut steps = 0u64;
        loop {
            if let Some(err) = self.aborted.clone() {
                return RunOutcome::Aborted(err);
            }
            if let Some(status) = self.settled {
                return RunOutcome::Settled {
                    status,
                    at: pacer.now(),
                };
            }
            if max_steps != 0 && steps >= max_steps {
                return RunOutcome::StepsUp { at: pacer.now() };
            }
            match self.next_wake(bb) {
                None => {
                    return RunOutcome::Settled {
                        status: self.settled.unwrap_or(Status::Running),
                        at: pacer.now(),
                    };
                }
                // Peeking rather than stepping keeps the budget from being
                // overshot by a whole period just to discover it was reached.
                Some(next) if next >= limit => return RunOutcome::TimeUp { at: pacer.now() },
                Some(_) => {}
            }
            if let Some(err) = self.check_abort(pacer) {
                self.aborted = Some(err.clone());
                return RunOutcome::Aborted(err);
            }

            self.step(bb, pacer);
            steps += 1;
        }
    }

    /// A region under `Abort` that is already past its deadline.
    fn check_abort(&self, pacer: &impl Pacer) -> Option<SchedError> {
        let now = pacer.now();
        self.regions.iter().find_map(|r| {
            (r.overrun == OverrunPolicy::Abort && now > r.deadline).then(|| {
                SchedError::DeadlineMissed {
                    region: r.boundary.unwrap_or(NodeId::ROOT),
                    late_by: now - r.deadline,
                    deadline: r.deadline,
                }
            })
        })
    }

    /// Halt every running node and clear each region's published verdict.
    pub fn halt(&mut self, bb: &mut Blackboard, pacer: &impl Pacer) {
        let mut ctx = Ctx::new(bb, pacer.clock());
        halt_node(self.tree.graph_mut(), NodeId::ROOT, &mut ctx);
        self.settled = None;
        self.aborted = None;
    }

    #[must_use]
    pub fn into_tree(self) -> Tree {
        self.tree
    }

    /// Install a structurally different tree and rebuild the region table.
    ///
    /// A patch renumbers nodes, so every `NodeId` the scheduler was holding is
    /// stale the moment it lands — region boundaries, entry points and read sets
    /// all have to be found again. Rebuilding is O(nodes) and happens between
    /// ticks, which is the whole reason patches are applied there.
    ///
    /// Per-region deadlines are *carried over by position*, so a swap does not
    /// reset the phase of regions that were not touched. `arm` afterwards if you
    /// want a clean start instead.
    ///
    /// # Errors
    /// See [`SchedError`] — the new tree's region ports are read here.
    pub fn replace_tree(
        &mut self,
        tree: Tree,
        bb: &Blackboard,
        root_hz: f64,
    ) -> Result<(), SchedError> {
        let carried: Vec<(Nanos, Nanos, RegionStats)> = self
            .regions
            .iter()
            .map(|r| (r.deadline, r.last_tick, r.stats))
            .collect();

        let mut next = Scheduler::new(tree, bb, root_hz)?;
        for (i, r) in next.regions.iter_mut().enumerate() {
            if let Some((deadline, last_tick, stats)) = carried.get(i) {
                r.deadline = *deadline;
                r.last_tick = *last_tick;
                r.stats = *stats;
            }
        }
        next.settled = self.settled;
        *self = next;
        Ok(())
    }
}

fn build_region(
    graph: &bteng_core::TreeGraph,
    bb: &Blackboard,
    id: NodeId,
) -> Result<RegionState, SchedError> {
    let read = |port: u16, name: &'static str| -> Result<SlotId, SchedError> {
        graph.port_slot(id, port).ok_or(SchedError::UnreadablePort {
            region: id,
            port: name,
        })
    };

    let hz = *bb
        .get::<f64>(read(bteng_core::REGION_PORT_HZ, "hz")?)
        .map_err(|_| SchedError::UnreadablePort {
            region: id,
            port: "hz",
        })?;

    let overrun = OverrunPolicy::parse(
        bb.get::<String>(read(bteng_core::REGION_PORT_OVERRUN, "overrun")?)
            .map_err(|_| SchedError::UnreadablePort {
                region: id,
                port: "overrun",
            })?,
    )?;

    let mode = bb
        .get::<String>(read(bteng_core::REGION_PORT_MODE, "mode")?)
        .map_err(|_| SchedError::UnreadablePort {
            region: id,
            port: "mode",
        })?
        .clone();

    let min_period_ms = *bb
        .get::<u64>(read(bteng_core::REGION_PORT_MIN_PERIOD, "min_period_ms")?)
        .map_err(|_| SchedError::UnreadablePort {
            region: id,
            port: "min_period_ms",
        })?;

    let policy = match mode.trim() {
        "periodic" => TickPolicy::periodic_hz(hz)?,
        "on_change" => TickPolicy::on_change(hz, min_period_ms.saturating_mul(1_000_000))?,
        other => {
            return Err(SchedError::BadMode {
                text: other.to_string(),
            });
        }
    };

    Ok(RegionState {
        boundary: Some(id),
        // A Region has exactly one child; arity is checked at build.
        entry: graph.child(id, 0),
        policy,
        overrun,
        deadline: 0,
        last_tick: 0,
        read_set: Vec::new(),
        versions: Vec::new(),
        stats: RegionStats::default(),
    })
}

/// Every slot read by any node in a region, computed once from the graph.
///
/// This is the precise thing Python BTEng approximates with a global dirty flag.
/// It stops at nested region boundaries: an inner region's inputs are the inner
/// region's business, and rolling them up would make every parent wake for
/// everything its children read.
fn read_set_of(graph: &bteng_core::TreeGraph, entry: NodeId) -> Vec<SlotId> {
    let mut out: Vec<SlotId> = Vec::new();
    let mut stack = vec![entry];
    while let Some(id) = stack.pop() {
        let manifest = graph.manifest(id);
        let bindings = graph.bindings_of(id);
        for (i, spec) in manifest.ports.iter().enumerate() {
            if spec.direction.readable()
                && let Some(b) = bindings.get(i)
                && !out.contains(&b.slot)
            {
                out.push(b.slot);
            }
        }
        for &child in graph.children_of(id) {
            if !graph.is_region(child) {
                stack.push(child);
            }
        }
    }
    // Sorted so a region's read set is stable across builds — the dirty scan
    // walks it linearly and a stable order keeps that cache-friendly.
    out.sort_unstable();
    out
}
