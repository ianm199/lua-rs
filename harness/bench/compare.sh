#!/usr/bin/env bash
# harness/bench/compare.sh — side-by-side throughput benchmark between
# lua-rs (Rust impl) and the pinned upstream Lua 5.4.7 reference binary.
#
# For each workload in harness/bench/workloads/, both binaries run N times
# (default 5). We record min wall-clock + max RSS for each, then emit a TSV
# with the ratio (lua-rs / reference). Min wall-clock is the standard
# "best of N" interpreter benchmark convention (filters out scheduling jitter).
#
# Usage:
#   bash harness/bench/compare.sh                       # all workloads, N=5
#   bash harness/bench/compare.sh --runs 3              # fewer repetitions
#   bash harness/bench/compare.sh --workloads fib,table # subset
#
# Output:
#   harness/bench/results/<UTC-timestamp>-<short-sha>-compare.tsv
#   harness/bench/results/<UTC-timestamp>-<short-sha>-compare.json
#
# Reproducibility: hardware + OS + commit fingerprint in the TSV header so
# results from different machines do not get silently merged.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

REF_BIN="$ROOT/reference/lua-5.4.7/src/lua"
RS_BIN="$ROOT/target/release/lua-rs"
WORKLOAD_DIR="$ROOT/harness/bench/workloads"
OUT_DIR="$ROOT/harness/bench/results"
mkdir -p "$OUT_DIR"

RUNS=5
WORKLOAD_FILTER=""

while [ $# -gt 0 ]; do
    case "$1" in
        --runs)      RUNS="$2";              shift 2 ;;
        --workloads) WORKLOAD_FILTER="$2";   shift 2 ;;
        -h|--help)
            sed -n '2,/^set -euo/p' "${BASH_SOURCE[0]}" | sed 's/^# //; s/^#//'
            exit 0 ;;
        *) echo "unknown flag: $1" >&2; exit 1 ;;
    esac
done

[ -x "$REF_BIN" ] || { echo "[err] reference binary missing: $REF_BIN — run 'make macosx -C reference/lua-5.4.7' first" >&2; exit 2; }
[ -x "$RS_BIN" ]  || { echo "[err] lua-rs release binary missing: $RS_BIN — run 'cargo build --release -p lua-cli'" >&2; exit 2; }

TS=$(date -u +%Y%m%dT%H%M%SZ)
COMMIT=$(git rev-parse --short HEAD 2>/dev/null || echo "unknown")
TSV="$OUT_DIR/${TS}-${COMMIT}-compare.tsv"
JSON="$OUT_DIR/${TS}-${COMMIT}-compare.json"

OS_NAME="$(uname -sr)"
ARCH="$(uname -m)"
CPU="$(sysctl -n machdep.cpu.brand_string 2>/dev/null || grep -m1 'model name' /proc/cpuinfo 2>/dev/null | cut -d: -f2- | sed 's/^ *//' || echo 'unknown')"

# /usr/bin/time -lp on macOS prints:
#   real         0.12
#   user         0.10
#   sys          0.01
#        16777216  maximum resident set size
# We extract real (seconds) and maximum-RSS (bytes).
#
# On Linux, /usr/bin/time -v gives different output. We fall back to a Python
# helper there. macOS is the primary target.

measure_one() {
    local bin="$1"
    local workload="$2"
    local tmp
    tmp=$(mktemp)
    /usr/bin/time -lp "$bin" "$workload" >/dev/null 2>"$tmp"
    local real rss
    real=$(awk '$1=="real" {print $2}' "$tmp" | head -1)
    rss=$(awk '/maximum resident set size/ {print $1}' "$tmp" | head -1)
    rm -f "$tmp"
    if [ -z "$real" ] || [ -z "$rss" ]; then
        echo "[err] failed to parse /usr/bin/time output for $bin $workload" >&2
        return 1
    fi
    printf "%s %s\n" "$real" "$rss"
}

best_of_n() {
    local bin="$1"
    local workload="$2"
    local n="$3"
    local best_real="" best_rss=""
    for _ in $(seq 1 "$n"); do
        local pair real rss
        pair=$(measure_one "$bin" "$workload") || return 1
        real=$(echo "$pair" | awk '{print $1}')
        rss=$(echo "$pair" | awk '{print $2}')
        if [ -z "$best_real" ] || awk -v a="$real" -v b="$best_real" 'BEGIN{exit !(a < b)}'; then
            best_real="$real"
        fi
        if [ -z "$best_rss" ] || awk -v a="$rss" -v b="$best_rss" 'BEGIN{exit !(a > b)}'; then
            best_rss="$rss"
        fi
    done
    printf "%s %s\n" "$best_real" "$best_rss"
}

{
    printf '# lua-rs bench compare\n'
    printf '# timestamp_utc: %s\n' "$TS"
    printf '# commit:        %s\n' "$COMMIT"
    printf '# os:            %s\n' "$OS_NAME"
    printf '# arch:          %s\n' "$ARCH"
    printf '# cpu:           %s\n' "$CPU"
    printf '# runs:          %d (reporting best wall-clock, max RSS)\n' "$RUNS"
    printf '# reference:     %s (Lua 5.4.7)\n' "$REF_BIN"
    printf '# lua-rs:        %s\n' "$RS_BIN"
    printf '#\n'
    printf 'workload\tref_wall_s\trs_wall_s\twall_ratio\tref_rss_kb\trs_rss_kb\trss_ratio\n'
} > "$TSV"

JSON_ROWS=""
TOTAL_REF=0
TOTAL_RS=0

for wpath in "$WORKLOAD_DIR"/*.lua; do
    wname=$(basename "$wpath" .lua)
    if [ -n "$WORKLOAD_FILTER" ]; then
        echo ",$WORKLOAD_FILTER," | grep -q ",$wname," || continue
    fi

    echo "==> $wname" >&2
    pair_ref=$(best_of_n "$REF_BIN" "$wpath" "$RUNS")
    pair_rs=$(best_of_n "$RS_BIN" "$wpath" "$RUNS")
    ref_wall=$(echo "$pair_ref" | awk '{print $1}')
    ref_rss=$(echo "$pair_ref" | awk '{print $2}')
    rs_wall=$(echo "$pair_rs" | awk '{print $1}')
    rs_rss=$(echo "$pair_rs" | awk '{print $2}')

    wall_ratio=$(awk -v a="$rs_wall" -v b="$ref_wall" 'BEGIN{if (b>0) printf "%.2f", a/b; else print "NaN"}')
    rss_ratio=$(awk  -v a="$rs_rss"  -v b="$ref_rss"  'BEGIN{if (b>0) printf "%.2f", a/b; else print "NaN"}')

    ref_rss_kb=$(awk -v b="$ref_rss" 'BEGIN{printf "%.0f", b/1024}')
    rs_rss_kb=$(awk  -v b="$rs_rss"  'BEGIN{printf "%.0f", b/1024}')

    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
        "$wname" "$ref_wall" "$rs_wall" "$wall_ratio" "$ref_rss_kb" "$rs_rss_kb" "$rss_ratio" >> "$TSV"

    if [ -n "$JSON_ROWS" ]; then JSON_ROWS="$JSON_ROWS,"; fi
    JSON_ROWS="$JSON_ROWS{\"workload\":\"$wname\",\"ref_wall_s\":$ref_wall,\"rs_wall_s\":$rs_wall,\"wall_ratio\":$wall_ratio,\"ref_rss_kb\":$ref_rss_kb,\"rs_rss_kb\":$rs_rss_kb,\"rss_ratio\":$rss_ratio}"

    TOTAL_REF=$(awk -v t="$TOTAL_REF" -v a="$ref_wall" 'BEGIN{printf "%.4f", t+a}')
    TOTAL_RS=$(awk  -v t="$TOTAL_RS"  -v a="$rs_wall"  'BEGIN{printf "%.4f", t+a}')
done

OVERALL_RATIO=$(awk -v a="$TOTAL_RS" -v b="$TOTAL_REF" 'BEGIN{if (b>0) printf "%.2f", a/b; else print "NaN"}')

{
    printf '{\n'
    printf '  "timestamp_utc": "%s",\n' "$TS"
    printf '  "commit": "%s",\n' "$COMMIT"
    printf '  "os": "%s", "arch": "%s", "cpu": "%s",\n' "$OS_NAME" "$ARCH" "$CPU"
    printf '  "runs_per_workload": %d,\n' "$RUNS"
    printf '  "totals": {"ref_wall_s": %s, "rs_wall_s": %s, "overall_wall_ratio": %s},\n' "$TOTAL_REF" "$TOTAL_RS" "$OVERALL_RATIO"
    printf '  "rows": [%s]\n' "$JSON_ROWS"
    printf '}\n'
} > "$JSON"

echo "" >&2
echo "==> results:" >&2
echo "    tsv:  $TSV" >&2
echo "    json: $JSON" >&2
echo "" >&2
column -t -s $'\t' "$TSV"
echo ""
printf "overall wall-clock ratio (lua-rs / reference): %s\n" "$OVERALL_RATIO"
