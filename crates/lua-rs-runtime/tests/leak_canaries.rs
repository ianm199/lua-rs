//! Counting-allocator leak canaries over the embedding lifecycle.
//!
//! The differential oracle can't see per-VM or per-call leaks: it diffs one
//! process running one VM, and the OS frees everything at exit. Issue #249
//! (guard-less GC allocations leaking past `Lua` drop) survived every suite
//! for months because of exactly that blind spot. For lifecycle properties
//! the oracle isn't the C binary — it's **net-zero allocation across
//! iterations**, measured by an observer outside the GC's own bookkeeping
//! (`allgc_count`/`bytes_used` are maintained by the code paths under test
//! and provably can't see detached boxes).
//!
//! One `#[test]` runs every scenario sequentially: the live-byte counter is
//! process-global, so parallel tests would pollute each other's readings.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, Ordering};

use omnilua::{Lua, LuaRuntime};

struct CountingAllocator;

static LIVE_BYTES: AtomicIsize = AtomicIsize::new(0);

// SAFETY: delegates every operation to `System` unchanged; the only addition
// is relaxed atomic accounting of live bytes, which allocates nothing.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            LIVE_BYTES.fetch_add(layout.size() as isize, Ordering::Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        LIVE_BYTES.fetch_sub(layout.size() as isize, Ordering::Relaxed);
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            LIVE_BYTES.fetch_add(layout.size() as isize, Ordering::Relaxed);
        }
        p
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            LIVE_BYTES.fetch_add(new_size as isize - layout.size() as isize, Ordering::Relaxed);
        }
        p
    }
}

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator;

fn live_bytes() -> isize {
    LIVE_BYTES.load(Ordering::Relaxed)
}

/// Iterations per scenario. High enough that any real per-iteration leak
/// (a single leaked GcBox is ≥48 bytes, the #249 leak was ~29KB/VM) dwarfs
/// the tolerance by orders of magnitude.
const ITERS: usize = 64;

/// Total growth tolerance per scenario, in bytes, across all `ITERS`
/// iterations after warmup. Absorbs amortized container growth that hasn't
/// plateaued (interner tables, TLS vectors) without masking real leaks: a
/// genuine per-iteration leak of even one box would exceed this by ~10x.
#[cfg(not(target_os = "windows"))]
const TOLERANCE: isize = 4096;

/// Windows measured envelope — issue #317: VM churn retained ~40-260
/// bytes/iteration there (pre-existing, GC-reclaimable, NOT #249-class —
/// the detached tripwire below stays exact on every platform). Root cause
/// (the io `LStream` side table outliving its VM in a `thread_local!`;
/// retention scaled with how rarely the platform allocator reuses
/// addresses, hence Windows ≫ Linux) is fixed — the table now lives on
/// `GlobalState` — and the `io_registry_churn` scenario pins it at the
/// POSIX tolerance. This envelope stays until a Windows suite run
/// re-measures; it still catches a #249-class leak (~29KB/iter →
/// ~1.8MB/scenario) by two orders of magnitude.
#[cfg(target_os = "windows")]
const TOLERANCE: isize = 65536;

/// True under `LUA_RS_GC_QUARANTINE=1`, where sweep parks swept boxes
/// (poisoned, freed only at heap teardown) instead of releasing them —
/// live bytes then grow per iteration by design for scenarios that keep
/// one VM alive. Only the live-byte assertions are relaxed for it; the
/// detached-allocation tripwire is unaffected by quarantine and always
/// runs.
fn quarantine_mode() -> bool {
    std::env::var_os("LUA_RS_GC_QUARANTINE").is_some_and(|v| v == "1")
}

/// Runs `scenario` twice to warm caches (string interner, lazy statics,
/// thread-locals), snapshots live bytes, runs it `ITERS` more times, and
/// asserts the counter came back to the snapshot within `TOLERANCE`.
/// `persistent_heap` marks scenarios that reuse one VM across iterations —
/// the only ones whose live-byte check must be skipped under quarantine
/// (create/drop scenarios still settle, since heap teardown frees
/// quarantined boxes).
fn assert_steady_state_inner(name: &str, persistent_heap: bool, mut scenario: impl FnMut()) {
    scenario();
    scenario();
    let detached_before = lua_gc::detached_allocations();
    let baseline = live_bytes();
    for _ in 0..ITERS {
        scenario();
    }
    let growth = live_bytes() - baseline;
    let detached_growth = lua_gc::detached_allocations() - detached_before;
    assert_eq!(
        detached_growth, 0,
        "{name}: {detached_growth} detached (never-freed) GC allocations \
         escaped during {ITERS} iterations — some path allocated with no \
         active HeapGuard (issue #249 class); the guard-less allocation site \
         now panics directly, so a panic backtrace pinpoints the offending path"
    );
    if persistent_heap && quarantine_mode() {
        return;
    }
    assert!(
        growth <= TOLERANCE,
        "{name}: live bytes grew by {growth} over {ITERS} iterations \
         (~{}/iter) after warmup — the embedding lifecycle is not \
         steady-state; something retains or leaks memory per iteration",
        growth / ITERS as isize
    );
}

fn assert_steady_state(name: &str, scenario: impl FnMut()) {
    assert_steady_state_inner(name, false, scenario)
}

fn assert_steady_state_shared_vm(name: &str, scenario: impl FnMut()) {
    assert_steady_state_inner(name, true, scenario)
}

#[test]
fn embedding_lifecycle_is_steady_state() {
    assert_steady_state("vm_churn: create, exec, drop", || {
        let lua = Lua::new();
        lua.load("local t = {1, 2, 3} return #t").exec().unwrap();
        drop(lua);
    });

    assert_steady_state("runtime_churn: LuaRuntime create/exec/drop", || {
        let mut rt = LuaRuntime::new().unwrap();
        rt.exec(b"return 1 + 1", b"=canary").unwrap();
        drop(rt);
    });

    assert_steady_state("io_registry_churn: std-file side table dies with the VM (#317)", || {
        for _ in 0..16 {
            let lua = Lua::new();
            lua.load("return io.type(io.stdout)").exec().unwrap();
            drop(lua);
        }
    });

    let lua = Lua::new();
    lua.load("collectgarbage('collect')").exec().unwrap();

    assert_steady_state_shared_vm("chunk_churn: load + into_function + drop", || {
        let f = lua
            .load("local x = 42 return x")
            .into_function()
            .unwrap();
        drop(f);
        lua.load("collectgarbage('collect')").exec().unwrap();
    });

    assert_steady_state_shared_vm("exec_churn: load + exec", || {
        lua.load("local s = 'a' .. 'b' return s").exec().unwrap();
        lua.load("collectgarbage('collect')").exec().unwrap();
    });

    assert_steady_state_shared_vm("coroutine_churn: create + resume + discard", || {
        lua.load(
            "for _ = 1, 8 do \
                 local co = coroutine.create(function() coroutine.yield(1) end) \
                 coroutine.resume(co) \
             end \
             collectgarbage('collect')",
        )
        .exec()
        .unwrap();
    });

    assert_steady_state("error_churn: host-side LuaError construction (no VM)", || {
        for i in 0..32 {
            let e = lua_types::LuaError::runtime(format_args!("host-side error {i}"));
            let _ = e.message_lossy();
            drop(e);
        }
    });

    assert_steady_state_shared_vm("callback_churn: create_function + drop", || {
        let f = lua.create_function(|_, n: i64| Ok(n + 1)).unwrap();
        drop(f);
        lua.load("collectgarbage('collect')").exec().unwrap();
    });
}
