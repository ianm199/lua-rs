#!/usr/bin/env bash
# mega_loop.sh — wide-funnel driver for the lua-rs-port stub frontier.
#
# Instead of stair-stepping one test program at a time, this:
#   1. Runs N test programs in parallel-ish (sequentially with timeouts)
#   2. Collects the union of `not yet implemented:` panics
#   3. Drains the resulting stub queue, dispatching agents (one per
#      unique stub) — the agent prompt already invites family expansion,
#      so each dispatch typically clears 5-15 sibling stubs in one go
#   4. Re-runs all programs after the queue drains, finds any new
#      surfaced stubs, repeats
#   5. Stops when all programs pass OR a stuck condition fires
#
# Usage:
#   nohup ./harness/mega_loop.sh > /tmp/mega.log 2>&1 &
#
# Programs source (in priority order):
#   1. argv if given:   ./harness/mega_loop.sh prog1 prog2 ...
#   2. $TEST_PROGS_FILE env (newline-separated, # comments OK)
#   3. Built-in default list (see DEFAULT_PROGS below)

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

OUT_DIR="harness/impl"
mkdir -p "$OUT_DIR"
LOG="$OUT_DIR/mega.log"
QUEUE="$OUT_DIR/stub_queue.txt"
touch "$LOG" "$QUEUE"

MAX_OUTER=${MAX_OUTER:-20}
MAX_PER_OUTER=${MAX_PER_OUTER:-25}
PER_AGENT_BUDGET=${PER_AGENT_BUDGET:-20.00}
TEST_TIMEOUT_S=${TEST_TIMEOUT_S:-20}
SESSION_BUDGET=${SESSION_BUDGET:-400.00}
AUTO_COMMIT=${AUTO_COMMIT:-1}
COMMIT_LOCK_DIR="$OUT_DIR/commit.lock"
PREV_PASS_COUNT=-1
# Last-seen error sig per prog (parallel-array, bash-3.2-compatible).
# LAST_SIG_KEY[i] = prog key; LAST_SIG_VAL[i] = normalized sig.
# Stuck = same sig as previous round. Reset when sig changes (= progress).
LAST_SIG_KEY=()
LAST_SIG_VAL=()
# Per-test stuck count + model assignment (parallel arrays). When a test's
# signature matches the previous round, increment its count. count == 1 →
# escalate that test's agent to MODEL_ESCALATE (Opus). count >= 2 → give up
# and skip in subsequent rounds. Reset to 0 if signature changes (progress).
STUCK_COUNT_KEY=()
STUCK_COUNT_VAL=()
MODEL_DEFAULT=${MODEL_DEFAULT:-sonnet}
MODEL_ESCALATE=${MODEL_ESCALATE:-opus}
# Comma-separated list of test labels to skip entirely (Phase E / heavy
# D-2 work — no point burning agent budget on these without prerequisite
# infrastructure landing first). Override at invocation time:
#   SKIP_TESTS="calls.lua,nextvar.lua" ./harness/mega_loop.sh
SKIP_TESTS=${SKIP_TESTS:-"calls.lua,nextvar.lua,gengc.lua"}
# Comma-separated list of test labels that start on MODEL_ESCALATE (Opus)
# directly — skip the Sonnet-first attempt for tests where Sonnet has no
# realistic shot at the underlying design problem (e.g. ephemeron
# fixed-point in gc.lua, to-be-closed semantics in locals.lua). Tests not
# in this list still go Sonnet → Opus via stuck-escalation.
HARD_TESTS=${HARD_TESTS:-"gc.lua,locals.lua"}
# Tell the lua-rs binary to emit the [n/4] + [ok] status lines that
# count_passing depends on. The CLI defaults to quiet output for human use.
export LUA_RS_VERBOSE=1

# A frontier of Lua test programs. Each entry is a TSV: program\texpected_stdout.
# expected_stdout uses literal \n for newlines and \t for tabs (interpreted via printf %b).
# Empty expected_stdout = "anything non-error counts as a pass".
DEFAULT_PROGS=(
    $'print("hello")\thello'
    $'print(1+2)\t3'
    $'local x = 5; print(x)\t5'
    $'local x = 5; local y = 3; print(x+y)\t8'
    $'if 1 < 2 then print("yes") end\tyes'
    $'for i=1,3 do print(i) end\t1\n2\n3'
    $'local i = 1; while i <= 3 do print(i); i = i + 1 end\t1\n2\n3'
    $'local function f(x) return x*2 end; print(f(5))\t10'
    $'local function fact(n) if n <= 1 then return 1 else return n * fact(n-1) end end; print(fact(5))\t120'
    $'local function counter() local n = 0; return function() n = n + 1; return n end end; local c = counter(); print(c()); print(c())\t1\n2'
    $'local t = {10,20,30}; print(t[1])\t10'
    $'local t = {}; table.insert(t, "a"); print(t[1])\ta'
    $'print("hi " .. "there")\thi there'
    $'print(#"hello")\t5'
    $'print(string.upper("hello"))\tHELLO'
    $'print(string.format("%d", 42))\t42'
    $'print(math.sqrt(16))\t4.0'
    $'local ok, err = pcall(function() error("boom") end); print(ok)\tfalse'
    $'local a, b = (function() return 1, 2 end)(); print(a, b)\t1\t2'
    $'local t = {1,2,3}; local s = 0; for i=1,#t do s = s + t[i] end; print(s)\t6'
    $'local f = function(x, y) return x or y end; print(f(nil, 42))\t42'
    $'local t = {}; for i=1,3 do t[#t+1] = i*i end; for i=1,#t do print(t[i]) end\t1\n4\n9'
    $'local o = {x=10}; function o.get() return o.x end; print(o.get())\t10'
    $'local s = ""; for i=1,3 do s = s .. tostring(i) end; print(s)\t123'
    $'local function fib(n) if n < 2 then return n else return fib(n-1) + fib(n-2) end end; print(fib(10))\t55'
    # --- next wave: method syntax, varargs, ipairs/pairs, metatables, more stdlib ---
    $'local o = {x=10}; function o:get() return self.x end; print(o:get())\t10'
    $'local function sum(...) local s = 0; for _, v in ipairs({...}) do s = s + v end; return s end; print(sum(1,2,3,4,5))\t15'
    $'local t = {a=1, b=2, c=3}; local total = 0; for k,v in pairs(t) do total = total + v end; print(total)\t6'
    $'local mt = {__add = function(a, b) return {v = a.v + b.v} end}; local a = setmetatable({v=10}, mt); local b = setmetatable({v=5}, mt); print((a+b).v)\t15'
    $'print(string.sub("hello world", 7))\tworld'
    $'print(string.rep("ab", 3))\tababab'
    $'print(string.find("hello world", "world"))\t7\t11'
    $'print(math.max(1, 5, 3, 7, 2))\t7'
    $'print(math.floor(3.7))\t3'
    $'local t = {3, 1, 4, 1, 5, 9, 2, 6}; table.sort(t); for i=1,#t do io.write(t[i]); if i < #t then io.write(",") end end; print()\t1,1,2,3,4,5,6,9'
    $'local function rev(s) local r = ""; for i = #s, 1, -1 do r = r .. string.sub(s, i, i) end; return r end; print(rev("hello"))\tolleh'
    $'local function compose(f, g) return function(x) return f(g(x)) end end; local addone = function(x) return x+1 end; local double = function(x) return x*2 end; print(compose(addone, double)(5))\t11'
    $'local function range(a, b) local t = {}; for i=a,b do t[#t+1] = i end; return t end; local r = range(1, 5); print(#r, r[1], r[5])\t5\t1\t5'
    # --- codegen gaps that block the official Lua test suite ---
    $'print(not true)\tfalse'
    $'print(not false)\ttrue'
    $'print(not nil)\ttrue'
    $'if not false then print("y") end\ty'
    $'local x = 1; if not (x == 2) then print("ne") end\tne'
    $'local r = (1==1 and 2==2 and 3==3); print(r)\ttrue'
    $'local r = (1==1 and 2==3 and 4==4); print(r)\tfalse'
    $'local function clip(x, lo, hi) if x < lo then return lo elseif x > hi then return hi else return x end end; print(clip(5,1,10), clip(-1,1,10), clip(99,1,10))\t5\t1\t10'
    $'do local i = 0; repeat i = i + 1; print(i) until i >= 3 end\t1\n2\n3'
    $'for i = 5, 1, -1 do print(i) end\t5\n4\n3\n2\n1'
    # --- official Lua 5.4 test files (sourced from reference/lua-c/testes) ---
    # No expected stdout: counted as a pass when binary exits with [ok] and no
    # panic/[err] — these tests typically self-assert with assert(...) calls.
    '@FILE:reference/lua-c/testes/tracegc.lua'
    '@FILE:reference/lua-c/testes/big.lua'
    '@FILE:reference/lua-c/testes/verybig.lua'
    '@FILE:reference/lua-c/testes/bwcoercion.lua'
    '@FILE:reference/lua-c/testes/vararg.lua'
    '@FILE:reference/lua-c/testes/heavy.lua'
    # These fail with a Phase-B stub blocker, so they feed the surface-scan
    # frontier and let one family-aware agent dispatch clear them.
    '@FILE:reference/lua-c/testes/goto.lua'
    '@FILE:reference/lua-c/testes/sort.lua'
    '@FILE:reference/lua-c/testes/closure.lua'
    # Phase-D GC test files. gc.lua exercises the collector; gengc.lua tests
    # generational mode (we only implement full collect for now — gengc may
    # fail until later D-phase work).
    '@FILE:reference/lua-c/testes/gc.lua'
    '@FILE:reference/lua-c/testes/gengc.lua'
    # Handwritten GC smokes that exercise specific collection scenarios.
    # These deliberately call collectgarbage to drive trace() coverage.
    $'do local t = {}; for i=1,100 do t[i] = {value=i} end; t = nil end; collectgarbage("collect"); print("ok")\tok'
    $'local function makecycle() local a, b = {}, {}; a.b = b; b.a = a; return a end; for i=1,50 do local _ = makecycle() end; collectgarbage("collect"); print("ok")\tok'
    '@FILE:reference/lua-c/testes/pm.lua'
    '@FILE:reference/lua-c/testes/strings.lua'
    '@FILE:reference/lua-c/testes/bitwise.lua'
    '@FILE:reference/lua-c/testes/math.lua'
    '@FILE:reference/lua-c/testes/tpack.lua'
    '@FILE:reference/lua-c/testes/literals.lua'
    '@FILE:reference/lua-c/testes/locals.lua'
    '@FILE:reference/lua-c/testes/calls.lua'
    '@FILE:reference/lua-c/testes/constructs.lua'
    '@FILE:reference/lua-c/testes/nextvar.lua'
    '@FILE:reference/lua-c/testes/utf8.lua'
    '@FILE:reference/lua-c/testes/files.lua'
    '@FILE:reference/lua-c/testes/errors.lua'
    '@FILE:reference/lua-c/testes/api.lua'
    '@FILE:reference/lua-c/testes/attrib.lua'
    '@FILE:reference/lua-c/testes/main.lua'
    '@FILE:reference/lua-c/testes/events.lua'
    '@FILE:reference/lua-c/testes/code.lua'
)

emit() {
    local ts; ts=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
    echo "[$ts mega] $*" | tee -a "$LOG"
}

# Many official tests do `require 'bwcoercion'` etc. Point LUA_PATH at the
# testes/ directory so require() finds them. The lua-rs binary inherits this
# env var. (run_official_test.sh sets the same thing for its wrapper path.)
TESTES_DIR="$ROOT/reference/lua-c/testes"
export LUA_PATH="$TESTES_DIR/?.lua;$TESTES_DIR/?/init.lua;./?.lua;./?/init.lua"

# Resolve which test programs to use. Each entry is TSV: prog\texpected.
# Split into two parallel arrays.
RAW=()
if [ $# -gt 0 ]; then
    RAW=("$@")
elif [ -n "${TEST_PROGS_FILE:-}" ] && [ -f "$TEST_PROGS_FILE" ]; then
    while IFS= read -r line; do
        [ -z "$line" ] && continue
        [[ "$line" =~ ^# ]] && continue
        RAW+=("$line")
    done < "$TEST_PROGS_FILE"
else
    RAW=("${DEFAULT_PROGS[@]}")
fi

PREAMBLE='_soft = true
_port = true
_nomsg = true
_U = false
arg = arg or {}
_G = _G or _ENV
if _VERSION == nil then _VERSION = "Lua 5.4" end
'

PROGS=()
EXPECTED=()
LABELS=()
for entry in "${RAW[@]}"; do
    prog="${entry%%$'\t'*}"
    rest="${entry#*$'\t'}"
    if [ "$rest" = "$entry" ]; then
        exp=""
    else
        exp="$rest"
    fi
    label=""
    if [[ "$prog" == @FILE:* ]]; then
        path="${prog#@FILE:}"
        if [ ! -f "$path" ]; then
            emit "WARN: skipping @FILE entry — not found: $path"
            continue
        fi
        label="$path"
        prog="$PREAMBLE"$'\n'"$(cat "$path")"
    fi
    PROGS+=("$prog")
    EXPECTED+=("$exp")
    LABELS+=("$label")
done

emit "loaded ${#PROGS[@]} test programs (with expected outputs)"

# Make sure binary exists.
ensure_binary() {
    cargo build -q -p lua-cli >/dev/null 2>&1
}

# Run one test program with timeout, return its output. The bash-only
# fallback at the bottom spawns the binary in a subshell with a watchdog
# that SIGKILLs it after TEST_TIMEOUT_S — required on macOS where neither
# `gtimeout` (coreutils) nor `timeout` is present by default.
run_one() {
    local prog="$1"
    local out_file="$2"
    local bin="target/debug/lua-rs"
    if command -v gtimeout >/dev/null 2>&1; then
        gtimeout --signal=KILL "$TEST_TIMEOUT_S" "$bin" "$prog" > "$out_file" 2>&1
    elif command -v timeout >/dev/null 2>&1; then
        timeout --signal=KILL "$TEST_TIMEOUT_S" "$bin" "$prog" > "$out_file" 2>&1
    else
        (
            "$bin" "$prog" > "$out_file" 2>&1 &
            local pid=$!
            (
                sleep "$TEST_TIMEOUT_S"
                kill -9 "$pid" 2>/dev/null
            ) &
            local watcher=$!
            wait "$pid" 2>/dev/null
            kill "$watcher" 2>/dev/null
            wait "$watcher" 2>/dev/null
        )
    fi
}

# How many test runs to launch in parallel. lua-rs binary is single-threaded
# and each run is small; 8-16 is a sensible cap.
PARALLEL_RUNS=${PARALLEL_RUNS:-12}
# How many agent dispatches to run concurrently. Agents on the same file
# can conflict, so we serialize same-file work; this caps the *total*
# concurrent agents across different files.
PARALLEL_AGENTS=${PARALLEL_AGENTS:-3}

# Run all programs in parallel into $1 dir; output files in $1/prog-N.out.
run_all_programs() {
    local scan_dir="$1"
    mkdir -p "$scan_dir"
    local pids=()
    local i=0
    for prog in "${PROGS[@]}"; do
        i=$((i + 1))
        local out_file="$scan_dir/prog-$i.out"
        while [ "${#pids[@]}" -ge "$PARALLEL_RUNS" ]; do
            local new_pids=()
            for p in "${pids[@]}"; do
                if kill -0 "$p" 2>/dev/null; then new_pids+=("$p"); fi
            done
            pids=("${new_pids[@]+"${new_pids[@]}"}")
            [ "${#pids[@]}" -ge "$PARALLEL_RUNS" ] && sleep 0.2
        done
        ( run_one "$prog" "$out_file" || true ) &
        pids+=($!)
    done
    for p in "${pids[@]+"${pids[@]}"}"; do wait "$p" 2>/dev/null || true; done
}

# Surface scan: derive unique stub blockers from a populated scan dir.
PARALLEL_RUNS=${PARALLEL_RUNS:-8}
surface_scan() {
    local scan_dir="$1"
    local stub_lines=()
    local idx=0
    for prog in "${PROGS[@]}"; do
        idx=$((idx + 1))
        local out_file="$scan_dir/prog-$idx.out"
        [ ! -f "$out_file" ] && continue
        local stub_func; stub_func=$(grep -E "not yet implemented: [a-z0-9_-]+:" "$out_file" \
            | head -1 | sed -E 's/^.*not yet implemented: [a-z0-9_-]+: //' \
            | sed -E 's/[[:space:]]+$//')
        if [ -n "$stub_func" ]; then
            local stub_loc; stub_loc=$(grep -oE "panicked at [^:]+:[0-9]+:[0-9]+" "$out_file" \
                | head -1 | sed 's/^panicked at //')
            stub_lines+=("$stub_func"$'\t'"$stub_loc")
        fi
    done
    if [ ${#stub_lines[@]} -eq 0 ]; then
        return 0
    fi
    printf "%s\n" "${stub_lines[@]}" | sort -u -t$'\t' -k1,1
}

# Run all programs in parallel, return how many produce expected output.
# A program passes only when:
#   (a) exits cleanly ([ok] line + no panic + no [err]), AND
#   (b) if EXPECTED is non-empty, stdout (post-[4/4]) matches exactly.
count_passing() {
    local scan_dir="$1"
    local pass=0
    local idx=0
    for prog in "${PROGS[@]}"; do
        idx=$((idx + 1))
        local out_file="$scan_dir/prog-$idx.out"
        local exp="${EXPECTED[$((idx-1))]}"
        [ ! -f "$out_file" ] && continue
        if ! grep -qE "^\[ok\] execution completed" "$out_file" \
            || grep -qE "not yet implemented:" "$out_file" \
            || grep -qE "^thread '[^']+' .* panicked at " "$out_file" \
            || grep -qE "^\[err\]" "$out_file"; then
            continue
        fi
        if [ -n "$exp" ]; then
            local actual; actual=$(awk '
                /^\[4\/4\] Executing chunk/ { capture=1; next }
                /^\[ok\] execution completed/ { capture=0 }
                capture { print }
            ' "$out_file")
            local expected_decoded; expected_decoded=$(printf "%b" "$exp")
            if [ "$actual" = "$expected_decoded" ]; then
                pass=$((pass + 1))
            fi
        else
            pass=$((pass + 1))
        fi
    done
    echo "$pass"
}

# Commit any pending changes under crates/ as one commit attributed to the
# named agent. Serialized via mkdir-lock so parallel agents don't race.
# Args: kind (impl|debug), target (func or prog), out_json (translator.json).
commit_agent_changes() {
    [ "$AUTO_COMMIT" != "1" ] && return 0
    local kind="$1"
    local target="$2"
    local out_json="$3"

    local lock_tries=0
    while ! mkdir "$COMMIT_LOCK_DIR" 2>/dev/null; do
        lock_tries=$((lock_tries + 1))
        if [ "$lock_tries" -gt 120 ]; then
            emit "  commit-lock timeout, skipping commit for $kind/$target"
            return 0
        fi
        sleep 0.5
    done

    if [ -z "$(git status --porcelain crates/ 2>/dev/null)" ]; then
        rmdir "$COMMIT_LOCK_DIR" 2>/dev/null
        return 0
    fi

    local result_text; result_text=$(jq -r '.result // ""' "$out_json" 2>/dev/null)
    local title_target; title_target=$(echo "$target" | tr '\n' ' ' | cut -c1-50)
    local subject="agent ${kind}: ${title_target}"
    local body_file="$OUT_DIR/.commit-body.tmp"
    {
        echo "$subject"
        echo
        printf "%s\n" "$result_text" | head -50
    } > "$body_file"

    git add crates/ >/dev/null 2>&1
    if git commit -F "$body_file" >/dev/null 2>&1; then
        local sha; sha=$(git rev-parse --short HEAD)
        emit "  → committed $sha (${kind}/${title_target})"
    else
        emit "  → no commit (probably nothing in crates/)"
    fi
    rm -f "$body_file"
    rmdir "$COMMIT_LOCK_DIR" 2>/dev/null
}

# Dispatch a family-aware implement agent for one stub (the prompt
# already invites the agent to handle siblings while in context).
dispatch_one() {
    local func="$1"
    local loc="${2:-}"
    local safe_name; safe_name=$(echo "$func" | tr -c 'A-Za-z0-9_' '_' | cut -c1-40)
    local out_json="$OUT_DIR/mega-O$OUTER-$ITER-$safe_name.translator.json"
    local transcript="$OUT_DIR/mega-O$OUTER-$ITER-$safe_name.transcript.jsonl"

    local prompt_template; prompt_template=$(cat "$ROOT/harness/family_agent_prompt.txt")
    local prompt="${prompt_template//__FUNC__/$func}"
    prompt="${prompt//__LOC__/$loc}"

    export CLAUDE_CONFIG_DIR="$HOME/.claude-personal"
    unset ANTHROPIC_API_KEY ANTHROPIC_AUTH_TOKEN
    export CLAUDE_CODE_MAX_OUTPUT_TOKENS="${CLAUDE_CODE_MAX_OUTPUT_TOKENS:-64000}"
    export CLAUDE_TARGET_RS_FILE="crates/lua-vm/src/state.rs"

    local timeout_cmd=""
    if command -v gtimeout >/dev/null 2>&1; then
        timeout_cmd="gtimeout --signal=KILL ${AGENT_WALL_TIMEOUT_S:-720}"
    elif command -v timeout >/dev/null 2>&1; then
        timeout_cmd="timeout --signal=KILL ${AGENT_WALL_TIMEOUT_S:-720}"
    fi
    $timeout_cmd claude -p \
        --append-system-prompt "$(cat PORTING.md)" \
        --allowedTools "Read,Write,Edit,Glob,Grep,Bash(cargo build*),Bash(cargo check*),Bash(grep *),Bash(rg *),Bash(cat *),Bash(head *),Bash(tail *),Bash(wc *),Bash(find *)" \
        --permission-mode dontAsk \
        --output-format stream-json \
        --include-partial-messages \
        --verbose \
        --max-budget-usd "$PER_AGENT_BUDGET" \
        "$prompt" \
        2>>"$OUT_DIR/mega-O$OUTER-$ITER-$safe_name.stderr" \
        | tee "$transcript" >/dev/null

    jq -s 'map(select(.type == "result")) | .[-1] // {}' "$transcript" > "$out_json" 2>/dev/null || echo '{}' > "$out_json"
    local cost; cost=$(jq -r '.total_cost_usd // 0' "$out_json")
    commit_agent_changes "impl" "$func" "$out_json"
    echo "$cost"
}

# Dispatch a DEBUG agent for a failing test program (real error, not a stub).
# Mode "inline": $prog is Lua source. Mode "file": $prog is a path to a .lua file
# that the agent should run via ./harness/run_official_test.sh.
dispatch_debug() {
    local prog="$1"
    local out_tail="$2"
    local mode="${3:-inline}"
    local model="${4:-$MODEL_DEFAULT}"
    local safe_name; safe_name=$(echo "$prog" | tr -c 'A-Za-z0-9_' '_' | cut -c1-30)
    local out_json="$OUT_DIR/mega-O$OUTER-D$ITER-$safe_name.translator.json"
    local transcript="$OUT_DIR/mega-O$OUTER-D$ITER-$safe_name.transcript.jsonl"

    local prompt_template; prompt_template=$(cat "$ROOT/harness/debug_agent_prompt.txt")
    local prompt="${prompt_template//__PROG__/$prog}"
    prompt="${prompt//__OUTPUT__/$out_tail}"
    prompt="${prompt//__MODE__/$mode}"

    export CLAUDE_CONFIG_DIR="$HOME/.claude-personal"
    unset ANTHROPIC_API_KEY ANTHROPIC_AUTH_TOKEN
    export CLAUDE_CODE_MAX_OUTPUT_TOKENS="${CLAUDE_CODE_MAX_OUTPUT_TOKENS:-64000}"

    local timeout_cmd=""
    if command -v gtimeout >/dev/null 2>&1; then
        timeout_cmd="gtimeout --signal=KILL ${AGENT_WALL_TIMEOUT_S:-1800}"
    elif command -v timeout >/dev/null 2>&1; then
        timeout_cmd="timeout --signal=KILL ${AGENT_WALL_TIMEOUT_S:-1800}"
    fi
    $timeout_cmd claude -p \
        --model "$model" \
        --append-system-prompt "$(cat PORTING.md)" \
        --allowedTools "Read,Write,Edit,Glob,Grep,Bash(cargo build*),Bash(cargo check*),Bash(grep *),Bash(rg *),Bash(cat *),Bash(head *),Bash(tail *),Bash(wc *),Bash(find *),Bash(target/debug/lua-rs *),Bash(./harness/run_official_test.sh *)" \
        --permission-mode dontAsk \
        --output-format stream-json \
        --include-partial-messages \
        --verbose \
        --max-budget-usd "$PER_AGENT_BUDGET" \
        "$prompt" \
        2>>"$OUT_DIR/mega-O$OUTER-D$ITER-$safe_name.stderr" \
        | tee "$transcript" >/dev/null

    jq -s 'map(select(.type == "result")) | .[-1] // {}' "$transcript" > "$out_json" 2>/dev/null || echo '{}' > "$out_json"
    local cost; cost=$(jq -r '.total_cost_usd // 0' "$out_json")
    commit_agent_changes "debug" "$prog" "$out_json"
    echo "$cost"
}

# Extract a coarse error signature from a test's output. Strips the
# [string "..."] source prefix and line numbers in assertion errors so
# different assertion sites (line 50 vs line 111) count as the same class
# — "still failing assert" — while truly different errors (parse error vs
# runtime panic) come back different.
error_signature() {
    local out_file="$1"
    if grep -qE "not yet implemented:" "$out_file"; then
        grep -E "not yet implemented:" "$out_file" | head -1 \
            | sed -E 's/^.*not yet implemented: //' \
            | head -c 80
    elif grep -qE "^thread '[^']+' .* panicked at " "$out_file"; then
        grep -oE "panicked at [^:]+:[0-9]+" "$out_file" | head -1
    elif grep -qE "^\[err\]" "$out_file"; then
        # Strip line numbers and [string "..."] wrapper from error to
        # normalize across rounds (assertion at line 42 vs line 99 is same
        # CLASS of failure — both "assertion failed").
        grep -E "^\[err\]" "$out_file" | head -1 \
            | sed -E 's/\[string "[^"]*"\]://g' \
            | sed -E 's/:[0-9]+:/:LINE:/g' \
            | head -c 100
    else
        echo "unknown"
    fi
}

emit "═════════════════════════════════════════════════════════════════"
emit "mega-loop start. ${#PROGS[@]} test programs."
emit "  MAX_OUTER=$MAX_OUTER  MAX_PER_OUTER=$MAX_PER_OUTER  PER_AGENT_BUDGET=\$$PER_AGENT_BUDGET  SESSION_BUDGET=\$$SESSION_BUDGET"
emit "  MODEL_DEFAULT=$MODEL_DEFAULT  MODEL_ESCALATE=$MODEL_ESCALATE"
emit "  SKIP_TESTS=$SKIP_TESTS"
emit "  HARD_TESTS=$HARD_TESTS"
emit "═════════════════════════════════════════════════════════════════"

OUTER=0
TOTAL_COST=0
PREV_QUEUE=""

while [ "$OUTER" -lt "$MAX_OUTER" ]; do
    OUTER=$((OUTER + 1))
    emit "─── outer round $OUTER (spent: \$$TOTAL_COST) ───"

    # SESSION_BUDGET guard — abort cleanly before dispatching more work.
    budget_exceeded=$(awk -v t="$TOTAL_COST" -v b="$SESSION_BUDGET" 'BEGIN { print (t >= b) ? 1 : 0 }')
    if [ "$budget_exceeded" = "1" ]; then
        emit "  SESSION_BUDGET (\$$SESSION_BUDGET) reached. Stopping cleanly."
        break
    fi

    if ! cargo build -q -p lua-cli 2>"$OUT_DIR/build-O$OUTER.err"; then
        emit "  BUILD BROKEN at top of round — abort. See $OUT_DIR/build-O$OUTER.err"
        head -30 "$OUT_DIR/build-O$OUTER.err" | tee -a "$LOG"
        break
    fi

    # ONE scan per outer round; reused for pass-count + stub-extraction.
    SCAN_DIR="$OUT_DIR/scan-$OUTER"
    run_all_programs "$SCAN_DIR"

    pass=$(count_passing "$SCAN_DIR")
    emit "  scan: $pass/${#PROGS[@]} programs pass cleanly"

    # Regression-abort: pass count should never decrease between rounds.
    if [ "$PREV_PASS_COUNT" -gt -1 ] && [ "$pass" -lt "$PREV_PASS_COUNT" ]; then
        emit "  REGRESSION: pass count went $PREV_PASS_COUNT → $pass. Aborting."
        break
    fi
    PREV_PASS_COUNT=$pass

    if [ "$pass" = "${#PROGS[@]}" ]; then
        emit "★ all ${#PROGS[@]} programs pass. Stopping."
        break
    fi

    queue=$(surface_scan "$SCAN_DIR")
    n=$(printf "%s\n" "$queue" | grep -c . || true)
    emit "  surface scan returned $n unique stub blockers"
    if [ "$n" = "0" ]; then
        emit "  no stub blockers — falling back to DEBUG agents on failing programs"
        debug_iter=0
        idx=0
        debug_pids=()
        debug_costs_file="$OUT_DIR/debug-costs-O$OUTER.txt"
        : > "$debug_costs_file"
        for prog in "${PROGS[@]}"; do
            idx=$((idx + 1))
            out_file="$SCAN_DIR/prog-$idx.out"
            exp="${EXPECTED[$((idx-1))]}"
            if [ ! -f "$out_file" ]; then continue; fi
            failing=1
            if grep -qE "^\[ok\] execution completed" "$out_file" \
                && ! grep -qE "not yet implemented:|panicked at|^\[err\]" "$out_file"; then
                if [ -n "$exp" ]; then
                    actual=$(awk '
                        /^\[4\/4\] Executing chunk/ { capture=1; next }
                        /^\[ok\] execution completed/ { capture=0 }
                        capture { print }
                    ' "$out_file")
                    expected_decoded=$(printf "%b" "$exp")
                    if [ "$actual" = "$expected_decoded" ]; then failing=0; fi
                else
                    failing=0
                fi
            fi
            if [ "$failing" = "0" ]; then continue; fi
            prog_key="${LABELS[$((idx-1))]:-inline-$idx}"

            # SKIP_TESTS hard-skip: known Phase-E / heavy-D-2 work that an
            # agent has no realistic path to fix in one round.
            skip_this=0
            for skip_pat in $(echo "$SKIP_TESTS" | tr ',' ' '); do
                if [[ "$prog_key" == *"$skip_pat"* ]]; then
                    skip_this=1
                    break
                fi
            done
            if [ "$skip_this" = "1" ]; then
                emit "  skip prog (in SKIP_TESTS): $prog_key"
                continue
            fi

            current_sig=$(error_signature "$out_file")
            prev_sig=""
            sig_idx=-1
            for i in "${!LAST_SIG_KEY[@]}"; do
                if [ "${LAST_SIG_KEY[$i]}" = "$prog_key" ]; then
                    prev_sig="${LAST_SIG_VAL[$i]}"
                    sig_idx=$i
                    break
                fi
            done

            stuck_count=0
            stuck_idx=-1
            for i in "${!STUCK_COUNT_KEY[@]}"; do
                if [ "${STUCK_COUNT_KEY[$i]}" = "$prog_key" ]; then
                    stuck_count="${STUCK_COUNT_VAL[$i]}"
                    stuck_idx=$i
                    break
                fi
            done

            if [ -n "$prev_sig" ] && [ "$prev_sig" = "$current_sig" ]; then
                stuck_count=$((stuck_count + 1))
            else
                stuck_count=0
            fi
            if [ "$stuck_idx" -ge 0 ]; then
                STUCK_COUNT_VAL[$stuck_idx]="$stuck_count"
            else
                STUCK_COUNT_KEY+=("$prog_key")
                STUCK_COUNT_VAL+=("$stuck_count")
            fi

            if [ "$stuck_count" -ge 2 ]; then
                emit "  skip stuck prog (no progress 2 rounds, sig=$current_sig): $prog_key"
                continue
            fi

            agent_model="$MODEL_DEFAULT"
            for hard_pat in $(echo "$HARD_TESTS" | tr ',' ' '); do
                if [[ "$prog_key" == *"$hard_pat"* ]]; then
                    agent_model="$MODEL_ESCALATE"
                    emit "  [hard-test] $prog_key → $agent_model (skip Sonnet)"
                    break
                fi
            done
            if [ "$stuck_count" = "1" ] && [ "$agent_model" = "$MODEL_DEFAULT" ]; then
                agent_model="$MODEL_ESCALATE"
                emit "  [stuck-escalate] $prog_key → $agent_model"
            fi

            if [ "$sig_idx" -ge 0 ]; then
                LAST_SIG_VAL[$sig_idx]="$current_sig"
            else
                LAST_SIG_KEY+=("$prog_key")
                LAST_SIG_VAL+=("$current_sig")
            fi
            debug_iter=$((debug_iter + 1))
            if [ "$debug_iter" -gt "$MAX_PER_OUTER" ]; then break; fi
            ITER=$debug_iter
            out_tail=$(tail -25 "$out_file")
            if [ -n "$exp" ]; then
                expected_decoded=$(printf "%b" "$exp")
                out_tail="EXPECTED OUTPUT (between [4/4] and [ok] lines):
$expected_decoded

ACTUAL OUTPUT (tail):
$out_tail"
            fi
            label="${LABELS[$((idx-1))]:-}"
            while [ "${#debug_pids[@]}" -ge "$PARALLEL_AGENTS" ]; do
                new_pids=()
                for p in "${debug_pids[@]+"${debug_pids[@]}"}"; do
                    if kill -0 "$p" 2>/dev/null; then new_pids+=("$p"); fi
                done
                debug_pids=("${new_pids[@]+"${new_pids[@]}"}")
                [ "${#debug_pids[@]}" -ge "$PARALLEL_AGENTS" ] && sleep 2
            done
            if [ -n "$label" ]; then
                emit "  [O$OUTER.D$debug_iter] DEBUG dispatch on file: $label [model=$agent_model] [parallel]"
                (
                    cost=$(dispatch_debug "$label" "$out_tail" "file" "$agent_model")
                    echo "$debug_iter $cost $label [$agent_model]" >> "$debug_costs_file"
                ) &
            else
                emit "  [O$OUTER.D$debug_iter] DEBUG dispatch on prog: $prog [model=$agent_model] [parallel]"
                (
                    cost=$(dispatch_debug "$prog" "$out_tail" "inline" "$agent_model")
                    echo "$debug_iter $cost inline [$agent_model]" >> "$debug_costs_file"
                ) &
            fi
            debug_pids+=($!)
        done
        for p in "${debug_pids[@]+"${debug_pids[@]}"}"; do wait "$p" 2>/dev/null || true; done
        while read -r iter c rest; do
            TOTAL_COST=$(awk -v a="$TOTAL_COST" -v b="$c" 'BEGIN { printf "%.4f", a + b }')
            emit "  [O$OUTER.D$iter] DEBUG done. iter cost=\$$c  total=\$$TOTAL_COST"
        done < "$debug_costs_file"
        if [ "$debug_iter" = "0" ]; then
            emit "  no failing programs and no stubs — done."
            break
        fi
        continue
    fi
    printf "%s\n" "$queue" > "$QUEUE"

    # Stuck-detect: if the queue is byte-identical to the previous round's, we're stuck.
    if [ "$queue" = "$PREV_QUEUE" ]; then
        emit "  STUCK: queue identical to previous round. Bailing."
        break
    fi
    PREV_QUEUE="$queue"

    # Group the queue by file. Keep only the FIRST stub per file (the
    # family-aware agent will sweep siblings inside its file). This lets
    # us run agents on disjoint files in parallel without conflicts.
    declare -a seen_files=()
    declare -a batch_funcs=()
    declare -a batch_locs=()
    while IFS=$'\t' read -r func loc; do
        [ -z "$func" ] && continue
        [ "${#batch_funcs[@]}" -ge "$MAX_PER_OUTER" ] && break
        file="${loc%%:*}"
        already=0
        for sf in "${seen_files[@]+"${seen_files[@]}"}"; do
            if [ "$sf" = "$file" ]; then already=1; break; fi
        done
        [ "$already" = "1" ] && continue
        seen_files+=("$file")
        batch_funcs+=("$func")
        batch_locs+=("$loc")
    done <<< "$queue"

    emit "  grouped into ${#batch_funcs[@]} disjoint-file agents (parallel cap $PARALLEL_AGENTS)"
    ITER=0
    agent_pids=()
    agent_costs_file="$OUT_DIR/agent-costs-O$OUTER.txt"
    : > "$agent_costs_file"
    while [ "$ITER" -lt "${#batch_funcs[@]}" ]; do
        # Limit concurrent agents.
        while [ "${#agent_pids[@]}" -ge "$PARALLEL_AGENTS" ]; do
            new_pids=()
            for p in "${agent_pids[@]+"${agent_pids[@]}"}"; do
                if kill -0 "$p" 2>/dev/null; then new_pids+=("$p"); fi
            done
            agent_pids=("${new_pids[@]+"${new_pids[@]}"}")
            [ "${#agent_pids[@]}" -ge "$PARALLEL_AGENTS" ] && sleep 2
        done
        ITER=$((ITER + 1))
        func="${batch_funcs[$((ITER-1))]}"
        loc="${batch_locs[$((ITER-1))]}"
        emit "  [O$OUTER.$ITER] dispatch on '$func' @ $loc (budget \$$PER_AGENT_BUDGET) [parallel]"
        (
            cost=$(dispatch_one "$func" "$loc")
            echo "$ITER $cost $func" >> "$agent_costs_file"
        ) &
        agent_pids+=($!)
    done
    for p in "${agent_pids[@]+"${agent_pids[@]}"}"; do wait "$p" 2>/dev/null || true; done

    # Roll up costs from this round.
    while read -r iter c rest; do
        TOTAL_COST=$(awk -v a="$TOTAL_COST" -v b="$c" 'BEGIN { printf "%.4f", a + b }')
        emit "  [O$OUTER.$iter] agent done. iter cost=\$$c  total=\$$TOTAL_COST"
    done < "$agent_costs_file"
done

emit "═════════════════════════════════════════════════════════════════"
emit "mega-loop end. Total cost: \$$TOTAL_COST  Outer rounds: $OUTER"
emit "═════════════════════════════════════════════════════════════════"
