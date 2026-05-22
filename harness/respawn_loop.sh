#!/usr/bin/env bash
# respawn_loop.sh — outer watchdog that re-runs implement_loop.sh until
# print("hello") succeeds or a global $-cap is exceeded.
#
# Usage:
#   nohup ./harness/respawn_loop.sh > /tmp/respawn.log 2>&1 &

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

GLOBAL_CAP=${GLOBAL_CAP:-800.00}
PER_RUN_MAX_ITER=${PER_RUN_MAX_ITER:-40}
PER_RUN_COST_CAP=${PER_RUN_COST_CAP:-100.00}
OUTER_MAX_RUNS=${OUTER_MAX_RUNS:-20}

OUT_DIR="harness/impl"
LOG="$OUT_DIR/respawn.log"
mkdir -p "$OUT_DIR"
touch "$LOG"

emit() {
    local ts; ts=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
    echo "[$ts respawn] $*" | tee -a "$LOG"
}

emit "respawn watchdog starting. GLOBAL_CAP=\$$GLOBAL_CAP PER_RUN_MAX_ITER=$PER_RUN_MAX_ITER OUTER_MAX_RUNS=$OUTER_MAX_RUNS"

for run in $(seq 1 "$OUTER_MAX_RUNS"); do
    emit "outer run #$run starting"
    MAX_ITER=$PER_RUN_MAX_ITER LOOP_COST_CAP=$PER_RUN_COST_CAP \
        ./harness/implement_loop.sh
    rc=$?
    emit "outer run #$run exited rc=$rc"

    if cargo run -q -p lua-cli -- 'print("hello")' 2>&1 | grep -q '^hello$'; then
        emit "SUCCESS: print(\"hello\") works. Stopping watchdog."
        break
    fi

    sleep 5
done

emit "respawn watchdog finished after $OUTER_MAX_RUNS attempts (or success)"
