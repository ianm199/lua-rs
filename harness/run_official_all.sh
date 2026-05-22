#!/usr/bin/env bash
# run_official_all.sh
#
# Runs every official Lua 5.4 test file in reference/lua-c/testes/ (excluding
# scratch files prefixed with `_`) through our lua-rs binary and reports
# per-test PASS / FAIL / TIMEOUT, plus a final summary line.
#
# Each test is wrapped in the same preamble as run_official_test.sh.
# Default timeout is 60 seconds; override with TEST_TIMEOUT_S=120.
#
# Output:
#   harness/impl/official/run_all.tsv  — TSV of test_name<TAB>status<TAB>fail_line
#   stdout                              — live progress + summary
#
# Usage:
#   ./harness/run_official_all.sh
#   TEST_TIMEOUT_S=120 ./harness/run_official_all.sh

set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

TEST_TIMEOUT_S=${TEST_TIMEOUT_S:-60}
BIN="$ROOT/target/debug/lua-rs"
TESTES_DIR="$ROOT/reference/lua-c/testes"
OUT_DIR="$ROOT/harness/impl/official"
TSV="$OUT_DIR/run_all.tsv"
mkdir -p "$OUT_DIR"
: > "$TSV"

if [ ! -x "$BIN" ]; then
    echo "[err] lua-rs binary not built at $BIN" >&2
    exit 2
fi

PREAMBLE='_soft = true
_port = true
_nomsg = true
_U = false
arg = arg or {}
_G = _G or _ENV
if _VERSION == nil then _VERSION = "Lua 5.4" end
'

export LUA_PATH="$TESTES_DIR/?.lua;$TESTES_DIR/?/init.lua;./?.lua;./?/init.lua"
export LUA_RS_VERBOSE=1

pass=0; fail=0; timeout=0
declare -a FAILED

for test_file in "$TESTES_DIR"/*.lua; do
    base=$(basename "$test_file" .lua)
    case "$base" in _*) continue ;; esac

    combined="$OUT_DIR/$base.combined.lua"
    outfile="$OUT_DIR/$base.out"
    { printf '%s\n' "$PREAMBLE"; cat "$test_file"; } > "$combined"
    src=$(cat "$combined")

    if command -v gtimeout >/dev/null 2>&1; then
        gtimeout --signal=KILL "$TEST_TIMEOUT_S" "$BIN" "$src" > "$outfile" 2>&1
        rc=$?
    else
        ( "$BIN" "$src" > "$outfile" 2>&1 ) &
        pid=$!
        ( sleep "$TEST_TIMEOUT_S" && kill -9 "$pid" 2>/dev/null ) &
        wpid=$!
        wait "$pid" 2>/dev/null
        rc=$?
        kill "$wpid" 2>/dev/null
        wait "$wpid" 2>/dev/null || true
    fi

    if [ "$rc" = "137" ] || [ "$rc" = "124" ]; then
        timeout=$((timeout+1))
        FAILED+=("$base")
        printf "  %-20s TIMEOUT (%ds)\n" "$base" "$TEST_TIMEOUT_S"
        printf '%s\tTIMEOUT\t-\n' "$base" >> "$TSV"
        continue
    fi

    if grep -q "^\[ok\] execution completed" "$outfile" \
        && ! grep -qE "not yet implemented:|panicked at|^\[err\]|pcall_k failed" "$outfile"; then
        pass=$((pass+1))
        printf "  %-20s PASS\n" "$base"
        printf '%s\tPASS\t-\n' "$base" >> "$TSV"
    else
        fail=$((fail+1))
        FAILED+=("$base")
        msg=$(grep -E "pcall_k failed|panicked at|not yet implemented|^\[err\]|stack overflow" "$outfile" | head -1 | cut -c1-100)
        printf "  %-20s FAIL  %s\n" "$base" "$msg"
        printf '%s\tFAIL\t%s\n' "$base" "$msg" >> "$TSV"
    fi
done

total=$((pass+fail+timeout))
echo ""
echo "=========================================="
printf "  Total:   %3d official tests\n" "$total"
printf "  Pass:    %3d  (%3d%%)\n" "$pass" "$(( pass*100/total ))"
printf "  Fail:    %3d\n" "$fail"
printf "  Timeout: %3d\n" "$timeout"
echo "  TSV:     $TSV"
echo "=========================================="
