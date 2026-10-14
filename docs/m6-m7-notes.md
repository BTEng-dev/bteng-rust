# M6–M7 — the extension proof, and live reconfiguration

140 tests (129 in the workspace, 11 in the out-of-workspace extension). 0 clippy warnings. `no_std`
core and aarch64 still build.

---

## M6 — an extension crate, built the way a third party builds one

`extensions/bteng-devices/` is **not a member of the workspace**. It has its own `[workspace]` stanza
and depends on `bteng-core`, `bteng-async` and `bteng-xml` by path, exactly as an out-of-tree OPC UA
or ROS 2 crate would. That is the entire point: DR-6 says "the extension surface is sufficient
without core changes", and this makes that a claim the compiler checks rather than one the roadmap
asserts.

It provides `ReadTag<T>`, `WriteTag<T>`, `WaitForTag<T>` and an async `CallMethod` over a `Device`
trait, with a `MockDevice` that models latency, disconnection and injected failure. That mock is not
a stub — the interesting behaviour of a device node is what it does when the device misbehaves, which
is exactly what hardware will not do on demand. Python BTEng's ROS 2 package has `FakeRosNode` for
the same reason.

An XML mission of these third-party nodes loads, validates, connects on `setup`, runs to completion
and disconnects on `shutdown`.

### It found two real gaps

Writing it against the public API is what surfaced them. Both are fixed in core.

**1. `NodeCtor` could not carry a connection.** It was `fn() -> NodeKind`, and a `fn` pointer cannot
capture. A device node has to be born holding its client; a ROS node holding its handle. The only
ways to give a bare `fn` one are a leaked `static` or a thread-local — both of which make the
connection process-global and invisible in the type, and I wrote about thirty lines of exactly that
before deciding it was the API that was wrong, not the extension.

`NodeCtor` is now `Arc<dyn Fn() -> NodeKind + Send + Sync>`. One `Arc` clone per node
*construction* — build time, never tick time. A test now registers **two devices side by side** in
one process, which the old signature could not express at all.

**2. The node-model export dropped generic nodes.** `export_node_models_xml` deduplicated by name,
skipping any entry whose registered name differed from its manifest's `type_name`. That rule exists
to stop an alias (`Selector` for `Fallback`) appearing twice — but `ReadTagF64` and `ReadTagBool`
share the type name `ReadTag`, so **both vanished from the palette** BTviz would read. Deduplication
is now by manifest *identity*: an alias shares its target's manifest and is skipped; a generic
instantiation has its own and is kept.

### It also closed an M1 gap

M1 left `SetBlackboard` unimplemented because a node generic over its port type had no obvious home.
The answer turns out to need no machinery: `Manifests::<f64>::READ` and `Manifests::<bool>::READ` are
separate associated constants with separate port arrays, monomorphised at the use site, registered
under separate names. No macro, no registry of type handlers.

---

## M7 — live reconfiguration

`TreePatch` — `Replace`, `InsertChild`, `RemoveChild` — validated against the live graph, then
applied as a bounded splice.

**Build outside, install inside.** The replacement subtree is built by `TreeBuilder` like any other
tree, off the tick thread. `validate` checks target existence, child indices, and — the one that
matters — that the edit leaves every affected node with a child count its type allows. Only `apply`
touches the tree, and it takes a `PatchPlan`, a type you can only get by validating. Applying cannot
fail.

Python BTEng's equivalent probes `hasattr(parent, "_children")`, mutates the private list, and then
halts the parent to avoid a stale child cursor — so any edit *restarts every branch under that
parent*, and nothing about the incoming subtree is checked at all.

### State survives, because nodes are moved

Surviving nodes are `mem::replace`d out of the old arena and into the new one. A `Sequence` three
children deep keeps its cursor; an async leaf keeps its task handle; a `Running` node stays
`Running` — because the value itself is carried across, not reconstructed from a description of it.

The headline test: swap a subtree while a **sibling is mid-run**, then assert the sibling's already-
succeeded first child does *not* re-run. That is the exact behaviour Python cannot offer.

### Rollback is real

`apply` hands back the subtree it removed *and* the inverse patch. Applying one to the other restores
the previous tree exactly — same uids, same live state, same cursors.

The uid rule that makes this work: **an incoming node keeps its uid if that uid is free, otherwise it
gets a fresh one.** A subtree straight out of a `TreeBuilder` numbers from zero and collides with the
live tree, so it is renumbered. A subtree a previous patch *removed* took its uids with it, so they
are free and a rollback reclaims them — which is what makes an undo restore the original identities,
and keeps an earlier trace still lining up with the tree. I got this wrong first (fresh uids
always), and the rollback test caught it.

### Where it lives, and why not where the plan said

The plan put patching in a `bteng-patch` crate. It is arena surgery — it moves `NodeKind` values
between graphs — and doing that from outside would make the arena's parallel-vector layout public API
forever. It lives in `bteng_core::patch` instead, next to the arena it operates on.

### The scheduler survives a patch

A splice renumbers nodes, so every `NodeId` the scheduler held is stale the instant it lands.
`Scheduler::replace_tree` rebuilds the region table, re-derives read sets, and **carries deadlines and
statistics across by position** — a swap is not a restart, and a region that was not touched should
not lose its phase or its history. Tested.

### Telemetry knows

A patch emits `Event::Patch` carrying the *whole* new structure, not a diff: a viewer that joined
late, or one reading a truncated file, must not have to replay every earlier patch to know what it is
looking at. `Timeline` takes the last structure it sees, so a trace spanning a reconfiguration still
reconstructs correctly. `SCHEMA_VERSION` is **2**.

---

## Still open

- **No ARM hardware.** Unchanged. Everything compiles and the suite runs under QEMU; timing numbers
  remain x86-only, and the atomics added since M3 are exactly the code that can be wrong on aarch64.
- **No `loom` tests.** Still worth doing before the threaded-regions work.
- **Regions are still single-threaded** (see `m2-notes.md` for why that was the right order).
- **One op per patch.** Batching is possible; every extra op multiplies the ways validation can be
  subtly wrong about what the tree looks like halfway through, and nothing needs it yet.
- **A patch rebuilds the whole arena.** O(nodes), not O(patch). Fine for tens of thousands of nodes
  between ticks; if a tree ever gets large enough for that to matter, in-place splicing is the
  optimisation, and the validated-plan boundary is already where it would go.
- **No remote transport.** `TreePatch` is a plain data type and could be serialised, but who is
  allowed to send one, and what happens to a half-applied batch, are product questions (DR-9) that
  have not been asked yet.
