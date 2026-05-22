# Stuck tests

Programs in `reference/lua-c/testes/` that the autonomous harness has plateaued on. Each doc captures: current failure point, what past agents tried (with transcript pointers), what's actually wrong, and a suggested next move.

| Test | Why agents got stuck | Next-move category |
|---|---|---|
| [gc.lua](gc.lua.md) | Step-granularity in `collectgarbage("step", n)` ignores `n`; needs `Heap::step(budget)` plumbing. | Architectural — defer to Phase E, OR carve into subtests, OR ~2 days human work |
| [files.lua](files.lua.md) | 5 file-op stubs (`f_seek`, `f_read`, `io_read`, `f_setvbuf`, partial `f_write`) blocked by `&mut state` × `&mut handle` borrow split. | Mechanical refactor — wrap file handle in `RefCell`. Good single-Opus-run candidate with a tight prompt |
| [errors.lua](errors.lua.md) | Each `checkmessage` mismatch is its own tiny error-wording bug; agents hit "assertion failed" without knowing which one. | Pre-instrument the test to surface (prog, expected, actual) before dispatching another agent |

## Why these plateaued (v4 stuck-skip in action)

The harness escalated each from Sonnet → Opus, ran ≥2 Opus rounds, observed no pass-count progress (despite real commits landing), and marked them `[skip stuck prog (no progress 2 rounds)]`. That's the intended behavior — stop pouring $10/round into agent attempts that keep peeling layers without converging. Captured in `docs/RETROSPECTIVE_AND_PRODUCTIZATION.md` §11.

## How agents *do* make progress on these — without passing them

Look at `git log --grep=gc.lua`: 8 commits. Each one fixed a real bug (D-1e, D-1f, D-2 weak-table sweep, finalizer reachability, …) and advanced the test by one phase. The test still fails — but the codebase is meaningfully better. So "stuck" is misleading: these are **multi-layer** tests where every layer fix is its own valuable PR. The harness just can't see that, because it only counts whole-test passes.
