# Matching C performance in a safe-Rust port

A working journal of what we've learned (and what we expect) about closing the
gap between a faithful, line-by-line C → Rust port and the original C
implementation's speed. Written against `lua-rs-port` but the lessons are
intended to be portable to any "translate a mature C systems program into safe
Rust" project (Valkey, nginx, …).

Read alongside `PERFORMANCE_PRINCIPLES.md`. That doc is the *playbook* for
ongoing perf work in this repo (evidence-driven, gate-driven, dashboard-driven).
This doc is the *research journal* — what we learned about why the gap exists
in the first place, what shape the fixes take, and what's on the other side of
the interpreter (a future JIT).

## The framing

lua-rs is, at the most honest level, an **experiment in safe-Rust C porting at
scale**. We picked Lua 5.4 because:

- The source is small enough to port end to end (~28k LoC of C).
- The test suite is a hard, externally-verifiable oracle (the PUC-Rio official
  tests).
- It runs real software (LuaRocks 3.11.1) so we know "done" when we see it.
- Reference performance is a single, faithful interpreter binary. No CDN, no
  network, no syscall noise. Just `target/release/lua-rs my_workload.lua`
  vs `reference/lua-5.4.7/src/lua my_workload.lua`. Numbers are unambiguous.

The harness work (typed work-packets, ledgered evidence, dashboards,
chassis-driven dispatch) is the *real* product. The port is how we earn the
right to claim "this methodology produces working safe-Rust ports of C
programs at performance parity." Each commit is a data point in that claim.

If you're picking up this doc later: the answers below were earned at the
cost of profiling sessions and bench commits. Use them. Don't re-derive.

## The headline finding

**Most of the performance gap between a faithful safe-Rust port and the
original C is not a safety tax. It's a Rust-idiom tax.**

When we first benchmarked lua-rs against reference Lua 5.4.7, the overall
wall-clock ratio was something like 5× slower on a microbench mix (and worse
on individual workloads — a 10,473× table-ops outlier that turned out to be a
missed fast path, not a quadratic algorithm). After two surgical fixes
(`#t` boundary search + GC snapshot fast-path) the overall ratio dropped to
2.61× without changing the algorithms or touching `unsafe_code = "forbid"`.

Splitting that remaining 2.61× into honest components:

| Source of gap | Estimated contribution | Recoverable without `unsafe`? |
|---|---|---|
| Genuine safety cost (bounds checks Rust can't elide, `RefCell` borrow checks on hot fields, `GcRef` indirection) | ~1.10–1.20× | mostly no |
| `LuaValue` is 24 bytes (tagged enum) vs C's 16-byte NaN-boxed `TValue` | ~1.05–1.10× | only with `unsafe` |
| Match-dispatch loop vs computed-goto threaded dispatch | ~1.05–1.10× | partial (cold-arm split, `#[inline(never)]` on rare opcodes) |
| **Clones where C uses pointers** — `state.get_at(i).clone()` to drop a borrow before passing `&mut state` to a helper | **~1.20–1.40×** | **yes, with careful refactoring** |
| **`RefCell` on the hot path** — `state.global.borrow()` per opcode, `RefCell<UpValState>` per upvalue read | **~1.10–1.20×** | **yes, restructure access patterns** |
| **Allocations in cold-but-frequent code** — e.g. the GC snapshot loop we just fixed | **~1.10–1.30×** | **yes** |
| **Accessor methods that should be inlined field reads** — `state.get_at`, `state.ci_base`, `Instruction::opcode` | **~1.05×** | **yes** |

The bolded rows are the bulk of the remaining gap, and **none of them are
safety taxes**. They are choices we made to satisfy the borrow checker or to
follow idiomatic Rust without thinking carefully about hot paths.

**The goal is parity (~1.0×), not "close enough."**

The sibling project `redis-rs-port` reached full performance parity with
upstream Valkey through exactly the kind of disciplined hot-path work
described in this doc. There is no fundamental reason — no law of physics,
no inherent Rust limitation — why a faithful safe-Rust port of an
interpreter cannot reach the same place. Every unit of the remaining
2.5× gap has a recoverable cause; the work is to find and fix them one
at a time.

What's different for an interpreter vs. a server like Valkey:

- **Valkey** spends most of its wall-clock in I/O and syscalls (epoll,
  socket reads/writes, packet parsing). The command-handling logic is a
  small fraction of total time. Hot-path optimization there means
  making the small CPU portion fast enough that I/O dominates again, at
  which point parity is the natural state.
- **Lua** is pure CPU. Every nanosecond of interpreter overhead is
  naked CPU time — there's no I/O layer to hide behind. So our
  remaining 2.5× *is* the bottleneck, by definition.

That makes the work harder per unit of improvement, but it doesn't move
the floor. The same disciplined "find a hot-path missed fast path, fix
it surgically" pattern that got redis-rs-port to parity will get us there
too. Earlier versions of this doc claimed "1.3–1.5× is the realistic
floor" — that was a guess, not a measured limit. Discard it.

**The last 1.0–1.2× of the gap** may require carefully-scoped `unsafe`
(NaN-boxed value representation, `get_unchecked` on the stack, possibly
`become` for tail-call dispatch when stabilized). Each of those would be
a deliberate, well-contained `unsafe` block with its own correctness
argument, not a wholesale departure from the `unsafe_code = "forbid"`
default. `redis-rs-port` accepts the same trade-off in its network
buffer and FFI code without compromising the overall safety story.

## What we've actually shipped (evidence)

Three commits this session, each with a `compare.sh` ledger row that shows
up on the dashboard:

### 1. `7682720` — `#t` boundary search via canonical `LuaTable::getn()`

**Pattern:** *Use the fast path that already exists.*

`state.table_length` was hand-rolling an O(n) probe loop that called
`tbl.get_int(i)` until nil. But `LuaTable` (in `lua-types`) already had a
proper C-Lua-style `getn()` using `alimit` + binary search, O(log n). The
slow caller bypassed it. 4-line fix; `table_ops` dropped from 10,473× to
5.4×, and three other workloads (`closure_ops`, `string_ops`) also moved
because they exercised the same path indirectly.

The forensics that found it: a `/tmp/probe_table.lua` micro-isolation
(table.remove only, no math.random) showed pure-table-remove was 6,348×
slower than C-Lua. That ratio implied "single hot function is doing way too
much work," not "wrong algorithm."

### 2. `36a3bbe` — GC fast-path: skip snapshot allocations when no step would fire

**Pattern:** *Cache the precondition, don't disable the feature.*

`state::GcHandle::collect_via_heap` was building 5+ `Vec`/`HashSet`/`RefCell`
allocations on every call — *even when the heap's `step()` would short-circuit
because `bytes_used < threshold`*. The threshold check happened *inside* the
heap, after we had already paid the snapshot allocation cost. Since
`gc_check_step` runs from `precall` on every Lua function call, fibonacci's
~10M calls created a continuous storm of `malloc`/`free` that the OS allocator
dominated profiles with.

A `/usr/bin/sample` profile (`harness/bench/profiles/...615e197-fibonacci/`)
showed the smoking gun directly: 30% of wall-clock in `_nanov2_free` and
`nanov2_malloc_type`, all reached through `collect_via_heap` (NOT user-code
allocation). The call graph made the path unambiguous.

Fix: add `Heap::would_collect()` predicate; bail out of `collect_via_heap`
before the snapshot block when `force=false && !would_collect()`.

Bench impact (best-of-5):

| workload | before | after | speedup |
|---|---|---|---|
| binarytrees | 3.23× | 1.80× | 1.8× |
| closure_ops | 4.06× | 2.28× | 1.8× |
| fibonacci | 5.25× | 2.71× | 1.9× |
| string_ops | 10.00× | 8.00× | 1.25× |
| overall | 4.85× | 2.61× | **1.86× geomean** |

One commit. One ledger row. Three workloads ~halved their gap to C.

### 3. `98f5e50` — `#[inline]` on bytecode-dispatch accessors

**Pattern:** *Inline single-instruction-equivalent functions on the hot path.*

`Instruction::opcode()` is a 83-arm match on the low 7 bits of the
instruction word. Without `#[inline]`, it was showing 8.4% of fibonacci's
post-fix profile as its own frame — i.e., LLVM was emitting an actual function
call per dispatched instruction. Same for `arg_a()`/`arg_b()`/`arg_c()` etc.,
each of which is a single shift+mask.

Adding `#[inline]` to all thirteen accessors collapsed them into the
dispatch loop where LLVM can fold them into the surrounding bytecode-decode
code. Bench impact marginal (2.61× → 2.60×) because LLVM was already inlining
some via LTO, but it's the right hygiene and shows up in the profile
re-sample: `Instruction::opcode` dropped from a top-5 frame to invisible.

## The patterns by name

Distilled from the three commits above, plus the redis-rs-port hotpath
methodology we adapted:

### Pattern 1: Use the fast path that already exists

C codebases routinely have two implementations of the same operation: a
generic one for the rare case (e.g. `lua_gettable` for any key type with
metamethod support) and a specialized one for the common case (e.g.
`luaH_getint` for raw integer-key array access). A naive Rust port often
routes everything through the generic path, paying generic-path overhead
on every call.

**The fix is to find the spot in the C source where the optimization happens
and replicate that structure, not the surface API.**

### Pattern 2: Cache the precondition, don't disable the feature

When a feature has overhead even in the inactive case (GC step, slowlog,
WATCH, line hooks, metatable lookups), the fix is a cheap precondition check
that bails *before* the expensive prep work. NOT a feature flag.

The trap shape: the inactive case eventually does early-return inside the
feature's main function, but only AFTER snapshot/prep allocations have
already fired. Move the check up.

### Pattern 3: Match upstream's structure, not just upstream's behavior

This is the meta-pattern. If C-Lua's `lua_geti` has an integer-key fast path
that skips metamethod handling, our `table_get_i` must have one too. If
C-Lua's `vmdispatch` macro inlines the opcode fetch into the loop body, our
match arm must not call into a separate function for it.

A faithful port matches structure. The benchmark gap *is* the surface area
where the port chose a different structure than C-Lua. Profiles reveal
exactly which differences are paying off and which are pure waste.

### Pattern 4: Don't clone where C uses pointers

C-Lua's hot opcode handlers operate on `TValue *` — pointers into the stack.
Our naive port writes:

```rust
let v1 = state.get_at(rb).clone();   // drops the borrow on `state`
let v2 = state.get_at(rc).clone();
helper(state, &v1, &v2, ...);        // pass &mut state + immutable refs
```

We clone because the borrow checker will not let us hold `&LuaValue` into
`state` while also passing `&mut state` to a helper. **But the clone is not
required by safety — it's required by the *shape* we chose for the helper.**

The fix is to either:

- inline the operation into the match arm so no helper is called;
- pass indices instead of values, and have the helper re-read;
- extract just the needed primitive (`i64`, `f64`) without cloning the
  enum;
- split-borrow via dedicated reader methods that return owned primitives.

None require `unsafe`. All require thinking carefully about hot-path
shapes.

### Pattern 5: Inline-friendly accessor discipline

Single-instruction operations (bit-shifts, masks, enum tag reads) should
NOT be function calls on the hot path. `#[inline]` is the cheap fix; sometimes
it's not enough and you have to flatten the abstraction.

### Pattern 6: Wall-clock sampling beats hypothesizing

Every meaningful win in this session came from a `/usr/bin/sample` profile
artifact. The hypothesis "table_ops is slow because it's quadratic" was
wrong (it was a missed fast path). The hypothesis "fibonacci is slow because
of call dispatch" was wrong (it was malloc churn inside the GC). Without
profile data, both fixes would have aimed at the wrong target.

The redis-rs-port discipline: every perf-shaped commit links the profile
artifact that motivated it. We follow the same rule.

## What requires `unsafe` (the last increment to parity)

The list below is what we can NOT recover purely in safe Rust. Each is a
candidate for carefully-scoped `unsafe` once the safe-Rust work plateaus.
None of these are blockers — the bulk of the gap is recoverable without
them, as the redis-rs-port precedent shows. They become relevant for the
final 1.0–1.2× push toward parity.

- **NaN-boxing the value representation.** C-Lua's `TValue` is 16 bytes;
  pointers and small ints are encoded in NaN bit patterns of a 64-bit
  float. Our `LuaValue` is a Rust tagged enum at ~24 bytes. Cloning is
  ~50% more expensive. Recovering this would require `union` + transmutes —
  it's possible in `unsafe`, but you'd be implementing a custom tagged
  pointer type and validating it via miri/loom.

- **Raw pointer stack access.** C-Lua reads `s2v(L->top.p - 1)` as a single
  pointer load. Our `state.get_at(idx)` indexes a `Vec<StackValue>` with
  whatever bounds-check LLVM can or can't elide. Profile evidence so far
  suggests this is ~5% overhead, not the dominant cost.

- **Computed gotos / true threaded dispatch.** C-Lua's `vmdispatch` macro
  emits `goto *jump_table[opcode]` after each opcode. There is no Rust
  equivalent in safe code. LLVM compiles our `match` to a similar jump
  table, but the loop header overhead is real — ~5–10%.

- **Tail-call optimization for the dispatcher.** A common pattern in
  hand-written interpreters (LuaJIT, V8 baseline) is to make each opcode a
  function that tail-calls the next. Rust does not guarantee TCO; the
  pattern is unreliable. Some projects use `become` (experimental) or
  `musttail` (LLVM attribute, not exposed) but those require nightly.

Combined, the unsafe-only opportunities account for maybe 1.15–1.25× of
additional speedup. **That's the difference between "well within striking
distance of parity" and "actually at parity."** Worth doing once we've
exhausted the safe-Rust hot-path work — same trade-off redis-rs-port
made for its network buffer and FFI code paths.

## Future: where a JIT would fit

The interpreter we're porting *isn't a stopping point*. It's the substrate
for a future JIT.

### The pattern

Every mature dynamic-language JIT keeps the interpreter alive and adds a
JIT alongside it. The interpreter remains the canonical execution path
and the deopt target; the JIT is an *optimization that can fail* (guard
violations) and fall back to the interpreter.

```
                  ┌──────────────────────────────┐
parse → bytecode  │                              │
       │          │  Interpreter (lua-rs today)  │
       ├──────────►  - canonical execution       │
       │          │  - deopt target              │
       │          │  - cold code stays here      │
       │          └──────────────────────────────┘
       │                       ▲
       │                       │ deopt on guard failure
       │                       │
       │          ┌────────────┴─────────────────┐
       │          │  Tracer / profiler           │
       └──────────►  - watch interpreter         │
                  │  - identify hot loops/funcs  │
                  └──────────┬───────────────────┘
                             ▼
                  ┌──────────────────────────────┐
                  │  JIT compiler                │
                  │  - bytecode/trace → IR       │
                  │  - IR → native code          │
                  │  - emit type guards          │
                  └──────────┬───────────────────┘
                             ▼
                  ┌──────────────────────────────┐
                  │  Native code cache           │
                  │  - executable pages          │
                  │  - per-trace/method          │
                  └──────────────────────────────┘
```

### Prior art (read these before designing)

- **YJIT** — Ruby's JIT, written in Rust, in production at Shopify and
  GitHub since 2021. Closest analog to "Rust JIT bolted onto an existing
  C-ported interpreter." ~30–50% speedup over MRI's interpreter.
  Architecturally a basic block / method JIT with custom assembler.
- **LuaJIT** (Mike Pall) — tracing JIT for Lua 5.1 in hand-written x86/ARM64
  assembly. The reference for "fast Lua." Architectural template; we
  wouldn't recreate the hand-assembly approach.
- **Wasmtime + Cranelift** — production codegen library used by Wasmtime,
  Wasmer, others. The realistic Rust-native option for emitting native
  code from typed IR. Would be the codegen backend for a lua-jit project.
- **PyPy** — meta-tracing JIT for Python. Different philosophy (generates
  the JIT from the interpreter spec) but proves the substrate pattern.
- **MoarVM** — VM for Raku with JIT. Less directly relevant but the design
  notes are educational.

### Three tiers of ambition

**Tier 1: method JIT via Cranelift (realistic 6–12 month project).**
Compile whole Lua functions to native code via Cranelift's SSA IR. Emit
type guards (e.g. "this register is `Int`") that deopt to the interpreter
on failure. Expected 2–5× over interpreter on numeric/call-heavy code.
Cranelift is real production codegen — you don't write your own
assembler.

**Tier 2: tracing JIT (LuaJIT-grade, multi-year).** Record traces through
hot loops, compile linear bytecode sequences with type specialization,
support side exits. Where the 10–50× numbers come from. Substantially
harder — trace assembly, deopt at arbitrary points, type-narrowing
inference.

**Tier 3: template JIT / baseline compiler (3–6 months as a first step).**
Compile each bytecode opcode to a small native template, stitch them
together, no type specialization. Maybe 1.5–2× over interpreter. What YJIT
started as. A good warm-up before Tier 1.

### What our current choices buy us toward this (good)

- **Faithful Lua 5.4 bytecode preserved.** Register-based bytecode maps
  cleanly to SSA IR (Cranelift, LLVM). Stack-based bytecodes are harder.
  We're already in the easy regime.
- **Deterministic interpreter, no hidden state.** Deopt requires
  reconstructing interpreter state from JIT state. Clean state model →
  tractable deopt.
- **Mark-and-sweep GC with explicit roots via `Trace`.** A JIT will need
  to emit stack maps (metadata telling the GC where references live in
  JIT'd frames at each safepoint). Our `Trace` discipline is a clean
  abstraction to extend. Harder GCs (concurrent, compacting) would be
  much harder.
- **`unsafe_code = "forbid"` in interpreter doesn't constrain the JIT.**
  The JIT itself will use `unsafe` (executable pages, calling convention
  juggling) but that's contained in a future `lua-jit/` crate.

### What might cause friction later (worth designing for now)

- **`LuaValue` as a 24-byte tagged enum.** A JIT wants to specialize
  register representation — keep values in CPU registers as raw `i64` /
  `f64`, only "boxing" back into `LuaValue` at safepoints. Not blocking,
  but worth knowing.
- **`RefCell`-wrapped state fields.** A JIT can't `borrow_mut` a RefCell
  from native code easily; it needs raw pointer access. Refactoring some
  state for JIT-friendliness is a real cost.
- **`GcRef` indirection layers.** A JIT wants to load a table's field
  with one `mov rax, [rdi+offset]`. If our `GcRef` has multiple layers of
  indirection, the inlined access path gets messy. Keep the `GcRef`
  abstraction thin.

### Three things to bake in NOW (cheap to do, expensive to undo)

1. **Safepoints are explicit.** GC only runs at known points. Our
   `gc_check_step` from `precall` is essentially the safepoint
   mechanism — keep that pattern. A future JIT will emit explicit
   safepoint checks at loop back-edges and call sites; the interpreter's
   safepoint discipline is the model.

2. **Value type tests are local, not metatable-traversing.** If
   `is_integer(v)` reads a single tag, the JIT can emit a single
   compare-and-branch. If it walks metatables, the JIT has to bail. Keep
   type tests cheap.

3. **No interpreter logic the JIT can't replicate.** If the interpreter's
   behavior depends on "what data structure is currently checked out via
   RefCell," the JIT will struggle. Stick to "input state → output state"
   semantics in the dispatch loop.

We are already doing all three by accident, because faithful C-Lua porting
demands it. Worth noticing so we don't lose it.

## Distilled rules for the next C → Rust port (the takeaway)

If you are starting another systems-level C → Rust port and want to hit
performance parity (or close), the lessons from this one in priority order:

1. **Build the bench loop before you need it.** A working `compare.sh`
   that puts a ratio number on the dashboard for every commit is the
   single most valuable piece of infrastructure. Without it, you'll
   optimize the wrong thing.

2. **Build the profile harness before you need it.** A `/usr/bin/sample`
   wrapper that captures the top frames in 10 seconds is the difference
   between "informed surgery" and "lucky guess." See
   `harness/bench/profile-hotspots.sh`.

3. **Port faithfully first, then optimize.** A working faithful port is
   the substrate. Out-of-the-box ratios of 5–10× are NORMAL. They are not
   the verdict. They are the starting point.

4. **Most gaps are non-faithful Rust idioms, not Rust safety.** Clones
   instead of pointers, `RefCell` instead of direct field access,
   allocations in cold-frequent code, accessor methods that should be
   inlined. The taxonomy table at the top of this doc applies.

5. **Profile. Don't guess.** The two big wins this session both came
   from profile evidence pointing at things we wouldn't have suspected
   (a missed fast path on `getn`, GC bookkeeping allocations). Hand
   inspection of the code suggested different targets.

6. **"Use the fast path that already exists" is the highest-leverage
   pattern.** C codebases have two impls of everything: generic for the
   rare case, specialized for the common one. Routing everything through
   the generic path is the default Rust port mistake.

7. **"Cache the precondition" is the second.** Features that short-circuit
   on inactive cases need the short-circuit at the outer layer, not
   inside.

8. **Every commit cites its evidence.** Profile artifact path in the
   commit body. Dashboard row showing before/after. 44/44 oracle test
   gate. No exceptions, even on "obvious" fixes — the GC fix looked
   obvious AFTER the profile said so.

9. **The interpreter is the substrate, not the endpoint.** Plan for a
   future JIT — keep safepoints explicit, type tests cheap, semantics
   clean. None of this costs anything if the C source you're porting
   already does it.

10. **`unsafe_code = "forbid"` is not the bottleneck. Sloppy idioms are.**
    The gap you should worry about is the one between "Rust that
    type-checks" and "Rust that compiles to the same machine code C
    does." That gap is most of the work. Safety is rarely where the
    time goes.

## What goes here next

This doc is a living journal. When a new perf commit lands, the lessons
that generalize should make their way back into "The patterns by name" or
"Distilled rules" above. Things to add as we encounter them:

- The string-library hot path investigation (next session's likely target).
- The arith-clone refactor (when we actually do it).
- The `RefCell`-on-hot-path audit (deeper refactor when ready).
- The first time we add a Cranelift dependency and emit one native opcode
  handler. (No timeline. Tier 3 baseline JIT as a thought experiment.)
- Whatever pattern emerges from running the same playbook on
  `redis-rs-port` and `nginx-rs-port`. If the lessons transfer, the
  methodology is the real product.
