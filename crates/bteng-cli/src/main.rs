//! `bteng` — command-line tools for behavior tree files.
//!
//! ```text
//! bteng validate mission.xml       what is wrong with this document
//! bteng structure mission.xml      the node table, as JSON — what BTviz draws from
//! bteng models                     the node palette: every built-in and its ports
//! bteng run mission.xml --hz 50 --seconds 5 --trace run.jsonl
//! ```
//!
//! # What a CLI can and cannot do
//!
//! It only knows the built-in nodes. It cannot construct your `NavigateTo`,
//! because that lives in your binary — so `run` works for trees made of
//! built-ins, and for everything else `validate` and `structure` are the useful
//! commands: they answer "is this document well-formed, and which node types
//! would my program have to provide?" without needing the program.

use std::process::ExitCode;

use bteng_core::{Blackboard, NodeRegistry, Tree};
use bteng_sched::{
    Pacer, RealTime, RunOutcome, Scheduler, SleepStrategy, StepOutcome, VirtualTime,
};
use bteng_telemetry::{Recorder, RegionReport, to_jsonl};
use bteng_xml::{XmlLoader, unknown_node_types};

const USAGE: &str = "\
bteng — behavior tree tools

USAGE:
    bteng validate <file.xml> [--strict]   --strict: fail if a node type is not built in
    bteng structure <file.xml>
    bteng models
    bteng run <file.xml> [OPTIONS]

RUN OPTIONS:
    --hz <rate>        root tick rate (default 10)
    --seconds <n>      stop after this long (default 1; 0 = until settled)
    --tree <id>        load a tree other than main_tree_to_execute
    --trace <file>     write a JSON Lines trace
    --virtual          run in simulated time: as fast as possible, deterministic
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first() else {
        eprint!("{USAGE}");
        return ExitCode::FAILURE;
    };

    let result = match cmd.as_str() {
        "validate" => cmd_validate(&args),
        "structure" => cmd_structure(&args),
        "models" => cmd_models(),
        "run" => cmd_run(&args),
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            Ok(())
        }
        other => Err(format!("unknown command {other:?}\n\n{USAGE}")),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("bteng: {msg}");
            ExitCode::FAILURE
        }
    }
}

fn read(args: &[String]) -> Result<String, String> {
    let path = args.get(1).ok_or("expected a file path")?;
    std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))
}

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

fn has(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

fn cmd_validate(args: &[String]) -> Result<(), String> {
    let xml = read(args)?;
    let registry = NodeRegistry::with_builtins();

    // Reported first, and as information rather than failure: a mission tree
    // naturally names nodes that live in the robot's binary, not here. Listing
    // them all in one pass beats failing at the first.
    let unknown = unknown_node_types(&xml, &registry).map_err(|e| e.to_string())?;
    if !unknown.is_empty() {
        println!("node types not built in ({}):", unknown.len());
        for t in &unknown {
            println!("  {t}");
        }
        println!("\nStructure below is checked with those types absent.");
        if has(args, "--strict") {
            return Err(format!("{} node type(s) not built in", unknown.len()));
        }
    }

    let loader = XmlLoader::new(&registry);
    let ids = loader.tree_ids(&xml).map_err(|e| e.to_string())?;
    println!("trees: {}", ids.join(", "));

    if unknown.is_empty() {
        let mut bb = Blackboard::new();
        let graph = loader.load(&xml, &mut bb).map_err(|e| e.to_string())?;
        println!("ok — {} nodes, {} blackboard slots", graph.len(), bb.len());
    }
    Ok(())
}

fn cmd_structure(args: &[String]) -> Result<(), String> {
    let xml = read(args)?;
    let registry = NodeRegistry::with_builtins();
    let mut bb = Blackboard::new();
    let graph = XmlLoader::new(&registry)
        .load(&xml, &mut bb)
        .map_err(|e| e.to_string())?;

    let mut rec = Recorder::with_capacity(1);
    rec.structure(&graph, &bb);
    print!("{}", to_jsonl(&rec.finish()));
    Ok(())
}

fn cmd_models() -> Result<(), String> {
    print!("{}", NodeRegistry::with_builtins().export_node_models_xml());
    Ok(())
}

fn cmd_run(args: &[String]) -> Result<(), String> {
    let xml = read(args)?;
    let registry = NodeRegistry::with_builtins();
    let mut bb = Blackboard::new();
    let loader = XmlLoader::new(&registry);

    let graph = match flag(args, "--tree") {
        Some(id) => loader.load_tree(&xml, id, &mut bb),
        None => loader.load(&xml, &mut bb),
    }
    .map_err(|e| e.to_string())?;

    let hz: f64 = flag(args, "--hz")
        .unwrap_or("10")
        .parse()
        .map_err(|_| "--hz expects a number")?;
    let seconds: f64 = flag(args, "--seconds")
        .unwrap_or("1")
        .parse()
        .map_err(|_| "--seconds expects a number")?;
    let budget = (seconds * 1e9) as u64;

    let mut sched = Scheduler::new(Tree::new(graph), &bb, hz).map_err(|e| e.to_string())?;
    let trace_path = flag(args, "--trace").map(str::to_string);
    let mut rec = trace_path
        .as_ref()
        .map(|_| Recorder::with_capacity(1 << 16));
    if let Some(r) = &mut rec {
        r.structure(sched.tree().graph(), &bb);
    }

    let outcome = if has(args, "--virtual") {
        drive(
            &mut sched,
            &mut bb,
            &VirtualTime::new(),
            budget,
            rec.as_mut(),
        )
    } else {
        drive(
            &mut sched,
            &mut bb,
            &RealTime::new(bteng_core::SteadyClock::new(), SleepStrategy::hybrid()),
            budget,
            rec.as_mut(),
        )
    };

    println!("{outcome}");
    for i in 0..sched.region_count() {
        let s = sched.stats(i).expect("region exists");
        println!(
            "  region {i}: {} ticks, mean late {:.1} us, max late {:.1} us, {} lost",
            s.ticks,
            s.mean_lateness() / 1e3,
            s.max_lateness as f64 / 1e3,
            s.overruns
        );
    }

    if let (Some(path), Some(r)) = (trace_path, rec) {
        let dropped = r.dropped();
        std::fs::write(&path, to_jsonl(&r.finish())).map_err(|e| format!("{path}: {e}"))?;
        println!("trace written to {path}");
        if dropped > 0 {
            println!("  (buffer was full: {dropped} events dropped, recorded as a gap)");
        }
    }
    Ok(())
}

/// The run loop, spelled out rather than using `run_for`, because the trace has
/// to see each region tick as it happens.
fn drive(
    sched: &mut Scheduler,
    bb: &mut Blackboard,
    pacer: &impl Pacer,
    budget: u64,
    mut rec: Option<&mut Recorder>,
) -> String {
    let start = pacer.now();
    sched.arm(start);
    let limit = if budget == 0 {
        u64::MAX
    } else {
        start.saturating_add(budget)
    };

    loop {
        if let Some(status) = sched.settled() {
            if let Some(r) = rec.as_deref_mut() {
                r.run_end(pacer.now(), status);
            }
            return format!(
                "settled: {status:?} at {} ms",
                (pacer.now() - start) / 1_000_000
            );
        }
        match sched.next_wake(bb) {
            None => return "nothing to schedule".to_string(),
            Some(next) if next >= limit => {
                return format!("time up at {} ms", (pacer.now() - start) / 1_000_000);
            }
            Some(_) => {}
        }

        let at = pacer.now();
        let outcome = match rec.as_deref_mut() {
            Some(r) => sched.step_traced(bb, pacer, Some(r)),
            None => sched.step(bb, pacer),
        };
        if let (
            StepOutcome::Ticked {
                region,
                status,
                lateness,
            },
            Some(r),
        ) = (outcome, rec.as_deref_mut())
        {
            r.region(RegionReport {
                at,
                region,
                status,
                lateness_ns: lateness,
            });
        }
        if let StepOutcome::Finished { .. } = outcome {
            let _ = RunOutcome::TimeUp { at };
            return "finished".to_string();
        }
    }
}
