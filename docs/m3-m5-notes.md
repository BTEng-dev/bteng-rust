# M3–M5 — async, event-driven scheduling, telemetry

117 tests green. 0 clippy warnings. `no_std` core and aarch64 still build. Allocation discipline holds
on both the tick path and the scheduler loop.

---

## M3 — async leaves

Three properties, each fixing a specific defect in the Python engine.

**1. Completion wakes the scheduler.** A settling task calls a `Waker`, so a result is observed
within scheduler latency rather than within a tick period. Python BTEng polls `future.done()` each
tick, so a task that finished a microsecond after a tick waits a whole period regardless of how fast
the work was.

**2. Halt never blocks.** `halt()` sets a cancel flag and moves the handle aside — the tick thread
does not join, does not sleep, does not wait for anything to unwind. `AsyncActionNode._on_halt` in
Python joins with a **5-second** timeout; `CoroActionNode` waits a grace period and then needs an
orphan guard because a halt can be issued from the very loop thread it is waiting on. A test asserts
halting a deliberately wedged worker returns in under 50 ms.

**3. One mechanism.** `Spawner` covers blocking work and futures; node code is identical either way.
Python has a thread pool *and* an asyncio bridge, with different semantics and different bugs.

### Design notes

- **`Job` is a closure, not a method.** `AsyncWork::start` runs on the tick thread and reads ports;
  the closure it returns captures *values*. Nothing can reach the arena from a worker because the
  closure simply does not have it — enforced by `Send`, not by discipline.
- **The orphan guard is node-local.** A node that was halted refuses to start a second copy until the
  first has unwound. Same protection Python needed, without a global grace period.
- **Everything settles.** A panicking job settles as `Panicked`; a job that returns `Running` settles
  as `Failure`. A leaf that panicked and then sat `Running` forever is the one outcome that must not
  happen, and Python shipped exactly that until it was patched.
- **Cancellation is a flag, not a drop.** Dropping a future is Rust's own cancellation and it is
  abrupt — cleanup that needs to run does not. A polled flag costs one relaxed load.

### No runtime dependency

`bteng-async` has **zero** dependencies. `Spawner` is the seam; `ThreadSpawner` and `InlineSpawner`
ship here. A tokio backend is a small impl in whichever crate already depends on tokio — vendoring
one would put a ~30-crate tree in front of every aarch64 and QEMU build for the sake of an example.
`spawn_future` is in the trait and works today: `ThreadSpawner` drives a future by blocking a worker
on it, via a ~20-line `block_on`.

`InlineSpawner` is not a toy — it makes the async path deterministic, which is what lets scheduler and
replay tests cover it at all.

---

## M4 — event-driven regions

`<Region hz="2" mode="on_change" min_period_ms="100">` ticks when something it reads changes, at most
every 100 ms, and at least every 500 ms.

**Read sets are precise.** A region's read set is the union of the *input* port slots of every node in
its subtree, computed once from the graph, stopping at nested region boundaries. Dirtiness is
`version != last_seen` over that set: O(read set), no locks, no allocation, **no callbacks**.

Python BTEng's `ReactiveSequence` subscribes to every blackboard reachable from its subtree and sets
one global dirty flag on *any* write. A test here asserts that hammering a sibling region's slot fifty
times does not tick this region once.

**Debounce is not optional.** A 1 kHz sensor writing a slot must not tick a 10 Hz region a thousand
times; `min_period_ms` is the floor. Neither is the heartbeat: a region that only ever woke on data
would never notice its own `Timeout` expiring.

**The wake bus carries no payload.** A wake means "look again"; *what* changed is answered by
comparing versions. A missed wake costs latency; a missed version bump would cost correctness — so
the mechanism that can be lossy is the one that is allowed to be. Generation counters close the
classic lost-wakeup window (signal-then-wait is tested).

**Failure mode of forgetting to wire the bus:** an event-driven region falls back to its heartbeat.
Degraded latency, never wrong behaviour.

Measured: an idle event-driven region at a 500 ms heartbeat ticks **twice** in a second while the
periodic root ticks a hundred times.

---

## M5 — telemetry

`bteng-telemetry` is the versioned contract with BTviz (DR-7). `SCHEMA_VERSION` goes up when a
consumer would have to change.

**Structure first.** Every trace opens with a `structure` event carrying the whole node table — uid,
parent, name, type, category, children, and every port with **the slot it resolved to**. Two ports
sharing a slot are wired together, which is the thing a viewer most wants to draw and cannot infer
any other way. Everything after refers to nodes by `uid`, which survives a structural edit.

**Transitions, not ticks.** A node emits an event when its state *changes*. A tree at 100 Hz with
nothing happening produces nothing — the difference between a trace you keep for a whole run and one
you throw away after ten seconds. A test runs 100 ticks of an unchanging tree and asserts fewer than
10 events.

**Halts are recorded.** They are not ticks, so nothing else would report them. Python only hooked
ticks, which is why a node stopped by a `Timeout` stayed "running" in every live view forever.

**A full recorder reports the gap.** `Recorder` is bounded and preallocated; when full it counts drops
and emits an `Dropped` event rather than growing. A `Vec` doubling inside a 1 kHz region is exactly
the unbounded pause DR-4 exists to keep out — and a gap you know about is a measurement, a gap you do
not is a lie.

**No serde.** The schema is small, the JSON is written by hand with proper escaping, and a consumer in
any language reads it with its own parser.

**`Timeline`** replays a trace with no tree, no registry and no engine — the same position BTviz is
in. It answers "what was every node doing at time *t*" and "what was the active leaf". A test asserts
two scheduled runs under `VirtualTime` produce **byte-identical** traces, which is the property M6's
replay rests on.

### CLI

```
bteng validate mission.xml     what is wrong, and which node types your binary must provide
bteng structure mission.xml    the node table as JSON — what BTviz draws from
bteng models                   the node palette: every built-in and its ports
bteng run mission.xml --hz 50 --seconds 5 --virtual --trace run.jsonl
```

A CLI cannot construct your `NavigateTo` — that lives in your binary. So `run` works for trees of
built-ins, and `validate` lists every unregistered type in one pass rather than failing at the first,
which is the useful answer for a document authored elsewhere. `examples/mission.xml` exercises both
region modes, a subtree with remapping, and six decorators.

---

## Still open

- **No ARM hardware.** Everything compiles and the suite runs under QEMU; timing numbers remain
  x86-only. The atomics added in M3/M4 (`CancelToken`, `TaskSlot`, `WakeBus`) all have written
  `Ordering` justifications, and they are exactly the code that passes on x86 and can be wrong on
  aarch64.
- **Regions are still single-threaded.** Unchanged from M2, for the reasons in `m2-notes.md`.
- **`Timeline` reads events, not JSON.** Export is one-way; nothing in the engine needs to parse a
  trace back. If BTviz wants a round-trip, that is a parser, and it should be written against the
  schema doc rather than guessed.
- **No `loom` tests yet.** The concurrency is small and the orderings are argued in comments, but
  argued is not proved. Worth doing before the threaded-regions work, not after.
