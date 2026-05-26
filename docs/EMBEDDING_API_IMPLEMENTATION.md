# Embedding API Implementation Status

Status as of commit `a096e24`: `lua-rs-runtime` has a Rust-native embedding API.
It is a preview surface, but the core substrate is implemented and verified:
owned rooted handles, re-entrant callbacks, captured Rust closures, userdata
methods/metamethods, and conversion traits.

This document describes what exists now. The design rationale lives in
[docs/design/EMBEDDING_API.md](design/EMBEDDING_API.md), and the original
implementation plan lives in
[docs/design/EMBEDDING_API_SPEC.md](design/EMBEDDING_API_SPEC.md).

## Public Surface

The primary API lives in `crates/lua-rs-runtime/src/lib.rs`.

- `Lua`: cheap-clone, single-threaded embedding handle.
- `Lua::new`, `Lua::try_new`, `Lua::with_hooks`: construct a state with parser,
  stdlib, and optional host hooks.
- `Lua::load(...).set_name(...).exec()` and `Lua::load(...).eval()`: run chunks.
- `Lua::globals`, `Lua::create_table`, `Lua::create_string`,
  `Lua::create_function`, `Lua::create_function_mut`, `Lua::create_userdata`.
- Owned handles: `Value`, `Table`, `Function`, `LuaString`, `AnyUserData`,
  and a rooted `Thread` wrapper.
- Table/function operations: `Table::get`, `Table::set`, `Table::len`,
  `Function::call`.
- Userdata traits: `UserData`, `UserDataMethods`, `MetaMethod`.
- Conversion traits: `IntoLua`, `FromLua`, `IntoLuaMulti`, `FromLuaMulti`.
- Conversion helpers: primitives, strings, `Option<T>`, `Vec<T>`, `HashMap<K,V>`,
  tuples up to three values, and `Variadic<T>`.

`LuaRuntime` remains available for the older low-level runtime shape and can be
converted into `Lua` with `LuaRuntime::into_lua`.

## Example

```rust
use lua_rs_runtime::{Lua, Result, UserData, UserDataMethods};

#[derive(Default)]
struct Counter {
    value: i64,
}

impl UserData for Counter {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method_mut("inc", |_, this, by: i64| {
            this.value += by;
            Ok(this.value)
        });
        methods.add_method("get", |_, this, ()| Ok(this.value));
    }
}

fn main() -> Result<()> {
    let lua = Lua::new();
    let globals = lua.globals();

    let shout = lua.create_function(|_, text: String| Ok(text.to_uppercase()))?;
    globals.set("shout", shout)?;
    globals.set("counter", Counter::default())?;

    let result: (String, i64) = lua
        .load(r#"
            counter:inc(2)
            counter:inc(3)
            return shout("lua-rs"), counter:get()
        "#)
        .eval()?;

    assert_eq!(result, ("LUA-RS".to_string(), 5));
    Ok(())
}
```

## Implementation Model

### Rooted Handles

External Rust handles are anchored by `ExternalRootSet` on `GlobalState`. The VM
traces that root set during collection, so Rust-owned `Table`, `Function`,
`LuaString`, `AnyUserData`, and collectable `Value` variants keep their Lua
referents alive.

Clone semantics use the simple model from the implementation spec: each clone
creates a fresh external root key. `Drop` unroots that key exactly once. Stale
keys include a generation, so a dropped handle cannot accidentally observe a
later value after slot reuse.

### Re-entrant `Lua`

`Lua` owns the VM state behind a boundary `RefCell`, but bytecode execution still
runs with direct `&mut LuaState`. Re-entry is handled at the Rust/Lua boundary:
while the VM is inside a callback, the active state pointer is available to the
callback-side `Lua` handle. This keeps borrow checks and dynamic dispatch out of
the opcode loop.

There is one audited unsafe bridge in `Lua::active_state_mut`. It is local,
documented with a `SAFETY` comment, and covered by callback/re-entry tests.

### Captured Rust Callbacks

Rust callbacks are not stored permanently in `GlobalState.c_functions`. Instead:

- one shared bare C trampoline is registered in the existing C-function table;
- each captured Rust callback is stored in a collectable userdata payload;
- a Lua C closure points at the shared trampoline and carries that userdata as
  an upvalue;
- the trampoline recovers the payload from the current call frame and invokes
  the Rust closure.

That shape lets dropped `Function` handles release captured Rust state after GC,
including captured Lua handles.

### Userdata and Metamethods

`Lua::create_userdata` stores Rust values inside `Rc<dyn Any>` payloads with
runtime borrow tracking. `AnyUserData::borrow` and `borrow_mut` expose typed
borrows and report Lua runtime errors for borrow conflicts.

`UserDataMethods` builds a metatable backed by the same closure machinery as
`create_function`. Supported metamethod names include `__index`, `__newindex`,
arithmetic/comparison methods, `__call`, and `__tostring`.

### GC Allocation Boundaries

Embedding-side allocations that create GC-managed objects now run under a
`lua_gc::HeapGuard`. This includes table/string creation, callback closure
payloads, userdata allocation, and parser-hook closure allocation.

If a Rust handle is dropped during GC sweep, mutating `GlobalState.external_roots`
would conflict with the collector's immutable borrow. Runtime handle drops use a
best-effort unroot path and queue pending external unroots until the next safe
embedding boundary.

## Verification

The landed implementation was verified with:

```bash
cargo test -p lua-rs-runtime --lib
cargo test -p lua-vm --lib external_root
cargo check -p lua-rs-runtime
cargo build -p lua-cli --bin lua-rs
cargo build --release -p lua-cli
./harness/canaries/gc/run_canaries.sh
.claude/hooks/unsafe-budget.sh
target/debug/lua-rs reference/lua-5.4.7-tests/gc.lua
target/debug/lua-rs reference/lua-5.4.7-tests/gengc.lua
TEST_TIMEOUT_S=60 ./harness/run_official_all.sh
cargo check --manifest-path ../bms-lua-rs/bevy_mod_scripting_lua_rs/Cargo.toml
```

Observed results:

- `lua-rs-runtime` tests: 14/14 pass.
- VM external-root tests: 2/2 pass.
- GC canaries: 10/10 pass across incremental and generational modes.
- Official `gc.lua` and `gengc.lua`: both reach `OK`.
- Full official suite: 44/44 pass.
- Downstream `bms-lua-rs` proof-of-concept check: pass.

Miri was not run because the local stable Apple Silicon toolchain did not have
Miri available.

## Performance

The embedding work is intended to keep cost at Rust/Lua boundaries and avoid
the VM opcode dispatch loop. The full benchmark matrix after `a096e24` reported
an overall wall-clock ratio of `1.31x` lua-rs/reference over the harness
workloads. The quick smoke remained stable:

- `fibonacci`: `1.90x` in the full matrix after `1.91x` in the smoke.
- `mandelbrot`: `1.88x` in both runs.

Benchmark artifacts:

- `harness/bench/results/20260526T140617Z-a096e24-compare.tsv`
- `harness/bench/results/20260526T140617Z-a096e24-compare.json`

## Known Limits

This is not a full `mlua` clone.

- No async API.
- No scoped-handle API like `mlua::Scope`.
- Tuple conversion coverage is intentionally small.
- No serde integration.
- `Thread` is only a rooted value wrapper, not a polished coroutine API.
- Error types are still `LuaError` shaped; they are not yet a rich public
  `mlua::Error` equivalent.
- Host hooks are still mostly function-pointer based; closure-capable host hooks
  are future work.
- The API is preview-level and not yet published as a standalone stable crate.

## Future Opportunities

The next useful work falls into five tracks:

1. Stabilization and examples.
   Add rustdoc examples, crate-level docs, bms-oriented migration notes, and a
   small standalone embedding example crate.

2. mlua parity where it matters.
   Add scoped handles, broader tuple impls, richer error variants, more userdata
   helpers, and ergonomic API aliases where direct bms/mlua migration asks for
   them.

3. Soundness hardening.
   Add Miri coverage once available, randomized create/clone/drop/GC stress
   tests, callback-GC torture tests, and leak checks for external-root slots.

4. Sandboxed embedding.
   Build `Lua::sandboxed` or a builder API for stdlib selection, instruction
   budgets, memory limits, fuel-style interruption, host-hook policies, and
   deterministic time/randomness. See [docs/design/SANDBOXING.md](design/SANDBOXING.md).

5. Ecosystem integration.
   Continue the bms backend port, keep WASM host embedding aligned with the new
   `Lua` API, and treat any C API/ABI compatibility work as a separate subsystem
   rather than part of the Rust embedding API.
