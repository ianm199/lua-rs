#!/usr/bin/env bash
# compare_luars.sh — three-way performance comparison:
#
#     reference C Lua 5.4.7   vs   ianm199/lua-rs   vs   CppCXY/lua-rs (luars)
#
# Prompted by https://github.com/ianm199/lua-rs/issues/12 — a friendly
# "compare notes" with CppCXY, whose luars is a separate Rust Lua runtime
# (Lua 5.5, with a fair amount of unsafe). This runs two suites through all
# three binaries: the CLBG-style numeric benchmarks shipped in CppCXY's repo,
# plus this repo's own harness/bench/workloads. It reports best-of-N min wall
# time, peak RSS, the wall-time ratio against reference C, and a geomean.
# Output is asserted byte-identical to reference C (any drift is flagged).
#
# Setup:
#   1. Build this repo's binary:   cargo build --release -p lua-cli --bin lua-rs
#   2. Build reference C Lua:      make -C reference/lua-5.4.7  (produces .../src/lua)
#   3. Clone + build CppCXY/luars next to this repo:
#        git clone https://github.com/CppCXY/lua-rs cppcxy-luars
#        cargo build --release -p luars_interpreter --bin lua --manifest-path cppcxy-luars/Cargo.toml
#
# Paths auto-detect a sibling ./cppcxy-luars clone; override via env:
#   REF=/path/to/c/lua  OURS=/path/to/lua-rs  LUARS=/path/to/luars/lua \
#   CLBG_DIR=/path/to/cppcxy-luars/lua_benchmarks  RUNS=5  bash compare_luars.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SIBLING="$(cd "$ROOT/.." && pwd)/cppcxy-luars"

REF="${REF:-$ROOT/reference/lua-5.4.7/src/lua}"
OURS="${OURS:-$ROOT/target/release/lua-rs}"
LUARS="${LUARS:-$SIBLING/target/release/lua}"
CLBG_DIR="${CLBG_DIR:-$SIBLING/lua_benchmarks}"
OWN="$ROOT/harness/bench/workloads"
RUNS=${RUNS:-5}

for pair in "reference C:$REF" "lua-rs:$OURS" "luars:$LUARS"; do
  bin="${pair#*:}"
  [ -x "$bin" ] || { echo "[err] ${pair%%:*} binary not found/executable: $bin" >&2;
                     echo "      see the Setup block at the top of this script." >&2; exit 2; }
done
[ -d "$CLBG_DIR" ] || { echo "[err] CLBG benchmark dir not found: $CLBG_DIR (clone CppCXY/lua-rs)" >&2; exit 2; }

# measure: $1=bin $2=script $3=arg(maybe empty) -> "minRealSeconds peakRssMB"
measure() {
  local bin="$1" s="$2" arg="${3:-}" best="" rss=0 tmp t r
  tmp=$(mktemp)
  for _ in $(seq 1 "$RUNS"); do
    if [ -n "$arg" ]; then /usr/bin/time -l "$bin" "$s" "$arg" >/dev/null 2>"$tmp"
    else /usr/bin/time -l "$bin" "$s" >/dev/null 2>"$tmp"; fi
    t=$(awk '{for(i=1;i<=NF;i++) if($i=="real") print $(i-1)}' "$tmp" | head -1)
    r=$(awk '/maximum resident set size/{print $1}' "$tmp" | head -1)
    if [ -z "$best" ] || awk -v a="$t" -v b="$best" 'BEGIN{exit !(a<b)}'; then best="$t"; fi
    if awk -v a="$r" -v b="$rss" 'BEGIN{exit !(a>b)}'; then rss="$r"; fi
  done
  rm -f "$tmp"; echo "$best $(awk -v b="$rss" 'BEGIN{printf "%.0f", b/1048576}')"
}

run_out() { local bin="$1" s="$2" arg="${3:-}"; if [ -n "$arg" ]; then "$bin" "$s" "$arg" 2>&1; else "$bin" "$s" 2>&1; fi; }

geo_o=1; geo_l=1; cnt=0
row() {  # $1=label $2=script $3=arg
  local n="$1" s="$2" a="${3:-}"
  local oref oours oluars match tr rr to ro tl rl rro rrl
  oref=$(run_out "$REF" "$s" "$a"); oours=$(run_out "$OURS" "$s" "$a"); oluars=$(run_out "$LUARS" "$s" "$a")
  match="ok"; [ "$oref" = "$oours" ] || match="OURS≠"; [ "$oref" = "$oluars" ] || match="${match}|LUARS≠"
  read tr rr < <(measure "$REF"   "$s" "$a")
  read to ro < <(measure "$OURS"  "$s" "$a")
  read tl rl < <(measure "$LUARS" "$s" "$a")
  rro=$(awk -v x="$to" -v y="$tr" 'BEGIN{printf "%.2f", x/y}')
  rrl=$(awk -v x="$tl" -v y="$tr" 'BEGIN{printf "%.2f", x/y}')
  geo_o=$(awk -v g="$geo_o" -v v="$rro" 'BEGIN{printf "%.6f", g*v}')
  geo_l=$(awk -v g="$geo_l" -v v="$rrl" 'BEGIN{printf "%.6f", g*v}')
  cnt=$((cnt+1))
  printf "%-18s %8s %8s %8s %7s %7s | %5s %5s %5s  %s\n" "$n" "$tr" "$to" "$tl" "$rro" "$rrl" "$rr" "$ro" "$rl" "$match"
}

echo "best-of-$RUNS min wall seconds; RSS peak MB; ratio = impl / reference-C (lower is faster, <1 beats C)"
echo "machine: $(sysctl -n machdep.cpu.brand_string 2>/dev/null || uname -p) $(uname -sm)"
echo
printf "%-18s %8s %8s %8s %7s %7s | %5s %5s %5s  %s\n" "benchmark" "C(s)" "ours" "luars" "ours/C" "luars/C" "C" "ours" "luars" "match"
echo "-- CLBG-style suite (from CppCXY/lua-rs) --------------------------------------------------"
row fannkuch_redux  "$CLBG_DIR/fannkuch_redux.lua" 10
row binary_trees    "$CLBG_DIR/binary_trees.lua"   15
row nbody           "$CLBG_DIR/nbody.lua"          1000000
row spectral_norm   "$CLBG_DIR/spectral_norm.lua"  500
row mandelbrot      "$CLBG_DIR/mandelbrot.lua"     1000
row partial_sums    "$CLBG_DIR/partial_sums.lua"   2000000
echo "-- this repo's own workloads (harness/bench/workloads) ------------------------------------"
row fibonacci          "$OWN/fibonacci.lua"
row binarytrees_own    "$OWN/binarytrees.lua"
row closure_ops        "$OWN/closure_ops.lua"
row gc_pressure        "$OWN/gc_pressure.lua"
row mandelbrot_own     "$OWN/mandelbrot.lua"
row string_ops         "$OWN/string_ops.lua"
row string_ops_long    "$OWN/string_ops_long.lua"
row table_ops          "$OWN/table_ops.lua"
row table_ops_long     "$OWN/table_ops_long.lua"
row table_hash_pressure "$OWN/table_hash_pressure.lua"

echo
awk -v go="$geo_o" -v gl="$geo_l" -v n="$cnt" 'BEGIN{
  printf "GEOMEAN wall-time ratio vs reference C:  lua-rs = %.2fx   luars = %.2fx   (n=%d)\n", go^(1/n), gl^(1/n), n
}'
