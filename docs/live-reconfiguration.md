# Live reconfiguration

A subtree can be swapped while a sibling is mid-run, and the sibling does not notice. It keeps its
`Sequence` cursor, its task handles and its `Running` state, because surviving nodes are *moved*
rather than rebuilt.

```rust
let plan = TreePatch::Replace { target: uid }.validate(&graph, Some(&incoming))?;
let out  = apply(graph, Some(incoming), plan);   // cannot fail: the plan is the proof
// out.removed + out.inverse put it all back, exactly.
```

Everything fallible happens in `validate`, off the tick thread. Python BTEng mutates a private list
and halts the parent, which restarts every branch under it.

## Rules

- One structural operation per patch. Batching is not implemented.
- Patch validation enforces the same maximum depth as the builder (`DEFAULT_MAX_DEPTH`, 512).
- Patch, rollback and spawn allocate **by design**. They are tested for correctness under repetition,
  not for zero allocation: a splice rebuilds the arena.

Background and the decisions behind this: [M6–M7 notes](m6-m7-notes.md).
