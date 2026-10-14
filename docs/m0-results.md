# M0 — representation spike: results

M0 exists to answer one question before anything is built on top of it:

> Is the arena tree + slot blackboard representation fast and allocation-free?

**Verdict: go.** Zero allocations per tick across every shape, ~24 ns per node
visited on x86-64, and ~33–45× Python BTEng on identical tree shapes.

Measured 2026-08-02. x86-64 Linux, 20 cores, `--release` (`lto = "thin"`,
`codegen-units = 1`). Python side: BTEng 0.3.1 on CPython 3.12.3, driving
`tick_once()` directly with no executor sleep, tracer, or inspector — engine to
engine, not loop to loop.

Reproduce:

```bash
cargo run --release -p bteng-bench
PYTHONPATH=~/bteng-dev/bteng python3 bench/python_baseline.py
```

Raw output is in `bench-results/`.

---

## Full-traversal shapes

Every tick settles `Success`, so every tick walks the whole tree and `ns/node`
means what it says.

| case | nodes | Rust ticks/s | Python ticks/s | speedup | Rust ns/node | Python ns/node |
|---|---:|---:|---:|---:|---:|---:|
| balanced d3 f3 | 68 | 506,512 | 15,536 | **32.6×** | 29.0 | 946.6 |
| balanced d5 f3 | 608 | 66,175 | 1,745 | **37.9×** | 24.9 | 942.4 |
| mission x20 | 101 | 398,624 | 9,058 | **44.0×** | 24.8 | 1093.0 |
| mission x200 | 1001 | 41,939 | 941 | **44.6×** | 23.8 | 1062.0 |

Tail latency, `mission x200`:

| | p50 | p99 | max |
|---|---:|---:|---:|
| Rust | 23.5 µs | 29.2 µs | 669 µs |
| Python | 1.05 ms | 1.36 ms | 1.77 ms |

The Rust `max` figures (170–670 µs) are OS scheduling noise on a desktop, not
engine behaviour — the p99/p50 ratio is 1.24 while max/p50 is 28. This is
exactly why DR-2 puts jitter measurement on real Jetson hardware rather than
trusting a developer machine or QEMU.

## Resume shapes

A long chain parked on a `Running` leaf. A plain `Sequence` resumes at its
running child, so these ticks re-enter only the active path: cost tracks path
depth, not tree size, and `ns/node` would be meaningless. Reported as ns/tick.

| case | nodes | ticks/s | ns/tick | p99 |
|---|---:|---:|---:|---:|
| resume d32 | 65 | 1,447,328 | 691 | 706 ns |
| resume d128 | 257 | 384,930 | 2,598 | 2,990 ns |

This distinction was itself an M0 finding: the first version of the harness
reported *0.20 ns/node for a 609-node tree* because the tree parked on a
`Running` leaf after tick one and never walked itself again. A benchmark that
does not assert what the tree is doing will happily measure nothing.

## Allocation

`0` allocations per tick in every case, verified two independent ways:

- `crates/bteng-core/tests/no_alloc.rs` — thread-local counting allocator around
  1000 steady-state ticks, 1000 blackboard writes, and 100 halt/tick cycles.
- `bteng-bench` — global counting allocator around each measured run; the binary
  exits non-zero if any case allocates, so CI gates on it.

The two places this could have gone wrong, and did not:

- `Blackboard::set` assigns through `downcast_mut` instead of replacing the
  `Box`. Replacing it is the obvious implementation and would have put a malloc
  on the hot path of every tick that publishes a value.
- `take_behavior`/`put_behavior` are `mem::replace` of a small enum, not a
  reallocation. `NodeKind` is 24 bytes and `NodeStruct` is 20; both have a size
  guard in the test suite, since both are moved or walked per node visit.

## Portability

| check | status |
|---|---|
| `cargo check -p bteng-core --lib --no-default-features` (no_std) | passes |
| `cargo check --target aarch64-unknown-linux-gnu` | passes |
| aarch64 test run under QEMU | wired in CI; **not yet run on real ARM hardware** |

No aarch64 machine was available for this milestone, so the ARM numbers do not
exist yet — only correctness and allocation counts carry over from emulation.
Real jitter figures are an M2 exit criterion.

---

## What M0 establishes

1. The arena representation works, including the borrow discipline. A control
   node ticks children that live in the same arena by lifting its own behaviour
   out for the duration (`take_behavior`), so nothing holds a `&mut` across a
   child tick. 64-level nesting is covered by test.
2. Zero-allocation ticking is real, not aspirational — which is the one
   constraint that keeps the hard-real-time option open (DR-4).
3. Build-time port binding removes string hashing and scope-chain walking from
   the tick entirely.
4. The ~40× headline is honest: same shapes, same semantics, same
   settle-every-tick condition, asserted before measurement.

## What M0 does not establish

- Nothing about async, multi-rate scheduling, event-driven wakeups, or
  reconfiguration. None of it exists yet.
- Nothing about custom `Box<dyn Node>` leaves under load — every benchmark leaf
  is a `fn` pointer, so the numbers measure the engine, not user code.
- Nothing about ARM performance.
- `Parallel` is capped at 64 children by its completion bitset. The builder
  rejects more rather than silently losing track.

## Open items carried into M1

- `Sequence`/`Fallback` halt all children on completion (BT.CPP resets
  children), so a settling tree visits each node about twice per tick. That cost
  is included in the numbers above. Worth revisiting whether the reset can be
  lazy.
- `NodeKind::Custom` arity is unchecked until the M1 registry carries node
  manifests.
- `Uid` is assigned sequentially by the builder. When patches arrive it must
  become a counter that never reuses a value, so telemetry can distinguish
  "same node, moved" from "new node".
