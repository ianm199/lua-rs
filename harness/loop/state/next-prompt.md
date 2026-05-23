# lua-rs-port Perf-Fixer Packet

You are running as the `test-fixer` role, **but this packet is performance
work, not correctness work.** Read the `PACKET_NOTE` below carefully — it
tells you which hot frame to attack and which profile artifact provides
the evidence.

## Required reading before you start

1. `docs/PERFORMANCE_PRINCIPLES.md` — the playbook
   (evidence-driven, gate-driven, dashboard-driven).
2. `docs/MATCHING_C_PERFORMANCE.md` — the lessons digest. The target is
   parity with reference Lua 5.4.7. The redis-rs-port sibling hit
   parity through this same methodology.
3. The profile artifact named in `PACKET_NOTE` below, under
   `harness/bench/profiles/`. The top of every such file is a human
   summary of top frames by % of wall-clock; the full call graph follows.
4. The C-Lua reference for the function you're attacking, under
   `reference/lua-c/`.

## Failing perf-parity fixtures (workloads with wall_ratio > 1.5x)

Latest oracle evidence blob:

`harness/bench/results/20260523T135221Z-0152912-compare.json`

  - `binarytrees` (proves `lua-perf-binarytrees`) - last attempt 20260523T135221ZZ: 0/1
  - `closure_ops` (proves `lua-perf-closure-ops`) - last attempt 20260523T135221ZZ: 0/1
  - `db` (proves `debug-line-hook-timing`) - last attempt 2026-05-22T03:29:44Z: 0/1
  - `fibonacci` (proves `lua-perf-fibonacci`) - last attempt 20260523T135221ZZ: 0/1
  - `gc` (proves `gc-cycle-convergence`) - last attempt 2026-05-22T03:33:29Z: 0/1
  - `gengc` (proves `gc-cycle-convergence`) - last attempt 2026-05-22T03:32:53Z: 0/1
  - `mandelbrot` (proves `lua-perf-mandelbrot`) - last attempt 20260523T135221ZZ: 0/1
  - `string_ops` (proves `lua-perf-string-ops`) - last attempt 20260523T135221ZZ: 0/1
  - `string_ops_long` (proves `lua-perf-string-ops-long`) - last attempt 20260523T135221ZZ: 0/1
  - `table_ops` (proves `lua-perf-table-ops`) - last attempt 20260523T135221ZZ: 0/1
  - `table_ops_long` (proves `lua-perf-table-ops-long`) - last attempt 20260523T135221ZZ: 0/1

Packet-specific hypothesis and target:

EXISTING PERF(port) CALLOUT at crates/lua-vm/src/tagmethods.rs:328 says 'Vec allocation on every call — profile in Phase B; may become a Cow<\'static, [u8]> once lifetimes are firmed up'. obj_type_name returns Vec<u8> for the type name, allocating per call. The static type names (b"nil", b"boolean", b"number" etc.) can be returned as &'static [u8] without allocation. HYPOTHESIS: change obj_type_name to return Cow<'static, [u8]> — Borrowed for the static-name case (no metatable __name), Owned only when reading a custom __name from a metatable. Update callers to accept Cow or call .as_ref() at the use site. Look at TYPE_NAMES in the same file. EXPECTED IMPACT: small per-call savings, may show up indirectly. The point is to retire the PERF(port) callout. Bench via compare.sh. 44/44 must stay green.

Declared target files (changes outside this set will fail the chassis
out-of-scope-changes gate):

  - `crates/lua-vm/src/tagmethods.rs`

Profile-evidence and reference paths (read in order):

  - `crates/lua-vm/src/tagmethods.rs:300-370`
  - `reference/lua-c/lstate.c:200-280`
  - `reference/lua-c/lobject.c:300-400`

Affected capabilities and owners:

  - **`debug-line-hook-timing`** - owners: `lua-vm`
  - **`gc-cycle-convergence`** - owners: `lua-gc`, `lua-vm`
  - **`lua-perf-binarytrees`** - owners: `crates/lua-gc/src/heap.rs`, `crates/lua-vm/src/state.rs`, `crates/lua-types/src/table.rs`
  - **`lua-perf-closure-ops`** - owners: `crates/lua-vm/src/vm.rs`, `crates/lua-vm/src/func.rs`, `crates/lua-vm/src/state.rs`
  - **`lua-perf-fibonacci`** - owners: `crates/lua-vm/src/vm.rs`, `crates/lua-vm/src/state.rs`, `crates/lua-vm/src/do_.rs`
  - **`lua-perf-mandelbrot`** - owners: `crates/lua-vm/src/vm.rs`
  - **`lua-perf-string-ops`** - owners: `crates/lua-stdlib/src/string_lib.rs`, `crates/lua-vm/src/api.rs`
  - **`lua-perf-string-ops-long`** - owners: (no owners listed)
  - **`lua-perf-table-ops`** - owners: `crates/lua-vm/src/state.rs`, `crates/lua-types/src/table.rs`, `crates/lua-stdlib/src/table_lib.rs`
  - **`lua-perf-table-ops-long`** - owners: (no owners listed)

## Required process

Follow the gate exactly:

1. **Read the profile artifact** named in `PACKET_NOTE`. Note which
   frame is hot and what % of wall it consumed. Form a hypothesis
   BEFORE editing.
2. **Read the C-Lua reference** for the same function. Note whether
   C-Lua has a fast path for the case your hypothesis suggests we're
   missing. It usually does — that's the whole pattern.
3. **Make ONE focused change.** Smaller is better. Patterns that work:
     - "Use the fast path that already exists"
     - "Cache the precondition, don't disable the feature"
     - "Match upstream's structure"
     - "Don't clone where C uses pointers"
4. **Rebuild**: `cargo build --release -p lua-cli -q`. Must be clean.
5. **44/44**: `./harness/run_official_all.sh`. Must stay green.
6. **GC canaries** (if you touched GC code): `./harness/canaries/gc/run_canaries.sh`.
7. **Bench**: `./harness/bench/compare.sh --runs 5`. Confirm the
   targeted workload moved (ratio drops, or at least no regression)
   and others stay within ±10%.

The Stop hook will auto-commit on green correctness checks.

## Hard rules

- **44/44 stays green.** If you can't keep it passing, revert.
- **No benchmark-only fast paths.** If the workload path is unrealistic, fix is fraudulent.
- **No skipped semantic correctness.** Skip work that's provably
  unnecessary; never skip work that matters.
- **No new `unsafe`** outside `lua-gc` / `lua-coro`.
- **No `String` for Lua data.** Use `&[u8]` / `Vec<u8>` / `LuaString`.
- **Touch only the declared target files.**
- **No inline `//` comments**, doc strings only.
- **No fallback patterns** like `x || y || z`.

## Output contract

- `cargo build --release -p lua-cli -q` clean
- 44/44 passes; GC canaries pass if touched
- compare.sh shows the targeted workload improved (or unchanged)
- Touched `.rs` files have a refreshed `PORT STATUS` trailer

If the bench did not actually improve, the commit message should explain why
(e.g. "LLVM was already optimizing this path; no measurable change but the
code is cleaner"). Hygiene is fine; lying about perf is not.

Project root: `/Users/ianmclaughlin/PycharmProjects/rustExperiments/lua-rs-port`
Prompt hash: `7dfd9fcbcca9200b`
Evidence path: `harness/evidence/runs/20260523T135409Z-a55fa43-test-fixer-perf-obj-type-name-vec-alloc.json`
