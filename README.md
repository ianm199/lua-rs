# lua-rs

A Lua 5.4.7 interpreter written in Rust. It runs Lua programs as a standalone
binary with no C dependency, and passes the full upstream PUC-Rio test suite
(44/44).

[![CI](https://github.com/ianm199/lua-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/ianm199/lua-rs/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/lua-cli.svg?label=crates.io%2Flua-cli)](https://crates.io/crates/lua-cli)
[![license](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![tests](https://img.shields.io/badge/upstream%20suite-44%2F44-0f8f68.svg)](#conformance)

```bash
cargo install lua-cli      # the crate is `lua-cli`; the binary it installs is `lua-rs`
lua-rs -e 'print("hello")'
```

## Highlights

- Passes the full upstream Lua 5.4.7 test suite (44/44).
- A standalone binary: no `liblua`, no C interpreter, no C toolchain to build it.
- Mostly safe Rust. The only `unsafe` is a small, audited core in the garbage
  collector and the optional dynamic-library loader.
- Competitive with reference C (~1.3× geomean wall time), benchmarked per commit.

## Usage

```bash
lua-rs                          # REPL
lua-rs script.lua               # run a file
lua-rs -e 'print(1 + 2)'        # run a one-liner
echo 'print("hi")' | lua-rs -   # read from stdin
lua-rs -v                       # version
```

`lua-rs` follows the standard `lua` CLI. Build from source with
`cargo build --release --bin lua-rs`.

## Conformance

The upstream Lua 5.4.7 test suite runs unmodified against `lua-rs`, with output
diffed against reference C:

```bash
TEST_TIMEOUT_S=90 ./harness/run_official_all.sh   # 44/44 PASS
```

This is evidence of Lua source/runtime compatibility. It does not imply C API or
ABI compatibility, and stock Lua C modules that expect `liblua` won't load.

## Safety

Most crates build under `#![forbid(unsafe_code)]`. The only `unsafe` is in the
garbage collector (`lua-gc`) and the optional dynamic-library loader (`lua-cli`);
both are budgeted and audited. It is not 100% safe Rust yet. See
[docs/LUA_SYSTEM_DEEP_DIVE.md](docs/LUA_SYSTEM_DEEP_DIVE.md).

## Performance

`lua-rs` is benchmarked against reference Lua 5.4.7 on every commit. At the
latest commit the geomean wall time is ~1.27× C and peak memory ~1.96× C, with
some workloads faster than C. It is competitive with C, not faster, and is not
LuaJIT. [Live dashboard.](https://ianm199.github.io/lua-rs/harness/bench/history/)

## LuaRocks

`lua-rs` runs the stock LuaRocks 3.11.1 client and installs pure-Lua rocks
(verified with `inspect`, `dkjson`, `argparse`, `middleclass`, `say`, and
`luassert`). Native C rocks are not supported yet.

LuaRocks looks for a `lua` interpreter on `PATH`, so point it at `lua-rs`:

```bash
curl -sSL https://luarocks.org/releases/luarocks-3.11.1.tar.gz | tar xz -C /tmp
mkdir -p /tmp/lua-bin
ln -sf "$(command -v lua-rs)" /tmp/lua-bin/lua
ln -sf "$(command -v lua-rs)" /tmp/lua-bin/lua5.4

HOME=/tmp/lua-rs-home PATH="/tmp/lua-bin:$PATH" \
LUA_PATH="/tmp/luarocks-3.11.1/src/?.lua;/tmp/luarocks-3.11.1/src/?/init.lua" \
  lua-rs /tmp/luarocks-3.11.1/src/bin/luarocks --tree /tmp/rocks install inspect

LUA_PATH="/tmp/rocks/share/lua/5.4/?.lua;;" lua-rs -e 'print(require("inspect")({1, 2, 3}))'
```

## Future goals

- Get to full safety: drive the remaining `unsafe` (in the garbage collector and
  the dynamic-library loader) to zero, so the whole runtime is safe Rust.
- Reach performance parity with reference Lua 5.4.
- A Rust embedding API: run Lua from Rust as a crate, with no C toolchain. See
  [docs/FUTURE_GOALS.md](docs/FUTURE_GOALS.md).

## Project layout

```
crates/
  lua-lex, lua-parse, lua-code   # lexer, parser, bytecode compiler
  lua-vm                         # register VM and core runtime
  lua-types                      # LuaValue, tables, strings, errors
  lua-gc                         # garbage collector
  lua-stdlib                     # standard library
  lua-coro                       # coroutines
  lua-cli                        # the `lua-rs` binary
harness/                         # test and benchmark scripts
reference/                       # pinned upstream Lua 5.4.7 (used to diff against)
```

## Development

```bash
cargo build --bin lua-rs
TEST_TIMEOUT_S=90 ./harness/run_official_all.sh                # full suite (44/44)
./harness/run_one_test.sh reference/lua-c/testes/strings.lua   # one test
```

## License

`lua-rs` is a port of [Lua](https://www.lua.org/) (created by Roberto
Ierusalimschy, Luiz Henrique de Figueiredo, and Waldemar Celes at PUC-Rio). Lua
and this port are both MIT-licensed. See [LICENSE](LICENSE).
