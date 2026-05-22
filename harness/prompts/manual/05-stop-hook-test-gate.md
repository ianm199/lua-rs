# Harness v5: Stop-hook test-gate

**Problem this solves**

Today the Stop hook commits the working tree on every agent turn if `cargo build` is clean. That has caused real regressions all week:

- An agent added `_G` walking to `arg_error_impl` for nicer error messages — built clean — Stop hook committed — turned out to be re-entrant and caused Rust stack overflow on `errors.lua`. **Days** of mega_loop rounds chased it.
- The GC budget slice introduced a state-machine bug that made `collectgarbage("collect")` hang. Stop hook committed because build was clean. mega_loop scan caught it eventually but not before several other commits piled on top.
- The Phase E coroutine slices broke `literals.lua` (was passing, started erroring with `Runtime: Nil`). Found later via the full-suite scan.

**Mechanism**: each agent commit compiles, so the build-only gate is satisfied. A net-regression doesn't surface until the next mega_loop scan, by which point 1-5 more commits have layered on.

## What v5 changes

The Stop hook becomes a script `harness/stop-hook.sh` that runs:

1. `cargo build -p lua-cli -q` (must be clean)
2. **Smoke set** — a fixed list of 6 tests, each with a 20s timeout (~2 min worst case)
3. Compare pass count to `harness/baseline-smoke.tsv` (tracked file)
   - **No regression** → allow commit, update baseline
   - **Regression** → reject commit, leave working tree dirty, emit diagnostic to agent transcript

## The smoke set

Pick tests that exercise the regression vectors we've actually hit this week:

```
reference/lua-c/testes/strings.lua    # stdlib + format
reference/lua-c/testes/closure.lua    # upvalues + closures
reference/lua-c/testes/tracegc.lua    # GC + require
reference/lua-c/testes/big.lua        # parser
reference/lua-c/testes/sort.lua       # table.sort (was the recursion vector)
reference/lua-c/testes/math.lua       # numeric, fast/stable
```

Plus, **if the agent's prompt referenced a specific test file** (via the `__PROG__` template var), include that file in the smoke set too. The Stop hook reads the agent's prompt-template to detect this.

All six should currently pass on main. They take ~5s combined on a clean build (worst case ~120s with timeouts).

## baseline-smoke.tsv format

Single line per test, tab-separated:

```
strings.lua	PASS	d15cd9e
closure.lua	PASS	1dbaa1d
tracegc.lua	PASS	8c48cb1
big.lua	PASS	452a433
sort.lua	PASS	2d7198f
math.lua	PASS	bb42766
```

Third column is the commit SHA when this test first passed (audit trail). Updated by the script when a previously-failing test passes.

## Stop-hook script outline

```bash
#!/usr/bin/env bash
# .claude/hooks/stop.sh — replaces the bare auto-commit
set -uo pipefail
ROOT="$(git rev-parse --show-toplevel)"
cd "$ROOT"

# 1. Build gate
cargo build -p lua-cli -q 2>"$ROOT/harness/impl/stop-build.err" || {
    echo "[stop-hook] BUILD BROKEN — rejecting commit"
    exit 1
}

# 2. Smoke set + the agent's target test (if any)
SMOKE_TESTS=(strings.lua closure.lua tracegc.lua big.lua sort.lua math.lua)
if [ -n "${AGENT_TARGET_PROG:-}" ]; then
    SMOKE_TESTS+=("$(basename "$AGENT_TARGET_PROG")")
fi

BASELINE="$ROOT/harness/baseline-smoke.tsv"
TMP_TSV=$(mktemp)
trap "rm -f $TMP_TSV" EXIT

failed=0
for t in "${SMOKE_TESTS[@]}"; do
    status=$(run_one_test "reference/lua-c/testes/$t")  # use harness/run_official_test.sh logic
    echo -e "$t\t$status" >> "$TMP_TSV"
    # If this test was passing in baseline but now fails -> regression
    base=$(grep "^$t	" "$BASELINE" | cut -f2)
    if [ "$base" = "PASS" ] && [ "$status" != "PASS" ]; then
        echo "[stop-hook] REGRESSION: $t was PASS, now $status — rejecting commit"
        failed=1
    fi
done

if [ "$failed" = "1" ]; then
    echo "[stop-hook] Working tree left dirty. Investigate or stash."
    exit 1
fi

# 3. Update baseline (any new passes)
for t in "${SMOKE_TESTS[@]}"; do
    status=$(grep "^$t	" "$TMP_TSV" | cut -f2)
    base=$(grep "^$t	" "$BASELINE" | cut -f2 || echo "")
    if [ "$status" = "PASS" ] && [ "$base" != "PASS" ]; then
        sha="$(git rev-parse --short HEAD)"
        # upsert: remove old line, append new
        grep -v "^$t	" "$BASELINE" > "$BASELINE.tmp" || true
        echo -e "$t\tPASS\t$sha" >> "$BASELINE.tmp"
        mv "$BASELINE.tmp" "$BASELINE"
    fi
done

# 4. Commit as before
git add -A
git commit -q -m "agent: auto-commit at stop ($(date -u +%Y-%m-%dT%H:%M:%SZ))"
```

## Where AGENT_TARGET_PROG comes from

mega_loop sets it explicitly when dispatching a debug agent:

```bash
# in mega_loop.sh dispatch_debug() around line 468
$timeout_cmd claude -p \
    --model "$model" \
    --append-system-prompt "$(cat PORTING.md)" \
    --setting-source "AGENT_TARGET_PROG=$prog" \  # ← new env passed through
    ...
```

For manual `claude -p` runs the user does ad-hoc, the var is unset and only the 6-test smoke set runs.

## Implementation scope

- `harness/stop-hook.sh` (new, ~80 LOC bash)
- `.claude/settings.json` → register stop-hook.sh as the Stop hook (replace bare `git add . && git commit`)
- `harness/baseline-smoke.tsv` (new, 6 lines)
- `harness/run_one_test.sh` extracted from `run_official_test.sh`'s body for stop-hook to call
- `harness/mega_loop.sh` — set `AGENT_TARGET_PROG` env around the `claude -p` call

## What NOT to do

- Don't run the full 44-test suite on every Stop. ~30s × 44 = too slow.
- Don't gate on `cargo test --workspace` — different signal, won't catch upstream-test regressions.
- Don't try to bisect / auto-revert on regression. Just refuse to commit and leave dirty for human review.
- Don't change auto-commit when the working tree is empty (the original Stop hook already handles this — preserve it).

## Acceptance

After installing v5:

1. Make a deliberately-bad change to `crates/lua-stdlib/src/string_lib.rs` (e.g. `pub fn format(...) -> ... { todo!() }`). Run an agent that touches anything. Stop hook should:
   - Build still succeeds (the change touches an export, not a runtime path) OR fails
   - One of `strings.lua` / smoke tests detects regression
   - Stop hook rejects commit, leaves dirty
   - Working tree shows the bad change for manual review

2. Make a deliberately-good change (e.g. fix a TODO that's currently failing). Stop hook should:
   - Build clean
   - Smoke set still passes (or improves)
   - Baseline updated for newly-passing tests
   - Commit lands

3. `mega_loop.sh` continues to dispatch and run rounds; per-round regression counts should drop to near-zero.

## Cost / friction

- Per-agent-turn overhead: ~5-30s for smoke set (depending on cache state)
- Worst case (cold build + slow test): 2-3 min — acceptable
- Catches ~80% of the regression categories we've seen this week, including the three from this morning's bundle work

Budget for implementation: 1 Opus run, $10. Mostly bash + careful integration with the existing Stop hook config.
