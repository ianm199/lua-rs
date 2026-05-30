# Sandboxing — exploratory prototype (2026-05-29)

Branch: `explore/sandboxing` (worktree `lua-rs-sandbox`).

## Why

Sandboxing — running untrusted Lua with bounded CPU, bounded memory, and no
ambient host authority — is one of the main capabilities [piccolo] has that we
did not. This is an exploratory pass to find the seams and prove what's
achievable on the current architecture.

[piccolo]: https://github.com/kyren/piccolo

## TL;DR — what is now possible

A working `Lua::sandboxed(SandboxConfig)` constructor that gives an embedder
three independent controls, each proven by a passing test
(`crates/lua-rs-runtime/tests/sandbox.rs`, 7/7 green):

| Control | Mechanism | Test |
|---|---|---|
| **Instruction budget** — abort after N VM instructions | VM count-hook decrements a shared budget; trips → staged `LuaError` unwinds the dispatch loop | `infinite_loop_is_aborted`, `runaway_recursion_is_aborted` |
| **Memory ceiling** — abort once GC bytes exceed a limit | same hook samples `GlobalState::total_bytes()` every interval | `memory_bomb_is_aborted` |
| **Capability stripping** — remove dangerous globals | nil out `os.execute`, `io`, `load`, `require`, `package`, `debug`, … from `_G` after stdlib init | `strict_preset_strips_capabilities` |

`while true do end` and a 1 GB-table memory bomb both abort cleanly instead of
hanging/OOMing the host; pure libraries (`string`, `math`, `table`, `os.time`)
remain. A plain `Lua::new()` is completely unaffected (no hook, no stripping).

**⚠️ Not yet a sound security boundary.** The budget does not follow coroutines
(limitation 0 below) — code inside `coroutine.wrap(...)` is unmetered and can
hang. Treat this as a working proof-of-concept, not a shippable sandbox, until
the budget moves into `GlobalState` with per-thread enforcement.

## How it works

The key discovery: **most of the infrastructure already existed**, it was just
never surfaced as a sandbox API.

- The VM dispatch loop already has a per-instruction `trap` check gated on
  `hook_mask() != 0` (`lua-vm/src/vm.rs:1437`). When no hook is set the cost is
  zero — so an instruction budget built on the count-hook adds **no overhead to
  the non-sandboxed hot path**.
- The count-hook (`LUA_MASKCOUNT`) already fires every `basehookcount`
  instructions and is wired through `trace_exec` → `call_hook_event` →
  `do_::hook`, all of which propagate `Result` via `?`.
- The GC heap already tracks total bytes (`GlobalState::total_bytes()`).
- Host capabilities were *already* gated behind optional `HostHooks`
  (file/process/dynlib); a sandbox simply omits them.

### The one real gap that had to be filled

A Lua C hook aborts execution by calling `luaL_error` (a longjmp). Our hook
closure is `FnMut(&mut LuaState, &LuaDebug) -> ()` and **cannot return an
error**. So the closure had nowhere to signal "stop."

Fix (minimal, additive): a `pending_hook_error: Option<LuaError>` slot on
`LuaState`. The closure stages an error there; `do_::hook` drains it after the
closure returns and converts it into the `Err` that unwinds the dispatch loop.
This is the only change to `lua-vm`:

- `lua-vm/src/state.rs` — new field + `set_pending_hook_error()`.
- `lua-vm/src/do_.rs` — `do_::hook` checks the slot and returns `Err` if set.

Everything else lives in `lua-rs-runtime/src/lib.rs` (the `Sandbox`,
`SandboxConfig`, `TripReason`, `Lua::sandboxed`, `install_sandbox`).

### Non-regression

`do_::hook` is shared with Lua's own `debug.sethook`. The slot is `None` in all
normal hook usage, so the added check is a no-op. Confirmed:

- `db.lua` (the official debug-hook test — count + line hooks) — **PASS**
- `errors.lua`, `coroutine.lua` — **PASS**
- `lua-vm` lib tests, full `lua-rs-runtime` build — clean.

## Usage

```rust
use lua_rs_runtime::{Lua, SandboxConfig, TripReason};

let (lua, sandbox) = Lua::sandboxed(SandboxConfig::strict())?;
match lua.load(untrusted_source).exec() {
    Ok(()) => { /* finished within limits */ }
    Err(_) => match sandbox.tripped() {
        Some(TripReason::Instructions) => { /* CPU budget hit */ }
        Some(TripReason::Memory)       => { /* memory ceiling hit */ }
        None                           => { /* ordinary Lua error */ }
    },
}
sandbox.reset(); // refill budget before re-running in the same state
```

`SandboxConfig::strict()` = 10M instructions, 64 MiB, dangerous globals removed.
Every field is tunable; `install_sandbox` lets you grant *some* `HostHooks` and
still bound execution.

## Honest limitations (and the path past them)

0. **⚠️ Coroutines escape the CPU/memory budget (verified).** The budget is
   enforced by a count-hook closure on *one* `LuaState`. Coroutines are separate
   `LuaState`s created with no hook, so code running inside a coroutine is
   completely unmetered — `coroutine.wrap(function() while true do end end)()`
   **hangs forever** under an instruction limit, and `strict()` does *not* strip
   `coroutine`. Root cause: the hook is a `Box<dyn FnMut>` that cannot be cloned
   into each new thread, so the closure design fundamentally can't span threads.
   - **Stopgap (verified to close the hole):** add `b"coroutine"` to
     `remove_globals`. Then coroutine creation is a plain nil-index error.
   - **Proper fix:** move the budget into `GlobalState` (already shared across
     all threads via `Rc<RefCell<…>>`) and check it in the dispatch-loop trap
     path for *every* thread, instead of using the per-thread user-hook slot.
     That is the design a production sandbox wants; it also frees the user-hook
     slot for `debug.sethook`.

   This is the reason the prototype is **not yet a sound security boundary** and
   should not be merged to `main` as "sandboxing" without the proper fix or, at
   minimum, the stopgap baked into `strict()` plus clear "best-effort" framing.

1. **Enforcement is granular, not exact.** Limits are checked every
   `check_interval` instructions (default 1000). A budget trips within
   `check_interval` of the true limit, and memory is sampled at that cadence —
   so a single huge allocation *between* two samples (e.g. `string.rep("x",
   1e9)`) can momentarily blow past the ceiling before the next check catches
   it. For a **hard, per-allocation** memory cap, enforce in
   `lua-gc/src/heap.rs::allocate` (the central byte-accounting point at
   `heap.rs:558`) — but `allocate` is currently infallible and called from
   hundreds of sites, so making it fail-able is a real project, not a patch.

2. **Abort, not pause/resume.** piccolo's fuel is *cooperative*: out-of-fuel
   suspends and resumes later, enabling preemptive scheduling of many scripts on
   one thread. Our interpreter is a recursive Rust function (`vm::execute` calls
   itself for Lua→Lua calls), so we can *abort* at a hook point but not *yield
   the Rust stack* mid-instruction. True pausable fuel would need the stackless
   re-entrant VM redesign that is piccolo's whole architecture — out of scope
   here, worth a separate strategy note.

3. **Capability stripping is a blocklist, not an allowlist.** `strict()` removes
   a known-dangerous set. A higher-assurance design builds the environment from
   an empty table and adds only vetted functions. The host-hook layer is the
   real backstop (omitted hooks make `io`/`os`/dynlib calls error even if a
   global slips through), so this is defense-in-depth, not the sole gate.

## Suggested next steps

- Hard memory cap via a fallible `Heap::allocate` (biggest correctness win).
- Allowlist-based environment builder for `SandboxConfig`.
- Wire `instruction_limit` / `memory_limit` flags into `lua-cli` for an
  end-to-end demo (`lua-rs --max-instructions=… script.lua`).
- Strategy note on whether pausable/cooperative fuel justifies any move toward a
  stackless dispatch core.
