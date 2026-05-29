# Contributing to lua-rs

Thanks for your interest in `lua-rs` — a Lua 5.4.7 runtime implemented in safe
Rust. This guide covers how to build it, how to run the tests, the project
layout, and the code-style rules the project enforces.

## Prerequisites

- A recent stable Rust toolchain (`rustup` recommended; the workspace pins
  `rust-version = "1.77"`).
- A POSIX shell for the harness scripts.

## Build

```bash
cargo build --bin lua-rs            # debug build of the CLI
cargo build --release --bin lua-rs  # optimized build
cargo build --workspace --bins      # everything
```

The debug binary lands at `target/debug/lua-rs`.

## Run

```bash
target/debug/lua-rs script.lua             # run a source file
target/debug/lua-rs -e 'print(1 + 2)'      # run a one-liner
target/debug/lua-rs 'print("bare source")' # a bare argument is treated as source
```

There is no REPL or stdin execution yet — see the roadmap in the
[README](README.md).

## Tests: the oracle is the source of truth

A change is **unverified** until an oracle says it behaves like reference C.
Build success is not signal.

One command runs everything CI runs (it builds its own binary, so there is no
stale-binary trap):

```bash
make test          # build + Rust tests + full conformance suite
```

The other Makefile targets:

```bash
make rust          # workspace unit/integration tests + embedding doctests
make conformance   # official Lua 5.4 suite only
make perf          # benchmark vs reference C Lua (measurement, not a gate)
make scaling       # flag superlinear (O(n^2)) behavior in hot operations
make setup         # recreate the reference/lua-c/testes symlink if missing
```

`make scaling` and `make perf` are how you check a change for complexity or
throughput regressions. `make scaling` runs each operation in
`harness/bench/scaling/` at growing sizes and fails if any goes superlinear;
that is the gate that catches O(n^2) bugs.

Under the hood, the conformance harness:

```bash
# Full upstream Lua 5.4.7 suite (the headline gate)
TEST_TIMEOUT_S=90 ./harness/run_official_all.sh

# A single official test
./harness/run_one_test.sh reference/lua-c/testes/strings.lua

# Inspect a failure
tail -120 harness/impl/official/<test>.out
```

`harness/impl/official/run_all.tsv` is the canonical scoring artifact. Do **not**
commit `harness/impl/official/*.out` — they regenerate on every run.

GC-sensitive changes should also run the canaries:

```bash
./harness/canaries/gc/run_canaries.sh
```

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
harness/                         # oracles, benchmarks, enforcement gates
docs/                            # architecture, performance, and porting docs
reference/                       # pinned upstream Lua 5.4.7 (the oracle; not edited)
```

## Code style (enforced)

These rules are mechanically checked by hooks in `.claude/hooks/` and will fail a
commit/Stop event if violated:

- **No inline `//` comments.** If something is worth explaining, put it in a doc
  string. (`// SAFETY:` justifications on `unsafe` blocks are the exception and
  are required.)
- **No fallback patterns** (`x || y || z`). Use a single source of truth; if data
  might be missing, fix the data path rather than papering over it.
- **No `String` / `&str` for Lua data.** Lua strings are byte strings — use
  `&[u8]` / `Vec<u8>` / `LuaString`.
- **`unsafe` is budgeted.** The workspace defaults to `#![forbid(unsafe_code)]`.
  Only `lua-gc` and `lua-cli` carry a budget; every `unsafe` block needs a
  `// SAFETY:` comment and must stay under the per-crate ceiling in
  `harness/unsafe-budgets.toml`. Run `.claude/hooks/unsafe-budget.sh` to check.
- **Never edit `reference/lua-c/testes/`.** The upstream tests are the oracle.
- **Every `crates/**/*.rs` carries a PORT STATUS trailer.** See `PORTING.md` §12.

## Making a change

1. Reproduce the target behavior as a tiny `lua-rs -e '...'` snippet first.
2. Patch the smallest cause; don't normalize a whole subsystem for one assertion.
3. Re-run the test that exposed the issue plus its adjacent gates.
4. Build before final verification: `cargo build --bin lua-rs`.
5. Keep temporary debugging (`eprintln!`, scratch files) out of the final diff.

The full debugging playbook lives in [CLAUDE.md](CLAUDE.md).

## Further reading

- [README.md](README.md) — what the project is and where it stands.
- [PORTING.md](PORTING.md) — the C→Rust translation rules.
- [HARNESS_DESIGN.md](HARNESS_DESIGN.md) — the harness and its enforcement model.
- [docs/LUA_SYSTEM_DEEP_DIVE.md](docs/LUA_SYSTEM_DEEP_DIVE.md) — architecture, GC,
  and the unsafe model.

## License

By contributing, you agree that your contributions are licensed under the
project's [MIT license](LICENSE).
