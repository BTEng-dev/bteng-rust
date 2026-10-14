# M1 — core node library, typed ports, XML

**Exit criterion met:** a BehaviorTree.CPP-shaped XML document loads, validates and ticks; the
extension API is exercised by nodes written against the public surface only.

72 tests green. Still **0 allocations per tick**; ~25 ns/node on the same benchmark shapes as M0, so
nothing in this milestone cost the representation anything.

---

## What landed

| | |
|---|---|
| **Typed ports** | Declared per node *type* in a `static [PortSpec]`; bound to a `SlotId` at build time with the type checked there. A tick reads a port with an array index and a `TypeId` compare — no name, no hashing, no scope chain. |
| **Node library** | 21 built-ins: `Sequence`, `SequenceWithMemory`, `ReactiveSequence`, `Fallback`, `ReactiveFallback`, `Parallel`, `IfThenElse`, `WhileDoElse`, `Inverter`, `ForceSuccess`, `ForceFailure`, `KeepRunningUntilFailure`, `Repeat`, `RetryUntilSuccessful`, `RunOnce`, `Timeout`, `Delay`, `SubTree`, `AlwaysSuccess`, `AlwaysFailure`, `Sleep`, plus function-pointer `Action`/`Condition` leaves. |
| **Registry** | `NodeRegistry` — a value you own, not a global singleton (D6). Carries manifests and constructors; exports a `<TreeNodesModel>` palette. |
| **Lifecycle** | `setup`/`shutdown` walks, collecting *every* setup failure rather than aborting at the first. |
| **XML front end** | `bteng-xml`: BT.CPP v4 and v3 element spellings, Python BTEng's `<BTEng>`/`<Tree>` tags, node aliases, `{key}` ports, `<SubTree>` with remapping and `_autoremap`. |

## Decisions taken during M1

**Port literals are blackboard slots.** `<Wait msec="250"/>` does not become a separate "static
parameter" lookup the way it does in BT.CPP and Python BTEng. The literal is parsed at build time
into an anonymous slot, so at tick time there is exactly one way to read a port. One path is one set
of bugs, and it means a literal and a `{key}` are indistinguishable to the node — which is what makes
retargeting a port at reconfiguration time a slot swap rather than a code path.

**An unwired output port gets a sink slot.** A node that writes to a port the tree never wired has to
write *somewhere*, or every `set` needs a branch on the tick path. It writes to a slot nobody reads.

**Four build-time rejections that Python BTEng accepts silently**, each because the silent version is
invisible rather than merely wrong:

- a value for a port the node type does not declare (a typo makes the node run on its default while
  the tree source *looks* configured);
- a literal on an output-only port (writes into nothing);
- a `{key}` whose slot already holds another type;
- a readable port with neither a value nor a default. `Timeout`'s `msec` has no default on purpose —
  silently defaulting a timeout is worse than refusing to build.

**Subtree isolation is key rewriting, done at load time.** Each `<SubTree>` instance gets a
namespace, so `{count}` inside `dock` becomes the key `dock#1/count` and two instances cannot collide.
Attributes map keys outward; `_autoremap="true"` passes everything through. Python BTEng walks a scope
chain on every access to achieve the same thing; here the built tree contains no scopes at all.

**Subtrees are inlined behind a marker node.** The `SubTree` built-in is a transparent decorator, so
the boundary survives into the arena for telemetry and BTviz even though the nodes themselves are
spliced in. Cycles (`a` includes `b` includes `a`) are rejected at load.

**Hot/cold split in the arena.** `NodeStruct` (child and port ranges) dropped from 24 bytes to 12 by
moving uid, name, parent and manifest into a parallel `NodeMeta` array that no tick touches. Both
sizes have a regression guard in the test suite.

## Two deliberate deviations from BehaviorTree.CPP

Both are documented at the node and both are DR-4 consequences.

1. **`Repeat` and `RetryUntilSuccessful` complete at most one child cycle per tick.** BT.CPP loops
   inside a single tick until the child returns `RUNNING`, which makes the cost of one tick depend on
   how fast the child settles — and unbounded if the child settles instantly. A tree observes the
   same behaviour; it just takes as many ticks as cycles.
2. **`RunOnce` has no `Skipped` status to return**, so `then_skip="false"` re-runs the child rather
   than reporting BT.CPP's `SKIPPED`. Adding a fourth status that every control node would have to
   interpret is not worth it for one node.

## Correcting an M0 note

M0's `builtins.rs` said the reactive control nodes were deferred because they would mean shipping
Python BTEng's global dirty flag under a better name. That conflated two things. BT.CPP's reactive
*semantics* — re-tick every child from the first, every tick — are well defined and need nothing
extra, and they are implemented here. What M4 adds is the *optimisation*: skipping children whose
inputs did not change, which needs read sets to do precisely. The dirty flag was the wrong
optimisation, not the wrong semantics.

## Carried into M2

- Custom nodes are constructed fresh from the registry each time, so a `<SubTree>` used twice gets two
  independent instances. That is correct, but it means a node type cannot currently share state
  between instances by construction — fine for now, worth revisiting when async handles arrive.
- `Parallel` remains capped at 64 children by its completion bitset; the builder rejects more.
- The `SubTree` marker adds one node per instance. Free at tick time (`SubTree` is a passthrough),
  but it does count against `Parallel`'s child budget if someone nests oddly.
- No `Script`/`Precondition` node: BT.CPP's version needs an expression language. Out of scope until
  there is a reason.
- Still no aarch64 hardware. CI compiles and runs the suite under QEMU; timing numbers wait for M2.
