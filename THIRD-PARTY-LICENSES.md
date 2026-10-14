# Third-party dependencies and licenses

Audit date: **2026-08-23**. Branch: `dev` (cut from `main` at `2a88c55`, v0.0.1).

**Everything BTEngRust depends on is open source.** There is no proprietary, source-available, or
commercially licensed component anywhere in the tree — no BSL/SSPL/Elastic-style license, and nothing
that has ever been relicensed *away* from open source.

## How this was produced

- Direct dependencies: every `Cargo.toml` in the workspace, plus the two out-of-workspace crates
  (`extensions/bteng-devices`, `crates/bteng-py` on `feat/py-control-plane`).
- Resolved versions: `Cargo.lock` (three of them — root, extension, `bteng-py`).
- License of the exact resolved version: the `license` field in the vendored source under
  `~/.cargo/registry/src/`.
- License *history*: the crates.io API (`/api/v1/crates/<name>/versions`), which records the `license`
  field of every published version. That is what the "changed?" columns below are read from — not
  recollection.
- Non-crate components (GitHub Actions, toolchain, cross tools): the GitHub license API and the
  distributions' own packaging.

## First-party crates

All under **MIT** (`LICENSE`, Copyright (c) 2026 Mahdi), declared once in `[workspace.package]`.

`bteng-core`, `bteng-async`, `bteng-sched`, `bteng-telemetry`, `bteng-xml`, `bteng-cli`,
`bteng-bench` — plus `bteng-devices` and `bteng-py`, which set `license = "MIT"` in their own
manifests because they are deliberately not workspace members.

## 1. Shipped dependency tree (`main` / `dev` workspace)

The whole engine has **two** third-party crates, both reached through `bteng-xml`. `bteng-core`,
`bteng-async`, `bteng-sched`, `bteng-telemetry`, `bteng-cli` and `bteng-bench` have **zero**
third-party dependencies.

| Crate | Version in use | Direct/transitive | License (this version) | OSI/open source | License ever changed? |
|---|---|---|---|---|---|
| [`roxmltree`](https://crates.io/crates/roxmltree) | 0.21.1 (declared `"0.21"`) | direct — `bteng-xml` | `MIT OR Apache-2.0` | yes | Text only. `MIT/Apache-2.0` → `MIT OR Apache-2.0` at **0.16.0** (2022-11-21). Same two licenses; the old form was deprecated SPDX syntax. |
| [`memchr`](https://crates.io/crates/memchr) | 2.8.3 | transitive — via `roxmltree` | `Unlicense OR MIT` | yes | Text only. `Unlicense/MIT` → `Unlicense OR MIT` at **2.6.0** (2023-08-28). Same terms, SPDX syntax fix. |

`extensions/bteng-devices` resolves the same two crates and nothing else.

The workspace is dependency-light by decision, not accident — the manifests record why in comments:
no `criterion` in `bteng-bench`, no `serde` in `bteng-telemetry`, no async runtime in `bteng-async`.
Each avoided crate is a licensing and aarch64/QEMU build-time liability that never had to be assessed.

## 2. Python control plane (`feat/py-control-plane`, parked, unmerged)

`crates/bteng-py` is excluded from the workspace and from CI. Its tree is where nearly all of the
project's third-party surface lives. Nothing here ships on `main`.

| Crate | Version | License (this version) | Open source | License ever changed? |
|---|---|---|---|---|
| [`pyo3`](https://crates.io/crates/pyo3) | 0.29.0 (declared `"0.29"`) | `MIT OR Apache-2.0` | yes | **Yes, substantively.** Apache-2.0 only from 0.1.0 (2017-07-23) through 0.19.x; **dual-licensed `MIT OR Apache-2.0` from 0.20.0 (2023-10-11)**. This is a genuine relicensing (a license *added*), not a syntax change. We are on 0.29, so both options are available. |
| `pyo3-ffi` | 0.29.0 | `MIT OR Apache-2.0` | yes | Same: Apache-2.0 only 0.16.0 → 0.19.x, dual from **0.20.0**. |
| `pyo3-macros` | 0.29.0 | `MIT OR Apache-2.0` | yes | Same: Apache-2.0 only from 0.13.0-alpha, dual from **0.20.0**. |
| `pyo3-macros-backend` | 0.29.0 | `MIT OR Apache-2.0` | yes | Same: Apache-2.0 only from 0.13.0-alpha, dual from **0.20.0**. |
| `pyo3-build-config` | 0.29.0 | `MIT OR Apache-2.0` | yes | Same: Apache-2.0 only from 0.14.0-alpha.0, dual from **0.20.0**. |
| [`libc`](https://crates.io/crates/libc) | 0.2.189 | `MIT OR Apache-2.0` | yes | Text only: `MIT/Apache-2.0` → `MIT OR Apache-2.0` at **0.2.44** (2018-11-22). |
| [`once_cell`](https://crates.io/crates/once_cell) | 1.21.4 | `MIT OR Apache-2.0` | yes | No. Same since 0.0.1 (2018). |
| [`portable-atomic`](https://crates.io/crates/portable-atomic) | 1.14.0 | `Apache-2.0 OR MIT` | yes | No. Same since 0.1.0 (2022). |
| [`proc-macro2`](https://crates.io/crates/proc-macro2) | 1.0.107 | `MIT OR Apache-2.0` | yes | Text only: `MIT/Apache-2.0` → `MIT OR Apache-2.0` at **1.0.0** (2019-08-13). |
| [`quote`](https://crates.io/crates/quote) | 1.0.47 | `MIT OR Apache-2.0` | yes | Text only: changed at **0.6.13** (2019-07-11). |
| [`syn`](https://crates.io/crates/syn) | 2.0.119 | `MIT OR Apache-2.0` | yes | Text only: changed at **0.15.35** (2019-06-08). |
| [`heck`](https://crates.io/crates/heck) | 0.5.0 | `MIT OR Apache-2.0` | yes | No. Same since 0.1.0 (2017). |
| [`target-lexicon`](https://crates.io/crates/target-lexicon) | 0.13.5 | `Apache-2.0 WITH LLVM-exception` | yes | Effectively no. 0.0.0 (2018-05-24) published with no parseable license field ("non-standard"); `Apache-2.0 WITH LLVM-exception` from **0.0.3** (2018-07-13) onward. |
| [`unicode-ident`](https://crates.io/crates/unicode-ident) | 1.0.24 | `(MIT OR Apache-2.0) AND Unicode-3.0` | yes | **Yes, twice.** `MIT OR Apache-2.0` at 0.0.0; **1.0.2** (2022-07-16) added `AND Unicode-DFS-2016` for the embedded Unicode tables; **1.0.14** (2024-11-20) moved that component to `Unicode-3.0` following Unicode's own relicensing. Terms stayed permissive throughout. |
| [`maturin`](https://pypi.org/project/maturin/) | `>=1.5,<2.0` (build backend, `pyproject.toml`) | `MIT OR Apache-2.0` | yes | No. Same since 0.7.0 (2019). Build-time only — it is not linked into the wheel. |

**The one item worth a decision:** PyO3 ≤ 0.19 is Apache-2.0 only. If anything ever pins an old PyO3,
the MIT option disappears for that build. On 0.29 there is nothing to do.

CPython itself (linked at runtime by the extension module) is under the **PSF License Agreement** —
permissive, GPL-compatible, no copyleft.

## 3. Toolchain and CI (build-time only, not distributed)

| Component | Where | License | Open source | Changed? |
|---|---|---|---|---|
| Rust compiler + `std` | `rust-toolchain.toml`, channel `stable` | `MIT OR Apache-2.0` (dual; `LICENSE-MIT` + `LICENSE-APACHE`) | yes | No. |
| [`actions/checkout@v4`](https://github.com/actions/checkout) | CI, all 5 jobs | MIT | yes | No. |
| [`dtolnay/rust-toolchain@stable`](https://github.com/dtolnay/rust-toolchain) | CI, all 5 jobs | MIT | yes | No. |
| [`Swatinem/rust-cache@v2`](https://github.com/Swatinem/rust-cache) | CI, all 5 jobs | **LGPL-3.0** | yes | No. |

`Swatinem/rust-cache` is the only copyleft license in the whole picture, and it is a GitHub Action:
it runs as a separate program in CI, is never linked into or distributed with any BTEngRust artifact,
and imposes nothing on the code. (The `aarch64` job once also installed GCC and QEMU; it now runs natively
on GitHub's ARM runner, so neither is a dependency any more.)

## 4. Provenance (DR-1, clean room)

Not a dependency question, but the one most likely to be asked next:

- **BehaviorTree.CPP** — MIT. Concepts only (node semantics, XML shape). **No code taken** (DR-1).
- **The Python `bteng` engine** (`~/bteng-dev/bteng`) — internal/unpublished, not distributed with
  this repo. Concepts only, again no code.

Because nothing was copied, neither project's license attaches to BTEngRust. The XML front end is
*format*-compatible with BT.CPP, which is interoperability, not derivation.

## 5. Obligations, concretely

For MIT / Apache-2.0 / Unlicense / Unicode-3.0 / LLVM-exception — all permissive:

1. **Binary distribution:** ship the license text and copyright notices of `roxmltree` and `memchr`
   (and, if the Python wheel is ever released, the PyO3 stack). A generated `NOTICE` file satisfies
   this; `cargo about` or `cargo bundle-licenses` produces one from `Cargo.lock`.
2. **Apache-2.0** additionally requires stating significant changes if a dependency is modified. None
   are — every one is used unmodified from crates.io.
3. **Patent grant:** Apache-2.0 carries one, MIT does not. If patent protection matters downstream,
   take the Apache-2.0 option where a crate offers `OR`.
4. **`Unicode-3.0`** (via `unicode-ident`, Python branch only) requires its notice be reproduced;
   it is permissive and imposes no source obligation.
5. **Nothing here requires source disclosure, copyleft compliance, or a commercial license.**

## 6. Where a dependency is limiting, and what replaces it

Two different kinds of "limited" turn up in the audit: one is a license constraint, one is a
functional one. Neither is urgent; both are cheap to fix and expensive to discover late.

### 6.1 Problem / solution summary

| # | Problem | Severity today | Solution | Advantage | Disadvantage |
|---|---|---|---|---|---|
| 1 | `Swatinem/rust-cache` is **LGPL-3.0** — the only copyleft license anywhere in the project. Used in all 5 CI jobs. | **None in practice.** It is a GitHub Action: a separate program, run in CI, never linked into or distributed with any BTEng artifact. No obligation reaches the code. | **Delete the action entirely — see §6.3.** | Measured: it saves ~1 s of compilation. The licence question disappears with the line, and CI likely gets *faster*. | Reverses if `bteng-py` ever lands in CI (~15 crates, `syn` included). |
| | | Do nothing, and record why. | Zero work. The legal analysis is already correct. | An SBOM or a procurement questionnaire will keep flagging "LGPL-3.0" and someone re-litigates it every time. |
| | | | Swap to `Leafwing-Studios/cargo-cache` (Apache-2.0). | Permissive SBOM with no footnote. Same job — it wraps `actions/cache` + `cargo-sweep`, so pruning survives. ~5 lines × 5 jobs. | Smaller user base than rust-cache; one more third party to track. Cache keys differ, so the first run after the swap is a full cold build. |
| | | | Swap to `actions/cache@v4` (MIT, official). | Fewest moving parts, GitHub's own action, longest support horizon. | **Loses** rust-cache's cargo-aware keying (lockfile + rustc version + job) and its stale-artifact pruning. The cache grows until it hits the 10 GB repo cap and starts evicting, at which point CI silently gets slower. You now maintain the key logic. |
| | | | Add `Mozilla-Actions/sccache-action` (Apache-2.0). | **More** function, not merely equal: your 5 jobs compile the same crates over and over, and sccache shares compiled objects across all of them, not just within a job. | Different mechanism, so it complements rather than replaces a target-dir cache. Another moving part in CI, and sccache misses are silent. |
| 2 | `actions-rust-lang/setup-rust-toolchain` looks like an MIT way out. | — | **Do not use it as the fix.** Its `action.yml` calls `Swatinem/rust-cache@v2.9.2` internally. | — | It hides the LGPL dependency instead of removing it, which is worse than the honest version. |
| 3 | `target-lexicon` is `Apache-2.0 WITH LLVM-exception` with **no MIT option** — the only dependency offering a single license. | None. Transitive under `pyo3-build-config`, Python branch only. | Nothing to do. | The LLVM exception is *more* permissive than bare Apache-2.0: it removes the binary-attribution burden. | Cannot be swapped without dropping PyO3 entirely. Accept it. |
| 4 | `unicode-ident` carries a `Unicode-3.0` component alongside MIT/Apache-2.0. | None. Transitive under `syn`, Python branch only. | Reproduce the Unicode notice in the generated `NOTICE`. | Permissive, no source obligation. | One extra notice block. Easy to forget when the wheel is first published. |
| 5 | PyO3 ≤ 0.19 is **Apache-2.0 only**; the MIT option only exists from 0.20.0. | None — the branch pins `"0.29"`. | Never pin PyO3 below 0.20. | Both license options stay available. | A future downgrade for compatibility reasons would silently remove the MIT option. Worth a comment in `bteng-py/Cargo.toml` if the branch merges. |
| 6 | `roxmltree` is a **read-only** DOM: no serialization, no XPath, no streaming. | None today. `bteng-xml` uses only `Document::parse`, `children`, `descendants`, `tag_name`, `attribute(s)`, `is_element` — the centre of what roxmltree is for. | Nothing, until XML has to be *written*. | Zero unsafe, tiny dependency tree, builds clean for aarch64 and under QEMU — the reason it was chosen. | The limit becomes real the day a patched tree (M5) or a BTviz round trip needs XML *out*. See 6.2. |

### 6.2 If XML output is ever needed

The gap is emitting XML, not parsing it. Options, in the order they should be considered:

| Option | License | Advantage | Disadvantage |
|---|---|---|---|
| **Hand-write the emitter over the arena** | — (first-party) | ~100 lines. No new dependency, no new license, nothing new to build for aarch64. Matches existing precedent exactly: `bteng-telemetry` writes its JSON by hand for the same reason. Output shape stays under our control, which matters for BT.CPP compatibility. | Hand-rolled escaping is a place to get XML wrong. Needs its own round-trip test (parse → emit → parse) to be trustworthy. |
| `quick-xml` 0.42 | MIT | Read *and* write, streaming, optional serde. Very widely used (~380M downloads), so bugs surface fast. | A second XML crate in the tree unless it also replaces `roxmltree` — and replacing means rewriting the whole `bteng-xml` front end against a pull parser, not a DOM. Verify its dependency tree before adopting; a large tree in front of every aarch64/QEMU build is precisely what `roxmltree` was chosen to avoid. |
| `xml-rs` 1.0 | MIT | Read and write, mature, pull parser. | Slower than the alternatives, and the API is dated. No reason to prefer it over `quick-xml`. |
| `minidom` 0.19 | **MPL-2.0** | Simple read/write DOM. | File-level copyleft, and unlike `rust-cache` this one would be **linked into the shipped binary**. It would be the only copyleft in the distributed artifact. Avoid. |

**Recommendation:** hand-write the emitter. It is consistent with how this repo has resolved the same
question twice already (no `serde` in telemetry, no `criterion` in bench), and the dependency-light
tree is a large part of why the licensing picture above is two crates long.

### 6.3 Do we need the caching action at all?

Measured on this machine (x86_64, warm crates.io cache, fresh `CARGO_TARGET_DIR` each run):

| Build | Wall | CPU | Artifact size |
|---|---|---|---|
| Third-party deps only (`roxmltree` + `memchr`, via `cargo build -p bteng-xml`) | **1.0 s** | 1.9 s | — |
| Full workspace, `--all-targets`, debug, cold | **1.3 s** | 10.9 s | 339 MB target dir |
| `cargo build --release -p bteng-bench` (lto=thin, codegen-units=1), cold | **3.6 s** | 6.4 s | 3.4 MB |

A dependency cache exists to avoid recompiling other people's code. This project has **two** crates of
other people's code and they compile in **one second**. Restoring and saving a cache blob over the
network costs more than that on any runner.

So the answer for `Swatinem/rust-cache` is not "find a permissively licensed replacement" — it is
**delete the line from all five jobs**. The LGPL-3.0 entry leaves the SBOM, and CI most likely gets
faster rather than slower. There is nothing to replace because there was nothing being bought.

Scale the wall-clock figures by CPU time, not wall time, before predicting a GitHub runner: these
numbers come from a many-core desktop and `ubuntu-latest` gives 2 cores. Even so, ~11 s of CPU for a
cold full build is not what caching exists for. (The aarch64 job formerly ran the suite under QEMU emulation; it now runs natively on GitHub's ARM
runner, so no cross toolchain or emulator is installed.)

The other two actions stay:

| Action | License | Keep? | Why |
|---|---|---|---|
| `actions/checkout@v4` | MIT | **Keep** | Required to get the source. No licence question. Hand-rolling `git clone` buys nothing. |
| `dtolnay/rust-toolchain@stable` | MIT | **Keep** | No licence question. `ubuntu-latest` does ship rustup, so it is technically droppable — but then `rustup target add aarch64-unknown-linux-gnu` and the `rustfmt`/`clippy` component installs become hand-written steps in five jobs. Not worth it to remove an MIT dependency that costs nothing. |

**This decision reverses if `feat/py-control-plane` merges and gets a CI job.** That tree is ~15 crates
including `syn` and `proc-macro2`, which are genuinely slow to compile. At that point a cache earns its
place — and the choice among the permissive options in §6.1 becomes a real one.

## Keeping this true

CI enforces it. The `licenses` job runs `cargo deny check licenses sources bans` against both the
workspace and `extensions/bteng-devices`, using the permissive allowlist in `deny.toml`. A dependency
whose license is not on that list fails the build, so this document cannot quietly go stale — the
build breaks before the audit does. Security advisories are deliberately excluded: they fail on the
calendar rather than on the diff, and this tree is two third-party crates deep.

## Regenerating this

```bash
# resolved versions + the license of each
cargo tree --workspace -e normal
for d in ~/.cargo/registry/src/*/*/; do grep -H -m1 '^license' "$d/Cargo.toml"; done

# per-version license history for one crate
curl -s https://crates.io/api/v1/crates/<crate>/versions \
  | jq -r '.versions | sort_by(.created_at)[] | "\(.num)\t\(.created_at[:10])\t\(.license)"'
```

Re-run when `Cargo.lock` changes, when `feat/py-control-plane` merges, or before the first binary or
wheel is published.
