//! Adversarial shapes and soak runs.
//!
//! The benchmark asks "how fast is the normal case". This asks the other
//! question: **where does it break, and does it break honestly?**
//!
//! Every case here is either a shape nobody would write on purpose (a 4000-deep
//! chain, 512 regions, 20 000 nodes) or a duration nobody runs in a test (ten
//! million ticks, a hundred thousand patches). Both find things unit tests
//! cannot: quadratic behaviour that is invisible at n=10, allocation that only
//! shows up after the first thousand ticks, growth that only reads as a leak
//! over minutes.
//!
//! ```text
//! cargo run --release -p bteng-bench -- --stress
//! cargo run --release -p bteng-bench -- --stress --heavy
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use bteng_core::{
    Blackboard, Builtin, Ctx, NodeId, SimClock, Status, Tree, TreeBuilder, TreeGraph, TreePatch,
    Uid, apply,
};
use bteng_sched::{Pacer, Scheduler, StepOutcome, VirtualTime};

use crate::{ALLOCS, COUNTING};

static SINK: AtomicU64 = AtomicU64::new(0);

fn work(_c: &mut Ctx<'_>) -> Status {
    SINK.fetch_add(1, Ordering::Relaxed);
    Status::Running
}

fn settle(_c: &mut Ctx<'_>) -> Status {
    SINK.fetch_add(1, Ordering::Relaxed);
    Status::Success
}

fn yes(_c: &Ctx<'_>) -> bool {
    true
}

// ── reporting ────────────────────────────────────────────────────────────────

/// What a case claims about allocation.
enum Alloc {
    /// Must be zero — a tick or a scheduler step.
    MustBeZero(u64),
    /// Allocates by design, and that is not a defect. Patching rebuilds an
    /// arena, spawning boxes a job, threads have stacks. Asserting zero here
    /// would be asserting a falsehood; what these cases check is that they stay
    /// *correct* under repetition.
    ByDesign,
    /// Not measured.
    NotMeasured,
}

struct Report {
    name: String,
    detail: String,
    allocs: Alloc,
    elapsed: std::time::Duration,
    verdict: Result<(), String>,
}

/// Count allocations across `f` and nothing else.
///
/// Scoped tightly on purpose: the first version of this harness wrapped the
/// whole case, so it counted the `format!` that built the case's own summary and
/// reported every case as allocating. A measurement that includes the
/// measurement is not a measurement.
fn alloc_gate<R>(f: impl FnOnce() -> R) -> (R, u64) {
    ALLOCS.store(0, Ordering::Relaxed);
    COUNTING.store(true, Ordering::Relaxed);
    let out = f();
    COUNTING.store(false, Ordering::Relaxed);
    (out, ALLOCS.load(Ordering::Relaxed))
}

fn measure<R>(name: &str, f: impl FnOnce() -> (String, Result<(), String>, Alloc, R)) -> Report {
    let t0 = Instant::now();
    let (detail, verdict, allocs, keep) = f();
    let elapsed = t0.elapsed();
    drop(keep);
    Report {
        name: name.to_string(),
        detail,
        allocs,
        elapsed,
        verdict,
    }
}

fn check(cond: bool, msg: impl Into<String>) -> Result<(), String> {
    if cond { Ok(()) } else { Err(msg.into()) }
}

// ── shapes ───────────────────────────────────────────────────────────────────

fn deep_chain(depth: usize) -> (Tree, Blackboard) {
    let mut b = TreeBuilder::new();
    for _ in 0..depth {
        b.begin("seq", Builtin::sequence());
    }
    b.leaf("leaf", Builtin::action(work));
    for _ in 0..depth {
        b.end();
    }
    let mut bb = Blackboard::new();
    let graph = b.build(&mut bb).expect("deep chain builds");
    (Tree::new(graph), bb)
}

fn wide_tree(branches: usize, depth: usize) -> (Tree, Blackboard) {
    let mut b = TreeBuilder::new();
    b.begin("root", Builtin::sequence());
    for i in 0..branches {
        b.begin("branch", Builtin::sequence());
        for _ in 0..depth {
            b.leaf("guard", Builtin::condition(yes));
            b.leaf(
                "work",
                Builtin::action(if i == branches - 1 { work } else { settle }),
            );
        }
        b.end();
    }
    b.end();
    let mut bb = Blackboard::new();
    let graph = b.build(&mut bb).expect("wide tree builds");
    (Tree::new(graph), bb)
}

/// Like [`wide_tree`] but every leaf succeeds, so the root settles each tick and
/// the next tick starts over — the whole arena, every time.
fn settling_tree(branches: usize, depth: usize) -> (Tree, Blackboard) {
    let mut b = TreeBuilder::new();
    b.begin("root", Builtin::sequence());
    for _ in 0..branches {
        b.begin("branch", Builtin::sequence());
        for _ in 0..depth {
            b.leaf("guard", Builtin::condition(yes));
            b.leaf("work", Builtin::action(settle));
        }
        b.end();
    }
    b.end();
    let mut bb = Blackboard::new();
    let graph = b.build(&mut bb).expect("settling tree builds");
    (Tree::new(graph), bb)
}

fn many_regions(count: usize) -> (Scheduler, Blackboard) {
    // Grouped, because `Parallel` caps at 64 children — the completion bitset is
    // a u64. Writing this fixture is what made me meet that limit, and the
    // builder refusing beats a bitset silently losing track of child 65.
    const GROUP: usize = 32;
    let mut b = TreeBuilder::new();
    b.begin("root", Builtin::parallel());
    for group in 0..count.div_ceil(GROUP) {
        b.begin("group", Builtin::parallel());
        for i in group * GROUP..((group + 1) * GROUP).min(count) {
            b.begin("region", Builtin::region())
                // Spread the rates so deadlines do not all collide on one instant.
                .port("hz", &format!("{}", 5 + (i % 95)))
                .leaf("work", Builtin::action(work))
                .end();
        }
        b.end();
    }
    b.end();
    let mut bb = Blackboard::new();
    let graph = b.build(&mut bb).expect("region farm builds");
    let sched = Scheduler::new(Tree::new(graph), &bb, 10.0).expect("scheduler builds");
    (sched, bb)
}

// ── the cases ────────────────────────────────────────────────────────────────

fn case_deep(depth: usize) -> Report {
    measure(&format!("deep chain d={depth}"), || {
        let (mut tree, mut bb) = deep_chain(depth);
        let clock = SimClock::new();
        let mut ctx = Ctx::new(&mut bb, &clock);
        for _ in 0..8 {
            tree.tick(&mut ctx);
        }
        let (status, allocs) = alloc_gate(|| {
            let mut status = Status::Success;
            for _ in 0..200 {
                status = tree.tick(&mut ctx);
            }
            status
        });
        (
            format!("{} nodes", tree.graph().len()),
            check(status == Status::Running, "deep chain settled unexpectedly"),
            Alloc::MustBeZero(allocs),
            tree,
        )
    })
}

fn case_wide(branches: usize, depth: usize) -> Report {
    measure(&format!("wide {branches}x{depth}"), || {
        let (mut tree, mut bb) = wide_tree(branches, depth);
        let n = tree.graph().len();
        let clock = SimClock::new();
        let mut ctx = Ctx::new(&mut bb, &clock);
        for _ in 0..8 {
            tree.tick(&mut ctx);
        }
        let ((), allocs) = alloc_gate(|| {
            for _ in 0..100 {
                tree.tick(&mut ctx);
            }
        });
        (
            format!("{n} nodes"),
            Ok(()),
            Alloc::MustBeZero(allocs),
            tree,
        )
    })
}

fn case_soak(ticks: u64) -> Report {
    measure(&format!("soak {ticks} ticks"), || {
        // A *settling* tree: every tick walks the whole thing. The first version
        // parked on a Running leaf, so a Sequence resumed straight into it and
        // each "tick" visited about three nodes — a soak that soaked nothing and
        // reported a very good number for it.
        let (mut tree, mut bb) = settling_tree(16, 4);
        let clock = SimClock::new();
        let mut ctx = Ctx::new(&mut bb, &clock);
        for _ in 0..1000 {
            tree.tick(&mut ctx);
        }
        let slots_before = ctx.bb.len();
        let ((), allocs) = alloc_gate(|| {
            for _ in 0..ticks {
                tree.tick(&mut ctx);
            }
        });
        let slots_after = ctx.bb.len();
        (
            format!("{} nodes, {ticks} ticks", tree.graph().len()),
            check(
                slots_before == slots_after,
                format!("blackboard grew from {slots_before} to {slots_after} slots"),
            ),
            Alloc::MustBeZero(allocs),
            tree,
        )
    })
}

fn case_region_farm(count: usize, steps: u64) -> Report {
    measure(&format!("{count} regions"), || {
        let (mut sched, mut bb) = many_regions(count);
        let pacer = VirtualTime::new();
        sched.arm(pacer.now());
        for _ in 0..2000 {
            sched.step(&mut bb, &pacer);
        }
        let (ticked, allocs) = alloc_gate(|| {
            let mut ticked = 0u64;
            for _ in 0..steps {
                if let StepOutcome::Ticked { .. } = sched.step(&mut bb, &pacer) {
                    ticked += 1;
                }
            }
            ticked
        });
        let ns_per_step = 0.0; // filled in by the caller's timing; see detail below
        let _ = ns_per_step;
        (
            format!(
                "{} regions, {steps} steps, {ticked} ticks",
                sched.region_count()
            ),
            check(ticked > 0, "no region ever ticked"),
            Alloc::MustBeZero(allocs),
            sched,
        )
    })
}

fn case_event_storm(writes: u64) -> Report {
    measure("event storm", || {
        let mut b = TreeBuilder::new();
        b.begin("root", Builtin::parallel())
            .begin("reactive", Builtin::region())
            .port("hz", "1.0")
            .port("mode", "on_change")
            .port("min_period_ms", "5")
            // KeepRunningUntilFailure so the region never settles: a region
            // that latches Success stops being scheduled, and the first version
            // of this case measured exactly two ticks and called it a debounce.
            .begin("forever", Builtin::KeepRunningUntilFailure)
            .leaf("wait", Builtin::sleep())
            .port("msec", "{churn}")
            .end()
            .end()
            .leaf("hold", Builtin::action(work))
            .end();
        let mut bb = Blackboard::new();
        let graph = b.build(&mut bb).expect("event tree builds");
        let churn = bb.resolve("churn").expect("bound");
        let mut sched = Scheduler::new(Tree::new(graph), &bb, 200.0).expect("builds");
        let pacer = VirtualTime::new();
        sched.arm(pacer.now());

        // Hammer the watched slot far faster than the debounce allows.
        let ((), allocs) = alloc_gate(|| {
            for i in 0..writes {
                bb.set(churn, i % 3).unwrap();
                sched.step(&mut bb, &pacer);
            }
        });
        let reactive = sched.stats(1).expect("region 1").ticks;
        let elapsed_ms = pacer.now() / 1_000_000;
        let ceiling = elapsed_ms / 5 + 8;
        (
            format!("{writes} writes over {elapsed_ms} ms -> {reactive} region ticks"),
            check(
                reactive <= ceiling && reactive > 0,
                format!("debounce: {reactive} ticks against a ceiling of {ceiling}"),
            ),
            Alloc::MustBeZero(allocs),
            sched,
        )
    })
}

fn case_patch_storm(rounds: usize, tree_nodes: usize) -> Report {
    measure(&format!("patch storm x{rounds}"), || {
        let branches = tree_nodes / 8;
        let (tree, mut bb) = wide_tree(branches, 4);
        let mut graph = tree.into_graph();
        let clock = SimClock::new();
        let n_before = graph.len();

        // Warm up so the counter measures steady state, not first-time growth.
        {
            let mut ctx = Ctx::new(&mut bb, &clock);
            for _ in 0..4 {
                bteng_core::tick_node(&mut graph, NodeId::ROOT, &mut ctx);
            }
        }

        for round in 0..rounds {
            let target = pick_target(&graph, round);
            // A single leaf, so the splice is depth-neutral. A two-level
            // replacement deepens the tree by one every round, and two thousand
            // rounds of that walks straight into the depth limit — which is the
            // limit working, but it makes this case measure the limit instead of
            // the splice. The next case measures the limit on purpose.
            let mut rb = TreeBuilder::new();
            rb.leaf("patched", Builtin::action(settle));
            let incoming = rb.build(&mut bb).expect("replacement builds");
            let patch = TreePatch::Replace { target };
            let plan = match patch.validate(&graph, Some(&incoming)) {
                Ok(p) => p,
                Err(e) => {
                    return (
                        format!("round {round}"),
                        Err(format!("validate failed: {e}")),
                        Alloc::NotMeasured,
                        graph,
                    );
                }
            };
            let out = apply(graph, Some(incoming), plan);
            graph = out.graph;

            let mut ctx = Ctx::new(&mut bb, &clock);
            bteng_core::tick_node(&mut graph, NodeId::ROOT, &mut ctx);

            if round % 500 == 0 {
                let mut uids: Vec<u64> = graph.ids().map(|id| graph.uid(id).0).collect();
                uids.sort_unstable();
                let total = uids.len();
                uids.dedup();
                if uids.len() != total {
                    return (
                        format!("round {round}"),
                        Err("uid collision after splice".into()),
                        Alloc::NotMeasured,
                        graph,
                    );
                }
            }
        }

        let n_after = graph.len();
        let version = graph.version().0;
        let slots = bb.len();
        (
            format!("{n_before}->{n_after} nodes, v{version}, {slots} slots"),
            check(
                version == rounds as u64,
                format!("expected v{rounds}, got v{version}"),
            ),
            // A splice rebuilds the arena, so of course it allocates. The claim
            // here is that it stays correct under repetition, not that it is
            // free.
            Alloc::ByDesign,
            graph,
        )
    })
}

/// Any node but the root, chosen deterministically.
///
/// The first version picked nodes named "branch", which the storm then replaced
/// one by one until there were none left and every later round hammered the same
/// node — a stress case that stopped stressing anything about a third of the way
/// in, and said nothing about it.
fn pick_target(graph: &TreeGraph, round: usize) -> Uid {
    let n = graph.len();
    debug_assert!(n > 1, "a one-node tree has nothing to replace");
    graph.uid(NodeId(1 + (round % (n - 1)) as u32))
}

/// Keep deepening the tree until the guard says no, and make sure it says no
/// rather than taking the process with it.
fn case_depth_guard() -> Report {
    measure("depth guard", || {
        let mut bb = Blackboard::new();
        let mut b = TreeBuilder::new();
        b.begin("root", Builtin::sequence())
            .leaf("tip", Builtin::action(work))
            .end();
        let mut graph = b.build(&mut bb).expect("builds");
        let clock = SimClock::new();

        let mut rounds = 0usize;
        let refusal = loop {
            let tip = graph
                .ids()
                .max_by_key(|id| {
                    let mut d = 0;
                    let mut cur = *id;
                    while cur != NodeId::ROOT {
                        cur = graph.parent(cur);
                        d += 1;
                    }
                    d
                })
                .expect("non-empty");
            let target = graph.uid(tip);

            let mut rb = TreeBuilder::new();
            rb.begin("deeper", Builtin::sequence())
                .leaf("tip", Builtin::action(work))
                .end();
            let incoming = rb.build(&mut bb).expect("builds");

            let patch = TreePatch::Replace { target };
            match patch.validate(&graph, Some(&incoming)) {
                Ok(plan) => {
                    graph = apply(graph, Some(incoming), plan).graph;
                    rounds += 1;
                    // Tick it every round: if the guard were off by a little,
                    // this is where the stack would go.
                    let mut ctx = Ctx::new(&mut bb, &clock);
                    bteng_core::tick_node(&mut graph, NodeId::ROOT, &mut ctx);
                }
                Err(e) => break e,
            }
            if rounds > bteng_core::DEFAULT_MAX_DEPTH * 2 {
                break bteng_core::PatchError::CannotReplaceRoot; // unreachable guard
            }
        };

        let refused_at_limit = matches!(refusal, bteng_core::PatchError::TooDeep { .. });
        (
            format!("refused after {rounds} deepening patches"),
            check(
                refused_at_limit && rounds > 100,
                format!("guard behaved oddly: {refusal} after {rounds} rounds"),
            ),
            Alloc::ByDesign,
            graph,
        )
    })
}

fn case_rollback_churn(rounds: usize) -> Report {
    measure(&format!("rollback churn x{rounds}"), || {
        let (tree, mut bb) = wide_tree(4, 2);
        let mut graph = tree.into_graph();
        let clock = SimClock::new();
        let original: Vec<u64> = graph.ids().map(|id| graph.uid(id).0).collect();

        for round in 0..rounds {
            let target = pick_target(&graph, 0);
            let mut rb = TreeBuilder::new();
            rb.leaf("tmp", Builtin::action(settle));
            let incoming = rb.build(&mut bb).expect("builds");
            let plan = TreePatch::Replace { target }
                .validate(&graph, Some(&incoming))
                .expect("valid");
            let out = apply(graph, Some(incoming), plan);
            graph = out.graph;
            let removed = out.removed.expect("Replace returns what it took");

            let back = out
                .inverse
                .validate(&graph, Some(&removed))
                .expect("inverse valid");
            let out2 = apply(graph, Some(removed), back);
            graph = out2.graph;

            let now: Vec<u64> = graph.ids().map(|id| graph.uid(id).0).collect();
            if now != original {
                return (
                    format!("round {round}"),
                    Err(format!(
                        "rollback did not restore identities: {} vs {} uids",
                        now.len(),
                        original.len()
                    )),
                    Alloc::NotMeasured,
                    graph,
                );
            }
            let mut ctx = Ctx::new(&mut bb, &clock);
            bteng_core::tick_node(&mut graph, NodeId::ROOT, &mut ctx);
        }

        let slots = bb.len();
        (
            format!("{rounds} apply+undo cycles, {slots} slots"),
            Ok(()),
            Alloc::ByDesign,
            graph,
        )
    })
}

fn case_wake_storm(threads: usize, per_thread: u64) -> Report {
    measure("wake storm", || {
        use bteng_core::Waker;
        use bteng_sched::WakeBus;

        let bus = Arc::new(WakeBus::new());
        let mut handles = Vec::new();
        for _ in 0..threads {
            let bus: Arc<dyn Waker> = Arc::clone(&bus) as Arc<dyn Waker>;
            handles.push(std::thread::spawn(move || {
                for _ in 0..per_thread {
                    bus.wake();
                }
            }));
        }
        for h in handles {
            h.join().expect("waker thread");
        }
        let generation = bus.generation();
        let expected = threads as u64 * per_thread;
        (
            format!("{threads} threads x {per_thread} wakes -> generation {generation}"),
            check(
                generation == expected,
                format!("lost wakes: generation {generation}, expected {expected}"),
            ),
            Alloc::ByDesign,
            bus,
        )
    })
}

fn case_async_churn(rounds: usize) -> Report {
    measure(&format!("async churn x{rounds}"), || {
        use bteng_async::{InlineSpawner, Spawner};
        let spawner = InlineSpawner::new();
        let mut settled = 0u64;
        for i in 0..rounds {
            let handle = spawner.spawn(
                Box::new(move |_t| {
                    if i % 7 == 0 {
                        Status::Failure
                    } else {
                        Status::Success
                    }
                }),
                None,
            );
            handle.cancel();
            if handle.is_done() {
                settled += 1;
            }
        }
        (
            format!("{rounds} spawn+cancel, {settled} settled"),
            check(
                settled == rounds as u64,
                format!("{} tasks never settled", rounds as u64 - settled),
            ),
            // Every spawn boxes a job and an Arc'd slot. Allocating is the
            // design; settling every time is the claim.
            Alloc::ByDesign,
            (),
        )
    })
}

fn case_halt_churn(rounds: usize) -> Report {
    measure(&format!("halt churn x{rounds}"), || {
        let (mut tree, mut bb) = wide_tree(8, 3);
        let clock = SimClock::new();
        let mut ctx = Ctx::new(&mut bb, &clock);
        for _ in 0..8 {
            tree.tick(&mut ctx);
            tree.halt(&mut ctx);
        }
        let ((), allocs) = alloc_gate(|| {
            for _ in 0..rounds {
                tree.tick(&mut ctx);
                tree.halt(&mut ctx);
            }
        });
        let root_idle = tree.graph().state(NodeId::ROOT) == bteng_core::NodeState::Idle;
        (
            format!("{rounds} tick+halt cycles"),
            check(root_idle, "tree did not come to rest after a halt"),
            Alloc::MustBeZero(allocs),
            tree,
        )
    })
}

// ── entry point ──────────────────────────────────────────────────────────────

/// Run the stress suite. `heavy` turns the dial up to where it takes minutes.
pub fn main(heavy: bool) -> bool {
    println!(
        "BTEng-Rust stress  [{}]{}",
        std::env::consts::ARCH,
        if heavy { "  (heavy)" } else { "" }
    );
    println!(
        "Allocations are counted only in the steady-state part of each case;\n\
         construction allocates by design.\n"
    );

    let scale = if heavy { 10 } else { 1 };

    println!(
        "{:<24} {:<38} {:>9} {:>9}  verdict",
        "case", "detail", "time", "allocs"
    );

    // Printed as they finish rather than collected first: a case that panics
    // should not take the results of the ones that passed with it.
    type Case = (&'static str, Box<dyn Fn() -> Report>);
    let cases: Vec<Case> = vec![
        ("deep 64", Box::new(|| case_deep(64))),
        (
            "deep at limit",
            Box::new(|| case_deep(bteng_core::DEFAULT_MAX_DEPTH - 1)),
        ),
        ("wide", Box::new(|| case_wide(2000, 4))),
        (
            "soak",
            Box::new(move || case_soak(1_000_000 * scale as u64)),
        ),
        ("regions 64", Box::new(|| case_region_farm(64, 100_000))),
        ("regions 512", Box::new(|| case_region_farm(512, 100_000))),
        (
            "event storm",
            Box::new(move || case_event_storm(20_000 * scale as u64)),
        ),
        (
            "patch storm",
            Box::new(move || case_patch_storm(2_000 * scale, 4_000)),
        ),
        ("depth guard", Box::new(case_depth_guard)),
        (
            "rollback",
            Box::new(move || case_rollback_churn(2_000 * scale)),
        ),
        ("wake storm", Box::new(|| case_wake_storm(8, 50_000))),
        ("async churn", Box::new(|| case_async_churn(200_000))),
        (
            "halt churn",
            Box::new(move || case_halt_churn(50_000 * scale)),
        ),
    ];

    let total = cases.len();
    let mut failures = 0;
    for (_, run) in cases {
        let r = run();
        let (alloc_cell, alloc_bad) = match r.allocs {
            Alloc::MustBeZero(n) => (format!("{n}"), n != 0),
            Alloc::ByDesign | Alloc::NotMeasured => ("by design".to_string(), false),
        };
        let verdict = match (&r.verdict, alloc_bad) {
            (Ok(()), false) => "ok".to_string(),
            (Ok(()), true) => "ALLOCATED on the tick path".to_string(),
            (Err(e), _) => e.clone(),
        };
        if r.verdict.is_err() || alloc_bad {
            failures += 1;
        }
        println!(
            "{:<24} {:<38} {:>8.2}s {:>9}  {}",
            r.name,
            r.detail,
            r.elapsed.as_secs_f64(),
            alloc_cell,
            verdict
        );
        use std::io::Write as _;
        let _ = std::io::stdout().flush();
    }

    println!();
    if failures == 0 {
        println!("all {total} cases passed");
    } else {
        println!("{failures} of {total} cases FAILED");
    }
    core::hint::black_box(SINK.load(Ordering::Relaxed));
    failures == 0
}
