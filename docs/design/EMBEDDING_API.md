# Embedding API design

Internal design note. What a good Rust embedding API for lua-rs looks like,
synthesized from two angles: designing the API directly, and building the first
real consumer (a Bevy Mod Scripting / bms backend, toward a nano9 "PICO-8 in the
browser" demo). The bms build independently re-derived the same two crux
decisions, which is the main reason to trust this design.

Implementation status: the core API described here landed in `lua-rs-runtime`
through commit `a096e24`. This file remains the rationale and product-shape
note. Current implementation details, verification evidence, known limits, and
future opportunities live in
[docs/EMBEDDING_API_IMPLEMENTATION.md](../EMBEDDING_API_IMPLEMENTATION.md).

## What the API bridges

Two opposite memory models: Lua (GC'd, dynamically typed, single-threaded heap)
and Rust (ownership/borrowing, static types, RAII). A good embedding API hides
that mismatch behind something that feels native to Rust, and solves five
things: run Lua from Rust with typed results; call Rust from Lua; convert values
both ways; let Rust hold Lua values across calls without the GC eating them;
errors (and, our angle, sandboxing) both directions.

mlua is the reference for what "good" feels like. rlua was the painful first
draft. The lesson from their evolution drives the two crux decisions.

## The two crux decisions (validated from both ends)

These were derived from the API side and then confirmed as the literal next
blockers while building the bms backend.

**1. Owned, GC-anchored handles — not `'lua` lifetime-bound ones.**
A `Table`/`Function` Rust holds must root its referent so the GC can't collect
it; it should be freely cloneable/storable and unroot on `Drop`. rlua used
`'lua` lifetimes (lifetime soup, can't store Lua values in your structs); mlua
abandoned that for owned anchored handles. lua-rs should do the same.
- bms evidence: the reflection bridge is userdata holding a `ReflectReference`
  with Rust-closure metamethods, and `register_callback` literally stores a Lua
  function Rust-side and calls it later. Both require Rust to hold Lua values
  across calls. So this is a prerequisite for the demo, not just ergonomics.

**2. A shared, re-entrant `Lua` handle — not `&mut self`.**
When Lua calls a registered Rust function, that function needs to call back into
Lua, but the VM already holds the state. The public `Lua` must be a cheap-clone
shared handle with interior mutability, so callbacks get a context they can
re-enter. The current `LuaRuntime::exec(&mut self)` can't support this.
- bms evidence: this is the wall the backend is standing at. bms host functions
  are `Arc<dyn Fn(FunctionCallContext, VecDeque<ScriptValue>) -> ScriptValue>` —
  stateful, and they re-enter the world and Lua. The Gate-2 function-registry
  bridge is blocked on exactly `create_function(closure)`, and the current
  `context.rt.state_mut()` (`&mut self`) aliases the instant a registered
  closure re-enters the VM during a `pcall`. "Shared handle, not `&mut self`" is
  the gate, confirmed from the consumer side.

## The shared prerequisite: a GC external root set

Both crux decisions reduce to one implementation task: give the GC an external
root set that embedder handles live in (clone → root, drop → unroot), so Rust
can hold Lua values safely. lua-rs's GC already only collects at safepoints where
roots are reachable, so this is the natural extension.

This one task has three payoffs: the embedding API, the bms function/reflection
bridges, and the eventual redis mlua-drop. Do it first, do it well — everything
else sits on top of it.

## Consumers and priorities (this changes the build order)

There are two consumer profiles, and they want different layers first:

- **Direct embedder (app dev):** wants the *sugar* — `FromLua`/`IntoLua` blanket
  impls (`i64`, `Vec`, `Option`, tuples) and `#[derive]` for structs ↔ tables.
  This is the ergonomic v1.
- **Framework backend (bms):** wants the *substrate* — owned `Table`/`Function`
  handles, `create_function` with captured state, a `Value` type, function call,
  and userdata-with-metamethods. bms marshals everything through its own
  `ScriptValue` enum, so it does **not** need the generic Rust-type sugar at all.

Implication: **the layer you build first should be chosen by the consumer you're
chasing.** For the direct-embedder / untrusted-script-sandbox market, lead with
the conversion sugar. For the demonstrated virality path (bms → nano9 → "PICO-8
in the browser"), lead with the substrate; the `FromLua`/`IntoLua` sugar is
largely irrelevant to it and can trail.

Given the virality path runs through bms, the recommended order is:

**root set → owned handles → `create_function` (with state) → userdata + metamethods → (sugar trails).**

## mlua-shape is a translation lever, not just migration marketing

The sharpest finding from the bms build: **mirror mlua's shape exactly at the
handle / `create_function` / userdata layers.** bms's existing Lua backend
(`bevy_mod_scripting_lua`) is written against mlua's API — it impls mlua's
`FromLua`/`IntoLua` for its `ScriptValue` wrapper and `UserData` +
`add_meta_function` for the reflection reference. The closer lua-rs's API is to
mlua's at those layers, the more the bms backend becomes a near-mechanical
*translation* of bms's existing mlua backend instead of a from-scratch
implementation. Concretely, it collapses the hardest remaining work — the
reflection bridge in `reference.rs` (~460 LoC) — from "design + implement" to
"port." A v0 written against lua-rs's raw stack API was clunky; an mlua-shaped
API would let the bridge be written the way bms already wrote it. For this case,
"be mlua-shaped" is the difference between tractable and not.

## Userdata-with-metamethods is required, not "v2"

A pure-design pass would park `UserData` in v2. For the bms/nano9 path it moves
up: the reflection bridge (what makes `entity.component.field` work, what nano9
needs) is userdata + `__index`/`__newindex`/arithmetic metamethods. So for the
demo, userdata-with-metamethods is a v1 requirement.

## `!Send`: document the impedance, don't fight it

lua-rs's shared handle is `!Send` (correct, and matches mlua's default — the Lua
heap is single-threaded). bms requires `Context: Send`, which the backend
bridged with a caveated `unsafe impl Send` that is sound under single-threaded
execution (e.g. wasm). The design should state this plainly: the answer is
single-threaded execution plus a documented `unsafe` at the framework boundary,
**not** making the VM thread-safe.

## Proposed API shape

Build up from the existing `LuaRuntime`/`HostHooks` (host-hook capability
injection is a good foundation for the sandbox layer). Layers, in the
bms-first build order:

```rust
// Layer 1 — substrate (root set + owned handles + Value)
let lua = Lua::new();
let t: Table = lua.create_table()?;
t.set("x", 1)?;                                  // owned, GC-anchored handle
let f: Function = lua.load("return function(n) return n*2 end").eval()?;
let r: Value = f.call(Value::Integer(21))?;      // call Lua from Rust

// Layer 2 — create_function with captured state (the gate)
let counter = Arc::new(AtomicI64::new(0));
let f = lua.create_function({
    let counter = counter.clone();
    move |lua, args: Variadic<Value>| {           // captures state, re-enters via `lua`
        Ok(Value::Integer(counter.fetch_add(1, SeqCst)))
    }
})?;
lua.globals().set("tick", f)?;

// Layer 3 — userdata + metamethods (required for the bms reflection bridge)
impl UserData for ReflectRef {
    fn add_meta_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_meta_method("__index",    |lua, this, key| ...);
        m.add_meta_method("__newindex", |lua, this, (key, val)| ...);
    }
}

// Layer 4 — sugar (FromLua/IntoLua), trails for the bms path
let sum: i64 = lua.load("return 2 + 3").eval()?;
lua.globals().set("greeting", "hello")?;
```

Two trait families for the sugar layer (copy mlua's design; well-proven):
`FromLua`/`IntoLua` and `FromLuaMulti`/`IntoLuaMulti` for arg/return lists.

## Implementation starting points (from the v0 bms backend, 2026-05-25)

The v0 `bevy_mod_scripting_lua_rs` backend compiles and runs scripts via the
*low-level* surface (`LuaRuntime` + the raw `api::` stack functions). That surface
is the escape hatch the product API should sit above — concrete notes for the layers:

- **`create_function` (Layer 2) already has a basis to generalize.** `lua-hlua-shim`
  implements captured-state callbacks via `func::registry_insert` + a `trampoline`
  cclosure that recovers the Rust closure from a registry by an index pushed as an
  upvalue. `create_function(closure)` is the generalization of that pattern — build
  it there, not from scratch. (v0 dodged it by registering a bare `fn log` via
  `api::push_cclosure(state, f, 0)` — fine for a no-state probe, useless for the
  function-registry bridge.)
- **The `&mut self` problem is concrete, not hypothetical.** v0's handler does
  `context.rt.state_mut()`, reads returns via `state.pop() -> LuaValue`, pushes args
  via `api::push_*`. It works, but it is exactly the surface the shared re-entrant
  `Lua` handle (crux #2) must replace before a stateful callback can re-enter the VM
  mid-`pcall`.
- **Marshalling shape:** bms converts through its own `ScriptValue`; the backend's
  `script_value.rs` is the analog of mlua's `FromLua`/`IntoLua` impls for the
  `ScriptValue` wrapper. Shipping those *traits* (even without the blanket Rust-type
  impls) lets that conversion port straight from bms's mlua backend.
- **`InteropError`-style gotcha worth mirroring cleanly:** dynamic error messages
  need an owned path. (In bms, `InteropError::str` wants `&'static str`; dynamic
  messages route through `InteropError::external`.) A lua-rs error type should make
  `impl std::error::Error` cheap so it drops into any host's error model.

## Where to beat mlua (still true, lower priority for bms)

Mirror mlua's shape for familiarity + translation leverage, but lean into what
pure-Rust enables: sandboxing as a safe-by-default, first-class feature
(`Lua::sandboxed()`, `memory_limit`, instruction/fuel budget, stdlib selection —
see SANDBOXING.md), cleaner `Result`-based errors with no `longjmp`, and a safe
public API. For the direct-embedder/untrusted-script market this is the wedge;
for the bms/nano9 path it's secondary to the substrate.

## Bottom line

Build order for the bms/nano9 demo: **GC external root set → owned handles →
`create_function` with state → userdata + metamethods**, mlua-shaped at every
one of those layers so the bms backend is a port, not a rewrite. The conversion
sugar and the sandbox preset are real and worth doing, but they serve the
direct-embedder market and can follow.
