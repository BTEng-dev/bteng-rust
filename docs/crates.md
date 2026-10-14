# Crates

| Crate | What it owns |
|-------|--------------|
| `bteng-core` | Arena tree, slot blackboard, node library, typed ports, registry, lifecycle, patching |
| `bteng-sched` | Regions, absolute-deadline pacing, overrun policy, event-driven wake-ups, stats |
| `bteng-async` | Spawners, task handles, non-blocking halt, wake-on-completion |
| `bteng-xml` | BehaviorTree.CPP-compatible XML, subtree inlining, key remapping |
| `bteng-telemetry` | Versioned trace schema, bounded recorder, `Timeline` — what visualizers read |
| `bteng-cli` | `bteng validate / structure / models / run` |
| `bteng-bench` | Dependency-free benchmark harness; also the allocation gate |
| `extensions/bteng-devices` | Device I/O, built **outside** the workspace against the public API only |

`extensions/bteng-devices` is the proof that the public API is enough to extend the engine (DR-6). It
must keep compiling untouched.

`bench/python_baseline.py` runs the same tree shapes on Python BTEng, for the comparison in
[Performance](performance.md).

## Trace schema

`bteng-telemetry` writes JSON Lines. Each line has a `t` field: `structure`, `node`, `region`,
`run_end`, `patch` or `dropped`. The schema version is bumped whenever a consumer of the JSON would
have to change, so a consumer should refuse a version it does not know. Port bindings are included
from schema 3: a `key` for a blackboard binding, a `value` for a literal.
