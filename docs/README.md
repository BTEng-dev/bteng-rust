# BTEng Rust

<p align="center">
  <img src="images/BTEng-Rust.webp" alt="BTEng Rust — Behavior Tree Engine" width="640">
</p>

**A behavior tree engine for robotics and tick-driven control, written in Rust.**

Clean-room: concepts come from the Python BTEng and BehaviorTree.CPP, no code does.

---

## Start here

1. [Getting started](getting-started.md) — install, run the CLI, build a first tree
2. [Crates](crates.md) — what each crate owns
3. [Status](status.md) — what is established, what is not

## Guides

| Topic | Page |
|-------|------|
| Subtrees on their own clocks, wake on change | [Multi-rate ticking](multi-rate.md) |
| Swap a subtree while a sibling is mid-run | [Live reconfiguration](live-reconfiguration.md) |
| Throughput, allocation gate, jitter, targets | [Performance](performance.md) |

## Design notes

The decisions behind the engine, and what each milestone established.

| Page | What it holds |
|------|---------------|
| [M0 results](m0-results.md) | Benchmarks, and what they do and do not establish |
| [M1 notes](m1-notes.md) | Ports, node library, XML |
| [M2 notes](m2-notes.md) | Regions, absolute deadlines, overrun policy, measured jitter |
| [M3–M5 notes](m3-m5-notes.md) | Async leaves, event-driven regions, trace schema |
| [M6–M7 notes](m6-m7-notes.md) | The extension proof, live reconfiguration |

## Project

| Topic | Page |
|-------|------|
| Dependency licenses and their history | [`THIRD-PARTY-LICENSES.md`](../THIRD-PARTY-LICENSES.md) |
