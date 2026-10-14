//! Scheduler jitter measurement.
//!
//! Answers one question, per target: **how late does a region actually start?**
//!
//! Lateness is measured from each *absolute deadline*, not from the previous
//! tick. That distinction is the whole point — a loop that sleeps a period
//! between ticks reports a perfect average rate while its phase walks steadily
//! away from where it was asked to be. Distance from the grid catches that; the
//! gap between ticks does not.
//!
//! ```text
//! cargo run --release -p bteng-bench -- --jitter
//! cargo run --release -p bteng-bench -- --jitter --seconds 30
//! ```
//!
//! This is the measurement DR-4 defers the hard/soft real-time decision *to*.
//! Run it on the target that matters — a desktop under a browser tells you
//! almost nothing about a Jetson with the workload pinned.

use std::time::Duration;

use bteng_core::{Blackboard, Builtin, Ctx, Status, SteadyClock, Tree, TreeBuilder};
use bteng_sched::{Pacer, RealTime, RegionStats, Scheduler, SleepStrategy};

/// A leaf that burns a fixed amount of CPU, so a region has real work to do.
///
/// Spinning rather than sleeping on purpose: sleeping would hand the CPU back
/// and measure the OS's generosity instead of the scheduler's discipline.
fn busy_work(_c: &mut Ctx<'_>) -> Status {
    let mut acc = 0u64;
    for i in 0..2000u64 {
        acc = acc.wrapping_add(i.wrapping_mul(2_654_435_761));
    }
    core::hint::black_box(acc);
    Status::Running
}

fn idle_work(_c: &mut Ctx<'_>) -> Status {
    Status::Running
}

/// 10 Hz root beside a `fast_hz` region, both with work to do.
fn two_rate_tree(fast_hz: f64) -> (Scheduler, Blackboard) {
    let mut b = TreeBuilder::new();
    // Parallel, not Sequence: a region publishes Running until its subtree
    // settles, and a Sequence parent would stop there and never reach the
    // sibling.
    b.begin("root", Builtin::parallel())
        .begin("fast", Builtin::region())
        .port("hz", &format!("{fast_hz}"))
        .port("overrun", "warn")
        .leaf("fast_work", Builtin::action(busy_work))
        .end()
        .leaf("slow_work", Builtin::action(idle_work))
        .end();

    let mut bb = Blackboard::new();
    let graph = b.build(&mut bb).expect("jitter tree builds");
    let sched = Scheduler::new(Tree::new(graph), &bb, 10.0).expect("scheduler builds");
    (sched, bb)
}

struct Row {
    label: String,
    region: &'static str,
    hz: f64,
    stats: RegionStats,
    wall_ns: u64,
}

fn run_case(label: &str, fast_hz: f64, strategy: SleepStrategy, seconds: f64) -> Vec<Row> {
    let (mut sched, mut bb) = two_rate_tree(fast_hz);
    let pacer = RealTime::new(SteadyClock::new(), strategy);
    let budget = (seconds * 1e9) as u64;

    let t0 = pacer.now();
    let _ = sched.run_for(&mut bb, &pacer, budget, 0);
    let wall_ns = pacer.now() - t0;

    vec![
        Row {
            label: label.to_string(),
            region: "root",
            hz: 10.0,
            stats: *sched.stats(0).unwrap(),
            wall_ns,
        },
        Row {
            label: label.to_string(),
            region: "fast",
            hz: fast_hz,
            stats: *sched.stats(1).unwrap(),
            wall_ns,
        },
    ]
}

fn print_rows(rows: &[Row]) {
    println!(
        "{:<14} {:<6} {:>7} {:>7} {:>10} {:>10} {:>10} {:>10} {:>9}",
        "strategy",
        "region",
        "asked",
        "got",
        "mean late",
        "p50 late",
        "p99 late",
        "max late",
        "overrun%"
    );
    for r in rows {
        let achieved = if r.wall_ns == 0 {
            0.0
        } else {
            r.stats.ticks as f64 / (r.wall_ns as f64 / 1e9)
        };
        println!(
            "{:<14} {:<6} {:>7.1} {:>7.1} {:>10} {:>10} {:>10} {:>10} {:>8.2}%",
            r.label,
            r.region,
            r.hz,
            achieved,
            fmt_ns(r.stats.mean_lateness() as u64),
            fmt_ns(r.stats.lateness.quantile(0.50)),
            fmt_ns(r.stats.lateness.quantile(0.99)),
            fmt_ns(r.stats.max_lateness),
            r.stats.overrun_rate() * 100.0,
        );
    }
}

fn fmt_ns(ns: u64) -> String {
    if ns < 1_000 {
        format!("{ns}ns")
    } else if ns < 1_000_000 {
        format!("{:.1}us", ns as f64 / 1e3)
    } else {
        format!("{:.2}ms", ns as f64 / 1e6)
    }
}

/// Run the jitter suite and print a report.
pub fn main(seconds: f64) {
    println!(
        "BTEng-Rust scheduler jitter  [{}]  {seconds:.0}s per case",
        std::env::consts::ARCH
    );
    println!(
        "lateness = actual start - absolute deadline. p50/p99 are log2-bucketed \
         upper bounds,\nso they over-state rather than flatter.\n"
    );

    let mut rows = Vec::new();
    rows.extend(run_case("Yield", 100.0, SleepStrategy::Yield, seconds));
    rows.extend(run_case(
        "SpinTail 200us",
        100.0,
        SleepStrategy::hybrid(),
        seconds,
    ));
    rows.extend(run_case(
        "Yield @1kHz",
        1000.0,
        SleepStrategy::Yield,
        seconds,
    ));
    rows.extend(run_case(
        "SpinTail @1kHz",
        1000.0,
        SleepStrategy::SpinTail {
            slack: Duration::from_micros(200),
        },
        seconds,
    ));

    print_rows(&rows);

    println!(
        "\nRead this on the target you care about. A developer desktop running a browser is not\n\
         a Jetson with the workload pinned, and neither is QEMU — under emulation only the\n\
         correctness of the run carries over, never the timing."
    );
}
