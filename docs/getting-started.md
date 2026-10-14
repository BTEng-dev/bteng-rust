# Getting started

## Requirements

Rust 1.85 or newer. Linux on x86-64 or aarch64.

## Build and test

```bash
cargo test --workspace
cargo run --release -p bteng-bench              # tick throughput, allocation gate
cargo run --release -p bteng-bench -- --jitter  # scheduler lateness
```

## The CLI

The CLI only knows the built-in nodes. It cannot construct your own `NavigateTo`, because that lives
in your binary. So `run` works for trees made of built-ins; for everything else `validate` and
`structure` answer "is this document well-formed, and which node types would my program have to
provide?"

```bash
cargo run -p bteng-cli -- validate examples/mission.xml            # what is wrong with this document
cargo run -p bteng-cli -- validate examples/mission.xml --strict   # fail if a node type is not built in
cargo run -p bteng-cli -- structure examples/mission.xml           # the node table, as JSON
cargo run -p bteng-cli -- models                                   # every built-in node and its ports
cargo run -p bteng-cli -- run examples/mission.xml --virtual --trace run.jsonl
```

`run` options: `--hz <rate>` (default 10), `--seconds <n>` (default 1; `0` = until settled),
`--tree <id>`, `--trace <file>`, `--virtual` (simulated time: as fast as possible, deterministic).

The input is XML. The `--trace` output is JSON Lines.

## A tree in Rust

```rust
use bteng_core::{Blackboard, Builtin, Ctx, SimClock, Status, Tree, TreeBuilder};

let mut bb = Blackboard::new();
let graph = TreeBuilder::new()
    .begin("root", Builtin::sequence())
        .leaf("battery_ok", Builtin::condition(|_| true))
        .begin("guarded", Builtin::timeout()).port("msec", "5000")
            .leaf("navigate", Builtin::action(|_| Status::Success))
        .end()
    .end()
    .build(&mut bb)?;

let mut tree = Tree::new(graph);
let clock = SimClock::new();
let mut ctx = Ctx::new(&mut bb, &clock);
assert_eq!(tree.tick(&mut ctx), Status::Success);
# Ok::<(), bteng_core::BuildError>(())
```

## A tree in XML

Against a registry of your own nodes:

```xml
<root BTCPP_format="4" main_tree_to_execute="main">
  <BehaviorTree ID="main">
    <Sequence>
      <BatteryAbove threshold="20.0"/>
      <Timeout msec="5000">
        <NavigateTo goal="{target}" arrived="{at_goal}"/>
      </Timeout>
      <SubTree ID="dock" station="{target}"/>
    </Sequence>
  </BehaviorTree>
</root>
```

Port values are parsed and type-checked when the document loads, not when the tree runs. A
misspelled port name or a `speed="quite fast"` is a load error, with the node and port named.

## Next

- [Multi-rate ticking](multi-rate.md)
- [Live reconfiguration](live-reconfiguration.md)
- [Crates](crates.md)
