# wasm32 Host Boundary Spec

## Summary

`lua-rs` has a credible harness-level `wasm32-unknown-unknown` story, but the
claim must stay precise. The interpreter core and stdlib can typecheck for bare
WASM:

```sh
cargo check --target wasm32-unknown-unknown \
  -p lua-types -p lua-lex -p lua-parse -p lua-code \
  -p lua-gc -p lua-vm -p lua-stdlib -p lua-coro
```

That proves the crates compile for the target. Runtime support is proven by the
separate `lua-wasm` artifact and the Node/browser smoke harnesses below. It
does not mean every Lua standard-library function has an ambient OS in bare
WASM; host capabilities are explicit.

The goal is:

- pure Lua compute runs under `wasm32-unknown-unknown`;
- OS-facing stdlib functions never accidentally call unsupported Rust `std`
  stubs;
- unsupported host capabilities fail as normal Lua errors or Lua failure
  tuples, not Rust panics or WASM traps;
- embedders can explicitly provide selected host capabilities such as output,
  file loading, time, environment variables, and process execution.

This is not a VM rewrite. It is a host-boundary cleanup around stdlib.

## Current Implementation Status

Implemented:

- `auxlib::load_filex` now reads through `GlobalState::file_loader_hook`
  instead of calling `std::fs::read` directly.
- `GlobalState` now has `stdout_hook` and `stderr_hook` output hooks.
- `GlobalState` now has `stdin_hook` for host-provided standard input.
- `LuaState::write_output` uses `stdout_hook` when installed.
- `io.stdin`, `io.stdout`, and `io.stderr` standard stream handles use host
  hooks when installed.
- `GlobalState` now has host hooks for env lookup, Unix time, CPU clock,
  entropy, and temp names.
- `os.getenv`, `os.time()` / `os.date()` without explicit timestamps,
  `os.clock`, package path initialization, `os.tmpname`, `io.tmpfile`, default
  `math.randomseed`, and table sort pivot randomization now route through those
  hooks or deterministic/unsupported bare-WASM fallbacks.
- `os.clock` has no portable `std` source for process CPU time (no
  `CLOCK_PROCESS_CPUTIME_ID` equivalent). Without a `cpu_clock_hook` it reports
  monotonic wall time elapsed since the first call — the same substitution
  wasi-libc and Emscripten make for C's `clock()`. The native CLI installs a
  hook with a process-start baseline so values match C's "since program start"
  semantics; a host wanting true CPU time can install one backed by `cpu-time`.
- `debug.debug()` reports a Lua error on bare WASM instead of attempting an
  interactive terminal read.
- `lua-cli` installs native stdout/stderr, env, time, CPU clock, entropy, and
  temp-name hooks alongside the existing file and process hooks.
- `lua-rs-runtime` provides a small embedding helper that creates a state,
  installs the parser hook, installs selected `HostHooks`, opens the standard
  libraries, and runs chunks.
- On bare `wasm32-unknown-unknown`, missing stdout/stderr hooks report
  unsupported instead of touching Rust stdio fallbacks.
- `crates/lua-wasm` builds the reusable bare `wasm32-unknown-unknown` embedding
  artifact. It exports allocation helpers, a stateful `lua_rs_wasm_run`,
  `lua_rs_wasm_reset`, and last-error readers for JS embedders.
- `crates/lua-wasm-smoke` builds a bare `wasm32-unknown-unknown` `cdylib`.
- `packages/lua-rs-wasm` provides a small reusable JS host wrapper, and
  `harness/wasm/runtime-smoke.mjs` uses it to instantiate `lua-wasm` in Node.
- `harness/wasm/lua-rs-host.mjs` remains as a compatibility re-export for old
  harness imports.
- `harness/wasm/browser-smoke.mjs` serves `browser-smoke.html` locally and
  drives Chrome/Chromium through the DevTools protocol, running the same JS host
  wrapper and smoke assertions in an actual browser.
- The smoke scenario passes real JS imports into the `.wasm` module
  for stdout, stdin, env lookup, Unix time, Lua source-file loading, and JS-backed
  `io.open` handles, then verifies Lua code can call `print`, `io.write`,
  `io.read`, `os.getenv`, `os.time`, `require("greeter")`, and file read/write/seek
  operations plus `file:setvbuf` through those host callbacks. It also verifies
  successive JS `lua.exec(...)` calls share one Lua state until reset, a
  directory-read failure reports Lua's normal `false, message, errno` tuple, and
  a failed Lua chunk exposes its error message through last-error exports.
- The WASM smoke exports `lua_rs_wasm_alloc`, `lua_rs_wasm_dealloc`, and
  `lua_rs_wasm_run_hosted_script`, so the Node host can copy arbitrary Lua
  source into WASM memory and execute it through the same host boundary.

Still open:

- The JS wrapper package can build and pack `dist/lua_wasm.wasm`, and the
  tarball install smoke imports it by package name from a temporary project.
  The manual `Publish lua-rs-wasm` GitHub Actions workflow can dry-run or publish
  it to npm once the repository has an `NPM_TOKEN` secret.
- The filesystem smoke covers representative source loading, read/write/seek,
  buffering, and errno propagation. It is not yet a broad virtual-filesystem
  conformance suite.
- WASI is not implemented; it should be a separate host backend.

## Target Model

`wasm32-unknown-unknown` means "WebAssembly with no standardized OS." There is
no ambient filesystem, stdout, process table, environment, wall clock, or dynamic
library loader. Rust `std` APIs may still compile for this target, but OS-backed
operations can be unsupported, stubbed, or panic/trap when actually called.

That means `cargo check` can pass while a runtime call like `std::fs::read`,
`SystemTime::now`, or `std::io::stdout` is still the wrong implementation for
bare WASM.

`wasm32-wasip1` is different. WASI defines host imports for capabilities like
stdio, clocks, and preopened directories. WASI support should be treated as a
separate host backend, not conflated with bare `wasm32-unknown-unknown`.

## Existing Architecture

The repo already has the right shape in `lua-vm::state::GlobalState`. Several
OS-facing operations are optional hooks installed by the embedder:

- `parser_hook`
- `file_loader_hook`
- `file_open_hook`
- `stdin_hook`
- `stdout_hook`
- `stderr_hook`
- `env_hook`
- `unix_time_hook`
- `cpu_clock_hook`
- `entropy_hook`
- `temp_name_hook`
- `popen_hook`
- `file_remove_hook`
- `file_rename_hook`
- `os_execute_hook`
- `dynlib_load_hook`
- `dynlib_symbol_hook`
- `dynlib_unload_hook`

The native CLI installs real implementations backed by `std::fs`,
`std::process`, and `libloading`. A sandboxed or bare-WASM embedder can leave
hooks unset, or install only the capabilities it wants.

The host-boundary rule is:

> `lua-stdlib` should not directly call OS-backed `std` APIs. It should go
> through `GlobalState` host hooks, or return a normal Lua-level unsupported
> result when no hook exists.

## Capability Categories

### Pure Compute

These should work in bare WASM without host support:

- parser
- compiler/codegen
- VM execution
- GC
- arithmetic
- strings
- tables
- closures
- coroutines, as long as they remain VM-level coroutines and do not use OS
  threads

Example:

```lua
local function fib(n)
  if n < 2 then return n end
  return fib(n - 1) + fib(n - 2)
end

assert(fib(20) == 6765)
```

### Host Output/Input

Lua-facing examples:

- `print`
- `io.write`
- `io.read`
- `io.stdin`, `io.stdout`, `io.stderr`
- warning/debug output

Current good parts:

- `LuaState::write_output` uses `stdout_hook` when installed.
- `io.stdout` and `io.stderr` use output hooks when installed.
- `io.stdin` uses `stdin_hook` when installed.
- Bare WASM without output hooks reports unsupported instead of touching stdio
  stubs.

Implemented shape:

- Browser/JS embedders can route stdout to `console.log`.
- The Rust embedding surface is now `lua-rs-runtime::{Lua, LuaRuntime,
  HostHooks}`. `Lua` handles owned values, captured Rust callbacks, userdata,
  and conversions; `HostHooks` still provides the current WASM host capability
  hooks.
- Browser and application embedders still need closure-capable host hooks if
  they want hook callbacks that capture per-instance state.

Design note: `print` is so commonly used that a no-op default is tempting, but
silent output loss is surprising. For first-class support, prefer an explicit
output hook plus a test helper/buffered host. If no output hook exists, returning
a Lua error is more honest for `io.write`; `print` can be decided separately.

### File Loading And Filesystem

Lua-facing examples:

- `loadfile`
- `dofile`
- `require` Lua searcher
- `package.searchpath`
- `io.open`
- `io.tmpfile`
- `os.remove`
- `os.rename`
- `os.tmpname`

Current good parts:

- package searchers already use `file_loader_hook`.
- `loadfile` and `dofile` now use `file_loader_hook`.
- `io.open` already uses `file_open_hook`.
- `io.popen` already uses `popen_hook`.
- `os.remove` and `os.rename` already use file hooks.
- `io.tmpfile` and `os.tmpname` now use `temp_name_hook` when installed and
  fail cleanly on bare WASM when it is absent.
- The bare-WASM smoke harness now proves a read-only JS virtual filesystem for
  Lua source modules by loading `require("greeter")` through `file_loader_hook`.
- The same smoke harness now proves JS-backed `io.open` handles for basic read,
  write, seek, setvbuf, flush, and close flows through `file_open_hook`.
- Host-file read errors can provide errno/message data through
  `LuaFileHandle::last_error_info`; the smoke verifies a directory-read failure
  returns errno `21` to Lua.

Remaining refinement:

- Browser or application embedders can use the same file-loader shape for a
  read-only virtual filesystem, and the same file-handle shape for mutable
  files. The smoke does not yet cover a broad matrix of filesystem edge cases
  for host-provided handles.
- WASI should use a separate backend where filesystem access is capability
  gated through preopened directories.

### Environment

Lua-facing example:

- `os.getenv`

Current good part:

- `os.getenv` uses `env_hook` when installed and returns `nil` on bare WASM
  without an environment hook.

Remaining refinement:

- Decide whether a stricter sandbox mode should error on `os.getenv` instead of
  returning `nil`. C Lua's `os.getenv` naturally returns `nil` for absent vars,
  so the current missing-hook default is deliberately quiet.

### Time And Clock

Lua-facing examples:

- `os.time`
- `os.date`
- `os.clock`
- default `math.randomseed`
- table sort pivot randomization

Current good parts:

- `os.time()` / `os.date()` without explicit timestamps use `unix_time_hook`
  when installed and fail as Lua errors on bare WASM without a clock.
- default `math.randomseed` and table sort pivot randomization use
  `entropy_hook` when installed and deterministic fallback values when absent.

Remaining refinement:

- Browser/JS embedding can route the time hook to `Date.now`.
- WASI can route the time hook to WASI clocks.

Important distinction:

- `os.time()` with no argument needs a host wall clock.
- `os.time(table)` can be pure computation if implemented in UTC.
- `os.date(format, timestamp)` can be pure UTC formatting for explicit
  timestamps, but local timezone behavior needs host support.

### Process And Dynamic Loading

Lua-facing examples:

- `os.execute`
- `io.popen`
- `package.loadlib`
- C module loading through `require`

Current good parts:

- `os.execute` uses `os_execute_hook`.
- `io.popen` uses `popen_hook`.
- dynamic loading uses `dynlib_*` hooks.

Desired shape:

- Bare WASM should leave these hooks unset.
- Calls should return the normal Lua failure shape, not trap.
- Browser/unknown-unknown should not attempt dynamic library loading.

## Public API Direction

The repo currently uses `fn` pointer hooks on `GlobalState`. `lua-rs-runtime`
wraps that in a first-pass public helper:

```rust
use lua_rs_runtime::{HostHooks, LuaRuntime};

fn stdout(bytes: &[u8]) -> std::io::Result<()> {
    // Native hosts can write to stdout; WASM hosts can bridge this function to
    // a host callback.
    use std::io::Write as _;

    let mut out = std::io::stdout();
    out.write_all(bytes)
}

let mut lua = LuaRuntime::with_hooks(HostHooks::new().stdout(stdout))?;
lua.exec(br#"print("hello from lua-rs")"#, b"=example")?;
```

That helper is deliberately thin: it installs hooks, parser support, and stdlib
setup without forcing embedders to mutate `GlobalState` fields directly. It is
enough for static CLI-style hooks and for the current bare-WASM smoke harness.

For bare WASM, the current proof point is a reusable `lua-wasm` artifact plus a
small browser-compatible JS wrapper in `packages/lua-rs-wasm`.
`crates/lua-wasm` imports functions from the `lua_rs_host` module:

- `write_stdout(ptr, len) -> i32`
- `read_stdin(out_ptr, out_len) -> i32`
- `unix_time() -> i64`
- `env_len(ptr, len) -> i32`
- `env_read(name_ptr, name_len, out_ptr, out_len) -> i32`
- `file_len(ptr, len) -> i32`
- `file_read(path_ptr, path_len, out_ptr, out_len) -> i32`
- `open_file(path_ptr, path_len, mode_ptr, mode_len) -> i32`
- `file_read_byte(file_id) -> i32`
- `file_write(file_id, ptr, len) -> i32`
- `file_flush(file_id) -> i32`
- `file_seek(file_id, whence, offset) -> i64`
- `file_set_buf_mode(file_id, mode, size) -> i32`
- `file_error_code(file_id) -> i32`
- `file_error_len(file_id) -> i32`
- `file_error_read(file_id, out_ptr, out_len) -> i32`

`packages/lua-rs-wasm` supplies those imports from JS, reads/writes the
module's exported linear memory, exposes `loadLuaRs(...).lua.exec(...)`, calls
`lua_rs_wasm_run`, calls `lua_rs_wasm_reset` when the JS wrapper resets the Lua
state, reads last-error messages when a chunk fails, and verifies Lua stdlib
calls cross the boundary correctly, including `require` loading Lua source from
a JS `Map`, `io.read` reading from a JS stdin buffer, and `io.open`
reading/writing/seeking/configuring buffering on JS-backed files. The Node and
browser smoke scripts use that wrapper and check state persistence/reset plus
directory-read errno propagation. This is a real end-to-end host callback path;
the remaining external packaging step is dispatching the guarded npm publish
workflow with an npm token.

The same harness also uses the module's allocation exports to pass a dynamic Lua
source string from JS into WASM, then runs it with
`lua_rs_wasm_run`.

A polished embedding API eventually wants closure-capable hooks, because
browser and application embedders need captured state:

```rust
// Shape only, not current API.
LuaHost::builder()
    .stdout(|bytes| console_log(bytes))
    .clock(|| js_date_now())
    .load_file(|name| virtual_fs.read(name))
    .build();
```

There are two reasonable implementation paths:

1. Incremental path: keep the existing `fn` hooks, expose them through
   `HostHooks`, and use that for tests and early embedders. This is the current
   path.
2. Polished embedding path: introduce a host trait or boxed closure fields, then
   migrate the existing hooks behind that API. This is nicer for consumers but
   larger and touches more of `state.rs`.

For now, the incremental path is implemented. The trait/closure design should be
driven by a concrete browser, wasmtime, or application embedding consumer.

## Feature Flags

A Cargo feature is not the core solution. The core solution is explicit host
capabilities. Feature flags are still useful for:

- keeping CLI-only dependencies out of library/WASM builds;
- selecting native default host hooks;
- trimming binary size;
- preventing dependencies that do not compile for bare WASM from entering the
  graph.

Recommended eventual shape:

```toml
[features]
default = ["native-host"]
native-host = []
wasm-sandbox = []
wasi-host = ["native-host"]
```

But avoid target-arch branching as the primary design. `wasm32-unknown-unknown`
and `wasm32-wasip1` are both `wasm32` and have different host capability
stories.

## Implementation Slices

### Slice 1: Finish Existing File Loader Boundary

- Route `auxlib::load_filex` through `GlobalState::file_loader_hook`.
- Keep `None` as a Lua file-load failure, not a Rust error.
- Preserve BOM/shebang handling and chunk names.
- Run native CLI smoke tests for `loadfile`, `dofile`, and `require`.
- Re-run `cargo check --target wasm32-unknown-unknown` for core crates.

Status: implemented. `loadfile`, `dofile`, and Lua `require` source loading go
through the host file-loader boundary.

### Slice 2: Output Hook

- Add stdout/stderr hooks to `GlobalState`.
- Change `LuaState::write_output` to use the stdout hook.
- Change standard stream file handles in `io_lib` to use host stdio hooks.
- CLI installs real `std::io` hooks.
- Add a test host that captures output in memory.

Status: implemented. `print`, `io.write`, and the standard streams can cross
host output/input hooks, and the WASM smoke proves stdout capture.

### Slice 3: Time/Env/Temp Hooks

- Add `time_now` or `unix_time` hook.
- Add env lookup hook.
- Add temp name/temp file hook, or deliberately make tmpfile/tmpname unsupported
  without one.
- Move default `math.randomseed` and table pivot randomization off direct
  `SystemTime`.

This removes the remaining common bare-WASM trap risks.

Status: implemented for env, Unix time, entropy, temp names, `os.getenv`,
`os.time`, `os.date`, `os.tmpname`, `io.tmpfile`, default `math.randomseed`, and
table sort pivot randomization. Stdin is implemented as its own hook and covered
by the lower-level smoke crate.

### Slice 4: Runtime WASM Harness

`cargo check` is not enough. Add a real runtime test. Options:

- a small `wasm-bindgen-test` harness for browser/node;
- a tiny `cdylib` test crate exposing "run this Lua string" and called from a
  JS or wasmtime host;
- a separate WASI smoke test for `wasm32-wasip1`.

Required smoke cases:

- pure compute script succeeds;
- unsupported `io.open` / `os.execute` can be wrapped with `pcall` or return
  Lua failure values;
- output hook captures `print`.

Status: implemented as `lua-wasm`, `lua-wasm-smoke`, and the JS harness under
`harness/wasm`. The current smoke builds `lua-wasm` for
`wasm32-unknown-unknown`, instantiates it with Node and headless Chrome, and
checks JS-host-provided stdout/stdin/env/time/file-loader/file-open/seek/setvbuf
imports, file error imports, last-error exports, and JS-provided Lua source
execution. The lower-level `lua-wasm-smoke` crate still covers pure compute,
output hook capture, input/env hook capture, and missing-hook unsupported
behavior.

The smoke crate now uses `lua-rs-runtime::{LuaRuntime, HostHooks}` rather than
duplicating parser and stdlib setup logic.

### Slice 5: Docs

README wording should stay precise:

- "Core crates compile for `wasm32-unknown-unknown`" is true today.
- "Bare-WASM runtime support" is true at harness level: the `lua-wasm` artifact
  is instantiated and exercised in Node and a browser.
- "JS/browser wrapper package" exists locally at `packages/lua-rs-wasm`.
- "Published npm package" is true only after the manual publish workflow has
  uploaded the package to npm; until then claim local package/tarball readiness.
- "CLI works on wasm32" is false for bare WASM and should not be claimed.
- WASI should be documented separately if/when supported.

## Acceptance Criteria

- Core crates continue to check for `wasm32-unknown-unknown`.
- `lua-cli` remains native-only unless explicitly refactored.
- No bare-WASM path reachable from ordinary Lua code calls an OS-backed Rust
  `std` stub when the corresponding host hook is absent. Native compatibility
  fallbacks are acceptable only behind `cfg(not(wasm32-unknown-unknown))`.
- Unsupported host operations fail at the Lua boundary.
- Native CLI behavior and LuaRocks support do not regress.
- Actual WASM runtime smoke tests exist before public WASM support is claimed.
  The current smokes instantiate bare `wasm32-unknown-unknown` modules in Node
  and headless Chrome/Chromium.
- `./harness/check_wasm_package.sh` is the one-command gate for the current
  bare-WASM/package story: wasm cargo check, package build, package smoke,
  tarball install smoke, npm pack contents, Node smoke, low-level smoke, and
  browser smoke unless `WASM_SKIP_BROWSER=1`.
