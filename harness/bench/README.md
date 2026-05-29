# lua-rs benchmark harness

Side-by-side performance characterization of `lua-rs` against pinned upstream
Lua 5.4.7. Modeled on the bench shape used in `redis-rs-port` and
`nginx-rs-port`, adapted for an interpreter (no servers, no protocol — just
"run this `.lua` workload through both binaries, measure").

The unit of measurement is **the ratio** (lua-rs / reference), not absolute
throughput. A standalone "lua-rs runs at X ops/s" number tells you nothing
about the port; the ratio against the reference C interpreter under the same
workload is the only fair comparison.

## What's wired

```
harness/bench/
├── README.md            <- this file
├── compare.sh           <- main ledgered bench: run all workloads vs reference
├── scaling-check.py     <- complexity gate: flag superlinear (O(n^2)) behavior
├── workloads/           <- self-contained .lua microbenchmarks (timed vs C)
│   ├── binarytrees.lua  <- GC pressure (CLBG-style)
│   ├── closure_ops.lua  <- closure allocation + upvalue access
│   ├── fibonacci.lua    <- recursive call dispatch + small-int math
│   ├── gc_pressure.lua  <- allocation/collection throughput under churn
│   ├── mandelbrot.lua   <- float math + nested loops
│   ├── string_ops.lua   <- concat/find/gsub/byte ops
│   ├── table_hash_pressure.lua <- hash-part insertion (#38 regression guard)
│   └── table_ops.lua    <- table insert/remove/iterate, array + hash
└── scaling/             <- size-parameterized workloads for scaling-check.py
    ├── array_insert.lua
    ├── gc_churn.lua
    ├── hash_insert.lua
    └── pairs_iter.lua
```

`compare.sh` measures the lua-rs/C ratio at one size. `scaling-check.py`
(`make scaling`) is complementary: it runs each `scaling/` workload at 1x..8x
of a base size and fits the complexity exponent, failing if an operation that
should be linear goes superlinear. That gate catches O(n^2) regressions a
single-size ratio would miss (it is what would have caught the #38 table bug).

Generated artifacts land under `results/` and `profiles/` (gitignored).
The static dashboard at `history/index.html` IS tracked so it can be viewed
directly from GitHub via [raw.githack.com][dash] or by opening the file
locally in a browser.

[dash]: https://raw.githack.com/ianm199/lua-rs/main/harness/bench/history/index.html

Every workload is **deterministic** — same output on every run, same on
both interpreters. The compare runner asserts checksum equality (any drift
fails the bench, doubling as a correctness oracle).

## How to run

```bash
# build both binaries first
make macosx -C reference/lua-5.4.7   # produces reference/lua-5.4.7/src/lua
cargo build --release -p lua-cli     # produces target/release/lua-rs

# all workloads, best-of-5
bash harness/bench/compare.sh

# subset, fewer runs (smoke)
bash harness/bench/compare.sh --runs 2 --workloads fibonacci,mandelbrot
```

Output:
- `harness/bench/results/<UTC>-<sha>-compare.tsv` (header + per-workload rows)
- `harness/bench/results/<UTC>-<sha>-compare.json` (machine-readable)
- Appends 2 rows per workload (`wall_ratio`, `rss_ratio`) to
  `harness/evidence/ledger.jsonl` so the dashboard can plot trends

To rebuild the dashboard after a bench run:

```bash
python3 harness/bench/history.py        # writes harness/bench/history/index.html
python3 harness/bench/history.py --open # also opens it in your browser
```

## How to read the numbers

`wall_ratio` is the headline. It is best-of-N wall-clock for lua-rs divided
by best-of-N wall-clock for reference. **Lower is better.**

Best-of-N (not mean) is the standard interpreter-benchmark convention. It
filters out scheduling jitter without smearing real performance differences.

`rss_ratio` is max-RSS lua-rs / max-RSS reference. Memory overhead at peak.

Hardware + commit fingerprint is in the TSV header. **Do not merge runs
from different machines** — apples to oranges.

## First numbers (Apple M3 Max, 2026-05-22)

| workload      | ref wall (s) | lua-rs wall (s) | wall ratio | rss ratio |
|---------------|--------------|-----------------|------------|-----------|
| mandelbrot    | 0.08         | 0.18            | **2.25x**  | 1.39x     |
| binarytrees   | 0.45         | 1.35            | **3.00x**  | 2.70x     |
| fibonacci     | 2.50         | 13.06           | **5.22x**  | 1.61x     |
| string_ops    | 0.01         | 0.35            | **35x**    | 3.93x     |
| closure_ops   | 0.18         | 25.80           | **143x**   | 2.68x     |
| table_ops     | 0.05         | 523.67          | **10,473x**| 2.09x     |

### What this tells us

- **mandelbrot 2.25x and binarytrees 3.00x are good** — float arithmetic
  loops and GC under steady allocation pressure are competitive. The
  interpreter's hot path on numeric work is in the right shape.
- **fibonacci 5.22x is acceptable** for a safe-Rust interpreter port — pure
  call dispatch overhead matches the typical "5–15x slower than optimized C"
  for interpreters without JIT or unsafe shortcuts.
- **string_ops 35x is a real hotspot.** The string library is the slowest
  part of the runtime measured here. Worth a `profile-hotspots` pass before
  any future optimization work.
- **closure_ops 143x is a major hotspot.** Closure allocation and upvalue
  access are doing far more work than they should. Possibly related to GC
  bookkeeping per closure or upvalue indirection cost.
- **table_ops 10,473x is almost certainly a bug, not just a slowdown.** An
  interpreter is not 10,000x slower than C without a pathological
  algorithm — most likely `table.remove` or `table.insert` at non-tail
  positions is O(n) per call where it should be O(1) amortized, or some
  similar quadratic implementation. **This is the first thing to
  investigate.**

## Probe vs ledgered bench split (when we add probes)

`compare.sh` is a **ledgered** bench: every run produces evidence that
should be commitable history. Numbers move with optimization work.

Probes are different — they answer narrow questions during exploration
("does throughput improve with N? does max-RSS scale with payload? where
are the allocation hot stacks?") and write to `profiles/` (gitignored).
**Probes never write ledger rows.** Treat their output as telemetry, not
evidence. This is the redis-rs-port convention; we follow it here.

`profile-hotspots.py` and `profile-calltree.py` will be the macOS-specific
CPU-sampler integrations (via `/usr/bin/sample` or `xctrace`), planned but
not yet wired.

## Reproducibility rules

- Always run with the matching `target/release/lua-rs` build (NOT `target/debug`)
- Always run from a clean working tree (no in-flight edits to runtime crates)
- Do not run other CPU-heavy work in parallel
- Record the hardware fingerprint from the TSV header when sharing numbers

## Next steps (not yet landed)

1. **Investigate the `table_ops` 10,473x outlier** — almost certainly a
   quadratic implementation in `crates/lua-stdlib/src/table_lib.rs` or
   the table internal representation. First profiling target.
2. `probe-hypotheses.py` — multi-mode exploration runner: `shape` mode for
   call-overhead vs body-cost split, `alloc-stacks` mode using
   `MallocStackLogging` + `malloc_history`, `xctrace-time` mode for raw
   CPU profile capture.
3. `profile-hotspots.py` + `profile-calltree.py` — ledgered profile runners
   that emit aggregated top-N frames per workload.
4. Wire bench runners into `harness/runners.toml` (`bench-vs-reference`,
   `bench-profile-hotspots`, `bench-profile-calltree`) and matching packets
   in `harness/work-packets.jsonl`.
5. `backfill.py` — historical bench data via detached worktrees per commit.
   The "when did perf regress?" answer. Real engineering work; do after
   the core flow is steady.
6. `.gitignore` patterns are in place for `results/` and `profiles/`; the
   Stop-hook auto-commit should be audited so it does not start tracking
   regenerated bench artifacts on the next session.
