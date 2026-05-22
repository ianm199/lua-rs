# Stuck: `reference/lua-c/testes/errors.lua`

Stuck-skipped after Sonnet then Opus both ran. Most recent transcript: `harness/impl/mega-O2-D5-reference_lua_c_testes_errors_.transcript.jsonl` (~1.8 MB).

## Current failure

```
testing errors
 >>> testC not active: skipping tests for messages in C <<<
+
lua: pcall_k failed: Runtime: reference/lua-c/testes/errors.lua:30: assertion failed!
```

Line 30 is inside `checkmessage`:
```lua
local function checkmessage (prog, msg, debug)
  local m = doit(prog)
  if debug then print(m, msg) end
  assert(string.find(m, msg, 1, true))   -- ← fires here
end
```

So **some error-message text we produce doesn't contain the substring the test expects**. To diagnose which `checkmessage(...)` call is failing, you need to instrument: add `print(prog, m, msg)` above the assert and re-run, or read `errors.lua` from line 44 down for the first `checkmessage` invocation (they cascade).

## What's actually wrong

This is not one bug — it's *every* error-message wording difference between our VM and reference Lua. Each call to `checkmessage(prog, expected_substring)` runs a tiny program that should error, captures the message, and asserts the message contains a specific phrase. We've fixed a handful (n%0 modulo, OP_GetUpval typo, obj_type_name routing) but there are dozens.

The failing assertion's prog/msg pair isn't known from the test output alone — we'd need either to instrument the test or check past transcripts.

## What past agents have tried

Five commits over the project:
- `f53fb1d` — early errors.lua work
- `f289e4e` Phase D-0 prep (collectgarbage wiring + error rails)
- `9a1c490` parser: defer `dyd.actvar` truncate (cascading effect on debug.getinfo name resolution)
- `949cffa` vm: `'n%%0'` → `'n%0'` in modulo-zero error
- `71b48a8` errors.lua debug (vm.rs, debug.rs — error wording)
- `67f00a4` errors.lua debug — bigger one

The O2 opus run (most recent) fixed a non-trivial parser bug. Quoting its summary:

> **Root cause:** `crates/lua-parse/src/lib.rs:2587` (`cg_self`) — the OP_SELF codegen always emitted the method-name constant index as the C operand with `k=1`, even when the constant index exceeded the 8-bit `MAXINDEXRK` limit (255). The index got truncated to its low 8 bits, so `t:bbb()` after 1000+ constants reported the wrong method name (e.g. `'x233'` instead of `'bbb'`).
>
> **Fix:** When `k_idx > MAXINDEXRK`, demote the key to `ExprKind::K` and discharge it into a register via `cg_exp_to_any_reg`, then emit OP_SELF with `C=reg, k=0`. Mirrors `codeABRK`/`exp2RK` from `lcode.c`.

That was a real find and committed. But it advanced past **one** `checkmessage` call; another one earlier in the test now fails.

The fact that the test fails at line 30 — inside the *first* checkmessage helper — suggests an early assertion is failing. The agent claimed it advanced past line 345; if true, the current failure-at-line-30 may be a fresh regression introduced by the cherry-pick of `42ff10f` (the new file_open_hook plumbing changed how some errors propagate). Worth checking.

## Suggested next move

Instrument the test BEFORE dispatching another agent:

```bash
sed -i.bak '30i\
  if not string.find(m, msg, 1, true) then print("FAIL prog=", prog); print("got msg=", m); print("want=", msg) end' reference/lua-c/testes/errors.lua
target/debug/lua-rs reference/lua-c/testes/errors.lua 2>&1 | head -40
```

The next agent should be told **which specific `checkmessage` call** is failing — without that, opus burns budget rediscovering it. Past attempts ran the test, hit a generic "assertion failed" line, and went hunting through the entire VM error-message surface.

Once you have the (prog, expected, actual) tuple in hand, this is a 5-minute fix per mismatch. Likely 3-10 mismatches still hidden.

## Files most touched

`crates/lua-vm/src/debug.rs` (error message formatting), `crates/lua-vm/src/vm.rs` (error context), `crates/lua-vm/src/state.rs` (`obj_type_name`), `crates/lua-parse/src/lib.rs` (syntax errors, codegen edges).
