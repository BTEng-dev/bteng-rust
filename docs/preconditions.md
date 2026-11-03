# Preconditions and `Skipped`

A node can carry up to four **preconditions**, written as XML attributes as in BehaviorTree.CPP v4.
They are checked before the node's `tick()`. When one decides, `tick()` is not called.

```xml
<Sequence>
  <Undock   _skipIf="state == 'UNDOCKED'"/>
  <Navigate _while="battery &gt; 15 &amp;&amp; !estop" goal="{target}"/>
  <Charge   _skipIf="battery &gt;= 80" _failureIf="charger_fault"/>
</Sequence>
```

(In XML, `<`, `>` and `&` inside an attribute are written `&lt;`, `&gt;` and `&amp;`. `>` is
legal unescaped; `<` and `&` are not.)

## The fifth status

`Status` and `NodeState` both have a `Skipped` variant. A node is skipped when a precondition stops
it from running. A skipped node did not run, so it counts as neither success nor failure.

Only three things produce `Skipped`:

- the precondition check;
- a control node or decorator passing it up from its children;
- `RunOnce` after its one run, as in BT.CPP (see [below](#runonce)).

A tree that uses no precondition and no `RunOnce` never sees `Skipped`.

A leaf's own `tick()` returning `Skipped` breaks the contract: a leaf has no precondition of its own
to report and no child to pass one up from. A debug build panics. A release build records `Failure`,
the same fallback the cycle guard uses.

## The four preconditions

| Attribute | When it is checked | Effect |
|---|---|---|
| `_failureIf` | node not `Running` | true → `Failure` |
| `_successIf` | node not `Running` | true → `Success` |
| `_skipIf` | node not `Running` | true → `Skipped` |
| `_while` | every tick | false → `Skipped`. If the node was `Running`, it is halted first. |

They are checked in the order of the table, and the first that decides wins. So `_failureIf` beats
`_successIf`, which beats `_skipIf`.

"Not `Running`" includes `Idle`, `Skipped`, and a node that kept its `Success`/`Failure` from the
last tick (a reactive parent re-ticks those). BT.CPP checks on `IDLE` or `SKIPPED`. This engine,
like Python BTEng, does not reset a finished node to idle until its parent halts it, so "not
running" is the equivalent test.

`_skipIf` is not re-checked while the node runs. An action already under way is not interrupted
when a skip condition turns true. Use `_while` for that.

A result decided by a precondition is recorded and traced like a ticked one. The trace event says
which precondition decided it (`"by":"_skipIf"`; see [Telemetry](#telemetry)).

A precondition on a control node guards its whole subtree. On a `<SubTree>` element, it guards the
subtree marker and reads the **calling** tree's keys.

## How control nodes and decorators treat a skipped child

| Node | One child skipped | Every child skipped |
|---|---|---|
| `Sequence`, `SequenceWithMemory`, `Fallback` | passed over; move to the next child | `Skipped` |
| `ReactiveSequence`, `ReactiveFallback` | passed over | `Skipped` |
| `Parallel` | left out of the child count before thresholds apply | `Skipped` |
| `IfThenElse`, `WhileDoElse` | condition skipped → `Skipped` (branches halted); branch skipped → passed through | — |
| `Inverter`, `ForceSuccess`, `ForceFailure`, `KeepRunningUntilFailure`, `Repeat`, `RetryUntilSuccessful`, `Timeout`, `Delay`, `RunOnce`, `SubTree` | `Skipped` passed through | — |
| `Region` | the region publishes `Skipped` if its subtree was skipped; the parent reads it like any verdict | — |

Notes:

- **"Every child skipped"** is read from the children's recorded states when the parent finishes.
  So a child skipped on an earlier tick of the same activation still counts. Composites need at
  least one child, so "every child" is never vacuous.
- **`ForceSuccess`/`ForceFailure`** force only a child that completed, as in BT.CPP. Forcing a
  skipped child would invent an outcome for work that never ran.
- **`Parallel`** re-ticks a skipped child every tick, as BT.CPP does, so a child whose `_skipIf`
  turns false can join in. Children skipped *this* tick are removed from the child count. A
  threshold is then clamped to the children that remain: `success_count="3"` over three children,
  one skipped, needs two. All skipped → `Skipped`. This deliberately differs from BT.CPP, where the
  answer depends on whether the threshold was given explicitly, and where an all-skipped `Parallel`
  with default thresholds returns `SUCCESS`.
- **`Repeat`/`RetryUntilSuccessful`** reset their counter when the child is skipped. BT.CPP keeps
  it. Here a skipped node is not `Running`, so `halt()` never runs for it and could not clear a kept
  counter later.
- **`IfThenElse`/`WhileDoElse`** with a skipped condition: BT.CPP leaves this undefined (it throws
  or returns `IDLE`). This engine returns `Skipped`.
- **A `Region` boundary** with a precondition that skips it is treated by the scheduler like a halted
  branch: its subtree is not ticked.

### RunOnce

`RunOnce` follows BT.CPP. The first run reports the child's status. After that, `then_skip="true"`
(the default) returns `Skipped`, and `then_skip="false"` replays the child's status without ticking
it again. Only `Success`/`Failure` count as the one run: if the child was itself skipped, it has not
run yet.

> [!WARNING]
> This changed in the release that added `Skipped`. `RunOnce` used to replay the cached status by
> default, and re-ran the child with `then_skip="false"`. A tree using a bare `<RunOnce>` now gets
> `Skipped` after the first run. Under a `Sequence` that behaves the same as before. Under a
> `Fallback`, a cached `Success` used to stop the fallback, and now the fallback moves on. Write
> `then_skip="false"` to keep the old default.

## The expression language

Deliberately small:

| Allowed | Example |
|---|---|
| Comparisons | `battery < 20`, `state == 'DOCKED'`, `name != "x"` |
| Logic | `&&` `\|\|` `!`, or `and` `or` `not` |
| Arithmetic | `+ - * / %` |
| Literals | numbers (`3`, `0.5`, `1e3`), strings (`'x'` or `"x"`), `true`, `false` |
| Names | a bare name is a blackboard key |
| Grouping | `( ... )` |

Not allowed, and rejected at load time by name: function calls, attribute access (`.`), indexing
(`[]`), assignment (`=`, `:=`). Two ambiguities are errors too:

- `!a == b`: C reads it as `(!a) == b`, Python's `not a == b` as `not (a == b)`. Add parentheses.
- `a < b < c`: Python chains comparisons, C does not. Join them with `&&`.

**Types are checked at build time.** Each operand is a boolean, a number or a string:

- Keys holding `bool`, any primitive integer, `f32`, `f64` or `String` can be read. A key of any
  other type is a build error.
- `&&`/`||`/`!` take booleans. Arithmetic takes numbers; there is no string `+`, because
  concatenating would allocate during the tick. `<`/`>` compare two numbers or two strings, and
  `==`/`!=` compare two values of the same type.
- The whole expression must be a boolean. `_skipIf="count"` is an error: write `count > 0`.

Every number is an `f64` at evaluation time. Integers above 2^53 lose precision. Since there is one
numeric type, nothing can overflow or divide by zero during a tick: `x / 0` is infinity, `x % 0` is
NaN, and every comparison with NaN is false.

Expressions are at most 32 levels deep and 256 terms long. That bounds the work done during a tick.

## Blackboard keys: resolved at build time

A name is looked up when the tree is **built**, not when it ticks. That follows this engine's rule
that everything fallible happens at build time.

- **An unknown key is a build error** that names the node, the attribute and the key:
  `node "dock": _skipIf="batery < 20": unknown blackboard key "batery"`. A typo in a key name never
  quietly becomes "condition false". The key must exist when `build()` runs: declared on the
  blackboard beforehand, or bound to any node's port in the same tree (ports bind first).
- **A key always holds a value.** The blackboard has no "unset" state: a slot is created holding a
  value, either the one you declared or the type's default when a port created it. An expression
  reads whatever value the slot currently holds. There is no "missing value" error at tick time,
  because the case cannot occur.
- **Inside a subtree**, names are rewritten the same way as `{key}` ports. `lvl` inside a subtree
  called with `lvl="{battery}"` reads `battery`; with `lvl="5"` it is the literal `5`; unmapped, it
  is the subtree's own `inner#1/lvl`.

If a tree is ticked against a different blackboard from the one it was built with, a slot might no
longer read as its build-time type. In that case the precondition does not decide anything and the
node ticks normally: a misconfiguration must not turn into a status the tree would act on. Debug
builds assert.

## Other `_` attributes are rejected

The only attributes starting with `_` are the four preconditions and `_autoremap`, `_description`
and `_uid`. Anything else is a load error rather than a silent port: BT.CPP's postconditions
(`_onSuccess`, `_onFailure`, `_onHalted`, `_post`, which need assignment and are not supported) and
typos such as `_skipif`.

## From Rust

```rust
use bteng_core::{Blackboard, Builtin, Precondition, TreeBuilder};

let mut bb = Blackboard::new();
bb.declare("battery", 50i32)?;
bb.declare("estop", false)?;
let graph = TreeBuilder::new()
    .begin("root", Builtin::sequence())
        .leaf("charge", Builtin::AlwaysSuccess)
        .precondition(Precondition::SkipIf, "battery >= 80")
        .leaf("work", Builtin::AlwaysSuccess)
        .precondition(Precondition::FailureIf, "estop")
    .end()
    .build(&mut bb)?;
```

`precondition` takes expression text. `precondition_expr` takes an already-parsed
`bteng_core::Expr`; that is how the XML loader passes a renamed one. There is deliberately no
form that takes a Rust function: a precondition must be writable in XML too (DR-5), and an
expression is also what the trace records. Giving the same precondition
twice on one node is a build error.

## Cost

A node without preconditions costs one extra pointer in the arena and one predictable branch per
visit. A node with them costs one bounded evaluation per visit. The expression is compiled at build
into a flat array and reads slots in place, so evaluating it **allocates nothing**.
`tests/no_alloc.rs` covers this: it alternates a `_while` between skip-and-halt and run, with
arithmetic, string comparison and short-circuit logic in the expressions.

## Telemetry

Trace schema **4** (`bteng-telemetry::SCHEMA_VERSION`) adds:

- `"skipped"` as a node state and a status;
- `"by"` on a `node` event decided by a precondition, for example
  `{"t":"node",...,"from":"idle","to":"skipped","by":"_skipIf"}`;
- `"pre"` on a node in the structure table:
  `"pre":{"_skipIf":"battery < 20"}`.

All three are omitted when absent, so the change is additive. A consumer that switches on state
names still has to handle `"skipped"`, which is why the version was bumped.

When `_while` halts a running node, the trace shows the halt (`running` → `idle`) and then the skip
(`idle` → `skipped`, `by: "_while"`).
