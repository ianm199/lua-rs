# Sandboxing (started 2026-05-29)

Branch: `explore/sandboxing` (worktree `lua-rs-sandbox`).

## Why

Sandboxing — running untrusted Lua with bounded CPU, bounded memory, and no
ambient host authority — is one of the main capabilities [piccolo] has that we
did not. This pass found the seams, built the feature, and hardened it into a
**sound boundary against adversarial code *and* adversarial input**: every
escape an attacker would reach for (coroutines, `pcall`/`xpcall`/`resume` loops,
catastrophic pattern matches, single huge allocations, deep recursion) is closed
and regression-locked, with the full official Lua suite still 44/44 and zero
overhead when no sandbox is active. What remains (limitations 4–5 below) is
piccolo's *cooperative scheduling* model and an allowlist environment — separate
bets, not safety gaps.

[piccolo]: https://github.com/kyren/piccolo

## TL;DR — what is now possible

A working `Lua::sandboxed(SandboxConfig)` constructor that gives an embedder
three independent controls, each proven by a passing test
(`crates/lua-rs-runtime/tests/sandbox.rs`, 23/23 green; full official Lua suite
44/44):

| Control | Mechanism | Test |
|---|---|---|
| **Instruction budget** — abort after N VM instructions | shared `GlobalState` budget charged in `trace_exec` on every thread; trips → `LuaError` unwinds the dispatch loop | `infinite_loop_is_aborted`, `runaway_recursion_is_aborted`, `coroutine_is_metered` |
| **Memory ceiling** — abort once GC bytes exceed a limit | same per-interval charge samples `GlobalState::total_bytes()` | `memory_bomb_is_aborted` |
| **Capability stripping** — remove dangerous globals | nil out `os.execute`, `io`, `load`, `require`, `package`, `debug`, … from `_G` after stdlib init | `strict_preset_strips_capabilities` |

`while true do end` and a 1 GB-table memory bomb both abort cleanly instead of
hanging/OOMing the host; pure libraries (`string`, `math`, `table`, `os.time`)
remain. A plain `Lua::new()` is completely unaffected (no hook, no stripping).

The budget spans **every thread** and is **uncatchable**. Two escapes that an
adversary would reach for are both closed:
- *Coroutines* — code inside `coroutine.wrap(...)` is metered (budget lives in
  `GlobalState`, every coroutine is armed).
- *`pcall`/`xpcall`/`coroutine.resume`* — once the budget trips, the abort is
  sticky and re-raised by every protected-call builtin, so
  `while true do pcall(function() while true do end end) end` aborts instead of
  running forever. Ordinary errors are still caught normally.

Remaining gaps are narrower and documented below: the memory ceiling is sampled
rather than per-allocation, and a single long-running stdlib C call can't be
preempted mid-call.

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

### Where the budget lives: shared `GlobalState`, enforced natively

The budget is **not** a hook closure on one thread — that design could not span
coroutines (see history below). Instead it lives in `GlobalState`
(`SandboxLimits`, a set of `Cell`s shared by every thread through the
`Rc<RefCell<GlobalState>>` they all hold), and is enforced *inside the VM*:

- `LuaState::install_sandbox_limits(interval, instr, mem)` stores the budget in
  `GlobalState` and arms the `LUA_MASKCOUNT` mask on the current thread.
- `preinit_thread` arms the same mask on every **new coroutine**, so metering
  spans all threads — this is what closes the coroutine escape.
- `trace_exec` calls `LuaState::sandbox_charge_interval()` once per `interval`
  instructions on each thread: it decrements the shared budget and samples
  `total_bytes()`, returning a `LuaError` directly (it's already a `Result` fn)
  when a limit is crossed. That `Err` unwinds the dispatch loop.

**Uncatchability.** A budget trip raises an ordinary `LuaError`, which `pcall`
would normally catch — letting untrusted code loop on it forever. To prevent
that, the trip sets a sticky `aborting` flag in `SandboxLimits`. While it is
set, the protected-call builtins re-raise instead of catching:
`pcall_fn`/`xpcall_fn` (`base.rs`) and `co_resume` (`coro_lib.rs`) check
`state.sandbox_aborting()` and propagate the error rather than returning
`false, msg`. `coroutine.wrap` already re-raises, so it inherits this. The flag
clears only on `Sandbox::reset()`. Ordinary (non-sandbox) errors are unaffected
— the re-raise is gated on an in-flight abort, so normal `pcall` semantics are
preserved (proven by `pcall_still_catches_ordinary_errors_under_sandbox` and the
44/44 official suite).

Because enforcement is native in `trace_exec` (not a user-hook closure), there
is **no `pending_hook_error` indirection** and the user-hook slot stays free for
`debug.sethook`. Changes to `lua-vm`:

- `lua-vm/src/state.rs` — `SandboxLimits` + `GlobalState.sandbox` field;
  `install_sandbox_limits`, `sandbox_charge_interval`, accessors; coroutine
  arming in `preinit_thread`.
- `lua-vm/src/debug.rs` — `trace_exec` charges the budget in the count path;
  `arm_traps` wrapper.

Changes to `lua-stdlib` (uncatchability):

- `base.rs` — `pcall_fn`/`xpcall_fn` re-raise while `sandbox_aborting()`.
- `coro_lib.rs` — `co_resume` re-raises while `sandbox_aborting()`.

Everything else lives in `lua-rs-runtime/src/lib.rs` (`Sandbox`,
`SandboxConfig`, `TripReason`, `Lua::sandboxed`, `install_sandbox`).

### Non-regression

`trace_exec` is shared with Lua's own `debug.sethook`; the sandbox charge only
runs when the sandbox is active (`interval != 0`), so normal hook usage is
unaffected, and the dispatch loop itself is untouched. Confirmed:

- **Full official Lua 5.4 suite: 44/44 PASS** (the gate; covers
  `pcall`/`xpcall`/`coroutine`/`errors` semantics the uncatchability change
  touches).
- `lua-vm` lib tests, full `lua-rs-runtime` build — clean.
- Sandbox tests **23/23**, including `coroutine_is_metered`,
  `pcall_loop_cannot_escape`, `xpcall_cannot_swallow_trip`,
  `resume_loop_cannot_escape`, and
  `pcall_still_catches_ordinary_errors_under_sandbox`.

### Cost (measured, release, worst-case tight integer loop)

| Config | Time | vs off |
|---|---|---|
| sandbox **off** (`Lua::new`) | 0.150s | 1.00× |
| sandbox **on** (instr only) | 0.265s | 1.77× |
| sandbox **on** (instr + memory) | 0.263s | 1.75× |

Off path is byte-for-byte identical to today (the `trap` flag stays false) →
**zero overhead for non-sandboxed embedders**. Inside a sandbox the cost is the
standard count-hook per-instruction trap dispatch; the memory check rides the
same per-interval charge for free. `check_interval` trades enforcement
precision, not throughput.

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

0. **✅ Coroutine escape — RESOLVED.** An earlier prototype enforced the budget
   with a count-hook *closure* on a single `LuaState`; coroutines are separate
   `LuaState`s, so code inside `coroutine.wrap(function() while true do end end)()`
   ran completely unmetered and hung forever. Fixed by moving the budget into
   `GlobalState` (shared across all threads) and arming the count mask on every
   coroutine in `preinit_thread`. Now metered on every thread — proven by
   `coroutine_is_metered` (aborts in <5s) and `yielding_coroutine_within_budget_completes`.
   The closure design couldn't do this because `Box<dyn FnMut>` can't be cloned
   into each thread; native `GlobalState` enforcement has no such constraint.

0b. **✅ `pcall`/`xpcall`/`resume` escape — RESOLVED.** The trip raised an
   ordinary `LuaError`, so `pcall` caught it and `while true do pcall(runaway)
   end` ran forever (total instructions unbounded). Fixed by a sticky `aborting`
   flag: once tripped, the protected-call builtins re-raise instead of catching,
   so the abort propagates all the way to the embedder and cannot be looped on.
   Ordinary errors still caught normally. Proven by `pcall_loop_cannot_escape`,
   `xpcall_cannot_swallow_trip`, `resume_loop_cannot_escape`, and
   `pcall_still_catches_ordinary_errors_under_sandbox`. A looping `__close`
   handler during the abort unwind is also bounded (the count hook meters the
   handler body; verified to abort in ~2ms).

1. **✅ Memory overshoot — RESOLVED (bounded).** Single size-known-upfront
   allocations (e.g. `string.rep`, whose own guard is 2 GiB — far above a typical
   cap) are refused *before* the buffer is built by `LuaState::sandbox_reserve`,
   which projects `total_bytes() + size` against the ceiling and arms the
   uncatchable memory abort. Incremental growth is caught by the per-interval
   charge, so overshoot is bounded to one `check_interval` of small allocations.
   Proven by `huge_string_rep_aborts_at_cap` and `memory_cap_is_uncatchable`.
   (A fully synchronous per-allocation fallible allocator awaits the Phase-D GC
   allocator-accounting migration; the reserve + per-interval combination makes
   it unnecessary for the realistic vectors.)

2. **✅ Single long-running stdlib C call — RESOLVED.** The pattern matcher
   (`string.find/match/gmatch/gsub`) counts `match_pat` invocations bounded by a
   `step_limit` set to the remaining instruction budget; on overrun it unwinds
   and the caller charges the work via `sandbox_charge`, exhausting the budget
   and arming the uncatchable abort. A catastrophic-backtracking pattern now
   aborts instead of spinning. `table.sort` needs no change — comparator calls
   are Lua frames already metered. Proven by `catastrophic_pattern_is_bounded`,
   `catastrophic_gsub_is_uncatchable`, `adversarial_sort_is_bounded`, and
   `ordinary_pattern_matching_still_works`.

3. **✅ Host-stack-overflow crash — bounded (structural).** A recursive Rust
   interpreter consumes host stack per nested Lua call. The `LUAI_MAXCCALLS`
   call-depth guard (single source of truth in `state.rs`, documented margin
   ~`stack_bytes/40k`) converts what would be a SIGSEGV into a catchable
   `"stack overflow"` error. Verified: deep non-tail recursion, infinite
   `__index`/`__concat`/`__tostring` chains, and nested-coroutine `__close`
   cascades all error cleanly (`recursion_*` tests, plus official `cstack.lua`).

4. **Abort, not pause/resume.** piccolo's fuel is *cooperative*: out-of-fuel
   suspends and resumes later, enabling preemptive scheduling of many scripts on
   one thread. Our interpreter is a recursive Rust function (`vm::execute` calls
   itself for Lua→Lua calls), so we can *abort* at a hook point but not *yield
   the Rust stack* mid-instruction. True pausable fuel would need the stackless
   re-entrant VM redesign that is piccolo's whole architecture — out of scope
   here, a separate product bet (a Lua async runtime), not required for the
   "safe embedded Lua 5.4" goal.

5. **Capability stripping is a blocklist, not an allowlist.** `strict()` removes
   a known-dangerous set. A higher-assurance design builds the environment from
   an empty table and adds only vetted functions. The host-hook layer is the
   real backstop (omitted hooks make `io`/`os`/dynlib calls error even if a
   global slips through), so this is defense-in-depth, not the sole gate.

## CLI

The budget is exposed end-to-end on the `lua-rs` binary:

```
lua-rs --sandbox script.lua              # strict preset: strip host globals + caps
lua-rs --max-instructions=5000000 s.lua  # CPU budget
lua-rs --max-memory=64M s.lua            # memory ceiling (K/M/G suffixes)
lua-rs --sandbox --max-memory=16M s.lua  # strict preset, tighter memory cap
```

`--sandbox` strips the code-loading and host-access globals
(`lua_stdlib::sandbox::STRICT_REMOVED_GLOBALS` — the single source of truth that
`SandboxConfig::strict()` also uses) and applies 10M-instruction / 64 MiB
defaults; the explicit flags override or set limits independently.

## Suggested next steps (beyond the safe-sandbox goal)

- Allowlist-based environment builder for `SandboxConfig` (limitation 5).
- Fully synchronous per-allocation memory failure once the Phase-D GC
  allocator-accounting migration lands (limitation 1 is already bounded).
- Strategy note on whether pausable/cooperative fuel (limitation 4) justifies a
  move toward a stackless dispatch core — a separate product bet (async runtime),
  not required here.
