# Status

**`v1.0.0`.** All seven planned milestones (M0–M7) are in. The engine ticks a full
BehaviorTree.CPP-compatible node set with typed ports bound at build time, loads BT.CPP-shaped XML,
runs subtrees at independent rates or on data changes, drives async work without blocking the tick
thread, records a versioned trace, and swaps subtrees live without disturbing what is running beside
them.

Three things are not established, and `1.0.0` does not claim them.

## Not established

- **No ARM hardware run.** CI runs the suite on a native ARM runner, which checks correctness. Timing
  on a Jetson or similar is unmeasured, so DR-4 ("hard-real-time capable") is a design intention, not
  a measurement. Every timing figure is x86-only.
- **No `loom` tests.** Every atomic carries a written `Ordering` justification, but argued is not
  proved. Do this before threaded regions, not after.
- **Nothing has driven a physical machine.** Every test uses a mock device and a simulated clock.
  Porting one real mission is the highest-value next step.

## Other gaps

- Regions are single-threaded, deliberately.
- One structural operation per patch.
- The Python control plane (`crates/bteng-py`) is parked on a branch and not covered by CI.

## Invariants

The rules a change must not break without saying so:

- Zero allocations per tick and per scheduler step.
- Nothing on the tick path blocks.
- Everything fallible happens at build time.
- Every atomic carries a written justification for its `Ordering`.
- `extensions/bteng-devices` keeps compiling untouched.

The commands to check them are under [Checks](#checks).

## Checks

```bash
cargo test --workspace
cargo clippy --workspace --all-targets                    # must be 0 warnings
cargo check -p bteng-core --lib --no-default-features     # the no_std door
cargo check --workspace --target aarch64-unknown-linux-gnu
(cd extensions/bteng-devices && cargo test && cargo clippy --all-targets -- -D warnings)
cargo deny check licenses sources bans
```
