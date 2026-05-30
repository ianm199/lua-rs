#!/usr/bin/env bash
#
# parity_check.sh — the BEHAVIORAL parity oracle for lua-rs vs reference C Lua 5.4.7.
#
# For each official Lua test file (reference/lua-c/testes/*.lua, excluding the
# `_`-prefixed helpers), this runs the SAME wrapped program through both the
# lua-rs binary and the reference C 5.4.7 binary, normalizes the volatile lines
# out of each transcript, and byte-compares normalized stdout+stderr together
# with the process exit code.
#
# A file MATCHes iff both binaries produce the same exit code AND the same
# normalized output. Otherwise it DIVERGEs.
#
# This is the truth-teller the no-crash `run_official_all.sh` gate lacks:
# run_official_all.sh only asserts "lua-rs did not crash / did not abort";
# this oracle asserts "lua-rs produced the SAME observable behavior as C".
#
# Exit code: 0 iff every file MATCHes, nonzero (count of diverging files,
# capped at 125) otherwise.
#
# Environment overrides:
#   REF=...         path to reference C lua binary (default below)
#   LUA_RS_BIN=...  path to lua-rs binary          (default below)
#
# Per-file normalized transcripts for diverging files are written to
# $TMPDIR/div_<file>_{rs,c}.txt for inspection.
#
# See harness/PARITY.md for the categorized divergence catalogue.

set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# ---- COMMON: default binaries -------------------------------------------------
LUA_RS_BIN="${LUA_RS_BIN:-$ROOT/target/debug/lua-rs}"
REF="${REF:-$ROOT/reference/lua-5.4.7/src/lua}"
TESTES="$ROOT/reference/lua-c/testes"

# Preamble injected before each test, matching the official soft/port harness
# knobs the test files expect (skip slow paths, skip platform-specific bits).
PRE='_soft=true; _port=true; _nomsg=true; _U=false; arg=arg or {}; _G=_G or _ENV; if _VERSION==nil then _VERSION="Lua 5.4" end'

export LUA_PATH="$TESTES/?.lua;$TESTES/?/init.lua;./?.lua;./?/init.lua"

OUTDIR="${TMPDIR:-/tmp}"

if [ ! -x "$LUA_RS_BIN" ]; then
  echo "ERROR: lua-rs binary not found/executable: $LUA_RS_BIN" >&2
  echo "       build it with: cargo build -p lua-cli -q" >&2
  exit 126
fi
if [ ! -x "$REF" ]; then
  echo "ERROR: reference C binary not found/executable: $REF" >&2
  exit 126
fi

# ---- normalization ------------------------------------------------------------
#
# Strip / canonicalize lines that legitimately vary run-to-run or host-to-host
# and are NOT a behavioral difference:
#   * heap addresses (0x...)             -> 0xADDR
#   * elapsed-time fractions (s/ms/sec)  -> dropped to ""
#   * "total time" / "memory" / "elapsed" summary lines -> deleted
#   * blank lines collapsed away
#
# Everything else is compared verbatim. Categories that survive normalization
# (PRNG seeds, GC step dot-counts, comparison counts, os.date timezone, ...) are
# the documented divergences in PARITY.md — they are intentionally NOT scrubbed
# here so the oracle keeps reporting them honestly.
norm() {
  sed -E \
    -e 's/0x[0-9a-fA-F]+/0xADDR/g' \
    -e 's/[0-9]+\.[0-9]+ *(s|sec|seconds|ms)//g' \
    -e '/total time/d' \
    -e '/memory/Id' \
    -e '/elapsed/Id'
}

match=0
diverge=0
declare -a DIV

printf "  %-14s %s\n" "FILE" "RESULT"
printf "  %-14s %s\n" "----" "------"

for tf in "$TESTES"/*.lua; do
  base=$(basename "$tf" .lua)
  case "$base" in _*) continue;; esac

  wrap=$(mktemp "$OUTDIR/pw.XXXXXX.lua")
  printf '%s\ndofile([[%s]])\n' "$PRE" "$tf" > "$wrap"

  # all.lua expects to be run from inside the testes dir (it dofiles siblings).
  cwd="$ROOT"
  [ "$base" = "all" ] && cwd="$TESTES"

  ( cd "$cwd" && timeout 90 "$LUA_RS_BIN" "$wrap" >"$OUTDIR/rs.out" 2>&1 ); rcrs=$?
  ( cd "$cwd" && timeout 90 "$REF"        "$wrap" >"$OUTDIR/c.out"  2>&1 ); rcc=$?

  norm <"$OUTDIR/rs.out" >"$OUTDIR/rs.n"
  norm <"$OUTDIR/c.out"  >"$OUTDIR/c.n"

  if [ "$rcrs" = "$rcc" ] && diff -q "$OUTDIR/rs.n" "$OUTDIR/c.n" >/dev/null 2>&1; then
    match=$((match+1))
    printf "  %-14s MATCH    (exit %s)\n" "$base" "$rcrs"
  else
    diverge=$((diverge+1)); DIV+=("$base")
    dl=$(diff "$OUTDIR/rs.n" "$OUTDIR/c.n" 2>/dev/null | grep -cE '^[<>]')
    printf "  %-14s DIVERGE  (exit rs=%s c=%s, difflines=%s)\n" "$base" "$rcrs" "$rcc" "$dl"
    cp "$OUTDIR/rs.n" "$OUTDIR/div_${base}_rs.txt"
    cp "$OUTDIR/c.n"  "$OUTDIR/div_${base}_c.txt"
  fi
  rm -f "$wrap"
done

total=$((match+diverge))
echo ""
echo "=== PARITY ORACLE (behavioral, lua-rs vs reference C 5.4.7) ==="
echo "    $match / $total MATCH   ($diverge DIVERGE)"
if [ "${#DIV[@]}" -gt 0 ]; then
  echo "    DIVERGING: ${DIV[*]}"
  echo "    (normalized transcripts in $OUTDIR/div_<file>_{rs,c}.txt; see harness/PARITY.md)"
fi

if [ "$diverge" -eq 0 ]; then
  exit 0
fi
# Cap exit code so it stays a valid 1..125 shell status.
[ "$diverge" -gt 125 ] && diverge=125
exit "$diverge"
