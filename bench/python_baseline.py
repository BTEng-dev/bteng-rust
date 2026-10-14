#!/usr/bin/env python3
"""Python BTEng baseline for the M0 comparison.

Builds the same tree shapes as `crates/bteng-bench` and measures raw
`tick_once()` throughput — no executor sleep, no tracer, no inspector, so the
comparison is engine-to-engine rather than loop-to-loop.

    python3 bench/python_baseline.py                 # uses the installed bteng
    PYTHONPATH=~/bteng-dev/bteng python3 bench/python_baseline.py

Both shapes settle SUCCESS every tick, so every tick walks the whole tree — the
same condition the Rust harness asserts before it measures.
"""

from __future__ import annotations

import argparse
import json
import statistics
import sys
import time

try:
    from bteng import NodeStatus, TreeBuilder
except ImportError:  # pragma: no cover
    sys.exit("bteng not importable; try PYTHONPATH=~/bteng-dev/bteng")


def _count(tree) -> int:
    n = 0

    def visit(_node):
        nonlocal n
        n += 1

    tree.visit(visit)
    return n


def balanced(depth: int, fanout: int):
    b = TreeBuilder().tree_id("balanced").sequence("root")

    def rec(d: int) -> None:
        if d == 0:
            b.condition("guard", lambda: True)
            b.action("work", lambda: NodeStatus.SUCCESS)
            return
        b.sequence("seq")
        for _ in range(fanout):
            rec(d - 1)
        b.end()

    rec(depth)
    return b.end().build()


def mission(branches: int):
    b = TreeBuilder().tree_id("mission").sequence("root")
    for _ in range(branches):
        b.sequence("branch")
        b.condition("precondition", lambda: True)
        b.fallback("guarded")
        b.action("try", lambda: NodeStatus.FAILURE)
        b.action("recover", lambda: NodeStatus.SUCCESS)
        b.end()
        b.end()
    return b.end().build()


def measure(name: str, tree, ticks: int) -> dict:
    nodes = _count(tree)

    for _ in range(200):  # warm-up
        tree.tick_once()

    samples = []
    t_start = time.perf_counter_ns()
    for _ in range(ticks):
        t0 = time.perf_counter_ns()
        status = tree.tick_once()
        samples.append(time.perf_counter_ns() - t0)
    elapsed = time.perf_counter_ns() - t_start

    if status is not NodeStatus.SUCCESS:
        sys.exit(f"{name}: expected SUCCESS every tick, got {status}")

    samples.sort()
    pct = lambda p: samples[int((len(samples) - 1) * p)]  # noqa: E731
    return {
        "name": name,
        "nodes": nodes,
        "ticks": ticks,
        "ticks_per_sec": ticks / (elapsed / 1e9),
        "ns_per_tick": elapsed / ticks,
        "ns_per_node": elapsed / (ticks * nodes),
        "p50_ns": pct(0.50),
        "p99_ns": pct(0.99),
        "max_ns": samples[-1],
    }


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--json", action="store_true")
    ap.add_argument("--scale", type=float, default=1.0,
                    help="scale tick counts (Python is slow; default keeps it under a minute)")
    args = ap.parse_args()

    cases = [
        ("balanced d3 f3", lambda: balanced(3, 3), 20_000),
        ("balanced d5 f3", lambda: balanced(5, 3), 2_000),
        ("mission x20", lambda: mission(20), 20_000),
        ("mission x200", lambda: mission(200), 2_000),
    ]

    reports = [measure(n, make(), max(1, int(t * args.scale))) for n, make, t in cases]

    if args.json:
        import bteng

        print(json.dumps({"engine": "python", "bteng": bteng.__version__,
                          "python": sys.version.split()[0], "cases": reports}, indent=2))
        return

    import bteng

    print(f"Python BTEng {bteng.__version__} baseline  [python {sys.version.split()[0]}]")
    print(f"{'case':<16}{'nodes':>7}{'ticks/s':>12}{'ns/tick':>12}{'ns/node':>10}"
          f"{'p50 ns':>10}{'p99 ns':>10}{'max ns':>10}")
    for r in reports:
        print(f"{r['name']:<16}{r['nodes']:>7}{r['ticks_per_sec']:>12.0f}"
              f"{r['ns_per_tick']:>12.0f}{r['ns_per_node']:>10.1f}"
              f"{r['p50_ns']:>10}{r['p99_ns']:>10}{r['max_ns']:>10}")


if __name__ == "__main__":
    main()
