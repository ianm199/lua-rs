# Stuck: `reference/lua-c/testes/gc.lua`

Plateaued after 8+ opus rounds. v4 stuck-skip is now refusing to spend on it.
Most recent transcript: `harness/impl/mega-O2-D1-reference_lua_c_testes_gc_lua_.transcript.jsonl` (~4 MB, run 2026-05-18 15:52).

## Current failure (post-cherry-pick of 42ff10f)

```
testing incremental garbage collection
creating many objects
functions with errors
long strings
steps
steps (2)
lua: pcall_k failed: Runtime: reference/lua-c/testes/gc.lua:201: assertion failed!
```

Line 201:
```lua
if not _port then
  assert(dosteps(10) < dosteps(2))   -- ← fires here
end
```

`dosteps(siz)` counts how many `collectgarbage("step", siz)` calls it takes to finish a full GC cycle. The assertion says size-10 steps should complete the cycle in fewer ticks than size-2 steps. **Our `step` implementation ignores the `siz` parameter** — both branches return the same count, so `10 < 2` fails.

## What's actually wrong

`crates/lua-vm/src/api.rs:1915-1942` shows how `collectgarbage("step", n)` is implemented: it
1. force-reclaims dead long strings,
2. **refills `totalbytes` to a constant 32 768-byte baseline if it has dropped below**,
3. returns whether `check_step` reports cycle-complete.

The PORT NOTE next to the refill block (`api.rs:1929`) admits this is a Phase-B shim that ignores step granularity — it just wants `gcinfo()` to shrink by some observable amount on each step. Real Lua's `lgc.c:luaC_step` runs `singlestep()` in a loop bounded by `g->GCstepmul * data` until either the cycle ends or the work budget runs out. We have nothing like that.

`crates/lua-gc/src/heap.rs` only exposes `Heap::step_with_post_mark(...)` which runs a fixed-cost increment of the gray-queue drain — no caller passes a budget multiplier.

## What past agents have tried (8+ runs)

The freshest opus run (mega-O2-D1) made substantial changes — replaced the placeholder `GcRef::strong_count() == 1` finalizer gate with a proper reachability-based `to_be_finalized` queue. Committed as `9355448 agent debug: reference/lua-c/testes/gc.lua` (2026-05-18 19:52Z). The agent's own summary:

> **Root cause**: `GcRef::strong_count()` always returns `1` in Phase D-1, but `run_pending_finalizers` used `t.strong_count() == 1` as the gate for "no user reference remains — fire `__gc`". This made every `__gc` fire eagerly on every collect, prematurely de-registering finalizers.
>
> **Fix** (4 files):
> - `state.rs`: added `GlobalState.to_be_finalized` field; rewrote `collect_via_heap`'s post-mark hook so any `pending_finalizers` entry NOT visited from non-finalizer roots is moved to `to_be_finalized` and marked via `marker.mark(pf.0)`.
> - `trace_impls.rs`: removed `pending_finalizers` from `GlobalState::trace` roots (so reachability through them is distinguishable) and added `to_be_finalized` as a strong root.
> - `api.rs`: `run_pending_finalizers` now drains `to_be_finalized` (LIFO) instead of scanning `pending_finalizers` by strong count.

This fix is correct and necessary — it advances the test past the original `__gc` failure. But the **next** assertion (line 201) is a separate, unrelated problem the agent didn't reach.

Earlier rounds (66ccd3d, 2809f30, 1ca9c4c, 64e74d7, 6158afd) and the D-1f / D-2 phases each fixed one bug and surfaced the next:
- D-1e — swap GcRef inner to `Gc<T>`
- D-1f — five mark-sweep bugs
- D-2 — reachability-based weak-table sweep
- finalizer reachability (O2-D1, above)

The pattern is clear: each round fixes one layer; gc.lua has many layers.

## Suggested next move

This test exercises 14 distinct GC behaviors. The next blocker (step sizing) is a **scope** problem, not a bug — we'd need to give `step_with_post_mark` a work-budget parameter and have `collectgarbage("step", n)` scale it. That's an architectural addition, not a fix.

Three options ranked:

1. **Skip this test until Phase E.** Put it in `SKIP_TESTS` permanently. It needs the real generational/incremental GC to ever pass; one-shot agent rounds will keep peeling layers without converging. ★ pragmatic.
2. **Carve out a stable subset.** `gc.lua` has ~14 `do ... end` blocks each testing one feature. Run them as separate test programs. The early ones (basic collection, weak tables, finalizers) likely already pass — the step-granularity / pause-multiplier / generational ones definitely don't. Adds budget-friendly green ratchets and isolates the hard work.
3. **Replace step shim with real budget.** Add `Heap::step(budget: usize)` that drains up to N gray-queue entries, plumb through `collectgarbage("step", n)`. Likely 1-2 days human work; agent-driven attempts have been spending $10/round without converging.

## Files most touched

`crates/lua-vm/src/api.rs`, `crates/lua-vm/src/state.rs`, `crates/lua-types/src/trace_impls.rs`, `crates/lua-gc/src/heap.rs`.
