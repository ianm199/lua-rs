# lua-rs

**Lua 5.4.7, reimplemented in safe Rust.**

`lua-rs` is a from-scratch Rust port of the reference [PUC-Rio Lua 5.4.7](https://www.lua.org/)
interpreter. It runs ordinary Lua programs with no C runtime dependency, and it
passes **44 / 44** of the upstream Lua test suite — the same `.lua` files the C
implementation is validated against.

[![CI](https://github.com/ianm199/lua-rs-port/actions/workflows/ci.yml/badge.svg)](https://github.com/ianm199/lua-rs-port/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/lua-cli.svg?label=crates.io%2Flua-cli)](https://crates.io/crates/lua-cli)
[![license](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![upstream tests](https://img.shields.io/badge/upstream%20suite-44%2F44-0f8f68.svg)](#conformance)
[![performance](https://img.shields.io/badge/perf-live%20dashboard-2f6fed.svg)](https://ianm199.github.io/lua-rs-port/harness/bench/history/)

```bash
cargo install lua-cli       # crate: lua-cli  →  binary: lua-rs
lua-rs -e 'print("hello from lua-rs")'
```

> [!NOTE]
> The package on crates.io is named **`lua-cli`**; it installs a binary named
> **`lua-rs`**. `cargo install lua-cli` is the install; `lua-rs` is what you run.

---

## Highlights

- **Passes the real Lua test suite.** Not a subset, not a lookalike — the
  upstream PUC-Rio Lua 5.4.7 `testes/` suite runs against this binary and passes
  44/44. See [Conformance](#conformance).
- **Safe Rust by default.** Most crates compile under `#![forbid(unsafe_code)]`.
  The only `unsafe` is a small, audited, budgeted core in the garbage collector
  and the optional dynamic-library loader. See [Safety model](#safety-model).
- **No C runtime.** Running a `.lua` script links no `liblua` and shells out to
  no C interpreter. It is a standalone Rust binary.
- **Competitive performance, tracked publicly.** Within ~1.3× of reference C on
  a geometric mean of wall time, faster than C on some workloads — and every
  commit's benchmark is plotted on a [live dashboard](https://ianm199.github.io/lua-rs-port/harness/bench/history/).
- **Built by an AI porting harness.** ~28k lines of C became safe Rust under a
  test-oracle-gated, multi-agent harness. That methodology is the deeper story —
  see [How it was built](#how-it-was-built).

## Installation

From crates.io (preview release `0.0.1`):

```bash
cargo install lua-cli
```

This installs the `lua-rs` binary into `~/.cargo/bin`. Confirm it is on your
`PATH`, then:

```bash
lua-rs -e 'print(("safe rust"):upper())'   # SAFE RUST
```

From source:

```bash
git clone https://github.com/ianm199/lua-rs-port
cd lua-rs-port
cargo build --release --bin lua-rs
./target/release/lua-rs -e 'print(_VERSION)'   # Lua 5.4
```

## Usage

```bash
lua-rs script.lua                 # run a Lua source file
lua-rs -e 'print(1 + 2)'          # run a one-liner
lua-rs 'print("bare source")'     # a bare non-file argument is treated as source
```

Supported today: running a script file, `-e <chunk>`, and a bare source-string
argument. There is **no REPL**, no stdin (`-`) execution, and no `--help`/`-v`
flag yet — see [Limitations](#limitations-and-non-goals).

## Conformance

The strongest claim this project makes is conformance. The repository runs the
unmodified upstream Lua 5.4.7 test files against the `lua-rs` binary through a
behavioral oracle (same input → diff stdout + exit code against reference C):

```bash
cargo build -q --bin lua-rs
TEST_TIMEOUT_S=90 ./harness/run_official_all.sh
# → 44/44 PASS
```

This is strong evidence for **Lua source/runtime compatibility**. It does *not*
imply C API or ABI compatibility (see [Limitations](#limitations-and-non-goals)).
The per-test debugging history is in
[docs/OFFICIAL_TEST_INVESTIGATIONS.md](docs/OFFICIAL_TEST_INVESTIGATIONS.md).

## Performance

Performance is measured by a dedicated **benchmark suite** — eight workloads
(`fibonacci`, `mandelbrot`, `binarytrees`, `closure_ops`, `table_ops`,
`table_ops_long`, `string_ops`, `string_ops_long`) run against reference
PUC-Rio Lua 5.4.7 via `harness/bench/compare.sh`. This is separate from the
[44/44 conformance suite](#conformance) above — it measures speed and memory,
not correctness. Every benchmarked commit is recorded and plotted:

### → [**Live performance dashboard**](https://ianm199.github.io/lua-rs-port/harness/bench/history/) — `ianm199.github.io/lua-rs-port`

Each point is the ratio of `lua-rs` to reference lua-c on the same workload.
**Lower is better; `1.00×` is parity with C.** At the latest benchmarked commit,
across the eight workloads:

| Metric | Value | Reading |
|---|---|---|
| Wall-time geomean | **1.27×** | ~27% slower than C on average |
| RSS geomean | **1.96×** | ~2× the memory of C |
| Best workload (`table_ops_long`) | **0.38×** | faster than C |
| Worst workload (`binarytrees`) | **2.07×** | slowest relative workload |

[**See the full per-workload trajectory on the dashboard →**](https://ianm199.github.io/lua-rs-port/harness/bench/history/)

The honest summary: this is **not** "faster than C." It is a memory-safe
reimplementation that is *competitive* with C — within a small constant factor
on average and ahead on some workloads — with the full per-workload trajectory
published rather than reduced to one headline number. Method and policy are in
[docs/PERFORMANCE_PRINCIPLES.md](docs/PERFORMANCE_PRINCIPLES.md).

## Safety model

`lua-rs` is a mostly-safe runtime wrapped around a small, explicit unsafe
kernel — **not** "completely safe Rust," but not unsafe-everywhere either. Most
of the workspace compiles under `#![forbid(unsafe_code)]` (the default), and the
VM, parser, lexer, bytecode compiler, standard library, and coroutine layer are
all budgeted at **zero** unsafe. Per-crate ceilings are enforced by a gate:

```bash
.claude/hooks/unsafe-budget.sh   # ceilings live in harness/unsafe-budgets.toml
```

All of the project's real `unsafe` lives in two places:

- **The GC core — `crates/lua-gc` (13 budgeted sites).** This is the trusted
  kernel: raw-pointer object identity, intrusive heap walking, gray-list
  traversal, sweep cursors, and `Box::from_raw` reclamation. Its soundness rests
  on one invariant — collection only runs at safepoints where every live
  `Gc<T>` handle is reachable through the traced root graph. This is the part to
  treat seriously: if that invariant is wrong, the bugs can be serious.
- **Dynamic library loading — `crates/lua-cli` (5 budgeted sites).**
  `libloading` is inherently unsafe — it opens arbitrary symbols from shared
  objects. This unsafe is narrow and only activated when loading dynamic
  modules; each block carries a `// SAFETY:` justification.

This split is a deliberate trade-off. `lua-rs` optimizes for Lua **ecosystem and
CLI compatibility** — a full CLI surface, `require`/`package`, LuaRocks pure-Lua
workflows, and dynamic module loading. Pure-Rust, embedding-focused Lua
implementations can be safer still, but typically target a narrower, sandboxed
host surface; our small unsafe core is what buys the broader compatibility.

Details in [docs/LUA_SYSTEM_DEEP_DIVE.md](docs/LUA_SYSTEM_DEEP_DIVE.md) and
[docs/PUBLISH_READINESS.md](docs/PUBLISH_READINESS.md).

## How it was built

The runtime is the artifact. The **AI-agent porting harness** is the method that
produced it — and the more reusable result.

Porting ~28k lines of C to safe Rust was driven by bounded, single-purpose
agents (translator, compiler-fixer, test-fixer, read-only verifier) gated by a
non-negotiable **oracle**: a change is unverified until the upstream test suite
or a structural diff says it matches reference C. Mechanical guardrails
(unsafe-budget ceilings, forbidden-pattern bans, required status trailers, a
verify-gate) are enforced as hooks, not vibes. The read-only verifier *cannot*
mark a test passing — anti-sycophancy by construction.

- [PORTING.md](PORTING.md) — the C→Rust translation rules agents follow.
- [HARNESS_DESIGN.md](HARNESS_DESIGN.md) — harness structure and enforcement model.
- [docs/RETROSPECTIVE_AND_PRODUCTIZATION.md](docs/RETROSPECTIVE_AND_PRODUCTIZATION.md)
  — what we learned and what a productized v2 needs.

## Roadmap

- **LuaRocks support for pure-Lua rocks (in progress).** `lua-rs` can run
  LuaRocks 3.11.1 well enough to search, install, list, show, and use pure-Lua
  rocks such as `inspect`. Native C rocks remain out of scope until there is
  either targeted Rust-native module coverage or a PUC-Rio Lua C API/ABI layer.
  Plain-English explainer: [docs/LUAROCKS_SIGNIFICANCE.md](docs/LUAROCKS_SIGNIFICANCE.md).
  Technical plan: [docs/PHASE_G_LUAROCKS_PLAN.md](docs/PHASE_G_LUAROCKS_PLAN.md).
- **Performance parity with PUC-Rio Lua.** Close the remaining wall-time gap
  (~1.27× geomean today) toward parity with reference C-Lua, tracked commit by
  commit on the [live dashboard](https://ianm199.github.io/lua-rs-port/harness/bench/history/).
- **A testbed for runtime research.** Use the safe-Rust substrate to prototype
  and measure new garbage-collection strategies and other language/runtime
  features against a real conformance suite and benchmark harness — changes are
  validated by the oracle, not by intuition.
- **CLI surface.** REPL, stdin execution, and a polished `--help`/`--version`.
- **Embedding API.** A Rust-native embedding surface; a C API/ABI story is a
  longer-term, separate effort. See [docs/FUTURE_GOALS.md](docs/FUTURE_GOALS.md).

## Limitations and non-goals

- **Not LuaJIT**, and not targeting LuaJIT-level performance.
- **Not a C-ABI drop-in.** This runtime does not currently expose the Lua C
  API/ABI, so stock Lua C modules that expect `liblua` will not load unchanged.
- **Not for Lua 5.1 ecosystems** (OpenResty, Neovim's LuaJIT embedding, WoW
  addons) — this is Lua 5.4.
- No REPL, stdin execution, or polished CLI help yet.

## Project layout

```
crates/
  lua-lex, lua-parse, lua-code   # front end: lexer, parser, bytecode compiler
  lua-vm                         # the register VM and core runtime
  lua-types                      # LuaValue, tables, strings, errors
  lua-gc                         # garbage collector (budgeted unsafe)
  lua-stdlib                     # standard library
  lua-coro                       # coroutines
  lua-cli                        # the `lua-rs` binary + dynamic-load backend
harness/                         # the porting harness: oracles, benches, gates
docs/                            # architecture, performance, and porting docs
reference/                       # pinned upstream Lua 5.4.7 C source (the oracle)
```

## Development

```bash
cargo build -q --bin lua-rs                 # build
TEST_TIMEOUT_S=90 ./harness/run_official_all.sh   # full upstream suite (44/44)
./harness/run_one_test.sh reference/lua-c/testes/strings.lua   # one test
python3 harness/bench/history.py            # rebuild the perf dashboard
.claude/hooks/unsafe-budget.sh              # unsafe-budget gate
```

## Acknowledgements

`lua-rs` is a port of [Lua](https://www.lua.org/), created by Roberto
Ierusalimschy, Luiz Henrique de Figueiredo, and Waldemar Celes at PUC-Rio. The
upstream Lua source is pinned in `reference/` and used as the conformance
oracle. Lua is distributed under the MIT license; this port is likewise MIT.

## License

MIT — see [LICENSE](LICENSE).
