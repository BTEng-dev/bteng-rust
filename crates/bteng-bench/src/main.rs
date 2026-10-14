//! M0 benchmark harness.
//!
//! Reports, per tree shape: ticks/second, tick-latency percentiles, and
//! allocations per tick. Dependency-free on purpose — it has to run unchanged
//! on x86-64, on a Jetson, and under QEMU, and its numbers have to be about the
//! engine rather than about a benchmarking framework.
//!
//! ```text
//! cargo run --release -p bteng-bench
//! cargo run --release -p bteng-bench -- --json > bench-results/x86.json
//! ```
//!
//! Latency percentiles from an emulated target are meaningless; only the
//! allocation count and the correctness of the run carry over. The harness
//! prints that warning rather than leaving it to be forgotten.

use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::alloc::System;
use std::time::Instant;

use bteng_core::{Blackboard, Builtin, Ctx, SimClock, Status, Tree, TreeBuilder};

mod jitter;
mod stress;

// ── allocation counter ───────────────────────────────────────────────────────

pub(crate) static ALLOCS: AtomicU64 = AtomicU64::new(0);
pub(crate) static COUNTING: AtomicBool = AtomicBool::new(false);

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // Relaxed on both: this is a statistics counter, and no other memory is
        // published through it. (DR-2 §3.1 — every ordering gets a reason.)
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

// ── trees under test ─────────────────────────────────────────────────────────
//
// Two shapes, measuring two different things.
//
// A *settling* tree returns Success every tick, so every tick walks the whole
// arena and `ns/node` means what it says. A *resuming* tree parks on a Running
// leaf, so a plain Sequence re-enters only the active path — realistic, but its
// per-tick cost is about path depth, not tree size, and it is reported as
// ns/tick only. Conflating the two is how a benchmark ends up claiming 0.2
// ns/node for a 609-node tree it never actually walked.

fn running(_c: &mut Ctx<'_>) -> Status {
    Status::Running
}
fn succeed(_c: &mut Ctx<'_>) -> Status {
    Status::Success
}
fn fail(_c: &mut Ctx<'_>) -> Status {
    Status::Failure
}
fn yes(_c: &Ctx<'_>) -> bool {
    true
}

/// Balanced `fanout`-ary tree, `depth` levels of Sequence over leaf pairs.
/// Settles Success every tick.
fn balanced(depth: u32, fanout: u32) -> (Tree, Blackboard) {
    fn rec(b: &mut TreeBuilder, depth: u32, fanout: u32) {
        if depth == 0 {
            b.leaf("guard", Builtin::condition(yes));
            b.leaf("work", Builtin::action(succeed));
            return;
        }
        b.begin("seq", Builtin::sequence());
        for _ in 0..fanout {
            rec(b, depth - 1, fanout);
        }
        b.end();
    }
    let mut b = TreeBuilder::new();
    b.begin("root", Builtin::sequence());
    rec(&mut b, depth, fanout);
    b.end();
    let mut bb = Blackboard::new();
    let graph = b.build(&mut bb).expect("balanced tree builds");
    (Tree::new(graph), bb)
}

/// Shallow and wide, with a recovery Fallback per branch — closer to the shape
/// of a real mission tree. Settles Success every tick.
fn mission(branches: u32) -> (Tree, Blackboard) {
    let mut b = TreeBuilder::new();
    b.begin("root", Builtin::sequence());
    for _ in 0..branches {
        b.begin("branch", Builtin::sequence());
        b.leaf("precondition", Builtin::condition(yes));
        b.begin("guarded", Builtin::fallback());
        b.leaf("try", Builtin::action(fail));
        b.leaf("recover", Builtin::action(succeed));
        b.end();
        b.end();
    }
    b.end();
    let mut bb = Blackboard::new();
    let graph = b.build(&mut bb).expect("mission tree builds");
    (Tree::new(graph), bb)
}

/// A long chain parked on a Running leaf: measures the resume path a
/// long-lived robot tree actually pays on most of its ticks.
fn resume_chain(depth: u32) -> (Tree, Blackboard) {
    let mut b = TreeBuilder::new();
    for _ in 0..depth {
        b.begin("seq", Builtin::sequence());
        b.leaf("guard", Builtin::condition(yes));
    }
    b.leaf("hold", Builtin::action(running));
    for _ in 0..depth {
        b.end();
    }
    let mut bb = Blackboard::new();
    let graph = b.build(&mut bb).expect("resume chain builds");
    (Tree::new(graph), bb)
}

// ── measurement ──────────────────────────────────────────────────────────────

/// name, tree constructor, tick count, whether every tick walks the whole tree
type BenchCase = (&'static str, fn() -> (Tree, Blackboard), u64, bool);

struct Report {
    name: &'static str,
    nodes: usize,
    /// False when the tree resumes into an active path instead of walking all
    /// of it — `ns/node` is then meaningless and is not reported.
    walks_all: bool,
    ticks: u64,
    elapsed_ns: u64,
    p50_ns: u64,
    p99_ns: u64,
    max_ns: u64,
    allocs: u64,
}

impl Report {
    fn ticks_per_sec(&self) -> f64 {
        self.ticks as f64 / (self.elapsed_ns as f64 / 1e9)
    }
    fn ns_per_node(&self) -> Option<f64> {
        self.walks_all
            .then(|| self.elapsed_ns as f64 / (self.ticks as f64 * self.nodes as f64))
    }
    fn ns_per_tick(&self) -> f64 {
        self.elapsed_ns as f64 / self.ticks as f64
    }
}

fn measure(
    name: &'static str,
    tree: &mut Tree,
    bb: &mut Blackboard,
    ticks: u64,
    walks_all: bool,
) -> Report {
    let nodes = tree.graph().len();
    let clock = SimClock::new();
    let mut ctx = Ctx::new(bb, &clock);

    // Warm the branch predictor and any lazy first-tick work.
    let mut warm = Status::Running;
    for _ in 0..1000 {
        warm = tree.tick(&mut ctx);
    }
    let expected = if walks_all {
        Status::Success
    } else {
        Status::Running
    };
    assert_eq!(
        warm, expected,
        "{name}: tree does not have the status this case claims to measure"
    );

    let mut samples: Vec<u64> = Vec::with_capacity(ticks as usize);
    ALLOCS.store(0, Ordering::Relaxed);
    COUNTING.store(true, Ordering::Relaxed);
    let start = Instant::now();
    for _ in 0..ticks {
        let t0 = Instant::now();
        let s = tree.tick(&mut ctx);
        samples.push(t0.elapsed().as_nanos() as u64);
        core::hint::black_box(s);
    }
    let elapsed = start.elapsed();
    COUNTING.store(false, Ordering::Relaxed);
    let allocs = ALLOCS.load(Ordering::Relaxed);

    samples.sort_unstable();
    let pct = |p: f64| samples[((samples.len() as f64 - 1.0) * p) as usize];

    Report {
        name,
        nodes,
        walks_all,
        ticks,
        elapsed_ns: elapsed.as_nanos() as u64,
        p50_ns: pct(0.50),
        p99_ns: pct(0.99),
        max_ns: *samples.last().unwrap(),
        allocs,
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "--stress") {
        let heavy = args.iter().any(|a| a == "--heavy");
        if !stress::main(heavy) {
            std::process::exit(1);
        }
        return;
    }

    if args.iter().any(|a| a == "--jitter") {
        let seconds = args
            .iter()
            .position(|a| a == "--seconds")
            .and_then(|i| args.get(i + 1))
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(3.0);
        jitter::main(seconds);
        return;
    }

    let json = args.iter().any(|a| a == "--json");

    let mut reports = Vec::new();
    let cases: Vec<BenchCase> = vec![
        ("balanced d3 f3", || balanced(3, 3), 200_000, true),
        ("balanced d5 f3", || balanced(5, 3), 20_000, true),
        ("mission x20", || mission(20), 200_000, true),
        ("mission x200", || mission(200), 20_000, true),
        ("resume d32", || resume_chain(32), 200_000, false),
        ("resume d128", || resume_chain(128), 200_000, false),
    ];

    for (name, make, ticks, walks_all) in cases {
        let (mut tree, mut bb) = make();
        reports.push(measure(name, &mut tree, &mut bb, ticks, walks_all));
    }

    if json {
        println!("{{");
        println!("  \"arch\": \"{}\",", std::env::consts::ARCH);
        println!("  \"cases\": [");
        for (i, r) in reports.iter().enumerate() {
            let comma = if i + 1 == reports.len() { "" } else { "," };
            println!(
                "    {{\"name\":\"{}\",\"nodes\":{},\"walks_all\":{},\"ticks\":{},\
                 \"ticks_per_sec\":{:.0},\"ns_per_tick\":{:.1},\"ns_per_node\":{},\
                 \"p50_ns\":{},\"p99_ns\":{},\"max_ns\":{},\"allocs\":{}}}{}",
                r.name,
                r.nodes,
                r.walks_all,
                r.ticks,
                r.ticks_per_sec(),
                r.ns_per_tick(),
                r.ns_per_node()
                    .map_or_else(|| "null".to_string(), |v| format!("{v:.2}")),
                r.p50_ns,
                r.p99_ns,
                r.max_ns,
                r.allocs,
                comma
            );
        }
        println!("  ]");
        println!("}}");
        return;
    }

    println!("BTEng-Rust M0 tick benchmark  [{}]", std::env::consts::ARCH);
    println!(
        "{:<16} {:>6} {:>12} {:>10} {:>9} {:>9} {:>9} {:>9} {:>7}",
        "case", "nodes", "ticks/s", "ns/tick", "ns/node", "p50 ns", "p99 ns", "max ns", "allocs"
    );
    for r in &reports {
        println!(
            "{:<16} {:>6} {:>12.0} {:>10.1} {:>9} {:>9} {:>9} {:>9} {:>7}",
            r.name,
            r.nodes,
            r.ticks_per_sec(),
            r.ns_per_tick(),
            r.ns_per_node()
                .map_or_else(|| "  resume".to_string(), |v| format!("{v:.2}")),
            r.p50_ns,
            r.p99_ns,
            r.max_ns,
            r.allocs
        );
    }

    let bad: Vec<_> = reports.iter().filter(|r| r.allocs != 0).collect();
    if bad.is_empty() {
        println!("\nallocations per tick: 0 across all cases — M0 allocation criterion met");
    } else {
        println!(
            "\nALLOCATION REGRESSION in: {:?}",
            bad.iter().map(|r| r.name).collect::<Vec<_>>()
        );
        std::process::exit(1);
    }
}
