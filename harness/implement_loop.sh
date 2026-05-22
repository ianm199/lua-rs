#!/usr/bin/env bash
# implement_loop.sh — panic-driven Phase-B/F implementation loop.
#
# Each iteration:
#   1. cargo run lua-cli with `print("hello")`
#   2. If it succeeds, log and exit
#   3. If it panics with `not yet implemented: <tag>: <fn>`, dispatch an
#      agent to implement <fn> on LuaState (or whichever type it lives on)
#   4. Loop
#
# Stop conditions:
#   - Success (program ran without panic, output contained "hello")
#   - Same function panics twice in a row (agent couldn't actually fix it)
#   - MAX_ITER iterations
#   - Cost cap (LOOP_COST_CAP)
#
# Safety:
#   - Each agent is scoped via the existing PreToolUse type-vocab gate
#     and Stop-hook chain. Cannot introduce new type duplications.
#   - Per-iteration cargo build verifies the agent's edits compile.
#   - If cargo build fails after an agent's edits, we revert with
#     git reset --hard before the next iteration to avoid carrying
#     a broken tree forward.
#
# Usage:
#   nohup ./harness/implement_loop.sh > /tmp/impl_loop.log 2>&1 &

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

OUT_DIR="harness/impl"
mkdir -p "$OUT_DIR"
STATE="$OUT_DIR/state.jsonl"
LOG="$OUT_DIR/loop.log"
LAST_PANIC_FILE="$OUT_DIR/last_panic.txt"
touch "$STATE" "$LOG" "$LAST_PANIC_FILE"

MAX_ITER=${MAX_ITER:-25}
LOOP_COST_CAP=${LOOP_COST_CAP:-200.00}
TEST_PROG=${TEST_PROG:-'print("hello")'}

TOTAL_COST="0.00"
PREV_FUNC=""
STUCK_COUNT=0

emit() {
    local ts; ts=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
    echo "[$ts] $*" | tee -a "$LOG"
}

record() {
    local action="$1" detail="${2:-}"
    local ts; ts=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
    jq -c -n --arg ts "$ts" --arg action "$action" --arg detail "$detail" \
        --argjson cost "$TOTAL_COST" \
        '{ts: $ts, action: $action, detail: $detail, total_cost: $cost}' >> "$STATE"
}

run_test() {
    cargo run -q -p lua-cli -- "$TEST_PROG" 2>&1
}

extract_panic_func() {
    # Extract the first todo!() description from the panic output. Accepts
    # symbol-style tags (`require_lib`, `LuaTable::get`) AND free-text
    # descriptions (`LuaTable metatable accessor`). Returns whatever follows
    # the second `: ` on the panic line, up to end-of-line.
    grep "not yet implemented:" "$1" | head -1 | sed -E 's/^.*not yet implemented: [a-z0-9_-]+: //' | sed -E 's/[[:space:]]+$//'
}

extract_panic_loc() {
    # Extract the file:line:col of the first panic, so the agent can navigate
    # to the todo!() directly without guessing.
    grep -oE "panicked at [^:]+:[0-9]+:[0-9]+" "$1" | head -1 | sed 's/^panicked at //'
}

dispatch_implement_agent() {
    local func="$1"
    local loc="${2:-}"
    local safe_name; safe_name=$(echo "$func" | tr -c 'A-Za-z0-9_' '_' | cut -c1-40)
    local out_json="$OUT_DIR/iter-$ITER-$safe_name.translator.json"
    local transcript="$OUT_DIR/iter-$ITER-$safe_name.transcript.jsonl"

    local prompt="You are an Implement-stub agent. Scope: replace exactly one todo!() call.

Target todo!() description: \`$func\`
Panic location: $loc

The Lua 5.4 → Rust port has reached the stage where lua-cli builds but
panics at runtime on todo!() stubs. Your job is to replace ONE specific
todo!() with a real implementation.

Process:

1. Open the panic location ($loc) and read 30-50 lines of context around
   the todo!(). Identify the function/method that contains it.

2. If the description has a Type::method form, search:
   grep -rn 'impl <TypeName>' crates/lua-vm/src/ crates/lua-stdlib/src/
   For plain names, search:
   grep -rn 'fn <name>' crates/lua-vm/src/ crates/lua-stdlib/src/

2. Read the C source for context. The canonical mapping is in
   ANALYSES/file_deps.txt; typical Lua functions live in:
   - reference/lua-5.4.7/src/lapi.c (lua_X functions on LuaState)
   - reference/lua-5.4.7/src/lauxlib.c (luaL_X helpers)
   - reference/lua-5.4.7/src/lstate.c
   Grep the C source for the matching name and read 30-50 lines around it.

3. Write a faithful Rust port of the C body. Adhere to PORTING.md:
   - No String/str for Lua data; use LuaString or [u8].
   - No tokio/async/futures.
   - LuaError for fallible paths; LuaResult is alias.
   - Use canonical types from lua-types and lua-vm. NEVER introduce
     a 'pub struct/enum NAME' for a name in harness/type-vocabulary.tsv.
     (The PreToolUse hook will block your edit if you try.)

4. If implementing \`$func\` requires calling other todo!() functions,
   that's OK — they panic at THEIR call sites, not yours. Don't recurse
   into implementing those; the loop catches them next iteration.

5. After your edits:
   cargo build -p lua-cli 2>&1 | tail -20
   to verify they compile. If they don't compile, fix until they do.
   THEN stop. Do NOT actually try to run the program — the loop will.

Constraints:

- Edit ONLY \`$func\` and immediate-neighbor helpers if needed.
- Do NOT modify other todo!() bodies, even if you see them nearby.
- Do NOT modify lua-types or harness/*.
- Keep the SAFETY budget — no new unsafe blocks without explicit need.
- Match the C-source semantics. Where the C uses raw pointers or
  global state that doesn't translate naturally, use the patterns
  already established by neighboring functions (look at sibling
  functions on LuaState for style cues).

Report (under 150 words):

- Which file you modified and which line(s).
- Brief summary of what \`$func\` now does.
- Any todo!() calls inside your new impl that will surface next.
- Whether cargo build -p lua-cli is green after your edits."

    export CLAUDE_CONFIG_DIR="$HOME/.claude-personal"
    unset ANTHROPIC_API_KEY ANTHROPIC_AUTH_TOKEN
    export CLAUDE_CODE_MAX_OUTPUT_TOKENS="${CLAUDE_CODE_MAX_OUTPUT_TOKENS:-64000}"
    export CLAUDE_TARGET_RS_FILE="crates/lua-vm/src/state.rs"

    claude -p \
        --append-system-prompt "$(cat PORTING.md)" \
        --allowedTools "Read,Write,Edit,Glob,Grep,Bash(cargo build*),Bash(cargo check*),Bash(grep *),Bash(rg *),Bash(cat *),Bash(head *),Bash(tail *),Bash(wc *),Bash(find *)" \
        --permission-mode dontAsk \
        --output-format stream-json \
        --include-partial-messages \
        --verbose \
        --max-budget-usd 10.00 \
        "$prompt" \
        2>>"$OUT_DIR/iter-$ITER-$func.stderr" \
        | tee "$transcript" >/dev/null

    jq -s 'map(select(.type == "result")) | .[-1] // {}' "$transcript" > "$out_json" 2>/dev/null || echo '{}' > "$out_json"
    local cost
    cost=$(jq -r '.total_cost_usd // 0' "$out_json")
    TOTAL_COST=$(awk -v a="$TOTAL_COST" -v b="$cost" 'BEGIN { printf "%.4f", a + b }')
    echo "$cost"
}

# ───── Main loop ───────────────────────────────────────────────────────

emit "═════════════════════════════════════════════════════════════════"
emit "Implement-loop start. test program: $TEST_PROG"
emit "  MAX_ITER=$MAX_ITER  LOOP_COST_CAP=\$$LOOP_COST_CAP"
emit "═════════════════════════════════════════════════════════════════"
record "run_start" "$TEST_PROG"

for ITER in $(seq 1 $MAX_ITER); do
    emit "─── iter $ITER  (spent: \$$TOTAL_COST) ───"

    # Build check (cheap, catches uncommitted broken state)
    if ! cargo build -q -p lua-cli 2>"$OUT_DIR/iter-$ITER.build.err"; then
        emit "  cargo build broke at iter $ITER — reverting to last commit"
        git reset --hard HEAD >/dev/null 2>&1
        record "build_broke" "iter=$ITER"
    fi

    # Run the test
    output_file="$OUT_DIR/iter-$ITER.run.out"
    run_test > "$output_file" 2>&1
    rc=$?

    if grep -q '^hello$' "$output_file" || grep -q '"hello"$' "$output_file"; then
        emit "  ★ SUCCESS: program produced 'hello' output"
        record "success" "iter=$ITER"
        break
    fi

    func=$(extract_panic_func "$output_file")
    loc=$(extract_panic_loc "$output_file")
    if [ -z "$func" ]; then
        emit "  no todo!() panic detected — final output:"
        tail -15 "$output_file" | tee -a "$LOG"
        record "no_panic" "iter=$ITER rc=$rc"
        break
    fi

    emit "  blocked on: $func"
    record "panic_detected" "func=$func iter=$ITER"
    echo "$func" > "$LAST_PANIC_FILE"

    if [ "$func" = "$PREV_FUNC" ]; then
        STUCK_COUNT=$((STUCK_COUNT + 1))
        if [ "$STUCK_COUNT" -ge 2 ]; then
            emit "  STUCK on $func after 2 consecutive iterations — bailing"
            record "stuck" "func=$func"
            break
        fi
    else
        STUCK_COUNT=0
    fi

    # Cost cap check
    if awk -v t="$TOTAL_COST" -v cap="$LOOP_COST_CAP" 'BEGIN { exit !(t > cap) }'; then
        emit "  cost cap \$$LOOP_COST_CAP exceeded; bailing"
        record "cost_cap" ""
        break
    fi

    # Dispatch
    emit "  dispatching agent for $func at $loc (budget \$10)"
    record "dispatch_start" "func=$func loc=$loc iter=$ITER"
    iter_cost=$(dispatch_implement_agent "$func" "$loc")
    emit "  agent done. iter cost=\$$iter_cost  total=\$$TOTAL_COST"
    record "dispatch_done" "func=$func cost=$iter_cost"

    PREV_FUNC="$func"
done

emit "═════════════════════════════════════════════════════════════════"
emit "Loop end. Total cost: \$$TOTAL_COST  Total iterations: $((ITER-1))"
emit "═════════════════════════════════════════════════════════════════"
record "run_end" "iterations=$((ITER-1)) total_cost=$TOTAL_COST"

exit 0
