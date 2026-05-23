# Performance principles for lua-rs

Adapted from the redis-rs-port hotpath methodology
(`redis-rs-port/docs/RUNTIME_OWNER_HOTPATH_PUSH.md`,
`redis-rs-port/docs/BENCHMARKS.md`,
`redis-rs-port/docs/history/HARNESS_LEARNINGS.md`). Same discipline, adapted
for an interpreter rather than a network server.

The headline rule: **perf work is evidence-driven, not intuition-driven.**
Every commit that claims to improve performance has to cite an artifact
that proves it. The dashboard at `harness/bench/history/index.html` is the
audit trail.

## The reference is the floor

Upstream Lua 5.4.7 is a 30-year-old, heavily micro-optimized C
interpreter. We will not beat it on raw throughput without a JIT or new
unsafe. **The goal is to close the gap to a normal interpreter ratio**
(2–8× slower), and to detect regressions early when we drift.

Today's per-workload ratios (best-of-N, Apple M3 Max, post 7682720):

| workload    | wall ratio | category |
|-------------|-----------:|----------|
| mandelbrot  | 2.25× | float math + nested loops, near floor |
| binarytrees | 3.23× | GC pressure under steady allocation |
| closure_ops | 4.06× | closure allocation + upvalue access |
| fibonacci   | 5.25× | pure call dispatch + small-integer math |
| table_ops   | 5.40× | table insert/remove/iterate, array+hash mix |
| string_ops  | 10×   | string concat / find / gsub / byte access |

The shape of `mandelbrot ≈ 2×` tells us a tight numeric loop with no
allocation runs within striking distance of C. The other ratios are
mostly opportunities, not laws.

## The gate

Every performance change goes through the same 4-step verification.
Refusal to honor this gate is the single biggest source of perf-work
self-deception:

```text
implementation
    ↓
44/44 official suite green (correctness wall)
    ↓
harness/bench/compare.sh shows the targeted workload improves
    ↓
profile artifact under harness/bench/profiles/ shows the previous top
frame is no longer top, or has shrunk by the predicted amount
```

If any step fails, the commit doesn't land. Profiling data that
contradicts the hypothesis is the most valuable data you have — it
means the hypothesis was wrong, and the fix would have been a coincidence.

## The packet shape (when filing perf work)

Borrowed unmodified from the redis hotpath methodology. Every perf
investigation should be written up in this shape, even when you're doing
it solo in one session:

1. **Hypothesis.** One sentence: what we think the bug is. Link the
   profile artifact that suggests it.
2. **Source anchors.** Reference C lines + current Rust lines, by path
   and line number.
3. **Allowed ideas.** A bounded list of legal interventions.
4. **Forbidden.** What you must *not* do — almost always: no benchmark
   special-cases, no removed safety, no skipped semantic correctness,
   no new `unsafe`.
5. **Gate.** The 4-step verification chain above.
6. **Evidence after.** Link the artifact that shows the fix worked.

A commit that lands a hotpath fix should mention the before/after ratio
in its message and link the artifacts.

## Hard rules (binding non-goals)

These apply to *every* perf-shaped commit:

- **44/44 must stay green.** If the official suite regresses, the
  commit doesn't land.
- **No benchmark-only fast paths.** If the workload triggers a code
  path no real Lua program would, the optimization is fraudulent. The
  workload exists to measure that path; bypassing it is lying.
- **No skipped semantic correctness.** No skipped metamethod dispatch,
  no skipped GC barriers, no skipped error checks. The fix is always
  "do less work when the work is provably unnecessary," never "do the
  same work less correctly."
- **No new `unsafe`** outside `lua-gc` / `lua-coro` (workspace default
  is `unsafe_code = "forbid"`). A hotpath fix that needs raw pointer
  arithmetic is a different architectural conversation.
- **No `String` for Lua data.** Use `&[u8]` / `Vec<u8>` / `LuaString`.
  The string library is byte-oriented and our impl must be too.
- **GC canaries must stay green.** `./harness/canaries/gc/run_canaries.sh`
  before and after any change to GC-touching code.

## What good fixes look like

These are the shapes that have worked, in this port and in the redis
sibling:

### 1. Don't disable a feature; cache its precondition

When a feature has overhead even in the inactive case (slowlog, WATCH,
line hooks, GC barriers, metatable lookup), the fix is a cheap
precondition check, not a feature flag.

Example (redis): `slowlog-log-slower-than` and `slowlog-max-len` get
cached into a `LiveConfig` predicate. Dispatch still routes every
command through the normal handler but skips the duration timer when
the gate says nothing will be recorded.

Example (lua, applied 7682720): `#t` was calling `get_int(i)` in a
loop. The canonical `LuaTable::getn()` already implements C-Lua's
`luaH_getn` (alimit + binary search). The "feature" — supporting
arbitrary integer keys past `alimit` — wasn't disabled. It's still
correct. The hot path just stopped paying for it when alimit alone
sufficed.

### 2. Match upstream's structure, not just upstream's behavior

C-Lua's `lua_geti` doesn't go through the same code path as
`lua_gettable`. It has a direct integer-key fast path. If our
`table_get_i` always routes through the generic `table_get_with_tm`
that handles arbitrary keys + metamethods, we pay the generic-path
overhead on every integer lookup, even when the table has no
metatable. **Solution:** add an integer-key fast path that
short-circuits when the table is metamethod-free for `__index`.

A second flavor of this pattern: C does work IMPLICITLY that we do
explicitly. C-Lua's `lua_geti(L, 1, n)` resolves stack index 1 with a
single pointer arithmetic op (`ci->func.p + idx`) folded into the
load — never materializes a "resolution function call." Our
`index_to_value(state, idx)` is a real function with branches for
positive, negative, pseudo, and upvalue indices. **In a shift loop
over an N-element array we re-resolved the same stack slot N times.**
The fix is to resolve ONCE at the top of the calling function (e.g.
`table.remove`) and pass the resolved `&LuaValue` through a sibling
method that skips the resolution.

Example (lua, applied f179afb): `table.remove(arr, pos)` now calls
`state.value_at(1)` once, then uses `table_get_i_value` /
`table_set_i_value` in the shift loop body. The new methods take the
resolved table directly. `table_ops_long` ratio dropped from 4.76x
to 4.02x — a 15% workload-level speedup from skipping the per-iteration
resolution.

The general lesson: **if the C version's runtime cost is "essentially
free" relative to ours, the question to ask is "what does C do
implicitly that we do explicitly?"** The fix is to hoist or fold the
implicit work, not to remove it.

### 3. Avoid allocation on the hot path

Every `Vec::new()`, `to_vec()`, `clone()`, `Box::new()` in an inner
loop is a potential pole. Look for them in the profile. Common
offenders in interpreters:

- Building error message vectors that are never read on the happy path
- Boxing values that don't need indirection
- Cloning `Rc`/`Arc` when the borrow would suffice
- `Vec::push` into a growing collection — use `Vec::with_capacity` if
  the size is known
- Constructing `String` for type-name lookups (PERF(port) callout in
  `tagmethods.rs:328`)

**The hidden-allocation-in-helper pattern.** Watch out for innocuous-
looking helpers that secretly allocate. Example (lua, fixed 3190288):
`check_arg_string(idx)` returned `Vec<u8>` — calling code treated it
as "argument coercion + tiny validation," but every call did
`as_bytes().to_vec()` of the full source string. For `string.byte(s, i)`
in a tight loop (e.g. iterating every byte of a 14 KB string), that's
14000 calls × 14000-byte copies = ~200 MB of allocator churn per
iteration — and the workload runs the loop 50 times. The fix was to
hold a `GcRef<LuaString>` (one-pointer Copy) and borrow `.as_bytes()`,
so the bytes stay heap-resident on the GC and the helper just adds a
type check. **`string_ops_long` dropped from 2.25× to 1.58×** — first
workload to fall below the 1.5× parity threshold. Max RSS for the
workload also dropped from 116 MB to 7 MB.

The general lesson: when a function is called N times in an inner loop,
*every* allocation it does internally is multiplied by N. Cheap-looking
one-liners (`x.to_vec()`, `format!(...)`, `Vec::new()`) deserve as much
profile scrutiny as explicit `Box::new` constructions.

### 4. Inline-friendly fast paths

If the inner loop is a method call that ends in a `match` on a tag,
profile to see whether LLVM can inline through it. If the function is
too large or crosses crate boundaries, `#[inline]` may help; if the
match is too wide, splitting the cold cases into a separate function
(`#[cold]` + `#[inline(never)]`) often does.

C-Lua's `vmcase`/`vmbreak` macros plus its `OP_GETI` / `OP_SETI`
opcodes are the bytecode-level expression of this discipline.

**The negative-result variant: clones aren't always the cost.** Example
(lua, da9401e): we suspected `LuaValue::Clone` in the arith opcodes
was a real cost — every `OP_ADD` cloned two operands to satisfy the
borrow checker. We refactored to use primitive-tag accessors
(`get_int_at` returns `Option<i64>`, no enum clone). **Result:
mandelbrot improved (2.12× → 2.00×) but fibonacci was essentially
unchanged** — the LLVM-inlined 24-byte enum copy was not material.
The real fibonacci bottleneck is the dispatch machinery (precall,
upvalue_get, instruction decode). Profile evidence beats hypothesis;
sometimes the hypothesis was wrong about magnitude even when the
direction is right.

The fix landed anyway because (a) it's a structural improvement
matching C-Lua's `op_arith_aux` shape, (b) float-heavy workloads
benefit, (c) the new primitive accessors enable follow-up fixes on
the ORDER/BITWISE opcodes. But the bench-driven discipline matters:
we'd otherwise have claimed "fixed the fibonacci arith clones" when
the truth is "fixed mandelbrot incidentally, fibonacci was elsewhere."

### 5. Reference-counted values: clone-vs-borrow

`LuaValue` carries `GcRef<T>` for tables / strings / closures. Cloning
that ref is cheap (one refcount bump) but not free. In tight loops,
prefer borrowing through `&LuaValue` or `&GcRef<T>` if the lifetime
allows. Profile to see whether `Drop`/`Clone` for `LuaValue` is
showing up.

## Profile discipline

- **Wall-clock sampling ≠ CPU profiling.** `/usr/bin/sample` and the
  Activity Monitor's "Sample Process" both record stacks of *running*
  threads at intervals, including time spent in syscalls and I/O. For
  pure CPU attribution, use `xctrace` Time Profiler. We use `sample`
  by default because it's universally available and good enough for
  finding the first 80% of hotspots.

- **Build with frame pointers + debug symbols** before profiling:

  ```bash
  CARGO_PROFILE_RELEASE_DEBUG=true \
  RUSTFLAGS="-C force-frame-pointers=yes" \
    cargo build --release -p lua-cli
  ```

  Without these, symbols are missing or wrong and the sample output is
  useless.

- **Sample long-running workloads.** A 0.18-second workload (mandelbrot)
  doesn't sample well. Loop the workload N times or use a workload that
  runs at least 5 seconds. fibonacci@13s is ideal.

- **Probes vs ledgered profiles.** Quick exploration runs go to
  `harness/bench/profiles/<UTC>-<sha>-<workload>/` and are gitignored —
  they're telemetry, not evidence. Profile artifacts that motivate a
  commit get linked from the commit message.

## What about JIT?

Out of scope. The lua-rs goal is *safe Rust*, no JIT, no tracing
compiler, no `unsafe` shortcuts. If/when we want LuaJIT-grade speed,
that's a separate project with a separate name. The dashboard's
purpose is to show how close we can get *without* a JIT, and to catch
regressions when normal porting work makes a hot path slower by accident.

## Workflow checklist

When starting a perf push:

1. Run `bash harness/bench/compare.sh` to capture the current state.
   This appends a ledger row at the current commit, which becomes the
   "before" datapoint on the dashboard.
2. Identify the workload to attack (typically the tallest pole in the
   current dashboard).
3. Build the profile-friendly binary (`force-frame-pointers=yes`).
4. Run `bash harness/bench/profile-hotspots.sh <workload>` to capture
   a wall-clock sample. Read the top frames.
5. Form a hypothesis. Write it down (commit message body is fine).
6. Apply *one* change. Smaller is better.
7. `cargo build --release -p lua-cli`. Run the 44/44 suite.
8. Run `bash harness/bench/compare.sh` again. Confirm the workload
   moved, others didn't regress.
9. Re-sample the workload. Confirm the previous top frame shrank.
10. Commit, referencing the before/after artifacts. Push.
11. Rebuild dashboard (`python3 harness/bench/history.py`). The new
    datapoint should appear.

If any step fails: that's the data. Move on or rethink. Don't paper over
a contradiction by tweaking the workload until it agrees with the fix.
