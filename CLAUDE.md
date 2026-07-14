# omniLua — agent guide

Pure-Rust port of Lua that runs **5.1, 5.2, 5.3, 5.4, and 5.5** from one core,
selected per instance. No C dependency. Ships to crates.io + npm, runs the stock
LuaRocks client, embeds in Rust programs and in the browser (`wasm32`). This file
is the operational entry point — read it first, then the subsystem guide for
wherever you're working (`crates/<x>/CLAUDE.md`, `harness/CLAUDE.md`).

The public artifact is **omniLua**: embedding crate `omnilua` (dir
`crates/lua-rs-runtime/` unchanged), CLI crate `omnilua-cli` producing the
`omnilua` binary, npm package `omnilua`. The version env var is
`OMNILUA_VERSION` (canonical), with `LUA_RS_VERSION` still read as a fallback.
The local directory `lua-rs-port/` and internal crate names (`lua-vm`, `lua-gc`,
…) are unchanged — they are implementation paths, not public surfaces.

This repo is one of three siblings under `../`; see **`../CLAUDE.md`** for the
tree-level story (the AI-agent harness is the real product). Cross-repo
"who's-doing-what" lives in **`../AGENT_COORDINATION_BOARD.md`**.

## Orientation — find current truth, don't trust prose

- **Backlog → GitHub issues** (`gh issue list`). No doc enumerates "what's left."
- **Test status → run the harness** (`harness/run_official_all.sh`). Never trust a
  hardcoded pass-count written in a doc — they rot.
- **Version → `CHANGELOG.md` / `git tag`.** Don't hardcode it anywhere.
- Active frontier: generational GC (issues #104, #113) and extracting the harness
  to `../port-harness` + `../redis-rs-port`.

## The one rule: the oracle is the only truth-teller

A change that builds but no oracle has spoken on is **unverified**. Build success
is not signal. The oracle is the unmodified upstream reference binary
(`reference/lua-5.4.7/`, `reference/lua-5.3.6/`, and `/tmp/lua-refs/bin/lua5.x`).
The full methodology for version work is **`specs/MULTIVERSION_PLAYBOOK.md`** —
read it before touching version-specific behavior.

## How to work with this repo

### Build & run
```bash
cargo build -p omnilua-cli -q         # debug CLI → target/debug/omnilua
target/debug/omnilua script.lua       # run a file
target/debug/omnilua -e 'print(1+2)'  # one-liner
target/debug/omnilua                  # REPL (no args)
OMNILUA_VERSION=5.1 target/debug/omnilua script.lua  # pick a version (5.1–5.5; default 5.4)
```

### The iteration ladder — climb only as far as the question forces
| Tier | Command | Answers |
|---|---|---|
| 1 | `cargo build -p <crate> -q` | does it compile? |
| 2 | `cargo test -p lua-rs-runtime --test multiversion_oracle` | behavior vs the baked oracle constants — **the inner loop** |
| 3 | `specs/oracle/diff_one.sh <ver> "<snippet>"` | one specific divergence vs the live reference binary |
| 4 | `harness/canaries/gc/run_canaries.sh` | a GC / metamethod / table change didn't break the collector |
| 5 | `harness/run_official_test.sh reference/lua-c/testes/<t>.lua` | one real program (5.4) |
| 6 | `harness/run_official_all.sh` + `cargo test --workspace` + `specs/oracle/check.sh` ×5 | the PR gate |

A GC-lifecycle, heap-guard, VM-construction, or embedding-entry-point change
also runs **`harness/strict_guard_check.sh`**. The GC's three no-active-heap
guard checks (detached allocation, sweep-blind weak handle, dropped pacer
charge) now panic **unconditionally in every build** — the never-freed dual of
`LUA_RS_GC_QUARANTINE`'s freed-too-early — so any guard-coverage violation
self-reports with a backtrace under the normal test suites; the script is just
the convenience runner for the whole workspace. The embedding leak canaries
(`crates/lua-rs-runtime/tests/leak_canaries.rs`, a counting global allocator
asserting net-zero live bytes across VM/chunk/coroutine/callback churn) run
with the normal workspace tests.

Start one rung lower than feels right; if the cheap rung is silent, that's your
answer. Per-version detail: `specs/MULTIVERSION_PLAYBOOK.md §3`.

### Multi-version
One core, version chosen at runtime (`Lua::new_versioned` / `LUA_RS_VERSION`). The
hot bytecode dispatch loop is **version-free** — resolve the version **once** in a
cold path (e.g. `lua-vm`'s `legacy_for` flag) and never branch per-opcode.
**Version-gated compat code is load-bearing; do not "simplify" it away.** Seam map
and per-version cheat-sheet: `specs/MULTIVERSION_PLAYBOOK.md §5–6`. To debug a
non-5.4 divergence, diff the snippet against that version's reference:
`specs/oracle/diff_one.sh 5.3 "<snippet>"`.

**Reference binaries.** `reference/lua-5.4.7/` (port baseline) and
`reference/lua-5.3.6/` are vendored in-repo — source committed, binaries
gitignored, build with `make macosx -C reference/lua-5.x`. The multi-version
oracle scripts use `/tmp/lua-refs/bin/lua5.x` (all five versions), pinned in
`specs/oracle/CONTRACT.md`; rebuild them from there if `/tmp` was cleared.

### Benchmarks
`harness/bench/` measures the **omniLua / reference-C ratio** (wall + RSS), not
absolute throughput — the ratio is the only fair number. Any perf **claim**
must follow **`docs/MEASUREMENT_PROTOCOL.md`** (frozen-baseline interleaved
A/B, Ir/branch-sim arbiters, drop-if-neutral) — wall time alone does not
attribute on this rig.
```bash
bash harness/bench/compare.sh                              # all workloads, best-of-5
bash harness/bench/compare.sh --runs 3 --workloads fibonacci,binarytrees
python3 harness/bench/history.py                           # rebuild the tracked dashboard
```
Trust the `_long` workloads; sub-100ms ones are process-startup-dominated and
noisy. The dashboard (`harness/bench/history/index.html`) is tracked and updated
on release. To pin a perf regression: build at a suspected-good commit, `time` the
workload, then `git bisect run` a script that thresholds the best-of-N wall time
(how issue #113's GC-pacing regression was found). See `harness/CLAUDE.md`.

## Debugging playbook

1. **Start from the newest concrete failure**, not stale notes. Inspect
   `harness/impl/official/<test>.out`; open the combined source around the
   reported line (`nl -ba harness/impl/official/<test>.combined.lua | sed -n
   'A,Bp'`). If the harness says `unknown`, check process behavior + tail output.
2. **Turn the failure into a tiny repro before editing runtime code.** Use
   `target/debug/omnilua -e '...'`; for a non-5.4 bug use
   `specs/oracle/diff_one.sh <ver> '...'`. Print the actual `(expected, actual)`
   for message-matching tests. Use `/tmp` copies to instrument an official test —
   never leave instrumentation in `reference/` or `harness/impl/official/`.
3. **Patch the smallest cause.** Fix the function that returned the wrong
   value/message; don't normalize a whole subsystem for one assertion. Don't edit
   official tests except for temporary local diagnosis.
4. **Use adjacent gates.** Error-wording fixes rerun `errors.lua` + the test that
   exposed it. Table/GC/coroutine changes rerun `nextvar.lua`/`gc.lua` or the
   smallest GC canary. A shared-core change must match **every** affected
   version's reference, not just the one you're on.
5. **Watch for harness/environment bugs.** Tests that inspect source names or line
   numbers must run from a real file path. Temp-file names must be unique across
   parallel `omnilua` processes (pid + counter).
6. **Keep debugging out of the final diff.** Remove `print`/`eprintln!`, scratch
   files, one-off test edits. Build before final verification:
   `cargo build -p omnilua-cli -q`.

## Code style (mechanically enforced by `.claude/hooks/`)

- **No inline `//` comments** — explain in doc-comments. `// SAFETY:` on `unsafe`
  blocks is the required exception.
- **No fallback patterns** (`x || y || z`). Single source of truth; if data might
  be missing that's a pipeline bug to fix, not to paper over.
- **No `String` / `&str` for Lua data** — Lua strings are bytes
  (`&[u8]` / `Vec<u8>` / `LuaString`).
- **`unsafe` is budgeted** (`harness/unsafe-budgets.toml`); every block needs a
  `// SAFETY:` comment and must stay under the per-crate ceiling.

## Worktrees & parallel agents

Multiple git worktrees of this repo exist (`git worktree list`). **One branch per
worktree; never run two agents in the same worktree** — branch pointers collide
and commits land on the wrong branch (this has happened). For parallel work, give
each agent its own worktree: `git worktree add ../lua-rs-port-<name> <branch>`.
**Never `git stash` in a worktree** — `refs/stash` is shared across ALL
worktrees of this repo, so concurrent agents' stash/pop interleave and grab
each other's entries (bit two agents on 2026-07-14; both recovered via
`git fsck --unreachable`). Use a backup file or a scratch commit instead.
Within a single session, do not parallelize file-editing subagents in a shared
tree (`specs/MULTIVERSION_PLAYBOOK.md §7`). Coordinate cross-repo work on
`../AGENT_COORDINATION_BOARD.md`.

## Releasing

See **`RELEASING.md`**: bump versions, merge the bump PR, tag the **exact merge
SHA** — the tag push publishes **irreversibly** to crates.io + npm.

## Doc map

- **Methodology**: `specs/MULTIVERSION_PLAYBOOK.md` (add/fix a version),
  `../CLAUDE.md` (harness-as-product concepts), `RELEASING.md`.
- **Subsystem guides** (auto-loaded when you work in that subtree):
  `crates/{lua-vm,lua-gc,lua-stdlib,lua-parse,lua-rs-runtime}/CLAUDE.md`,
  `harness/CLAUDE.md`.
- **Reference**: `ANALYSES/*.tsv` (C→Rust lookup tables — look up, don't
  re-derive), `docs/PERFORMANCE_PRINCIPLES.md`, `docs/MATCHING_C_PERFORMANCE.md`,
  `docs/DEBUGGING_STRATEGIES.md`, `docs/LUA_SYSTEM_DEEP_DIVE.md`.
- **Port evidence (historical)**: `specs/README.md` indexes the per-version
  research, adversarial findings, and phase reports.
- **Contributing / public**: `CONTRIBUTING.md`, `README.md`.
