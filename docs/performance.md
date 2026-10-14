# Performance

All figures are **x86-64 only**. Nothing has been measured on ARM hardware.

## Throughput

~33–45× Python BTEng on identical tree shapes, **0 allocations per tick**.

| case | nodes | Rust ticks/s | Python ticks/s | speedup |
|---|---:|---:|---:|---:|
| balanced d5 f3 | 608 | 66,175 | 1,745 | 37.9× |
| mission x200 | 1001 | 41,939 | 941 | 44.6× |

Methodology and caveats: [M0 results](m0-results.md).

## Commands

```bash
cargo run --release -p bteng-bench                      # throughput + allocation gate
cargo run --release -p bteng-bench -- --jitter          # scheduler lateness
cargo run --release -p bteng-bench -- --stress          # soak / adversarial shapes
```

Recorded results are in [`bench-results/`](../bench-results/).

## Stress findings

- **Depth is stack.** Ticking and halting recurse at about 250 bytes per level. A default 8 MB thread
  aborts between 30 000 and 35 000 levels, and a stack overflow is a *process abort*, not an error. So
  `DEFAULT_MAX_DEPTH` is 512 (about 128 KB), enforced by the builder and by patch validation.
- Zero allocations hold at 18 000 nodes, 512 regions, 1M ticks and 500 000 halt/tick cycles.
- A scheduler step is O(regions): 64 and 512 regions differ by about 4.5× for 8× the count.

## Targets

x86-64 Linux today. aarch64 (Jetson, ARM SBCs) is a first-class target: the test suite runs on a
native ARM runner in CI, because weak memory ordering does not forgive atomics that only look right
on x86. That checks correctness, not timing, since shared cloud cores say nothing about jitter.

`no_std` is kept cheap rather than chased: the core is `alloc`-first and builds without `std`.
