# Phase F Attack Plan

State as of 2026-05-19 late, after H/G-2/G-3/audit/table-refactor/R-alpha/small-fixes: **33/44 PASS** on the upstream Lua 5.4 official test suite.

Run-time scoring remains:

```bash
./harness/run_official_all.sh
cat harness/impl/official/run_all.tsv
```

This document is the current dispatch plan. Older pre-table-refactor notes are intentionally not retained inline because they now point agents at retired causes such as `FLAT_TABLE_GROW_CAP` and "defer gengc as pure generational GC."

## What Changed

The pass count did not move, but the failure floor did. The important progress is structural:

- **Canonical `LuaTable` moved to `lua-types`**. The flat `Vec<(K, V)>` table placeholder is retired; `crates/lua-vm/src/table.rs` is now a shim/re-export. The old `FLAT_TABLE_GROW_CAP` explanation is obsolete.
- **Table growth now uses a real array/hash implementation** with a principled cap (`TOTAL_GROW_CAP=1<<20`) instead of a fake per-table 8192-entry cap.
- **Coroutine/open-upvalue diagnostics got sharper**. The `gengc.lua:99` failure was reproduced by a canary under incremental mode, so it is not primarily a generational-GC algorithm bug.
- **Ghost abstraction audit exists**. `docs/GHOST_ABSTRACTION_REGISTER.md` and `harness/check_ghost_abstractions.sh` should stay in the gate. The goal is to prevent new "temporary compile surface became runtime architecture" regressions.
- **GC canaries exist** under `harness/canaries/gc/`. These should be run before spending on GC/gengc work.
- **Several error formatting and debug/runtime routing fixes landed**, moving failures deeper rather than flipping tests.

## Current Failure Matrix

| Test | Current failure | Primary family | Next move |
|---|---|---|---|
| `files` | panic at `io_lib.rs:1563` | stdlib TODO | Fix the reachable TODO first. This is the cleanest next win. |
| `errors` | `:stdin:591 assertion failed` | error text/source formatting | Instrument the failing `checkmessage` tuple, then patch the exact mismatch. |
| `nextvar` | `:stdin:16 assertion failed` | error text/checkerror | Identify which `checkerror` call got the wrong message or wrong error kind. |
| `db` | `:stdin:28 assertion failed` | debug line hooks | Audit `debug.sethook(..., "l")` event timing and `getinfo` line state. |
| `coroutine` | `:stdin:327 assertion failed` | yield/resume semantics | Needs a narrow spec around yield-through-pcall, not a broad coroutine prompt. |
| `locals` | `:stdin:982 assertion failed` | `__close` + coroutine yield | Same family as coroutine, with to-be-closed propagation. |
| `cstack` | stack overflow detection section | C stack / coroutine recursion / GC | Needs a dedicated stack-depth and close-chain spec. |
| `gc` | TIMEOUT | reachability convergence | Self-referenced threads; likely root/fixed-point/cycle traversal work. |
| `gengc` | `:stdin:130 assertion failed` | GC barrier/reachability | Do not dispatch as "implement generational GC"; first wire/check barriers and canaries. |
| `literals` | `cannot resume dead coroutine` | coroutine state after error | Medium follow-up after coroutine state machine fixes. |
| `all` | TIMEOUT | composite downstream | Re-test after `gc` timeout is fixed; then handle CWD/dofile if still needed. |

## Required Gates

Run these before accepting any Phase F commit:

```bash
cargo build -p lua-cli -q
./harness/check_ghost_abstractions.sh
./harness/run_official_test.sh reference/lua-c/testes/files.lua
./harness/run_official_test.sh reference/lua-c/testes/errors.lua
./harness/run_official_test.sh reference/lua-c/testes/nextvar.lua
./harness/run_official_test.sh reference/lua-c/testes/db.lua
```

For coroutine/GC changes, add:

```bash
./harness/canaries/gc/run_gc_canaries.sh
./harness/run_official_test.sh reference/lua-c/testes/coroutine.lua
./harness/run_official_test.sh reference/lua-c/testes/gc.lua
./harness/run_official_test.sh reference/lua-c/testes/gengc.lua
```

If a change touches table dispatch, add:

```bash
./harness/run_official_test.sh reference/lua-c/testes/nextvar.lua
./harness/run_official_test.sh reference/lua-c/testes/literals.lua
./harness/run_official_test.sh reference/lua-c/testes/gc.lua
```

## Dispatch Order

### F-0: Rebaseline After Table Work

Before launching any new agents, run:

```bash
cargo build -p lua-cli -q
./harness/check_ghost_abstractions.sh
./harness/run_official_all.sh
```

Capture the new `run_all.tsv` and update the failure matrix above. The table refactor changed enough runtime behavior that stale failure causes are not trustworthy.

Acceptance:

- `cargo build -p lua-cli -q` passes.
- Ghost check passes or only reports registered, still-active ghosts.
- The failure matrix in this doc matches the current `run_all.tsv`.

### F-1: Four Tractable Single-Test Slices

These are the next good budget targets. They are independent enough to run in parallel worktrees after F-0.

#### F-1.a `files.lua`: panic at `io_lib.rs:1563`

This should be treated as a reachable TODO/stub fix, not as a broad file-I/O rewrite.

Instructions for agent:

1. Open `crates/lua-stdlib/src/io_lib.rs` at the panic site.
2. Identify the exact C-Lua function being stubbed.
3. Implement the narrow missing behavior using the existing `LStream`/file-handle registry patterns.
4. Do not add direct `std::fs` access in `lua-stdlib`; platform operations go through hooks or existing handle abstractions.
5. Run only `files.lua` first, then the gate set.

Estimated cost: **$10-15 sonnet/opus-lite**.

Acceptance:

- `files.lua` advances past `io_lib.rs:1563`.
- No new ghost abstraction entry is needed unless a real new hook is introduced.

#### F-1.b `errors.lua`: line 591

The previous fixes moved this deep into `errors.lua`, so do not guess from the line number alone.

Instructions for agent:

1. Add temporary instrumentation to the test copy or harness wrapper to print failing `(prog, expected, actual)`.
2. Remove instrumentation before final commit.
3. Patch the runtime source of the exact message/source-prefix mismatch.
4. Avoid broad "normalize all error messages" changes.

Estimated cost: **$15 opus**.

Acceptance:

- `errors.lua` advances past line 591 or passes.
- Any new error-text helper is covered by a focused unit or official-test gate.

#### F-1.c `nextvar.lua`: line 16 `checkerror`

Line 16 is early in `nextvar.lua`, but the failure is usually caused by the first `checkerror` helper seeing a different message than C-Lua.

Instructions for agent:

1. Instrument `checkerror` to print the failing function/test name and actual error.
2. Determine whether the mismatch is:
   - wrong VM error text,
   - wrong error source prefix,
   - wrong error kind,
   - or wrong behavior that only happens to surface through message comparison.
3. Patch the smallest runtime location.

Estimated cost: **$20 opus**.

Acceptance:

- `nextvar.lua` advances past line 16.
- No regression in `errors.lua`.

#### F-1.d `db.lua`: line-hook event timing

The current failure at line 28 is too early for "full debug library completeness"; it is probably line-hook semantics.

Instructions for agent:

1. Read `reference/lua-c/testes/db.lua` lines 1-40.
2. Compare expected hook events with our `debug.sethook` implementation.
3. Patch event timing or line-number state; do not rewrite debug library wholesale.

Estimated cost: **$25-35 opus**.

Acceptance:

- `db.lua` advances past line 28.
- `errors.lua` remains stable because both share source/line formatting machinery.

F-1 target: **33/44 → 36-37/44** if all four land cleanly.

## F-2: Coroutine/Yield Family

Do not launch one broad "fix coroutine" agent. The recent R-beta/R-gamma attempts stalled because the scope was too wide.

Split into three specs:

### F-2.a `coroutine.lua`: yield-through-pcall

Likely surface:

- `pcall_k` / `call_k` continuation state.
- `CIST_YPCALL` and resume-time unroll.
- Result ordering after yield resumes through a protected call.

Acceptance:

- `coroutine.lua` advances past line 327.
- `pcall`, `xpcall`, `coroutine.resume`, and `coroutine.wrap` smoke tests still pass.

Estimated cost: **$40-60 opus**.

### F-2.b `locals.lua`: to-be-closed variables in coroutines

Likely surface:

- `__close` invocation during coroutine close/reset.
- Error propagation from close metamethods.
- Yield attempt inside close metamethod.

Acceptance:

- `locals.lua` advances past line 982.
- `coroutine.lua` does not regress.

Estimated cost: **$40-60 opus**.

### F-2.c `literals.lua`: coroutine state after error

Current visible failure is `cannot resume dead coroutine`, which suggests state transition after an error/yield path, not literal parsing.

Acceptance:

- `literals.lua` advances or passes.
- No regression in `coroutine.lua`.

Estimated cost: **$25-40 opus** after F-2.a.

## F-3: GC/Reachability Family

The key lesson from the canaries: `gengc` is not currently a pure "implement generational GC" task. It is exposing more basic reachability/barrier gaps.

### F-3.a `gc.lua`: self-referenced threads timeout

Likely surface:

- Mark fixed point over threads/open upvalues.
- Cycle-safe tracing of self-referenced thread/table graphs.
- Registry pruning order.

Do not use `strong_count` reasoning; D-1 `GcWeak` is placeholder-like and not a real liveness source.

Acceptance:

- `gc.lua` no longer times out.
- GC canaries still pass.

Estimated cost: **$60-100 opus**.

### F-3.b `gengc.lua`: barrier/reachability canary

Current state:

- `canary_b_coro_upvalue` proved the earlier failure was reproducible under incremental mode.
- The current `gengc.lua` failure at line 130 is after the non-`T` blocks, so inspect the exact combined test and output before dispatching.
- GC barriers are still known ghosts (`gc_barrier_back`, `gc_barrier_upval`, `GcHandle::barrier*`).

Correct approach:

1. Re-run GC canaries in both modes.
2. If incremental canaries fail, fix reachability before generational behavior.
3. Add internal tests for barrier calls before claiming `gengc` is supported.
4. Only then consider real age cohorts/minor cycles.

Acceptance:

- `gengc.lua` advances/passes.
- GC canaries pass in both incremental and generational modes.
- No "official suite blessed a shim" outcome: if gen mode is behavior-correct but perf-defeatured, document it explicitly.

Estimated cost: **$60-150 opus**, depending on whether barriers or only reachability are missing.

## F-4: C Stack And Composite Runner

### F-4.a `cstack.lua`

This is not just a message mismatch. It intersects:

- `nCcalls` accounting,
- coroutine close chains,
- deep `coroutine.wrap` recursion,
- GC traversal under extreme stack pressure.

Spec it only after F-2.a/F-2.b land, because coroutine state changes will move the failure.

Estimated cost: **$40-70 opus**.

### F-4.b `all.lua`

Currently downstream of `gc.lua` timing out. After `gc` passes, if `all` still fails:

- ensure `dofile("main.lua")` resolves relative to the official `testes/` directory;
- isolate global state between subtests if composite execution exposes cross-test pollution.

Estimated cost: **$10-20** after `gc` is stable.

## Expected Budget

| Slice | Cost | Possible pass impact |
|---|---:|---:|
| F-0 rebaseline/gates | human only | 0 |
| F-1 files/errors/nextvar/db | $70-90 | +2 to +4 |
| F-2 coroutine/locals/literals | $105-160 | +1 to +3 |
| F-3 gc/gengc | $120-250 | +1 to +3 |
| F-4 cstack/all | $50-90 | +1 to +2 |

Realistic next-session target with ~$300: **39-41/44**.

Full completion target: **43-44/44**, but only if GC reachability and coroutine close/yield semantics are solved rather than papered over.

## Dispatch Notes

Use agents for bounded slices, not architectural diagnosis loops.

Good prompts:

- "Fix `files.lua` panic at `io_lib.rs:1563`; do not touch coroutine code."
- "Instrument `errors.lua` checkmessage, identify the exact mismatch, remove instrumentation, patch runtime."
- "Make `canary_b_coro_upvalue` pass under incremental mode; do not implement generational GC."

Bad prompts:

- "Fix coroutine.lua."
- "Implement gengc."
- "Make gc.lua pass."
- "Clean up TODOs."

Every accepted patch should include the exact official test(s) it advanced and any canary/gate output. If a patch only changes the failure point, record the new line in the matrix instead of counting it as done.
