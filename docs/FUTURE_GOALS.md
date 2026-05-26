# Future Goals

This document separates the compatibility targets for `lua-rs`. They are often
collapsed into one phrase, but they are very different engineering projects.

## Current Target: Lua Source Compatibility

The current project target is Lua 5.4.7 source/runtime compatibility:

- parse and execute Lua source;
- implement Lua 5.4 value, table, closure, coroutine, metatable, error, and GC
  semantics;
- provide the Lua standard libraries through the Rust runtime;
- pass the upstream Lua 5.4.7 official test suite in the repo harness.

As of 2026-05-26, the harnessed official suite passes 44/44 tests.

That is strong evidence for Lua language compatibility. It is not the same as
being a drop-in replacement for PUC-Rio Lua's C API or binary ABI.

## Current Rust-Native Embedding API

`lua-rs-runtime` now has a preview Rust-native embedding interface:

- create a Lua state from Rust;
- load source chunks;
- call Lua functions from Rust;
- expose captured Rust functions and user data to Lua;
- hold Lua values from Rust through owned GC-rooted handles;
- convert common Rust values to and from Lua values;
- run explicit garbage collection;
- report errors without C `longjmp` semantics;
- keep the public API safe where possible and explicitly isolate unsafe internals.

The API is intentionally Rust-first and mlua-shaped at the handle, callback,
conversion, and userdata layers. It does not mimic `lua.h`, and it is not C API
compatibility. Current implementation details are in
[docs/EMBEDDING_API_IMPLEMENTATION.md](EMBEDDING_API_IMPLEMENTATION.md).

Important remaining embedding work:

- stabilize the public API shape and add rustdoc examples;
- broaden mlua parity where real consumers need it;
- add scoped handles, richer error variants, serde support, and broader tuple
  coverage;
- add Miri and randomized root/GC/callback stress testing;
- add a sandbox builder for stdlib selection, memory limits, instruction/fuel
  budgets, and deterministic host hooks.

## Why a Pure-Rust Lua for Embedding

The Rust-native embedding API is not just an ergonomics nicety. A pure-Rust Lua
has two clusters of advantages over today's C-backed bindings — bindings that
link PUC-Rio Lua and expose a Rust wrapper around the C interpreter.

### 1. Build and deployment

Linking C Lua drags the entire C build model along with it:

- The build needs a C toolchain, and cross-compilation inherits all of C's
  cross-compilation pain.
- A pure-Rust Lua is just a crate: `cargo build` everywhere, trivial
  cross-compilation, no `cc` or `make`, clean reproducible builds.
- The sharpest version is **WASM and embedded targets**. Pure Rust compiles to
  `wasm32` cleanly, whereas getting C Lua into a WASM module is genuinely
  painful. If the deployment target is unusual, that is a concrete win.

### 2. Safety and sandboxing of untrusted scripts

This is the real differentiator, and it is two distinct things.

**Memory safety of the implementation, not just the wrapper.** A C-backed
binding can give a safe *API*, but the interpreter underneath is still C — so a
bug in the Lua core is a memory-safety bug in the host process. A
mostly-safe-Rust VM means the *implementation* is memory-safe, not only the
wrapper around it.

**Real resource sandboxing.** C Lua lets you bolt on instruction hooks and a
custom allocator, but it is bolted on and sharp-edged. Crucially, C Lua's error
model is `longjmp`, and bridging `longjmp` with Rust's stack unwinding and
destructors is one of the hardest, most soundness-sensitive parts of any
Lua-in-Rust binding — existing bindings work hard to contain it. A pure-Rust Lua
built **stackless with a fuel system** gives, by construction:

- bounded CPU and memory;
- guaranteed return-to-caller (no runaway native frames);
- a native `Result` error model;
- no `longjmp` hazard at all.

For multi-tenant "run untrusted user scripts" workloads, that is a qualitative
difference, not a marginal one.

### Smaller wins that ride along

- A stackless design makes Lua-coroutine / Rust-async interleaving natural, where
  C Lua fights you.
- A native implementation can let Rust values participate in Lua's GC more
  seamlessly than a C binding's lifetime-juggling allows.

### Honest status

The Rust-native embedding API now exists, but the sandboxing guarantees above do
not exist yet. `lua-rs` today is a runtime, CLI, WASM package, and preview Rust
embedding API, not a hardened embedding sandbox. Its current incremental
mark-and-sweep GC is not the stackless + fuel design this argument assumes.
Sandboxing remains the destination that justifies continued embedding work, not
a guarantee that ships today.

## Possible Long-Term Goal: C API Compatibility

C API compatibility would mean C code can embed `lua-rs` through functions shaped
like Lua 5.4's public API:

- `lua_newstate`, `lua_close`;
- stack operations such as `lua_gettop`, `lua_settop`, `lua_pushvalue`;
- loading and calling APIs such as `lua_load`, `lua_pcallk`, `lua_callk`;
- table/global/registry APIs such as `lua_getfield`, `lua_setfield`,
  `lua_rawgeti`, `luaL_ref`;
- userdata, metatable, finalizer, and uservalue support;
- `lauxlib.h` helpers such as `luaL_check*`, `luaL_error`, `luaL_Buffer`,
  `luaL_newmetatable`, and `luaL_requiref`;
- debug APIs such as `lua_getstack`, `lua_getinfo`, hooks, locals, and upvalues;
- allocator compatibility through `lua_Alloc`;
- C-facing headers and a linkable library artifact.

This is plausible as a compatibility layer, but it should be treated as a
separate subsystem. It would need its own C conformance tests, small embedding
programs, and native module fixtures.

## Hardest Target: ABI Drop-In Compatibility

ABI drop-in compatibility would mean existing C host programs or compiled Lua C
modules can link or load against `lua-rs` unchanged, as if it were `liblua`.

That requires more than exposing similarly named functions:

- exact exported symbol names and platform linker behavior;
- C-compatible type sizes and calling conventions for `lua_State`,
  `lua_CFunction`, `lua_KFunction`, `lua_Integer`, `lua_Number`, `lua_Debug`,
  `lua_Reader`, `lua_Writer`, and `lua_Alloc`;
- stack-index, pseudo-index, registry, upvalue, error, and continuation
  behavior matching PUC-Rio Lua closely enough for real C modules;
- support for arbitrary `.so`/`.dylib` Lua modules calling into the C API;
- userdata and finalization behavior that matches C module expectations;
- allocator behavior compatible with `lua_newstate`;
- a safe policy for PUC-Rio Lua's `setjmp`/`longjmp` style error unwinding.

The unwinding model is the largest safety and design issue. PUC-Rio Lua uses
long-jump based error propagation. Rust code cannot safely assume arbitrary C
`longjmp` through Rust frames, and Rust unwinding through C frames is also
constrained. Any serious ABI project needs an explicit boundary design before
implementation.

## Suggested Order

1. Keep source compatibility green with the official suite.
2. Stabilize and harden the Rust-native embedding API.
3. Add sandbox controls to the Rust embedding API.
4. Build a small C API compatibility crate as an experiment.
5. Add C fixture programs that cover stack operations, protected calls, registry
   references, userdata, finalizers, and callbacks.
6. Add simple native-module loading fixtures.
7. Decide whether `longjmp`-compatible behavior is feasible without compromising
   the safety model.
8. Only claim C API compatibility after the C fixture suite is broad and green.
9. Treat ABI drop-in compatibility as a separate release line unless proven
   practical.

## Public Claim Guidance

Good current phrasing:

> `lua-rs` is a Lua 5.4.7-compatible runtime implemented in Rust. The preview
> release targets Lua source/runtime compatibility first and includes a preview
> Rust-native embedding API. C API compatibility is a future goal.

Avoid claiming:

- complete PUC-Rio Lua C API compatibility;
- ABI drop-in compatibility with `liblua`;
- compatibility with arbitrary existing Lua C modules;
- completely safe Rust.
