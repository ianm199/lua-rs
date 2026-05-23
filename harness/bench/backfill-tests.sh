#!/usr/bin/env bash
# harness/bench/backfill-tests.sh — backfill official-test-suite pass counts
# across historical commits.
#
# For each commit listed in the kind=bench/target=rust-vs-reference rows of
# harness/evidence/ledger.jsonl, check it out in an isolated git worktree,
# build the debug binary, run the official test suite, and append one
# kind=tests row to the MAIN repo's ledger.
#
# Commits with bench rows are the ones we know compile (compare.sh ran on
# them successfully). This avoids touching agent-intermediate auto-commits
# that may not build.
#
# Usage:
#   bash harness/bench/backfill-tests.sh
#   bash harness/bench/backfill-tests.sh --skip-existing   # skip commits that already have a tests row

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

LEDGER="$ROOT/harness/evidence/ledger.jsonl"
WORKTREE="/tmp/lua-rs-backfill-$$"
SKIP_EXISTING=0

while [ $# -gt 0 ]; do
    case "$1" in
        --skip-existing) SKIP_EXISTING=1; shift ;;
        *) echo "unknown flag: $1" >&2; exit 1 ;;
    esac
done

OS_NAME="$(uname -sr)"
ARCH="$(uname -m)"
CPU="$(sysctl -n machdep.cpu.brand_string 2>/dev/null || grep -m1 'model name' /proc/cpuinfo 2>/dev/null | cut -d: -f2- | sed 's/^ *//' || echo 'unknown')"

cleanup() {
    if [ -d "$WORKTREE" ]; then
        git worktree remove --force "$WORKTREE" 2>/dev/null || rm -rf "$WORKTREE"
    fi
}
trap cleanup EXIT

git worktree add --detach "$WORKTREE" HEAD >/dev/null
echo "[backfill] worktree at $WORKTREE"

# reference/lua-c (the upstream Lua source + testes/ dir) is local-only,
# not tracked in git. The worktree won't have it. Symlink the main repo's
# reference/ subtrees into the worktree so run_official_all.sh can find
# reference/lua-c/testes/*.lua.
mkdir -p "$WORKTREE/reference"
for ref_subdir in lua-c lua-5.4.7 lua-5.4.7-tests; do
    if [ -d "$ROOT/reference/$ref_subdir" ] && [ ! -e "$WORKTREE/reference/$ref_subdir" ]; then
        ln -s "$ROOT/reference/$ref_subdir" "$WORKTREE/reference/$ref_subdir"
    fi
done

# Collect commits with bench rows that don't yet have a tests row.
commits=$(python3 - "$LEDGER" "$SKIP_EXISTING" <<'PY'
import json, sys
ledger, skip = sys.argv[1:]
skip = int(skip)
bench_commits = []
tested = set()
for line in open(ledger):
    try: row = json.loads(line)
    except: continue
    if row.get("kind") == "bench" and row.get("target") == "rust-vs-reference":
        if row["commit"] not in bench_commits:
            bench_commits.append(row["commit"])
    elif row.get("kind") == "tests" and row.get("target") == "official-suite":
        tested.add(row["commit"])
out = [c for c in bench_commits if not skip or c not in tested]
print("\n".join(out))
PY
)

if [ -z "$commits" ]; then
    echo "[backfill] nothing to do."
    exit 0
fi

total=$(echo "$commits" | wc -l | tr -d ' ')
i=0
ok=0
build_fail=0
parse_fail=0

for commit in $commits; do
    i=$((i+1))
    echo ""
    echo "──────────────────────────────────────────────────────────"
    echo "[$i/$total] $commit"
    echo "──────────────────────────────────────────────────────────"

    # Use the actual commit's iso-strict author date as the ledger ts, so
    # the chart sorts by code-age not by backfill-run-time.
    ts_raw=$(git log -1 --format=%aI "$commit" 2>/dev/null)
    if [ -z "$ts_raw" ]; then
        echo "  [skip] commit not found locally"
        continue
    fi
    ts=$(python3 -c "from datetime import datetime; print(datetime.fromisoformat('$ts_raw'.replace('Z','+00:00')).astimezone().strftime('%Y%m%dT%H%M%SZ'))" 2>/dev/null)
    [ -z "$ts" ] && ts=$(date -u +%Y%m%dT%H%M%SZ)

    co_err=$(cd "$WORKTREE" && git checkout --detach --quiet "$commit" 2>&1) || { echo "  [skip] checkout failed: $co_err"; continue; }

    echo "  [build] cargo build -p lua-cli (debug)..."
    start=$(date +%s)
    if ! (cd "$WORKTREE" && cargo build -p lua-cli 2>&1 | tail -3); then
        echo "  [skip] build failed"
        build_fail=$((build_fail+1))
        continue
    fi
    build_s=$(( $(date +%s) - start ))
    echo "  [build] ok (${build_s}s)"

    echo "  [tests] running suite via MAIN runner with worktree binary..."
    start=$(date +%s)
    tmp=$(mktemp)
    # Invoke the MAIN repo's harness runner (which has the dofile-wrapper
    # invocation that works on any binary version) and point it at the
    # worktree's freshly-built lua-rs. testes/ comes from MAIN (it's
    # local-only / not tracked, so the worktree wouldn't have it either).
    LUA_RS_BIN="$WORKTREE/target/debug/lua-rs" bash "$ROOT/harness/run_official_all.sh" >"$tmp" 2>&1 || true
    elapsed=$(( $(date +%s) - start ))

    total_t=$(awk '/^[[:space:]]*Total:/ {print $2; exit}' "$tmp")
    pass=$(awk '/^[[:space:]]*Pass:/ {print $2; exit}' "$tmp")
    fail=$(awk '/^[[:space:]]*Fail:/ {print $2; exit}' "$tmp")
    timeout_=$(awk '/^[[:space:]]*Timeout:/ {print $2; exit}' "$tmp")
    rm -f "$tmp"

    if [ -z "$total_t" ] || [ -z "$pass" ]; then
        echo "  [skip] parse failed"
        parse_fail=$((parse_fail+1))
        continue
    fi

    echo "  [tests] pass=$pass/$total_t fail=$fail timeout=$timeout_ runtime=${elapsed}s"

    python3 - "$LEDGER" "$ts" "$commit" "$OS_NAME" "$ARCH" "$CPU" "$pass" "$total_t" "$fail" "$timeout_" "$elapsed" <<'PY'
import json, sys
ledger, ts, commit, os_name, arch, cpu, pass_, total, fail, timeout_, elapsed = sys.argv[1:]
row = {
    "kind": "tests", "target": "official-suite", "metric": "pass_count",
    "value": int(pass_), "total": int(total),
    "fail": int(fail), "timeout": int(timeout_),
    "runtime_s": int(elapsed),
    "commit": commit, "ts": ts,
    "os": os_name, "arch": arch, "cpu": cpu,
    "runner": "run_official_all", "schema_version": 1,
    "evidence": "harness/impl/official/run_all.tsv",
    "backfilled": True,
}
with open(ledger, "a", encoding="utf-8") as f:
    f.write(json.dumps(row, sort_keys=True) + "\n")
PY
    ok=$((ok+1))
done

echo ""
echo "──────────────────────────────────────────────────────────"
echo "[backfill] done. ok=$ok build_fail=$build_fail parse_fail=$parse_fail"
echo "──────────────────────────────────────────────────────────"
