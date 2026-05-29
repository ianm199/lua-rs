#!/usr/bin/env python3
"""Detect superlinear (O(n^2) and worse) behavior in lua-rs.

Each workload in harness/bench/scaling/ runs the same operation N times and
prints a `time=<seconds>` line. We run it at 1x/2x/4x/8x of a base size and
fit the complexity exponent on a log-log scale: time ~ N^slope. slope ~1.0 is
linear, ~2.0 is quadratic. Any workload here is one that *should* be linear,
so a high slope means a regression (this is what would have caught the O(n^2)
table-insert bug in #38).

Usage:
    python3 harness/bench/scaling-check.py [--base N] [--reps R] [--bin PATH]

Exit code is nonzero if any workload comes out superlinear, so it can gate.
"""
import argparse
import math
import os
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCALING_DIR = ROOT / "harness" / "bench" / "scaling"
TIME_RE = re.compile(r"time=([0-9.]+)")

# Slope thresholds (exponent of time vs N). A true O(n^2) regression has
# slope ~2.0 (time 4x per 2x size); cache effects at large N push even linear
# code to ~1.3-1.4, so the fail bar sits well above that to avoid false
# positives. Compare against reference C (--vs-ref) for a precise read.
OK_MAX = 1.5      # <= this: treat as linear (covers cache effects)
WARN_MAX = 1.7    # (OK_MAX, this]: superlinear, warn only
                  # > WARN_MAX: quadratic-ish, fail


def run_once(binary, workload, n):
    """Returns elapsed seconds, or None if the run failed (e.g. the workload
    errored at this size). Failures are reported, not fatal."""
    env = dict(os.environ, LUA_SCALING_N=str(n))
    out = subprocess.run(
        [binary, str(workload)], env=env, capture_output=True, text=True, timeout=300
    )
    m = TIME_RE.search(out.stdout)
    if not m:
        return None
    return float(m.group(1))


def measure(binary, workload, sizes, reps):
    # Best (min) of reps runs per size, to cut scheduler noise. A size that
    # fails on every rep records None.
    out = []
    for n in sizes:
        runs = [t for t in (run_once(binary, workload, n) for _ in range(reps)) if t is not None]
        out.append(min(runs) if runs else None)
    return out


def slope(sizes, times):
    # Least-squares fit of log(time) vs log(N). Skip missing/near-zero times.
    pts = [(math.log(n), math.log(t)) for n, t in zip(sizes, times) if t and t > 1e-6]
    if len(pts) < 2:
        return float("nan")
    n = len(pts)
    sx = sum(x for x, _ in pts)
    sy = sum(y for _, y in pts)
    sxx = sum(x * x for x, _ in pts)
    sxy = sum(x * y for x, y in pts)
    denom = n * sxx - sx * sx
    if abs(denom) < 1e-12:
        return float("nan")
    return (n * sxy - sx * sy) / denom


def verdict(s):
    if math.isnan(s):
        return "NOISE"
    if s <= OK_MAX:
        return "LINEAR"
    if s <= WARN_MAX:
        return "SUPERLINEAR"
    return "QUADRATIC"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", type=int, default=50000, help="base size N (1x)")
    ap.add_argument("--reps", type=int, default=3, help="runs per size, min is kept")
    ap.add_argument("--bin", default=os.environ.get("LUA_RS_BIN", str(ROOT / "target/release/lua-rs")))
    args = ap.parse_args()

    if not Path(args.bin).exists():
        sys.exit(f"binary not found: {args.bin} (build with: cargo build --release --bin lua-rs)")

    sizes = [args.base * m for m in (1, 2, 4, 8)]
    workloads = sorted(SCALING_DIR.glob("*.lua"))
    if not workloads:
        sys.exit(f"no workloads in {SCALING_DIR}")

    print(f"scaling check  bin={args.bin}  sizes={sizes}  reps={args.reps}\n")
    print(f"{'workload':<16}{'t@1x':>10}{'t@8x':>10}{'slope':>8}  verdict")
    print("-" * 54)

    def fmt(t):
        return f"{t:.4f}" if t else "n/a"

    failed = []
    for w in workloads:
        times = measure(args.bin, w, sizes, args.reps)
        s = slope(sizes, times)
        v = verdict(s) if any(times) else "ERROR"
        print(f"{w.stem:<16}{fmt(times[0]):>10}{fmt(times[-1]):>10}{s:>8.2f}  {v}")
        if v == "QUADRATIC":
            failed.append((w.stem, v, s))

    print()
    if failed:
        for name, v, s in failed:
            print(f"FAIL: {name} is {v} (slope {s:.2f}); it should be ~linear")
        sys.exit(1)
    print("all workloads linear")


if __name__ == "__main__":
    main()
