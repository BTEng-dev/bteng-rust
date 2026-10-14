# M2 — the multi-rate scheduler

**Exit criterion met, with one caveat.** Two subtrees at 10 Hz and 100 Hz in one tree, each holding
its own rate; jitter measured and reported. The caveat is that the numbers are x86-64 only — there is
still no ARM hardware, and QEMU timing is not evidence of anything.

90 tests green. Scheduler stepping allocates nothing.

---

## What a region is

A **region** is a subtree with its own rate, marked by a `Region` node. That node is a *scheduling
boundary*: the parent's traversal stops there and reads the last verdict the region published, while
the scheduler ticks the region's subtree directly, on the region's own clock.

```text
  root  ──── 10 Hz ────────●───────────────●───────────────●
    └─ Region(100 Hz) ─●─●─●─●─●─●─●─●─●─●─●─●─●─●─●─●─●─●─●
```

This is the specific thing Python BTEng's `RateController` cannot do. That decorator ticks its child
*inside the parent's tick*, so the parent's rate is a hard ceiling — a 100 Hz subtree under a 10 Hz
parent runs at 10 Hz. Publishing a verdict and letting the scheduler drive the subtree is what breaks
that coupling. A test asserts the 10:100 split exactly.

Nesting works: an outer region's traversal stops at an inner region's boundary the same way, and the
inner rate is bounded by neither parent.

### The trap this creates, and why it stays

A region publishes `Running` until its subtree settles. A `Sequence` parent therefore **stops at the
region and never reaches its later children**. That is ordinary BT semantics applied to a node that
is deliberately slow to settle, not a bug — but it caught three of my own test fixtures before it
caught anything else, so it is worth stating plainly: put a region under a `Parallel`, under a
`ReactiveSequence`, or last in a `Sequence`.

## Absolute deadlines

The next deadline is computed from the *previous deadline*, never from "now":

```rust
let mut next = r.deadline.saturating_add(period);
```

Python BTEng's loop is `do_tick(); sleep(interval)`, so every tick's duration is added to the period,
permanently. A test injects 3 ms of simulated work into a 100 Hz region and asserts the 20th tick
starts at exactly 190 ms. Under `sleep(period)` pacing it would start at 260 ms — an effective 77 Hz
for a region that asked for 100, and drifting further every second.

## Overrun policy

A real-time system has to state what a missed deadline means. Python BTEng states nothing, which in
practice is unbounded catch-up: one slow tick silently becomes a burst that causes the next one.

| policy | behaviour |
|---|---|
| `skip` (default) | Drop the missed ticks, re-align to the grid. Right for control — a stale command is worse than none. |
| `warn` | As `skip`, counted separately so a run can be audited. |
| `catch_up[:N]` | Fire up to N missed ticks back-to-back, then re-align. For integrators, where the count of ticks is the point. |
| `abort` | Stop the run and say which region, how late, and against which deadline. |

Set per region: `<Region hz="100" overrun="catch_up:2">`. An unrecognised spelling is an **error**,
not a fallback to `skip` — a typo silently changing a real-time policy is exactly the class of
failure this engine keeps refusing to ship.

**An overrun is a lost tick, not a late one.** The first version counted any lateness at all, and the
jitter report came back reading `overrun% = 100.00` on a run whose worst lateness was 158 ns. True,
and useless. Sub-period lateness is what the lateness histogram is for.

## Measured jitter (x86-64, 5 s per case)

Desktop, 20 cores, nothing pinned, browser running. Lateness is distance from the absolute deadline;
p50/p99 are log2-bucketed **upper** bounds, so they over-state rather than flatter.

| strategy | region | asked | got | mean late | p99 late | max late | lost ticks |
|---|---|---:|---:|---:|---:|---:|---:|
| `Yield` | root | 10 | 10.0 | 213 µs | 524 µs | 404 µs | 0.00% |
| `Yield` | fast | 100 | 100.2 | 221 µs | 524 µs | 411 µs | 0.00% |
| `SpinTail 200µs` | root | 10 | 10.0 | 31 µs | 262 µs | 160 µs | 0.00% |
| `SpinTail 200µs` | fast | 100 | 100.2 | 33 µs | 262 µs | 182 µs | 0.00% |
| `Yield` | fast | 1000 | 1000.0 | 185 µs | 524 µs | 1.28 ms | 0.02% |
| `SpinTail 200µs` | fast | 1000 | 999.6 | 5.8 µs | 131 µs | 2.81 ms | 0.04% |

What this says:

- **Rate is held.** 100.0–100.2 Hz and 999.6–1000.0 Hz against asked, on an unpinned desktop.
- **Sleep strategy is the dominant term.** Handing the whole wait to the OS costs ~200 µs of mean
  lateness — that is the scheduler's wake-up granularity, not the engine's. Spinning the last 200 µs
  takes the mean to 5.8 µs at 1 kHz, a ~35× improvement, for one core's worth of tail spin.
- **The tail is the OS, not the engine.** `max late` of 1–3 ms with `mean` at 5.8 µs is a preemption,
  and no amount of scheduler care removes it on a general-purpose kernel with other work running. A
  hard-real-time answer needs `PREEMPT_RT`, CPU isolation and thread pinning — none of which this
  engine can supply for you, and all of which this harness will measure.

Reproduce: `cargo run --release -p bteng-bench -- --jitter --seconds 30`.

## Determinism

`VirtualTime` jumps the clock instead of sleeping, so a run costs no wall-clock time and produces the
same sequence of region ticks with the same timestamps every time, on every architecture. Every
scheduler test asserts under it, and one test asserts two runs are bit-identical — because M6's
replay depends on that being true and it is cheaper to find out now.

## What is deliberately not here

**Regions do not run on separate threads.** One scheduler thread interleaves them. That is what makes
a run deterministic and lets every region share one `Blackboard` with no locking at all.

Real cross-core parallelism needs per-region blackboards plus a wait-free link between them, and that
link is exactly the kind of code that compiles, passes on x86, and is wrong on aarch64 — x86 is TSO,
ARM is not (DR-2 §3.1). Writing it the week before there is ARM hardware to test it on would be
choosing the worst possible order. The region boundary is already the right seam; the work is
additive.

**Event-driven wake-ups are M4.** `TickPolicy` is periodic-only. `OnChange` needs per-node read sets,
which is what the blackboard's version counters exist for. Adding a variant now that polled every
tick would be a pessimisation wearing the name of an optimisation.

**A region's rate is read once, at construction.** Changing a rate at runtime is reconfiguration, and
belongs with the rest of it. The rate *is* already a port, so `hz="{cruise_rate}"` parses and binds
today; only the scheduler's re-reading of it is missing.

## Assumption to confirm

`MAX_HZ` is **1 kHz**, picked because you said "full gas" rather than answering the timing-class
question. It is a validation bound, not a design constant — nothing in the scheduler is sized by it,
and raising it costs only the honesty of saying what has been measured. The jitter table above is the
evidence for what 1 kHz actually looks like on a desktop; the same table from a Jetson is what would
settle DR-4.

## Carried into M3

- One clock for all regions. Per-region clocks (a sim-time region beside a wall-clock one) are a
  small change to `RegionState`, unneeded until something asks.
- `run_for`'s budget excludes a deadline landing exactly on the boundary, so "1 second at 50 Hz" is
  50 ticks, not 51. Worth knowing when reading the numbers.
- The scheduler owns the `Tree`. When async leaves arrive they will want to wake it, which means the
  region queue needs a second input beside the timer heap — the `Wake` enum from the plan.
