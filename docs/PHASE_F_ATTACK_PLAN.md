# Phase F: Attacking the remaining 11 official-test failures

State as of 2026-05-19 post-fix-bundle (`f5710c4`): **33/44 PASS (75%)** on the upstream Lua 5.4 test suite. This doc lays out how to drive the remaining 11 to passing, ordered by leverage and dependencies.

Run-time scoring is via `./harness/run_official_all.sh` → `harness/impl/official/run_all.tsv`.

## Prerequisite: v5 Stop-hook test gate

Before launching anything else, install the Stop-hook test gate from `harness/prompts/manual/05-stop-hook-test-gate.md`. This morning's bundle work caught 3 regressions that landed because the build-only gate is too loose. Every subsequent Phase F slice is at risk of the same pattern until v5 lands.

**Order: v5 gate FIRST, then everything below.**

## The 11 failures, classified

| Tier | Test | Current failure | Effort | Phase |
|---|---|---|---|---|
| **Easy** | literals | `attempt to concatenate a table value` | sonnet, $2-3 | F-1 |
| **Easy** | nextvar | `bad 'for' limit (number expected, got number)` (wording bug) | sonnet, $2 | F-1 |
| **Easy** | errors | `errors.lua:38 assertion failed` (specific checkmessage) | sonnet, $3 | F-1 |
| **Medium** | locals | `locals.lua:861 assertion failed` (deep, post-require) | opus, $10 | F-2 |
| **Medium** | gc | `gc.lua:552 assertion failed` (deep) | opus, $10 | F-2 |
| **Medium** | coroutine | `coroutine.lua:165 assertion failed` | opus, $10 | F-2 |
| **Hard arch** | files | `attempt to yield across a C-call boundary` (needs continuations) | opus, $30 | F-3 |
| **Hard arch** | cstack | `testing stack overflow detection` (C-stack limit) | opus, $20 | F-3 |
| **Hard arch** | db | `db.lua:50 assertion failed` (debug library) | opus, $30 | F-3 |
| **Defer** | gengc | `attempt to index a nil value (upvalue 'x')` (gen GC) | Phase D-3 | F-4 |
| **Defer** | all | `cannot open main.lua` (harness setup) | harness work | F-4 |

Total estimated spend to clear F-1 through F-3: **$135 in agent budget** + ~3 days human attention for design oversight.

---

## F-1: Easy wins (kick off in parallel after v5)

Three sonnet runs in parallel worktrees. Disjoint files. All should drop in <1 hour each.

### F-1.a literals.lua: `attempt to concatenate a table value`

**Likely cause**: A test in literals.lua tries `s .. t` where `t` is a table that has a `__concat` metamethod. Our concat path doesn't honor `__concat` correctly, or doesn't honor it at all.

**Investigation entry points**:
- `crates/lua-vm/src/vm.rs` — search for `OP_CONCAT` / `concat` opcode dispatch
- `crates/lua-vm/src/object.rs` — `luaO_str2num` and concat helpers
- C-Lua reference: `lvm.c::luaV_concat` (line ~775)

**Reproducer**:
```bash
PREAMBLE='_soft=true; _port=true; _nomsg=true; arg=arg or {}; _G=_G or _ENV'
src="$PREAMBLE"$'\n'"$(cat reference/lua-c/testes/literals.lua)"
target/debug/lua-rs "$src" 2>&1 | head -50
```
Find the test phase that triggers the error; bisect to the specific `..` expression.

**Acceptance**: literals.lua passes the harness scan.

### F-1.b nextvar.lua: error wording mismatch

**Cause**: `bad 'for' limit (number expected, got number)` is the error message we emit when a numeric-for loop's limit can't be converted. The wording is wrong — C-Lua says `'for' initial value must be a number` or similar variants depending on phase. Likely `for_error` in `crates/lua-vm/src/debug.rs`.

**Investigation**:
- `crates/lua-vm/src/debug.rs::for_error`
- `crates/lua-vm/src/vm.rs` — OP_FORPREP / OP_FORLOOP dispatch
- C-Lua reference: `lvm.c::forprep` and `ldebug.c::luaG_forerror` (line ~720)

**Reproducer**: read nextvar.lua:611 and the surrounding test; instrument as needed.

**Acceptance**: nextvar.lua passes.

### F-1.c errors.lua: checkmessage at line 38

Now that the `_G` recursion is gone, errors.lua hits a real assertion at line 38. That's inside `checkmessage(prog, msg)` (the same harness we documented for the morning errors.lua dispatch). The prompt template at `harness/prompts/errors.lua.txt` instructs the agent to:

1. Pre-instrument errors.lua to print the (prog, msg, actual) tuple for the failing case
2. Fix the specific wording mismatch in the VM
3. Don't re-add the `_G` walk (we just removed it)

**Acceptance**: errors.lua advances past line 38. Repeat the slice for each successive checkmessage failure (likely 5-10 cycles, ~$3 each).

---

## F-2: Medium (Opus, post-F-1)

Three opus runs. Each is a real diagnosis-then-fix job. Can run in parallel worktrees but cherry-pick sequentially.

### F-2.a locals.lua: assertion at line 861

The morning fix moved this from "tracegc not found" to a real assertion deep in the test. Line 861 is in the `<close>` attribute tests — Lua 5.4's to-be-closed variables.

**Investigation**:
- Read locals.lua 850-870 to identify the assertion
- Likely involves to-be-closed cleanup interacting with goto / break / return paths
- `crates/lua-parse/src/lib.rs` — TBC parsing
- `crates/lua-vm/src/func.rs::close` — TBC close machinery
- C-Lua reference: `lparser.c::checktoclose`, `lfunc.c::luaF_close`

### F-2.b gc.lua: assertion at line 552

Now reaches deep in the test post-budget-fix. Line 552 — probably weak-table edge case or finalizer count mismatch.

**Investigation**:
- Read gc.lua 540-560
- Likely one of: weak-key ephemeron iteration, finalizer ordering, `__gc` on userdata vs tables, or count-after-collect mismatch
- `crates/lua-types/src/trace_impls.rs` — weak-table trace
- `crates/lua-vm/src/state.rs::collect_via_heap` — post-mark hook
- C-Lua reference: `lgc.c::clearbyvalues` / `clearbykeys` / `GCTM`

### F-2.c coroutine.lua: assertion at line 165

Phase E got us to line 141 (coroutine.close stub) → bundle fix moved past close → now line 165. Probably an xmove edge case or status-machine mismatch.

**Investigation**:
- Read coroutine.lua 155-175
- Check whether `xmove` is correctly handling N-value transfers
- C-Lua reference: `lapi.c::lua_xmove`

---

## F-3: Hard architectural (Opus, post-F-2)

Each is a real slice, not a fix. Spec these out with their own `harness/prompts/manual/0X-*.md` files first.

### F-3.a files.lua: continuation support

Spec'd in this morning's TODO comment at `crates/lua-vm/src/api.rs:1772`. C-Lua's `lua_callk(L, nargs, nresults, ctx, k)` registers `k` as a continuation on the CallInfo; on resume after yield, `finishCcall` calls `k(L, status, ctx)` to continue the C code. Our port has stubbed `k = None` everywhere.

The slice:
1. Add `LuaKFunction` type alias for `fn(&mut LuaState, i32 /*status*/, isize /*ctx*/) -> Result<usize, LuaError>`
2. Wire `k` through `api::call_k` → `state.call_with_k(func, nresults, ctx, k)` → registers on CallInfo
3. `finishCcall` (do_.rs:1109) invokes the registered `k` on resume
4. `dofile_fn`, `pcall_k`, and other stdlib functions that need yield-across pass real continuations instead of `None`
5. Verify with files.lua's "yielding during dofile" test

**Estimated cost**: $30. Largest slice in F-3.

### F-3.b cstack.lua: C-stack overflow detection

C-Lua tracks `nCcalls` and aborts the Lua-side call when it crosses `LUAI_MAXCCALLS`. Our port has `nCcalls` and the constant but the check may not be wired in all the right places.

Investigation:
- C-Lua reference: `ldebug.c::stackerror`, `ldo.c::luaD_pretailcall`
- Search our codebase for `LUAI_MAXCCALLS`
- The test specifically validates that `f() f() f()...` deep recursion produces a clean Lua error, not a crash

### F-3.c db.lua: debug library completeness

`debug.getinfo`, `debug.getlocal`, `debug.setlocal`, `debug.gethook`, `debug.sethook` — these are partially wired but several edge cases fail.

Read db.lua:50 to find the first failing assertion. Each subsequent assertion is its own ~$5 fix. Probably 5-10 cycles to clear the whole file.

---

## F-4: Deferred (Phase D-3 / harness work)

### gengc.lua — generational GC

Requires age bits, old cohorts, back barriers, touched lists. Spec'd at high level in `docs/LUA_PHASE_E_RUNTIME_SPEC.md` Part 2. Real engineering — 1-2 weeks human + agent. Not on the immediate roadmap.

### all.lua — harness composition

all.lua does `dofile("strings.lua"); dofile("locals.lua"); ...` etc. Needs:
- `dofile` resolving relative to the directory of the running script (our `prepend_lua_path` helps for require but not for `dofile`)
- Test isolation between sub-test runs

Mostly mechanical once dofile-relative is in. ~$10.

---

## Dispatch order (after v5 gate lands)

**Round 1 (F-1, parallel)**: 3 sonnet worktrees on literals + nextvar + errors. ~1 hour, ~$10. Targets 36/44.

**Round 2 (F-2, parallel)**: 3 opus worktrees on locals + gc + coroutine. ~3 hours, ~$30. Targets 39/44.

**Round 3 (F-3.a, single)**: 1 opus on files.lua continuations. ~2 hours, ~$30. Targets 40/44.

**Round 4 (F-3.b + F-3.c, parallel)**: cstack + db. ~2-4 hours, ~$50. Targets 42-43/44.

**Defer**: gengc (Phase D-3) + all (harness).

**Total estimated**: $120-150 + ~10 hours human attention to dispatch and cherry-pick. End state: **42-43/44 PASS (95-98%)**.

---

## How to actually run this

After v5 lands:

```bash
# F-1 round
./harness/dispatch.sh manual/F-1a-literals.md &
./harness/dispatch.sh manual/F-1b-nextvar.md &
./harness/dispatch.sh manual/F-1c-errors.md &
wait
./harness/cherry_pick_worktrees.sh   # auto-cherry-pick all finished worktrees
./harness/run_official_all.sh        # measure

# F-2 round
# ...
```

(`dispatch.sh` and `cherry_pick_worktrees.sh` are TBD — wrapping the worktree-Agent + cherry-pick dance we've been doing manually. Worth building as part of the v5 work.)

## What "done" looks like

When the official-test pass count is 42+/44 AND the v5 gate is preventing per-commit regressions, the autonomous loop is effectively in production-quality territory. At that point the "Lua 5.4 in safe Rust runs LuaRocks" demo from PORT_STRATEGY.md §8 becomes the next milestone.
