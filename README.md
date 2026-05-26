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
- Rust-native embedding API with owned handles, captured Rust callbacks, and
  userdata/metamethod support in `lua-rs-runtime`.
- Mostly safe Rust, with unsafe isolated to audited GC, dynamic-loading, and
  WASM pointer-ABI boundaries.
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

## Rust Embedding API

`lua-rs-runtime` exposes a preview Rust-native embedding API shaped after
`mlua` at the handle, callback, conversion, and userdata layers. It supports
owned GC-rooted handles (`Value`, `Table`, `Function`, `LuaString`,
`AnyUserData`), closure-based `create_function` / `create_function_mut`,
re-entrant callbacks, userdata methods and metamethods, and conversion traits
(`IntoLua`, `FromLua`, `IntoLuaMulti`, `FromLuaMulti`).

```rust
use lua_rs_runtime::{Lua, Result};

fn main() -> Result<()> {
    let lua = Lua::new();
    let globals = lua.globals();

    let f = lua.create_function(|_, name: String| {
        Ok(format!("hello, {name}"))
    })?;
    globals.set("greet", f)?;

    let out: String = lua.load(r#"return greet("lua-rs")"#).eval()?;
    assert_eq!(out, "hello, lua-rs");
    Ok(())
}
```

The implementation status, soundness model, verification evidence, and future
work are documented in
[docs/EMBEDDING_API_IMPLEMENTATION.md](docs/EMBEDDING_API_IMPLEMENTATION.md).
The original design rationale and build spec live in
[docs/design/EMBEDDING_API.md](docs/design/EMBEDDING_API.md) and
[docs/design/EMBEDDING_API_SPEC.md](docs/design/EMBEDDING_API_SPEC.md).

## Conformance

The upstream Lua 5.4.7 test suite runs unmodified against `lua-rs`, with output
diffed against reference C:

```bash
TEST_TIMEOUT_S=90 ./harness/run_official_all.sh   # 44/44 PASS
```

This is evidence of Lua source/runtime compatibility. It does not imply C API or
ABI compatibility, and stock Lua C modules that expect `liblua` won't load.

## Safety

Most crates build under `#![forbid(unsafe_code)]`. The trusted unsafe surface is
budgeted in `lua-gc` for the collector, `lua-cli` for optional dynamic-library
loading, and the dedicated `lua-wasm` / `lua-wasm-smoke` crates for the
WebAssembly linear-memory pointer ABI. The core VM, stdlib, parser, runtime
helper, and package wrapper stay outside that unsafe surface. It is not 100%
safe Rust yet. See [docs/LUA_SYSTEM_DEEP_DIVE.md](docs/LUA_SYSTEM_DEEP_DIVE.md).

## Performance

`lua-rs` is benchmarked against reference Lua 5.4.7 on every commit. At the
latest commit the geomean wall time is ~1.27× C and peak memory ~1.96× C, with
some workloads faster than C. It is competitive with C, not faster, and is not
LuaJIT. [Live dashboard.](https://ianm199.github.io/lua-rs/harness/bench/history/)

## WebAssembly

`lua-rs` also ships as [`lua-rs-wasm`](https://www.npmjs.com/package/lua-rs-wasm),
a browser and Node package for running Lua inside WebAssembly without bundling
the C Lua interpreter.

```bash
npm install lua-rs-wasm
```

Try it in the browser:
[ianm199.github.io/lua-rs/examples/wasm-browser/](https://ianm199.github.io/lua-rs/examples/wasm-browser/).

Use it from a browser or bundler:

```js
import { loadLuaRs, luaRsWasmUrl } from "lua-rs-wasm";

const { lua } = await loadLuaRs(luaRsWasmUrl, {
  onStdout: (chunk) => console.log(chunk),
});

lua.exec(`
local function fib(n)
  if n < 2 then return n end
  return fib(n - 1) + fib(n - 2)
end

print("fib(20) = " .. fib(20))
`);
```

Use it from Node:

```js
import { loadLuaRsNode } from "lua-rs-wasm/node";

const { lua } = await loadLuaRsNode({
  onStdout: (chunk) => process.stdout.write(chunk),
});

lua.exec('print("hello from lua-rs wasm")');
```

The WebAssembly package targets `wasm32-unknown-unknown`. That means there is no
ambient operating system: stdout, stdin, environment variables, time, and file
access come from JS host hooks. The package provides a small in-memory host for
common browser/Node use cases, including `print`, `io.open`, `require` from
provided files, and stateful `lua.exec(...)` calls. The native `lua-rs` CLI is
still the right choice for terminal use, and WASI support is separate future
work.

For lower-level embedding details, see
[harness/wasm/README.md](harness/wasm/README.md). For npm release steps, see
[docs/NPM_WASM_PUBLISHING.md](docs/NPM_WASM_PUBLISHING.md).

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
- Stabilize and broaden the Rust embedding API: richer mlua parity, scoped
  handles, better examples, Miri/fuzz coverage, sandbox presets, and future
  async/fuel hooks. See [docs/FUTURE_GOALS.md](docs/FUTURE_GOALS.md) and
  [docs/EMBEDDING_API_IMPLEMENTATION.md](docs/EMBEDDING_API_IMPLEMENTATION.md).

## Project layout

```
crates/
  lua-lex, lua-parse, lua-code   # lexer, parser, bytecode compiler
  lua-vm                         # register VM and core runtime
  lua-types                      # LuaValue, tables, strings, errors
  lua-gc                         # garbage collector
  lua-stdlib                     # standard library
  lua-coro                       # coroutines
  lua-rs-runtime                 # Rust embedding API and host-hook setup
  lua-wasm                       # bare wasm32 embedding artifact
  lua-cli                        # the `lua-rs` binary
  lua-wasm-smoke                 # bare wasm32 runtime smoke harness
harness/                         # test and benchmark scripts
reference/                       # pinned upstream Lua 5.4.7 (used to diff against)
```

## Development

```bash
cargo build --bin lua-rs
TEST_TIMEOUT_S=90 ./harness/run_official_all.sh                # full suite (44/44)
./harness/run_one_test.sh reference/lua-c/testes/strings.lua   # one test
RUSTFLAGS='-Awarnings' cargo build --target wasm32-unknown-unknown -p lua-wasm --release
node harness/wasm/runtime-smoke.mjs
node harness/wasm/browser-smoke.mjs
npm run build:wasm --prefix packages/lua-rs-wasm
npm test --prefix packages/lua-rs-wasm
npm run test:install --prefix packages/lua-rs-wasm
./harness/check_wasm_package.sh
```

## License

`lua-rs` is a port of [Lua](https://www.lua.org/) (created by Roberto
Ierusalimschy, Luiz Henrique de Figueiredo, and Waldemar Celes at PUC-Rio). Lua
and this port are both MIT-licensed. See [LICENSE](LICENSE).
