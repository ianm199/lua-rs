#!/usr/bin/env bash
# Run the test set for a given phase. Writes test-results.json.
# Usage: ./run-phase.sh <phase>      (A, B, C, D, E, or F)
#
# Per-phase test sets are frozen in this script (see PORT_STRATEGY.md §4).
# Adding a test to a phase requires editing this file in a commit.

set -uo pipefail

if [ "$#" -ne 1 ]; then
    echo "usage: $0 <phase>" >&2
    exit 2
fi

PHASE="$1"
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
RESULTS_JSON="$ROOT/harness/oracle/test-results.json"
ORACLE="$ROOT/harness/oracle"

# Frozen phase test sets — DO NOT EDIT in-session without a commit.
case "$PHASE" in
    A)
        MODE="bytecode"
        TESTS=( $(ls "$ORACLE/corpus" 2>/dev/null) )
        ;;
    B)
        MODE="suite"
        TESTS=(constructs.lua locals.lua closure.lua vararg.lua goto.lua literals.lua code.lua calls.lua)
        ;;
    C)
        MODE="suite"
        TESTS=(strings.lua pm.lua math.lua sort.lua bitwise.lua tpack.lua nextvar.lua utf8.lua)
        ;;
    D)
        MODE="suite"
        TESTS=(gc.lua gengc.lua events.lua)
        ;;
    E)
        MODE="suite"
        TESTS=(coroutine.lua cstack.lua files.lua db.lua)
        ;;
    F)
        MODE="suite"
        TESTS=(all.lua)
        ;;
    *)
        echo "unknown phase: $PHASE (expected A|B|C|D|E|F)" >&2
        exit 2
        ;;
esac

PASS=0
FAIL=0
FAILED_NAMES=()

for t in "${TESTS[@]}"; do
    case "$MODE" in
        bytecode)
            if "$ORACLE/diff-bytecode.sh" "$ORACLE/corpus/$t" >/dev/null 2>&1; then
                PASS=$((PASS+1))
            else
                FAIL=$((FAIL+1))
                FAILED_NAMES+=("$t")
            fi
            ;;
        suite)
            if "$ORACLE/run-test-file.sh" "$t" >/dev/null 2>&1; then
                PASS=$((PASS+1))
            else
                FAIL=$((FAIL+1))
                FAILED_NAMES+=("$t")
            fi
            ;;
    esac
done

# Write results JSON. The verify-gate.sh hook requires the corresponding
# evidence file (in results/) to have been Read before this is allowed to
# be set to PASS via a Write/Edit operation.
PASS_FLAG="false"
if [ "$FAIL" = "0" ] && [ "$PASS" -gt "0" ]; then
    PASS_FLAG="true"
fi

FAILED_JSON=$(printf '"%s",' "${FAILED_NAMES[@]+"${FAILED_NAMES[@]}"}" | sed 's/,$//')

cat > "$RESULTS_JSON" <<EOF
{
  "phase": "$PHASE",
  "mode": "$MODE",
  "total": $((PASS+FAIL)),
  "passed": $PASS,
  "failed": $FAIL,
  "failed_tests": [${FAILED_JSON:-}],
  "passes": $PASS_FLAG
}
EOF

echo "Phase $PHASE: $PASS passed, $FAIL failed."
[ "$FAIL" = "0" ]
