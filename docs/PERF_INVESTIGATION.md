# lua-rs Performance Investigation

## 1. Executive summary

The highest-leverage moves, ordered cheap-safe-win first. Every one of these is achievable without `unsafe`; the constraint `unsafe_code = "forbid"` is set workspace-wide and the four port crates (lua-vm, lua-types, lua-coro, lua-stdlib) inherit it.

1. **Build the deterministic perf inner loop first (`cargo-show-asm` natively, iai-callgrind as a Linux-CI gate).** Safe, no runtime gain, but it is the prerequisite that lets every lever below be measured honestly instead of through best-of-5 wall-clock noise. Lead with the native `cargo asm` view; treat valgrind-based iai as CI-only because the dev machine is arm64 and has no native valgrind.
2. **GC visited-set hasher swap SipHash to FxHasher.** Safe, small (~3.5% on GC-heavy workloads, zero elsewhere), one-file change. The cheapest real win to bank while building the harness.
3. **Inline (SmallVec-style) storage for tiny tables.** Safe, large on alloc/GC-bound workloads, and an edge over both reference C and CppCXY/luars (his own open issue #37). Gate it on a dhat alloc-bucket measurement first.
4. **Re-confirm, then attack, the dispatch question with the right tool.** Safe. The prior ~1.8x "indirect-branch misprediction dominates" thesis is contested by current literature and by the fact that luars hits ~111-132% of C with a plain `match`. Verify the indirect-branch site count and re-run the homogeneous-vs-cycle experiment before committing to any threaded-dispatch rewrite. Do not jump to nightly `become`.

A standing correction runs through this document: the framing that "dispatch misprediction dwarfs everything, fixable for ~1.8x via threaded dispatch" is the single claim most likely to mislead the next sprint. It is demoted here pending a real-workload re-measurement, and the memory-side levers (tiny-table inline storage, GC hasher/pacing) are promoted because they are cheaper, safer, and they are where the codebase's own profiler points.

## 2. Where the time goes today

The current picture is split across three buckets: per-instruction compute, allocation, and GC. All structural claims below were re-validated against current code; the magnitude figures rest on a prior macOS `sample` profiling run (2026-05-26) and are flagged as plausible-but-unverified-here where they were not re-derived.

### Compute (the per-instruction tax)

The interpreter core is one centralized `match op { ... }` over an `OpCode` enum inside a labeled triple-loop, dispatch site at `crates/lua-vm/src/vm.rs:1459`, inside `execute` at `vm.rs:1410`, roughly 83 arms ending at `vm.rs:2648`. There are effectively two table-shaped branches per instruction: the opcode decode `i.opcode()` (`vm.rs:183`, a `match (raw & 0x7F)` over 0..=82) feeding the dispatch `match` itself. No threaded dispatch, no fn-pointer table, no `become`/tail-calls exist.

`LuaValue` (`crates/lua-types/src/value.rs:13`) is a 10-variant tagged enum, `Copy`, `size_of == 16`, `align == 8`. It is NOT NaN-boxed. Every stack move copies 16 bytes; every opcode arm pays a tag `match`. This is the uniform per-op tax. NaN-boxing to 8 bytes is the only way to halve it and is correctly deprioritized as the one lever that fights `forbid` cleanly.

Register access is bounds-checked indexing (`state.stack[idx.0 as usize]`, ~27 sites, e.g. `vm.rs:1464`), zero `unsafe`. A prior `get_unchecked` ablation showed **zero** effect: these checks are predicted-free. This is confirmed unchanged and means bounds checks are not the bottleneck.

The string-keyed field read path is intact and is per-access: `OP_GETFIELD` (`vm.rs:1603`) to `state.fast_get_short_str` (`crates/lua-vm/src/state.rs:2635`) to `LuaTable::get_short_str` (`crates/lua-types/src/table.rs:1232`), which takes a fresh `self.inner.borrow()` (RefCell, `table.rs:1157`) on every access then calls `get_str_value` (`table.rs:881`). The lookup is a `hashpow2_idx` then a Brent-chain `loop`; because short strings are interned, the first comparison `GcRef::ptr_eq` (`table.rs:888`) wins on essentially every hit, and the hash is a precomputed cached field, not a live byte-hash.

**Refuted / changed claims to note:**

- The "~27% of compute in `fast_get_short_str`" figure is structurally plausible (the path is intact) but is **unverified-in-repo** and rests on an external profile not present in this tree. Treat the magnitude skeptically. The actual cost in that path is dominated by the RefCell borrow, the chain-loop setup, and the result clone, NOT by hashing or byte comparison, both of which are already engineered away (cached hash + interned pointer-eq).
- The "indirect-branch misprediction is the dominant ~1.8x compute lever" thesis is **demoted, not refuted.** The dispatch structure is real, but see section 3: current literature and the luars data point say the realistic ceiling is single-to-low-double-digit percent and is architecture-dependent, not 1.8x.

### Allocation

Every GC object is its own heap allocation. A tiny table is at minimum a `GcBox<LuaTable>` plus separate heap buffers for `array: Vec<LuaValue>` and `node: Vec<TableNode>` (`crates/lua-types/src/table.rs:231` / `:235-236`) the moment it grows, plus each string key/value is its own `GcBox<LuaString>` and its own `Rc<[u8]>` buffer. There is no inline/small-table storage. The reported figure is ~1250 bytes per tiny table versus reference C ~100, but this number traces to an external brief and should be re-measured with dhat before it drives work.

### GC

Three confirmed sub-claims:

- **Visited-set hasher.** `Marker.visited` is `std::collections::HashSet<usize>` (`crates/lua-gc/src/heap.rs:343`) built with `HashSet::new()` (`heap.rs:350`), so it uses RandomState/SipHash. `mark()` does `self.visited.insert(gc.identity())` per object (`heap.rs:366-368`). Pointer keys do not need HashDoS resistance; FxHasher is the cheaper swap. The set is rebuilt per cycle.
- **Byte-accounting undercount.** `Heap::allocate` charges only `size_of::<GcBox<T>>()` (`heap.rs:555`, added at `:566`; `new_uncollected` same at `:206`). Table Vec backing buffers and string `Rc<[u8]>` buffers are never counted. Reported as ~21% of RSS invisible to the pacer.
- **Per-object alloc count.** Confirmed many small allocations per tiny table, no inline storage anywhere in `TableInner`.

A note on the **gc_pressure 118.5x** result in the latest TSV: this is far more likely a sub-resolution-timer artifact (reference runs at ~0.02s) than a live 118x regression, and because `gc_pressure` is not in `history.py`'s hardcoded `WORKLOADS` list it is invisible on the dashboard while the fixed-1.5x oracle reports it as a perpetual failing fixture. Adjudicate it before trusting it.

### Validating the prior CppCXY claim

Issue #38's 140x-worse-than-C GC gap was reportedly cut to ~2x on main already. Confirm the current state of `lua-gc/src/heap.rs` against any re-attack so landed work is not redone.

## 3. The dispatch question (deep dive)

This is the claim that most needs re-litigating before any code is written.

### The asserted root cause

The interpreter routes every instruction through one indirect branch at the bottom of the centralized `match` (`vm.rs:1459`). The hypothesis: because all ~83 handlers jump back to a single shared dispatch site, the CPU branch-target predictor sees one PC and cannot learn opcode-to-opcode transition correlations, so it mispredicts, and this misprediction was rated the dominant residual compute gap (~1.8x vs C). The supporting in-house experiment was a 2000-identical-opcode loop at 1.22x vs C (predictable target) versus a 4-opcode-cycling loop at 2.92x vs C (adversarial target), instruction count held constant.

### Why the magnitude is suspect

Three recent, independent sources deflate the dispatch folklore:

- **Rohou/Swamy/Seznec, "Don't Trust Folklore" (CGO 2015).** On modern out-of-order CPUs with ITTAGE-class predictors, the dispatch indirect branch is no longer a dominant misprediction source. Measured on Haswell with hardware counters; an Apple M3 has a substantially better predictor.
- **Nelhage, "Performance of the Python 3.14 tail-call interpreter" (2025).** The celebrated "10% from tail calls" was mostly an LLVM 19 regression artifact. Against a correctly-compiled baseline, switch vs computed-goto vs tail-call differ by ~1-5%, and modern Clang auto-tail-duplicates a plain switch so the three converge.
- **Keeter, "A tail-call interpreter in nightly Rust" (2026).** Measured `become` directly: meaningful help on ARM64 (the M3 target), barely-or-negative on x86-64 due to poor LLVM codegen.

The 4-opcode cycle is an adversarial pattern that defeats any predictor and is not representative of real Lua bytecode mixes, which an ITTAGE-class predictor learns. Decisively, **CppCXY/luars reaches ~111-132% of reference C using a plain `loop { match opcode }` with no threading at all.** A peer that close to C without threaded dispatch is strong evidence the compute gap is not primarily dispatch misprediction.

### The safe Rust options

- **(d) `loop { match }` baseline.** LLVM may already tail-duplicate the dispatch tail per arm, giving threaded-code prediction for free, or may collapse to one indirect branch. This is the cheapest thing to check: disassemble `execute` and count indirect-branch sites. Roughly one per opcode means you already have threaded dispatch and the lever is largely spent; exactly one means there is headroom. No unsafe.
- **(c) Force dispatch-tail replication.** Coax LLVM by keeping hot arm bodies small (`#[inline(never)]` on cold handlers). Best-effort and LLVM-version-fragile; safe Rust has no stable `asm volatile` barrier to force it. Note: `#[inline(never)]` on cold arms shrinks the function but is not guaranteed to manufacture per-opcode dispatch tails for the hot arms, which is the actual requirement. Expected 0-10%. No unsafe.
- **(b) Function-pointer dispatch table.** A `[fn(&mut Vm) -> Next; N]` with `#[inline(never)]` handlers gives per-handler dispatch PCs without nightly. Risk: without guaranteed tail calls each handler is a real call/ret with stack churn; often a wash or slight loss vs a good `match` on x86. Expected -5% to +5%. No unsafe.
- **(a) `become` explicit tail calls (RFC 3407, nightly only).** The "real" CPS threaded interpreter (the Deegen/LuaJIT-Remake design, ~34% over LuaJIT's interpreter, but that number bundles inline caching and copy-and-patch, not tail-calls alone). It is 100% safe Rust, but: nightly-only with an unaccepted RFC; the `Drop` restriction fights a Lua VM whose state is full of `Drop` types (you would thread `&mut Vm` to sidestep, but any by-value `Drop` payload in the continuation signature is a blocker); and committing the VM to nightly is a project-policy decision.

### Recommendation and risk

**Measure before building.** Run `cargo asm -p lua-vm 'lua_vm::vm::execute'` natively on the arm64 release build and count indirect-branch sites. Re-run the homogeneous-vs-cycle experiment via the existing `compare_luars.sh`-style path to confirm the 1.22x-vs-2.92x split still reproduces. If dispatch is one collapsed site and the split reproduces, the highest-value safe experiment is a **function-pointer dispatch table** (option b), evaluated as its own spike, on the ARM target, against a real-workload opcode stream. The realistic upside is single-to-low-double-digit percent on ARM, not 1.8x. **Do not** prioritize a nightly `become` rewrite, and do not assume `#[inline(never)]` hygiene will deliver the win. Risk: even a clean fn-pointer table can be a wash on real workloads; this lever must be gated on measurement at every step.

## 4. What CppCXY does, and what we can take safely

luars targets Lua 5.5 and is architecturally a near-line-by-line port of C Lua with `unsafe` substituted for C's raw pointers. Its speed comes from being C Lua (C-faithful layout, raw-pointer hot paths, a custom paged allocator), not from an exotic JIT trick. Classified by safe-adoptability:

| Technique | What it is | Safe to adopt? | Note |
|---|---|---|---|
| Value union `{union, tt:u8}` | C `TValue`, not NaN-boxed | spirit yes, verbatim no | Our enum is already 16B/Copy/unboxed numbers. The union is not the lever. |
| Plain `loop { match opcode }` dispatch | Same shape as ours, no threading | nothing to adopt | This is the refutation: a peer at 111-132% of C uses a plain match. |
| `get_unchecked` + raw-ptr register access | Unchecked stack indexing | unsafe-only, and skip | Our own `get_unchecked` test showed zero effect. Do not pursue. |
| **Paged free-list allocator (`paged_pool.rs`)** | Slab/arena: O(1) alloc/free, contiguous, bulk reclaim | design yes (with restructure) | THE luars win. Safe via `slab`/`slotmap`/`typed-arena`. But see the caveat below. |
| **On-header GC colors + two-white flip** | Per-object color marks, no visited set | yes (with restructure) | The color byte needs no unsafe. Directly targets our SipHash visited-set. |
| **Honest byte-accounting** | Charges true allocated bytes | yes | Adopt the discipline of charging every backing allocation. |
| **Interned short-string fast path** | Pointer-eq on interned shorts | already have it | luars confirms the design; we already do `ptr_eq` first. |
| Inline tiny-table storage | (luars does NOT have this) | yes, and it is our edge | His open #37. Beating him here is pure upside. |
| `shared-proto` feature | Caches compiled proto across loads | yes | Low for single-run benches; matters for embedding (Redis/game-engine). |

The genuinely safe, high-payoff wins he demonstrates are: a slab allocator, on-header GC colors, honest byte-accounting, and interned fast-path field access (which we already have). His weakness, no inline tiny-table storage, is our opportunity.

**A caveat that downgrades the slab.** The slab story is less clean than "use `slotmap`" implies. The GC stores `GcBox<dyn Trace>`, an unsized type (`heap.rs`), and `slotmap`/`slab`/`typed-arena` all require `Sized`. You cannot hold heterogeneous `dyn Trace` in one `Vec<Slot<T>>` without per-type slabs. The existing allocator is already a hand-rolled raw-pointer mark-sweep in `lua-gc`, the one crate with `unsafe_code = "allow"`. So a real slab integration entangles with that unsafe machinery, which is why its risk is "low" rather than "none". And critically: a `GcBox` slab removes only ONE of the 2-3 allocations per tiny table. The `array`/`node` Vec buffers and `Rc<[u8]>` string buffers are not `GcBox` allocations and are untouched by it. Inline tiny-table storage is the cheaper, higher-yield fix for the same target.

## 5. Tooling and the perf iteration ladder

Today perf has only rungs 5-7: everything goes through best-of-5 `/usr/bin/time` wall-clock on real binaries (`harness/bench/compare.sh`, `compare_luars.sh`), whose ±2-5% jitter swamps the 3-20% levers and cannot attribute branch misprediction at all. There is no `criterion`/`iai`/`dhat`/`divan` anywhere and no `benches/` dir. The missing rungs 1-3 are exactly the project's own `conn_transport_kit` doctrine, unrealized on the perf side.

### The recommended ladder, fastest to slowest

| Tier | Tool | Exact command | Question |
|---|---|---|---|
| 0 | cargo check | `cargo check -p lua-vm` | does it compile? |
| 1 | cargo-show-asm | `cargo asm -p lua-vm --rust 'lua_vm::vm::execute'` | did `match op` lower to one jump table or per-arm branches? |
| 1 | iai-callgrind | `cargo bench -p lua-vm --bench dispatch_kit` (Linux/CI) | did instruction count / cache hits move? sub-1% sensitive, deterministic |
| 2 | criterion | `cargo bench -p lua-vm --bench vm_micro` | did real wall-time move, with CI bounds? |
| 3 | samply / sample | `samply record target/release/lua-rs <workload>` or `bash harness/bench/profile-hotspots.sh <wl>` | where is wall-time going? |
| 3 | Linux perf | `perf stat -e branch-misses,branches,instructions,cycles target/release/lua-rs <wl>` | the only tool that proves a misprediction change |
| 5 | dhat | run with a `dhat-heap` feature on `gc_pressure.lua`/`binarytrees.lua` | alloc count, bytes, bytes-per-table |
| 6 | oracle | `bash harness/bench/compare_luars.sh` / `make perf` | did geomean vs C/luars actually improve? the truth-teller |

Discipline mirrors the project ladder: start one rung lower than feels right. "Did this opcode tweak regress?" lives at Tier 1, not Tier 6. Only climb to perf/criterion when the change is branch- or cache-behavior-dependent (the dispatch work, where instruction count can stay flat while cycles move).

**Critical caveat for the dispatch work:** iai-callgrind's cycle estimate is a static cache model and does NOT model branch misprediction, which is precisely the class of change the dispatch work is about. So pair iai (instruction-count stability) with perf `branch-misses` (the actual mispredict rate) and criterion (wall confirmation). iai catches "I added work"; only perf catches "I fixed/broke the mispredict".

### Adopt these first

1. **cargo-show-asm, adopt first.** `cargo install cargo-show-asm`, zero runtime, runs natively on arm64 with no valgrind and no Docker. It directly inspects the one artifact the dispatch thesis rests on. This is the genuinely cheap, native-on-target rung-1 view. Do this before writing any dispatch code.
2. **iai-callgrind, adopt as a CI-only Linux gate, not a local inner loop.** Valgrind has no macOS-arm64 support, so on the dev machine it would run under x86 emulation in Docker, which is neither sub-second nor on-target. Make Linux CI the canonical iai surface (where deterministic instruction counts make perf regressions gateable, impossible with wall-time noise); macOS devs develop against criterion + samply locally and let CI's iai be the backstop.

### The permanent microbench harness: `dispatch_kit`

Make the improvised "N identical vs N cycling opcodes" experiment a durable, parameterized artifact, the perf analogue of `conn_transport_kit`. Two benches in `crates/lua-vm/benches/`, sharing a generator that synthesizes `Proto`s in memory (no parser, no file I/O, no GC churn) so a run is microseconds and reproduces deterministically.

`crates/lua-vm/Cargo.toml`:
```toml
[dev-dependencies]
iai-callgrind = "0.14"
criterion = { version = "0.5", default-features = false }

[[bench]]
name = "dispatch_kit"
harness = false

[[bench]]
name = "vm_micro"
harness = false
```
Workspace `Cargo.toml` (iai/samply/perf need symbols, iai needs un-stripped):
```toml
[profile.bench]
debug = true
strip = false
```
The load-bearing idea is the generator:
```rust
/// Synthesize a Proto whose body is `n` instructions drawn from `pattern`,
/// each writing a register the next reads (defeats DCE), then RETURN.
///   Homogeneous(op)  -> 1 indirect-branch target, predictor ~100% hit
///   Cycle(&[op,...]) -> rotating targets, predictor mispredicts
fn make_loop_proto(n: usize, pattern: Pattern) -> Proto { /* emit OpCodes */ }
enum Pattern { Homogeneous(OpCode), Cycle(&'static [OpCode]) }
```
Holding instruction count constant between `Homogeneous` and `Cycle` isolates branch misprediction specifically. `dispatch_kit.rs` runs the iai (deterministic) cases; `vm_micro.rs` runs the criterion wall-time pair plus the in-tree `workloads/*.lua` end-to-end. Wire `make bench-micro` (criterion, macOS-friendly) and a Linux CI `cargo bench --bench dispatch_kit` step that diffs against a committed baseline, the perf analogue of `adjacency-gate.sh`. Keep `unsafe_code = "forbid"` untouched; none of this needs unsafe.

## 6. Prioritized backlog

Sorted cheap-safe-big-win first.

| Title | Expected gain | Unsafe risk | Effort | Verdict | Cheapest next experiment |
|---|---|---|---|---|---|
| Tooling: cargo-show-asm + dispatch_kit harness | Enabler (no direct gain) | none | medium (asm part: minutes) | STRONG (asm half) / defer iai | `cargo install cargo-show-asm`; `cargo asm -p lua-vm lua_vm::vm::execute` natively |
| GC visited-set FxHasher swap | ~3.5% on GC-heavy, 0 elsewhere | none | small | STRONG | hasher-only swap (no pre-reserve); `compare_luars.sh` binary_trees best-of-5 |
| Inline tiny-table storage | Large on alloc/GC-bound | none | large | STRONG | dhat alloc-bucket on binarytrees: is it buffer- or GC-box-dominated? |
| Verify/force dispatch tail replication (ARM) | single-to-low-double-digit, arch-dependent | none | medium | reconsider as fn-ptr table | re-run homogeneous-vs-cycle; count indirect-branch sites |
| Inline cache for OP_GETFIELD | speculative (10-40% lit, likely far less here) | none | large | WEAK | throwaway no-invalidation cache to upper-bound the win |
| Slab/arena allocator | partial of ~21% malloc | low | large | WEAK | dhat: GcBox share of allocs (<40% means wrong lever) |
| GC two-white flip (delete visited set) | medium (set query sites block full removal) | low | large | WEAK | samply gc_pressure: is HashSet::insert even hot? |
| Honest GC byte-accounting | indirect (pacing), small RSS | none | medium | WEAK | debug recount of true vs charged bytes; abandon if <1.3x |
| RefCell to Cell/qcell token | small (borrow is predicted-free load) | low (large to land) | large | WEAK | cargo-asm get_short_str: count borrow-flag instrs |
| Symbol(u32) table keys | ~nil (hash already cached, ptr_eq already wins) | none | medium | REJECT | cargo-asm get_str_value: confirm hash is a cached load |
| Superinstructions / opcode fusion | ~1.3x lit ceiling, less here | none | large | REJECT | count adjacent-opcode-pair frequency; sample for dispatch in top frames |
| Bounds-check window slicing | ~0 (get_unchecked showed zero) | none | medium | REJECT | cargo-asm hot arms: are there any bounds branches to remove? |

### STRONG candidates

**GC visited-set FxHasher swap.** `Marker.visited` (`heap.rs:343/350`) uses SipHash on box-address keys that are never attacker-controlled, so HashDoS resistance buys nothing; FxHasher is strictly cheaper. Inline a self-contained FxHasher rather than adding a dependency (lua-gc currently has zero deps). The win is narrow: ~3.5% on binary_trees, neutral on compute, zero on the geomean. Decouple the hasher swap from any `with_capacity` pre-reserve decision (a 65536-slot pre-reserve every cycle can erase the gain on small heaps); A/B the pre-reserve separately. Gate correctness with the GC canaries. If the binary_trees delta is under ~2% at -j1 best-of-5, drop it and do the two-white flip instead.

**Inline tiny-table storage.** `TableInner` (`crates/lua-types/src/table.rs:231`) heap-allocates `array` and `node` Vecs separately from the `GcBox<LuaTable>`. A `SmallVec<[_; N]>` swap keeps all the index-heavy algorithms working under safe slice indexing, cuts malloc count and RSS and locality at once, and is something luars lacks (his #37). Do not start the rewrite blind. First add a small `dhat-heap` feature and measure binarytrees: if allocations are GC-box-dominated rather than buffer-dominated, this lever barely helps (the box stays one alloc) and you should reject. The "~1250 bytes/tiny-table" figure is unverified and likely high; dhat settles it. If buffers dominate, do a throwaway `SmallVec` spike, run `cargo test -p lua-types` plus `nextvar.lua`/`gc.lua` canaries (the `next()` iteration order and rehash boundaries are correctness-sensitive), then `make perf` binarytrees. Frame the win as malloc/RSS/locality, not GC scan cost (the trace path still walks every element). Promote only on a measured RSS drop with green canaries.

**Tooling (asm half).** `cargo-show-asm` is native, cheap, and inspects exactly the artifact every dispatch decision rests on. Adopt it now. Defer iai-callgrind to a CI-only Linux gate as described in section 5, and drop any framing that it "unblocks all candidates", the alloc/GC/RSS levers are better served by dhat, and the dispatch experiment runs natively in seconds via the existing wall-clock path plus the proto generator.

## 7. Recommended first sprint

A concrete three-item ordered sequence, leading with a safe win and a deterministic measurement so iteration is tight.

1. **Adopt `cargo-show-asm` and disassemble `execute` (half a day, safe, deterministic).** `cargo install cargo-show-asm`; `cargo asm -p lua-vm --rust 'lua_vm::vm::execute'` on the arm64 release build. Count indirect-branch sites and confirm whether `i.opcode()` and the dispatch `match` lower to one jump table each. This is the cheapest possible settling of the dispatch question and produces a static artifact that needs no benchmark. **Decision:** one collapsed dispatch site means the threaded-dispatch lever has real (if modest) headroom; roughly one site per opcode means LLVM already tail-duplicated and the lever is largely spent. Either way you stop guessing.

2. **Land the GC FxHasher swap as a measured spike (one day, safe, small but real).** Swap `HashSet::new()` at `heap.rs:350` to an inlined FxHasher build, no pre-reserve. Measure with the workload the gain is claimed on: `compare_luars.sh` binary_trees and the gc_pressure path, best-of-5, vs current main, on the machine the 3.5% was measured on. Run the GC canaries. This banks a safe win and, more importantly, exercises the measurement loop end-to-end before any large refactor. Keep it only if the binary_trees delta clears ~2% at -j1; otherwise drop and pivot the GC effort to the two-white flip.

3. **dhat-gate the inline tiny-table decision (one to two days, safe, sets up the big win).** Add a `dhat-heap` cargo feature to lua-cli, run binarytrees and gc_pressure, and bucket allocations by call site. The single decisive number is the ratio of `GcBox` allocations to `array`/`node`/`Rc<[u8]>` buffer allocations. If buffers dominate, the inline-storage lever is the highest-leverage safe win in the backlog and you proceed to the `SmallVec` spike with the canary suite as the guardrail. If GC boxes dominate, inline storage barely helps and the slab (with its `Sized`/unsafe caveats) is the only allocation lever left, which reframes the whole memory campaign. Either outcome is a hard decision made on a deterministic measurement, which is the point.

The through-line: every step leads with a safe change and a reproducible measurement, and explicitly defers the contested ~1.8x dispatch rewrite until the cheap native asm view and a re-run of the homogeneous-vs-cycle experiment have either confirmed or killed the premise.
