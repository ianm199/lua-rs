#!/usr/bin/env bash
# Thin wrapper: run the Lua oracle for one phase and print its verdict.
# Replaces the Verifier subagent; see port-harness/tools/verify-report.sh.
#
#   ./harness/verify-report.sh <phase>
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
export PORT_PROJECT_ROOT="$ROOT"
export PORT_ORACLE_CMD="harness/oracle/run-phase.sh"
export PORT_RESULTS_JSON="harness/oracle/test-results.json"
export PORT_EVIDENCE_DIR="harness/oracle/results"
exec "$ROOT/../port-harness/tools/verify-report.sh" "$@"
