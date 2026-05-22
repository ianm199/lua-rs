# Phase D Migration — Real GC, Per-GlobalState

## Status

**Phase D-0 (skeleton) is committed:**
- `crates/lua-gc/src/heap.rs` — production `Gc<T>`, `Heap`, `Trace`, `Marker`, mark-and-sweep with forward write barrier
- Trace impls for 9 GC-rooted types (`LuaValue`, `LuaString`, `UpVal`, `LuaTable`, `LuaProto`, `LuaLClosure`, `LuaClosure`, `LuaState`, `GlobalState`)
- `Heap` field on `GlobalState`
- `GcHandle::full_collect()` wired to `heap.full_collect(&*global)`
- Smoke tests pass; `collectgarbage("collect")` correctly traces the root set

**What's broken / paper-only:**
- `GcRef::new(...)` is still `Rc::new(...)` (lua-types/src/gc.rs)
- Heap's `allgc` chain is empty — nothing allocates into it
- Sweep finds nothing to free; cycles still leak via Rc
- 462 `GcRef::new` call sites still in place
- One DEBUG agent had to add `Marker::try_visit` as a cycle-guard because Trace recurses via `Deref` on cyclic `_G._G == _G`

## Architectural Decision (Locked)

**One heap per `GlobalState`. Not thread-local.**

Reasoning: Lua's C API explicitly supports multiple independent `lua_State` universes on one OS thread (sandbox-per-state is a real embedding pattern). A thread-local heap would corrupt:

```c
lua_State *L1 = luaL_newstate();
lua_State *L2 = luaL_newstate();   // same thread
collectgarbage("collect");          // on L1
// L2's objects swept because they share the TLS heap
```

So allocation routes through `state` → `state.global` → `state.global.heap`. Same shape as C-Lua's `G(L)` macro.

**TLS is permitted only as a migration crutch** — a scoped `CURRENT_HEAP` thread-local set inside `state.run()` so legacy `GcRef::new` finds the active heap. Removed at end of D-1.

## Soundness Story

`Gc<T>: Copy + Deref` is safe IF AND ONLY IF the safepoint invariant holds:

> No `Gc<T>` may be held across a call that triggers collection.

We enforce this **by construction** in `Heap`:
- `heap.allocate()` is `&self`, never collects, only prepends to allgc.
- `heap.step()` and `heap.full_collect()` are the only collection entry points.
- `step` is called from a small, grep-able set of safepoints: `collectgarbage()`, VM dispatch boundary (planned), at function entry/exit only.

Test invariant: `cargo test gc::no_collect_in_allocate` does 10K allocations and asserts `heap.collections() == 0`.

## The Six-Step Migration

Each step lists: **What** (deliverable), **Static** (work I or scripts do directly), **Agent** (work for agent loop), **DoD** (definition of done), **Risks**.

---

### Phase D-1a — State-owned allocation APIs

**What:** Add `state.alloc_*` methods that wrap `GcRef::new` (for now) and become the canonical allocation surface. Once the bridge is in place, swap their bodies to route through `heap.allocate`.

Six constructor types to add:
- `alloc_string(bytes: &[u8]) -> GcRef<LuaString>`
- `alloc_table() -> GcRef<LuaTable>`
- `alloc_proto() -> GcRef<LuaProto>`
- `alloc_lclosure(proto: GcRef<LuaProto>) -> GcRef<LuaLClosure>`
- `alloc_cclosure(f: LuaCFunction, nupval: usize) -> GcRef<LuaCClosure>`
- `alloc_userdata(size: usize, nuv: usize) -> GcRef<LuaUserData>`
- `alloc_upval(value: LuaValue) -> GcRef<UpVal>`

Plus existing `state.new_table()` and `state.intern_str()` re-route to these.

**Static** (~30 min, I do this):
```bash
# Identify the 462 GcRef::new call sites, classify by what they allocate
grep -rn "GcRef::new(" crates/ | awk -F: '{print $1, $3}' \
  | grep -oE "GcRef::new\([^)]+\)" | sort | uniq -c | sort -rn > sites.txt
# Use sites.txt to confirm the 7 constructor categories above cover ~95%
```

Add the 7 method signatures + thin-wrapper bodies (delegating to `GcRef::new` for now) to `crates/lua-vm/src/state.rs`. ~50 LOC. Hand-write.

**Agent** ($15-25, family agent dispatch per site cluster):
- "Replace every `GcRef::new(LuaTable::...)` in this file with `state.alloc_table().set_...`"
- One agent per crate (lua-stdlib, lua-vm, lua-parse), bounded.

**DoD:**
- `grep -rn "GcRef::new" crates/` returns 0 hits outside `lua-types/src/gc.rs` and tests.
- `cargo test --workspace` passes.
- 79-program frontier still at 68+/79 (no regression).

**Risks:**
- Some `GcRef::new` sites have no `&mut LuaState` in scope (parser internals, dump/undump). These need either threading `state` in OR the TLS bridge from D-1c. Tag as TODO and defer.
- `LuaUserData::new` etc. might already be free functions; just make sure they're called from `state.alloc_*`.

---

### Phase D-1b — Abstract GcRef's Rc surface

**What:** Eliminate every assumption that `GcRef<T>` is `Rc<T>` from the callsite surface. The internals can stay `Rc` for now; we just don't let callers reach into `.0` or call `Rc::strong_count` etc.

Surface inventory (grep yields):
- `.0` field access on a `GcRef`
- `Rc::strong_count(&gc.0)` / `Rc::weak_count(&gc.0)`
- `Rc::downgrade(&gc.0)`
- `Weak<T>` types holding GC objects
- `Rc::ptr_eq(&a.0, &b.0)` (we already have `GcRef::ptr_eq`)
- `&*gc` (deref) — *this one's fine; we keep Deref*

Add wrappers on `GcRef<T>`:
```rust
pub fn strong_count(&self) -> usize { Rc::strong_count(&self.0) }
pub fn weak_count(&self) -> usize { Rc::weak_count(&self.0) }
pub fn downgrade(&self) -> GcWeak<T> { GcWeak(Rc::downgrade(&self.0)) }
```

Plus a new `GcWeak<T>` wrapper that survives the eventual backend swap (in real GC, `GcWeak` becomes a weak Gc reference; the trait surface stays the same).

**Static** (~1 hr, mostly automated with sed):
```bash
# Find every .0 access on a GcRef-typed binding
grep -rn '\.\<0\>' crates/ | grep -vE "TaskOutput|protobuf" > gcref_dot_zero.txt
# These mostly look like:
#   gc.0           -> gc.inner()  (new accessor)
#   &gc.0          -> gc.inner()
#   Rc::strong_count(&gc.0)  -> gc.strong_count()
#   Rc::downgrade(&gc.0)     -> gc.downgrade()
#   Rc::ptr_eq(&a.0, &b.0)   -> GcRef::ptr_eq(&a, &b)

# Apply patterns via sed:
find crates -name "*.rs" -exec sed -i '' \
  -e 's/Rc::strong_count(&\([a-z_]*\)\.0)/\1.strong_count()/g' \
  -e 's/Rc::weak_count(&\([a-z_]*\)\.0)/\1.weak_count()/g' \
  -e 's/Rc::downgrade(&\([a-z_]*\)\.0)/\1.downgrade()/g' \
  -e 's/Rc::ptr_eq(&\([a-z_]*\)\.0, &\([a-z_]*\)\.0)/GcRef::ptr_eq(\&\1, \&\2)/g' \
  {} \;
cargo build --workspace 2>&1 | tail -20  # find remaining unpatched sites
```

After sed, hand-review the diff; some patterns might be over-eager. Then run cargo build, fix remaining.

**Agent** (~$5-10):
- Only the residue after sed. Probably 10-30 sites with unusual shapes the regex missed.
- "Find every remaining `.0` access on a Gc-typed binding in this file and route through the new accessor methods."

**DoD:**
- `grep -rn '\.0' crates/ | grep -i "rc\|gcref"` returns only the wrapper definitions in `gc.rs`.
- `cargo build --workspace` passes.

**Risks:**
- `GcWeak<T>` is new — make sure it actually compiles cleanly with the current Phase-A-C `Rc` shape.
- Sed regex may match unrelated `.0` (struct field, tuple). Manual review of diff before commit.

---

### Phase D-1c — Scoped TLS bridge

**What:** A thread-local pointer to the "currently executing state's heap." Set during `state.run()` and similar entry points. Allows legacy `GcRef::new` (in code paths not yet migrated) to route through the active heap without threading `state` everywhere.

This is **scaffolding, not architecture.** It exists for the duration of migration (Phase D-1) and is removed at the end of D-1e.

```rust
// crates/lua-gc/src/heap.rs
thread_local! {
    static CURRENT_HEAP: Cell<Option<NonNull<Heap>>> = Cell::new(None);
}

pub struct HeapGuard;
impl HeapGuard {
    pub fn install(heap: &Heap) -> Self {
        let prev = CURRENT_HEAP.with(|c| c.replace(Some(NonNull::from(heap))));
        // ASSERT: prev is None for now (no re-entry). Phase D will need a stack.
        debug_assert!(prev.is_none(), "nested HeapGuard not yet supported");
        HeapGuard
    }
}
impl Drop for HeapGuard {
    fn drop(&mut self) {
        CURRENT_HEAP.with(|c| c.set(None));
    }
}

pub fn current_heap() -> Option<&'static Heap> {
    CURRENT_HEAP.with(|c| {
        c.get().map(|ptr| unsafe { ptr.as_ref() })
    })
}
```

Plus update `GcRef::new`:
```rust
impl<T> GcRef<T> where T: Trace + 'static {
    pub fn new(value: T) -> Self {
        if let Some(heap) = lua_gc::current_heap() {
            // Migration path: route through the real heap so the object
            // joins allgc and becomes sweep-visible.
            heap.allocate(value); // <-- but this returns Gc<T>, not Rc<T>...
        }
        GcRef(Rc::new(value))
    }
}
```

⚠️ **The above sketch has a bug.** `Gc<T>` and `Rc<T>` are different types; we can't return both from one function. Resolution: during the migration window, `heap.allocate(value)` returns `Gc<T>` AND we ALSO build the `Rc<T>` so `GcRef` keeps its `Rc` backing. The `Gc<T>` is discarded — its only purpose is to register the box in allgc for later sweep visibility.

This means objects are double-allocated during D-1c. Memory ~2x during migration. Acceptable temporarily; gone after D-1e.

**Static** (~1 hr, I write this):
- Add the TLS + HeapGuard + current_heap to lua-gc/src/heap.rs
- Update GcRef::new in lua-types/src/gc.rs
- Install HeapGuard in 3-5 key entry points: `state.run()`, `state.protected_call()`, `state.load()`, similar.

**Agent** (~$0): no agent work needed.

**DoD:**
- `cargo test` passes with no panics about nested guards.
- Smoke: allocate a string from inside a `state.run()` block, verify it appears in `state.global.heap.bytes_used() > 0`.

**Risks:**
- The double-allocation is wasteful but bounded (memory roughly 2x during migration; we have plenty of headroom for the test suite).
- Nested guards (e.g. when one Lua function calls another that also wraps in HeapGuard) — handle by making HeapGuard install a STACK of heaps, not a single slot. Simple Vec.

---

### Phase D-1d — Forbid GcRef::new outside bootstrap

**What:** Once D-1a's `state.alloc_*` methods cover the canonical allocation surface, prevent regression by banning new `GcRef::new` calls outside a small whitelist.

The whitelist:
- `crates/lua-types/src/gc.rs` (the definition)
- `crates/lua-cli/src/main.rs` (bootstrap)
- `tests/` directories
- One or two parser/dump sites that genuinely need it (review case-by-case)

**Static** (~30 min):

Add a PreToolUse hook (we already have hooks for type vocabulary):

```bash
# harness/check_no_gcref_new.sh
#!/bin/bash
file="$1"
content="$2"

# Skip whitelist
case "$file" in
  crates/lua-types/src/gc.rs) exit 0 ;;
  crates/lua-cli/src/main.rs) exit 0 ;;
  *test*) exit 0 ;;
  *bootstrap*) exit 0 ;;
esac

if echo "$content" | grep -q 'GcRef::new('; then
  echo "BLOCKED: GcRef::new outside bootstrap/tests. Use state.alloc_*() instead."
  exit 1
fi
exit 0
```

Wire into `.claude/settings.json` PreToolUse hooks (mirror existing type-vocab hook).

**Agent** (~$0).

**DoD:**
- An agent attempting to write `GcRef::new` in a non-whitelisted file gets blocked.
- Existing call sites already migrated (D-1a) remain — this only prevents NEW ones.

**Risks:**
- May surface 5-10 sites D-1a missed; those need either `state.alloc_*` migration or whitelist addition (with justification).

---

### Phase D-1e — Swap GcRef<T> = Gc<T>

**What:** Flip the type alias. After this, `GcRef<T>` IS `Gc<T>`. The Rc backing is gone. The TLS bridge becomes load-bearing for the migration window.

```rust
// crates/lua-types/src/gc.rs — before:
pub struct GcRef<T: ?Sized>(pub Rc<T>);

// after:
pub type GcRef<T> = lua_gc::Gc<T>;
```

Plus update all `GcRef::new` to call the heap:
```rust
impl<T: Trace + 'static> GcRef<T> {
    pub fn new(value: T) -> Self {
        lua_gc::current_heap()
            .expect("GcRef::new outside HeapGuard scope")
            .allocate(value)
    }
}
```

**Static** (~30 min I do):
- The alias swap (1 line).
- The new `GcRef::new` body (3 lines).
- Update any `T: ?Sized` constraint sites — `Gc<T>` requires `T: Sized` for unsizing coercion in mark.

**Agent** (~$30-50, the heaviest agent phase):
- `cargo build --workspace` will produce hundreds of errors.
- Family agents on clusters:
  - "Fix all `cannot coerce ?Sized to dyn Trace` errors in this file"
  - "Replace `gc.clone()` (was free for Rc, now Gc is Copy) with implicit copy"
  - "Remove `Weak<T>` patterns — Gc has no weak references in v1 (defer to D-2)"
- Iterative: each round agents fix what they can; rebuild; next round addresses residue.

**DoD:**
- `cargo build --workspace` is green.
- `cargo test --workspace` passes (modulo Weak-related tests we explicitly skip).
- Smoke: run all 48 inline test programs from the mega_loop frontier; pass-count matches pre-swap.
- `state.global.heap.bytes_used()` increases monotonically as a Lua program runs.

**Risks:**
- `Weak<GcRef<T>>` patterns. The string intern table uses these. Solution: replace with Strong refs + explicit ref-counting OR defer weak support to D-2 (after D-1 done). For now, intern table holds strong refs — strings live as long as the pool — acceptable, matches C-Lua's "strings never collected" semantics for fixed strings.
- The TLS bridge MUST be installed at the right entry points before this swap. Verify D-1c was wired correctly first.
- Agents may try to write `GcRef::new()` in new code paths. The D-1d hook catches this.

---

### Phase D-1f — Validate against gc.lua + cycles

**What:** Real GC stress tests. The frontier already has `gc.lua` + `gengc.lua` + 2 handwritten cycle smokes added during the overnight run. They should now exercise the real heap.

**Static** (~10 min):
- Add a specific cycle-collection test that hand-rolls a known cycle, calls collectgarbage, asserts memory is freed. Verifies the actual mark-sweep works end-to-end (not just compiles).
- Update mega_loop to ALWAYS run gc.lua first in each round (canary).

**Agent** (~$30-50, DEBUG mode):
- Re-launch mega_loop with $200 budget cap, MAX_OUTER=15.
- DEBUG agents iterate on whatever gc.lua and gengc.lua reveal.
- Typical bugs at this stage: missed write barriers, missing roots, finalizer ordering.

**DoD:**
- `gc.lua` PASS in `./harness/run_official_test.sh`
- `cargo test gc::cycle_collection` passes
- `heap.collections()` > 0 after a typical program run
- 79-program frontier ≥ 70/79 (we gained at least 2 from real GC: gc.lua + the handwritten cycle test)

**Risks:**
- `gengc.lua` requires generational mode; we don't implement that in D-1. Mark as known-fail / deferred.
- Some tests pass under leak-everything-Rc that may now fail under real GC if a barrier is missing. Each is a real bug to fix, not a regression.

---

## Total Budget Estimate

| Phase | Static | Agent | Wall |
|---|---|---|---|
| D-1a | 30 min | $15-25 | 1-2 hrs |
| D-1b | 1 hr | $5-10 | 1-2 hrs |
| D-1c | 1 hr | $0 | 1 hr |
| D-1d | 30 min | $0 | 30 min |
| D-1e | 30 min | $30-50 | 2-3 hrs |
| D-1f | 10 min | $30-50 | 2-3 hrs |
| **Total** | **~4 hrs** | **$80-135** | **8-12 hrs** |

This is roughly the same scope as the overnight run we just did, but front-loaded with much more human design and back-loaded with focused agent dispatch. Net cost should be lower because:
- Less wandering (each phase has a precise DoD)
- Less stuck-detect noise (we're moving in a deliberate direction)
- The TLS bridge eliminates the "thread &Heap through 462 sites" footgun

## How to Kick This Off

**Pre-flight (5 min):**
1. Confirm overnight run state: `git status` clean? Last good commit known?
2. Verify Phase D-0 still works: `cargo test -p lua-gc heap::` (5 tests).
3. Verify frontier baseline: run mega_loop with MAX_OUTER=1 MAX_PER_OUTER=0, expect 67-68/79.

**Phase D-1a launch:**
```bash
# Static prep — I do this
# 1. Generate the 7 alloc_* method skeletons in state.rs (hand-write)
# 2. Commit as "Phase D-1a: state-owned allocation API skeletons"

# Agent dispatch — family agents on each crate
# Prompt template (in harness/d1a_agent_prompt.txt):
#   "In crates/<crate>/src/, replace every GcRef::new(LuaXxx::new(...)) 
#    call with state.alloc_xxx(...). Constructor → method mapping in 
#    harness/alloc_migration.tsv. Stay in <crate>. Cap at 50 sites per 
#    agent. Run cargo build to verify."
```

**Phase D-1b launch:**
```bash
# Static prep — automated sed pass
bash harness/d1b_sed_pass.sh   # applies the 4 main patterns
cargo build --workspace 2>&1 | tee d1b_errors.log
# Inspect d1b_errors.log; if <30 errors, dispatch a single cleanup agent.
```

Same shape for D-1c through D-1f.

## What Could Still Go Wrong

1. **Borrow-checker explosion at D-1e.** Swapping GcRef from Rc to Gc may surface borrow conflicts we haven't seen because Rc has interior cloning. Mitigation: D-1a/b clean up the worst patterns first.

2. **Sweep collecting a live object.** If `Trace` for some type misses a field, that object's downstream gets swept. Symptom: random use-after-free panics inside lua-gc::heap. Mitigation: the smoke tests in `heap.rs` cover the basic case; comprehensive cycle tests in D-1f catch more.

3. **Performance regression.** Real GC on every step might slow programs that allocated heavily under leak-everything-Rc. Acceptable as long as it doesn't push test programs past the 30s timeout.

4. **The Weak refs problem.** I deferred them but if the string intern table actually NEEDS weak refs (i.e. strings should be collectible), we have to either implement Gc weak refs or rethink. Investigation needed before D-1e.

## Locked Decisions (D-1 scope)

1. **Multi-GlobalState support: YES, v1.** Single-slot HeapGuard is a footgun. The bridge is a `Vec<NonNull<Heap>>` push/pop from day one — one extra line and zero downside.

2. **Generational mode: DEFER to D-2.** D-1 ships stop-the-world mark-sweep. `gengc.lua` is known-fail in D-1.

3. **Finalizers (`__gc`): DEFER to D-2, marked required for full compat.** Pending-queue + resurrection semantics are too much overhead to land alongside the allocator migration. Userdata-with-`__gc` tests are known-fail in D-1.

4. **Weak refs: DEFER full design to D-2.** D-1 accepts strong interned strings (memory leak, not semantic gap — matches C-Lua's "fixed strings never collected" for short strings). Weak tables are a semantic-compat gap; mark known-fail in D-1.

## Correction to D-1c (the bridge)

The earlier D-1c sketch implied allocation routing during the migration window. **That was wrong.** If `GcRef::new` calls `heap.allocate(value)` AND `Rc::new(value)`, you get:
- A move conflict (value consumed twice), OR
- A clone that creates two distinct allocations with no shared identity, so the real graph stays in Rc-land and the `allgc` chain holds ghost objects that sweep clean every cycle. Looks like GC is working; isn't.

**Corrected D-1c scope:** Just the infrastructure.

- Add `HeapGuard` as a stack-based TLS guard (Vec of NonNull<Heap>). Push on `state.run()`, pop on drop.
- Add `current_heap() -> Option<&Heap>` accessor.
- DO NOT change `GcRef::new` behavior. It stays `Rc::new`.
- Wire `HeapGuard::install` into `state.run()`, `state.protected_call()`, `state.load()`.

The bridge becomes load-bearing only at D-1e, when `GcRef<T>` is aliased to `Gc<T>` and `GcRef::new`'s body switches to `current_heap().expect(...).allocate(value)`. Until then, the bridge is dormant — just a pointer set/cleared.

This also means D-1c is even cheaper than estimated: ~30 min static, zero risk of phantom-allocation behavior.
