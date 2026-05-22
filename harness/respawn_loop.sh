#!/usr/bin/env bash
# respawn_loop.sh — outer watchdog that re-runs implement_loop.sh until
# print("hello") succeeds or a global $-cap is exceeded.
#
# Usage:
#   nohup ./harness/respawn_loop.sh > /tmp/respawn.log 2>&1 &

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

GLOBAL_CAP=${GLOBAL_CAP:-2000.00}
PER_RUN_MAX_ITER=${PER_RUN_MAX_ITER:-80}
PER_RUN_COST_CAP=${PER_RUN_COST_CAP:-500.00}
OUTER_MAX_RUNS=${OUTER_MAX_RUNS:-50}

OUT_DIR="harness/impl"
LOG="$OUT_DIR/respawn.log"
mkdir -p "$OUT_DIR"
touch "$LOG"

emit() {
    local ts; ts=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
    echo "[$ts respawn] $*" | tee -a "$LOG"
}

emit "respawn watchdog starting. GLOBAL_CAP=\$$GLOBAL_CAP PER_RUN_MAX_ITER=$PER_RUN_MAX_ITER OUTER_MAX_RUNS=$OUTER_MAX_RUNS"

TEST_PROG=${TEST_PROG:-'print("hello"); print(1+2); print("end")'}
SUCCESS_MARKER=${SUCCESS_MARKER:-'^end$'}

for run in $(seq 1 "$OUTER_MAX_RUNS"); do
    emit "outer run #$run starting (test: $TEST_PROG)"
    MAX_ITER=$PER_RUN_MAX_ITER LOOP_COST_CAP=$PER_RUN_COST_CAP \
        TEST_PROG="$TEST_PROG" \
        ./harness/implement_loop.sh
    rc=$?
    emit "outer run #$run exited rc=$rc"

    cargo build -q -p lua-cli >/dev/null 2>&1 || true
    BIN="target/debug/lua-rs"
    if [ ! -x "$BIN" ]; then
        TEST_CMD="cargo run -q -p lua-cli -- $TEST_PROG"
    elif command -v gtimeout >/dev/null 2>&1; then
        TEST_CMD="gtimeout --signal=KILL 20 $BIN"
    elif command -v timeout >/dev/null 2>&1; then
        TEST_CMD="timeout --signal=KILL 20 $BIN"
    else
        TEST_CMD="$BIN"
    fi
    if $TEST_CMD "$TEST_PROG" 2>&1 | grep -qE "$SUCCESS_MARKER"; then
        emit "SUCCESS: test program produced expected output. Stopping watchdog."
        break
    fi

    sleep 5
done

emit "respawn watchdog finished after $OUTER_MAX_RUNS attempts (or success)"
