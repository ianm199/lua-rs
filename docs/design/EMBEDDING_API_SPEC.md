# Embedding API — implementation spec

Handoff spec for the implementation agent. Rationale and design tradeoffs live in
[EMBEDDING_API.md](EMBEDDING_API.md); this document is the build plan: phases,
concrete types, integration points in the current codebase, soundness
invariants, and acceptance criteria.

Implementation status: this plan has been executed through commit `a096e24`.
Keep this file as the historical build spec and acceptance-contract record. The
current implementation status and future work are documented in
[docs/EMBEDDING_API_IMPLEMENTATION.md](../EMBEDDING_API_IMPLEMENTATION.md).

Goal: a Rust embedding API for lua-rs, mlua-shaped at the handle /
`create_function` / userdata layers, so the bms backend becomes a port of its
existing mlua backend rather than a rewrite. Build order targets the
bms → nano9 path: substrate first, conversion sugar last.

## Current state (verified against the tree)

- **Embedding entry point:** `lua-rs-runtime` (`LuaRuntime::new/with_hooks`,
  `exec(&mut self, src, name)`, `state()/state_mut()`, `HostHooks` builder).
- **Rust functions today:** `lua_CFunction = fn(&mut LuaState) -> Result<usize, LuaError>`
  (a **bare fn pointer**, stack-protocol: args on the Lua stack, return the
  pushed-result count). Stored in `GlobalState.c_functions: Vec<lua_CFunction>`,
  referenced by index, deduped by `fn_addr_eq`. `push_c_closure(f, n)` exists but
  `f` is still a bare fn. → **no captured state, no re-entrant context.**
- **GC:** `lua-gc::Heap::full_collect(roots)`; `trait Trace { fn trace(&self, &mut Marker) }`;
  roots are gathered by `GlobalState::trace` (`crates/lua-vm/src/trace_impls.rs`),
  which already traces `l_registry`, globals, threads, `to_be_finalized`, etc.
- **Registry:** `GlobalState.l_registry: LuaValue` with `LUA_RIDX_MAINTHREAD=1`,
  `LUA_RIDX_GLOBALS=2`. There is **no `luaL_ref`/`luaL_unref`** yet. The registry
  is already traced as a root — it is the anchor for the external root set.
- **External roots:** a dedicated `ExternalRootSet` now lives on `GlobalState`
  and is traced from `GlobalState::trace`. It uses generational keys so stale
  keys cannot observe reused slots. `LuaState` exposes root/get/replace/unroot
  helpers. This is the Phase-1 substrate; public owned handles are not built yet.
- **GC canaries:** `harness/canaries/gc/run_canaries.sh` writes a temporary Lua
  file before invoking `lua-rs`. Passing source as argv[1] is wrong because the
  CLI treats argv[1] as a filename.

The two limitations to remove: bare-fn (no capture) and `&mut self` (no
re-entrancy). Everything below follows.

## What is already landed

- `crates/lua-vm/src/state.rs`
  - `ExternalRootKey`
  - `ExternalRootSet`
  - `GlobalState.external_roots`
  - `LuaState::{external_root_value, external_rooted_value,
    external_replace_root, external_unroot_value}`
  - focused root-set tests for stale keys, slot reuse, and full-collection
    liveness
- `crates/lua-vm/src/trace_impls.rs`
  - `GlobalState::trace` marks all live external-root values
- `harness/canaries/gc/run_canaries.sh`
  - fixed to run generated temp files instead of treating inline Lua source as a
    filename

Verification for this landing:

- `cargo test -p lua-vm --lib external_root`
- `cargo build -p lua-cli --bin lua-rs`
- `./harness/canaries/gc/run_canaries.sh` => 10/10 PASS
- manual official `gc.lua` and `gengc.lua` runs to `/tmp` => both `OK`
- `cargo check -p lua-rs-runtime`
- `git diff --check`

## Phases and acceptance criteria

Each phase must keep the official suite at **44/44** (run from a clean worktree
against the built binary, per the project's verification pattern) and keep the
benchmark geomean unchanged (proof the hot loop is untouched).

### Phase 0 — shared, re-entrant state

Replace the `&mut self` access model with a shared handle that supports
re-entrancy, **without** putting a borrow/cell in the per-instruction dispatch
loop.

- Introduce `Lua` (or evolve `LuaRuntime`) as a cheap-clone, `!Send` handle.
- Do not hold a `RefCell` mutable borrow of the whole VM across callback entry.
  A Rust callback must be able to call back into Lua without colliding with an
  outer borrow.
- The VM still runs with direct `&mut LuaState` access while executing bytecode.
  Any cell/borrow/dynamic-dispatch cost belongs at the Rust<->Lua boundary, not
  in the per-instruction loop.
- A callback receives a context (`&Lua` / a `Context`) it can re-enter through.
  If this needs a small audited unsafe bridge to expose an mlua-shaped safe
  public API over the current `&mut LuaState` engine, keep that bridge tiny and
  documented.

Acceptance: a registered function can call back into the VM during a `pcall`
without aliasing; `bash harness/bench/compare.sh` geomean unchanged vs baseline;
44/44.

### Phase 1 — GC external root set + owned handles

- Done: add a dedicated external root slab on `GlobalState` and trace it from
  `GlobalState::trace` so rooted values are marked.
- Remaining: decide clone semantics for public handles. Either clone re-roots
  with a unique key, or `ExternalRootSet` grows refcounted entries. Prefer the
  simpler unique-key model unless profiling or API behavior proves it wasteful.
- Owned handle types, each holding a root key + a back-reference to `Lua`:
  `Value`, `Table`, `Function`, `LuaString` (add `Thread`, `AnyUserData` later).
  `Clone` re-roots (or bumps refcount); `Drop` unroots exactly once.

Acceptance: a Rust-held `Table` survives `collectgarbage("collect")` and is still
usable; handles drop without leaking root slots; **Miri-clean** on the
handle/root-set unit tests; 44/44.

### Phase 2 — `create_function` with captured state (the gate)

- Extend the C-function mechanism to store **boxed closures** alongside bare fns:
  `Rc<dyn Fn(&Lua, A) -> Result<R>>` (and a `_mut` variant via interior
  mutability). The existing `c_functions: Vec<lua_CFunction>` becomes a registry
  of callables that can be either a bare fn or a boxed closure.
- Invocation: marshal Lua-stack args → closure inputs, run the closure with the
  re-entrant `Lua` from Phase 0, marshal the result back to the stack, propagate
  `Err` as a Lua error. Captured Lua handles (Phase 1) keep their referents
  rooted for the closure's lifetime.

Acceptance: register a stateful closure (captures an `Arc`/counter and a stored
`Function`), call it from Lua, have it call back into Lua and return a value;
the **bms Gate-2 function-registry bridge** compiles against this; 44/44.

### Phase 3 — userdata + metamethods (required for bms, not deferred)

- `AnyUserData` handle (Phase 1 anchored) wrapping a Rust value, with runtime
  borrow tracking (RefCell-style) to bridge Rust aliasing vs Lua re-entrancy.
- `UserData` trait with `add_method`/`add_method_mut`/`add_meta_method` (mirror
  mlua's `UserDataMethods`), backed by Phase-2 closures. Support
  `__index`/`__newindex`/arithmetic metamethods.

Acceptance: a Rust struct exposed to Lua with `__index`/`__newindex`; the **bms
reflection bridge** (`reference.rs`, ~460 LoC) ports from its mlua version with
mechanical changes only; 44/44.

### Phase 4 — conversion sugar (trails)

`FromLua`/`IntoLua` + `FromLuaMulti`/`IntoLuaMulti` with blanket impls
(`i64/f64/bool/String/&str/&[u8]/Option<T>/Vec<T>/HashMap/tuples`), later a
derive. Serves the direct-embedder profile; bms marshals via its own
`ScriptValue` and doesn't need it.

## Target API (mlua-shaped — mirror these names)

```rust
// state
let lua = Lua::new();                 // !Send, cheap clone
let t: Table = lua.create_table()?;
let s: LuaString = lua.create_string("x")?;

// run
lua.load(src).set_name(name).exec()?;
let v: i64 = lua.load("return 2+3").eval()?;     // eval needs FromLua (Phase 4)
let g: Table = lua.globals();

// handles (owned, anchored)
t.set("k", v)?; let x: Value = t.get("k")?; let n = t.len()?;
let f: Function = t.get("fn")?; let r: Value = f.call(args)?;

// create_function (Phase 2)
let f = lua.create_function(|lua: &Lua, args: A| -> Result<R> { ... })?;
let f = lua.create_function_mut(|lua, args| { ... })?;       // FnMut

// userdata (Phase 3)
impl UserData for T {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) { ... }
    fn add_meta_methods<M: UserDataMethods<Self>>(m: &mut M) { ... }
}
let ud: AnyUserData = lua.create_userdata(value)?;
```

Error type: `lua_rs::Error` (RuntimeError(Value), conversion errors, borrow
errors). Callbacks return `Result<_, Error>`; Rust panics must never cross the
boundary; Lua errors are values.

## Integration points (where to touch)

- Root set + ref/unref: new structure on `GlobalState`, anchored at/in
  `l_registry`; mark it in `GlobalState::trace` (`lua-vm/src/trace_impls.rs`).
- Closure storage + dispatch: extend `GlobalState.c_functions` and the
  C-function call path (`state_stub.rs` / `api.rs` `push_c_*` and the precall-C
  path in `lua-vm`). Verify exact signatures before editing.
- Re-entrant access: the `LuaState` borrow model in `lua-vm` (`execute()` is
  driven with `&mut`); introduce the cell at the call boundary only.
- Public surface: `lua-rs-runtime` (evolve `LuaRuntime`/add `Lua`); keep
  `HostHooks` as the capability layer.

## Soundness invariants (the part to get right)

State these as tested properties:

1. **Rooting:** every value referenced by a live handle is in the root set and is
   marked; collection can never free a value a live handle points to.
2. **Drop discipline:** a handle's `Drop` unroots exactly once; no double-unroot,
   no leaked root slot; refcount (if used) hits zero exactly when the last clone
   drops.
3. **GC-during-callback:** a callback may allocate, which may trigger collection;
   anything the callback holds (args, created values, captured handles) must be
   rooted for the duration. No "transient unrooted value held across an alloc."
4. **Re-entrancy aliasing:** entering the VM re-entrantly must not create two
   live `&mut` paths into the heap. The cell is borrowed at the boundary and
   released before re-entry; the dispatch loop never holds it across a callback.
5. **Generational write barrier:** rooting an old-gen object that then points to
   a young-gen object requires the existing barrier; verify the root set plays
   with generational mode.

## Anti-requirements

- No `'lua` lifetimes on handles (rlua's mistake). Handles are owned/anchored.
- No cell/borrow in the per-instruction dispatch loop (perf regression).
- Public API stays safe; new `unsafe` is confined to the anchor/Drop/GC-interface
  core and budgeted in `harness/unsafe-budgets.toml`. (Note: this work will
  *grow* the unsafe surface — a conscious call against the full-safety goal.)
- Do not regress the official suite or the benchmark geomean.
- Mirror mlua's public names where they exist.

## Verification

- Official suite 44/44 after each phase (clean worktree, `LUA_RS_BIN` against the
  built binary).
- Benchmarks: geomean unchanged vs the pre-work baseline (proves boundary-only
  cost; the dashboard has the baseline).
- Miri on handle/root-set/create_function unit tests.
- A **GC-torture test**: force a full collection between every handle op and
  every callback step; rooted handles must survive, dropped ones must free.
- A stress/fuzz test: random create/clone/drop of handles interleaved with
  collection and re-entrant callbacks; assert no leak, no UB (under Miri).

## mlua-shape mapping (so the bms backend ports, not rewrites)

| lua-rs | mlua |
|---|---|
| `Lua` | `mlua::Lua` |
| `Value` | `mlua::Value` |
| `Table` / `Function` / `LuaString` / `AnyUserData` | same names |
| `lua.create_function(...)` | `Lua::create_function` |
| `lua.create_table()` / `create_userdata()` | same |
| `UserData` + `UserDataMethods` (`add_method`, `add_meta_method`) | same |
| `FromLua`/`IntoLua` (+ `*Multi`) | same |
| `lua_rs::Error` / `Result` | `mlua::Error` / `mlua::Result` |

Keeping these aligned is what turns bms's `reference.rs` and the `FromLua`/
`UserData` impls for `ScriptValue` into a near-mechanical translation.

## Tradeoffs (carry into implementation)

- Costs are at the Rust↔Lua boundary (handle root/unroot, marshalling, dynamic
  dispatch), not the interpreter hot loop — pure-Lua perf is unaffected if
  invariant #4 holds. Mitigate boundary churn with scoped/transient handles
  (mlua's `scope`) later if needed.
- The real risk is soundness (invariants 1–5), not throughput. It's one-shot,
  high-stakes; lean on Miri + the torture test.
- It grows the audited `unsafe` surface — a deliberate tension with "get to full
  safety." Make that call explicitly and document each block with `// SAFETY:`.

Additional explicit calls:

- **Owned handles over lifetime handles:** owned handles are more work because
  every live handle must be rooted and dropped correctly. They are still the
  right API for bms/mlua compatibility. Do not switch to rlua-style `'lua`
  handles to make the implementation easier.
- **Direct root slab over `luaL_ref`:** the direct slab is cheaper and stack
  independent, and `Drop` can unroot without going through Lua stack protocol.
  The tradeoff is that lua-rs owns the correctness story rather than inheriting
  C-Lua registry behavior. Tests must cover stale keys, clone/drop, GC survival,
  and slot reuse.
- **Re-entrancy over simplicity:** banning callbacks from re-entering Lua would
  simplify borrowing, but it would make the embedding API incomplete for real
  bms use. Support re-entry and keep the aliasing boundary explicit.
- **Safe public API over a tiny audited core:** the public surface must remain
  safe Rust. If a small unsafe bridge is required to connect `&Lua` callbacks to
  the current `&mut LuaState` VM, keep it local, documented, and covered by Miri
  tests. Do not spread unsafe through table/function/userdata code.
- **Boundary allocation over hot-loop regression:** handle operations may
  allocate or root/unroot. That is acceptable at embedding boundaries. A design
  that adds dynamic dispatch, `RefCell`, or root lookup to opcode dispatch is a
  rejection condition.

## Performance guardrails

Embedding work is allowed to add overhead only at Rust<->Lua boundaries.
Pure-Lua programs should not slow down meaningfully.

- Before starting a phase, record the current commit and current benchmark
  comparison artifact from `harness/bench/results`, or run:
  - `make macosx -C reference/lua-5.4.7`
  - `cargo build --release -p lua-cli`
  - `bash harness/bench/compare.sh`
- For a smoke check during development, run
  `bash harness/bench/compare.sh --runs 2 --workloads fibonacci,mandelbrot`.
- After each phase, compare geomean against the pre-phase baseline. Treat a
  repeatable regression greater than 2% as a blocker unless the phase explicitly
  changed a boundary-heavy benchmark.
- If a regression appears, profile before refactoring. Suspect accidental
  hot-loop borrow/cell work first, then excess `LuaValue` clones, then boundary
  conversion churn.
- Never commit generated `harness/impl/official/*.out` files as evidence.
  Use TSV/evidence artifacts and final command output summaries.
- Keep GC canaries in both incremental and generational modes in the verification
  loop. The root slab must stay compatible with generational collection.

## Rollback and commit discipline

- Commit each completed substrate phase separately so the next morning rollback
  is one `git revert <commit>` away.
- Do not include unrelated `.claude/worktrees/*` dirt or generated official
  `.out` files in these commits.
- Commit messages should name the invariant being added, for example
  `vm: trace external embedding roots` or `runtime: add rooted embedding handles`.
- Every commit that changes GC/rooting/re-entry should list the exact
  verification commands in the commit body or PR notes.

## Next agent goal

Objective:

> Build Phase 1 public owned handles on top of the landed `ExternalRootSet`,
> without changing the VM hot loop. Introduce an mlua-shaped `Lua` handle in
> `lua-rs-runtime` and implement rooted `Value`, `Table`, `Function`, and
> `LuaString` handles with correct clone/drop/root behavior.

Scope:

- Keep existing `LuaRuntime` source compatibility where practical, but add the
  new `Lua` API as the future embedding surface.
- Implement handle rooting through
  `LuaState::{external_root_value, external_rooted_value, external_unroot_value}`.
- `Drop` must unroot exactly once.
- `Clone` must either re-root or use a deliberately implemented refcount; choose
  the simpler unique-root model unless there is a concrete reason not to.
- Implement enough operations to prove handles are usable:
  - `Lua::new` / `Lua::with_hooks`
  - `Lua::load(...).exec()` or an equivalent compatibility wrapper
  - `Lua::create_table`
  - `Lua::create_string`
  - `Lua::globals`
  - `Table::get` / `Table::set` for primitive keys/values and `Value`
  - `Function::call` for `Value` or simple primitive arguments/returns
- Do not implement captured Rust closures or userdata in this goal unless they
  are strictly necessary for the handle substrate tests.

Completion criteria:

- Focused unit tests prove:
  - a Rust-held table survives full collection;
  - dropping the last handle frees its root slot;
  - cloned handles remain independently valid after the original drops;
  - stale keys cannot read or replace reused slots;
  - forced full collection between handle operations does not break live
    handles.
- Verification passes:
  - `cargo test -p lua-vm --lib external_root`
  - focused `lua-rs-runtime` handle tests
  - `cargo check -p lua-rs-runtime`
  - `cargo build -p lua-cli --bin lua-rs`
  - `./harness/canaries/gc/run_canaries.sh`
  - official `gc.lua` and `gengc.lua` into temporary output, both reaching `OK`
  - `git diff --check`
- Performance check recorded:
  - either run the benchmark compare command documented in `harness/bench`, or
    explicitly state why it was not run and record the pre/post commits needed
    for a follow-up compare.
- The final diff excludes generated official `.out` files and unrelated
  worktree dirt.
