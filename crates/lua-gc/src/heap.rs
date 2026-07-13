//! Phase D mark-and-sweep garbage collector.
//!
//! This module is the production GC for the Lua runtime, replacing the
//! `Rc<T>`-backed `GcRef<T>` placeholder used through Phase B/C. It is a
//! single-threaded precise tracing collector with incremental and
//! generational paths plus forward/backward write barriers.
//!
//! # Vocabulary
//!
//! - **Gc<T>**: a pointer-sized handle. `Copy + Clone`. Replaces `GcRef<T>`.
//! - **GcBox<T>**: the heap allocation; contains a header and the value.
//! - **GcHeader**: per-object metadata (color, age, flags, pacer size) — 8 B,
//!   no pointer. Heap ownership is an external [`OwnerVec`] slot, not an
//!   intrusive header link (#113 Wave 2); the grayagain revisit set is
//!   likewise heap-owned (`Heap::grayagain`).
//! - **Trace**: trait every GC-rooted type implements. The `trace` method
//!   walks all `Gc<_>` fields and calls `Marker::mark` on each.
//! - **Marker**: passed to `trace`; carries the gray queue.
//! - **OwnerVec**: the collector-spine owner-class vector — `Option<NonNull>`
//!   slots with tombstone removal and generational cohort counts; three of
//!   them (allgc/finobj/tobefnz) replace the old intrusive lists + cursors.
//! - **Heap**: owns the allgc/finobj/tobefnz owner vectors, byte counters, and
//!   GC state machine.
//!
//! # Safety model
//!
//! All `unsafe` is confined to this crate (per `harness/unsafe-budgets.toml`).
//! The invariants are:
//!
//! 1. Every `Gc<T>` points to a valid, allocated, not-yet-swept `GcBox<T>`.
//! 2. The owner vectors are consistent: every live heap-owned `GcBox` is held
//!    by exactly one `Some` slot across `Heap.allgc`, `Heap.finobj`, and
//!    `Heap.tobefnz` (plus the transitional `pending_release` during a sweep
//!    drain), and is reached exactly once by a slot walk.
//! 3. After `Heap::full_collect(roots)`, every `Gc<T>` reachable from `roots`
//!    is still valid; unreachable boxes are dropped and deallocated.
//!
//! # Migration shape
//!
//! Existing code holds `GcRef<T>` (which after Phase D is a type alias for
//! `Gc<T>`). Legacy call sites like `GcRef::new(value)` route through
//! `Gc::new_uncollected` which allocates a `GcBox` but does NOT register it
//! in any heap. Phase D-1b agent work converts these to
//! `state.heap().allocate(value)` so the new box joins the heap owner lists.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};
use std::marker::PhantomData;
use std::ptr::NonNull;

#[derive(Default)]
struct IdentityHasher {
    value: u64,
}

impl Hasher for IdentityHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        const PRIME: u64 = 0x0000_0100_0000_01b3;
        for &byte in bytes {
            self.value ^= u64::from(byte);
            self.value = self.value.wrapping_mul(PRIME);
        }
    }

    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.value = i as u64;
    }

    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.value = i;
    }

    #[inline]
    fn finish(&self) -> u64 {
        let mut x = self.value;
        x ^= x >> 30;
        x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
        x ^= x >> 27;
        x = x.wrapping_mul(0x94d0_49bb_1331_11eb);
        x ^ (x >> 31)
    }
}

type IdentityBuildHasher = BuildHasherDefault<IdentityHasher>;
type IdentityHashSet = HashSet<usize, IdentityBuildHasher>;
type IdentityHashMap<V> = HashMap<usize, V, IdentityBuildHasher>;

// ──────────────────────────────────────────────────────────────────────────
// Phase D-1c — scoped thread-local HeapGuard
//
// Lua's C API supports multiple `lua_State`s on one OS thread (sandbox-per-
// state is a real embedding pattern). We honor that by stacking the
// currently-active heap rather than holding a single slot. `HeapGuard::push`
// activates a heap; drop pops it.
//
// `with_current_heap(...)` exposes the top of the stack only for the dynamic
// extent of a closure — used by `GcRef::new` call sites that don't have
// `&mut LuaState` in scope.
// ──────────────────────────────────────────────────────────────────────────

thread_local! {
    static CURRENT_HEAP_STACK: RefCell<Vec<std::rc::Rc<Heap>>> = const { RefCell::new(Vec::new()) };
}

/// A scoped guard for the currently-active heap. Pushed at entry to
/// `state.run()` / `state.protected_call()` / `state.load()`; popped on
/// drop. Supports nesting (multiple LuaStates on one thread).
///
/// Holds a strong `Rc<Heap>` on the TLS stack (issue #252): the guard is
/// sound with no outlives contract — a guard that outlives its `Lua` keeps
/// the heap alive until the guard pops, rather than dangling. Guards are
/// function-scoped everywhere in-tree, so the lifetime extension is
/// transient by construction.
///
/// `!Send`/`!Sync` (via the `Rc` it carries): the stack it pops lives in
/// this thread's TLS, so moving a guard to another thread would pop the
/// wrong stack. The guard keeps its own copy of the heap handle so `Drop`
/// can assert it pops the frame it pushed.
pub struct HeapGuard {
    heap: std::rc::Rc<Heap>,
}

impl HeapGuard {
    /// Push `heap` onto the active stack. Returns a guard; dropping it pops.
    pub fn push(heap: &std::rc::Rc<Heap>) -> Self {
        CURRENT_HEAP_STACK.with(|stack| stack.borrow_mut().push(std::rc::Rc::clone(heap)));
        HeapGuard {
            heap: std::rc::Rc::clone(heap),
        }
    }
}

impl Drop for HeapGuard {
    fn drop(&mut self) {
        CURRENT_HEAP_STACK.with(|stack| {
            let popped = stack.borrow_mut().pop();
            debug_assert!(
                popped
                    .as_ref()
                    .is_some_and(|top| std::rc::Rc::ptr_eq(top, &self.heap)),
                "HeapGuard::drop popped a frame it did not push — guards must \
                 drop in reverse push order on the thread that created them"
            );
        });
    }
}

/// RAII handle for a heap bootstrap window; created by
/// [`Heap::bootstrap_scope`], ends the window on drop. Exists so error paths
/// (`?` during stdlib install, `lua_open` failure) cannot leave the heap
/// stuck in bootstrap mode the way a manual `end_bootstrap` call can.
///
/// Sound with no lifetime and no unsafe: the scope shares ownership of the
/// heap's depth counter (`Rc<Cell<usize>>`), so a scope that outlives its
/// heap decrements a still-live cell instead of dereferencing a dead heap —
/// the count on a dead heap is meaningless but harmless. (Contrast with
/// [`HeapGuard::push`]'s raw-pointer contract, tracked for the same
/// treatment in the heap-ownership follow-up.)
pub struct BootstrapScope {
    depth: std::rc::Rc<Cell<usize>>,
}

impl Drop for BootstrapScope {
    fn drop(&mut self) {
        let depth = self.depth.get();
        debug_assert!(depth > 0, "BootstrapScope dropped with zero depth");
        self.depth.set(depth.saturating_sub(1));
    }
}

thread_local! {
    static DETACHED_ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

/// Total detached ([`Gc::new_uncollected`]) allocations made on this thread
/// since it started. Leak canaries assert a zero delta across embedding
/// scenarios. This counter deliberately lives outside the heap's own
/// bookkeeping: detached boxes never touch `bytes`/`objects`, which is
/// exactly the blind spot that hid issue #249 — a mechanism must not be
/// verified with bookkeeping it maintains itself.
pub fn detached_allocations() -> usize {
    DETACHED_ALLOCATIONS.with(|c| c.get())
}

/// Runs `f` with a reference to the currently-active heap, or `None` if no
/// `HeapGuard` is in scope.
///
/// The heap reference is deliberately scoped to the closure. This avoids the
/// previous `current_heap() -> Option<&'static Heap>` lifetime lie while still
/// supporting legacy `GcRef::new` call sites that do not receive `&mut LuaState`.
pub fn with_current_heap<R>(f: impl for<'a> FnOnce(Option<&'a std::rc::Rc<Heap>>) -> R) -> R {
    let top = CURRENT_HEAP_STACK.with(|stack| stack.borrow().last().cloned());
    f(top.as_ref())
}

/// A non-owning heap identity handle for weak references (issue #252):
/// holds a `Weak<Heap>`, so a `GcWeak` that outlives its heap answers
/// "does the heap still contain my target" with a plain `false` — after
/// sweep *or* after heap teardown, indistinguishably — instead of
/// dereferencing freed memory. No unsafe, no outlives contract.
#[derive(Clone, Debug)]
pub struct HeapRef {
    weak: std::rc::Weak<Heap>,
}

impl HeapRef {
    pub fn from_heap(heap: &std::rc::Rc<Heap>) -> Self {
        HeapRef {
            weak: std::rc::Rc::downgrade(heap),
        }
    }

    pub fn contains_allocation(&self, identity: usize, token: usize) -> bool {
        match self.weak.upgrade() {
            Some(heap) => heap.contains_allocation(identity, token),
            None => false,
        }
    }
}

/// A traced color in the tri-color invariant.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Color {
    /// Not yet visited in the current cycle. The collector alternates between
    /// two white bits so allocations made during sweep are not collected by
    /// the cycle already in progress.
    White0,
    /// Alternate white bit.
    White1,
    /// Visited; outgoing references not yet traced.
    Gray,
    /// Fully traced; no outgoing pointers to white objects.
    Black,
}

impl Color {
    pub fn is_white(self) -> bool {
        matches!(self, Color::White0 | Color::White1)
    }

    fn other_white(self) -> Self {
        match self {
            Color::White0 => Color::White1,
            Color::White1 => Color::White0,
            Color::Gray | Color::Black => self,
        }
    }
}

/// Object age used by Lua's generational collector.
///
/// Mirrors `G_NEW` through `G_TOUCHED2` in `lgc.h`.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum GcAge {
    New,
    Survival,
    Old0,
    Old1,
    Old,
    Touched1,
    Touched2,
}

impl GcAge {
    pub fn is_old(self) -> bool {
        !matches!(self, GcAge::New | GcAge::Survival)
    }

    fn next_after_minor(self) -> Self {
        match self {
            GcAge::New => GcAge::Survival,
            GcAge::Survival | GcAge::Old0 => GcAge::Old1,
            GcAge::Old1 | GcAge::Old | GcAge::Touched2 => GcAge::Old,
            GcAge::Touched1 => GcAge::Touched2,
        }
    }
}

/// A Lua 5.1 collect-time finalizability probe for a single userdata.
///
/// Lua 5.1 decides which userdata need finalization at collection time, not
/// at `setmetatable` time: `luaC_separateudata` (`lgc.c`) walks every
/// userdata and, for each white non-finalized one, reads its **live**
/// metatable for `__gc`. This is observably different from 5.2+, where
/// `luaC_checkfinalizer` makes the decision eagerly at `setmetatable` and a
/// `__gc` added to a shared metatable afterwards never takes effect.
///
/// The collector cannot read a userdata's metatable (a VM concept), so the VM
/// supplies one probe per userdata. The collector only stores probes and
/// prunes dead ones via [`is_alive`](Self::is_alive); the VM drives the
/// metatable read and the actual finalizer registration. Probes are erased
/// behind [`std::any::Any`] so the VM can downcast back to its concrete type
/// without the collector naming a VM type.
pub trait Udata51Probe: std::any::Any {
    /// True while the backing userdata box has not yet been swept. Backed by
    /// a weak handle on the VM side, so this is safe to call after a sweep
    /// freed the box.
    fn is_alive(&self) -> bool;

    /// Erased self for VM-side downcast.
    fn as_any(&self) -> &dyn std::any::Any;
}

/// Minimal metadata a finalizable VM object must expose for collector-owned
/// finalizer-list bookkeeping.
pub trait FinalizerEntry: Clone {
    fn identity(&self) -> usize;
    fn heap_ptr(&self) -> Option<NonNull<GcBox<dyn Trace>>> {
        None
    }
    fn age(&self) -> GcAge;
    fn is_finalized(&self) -> bool;
    fn set_finalized(&self, finalized: bool);
}

/// Minimal operations needed for collector-owned weak-registry bookkeeping.
pub trait WeakEntry: Clone {
    type Strong: Clone;

    fn identity(&self) -> usize;
    fn list_kind(&self) -> WeakListKind;
    fn upgrade(&self) -> Option<Self::Strong>;
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WeakListKind {
    WeakValues,
    Ephemeron,
    AllWeak,
}

#[derive(Copy, Clone, Default, Debug, PartialEq, Eq)]
pub struct WeakRegistryStats {
    pub tracked: usize,
    pub snapshot_live: usize,
    pub snapshot_dead: usize,
    pub retained: usize,
    pub weak_values: usize,
    pub ephemeron: usize,
    pub all_weak: usize,
}

#[derive(Clone, Debug)]
pub struct WeakRegistry<T: WeakEntry> {
    weak_values: Vec<T>,
    ephemeron: Vec<T>,
    all_weak: Vec<T>,
    last_stats: WeakRegistryStats,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WeakRegistrySnapshot<T> {
    pub weak_values: Vec<T>,
    pub ephemeron: Vec<T>,
    pub all_weak: Vec<T>,
}

impl<T> Default for WeakRegistrySnapshot<T> {
    fn default() -> Self {
        Self {
            weak_values: Vec::new(),
            ephemeron: Vec::new(),
            all_weak: Vec::new(),
        }
    }
}

impl<T> WeakRegistrySnapshot<T> {
    pub fn len(&self) -> usize {
        self.weak_values
            .len()
            .saturating_add(self.ephemeron.len())
            .saturating_add(self.all_weak.len())
    }

    pub fn into_flat(self) -> Vec<T> {
        self.weak_values
            .into_iter()
            .chain(self.ephemeron)
            .chain(self.all_weak)
            .collect()
    }
}

impl<T: WeakEntry> Default for WeakRegistry<T> {
    fn default() -> Self {
        Self {
            weak_values: Vec::new(),
            ephemeron: Vec::new(),
            all_weak: Vec::new(),
            last_stats: WeakRegistryStats::default(),
        }
    }
}

impl<T: WeakEntry> WeakRegistry<T> {
    pub fn len(&self) -> usize {
        self.weak_values
            .len()
            .saturating_add(self.ephemeron.len())
            .saturating_add(self.all_weak.len())
    }

    pub fn stats(&self) -> WeakRegistryStats {
        self.last_stats
    }

    fn list_mut(&mut self, kind: WeakListKind) -> &mut Vec<T> {
        match kind {
            WeakListKind::WeakValues => &mut self.weak_values,
            WeakListKind::Ephemeron => &mut self.ephemeron,
            WeakListKind::AllWeak => &mut self.all_weak,
        }
    }

    pub fn remove_identity(&mut self, id: usize) {
        self.weak_values.retain(|entry| entry.identity() != id);
        self.ephemeron.retain(|entry| entry.identity() != id);
        self.all_weak.retain(|entry| entry.identity() != id);
        self.last_stats.tracked = self.len();
        self.last_stats.retained = self.len();
        self.update_cohort_stats();
    }

    fn update_cohort_stats(&mut self) {
        self.last_stats.weak_values = self.weak_values.len();
        self.last_stats.ephemeron = self.ephemeron.len();
        self.last_stats.all_weak = self.all_weak.len();
    }

    pub fn push_unique(&mut self, entry: T) {
        let id = entry.identity();
        self.remove_identity(id);
        self.list_mut(entry.list_kind()).push(entry);
        self.last_stats.tracked = self.len();
        self.last_stats.retained = self.len();
        self.update_cohort_stats();
    }

    pub fn live_snapshot_by_kind(&mut self) -> WeakRegistrySnapshot<T::Strong> {
        let tracked_before = self.len();
        let weak_values_capacity = self.weak_values.len();
        let ephemeron_capacity = self.ephemeron.len();
        let all_weak_capacity = self.all_weak.len();
        let mut seen = std::collections::HashSet::<usize>::with_capacity(tracked_before);
        let mut live = WeakRegistrySnapshot {
            weak_values: Vec::with_capacity(weak_values_capacity),
            ephemeron: Vec::with_capacity(ephemeron_capacity),
            all_weak: Vec::with_capacity(all_weak_capacity),
        };
        let mut dead = 0usize;

        let entries = std::mem::take(&mut self.weak_values)
            .into_iter()
            .chain(std::mem::take(&mut self.ephemeron))
            .chain(std::mem::take(&mut self.all_weak));
        for entry in entries {
            if !seen.insert(entry.identity()) {
                continue;
            }
            match entry.upgrade() {
                Some(strong) => {
                    match entry.list_kind() {
                        WeakListKind::WeakValues => live.weak_values.push(strong),
                        WeakListKind::Ephemeron => live.ephemeron.push(strong),
                        WeakListKind::AllWeak => live.all_weak.push(strong),
                    }
                    self.list_mut(entry.list_kind()).push(entry);
                }
                None => dead += 1,
            }
        }

        self.last_stats = WeakRegistryStats {
            tracked: tracked_before,
            snapshot_live: live.len(),
            snapshot_dead: dead,
            retained: self.len(),
            weak_values: self.weak_values.len(),
            ephemeron: self.ephemeron.len(),
            all_weak: self.all_weak.len(),
        };
        live
    }

    pub fn live_snapshot(&mut self) -> Vec<T::Strong> {
        self.live_snapshot_by_kind().into_flat()
    }

    pub fn retain_identities(&mut self, ids: &std::collections::HashSet<usize>) {
        self.weak_values
            .retain(|entry| ids.contains(&entry.identity()));
        self.ephemeron
            .retain(|entry| ids.contains(&entry.identity()));
        self.all_weak
            .retain(|entry| ids.contains(&entry.identity()));
        self.last_stats.retained = self.len();
        self.last_stats.tracked = self.len();
        self.update_cohort_stats();
    }
}

#[derive(Clone, Debug)]
pub struct FinalizerRegistry<T: FinalizerEntry> {
    pending: Vec<T>,
    to_be_finalized: Vec<T>,
    pending_reallyold: usize,
    pending_old1: usize,
    pending_survival: usize,
}

#[derive(Copy, Clone, Default, Debug, PartialEq, Eq)]
pub struct FinalizerRegistryStats {
    pub pending_young: usize,
    pub pending_old: usize,
    pub to_be_finalized_young: usize,
    pub to_be_finalized_old: usize,
    pub finobj_new: usize,
    pub finobj_survival: usize,
    pub finobj_old1: usize,
    pub finobj_reallyold: usize,
    pub finobj_minor_scan: usize,
}

impl<T: FinalizerEntry> Default for FinalizerRegistry<T> {
    fn default() -> Self {
        Self {
            pending: Vec::new(),
            to_be_finalized: Vec::new(),
            pending_reallyold: 0,
            pending_old1: 0,
            pending_survival: 0,
        }
    }
}

impl<T: FinalizerEntry> FinalizerRegistry<T> {
    fn pending_new_len(&self) -> usize {
        self.pending.len().saturating_sub(
            self.pending_reallyold
                .saturating_add(self.pending_old1)
                .saturating_add(self.pending_survival),
        )
    }

    fn minor_scan_start(&self) -> usize {
        self.pending_reallyold.saturating_add(self.pending_old1)
    }

    fn debug_assert_pending_cohorts(&self) {
        debug_assert!(
            self.pending_reallyold
                .saturating_add(self.pending_old1)
                .saturating_add(self.pending_survival)
                <= self.pending.len()
        );
    }

    pub fn pending(&self) -> &[T] {
        &self.pending
    }

    pub fn pending_snapshot(&self) -> Vec<T> {
        self.pending.clone()
    }

    pub fn pending_minor_snapshot(&self) -> Vec<T> {
        self.pending[self.minor_scan_start().min(self.pending.len())..].to_vec()
    }

    pub fn to_be_finalized(&self) -> &[T] {
        &self.to_be_finalized
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    pub fn to_be_finalized_len(&self) -> usize {
        self.to_be_finalized.len()
    }

    pub fn has_to_be_finalized(&self) -> bool {
        !self.to_be_finalized.is_empty()
    }

    pub fn stats(&self) -> FinalizerRegistryStats {
        fn count_by_age<T: FinalizerEntry>(objects: &[T]) -> (usize, usize) {
            objects
                .iter()
                .fold((0usize, 0usize), |(young, old), object| {
                    if object.age().is_old() {
                        (young, old + 1)
                    } else {
                        (young + 1, old)
                    }
                })
        }
        let (pending_young, pending_old) = count_by_age(&self.pending);
        let (to_be_finalized_young, to_be_finalized_old) = count_by_age(&self.to_be_finalized);
        FinalizerRegistryStats {
            pending_young,
            pending_old,
            to_be_finalized_young,
            to_be_finalized_old,
            finobj_new: self.pending_new_len(),
            finobj_survival: self.pending_survival,
            finobj_old1: self.pending_old1,
            finobj_reallyold: self.pending_reallyold,
            finobj_minor_scan: self.pending.len().saturating_sub(self.minor_scan_start()),
        }
    }

    pub fn push_pending_unique(&mut self, object: T) -> bool {
        if object.is_finalized() {
            return false;
        }
        let id = object.identity();
        if !self.pending.iter().any(|o| o.identity() == id) {
            object.set_finalized(true);
            self.pending.push(object);
            self.debug_assert_pending_cohorts();
            true
        } else {
            false
        }
    }

    pub fn take_pending(&mut self) -> Vec<T> {
        self.pending_reallyold = 0;
        self.pending_old1 = 0;
        self.pending_survival = 0;
        std::mem::take(&mut self.pending)
    }

    fn retain_pending_not_in(&mut self, ids: &std::collections::HashSet<usize>) {
        if ids.is_empty() {
            return;
        }
        let original_reallyold = self.pending_reallyold;
        let original_old1 = self.pending_old1;
        let original_survival = self.pending_survival;
        let mut retained_reallyold = original_reallyold;
        let mut retained_old1 = original_old1;
        let mut retained_survival = original_survival;
        let mut retained = Vec::with_capacity(self.pending.len());
        for (index, object) in std::mem::take(&mut self.pending).into_iter().enumerate() {
            if ids.contains(&object.identity()) {
                if index < original_reallyold {
                    retained_reallyold -= 1;
                } else if index < original_reallyold + original_old1 {
                    retained_old1 -= 1;
                } else if index < original_reallyold + original_old1 + original_survival {
                    retained_survival -= 1;
                }
            } else {
                retained.push(object);
            }
        }
        self.pending = retained;
        self.pending_reallyold = retained_reallyold;
        self.pending_old1 = retained_old1;
        self.pending_survival = retained_survival;
        self.debug_assert_pending_cohorts();
    }

    pub fn push_to_be_finalized(&mut self, object: T) {
        object.set_finalized(true);
        self.to_be_finalized.push(object);
    }

    fn extend_to_be_finalized(&mut self, objects: Vec<T>) -> Vec<T> {
        let drain_order: Vec<T> = objects.into_iter().rev().collect();
        for object in drain_order.iter().cloned() {
            self.push_to_be_finalized(object);
        }
        drain_order
    }

    pub fn promote_pending_to_finalized(&mut self, objects: Vec<T>) -> Vec<T> {
        if objects.is_empty() {
            return Vec::new();
        }
        let mut ids: std::collections::HashSet<usize> =
            std::collections::HashSet::with_capacity(objects.len());
        ids.extend(objects.iter().map(|object| object.identity()));
        self.retain_pending_not_in(&ids);
        self.extend_to_be_finalized(objects)
    }

    pub fn promote_all_pending_to_old(&mut self) {
        self.pending_reallyold = self.pending.len();
        self.pending_old1 = 0;
        self.pending_survival = 0;
    }

    pub fn reset_generation_boundaries(&mut self) {
        self.pending_reallyold = 0;
        self.pending_old1 = 0;
        self.pending_survival = 0;
    }

    pub fn finish_minor_collection(&mut self) {
        let new = self.pending_new_len();
        self.pending_reallyold = self.pending_reallyold.saturating_add(self.pending_old1);
        self.pending_old1 = self.pending_survival;
        self.pending_survival = new;
        self.debug_assert_pending_cohorts();
    }

    pub fn pop_to_be_finalized(&mut self) -> Option<T> {
        let object = if self.to_be_finalized.is_empty() {
            None
        } else {
            Some(self.to_be_finalized.remove(0))
        };
        if let Some(ref object) = object {
            object.set_finalized(false);
        }
        object
    }
}

/// Per-object GC metadata. Lives at the start of every `GcBox`.
#[repr(C)]
pub struct GcHeader {
    /// Hot fields read/written by the mark/sweep/barrier loops keep their
    /// own bytes — packing them measurably taxed gc-heavy workloads
    /// (recount 2026-06-10: +4% Ir on gc_pressure). The 64 -> 40 byte diet
    /// came from the COLD side instead: the diagnostics-only `type_name`
    /// fat pointer became a `Trace` method, the three cold bool flags share
    /// one byte, and the pacer size is u32. The 40 -> 24 byte diet (#113
    /// Wave 1) removed the `gray_next` grayagain link. The 24 -> 8 byte diet
    /// (#113 Wave 2) removed the last one, `next`: heap ownership is now an
    /// external [`OwnerVec`] slot, not an intrusive header link, so the header
    /// is `color + age + flags + pad + size(u32)` = **8 B on both 64-bit and
    /// wasm32** (it contains no pointers; the `const _` assert below pins it).
    color: Cell<Color>,
    age: Cell<GcAge>,
    /// Cold flags, one bit each: finalized (FINALIZEDBIT — set while the
    /// object is registered in a pending/to-be-finalized list, cleared when
    /// popped for `__gc`), collected (true iff this box is linked into a
    /// heap owner list so it will be swept and its `size` refunded;
    /// `new_uncollected` boxes stay false and must never have buffer bytes
    /// charged — [`Gc::account_buffer`] no-ops), gray_listed (true while the
    /// object has an entry in the heap-side `Heap::grayagain` revisit vector).
    flags: Cell<u8>,
    /// Rough byte size charged to the pacer for this object. Starts at the
    /// `GcBox<T>` size and is adjusted in place by [`Gc::account_buffer`]
    /// when the value's owned heap buffers (table array/node Vecs) grow or
    /// shrink. Invariant: this is always exactly the amount sweep will
    /// refund to the heap's byte counter when this object is freed. `u32`:
    /// a single object cannot meaningfully exceed 4 GiB; setters saturate.
    size: Cell<u32>,
}

/// Post-#113 the header carries no pointer, so the 8-byte size is valid on
/// every target width, not just 64-bit (R1 finding 11 / spec ex-W3). Alignment
/// is 4, from the `u32`.
const _: () = assert!(std::mem::size_of::<GcHeader>() == 8);

const HDR_FINALIZED: u8 = 1;
const HDR_COLLECTED: u8 = 2;
const HDR_GRAY_LISTED: u8 = 4;
/// Set on every box a `Heap` owns — both the sweepable `allocate` path and
/// the never-swept `allocate_uncollected` bootstrap path — and never on
/// detached `Gc::new_uncollected` boxes. Distinct from `HDR_COLLECTED`
/// (sweepable only): strict-guard checks need "will this box ever be freed
/// by a heap" (bootstrap boxes die in `drop_all`, so a guard-less weak
/// handle to one dangles after heap teardown), not "is it sweepable".
const HDR_HEAP_OWNED: u8 = 16;
/// Set by sweep under `LUA_RS_GC_QUARANTINE=1` instead of freeing the box.
/// Debug builds assert this bit is clear on every `Gc` dereference and on
/// every `Marker::mark_box` visit, turning use-after-sweep into a
/// deterministic panic with a backtrace (see `Heap::release_box`).
const HDR_FREED: u8 = 8;

impl GcHeader {
    fn new_white(size: usize, color: Color, flags: u8) -> Self {
        Self {
            color: Cell::new(color),
            age: Cell::new(GcAge::New),
            flags: Cell::new(flags),
            size: Cell::new(size.min(u32::MAX as usize) as u32),
        }
    }

    fn flag(&self, bit: u8) -> bool {
        self.flags.get() & bit != 0
    }

    fn set_flag(&self, bit: u8, on: bool) {
        let f = self.flags.get();
        self.flags.set(if on { f | bit } else { f & !bit });
    }

    pub fn finalized(&self) -> bool {
        self.flag(HDR_FINALIZED)
    }

    pub fn set_finalized(&self, finalized: bool) {
        self.set_flag(HDR_FINALIZED, finalized);
    }

    pub fn collected(&self) -> bool {
        self.flag(HDR_COLLECTED)
    }

    pub fn gray_listed(&self) -> bool {
        self.flag(HDR_GRAY_LISTED)
    }

    pub fn set_gray_listed(&self, listed: bool) {
        self.set_flag(HDR_GRAY_LISTED, listed);
    }

    pub fn size(&self) -> usize {
        self.size.get() as usize
    }

    pub fn set_size(&self, size: usize) {
        self.size.set(size.min(u32::MAX as usize) as u32);
    }
}

/// A heap-allocated, GC-tracked value plus its header.
#[repr(C)]
pub struct GcBox<T: ?Sized> {
    header: GcHeader,
    value: T,
}

impl<T: ?Sized> GcBox<T> {
    pub fn header(&self) -> &GcHeader {
        &self.header
    }
    pub fn value(&self) -> &T {
        &self.value
    }
}

/// A GC-managed pointer. Copy + Clone (one-machine-word). Replaces `GcRef<T>`.
pub struct Gc<T: ?Sized> {
    ptr: NonNull<GcBox<T>>,
    /// Marker so `Gc<T>` is treated as if it owns `T` for variance.
    _marker: PhantomData<T>,
}

// SAFETY: Gc is just a pointer. The Cell-based interior mutability of the
// header is single-threaded (the entire Lua runtime is single-threaded), so
// no Send/Sync impls are needed and we don't provide them.
impl<T: ?Sized> Copy for Gc<T> {}
impl<T: ?Sized> Clone for Gc<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: ?Sized> PartialEq for Gc<T> {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::addr_eq(self.ptr.as_ptr(), other.ptr.as_ptr())
    }
}
impl<T: ?Sized> Eq for Gc<T> {}

impl<T: ?Sized> std::hash::Hash for Gc<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.ptr.as_ptr().hash(state)
    }
}

impl<T: ?Sized> std::fmt::Debug for Gc<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Gc({:p})", self.ptr.as_ptr())
    }
}

impl<T: Trace + 'static> Gc<T> {
    /// Allocate a `GcBox<T>` outside any heap registry. Used by legacy
    /// `GcRef::new` call sites until Phase D-1b migrates them. The returned
    /// `Gc<T>` is reachable only through the caller's own retention path;
    /// without joining a heap owner list, it will never be swept (so
    /// effectively leaks until process exit — same as Rc behavior).
    pub fn new_uncollected(value: T) -> Self {
        DETACHED_ALLOCATIONS.with(|c| c.set(c.get() + 1));
        let size = std::mem::size_of::<T>();
        let boxed = Box::new(GcBox {
            header: GcHeader::new_white(size, Color::White0, 0),
            value,
        });
        Gc {
            ptr: NonNull::new(Box::into_raw(boxed)).expect("Box::into_raw is non-null"),
            _marker: PhantomData,
        }
    }

    /// Erased heap-list pointer for collector-owned intrusive bookkeeping.
    pub fn as_trace_ptr(self) -> NonNull<GcBox<dyn Trace>> {
        self.ptr
    }
}

impl<T: ?Sized> Gc<T> {
    /// Two `Gc<T>`s are identity-equal iff they point at the same box.
    pub fn ptr_eq(a: Self, b: Self) -> bool {
        std::ptr::addr_eq(a.ptr.as_ptr(), b.ptr.as_ptr())
    }

    /// Identity as a usize — usable as a hash table key for "is the *same
    /// object*" lookups.
    pub fn identity(self) -> usize {
        self.ptr.as_ptr() as *const () as usize
    }

    /// Access the underlying value. Returns `&T` so callers can read fields
    /// without taking the `Gc` apart. Interior mutability lives inside T's
    /// own fields (Cell, RefCell, etc.).
    fn as_box(&self) -> &GcBox<T> {
        // SAFETY: A Gc<T> is constructed only by allocate() or
        // new_uncollected(), both of which produce a valid GcBox. The box
        // outlives the Gc until sweep, which only frees boxes not reachable
        // from any root — so as long as this Gc is on the stack or in a
        // traced field, the box is live.
        let bx = unsafe { self.ptr.as_ref() };
        debug_assert!(
            !bx.header.flag(HDR_FREED),
            "use-after-sweep: Gc<{}> dereferenced after the collector swept it \
             (caught by LUA_RS_GC_QUARANTINE; this is a rooting bug — the object \
             was reachable by execution but not by the root trace)",
            std::any::type_name::<T>()
        );
        bx
    }

    fn header(&self) -> &GcHeader {
        &self.as_box().header
    }

    /// True iff this box is linked into a sweepable heap owner list
    /// (`HDR_COLLECTED` set) — the collector may free it during the owning
    /// heap's lifetime. Detached (`new_uncollected`) boxes and heap-owned
    /// bootstrap (`allocate_uncollected`) boxes report false.
    pub fn is_heap_tracked(self) -> bool {
        self.header().collected()
    }

    /// True iff some `Heap` will free this box — during collection
    /// (`allocate`) or at teardown in `drop_all` (`allocate_uncollected`).
    /// Only detached `Gc::new_uncollected` boxes report false. Strict-guard
    /// mode keys on this: a guard-less weak handle or dropped buffer charge
    /// is hazardous for any heap-owned box (a bootstrap box's weak handle
    /// dangles after heap teardown just the same), while the detached
    /// process-lifetime path is the sanctioned legacy behavior.
    pub fn is_heap_owned(self) -> bool {
        self.header().flag(HDR_HEAP_OWNED)
    }

    pub fn color(self) -> Color {
        self.header().color.get()
    }

    pub fn set_color(self, color: Color) {
        self.header().color.set(color);
    }

    pub fn age(self) -> GcAge {
        self.header().age.get()
    }

    pub fn set_age(self, age: GcAge) {
        self.header().age.set(age);
    }

    pub fn is_finalized(self) -> bool {
        self.header().finalized()
    }

    pub fn set_finalized(self, finalized: bool) {
        self.header().set_finalized(finalized);
    }

    /// Charge (`delta > 0`) or refund (`delta < 0`) `delta` bytes of this
    /// object's owned heap buffers against the pacer, keeping `header.size`
    /// as the single source of truth for what sweep will refund.
    ///
    /// No-op when `delta == 0` or when this box is not on a heap owner list
    /// (`collected == false`): an uncollected box is never swept, so charging
    /// it would permanently inflate the byte counter. On the collected path,
    /// `header.size` and the heap's byte counter move together, so after sweep
    /// frees this box it refunds exactly the bytes that were charged here.
    pub fn account_buffer(&self, heap: &Heap, delta: isize) {
        if delta == 0 {
            return;
        }
        let header = self.header();
        if !header.collected() {
            return;
        }
        if delta >= 0 {
            header.set_size(header.size().saturating_add(delta as usize));
        } else {
            header.set_size(header.size().saturating_sub((-delta) as usize));
        }
        heap.adjust_bytes(delta);
    }
}

impl<T: ?Sized> std::ops::Deref for Gc<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.as_box().value
    }
}

impl<T: ?Sized> AsRef<T> for Gc<T> {
    fn as_ref(&self) -> &T {
        &self.as_box().value
    }
}

/// Every GC-rooted type implements `Trace` to expose its `Gc<_>` fields.
///
/// The `trace` method visits every reachable `Gc<_>` and calls
/// `Marker::mark` on it. For container fields (`Vec<LuaValue>`, etc.) call
/// `field.trace(m)` to delegate.
///
/// # Mechanical pattern
///
/// ```ignore
/// impl Trace for LuaTable {
///     fn trace(&self, m: &mut Marker) {
///         for v in self.array.iter() { v.trace(m); }
///         if let Some(mt) = self.metatable { m.mark(mt); }
///     }
/// }
/// ```
pub trait Trace {
    fn trace(&self, m: &mut Marker);

    /// Concrete Rust type name for diagnostic/testC telemetry
    /// ([`Heap::type_name_count`]). Collector behavior must not branch on
    /// this. The default covers container blanket impls, which are never
    /// GC-boxed directly; concrete runtime types override it with
    /// `std::any::type_name::<Self>()`.
    fn type_name(&self) -> &'static str {
        "unknown"
    }
}

// Common blanket impls so most container types Just Work.
impl<T: Trace> Trace for Vec<T> {
    fn trace(&self, m: &mut Marker) {
        for item in self.iter() {
            item.trace(m);
        }
    }
}

impl<T: Trace> Trace for Option<T> {
    fn trace(&self, m: &mut Marker) {
        if let Some(v) = self {
            v.trace(m);
        }
    }
}

impl<T: Trace + ?Sized> Trace for Box<T> {
    fn trace(&self, m: &mut Marker) {
        (**self).trace(m);
    }
}

impl<T: Trace + ?Sized> Trace for std::rc::Rc<T> {
    fn trace(&self, m: &mut Marker) {
        (**self).trace(m);
    }
}

impl<T: Trace> Trace for RefCell<T> {
    fn trace(&self, m: &mut Marker) {
        self.borrow().trace(m);
    }
}

/// `Gc<T>` is itself traceable: marking it forwards to the contained `T`.
impl<T: Trace + 'static> Trace for Gc<T> {
    fn trace(&self, m: &mut Marker) {
        m.mark(*self);
    }
}

// Trivially-traceable primitive types: visiting does nothing.
macro_rules! trace_noop {
    ($($t:ty),*) => {
        $(impl Trace for $t {
            fn trace(&self, _m: &mut Marker) {}
        })*
    };
}
trace_noop!(
    bool, u8, u16, u32, u64, u128, usize, i8, i16, i32, i64, i128, isize, f32, f64, char, String,
    str
);

impl<T> Trace for std::marker::PhantomData<T> {
    fn trace(&self, _m: &mut Marker) {}
}

/// Diagnostic counters for the latest mark phase.
///
/// These are read-only telemetry for testC/canaries and unit tests. Collector
/// decisions must continue to use object color/age metadata, not these counts.
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq)]
pub struct MarkerStats {
    pub marked: usize,
    pub marked_young: usize,
    pub marked_old: usize,
    pub traced: usize,
    pub traced_young: usize,
    pub traced_old: usize,
}

/// Diagnostic counters for the latest sweep phase.
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq)]
pub struct SweepStats {
    pub visited: usize,
    pub visited_young: usize,
    pub visited_old: usize,
    pub revisit: usize,
    pub freed: usize,
    pub freed_bytes: usize,
}

impl SweepStats {
    fn record_visit(&mut self, age: GcAge) {
        self.visited += 1;
        if age.is_old() {
            self.visited_old += 1;
        } else {
            self.visited_young += 1;
        }
    }

    fn record_free(&mut self, bytes: usize) {
        self.freed += 1;
        self.freed_bytes += bytes;
    }

    fn add(&mut self, other: Self) {
        self.visited += other.visited;
        self.visited_young += other.visited_young;
        self.visited_old += other.visited_old;
        self.revisit += other.revisit;
        self.freed += other.freed;
        self.freed_bytes += other.freed_bytes;
    }
}

struct OldRevisitTracker {
    old_revisit_ids: Vec<usize>,
    processed_ids: Vec<usize>,
}

impl OldRevisitTracker {
    fn new(old_revisit: &[NonNull<GcBox<dyn Trace>>]) -> Option<Self> {
        if old_revisit.is_empty() {
            return None;
        }
        let mut old_revisit_ids: Vec<usize> = old_revisit
            .iter()
            .map(|ptr| ptr.as_ptr() as *const () as usize)
            .collect();
        old_revisit_ids.sort_unstable();
        old_revisit_ids.dedup();
        Some(Self {
            old_revisit_ids,
            processed_ids: Vec::new(),
        })
    }

    #[inline(always)]
    fn record_processed(&mut self, id: usize) {
        if self.old_revisit_ids.binary_search(&id).is_ok() {
            self.processed_ids.push(id);
        }
    }

    fn finish(&mut self) {
        self.processed_ids.sort_unstable();
        self.processed_ids.dedup();
    }

    #[inline(always)]
    fn was_processed(&self, id: usize) -> bool {
        self.processed_ids.binary_search(&id).is_ok()
    }
}

/// Diagnostic counts for the allgc list split by generational cursors.
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq)]
pub struct AllGcCohortStats {
    pub new: usize,
    pub survival: usize,
    pub old1: usize,
    pub old: usize,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum MarkerMode {
    Full,
    Minor,
}

/// Holds the gray queue during a mark phase. Passed to `Trace::trace`.
pub struct Marker {
    gray_queue: Vec<NonNull<GcBox<dyn Trace>>>,
    visited: IdentityHashSet,
    stats: MarkerStats,
    mode: MarkerMode,
}

impl Marker {
    fn new_with_capacity(mode: MarkerMode, capacity: usize) -> Self {
        Self {
            gray_queue: Vec::with_capacity(256),
            visited: IdentityHashSet::with_capacity_and_hasher(
                capacity,
                IdentityBuildHasher::default(),
            ),
            stats: MarkerStats::default(),
            mode,
        }
    }

    fn should_trace_age(&self, age: GcAge) -> bool {
        match self.mode {
            MarkerMode::Full => true,
            MarkerMode::Minor => !matches!(age, GcAge::Old),
        }
    }

    /// Mark a `Gc<T>` as gray (reachable, but its outgoing edges not yet
    /// traced). Called by `Trace::trace` implementations.
    ///
    /// Per-cycle dedup uses `visited` (a HashSet of box identities) rather
    /// than the color flag. Color-based dedup would silently skip
    /// `new_uncollected` boxes left Black by the previous cycle — those
    /// allocations are NOT on a heap owner list, so the start-of-mark
    /// "reset heap-owned objects to White" loop does not reach them, and a Black
    /// uncollected box would be skipped without re-tracing its children
    /// (causing reachable allgc descendants to be swept). The visited set
    /// is rebuilt every `full_collect` (Marker::new), so this dedup is
    /// always per-cycle.
    pub fn mark<T: Trace + 'static>(&mut self, gc: Gc<T>) {
        let ptr: NonNull<GcBox<dyn Trace>> = gc.ptr;
        self.mark_box(ptr, gc.header(), gc.identity());
    }

    fn mark_box(&mut self, ptr: NonNull<GcBox<dyn Trace>>, header: &GcHeader, id: usize) {
        debug_assert!(
            !header.flag(HDR_FREED),
            "GC marker reached a quarantined (swept) object at {id:#x} — a root \
             traced a stale GcRef (caught by LUA_RS_GC_QUARANTINE; bug-B class: \
             garbage slot fed into the marker)"
        );
        if self.visited.insert(id) {
            let age = header.age.get();
            self.stats.marked += 1;
            if age.is_old() {
                self.stats.marked_old += 1;
            } else {
                self.stats.marked_young += 1;
            }
            if self.should_trace_age(age) {
                header.color.set(Color::Gray);
                self.gray_queue.push(ptr);
            }
        }
    }

    /// Record that an Rc-backed object (`GcRef<T>` in Phase A-D-0) has been
    /// visited and return whether this is the first visit. Used by recursive
    /// `Trace` impls to break cycles while the real `Gc<T>` gray-queue path is
    /// not yet wired (e.g. `_G._G == _G` would otherwise infinitely recurse).
    pub fn try_visit(&mut self, addr: usize) -> bool {
        self.visited.insert(addr)
    }

    /// True iff `id` was reached during the mark phase. Used by the
    /// post-mark hook (`Heap::full_collect_with_post_mark`) to decide whether
    /// a weak-table entry's target is still strongly reachable. Read-only —
    /// callers cannot add entries.
    pub fn is_visited(&self, id: usize) -> bool {
        self.visited.contains(&id)
    }

    /// True when the object was marked in this cycle, or when a minor cycle
    /// deliberately skipped an old object that the young sweep will not free.
    pub fn is_marked_or_old<T: Trace + 'static>(&self, gc: Gc<T>) -> bool {
        self.is_visited(gc.identity())
            || (matches!(self.mode, MarkerMode::Minor) && gc.age().is_old())
    }

    /// Number of objects marked so far. Used by the post-mark hook's
    /// ephemeron-convergence fixed-point loop to detect when an iteration
    /// added no new reachable objects and the loop can terminate.
    pub fn visited_count(&self) -> usize {
        self.visited.len()
    }

    /// Return diagnostic counters for the current mark phase.
    pub fn stats(&self) -> MarkerStats {
        self.stats
    }

    /// Drain the gray queue, transitively marking children. Each gray box
    /// becomes black; its `Trace::trace` is called so the children it points
    /// at get pushed onto the queue. Repeats until the queue is empty.
    ///
    /// Exposed for the post-mark hook so it can run ephemeron convergence:
    /// after marking new values via [`Marker::mark`] (or `value.trace(self)`),
    /// the hook calls `drain_gray_queue` to propagate the new reachability,
    /// then re-checks for fixed-point.
    pub fn drain_gray_queue(&mut self) {
        while let Some(gray_ptr) = self.gray_queue.pop() {
            unsafe {
                let bx = gray_ptr.as_ref();
                self.stats.traced += 1;
                if bx.header.age.get().is_old() {
                    self.stats.traced_old += 1;
                } else {
                    self.stats.traced_young += 1;
                }
                bx.header.color.set(Color::Black);
                bx.value.trace(self);
            }
        }
    }
}

/// Phases of the incremental collection cycle.
///
/// The state machine matches a simplified subset of C-Lua's `lgc.c` FSM and
/// is driven by [`Heap::incremental_step_with_post_mark`].
///
/// Transitions:
/// - `Pause` → `Propagate` (on first step: reset colors, trace roots).
/// - `Propagate` → `EnterAtomic` (when the gray queue empties).
/// - `EnterAtomic` → `Atomic` (atomic phase is about to run).
/// - `Atomic` → `SweepAllGc` (post-mark hook has run; sweep cursor is initialized).
/// - `SweepAllGc` → `SweepFinObj` (allgc sweep cursor reached the end).
/// - `SweepFinObj` → `SweepToBeFnz` (finobj sweep cursor reached the end).
/// - `SweepToBeFnz` → `SweepEnd` (tobefnz sweep cursor reached the end).
/// - `SweepEnd` → `CallFin` (finish sweep bookkeeping).
/// - `CallFin` → `Pause` (cycle is complete).
///
/// `Collecting` is kept as a compatibility alias for the old API (used by
/// `barrier`) — it means "anything but Pause."
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum GcState {
    Pause,
    Propagate,
    EnterAtomic,
    Atomic,
    SweepAllGc,
    SweepFinObj,
    SweepToBeFnz,
    SweepEnd,
    CallFin,
}

impl GcState {
    pub fn is_pause(self) -> bool {
        matches!(self, GcState::Pause)
    }
    pub fn is_propagate(self) -> bool {
        matches!(self, GcState::Propagate)
    }
    pub fn is_invariant(self) -> bool {
        matches!(
            self,
            GcState::Propagate | GcState::EnterAtomic | GcState::Atomic
        )
    }
    pub fn is_sweep(self) -> bool {
        matches!(
            self,
            GcState::SweepAllGc | GcState::SweepFinObj | GcState::SweepToBeFnz | GcState::SweepEnd
        )
    }
}

/// Outcome of one call to [`Heap::incremental_step_with_post_mark`].
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum StepOutcome {
    /// The step finished a cycle. The heap is back at `GcState::Pause`.
    Paused,
    /// The step performed work but the cycle is not finished. Caller may
    /// step again.
    InProgress,
    /// The heap is paused (via [`Heap::pause`]) or the caller asked for zero
    /// budget while no cycle was in progress and no work was needed.
    SkippedStopped,
}

/// Work budget for one incremental step.
///
/// `remaining_work` counts down by one for each unit of work performed (one
/// gray object traced, one swept node visited, one finalizer dispatched).
/// `max_credit` caps how negative `remaining_work` may be allowed to go — a
/// step that overshoots its budget rolls the overrun into the caller's debt
/// rather than letting unbounded work happen in one call.
#[derive(Copy, Clone, Debug)]
pub struct StepBudget {
    pub remaining_work: isize,
    pub max_credit: isize,
}

impl StepBudget {
    /// Build a budget from a number of allowed work units.
    pub fn from_work(work: isize) -> Self {
        Self {
            remaining_work: work.max(1),
            max_credit: work.max(1),
        }
    }
}

/// The heap. One per `GlobalState`. Owns the intrusive allgc list of every
/// allocated `GcBox`, tracks total bytes, and runs collections.
/// Floor for the post-collection threshold. Without it, a tight
/// allocation loop drives the live set near zero, so `bytes * pause/100`
/// collapses toward zero and a full stop-the-world collection fires every
/// few allocations, re-tracing all roots each time (issue #38, GC path).
/// The floor bounds the wasted work: the collector waits until at least
/// this many bytes of garbage accumulate before collecting a small heap.
///
/// Raised from 256 KB to 1 MB once table array/node buffer bytes became
/// honestly accounted (see [`Gc::account_buffer`]): with real buffer bytes
/// flowing into the pacer, a 256 KB floor fires too eagerly on table-heavy
/// workloads, re-tracing all roots each time. 1 MB keeps the small-heap
/// over-collection guard while letting honest pressure drive the threshold.
const GC_MIN_THRESHOLD: usize = 1024 * 1024;

/// Tombstone-density trigger denominator: compact when `tombstones * 4 >
/// slots.len()` — i.e. once tombstones exceed 25% of physical slots (#113
/// Wave 2, spec "The one mutation rule"). A named constant so measurement can
/// retune it without hunting a literal.
const OWNERVEC_TOMBSTONE_DENOM: usize = 4;

/// Owner-class vector: the #113 Wave 2 replacement for one intrusive heap
/// owner list (`head`/`finobj`/`tobefnz`). Each live heap-owned box is
/// referenced by exactly one `Some` slot; removal writes a `None` tombstone
/// rather than shifting, so a slot index stays valid for the entire lifetime
/// of a sweep cursor over this vector. Compaction — the only operation that
/// relocates slots — runs only at points with no live cursor for this vector.
///
/// `reallyold`/`old1`/`survival` are **physical slot-index cohort boundaries**
/// (they count slots, tombstoned or not): `[0, reallyold)` is the old/reallyold
/// cohort, `[reallyold, reallyold+old1)` old1, `[reallyold+old1, +survival)`
/// survival, and the remainder `[reallyold+old1+survival, len)` is the nursery.
/// A tombstone below a boundary does not move it; only [`compact`](OwnerVec::compact)
/// recounts boundaries to dense indices. `tobefnz` keeps all three at zero
/// (its whole extent is scanned every minor), so it uses only the tombstone
/// half of the machinery.
struct OwnerVec {
    slots: Vec<Option<NonNull<GcBox<dyn Trace>>>>,
    tombstones: usize,
    reallyold: usize,
    old1: usize,
    survival: usize,
}

impl Default for OwnerVec {
    fn default() -> Self {
        Self {
            slots: Vec::new(),
            tombstones: 0,
            reallyold: 0,
            old1: 0,
            survival: 0,
        }
    }
}

impl OwnerVec {
    /// Physical slot count including tombstones (the sweep watermark domain).
    fn len(&self) -> usize {
        self.slots.len()
    }

    /// Live (non-tombstone) member count.
    fn live_len(&self) -> usize {
        self.slots.len() - self.tombstones
    }

    /// Physical index where the survival cohort begins — the first slot a
    /// minor sweep scans (survival cohort + nursery = `slots[young_start()..]`).
    fn young_start(&self) -> usize {
        self.reallyold + self.old1
    }

    /// Append a newest object at the tail (nursery). Cohort boundaries are
    /// unchanged — the tail is always beyond the survival cohort. One amortized
    /// push replacing the intrusive list's two `Cell` writes.
    fn push(&mut self, ptr: NonNull<GcBox<dyn Trace>>) {
        self.slots.push(Some(ptr));
    }

    /// Linear membership scan for the slot holding `ptr`. Cold path (moves and
    /// nothing else); the density policy bounds `slots.len()` near
    /// `4/3 * live_len` at no-cursor times.
    fn find(&self, ptr: NonNull<GcBox<dyn Trace>>) -> Option<usize> {
        self.slots.iter().position(|slot| match slot {
            Some(p) => std::ptr::addr_eq(p.as_ptr(), ptr.as_ptr()),
            None => false,
        })
    }

    /// Tombstone the slot at `index`: write `None`, bump the counter. Boundaries
    /// do not move — they are physical indices until the next compaction.
    fn tombstone_at(&mut self, index: usize) {
        debug_assert!(
            self.slots[index].is_some(),
            "OwnerVec::tombstone_at on an already-empty slot {index}"
        );
        self.slots[index] = None;
        self.tombstones += 1;
    }

    /// Find `ptr` and tombstone its slot, returning the index; `None` if `ptr`
    /// is not a member.
    fn tombstone(&mut self, ptr: NonNull<GcBox<dyn Trace>>) -> Option<usize> {
        let index = self.find(ptr)?;
        self.tombstone_at(index);
        Some(index)
    }

    /// True when tombstone density exceeds 25% — the threshold-compaction
    /// trigger checked at `start_cycle` and after a tombstoning move outside a
    /// sweep phase.
    fn over_density(&self) -> bool {
        self.tombstones * OWNERVEC_TOMBSTONE_DENOM > self.slots.len()
    }

    /// Whole-vector, order-preserving compaction (the single compaction
    /// algorithm — the slice-only variant was withdrawn, R2 finding 2): drop
    /// every `None` slot, zero the tombstone counter, and recount each cohort
    /// boundary as the number of surviving slots below its old physical index.
    /// Dense afterwards, cohort membership preserved. Idempotent on an already
    /// dense vector (no tombstones → early return).
    fn compact(&mut self) {
        if self.tombstones == 0 {
            return;
        }
        let b1 = self.reallyold;
        let b2 = self.reallyold + self.old1;
        let b3 = self.reallyold + self.old1 + self.survival;
        let mut new_reallyold = 0;
        let mut new_old1 = 0;
        let mut new_survival = 0;
        let mut write = 0;
        for read in 0..self.slots.len() {
            if let Some(ptr) = self.slots[read] {
                if read < b1 {
                    new_reallyold += 1;
                } else if read < b2 {
                    new_old1 += 1;
                } else if read < b3 {
                    new_survival += 1;
                }
                self.slots[write] = Some(ptr);
                write += 1;
            }
        }
        self.slots.truncate(write);
        self.tombstones = 0;
        self.reallyold = new_reallyold;
        self.old1 = new_old1;
        self.survival = new_survival;
    }

    /// Cap retained capacity near `2 * live` at mandatory compaction points —
    /// `Vec::truncate` never shrinks capacity, so a spike would pin memory
    /// forever otherwise. Leaves 1.5x headroom against shrink-grow thrash.
    fn shrink_high_water(&mut self) {
        let len = self.slots.len();
        if self.slots.capacity() > 2 * len {
            self.slots.shrink_to(len + len / 2);
        }
    }

    /// Rotate cohort counts after a minor sweep's scan + compaction: the old1
    /// cohort merges into reallyold, the survival cohort becomes old1, and the
    /// nursery survivors become survival — mirroring `finish_minor_collection`'s
    /// `reallyold += old1; old1 = survival; survival = new`. Requires a
    /// compacted (tombstone-free) vector so `len` is a dense count.
    fn rotate_after_minor(&mut self) {
        debug_assert_eq!(
            self.tombstones, 0,
            "OwnerVec::rotate_after_minor requires a compacted vector"
        );
        let nursery = self
            .slots
            .len()
            .saturating_sub(self.reallyold + self.old1 + self.survival);
        self.reallyold += self.old1;
        self.old1 = self.survival;
        self.survival = nursery;
    }

    /// Promote every live object into the old/reallyold cohort (generational
    /// mode entry). Compacts first so `len` is dense, then sets the whole
    /// vector as the old prefix.
    fn promote_all_old(&mut self) {
        self.compact();
        self.reallyold = self.slots.len();
        self.old1 = 0;
        self.survival = 0;
    }

    /// Reset cohort boundaries to zero (all objects become nursery again).
    /// Does not touch slots or tombstones.
    fn clear_cohorts(&mut self) {
        self.reallyold = 0;
        self.old1 = 0;
        self.survival = 0;
    }

    /// Count live (non-tombstone) slots per cohort as `(nursery, survival, old1,
    /// reallyold)`. Diagnostic (lua-cli telemetry) only.
    fn cohort_live_counts(&self) -> (usize, usize, usize, usize) {
        let b1 = self.reallyold;
        let b2 = self.reallyold + self.old1;
        let b3 = self.reallyold + self.old1 + self.survival;
        let mut nursery = 0;
        let mut survival = 0;
        let mut old1 = 0;
        let mut reallyold = 0;
        for (i, slot) in self.slots.iter().enumerate() {
            if slot.is_some() {
                if i < b1 {
                    reallyold += 1;
                } else if i < b2 {
                    old1 += 1;
                } else if i < b3 {
                    survival += 1;
                } else {
                    nursery += 1;
                }
            }
        }
        (nursery, survival, old1, reallyold)
    }

    /// Take the slots out for teardown, resetting all bookkeeping so the
    /// vector is empty-and-consistent afterwards (a re-entrant allocation
    /// during the drain lands in a fresh vector).
    fn take_slots(&mut self) -> Vec<Option<NonNull<GcBox<dyn Trace>>>> {
        self.tombstones = 0;
        self.reallyold = 0;
        self.old1 = 0;
        self.survival = 0;
        std::mem::take(&mut self.slots)
    }
}

pub struct Heap {
    /// The allgc owner-class vector (heap-owned objects not currently
    /// registered for finalization). #113 Wave 2 replaced the intrusive
    /// `head` list + `survival`/`old1`/`reallyold`/`firstold1` cursor cells
    /// with an [`OwnerVec`] whose three cohort counts *are* the generational
    /// boundaries. `firstold1` is deleted outright — it was written and
    /// cursor-corrected but read by no collector decision (only one test
    /// assert), so its removal is behavior-neutral (changelog note).
    allgc: RefCell<OwnerVec>,
    /// The finobj owner-class vector (objects registered for `__gc`). Replaced
    /// the intrusive `finobj` list + `finobjsur`/`finobjold1`/`finobjrold`
    /// cursors; its cohort counts are the finobj generational boundaries.
    finobj: RefCell<OwnerVec>,
    /// The tobefnz owner-class vector (objects awaiting `__gc`). An `OwnerVec`
    /// for the tombstone/membership machinery only — its cohort counts stay
    /// permanently zero because the young sweep scans its whole extent. The
    /// typed `FinalizerRegistry` is the semantic FIFO authority for `__gc`
    /// order; this vector is a membership mirror.
    tobefnz: RefCell<OwnerVec>,
    /// Total bytes allocated (sum of header sizes; rough).
    bytes: Cell<usize>,
    /// Number of currently heap-owned GC boxes across all owner lists.
    objects: Cell<usize>,
    /// White bit used for new allocations and for survivors after a sweep.
    current_white: Cell<Color>,
    /// Heap-owned allocation tokens keyed by box address. Weak handles store
    /// these tokens so address reuse after sweep cannot resurrect a stale weak
    /// target.
    allocation_tokens: RefCell<IdentityHashMap<usize>>,
    /// Next non-zero token for a collected allocation.
    next_allocation_token: Cell<usize>,
    /// Threshold above which `step` triggers a collection.
    threshold: Cell<usize>,
    /// HARDMEMTESTS-style stress mode (env `LUA_RS_GC_STRESS=1`, read once
    /// at construction): `would_collect` fires at every checkpoint, so a
    /// collection happens at essentially every allocation boundary. Turns
    /// GC-cadence-dependent anchoring bugs (objects reachable from Rust
    /// locals but not from roots) into deterministic failures — pair with
    /// an ASAN build. Debug instrument only; never set in benchmarks.
    stress: bool,
    /// Quarantine mode (env `LUA_RS_GC_QUARANTINE=1`, read once at
    /// construction): sweep unlinks dead boxes but parks them on the
    /// `quarantined` list with `HDR_FREED` set instead of freeing, so a
    /// use-after-sweep dereference reads intact (still-allocated) memory and
    /// trips a `debug_assert` in `Gc::as_box` / `Marker::mark_box` — a
    /// deterministic Rust panic with a backtrace, no ASAN or nightly needed.
    /// All sweep accounting (byte refunds, token removal, object counts) is
    /// unchanged so collection cadence is identical to a normal run. Memory
    /// grows without bound; debug-build test instrument only — the asserts
    /// compile out in release. Pair with `LUA_RS_GC_STRESS=1`.
    quarantine: bool,
    /// Swept-but-not-freed boxes under quarantine mode. Append-only during
    /// life (`release_box` parks here with `HDR_FREED`), drained only in
    /// `drop_all` — so a plain `Vec`, no tombstones or cohorts (#113 Wave 2;
    /// was an intrusive list through `header.next`).
    quarantined: RefCell<Vec<NonNull<GcBox<dyn Trace>>>>,
    /// Boxes allocated via [`allocate_uncollected`](Self::allocate_uncollected)
    /// — heap-owned so `drop_all` frees them, but never linked into
    /// `allgc`/`finobj`/`tobefnz` so sweep never visits them (issue #249: this
    /// is what makes "never collected during the VM's life" not mean "leaked
    /// past the VM's life"). Append-only, drained only in `drop_all` — a plain
    /// `Vec`. Distinct from the true process-lifetime `Gc::new_uncollected`
    /// boxes, which carry no heap reference at all (allocated before any `Heap`
    /// exists, or with no `HeapGuard` active).
    uncollected: RefCell<Vec<NonNull<GcBox<dyn Trace>>>>,
    /// Set once at the top of [`drop_all`](Self::drop_all) and never cleared:
    /// this heap has begun (or completed) teardown. This is the durable half
    /// of a deliberate two-state distinction with [`tearing_down`](Self::tearing_down),
    /// which is true *only* while the drain loop runs. A post-close
    /// [`allocate`](Self::allocate) / [`allocate_uncollected`](Self::allocate_uncollected)
    /// panics precisely when `closed && !tearing_down` — the signature of a
    /// [`HeapGuard`] that outlived `close()` and tried to allocate into a
    /// torn-down heap. Read through [`is_closed`](Self::is_closed).
    closed: Cell<bool>,
    /// True for the duration of the [`drop_all`](Self::drop_all) drain loop
    /// only, and the transient half of the two-state teardown distinction with
    /// [`closed`](Self::closed). Re-entrant allocation from a payload `Drop`
    /// running *inside* the drain is legitimate — drain-until-stable frees
    /// whatever a destructor allocates on a later pass — so the closed-heap
    /// allocation panic is suppressed while this is set. It re-arms the instant
    /// the final (empty) pass completes and this clears.
    tearing_down: Cell<bool>,
    /// True for the duration of a sweep/minor `pending_release` drain (#113
    /// Wave 2 destruction ownership, R2 finding 1). While set, every collection
    /// entry point is inert (folded into [`collection_inert`](Self::collection_inert)),
    /// so a payload `Drop` running inside the drain cannot start a nested
    /// collection that traces a still-`pending_release` box as a root and then
    /// has the outer drain free it. A distinct, transient window from `closed`
    /// (permanent teardown) and `tearing_down` (the teardown drain); restored by
    /// an RAII guard so a caught panic cannot wedge the collector shut.
    releasing: Cell<bool>,
    /// While non-zero, [`allocate`](Self::allocate) routes through
    /// [`allocate_uncollected`](Self::allocate_uncollected) instead of the
    /// normal collectable `head` list. Raised around VM construction
    /// (`new_state()` / stdlib install), where objects may not yet be
    /// reachable from a self-consistent root set even though `paused` has
    /// already been cleared partway through — so a step triggered by
    /// allocation pressure during setup must not sweep them. A depth rather
    /// than a flag so windows nest (an outer embedding-layer window survives
    /// an inner one closing). Zero by default: a bare `Heap::new()`
    /// (low-level GC tests) allocates normally. Behind an `Rc` so
    /// [`BootstrapScope`] can decrement it without holding a heap pointer
    /// (sound even if a scope outlives its heap).
    bootstrap_depth: std::rc::Rc<Cell<usize>>,
    /// Multiplier on bytes_used to set next threshold after collection.
    pause_multiplier: Cell<usize>,
    /// State machine for the incremental collector.
    state: Cell<GcState>,
    /// If true, `step` and `barrier` are no-ops (for bootstrap before the
    /// world is consistent).
    paused: Cell<bool>,
    /// Counter of completed collections performed (for diagnostics).
    collections: Cell<usize>,
    /// Counter of completed young-generation collections.
    minor_collections: Cell<usize>,
    /// Counter of explicit stop-the-world full-collection requests.
    full_collections: Cell<usize>,
    /// Diagnostic counters from the most recently completed mark phase.
    last_mark_stats: Cell<MarkerStats>,
    /// Diagnostic counters from the most recent sweep phase.
    last_sweep_stats: Cell<SweepStats>,
    /// Heap-owned grayagain revisit set: objects young collections must
    /// revisit even if they are not reached through normal roots (OLD0/OLD1
    /// and touched old objects). Non-owning — every entry is a live sweepable
    /// box, membership is a subset of the live sweepable set, and
    /// `HDR_GRAY_LISTED` is the O(1) membership/dedup bit. Replaced the
    /// `GcHeader::gray_next` intrusive link in #113 Wave 1, shrinking every
    /// object's header by one fat pointer; the `Vec` slot exists only while
    /// an object is on the revisit list.
    grayagain: RefCell<Vec<NonNull<GcBox<dyn Trace>>>>,
    /// Capacity-recycling scratch for [`sweep_young`](Self::sweep_young)'s
    /// next-revisit builder: the buffer drained from `grayagain` last minor
    /// is cleared and parked here, then reused as the builder next minor —
    /// the two buffers swap roles each cycle, so steady-state minors
    /// allocate nothing (spec rev-3 W1 buffer-reuse requirement). Always
    /// empty outside `sweep_young`; carries no membership and no flags.
    grayagain_scratch: RefCell<Vec<NonNull<GcBox<dyn Trace>>>>,
    /// In-progress marker state for incremental cycles. `Some` between
    /// `Propagate` start and `Sweep` start; `None` otherwise.
    marker: RefCell<Option<Marker>>,
    /// Recycled mark-phase buffers (gray queue + visited set). A mark phase
    /// sizes its visited set to the live-object count (hundreds of KB);
    /// without pooling, binarytrees churned 396 such buffers for 249 MB of
    /// allocator traffic (dhat, 2026-06-10). One buffer pair is kept and
    /// reused across cycles; capacity follows the heap's high-water mark.
    marker_pool: RefCell<Option<(Vec<NonNull<GcBox<dyn Trace>>>, IdentityHashSet)>>,
    /// Incremental-sweep cursor: the next slot index the budgeted sweep will
    /// examine in the OwnerVec selected by `state` (#113 Wave 2; replaced the
    /// raw `sweep_prev_next` cell pointer). Tombstones never shift indices, so
    /// this stays valid across mid-sweep moves; compaction runs only at
    /// no-cursor points, so it is never invalidated mid-sweep.
    sweep_index: Cell<usize>,
    /// Incremental-sweep watermark: the slot count of the currently-swept
    /// OwnerVec captured at phase entry. Slots appended at or beyond this index
    /// (mid-sweep `allocate`/moves) are never visited this cycle — the
    /// structural twin of the old cursor-behind-head behavior.
    sweep_watermark: Cell<usize>,
    /// Dead-in-transit owner (#113 Wave 2 destruction ownership, R2 finding 1):
    /// a sweep/minor scan tombstones a dead slot and transfers its single
    /// owning pointer here (a `RefCell` distinct from every owner vector), then
    /// the release drain pops one at a time — settling *that object's*
    /// accounting and calling `release_box` with no owner-vector borrow held.
    /// This keeps the exactly-one-owner invariant true *during* destruction and
    /// makes a payload `Drop` that panics leak nothing beyond the box in flight
    /// (the remainder is drained by the next step or by `drop_all`).
    pending_release: RefCell<Vec<NonNull<GcBox<dyn Trace>>>>,
    /// Lua 5.1 only: every userdata that has ever carried a metatable, so the
    /// collect-time finalizability scan (`luaC_separateudata`) can re-read its
    /// live metatable for a late-added `__gc`. Empty on 5.2–5.5, where
    /// finalizability is decided eagerly at `setmetatable`. Probes hold weak
    /// handles, so they never root the userdata; dead ones are pruned by
    /// [`scan_v51_finalizable`](Self::scan_v51_finalizable).
    v51_udata_roster: RefCell<Vec<std::rc::Rc<dyn Udata51Probe>>>,
}

impl Heap {
    /// Construct a heap behind `Rc` — the only ownership shape the guard
    /// and weak-handle machinery accept since issue #252. Everything that
    /// needs `&Heap` gets it by deref.
    ///
    /// Do not store a clone of this `Rc` inside anything the heap itself
    /// owns (a traced value, a userdata payload): that is a reference cycle
    /// and the heap will never drop. Collector-internal bookkeeping only
    /// ever holds `Weak<Heap>` (`HeapRef`) for this reason.
    pub fn new() -> std::rc::Rc<Self> {
        std::rc::Rc::new(Self {
            allgc: RefCell::new(OwnerVec::default()),
            finobj: RefCell::new(OwnerVec::default()),
            tobefnz: RefCell::new(OwnerVec::default()),
            bytes: Cell::new(0),
            objects: Cell::new(0),
            current_white: Cell::new(Color::White0),
            allocation_tokens: RefCell::new(IdentityHashMap::default()),
            next_allocation_token: Cell::new(1),
            threshold: Cell::new(64 * 1024), // initial threshold: 64 KB
            stress: std::env::var_os("LUA_RS_GC_STRESS").is_some_and(|v| v == "1"),
            quarantine: std::env::var_os("LUA_RS_GC_QUARANTINE").is_some_and(|v| v == "1"),
            quarantined: RefCell::new(Vec::new()),
            uncollected: RefCell::new(Vec::new()),
            closed: Cell::new(false),
            tearing_down: Cell::new(false),
            releasing: Cell::new(false),
            bootstrap_depth: std::rc::Rc::new(Cell::new(0)),
            pause_multiplier: Cell::new(200), // 200% = collect when bytes 2x threshold
            state: Cell::new(GcState::Pause),
            paused: Cell::new(true), // start paused; caller enables when world is consistent
            collections: Cell::new(0),
            minor_collections: Cell::new(0),
            full_collections: Cell::new(0),
            last_mark_stats: Cell::new(MarkerStats::default()),
            last_sweep_stats: Cell::new(SweepStats::default()),
            grayagain: RefCell::new(Vec::new()),
            grayagain_scratch: RefCell::new(Vec::new()),
            marker: RefCell::new(None),
            marker_pool: RefCell::new(None),
            sweep_index: Cell::new(0),
            sweep_watermark: Cell::new(0),
            pending_release: RefCell::new(Vec::new()),
            v51_udata_roster: RefCell::new(Vec::new()),
        })
    }

    /// Enable collection. Until this is called, `step` is a no-op (so the
    /// runtime can bootstrap without prematurely freeing objects).
    pub fn unpause(&self) {
        self.paused.set(false);
    }

    pub fn is_paused(&self) -> bool {
        self.paused.get()
    }

    /// True once [`drop_all`](Self::drop_all) has begun tearing this heap
    /// down. After this returns true, [`allocate`](Self::allocate) and
    /// [`allocate_uncollected`](Self::allocate_uncollected) panic — a
    /// [`HeapGuard`] that outlived `close()` cannot allocate into a
    /// torn-down heap — with one exception: re-entrant allocation from a
    /// destructor running inside the teardown drain itself, which the drain
    /// is designed to accept.
    pub fn is_closed(&self) -> bool {
        self.closed.get()
    }

    /// Enter bootstrap mode: [`allocate`](Self::allocate) routes new boxes
    /// through [`allocate_uncollected`](Self::allocate_uncollected) instead of
    /// the normal collectable list until the matching
    /// [`end_bootstrap`](Self::end_bootstrap) is called. Prefer the RAII
    /// [`bootstrap_scope`](Self::bootstrap_scope) — a manual `end_bootstrap`
    /// is skipped by early returns and error paths. See the `bootstrap_depth`
    /// field doc for why this exists separately from `paused`.
    pub fn begin_bootstrap(&self) {
        self.bootstrap_depth.set(
            self.bootstrap_depth
                .get()
                .checked_add(1)
                .expect("Heap bootstrap depth overflow"),
        );
    }

    /// Leave one level of bootstrap mode; once the depth returns to zero,
    /// subsequent `allocate` calls join the normal collectable `head` list.
    pub fn end_bootstrap(&self) {
        let depth = self.bootstrap_depth.get();
        debug_assert!(depth > 0, "Heap::end_bootstrap without begin_bootstrap");
        self.bootstrap_depth.set(depth.saturating_sub(1));
    }

    pub fn is_bootstrapping(&self) -> bool {
        self.bootstrap_depth.get() != 0
    }

    /// RAII form of [`begin_bootstrap`](Self::begin_bootstrap)/
    /// [`end_bootstrap`](Self::end_bootstrap): the returned scope ends the
    /// bootstrap window when dropped, so `?`-style early exits from VM
    /// construction cannot leave the heap stuck in bootstrap mode.
    ///
    /// Safe to hold past the heap's death (see [`BootstrapScope`]).
    pub fn bootstrap_scope(&self) -> BootstrapScope {
        self.begin_bootstrap();
        BootstrapScope {
            depth: std::rc::Rc::clone(&self.bootstrap_depth),
        }
    }

    /// Guard both allocation entry points against a closed heap. Fires only
    /// when the heap is `closed` and no longer `tearing_down` — i.e. `close()`
    /// (via `drop_all`) has fully completed and a [`HeapGuard`] that outlived
    /// it is trying to allocate. Re-entrant allocation from a destructor
    /// *during* the drain sees `tearing_down` set and is allowed through,
    /// because drain-until-stable frees whatever it allocates on a later pass.
    fn assert_open(&self) {
        if self.closed.get() && !self.tearing_down.get() {
            panic!("allocation into a closed heap — a HeapGuard outlived close()");
        }
    }

    /// Allocate a new `GcBox<T>` and prepend it to the allgc chain. While
    /// [`is_bootstrapping`](Self::is_bootstrapping) is true, delegates to
    /// [`allocate_uncollected`](Self::allocate_uncollected) instead.
    pub fn allocate<T: Trace + 'static>(&self, value: T) -> Gc<T> {
        self.assert_open();
        if self.is_bootstrapping() {
            return self.allocate_uncollected(value);
        }
        let size = std::mem::size_of::<GcBox<T>>();
        let boxed = Box::new(GcBox {
            header: GcHeader::new_white(
                size,
                self.current_white.get(),
                HDR_COLLECTED | HDR_HEAP_OWNED,
            ),
            value,
        });
        let raw: *mut GcBox<T> = Box::into_raw(boxed);
        let ptr: NonNull<GcBox<T>> = NonNull::new(raw).expect("Box::into_raw is non-null");
        let dyn_ptr: NonNull<GcBox<dyn Trace>> = ptr;
        self.allgc.borrow_mut().push(dyn_ptr);
        self.bytes.set(self.bytes.get() + size);
        self.objects.set(self.objects.get() + 1);
        Gc {
            ptr,
            _marker: PhantomData,
        }
    }

    /// Allocate a `GcBox<T>` owned by this heap but linked onto neither
    /// `head`, `finobj`, nor `tobefnz` — sweep never visits it, so it is
    /// never collected while the heap is alive (matching `Gc::new_uncollected`
    /// semantics for permanent roots), but it *is* linked onto the heap's
    /// `uncollected` list, so [`drop_all`](Self::drop_all) frees it when the
    /// heap shuts down instead of leaking it past the heap's lifetime.
    ///
    /// Does not charge `bytes`/`objects` — those drive collection pacing and
    /// diagnostics for the collectable set; an object that sweep never visits
    /// would inflate them permanently.
    pub fn allocate_uncollected<T: Trace + 'static>(&self, value: T) -> Gc<T> {
        self.assert_open();
        let size = std::mem::size_of::<GcBox<T>>();
        let boxed = Box::new(GcBox {
            header: GcHeader::new_white(size, self.current_white.get(), HDR_HEAP_OWNED),
            value,
        });
        let raw: *mut GcBox<T> = Box::into_raw(boxed);
        let ptr: NonNull<GcBox<T>> = NonNull::new(raw).expect("Box::into_raw is non-null");
        let dyn_ptr: NonNull<GcBox<dyn Trace>> = ptr;
        self.uncollected.borrow_mut().push(dyn_ptr);
        Gc {
            ptr,
            _marker: PhantomData,
        }
    }

    /// Bytes currently retained by GC-tracked objects (rough estimate).
    pub fn bytes_used(&self) -> usize {
        self.bytes.get()
    }

    /// Adjust the heap's pacer byte counter by a signed delta, saturating at
    /// zero. Used by [`Gc::account_buffer`] to charge or refund the bytes of
    /// an object's owned heap buffers (table array/node Vecs) so collections
    /// fire at honest memory pressure rather than only on header sizes.
    pub fn adjust_bytes(&self, delta: isize) {
        if delta >= 0 {
            self.bytes
                .set(self.bytes.get().saturating_add(delta as usize));
        } else {
            self.bytes
                .set(self.bytes.get().saturating_sub((-delta) as usize));
        }
    }

    /// Current collection threshold in bytes. When `bytes_used() >= threshold_bytes()`,
    /// the next `step()` will run a full collection (unless paused). Used by
    /// callers that want to short-circuit expensive prep work (e.g. snapshotting
    /// weak tables / pending finalizers) when no collection will actually fire.
    pub fn threshold_bytes(&self) -> usize {
        self.threshold.get()
    }

    /// Override the next automatic collection threshold.
    ///
    /// The VM uses this when Lua-level GC pacing (`GCdebt`, minor-debt, and
    /// pause-debt calculations) has already computed a byte threshold from the
    /// collector-owned live-byte counter.
    pub fn set_threshold_bytes(&self, threshold: usize) {
        self.threshold.set(threshold.max(1));
    }

    /// Cheap predicate: would a `step()` actually do work? Equivalent to
    /// `!paused && bytes_used() >= threshold_bytes()`. Callers that build
    /// snapshot state before invoking the heap should gate on this. Always
    /// false once the heap is closed (see [`collection_inert`](Self::collection_inert)).
    pub fn would_collect(&self) -> bool {
        if self.collection_inert() {
            return false;
        }
        if self.stress {
            return true;
        }
        self.bytes.get() >= self.threshold.get()
    }

    /// True when every collection entry point must be a no-op: the heap is
    /// paused, or it is `closed` (teardown has begun or completed). The
    /// closed arm is load-bearing for teardown soundness (issue #260 codex
    /// round 2, finding 1): a destructor running inside the `drop_all` drain
    /// can invoke a collection, and `sweep_young` re-installs grayagain
    /// entries via `replace_grayagain` — pointers a later drain pass frees,
    /// recreating the second-`drop_all` use-after-free that gating only the
    /// barrier path was meant to prevent. No collection may ever run on a
    /// closed heap; every public collect/step/mark entry checks this.
    ///
    /// The `releasing` arm (#113 Wave 2, R2 finding 1) is the transient twin:
    /// while a sweep/minor `pending_release` drain runs, a payload `Drop` must
    /// not start a nested collection that traces a still-pending (owner-vec
    /// absent, tombstoned) box as a root and then has the outer drain free it.
    /// Folding it here makes all eight entry points inert during the drain with
    /// no per-site edits.
    fn collection_inert(&self) -> bool {
        self.paused.get() || self.closed.get() || self.releasing.get()
    }

    pub fn collections(&self) -> usize {
        self.collections.get()
    }

    pub fn minor_collections(&self) -> usize {
        self.minor_collections.get()
    }

    pub fn full_collections(&self) -> usize {
        self.full_collections.get()
    }

    pub fn last_mark_stats(&self) -> MarkerStats {
        self.last_mark_stats.get()
    }

    pub fn last_sweep_stats(&self) -> SweepStats {
        self.last_sweep_stats.get()
    }

    pub fn allgc_cohort_stats(&self) -> AllGcCohortStats {
        let (new, survival, old1, old) = self.allgc.borrow().cohort_live_counts();
        AllGcCohortStats {
            new,
            survival,
            old1,
            old,
        }
    }

    fn next_token(&self) -> usize {
        let token = self.next_allocation_token.get().max(1);
        let next = token.checked_add(1).unwrap_or(1).max(1);
        self.next_allocation_token.set(next);
        token
    }

    fn current_white(&self) -> Color {
        self.current_white.get()
    }

    fn other_white(&self) -> Color {
        self.current_white.get().other_white()
    }

    fn flip_current_white(&self) {
        self.current_white.set(self.other_white());
    }

    fn for_each_owner_vec_header(&self, ov: &RefCell<OwnerVec>, f: &mut impl FnMut(&GcHeader)) {
        let ov = ov.borrow();
        for slot in ov.slots.iter() {
            if let Some(ptr) = slot {
                f(self.header_from_ptr(*ptr));
            }
        }
    }

    /// Visit every live header across the three sweepable owner vectors. The
    /// callback only ever reads/writes header fields (colors, ages) — never the
    /// owner vectors themselves — so the shared borrow is safe to hold across
    /// it (every caller: `start_cycle` repaint, `promote_all_to_old`,
    /// `reset_all_ages`, `abort_cycle`).
    fn for_each_header(&self, mut f: impl FnMut(&GcHeader)) {
        self.for_each_owner_vec_header(&self.allgc, &mut f);
        self.for_each_owner_vec_header(&self.finobj, &mut f);
        self.for_each_owner_vec_header(&self.tobefnz, &mut f);
    }

    fn header_from_ptr<'a>(&'a self, ptr: NonNull<GcBox<dyn Trace>>) -> &'a GcHeader {
        unsafe { &(*ptr.as_ptr()).header }
    }

    /// The single point where a swept box leaves the heap. The caller must
    /// already have unlinked `ptr` from its owner list and settled all
    /// accounting (byte refund, token removal, object count). Under
    /// quarantine mode the box is poisoned and parked instead of freed, so
    /// later use-after-sweep dereferences hit intact memory and the
    /// `HDR_FREED` debug asserts instead of undefined behavior.
    fn release_box(&self, ptr: NonNull<GcBox<dyn Trace>>) {
        if self.quarantine {
            let header = self.header_from_ptr(ptr);
            header.set_flag(HDR_FREED, true);
            self.quarantined.borrow_mut().push(ptr);
        } else {
            // SAFETY: the caller tombstoned `ptr`'s owner-vector slot, so no
            // owner structure references it; only stale (buggy) GcRefs could.
            // This is the sole runtime free of a GcBox.
            unsafe {
                let _ = Box::from_raw(ptr.as_ptr());
            }
        }
    }

    /// Reset the generational cohort boundaries on every owner vector to zero
    /// (all objects become nursery again) and clear grayagain. #113 Wave 2
    /// replacement for the seven cursor-cell nulls (from `reset_all_ages` and
    /// `drop_all`).
    fn clear_generation_cursors(&self) {
        self.allgc.borrow_mut().clear_cohorts();
        self.finobj.borrow_mut().clear_cohorts();
        self.tobefnz.borrow_mut().clear_cohorts();
        self.clear_grayagain();
    }

    /// Grayagain-entry deletion on a cross-list move or an owner-vector free.
    /// #113 Wave 2 folded the seven cursor fixups out (tombstones never shift
    /// indices) — this is `correct_generation_pointers`' sole surviving duty,
    /// now called directly by the move and sweep-scan paths.
    fn delete_grayagain_entry_if_listed(&self, removed: NonNull<GcBox<dyn Trace>>) {
        if self.header_from_ptr(removed).gray_listed() {
            self.unlink_grayagain(removed);
        }
    }

    /// Threshold compaction after a tombstoning move, but only outside a sweep
    /// phase (a live sweep cursor forbids relocating slots). Moves are the only
    /// tombstone source outside a sweep, so checking here closes the churn hole
    /// (R2 finding 2 / spec "The one mutation rule").
    fn maybe_compact_after_move(&self, ov: &RefCell<OwnerVec>) {
        if self.state.get().is_sweep() {
            return;
        }
        let mut ov = ov.borrow_mut();
        if ov.over_density() {
            ov.compact();
        }
    }

    pub fn move_allgc_to_finobj(&self, ptr: NonNull<GcBox<dyn Trace>>) -> bool {
        let header = self.header_from_ptr(ptr);
        if !header.collected() {
            return false;
        }
        if self.allgc.borrow_mut().tombstone(ptr).is_none() {
            return false;
        }
        self.delete_grayagain_entry_if_listed(ptr);
        if self.state.get().is_sweep() {
            header.color.set(self.current_white());
        }
        self.finobj.borrow_mut().push(ptr);
        self.maybe_compact_after_move(&self.allgc);
        true
    }

    pub fn move_finobj_to_tobefnz(&self, ptr: NonNull<GcBox<dyn Trace>>) -> bool {
        if self.finobj.borrow_mut().tombstone(ptr).is_none() {
            return false;
        }
        self.delete_grayagain_entry_if_listed(ptr);
        self.tobefnz.borrow_mut().push(ptr);
        self.maybe_compact_after_move(&self.finobj);
        true
    }

    pub fn move_tobefnz_to_allgc(&self, ptr: NonNull<GcBox<dyn Trace>>) -> bool {
        let header = self.header_from_ptr(ptr);
        if self.tobefnz.borrow_mut().tombstone(ptr).is_none() {
            return false;
        }
        self.delete_grayagain_entry_if_listed(ptr);
        if self.state.get().is_sweep() {
            header.color.set(self.current_white());
        }
        self.allgc.borrow_mut().push(ptr);
        self.maybe_compact_after_move(&self.tobefnz);
        true
    }

    /// Append `ptr` to the grayagain revisit set. `HDR_GRAY_LISTED` is the
    /// O(1) dedup guard: an already-listed object is a no-op, so the set never
    /// holds a duplicate (`grayagain_links_object_once` pins this). Tail-append
    /// replaces the intrusive list's prepend; the two consumers are
    /// order-insensitive (see `mark_minor_revisit_objects` / `sweep_young`).
    ///
    /// Inert once the heap is `closed`: no collection can ever run again on a
    /// closed heap, so the revisit obligation is void — and accepting entries
    /// would be a use-after-free factory. A destructor running inside the
    /// teardown drain can fire a generational barrier on a box it just
    /// allocated; a later drain pass frees that box, and if the pointer had
    /// been recorded here a second `drop_all` (or `Heap::drop` after an
    /// explicit close) would dereference the freed header in
    /// `clear_grayagain`. This is the single grayagain entry point (both
    /// generational barriers route through it), so gating it here covers
    /// every barrier path.
    fn remember_minor_revisit(&self, ptr: NonNull<GcBox<dyn Trace>>) {
        if self.closed.get() {
            return;
        }
        let header = self.header_from_ptr(ptr);
        if header.gray_listed() {
            return;
        }
        header.set_gray_listed(true);
        self.grayagain.borrow_mut().push(ptr);
    }

    /// Re-mark every grayagain entry at the start of a minor mark. Marking is
    /// idempotent and deduped by `Marker::visited`, so iteration order affects
    /// only gray-queue push order, not the reachability fixed point. The short
    /// immutable borrow is safe: `Marker::mark_box` never re-enters grayagain.
    fn mark_minor_revisit_objects(&self, marker: &mut Marker) {
        let grayagain = self.grayagain.borrow();
        for &ptr in grayagain.iter() {
            let header = self.header_from_ptr(ptr);
            let id = ptr.as_ptr() as *const () as usize;
            marker.mark_box(ptr, header, id);
        }
    }

    /// Empty the grayagain set, clearing each entry's `HDR_GRAY_LISTED` bit.
    /// `Vec::clear` retains capacity so the buffer is recycled across cycles.
    fn clear_grayagain(&self) {
        let mut grayagain = self.grayagain.borrow_mut();
        for &ptr in grayagain.iter() {
            self.header_from_ptr(ptr).set_gray_listed(false);
        }
        grayagain.clear();
    }

    /// Drain the grayagain set to an owned `Vec`, clearing each entry's flag.
    /// Flags are cleared *after* the `borrow_mut` is released so no owner
    /// borrow is ever held across the header writes. `sweep_young` feeds the
    /// returned `Vec` to `OldRevisitTracker` unchanged.
    fn take_grayagain(&self) -> Vec<NonNull<GcBox<dyn Trace>>> {
        let objects = std::mem::take(&mut *self.grayagain.borrow_mut());
        for &ptr in &objects {
            self.header_from_ptr(ptr).set_gray_listed(false);
        }
        objects
    }

    /// Replace the grayagain set with `objects`: clear the current entries'
    /// flags, then install `objects` as the new buffer and set their flags.
    /// `objects` is already dedup'd by its producer (`next_revisit_seen` in
    /// `sweep_young`), so no membership check is needed here.
    fn replace_grayagain(&self, objects: Vec<NonNull<GcBox<dyn Trace>>>) {
        self.clear_grayagain();
        for &ptr in &objects {
            self.header_from_ptr(ptr).set_gray_listed(true);
        }
        *self.grayagain.borrow_mut() = objects;
    }

    /// Remove `removed` from the grayagain set and clear its flag. Called from
    /// `correct_generation_pointers` whenever a gray-listed object is unlinked
    /// from an owner list (full-sweep free, young-sweep free, or a cross-list
    /// move). Order-preserving `retain`; the borrow is dropped before the box
    /// can be freed.
    fn unlink_grayagain(&self, removed: NonNull<GcBox<dyn Trace>>) {
        self.header_from_ptr(removed).set_gray_listed(false);
        self.grayagain
            .borrow_mut()
            .retain(|ptr| !std::ptr::addr_eq(ptr.as_ptr(), removed.as_ptr()));
    }

    /// Number of objects on the grayagain revisit set. Flag-dedup guarantees
    /// no duplicates, so this equals the intrusive list's old walk count
    /// (`grayagain_links_object_once` pins it). Read by lua-cli telemetry.
    pub fn grayagain_count(&self) -> usize {
        self.grayagain.borrow().len()
    }

    /// Record a userdata in the Lua 5.1 collect-time finalizability roster.
    ///
    /// Called by the VM for every userdata that receives a metatable on 5.1.
    /// The probe holds a weak handle, so the roster never roots the userdata.
    /// No-op for the collector beyond storage; the VM reads metatables and
    /// registers finalizers via [`scan_v51_finalizable`](Self::scan_v51_finalizable).
    pub fn register_v51_udata(&self, probe: std::rc::Rc<dyn Udata51Probe>) {
        self.v51_udata_roster.borrow_mut().push(probe);
    }

    /// Drain the Lua 5.1 finalizability roster of dead entries and return the
    /// still-live probes for the VM to inspect.
    ///
    /// Mirrors the enumeration half of C 5.1 `luaC_separateudata`: the VM
    /// caller then reads each probe's live metatable for `__gc` and registers
    /// the finalizable ones. Dead probes (their userdata already swept) are
    /// dropped here so the roster stays bounded. The returned probes are kept
    /// in the roster too (still live), so an already-finalized userdata that
    /// outlives its `__gc` is naturally pruned on a later scan once swept.
    pub fn scan_v51_finalizable(&self) -> Vec<std::rc::Rc<dyn Udata51Probe>> {
        let mut roster = self.v51_udata_roster.borrow_mut();
        roster.retain(|probe| probe.is_alive());
        roster.clone()
    }

    /// Return the current heap token for an allocation identity, or `None` when
    /// the identity was never registered or has since been swept.
    ///
    /// Registration is lazy: tokens are minted at weak-handle creation
    /// ([`register_allocation_token`](Self::register_allocation_token)), not at
    /// allocation, so an object that was never weak-referenced reports `None`
    /// here even while live.
    pub fn allocation_token(&self, identity: usize) -> Option<usize> {
        self.allocation_tokens.borrow().get(&identity).copied()
    }

    /// Register `identity` in the weak-handle validation table, returning its
    /// token (get-or-insert against the monotonic counter).
    ///
    /// This is the lazy half of weak-handle validation. The hot allocation path
    /// no longer touches `allocation_tokens`; instead a token is minted the
    /// first time an object is downgraded to a weak handle, which is the only
    /// moment the token is ever consumed.
    ///
    /// Correctness: every valid weak handle calls this at creation while holding
    /// a strong reference, so the object is provably live at registration. A
    /// later [`contains_allocation`](Self::contains_allocation) returning false
    /// for an absent identity therefore means "swept" exactly as the eager
    /// scheme did — sweep removes the entry when it frees the box. The monotonic
    /// counter never reissues a token, so when the allocator reuses an address
    /// (a freed identity re-registered by a fresh object) the new token differs
    /// from any stale handle's, preventing address reuse from resurrecting a
    /// dead handle. Objects that are never weak-referenced never enter the map.
    ///
    /// On a `closed` heap this refuses to mint: it returns 0 — a value
    /// [`next_token`](Self::next_token) never issues, so it can never
    /// validate — without touching the map. A downgrade of a stale `GcRef`
    /// after close would otherwise re-register the freed box's address and
    /// resurrect it as an upgradable weak target.
    pub fn register_allocation_token(&self, identity: usize) -> usize {
        if self.closed.get() {
            return 0;
        }
        let mut tokens = self.allocation_tokens.borrow_mut();
        if let Some(token) = tokens.get(&identity) {
            return *token;
        }
        let token = self.next_token();
        tokens.insert(identity, token);
        token
    }

    /// Return true when `identity` still names the same heap allocation.
    ///
    /// The token check prevents allocator address reuse from making a stale
    /// weak handle look live again. Unconditionally false once the heap is
    /// `closed`: every box is freed (or about to be, mid-drain), so no weak
    /// handle may upgrade regardless of what the token map transiently holds.
    pub fn contains_allocation(&self, identity: usize, token: usize) -> bool {
        if self.closed.get() {
            return false;
        }
        self.allocation_token(identity) == Some(token)
    }

    /// Forward write barrier: invoked when `parent` (already-traced black
    /// object) gains a new reference to `child`. To preserve the tri-color
    /// invariant ("no black points to white"), we mark the child gray
    /// immediately. Cheap: one branch + maybe one queue push.
    ///
    /// During incremental mode this prevents the marking phase from missing
    /// the new edge. In current stop-the-world mode it's still correct (a
    /// no-op when the collection is idle), so call sites can be wired now
    /// and the incremental upgrade is mechanical later.
    pub fn barrier<P, C>(&self, parent: Gc<P>, child: Gc<C>)
    where
        P: Trace + 'static,
        C: Trace + 'static,
    {
        if self.paused.get() || self.state.get().is_pause() {
            return;
        }
        if parent.header().color.get() != Color::Black {
            return;
        }
        if !child.header().color.get().is_white() {
            return;
        }
        child.header().color.set(Color::Gray);
        if let Ok(mut m_opt) = self.marker.try_borrow_mut() {
            if let Some(m) = m_opt.as_mut() {
                let ptr: NonNull<GcBox<dyn Trace>> = child.ptr;
                m.gray_queue.push(ptr);
                m.visited.insert(child.identity());
            }
        }
    }

    /// Backward barrier: if a black object receives a reference to a white
    /// child, gray the parent so the in-progress cycle will rescan it.
    pub fn barrier_back<P, C>(&self, parent: Gc<P>, child: Gc<C>)
    where
        P: Trace + 'static,
        C: Trace + 'static,
    {
        if self.paused.get() || self.state.get().is_pause() {
            return;
        }
        if parent.header().color.get() != Color::Black {
            return;
        }
        if !child.header().color.get().is_white() {
            return;
        }
        parent.header().color.set(Color::Gray);
        if let Ok(mut m_opt) = self.marker.try_borrow_mut() {
            if let Some(m) = m_opt.as_mut() {
                let ptr: NonNull<GcBox<dyn Trace>> = parent.ptr;
                m.gray_queue.push(ptr);
                m.visited.insert(parent.identity());
            }
        }
    }

    /// Generational forward barrier: if an old object receives a reference to a
    /// young object, the child cannot jump directly to OLD because it may still
    /// point at younger objects. Lua marks it OLD0 so later young collections
    /// advance it through OLD1 to OLD.
    pub fn generational_forward_barrier<P, C>(&self, parent: Gc<P>, child: Gc<C>)
    where
        P: Trace + 'static,
        C: Trace + 'static,
    {
        if parent.age().is_old() && !child.age().is_old() {
            child.set_age(GcAge::Old0);
            let ptr: NonNull<GcBox<dyn Trace>> = child.ptr;
            self.remember_minor_revisit(ptr);
        }
        self.barrier(parent, child);
    }

    /// Generational backward barrier: an old object that now points to a young
    /// object is revisited by the next young collection. This mirrors
    /// `luaC_barrierback_`'s age transition to TOUCHED1.
    pub fn generational_backward_barrier<P>(&self, parent: Gc<P>)
    where
        P: Trace + 'static,
    {
        if parent.age().is_old() {
            parent.set_color(Color::Gray);
            parent.set_age(GcAge::Touched1);
            let ptr: NonNull<GcBox<dyn Trace>> = parent.ptr;
            self.remember_minor_revisit(ptr);
        }
    }

    /// Possibly run a collection. Trigger: bytes_used > threshold.
    /// Caller passes the root set (the runtime — typically `GlobalState`
    /// implementing `Trace`).
    pub fn step(&self, roots: &dyn Trace) {
        self.step_with_post_mark(roots, |_: &mut Marker| {});
    }

    /// Like [`step`] but invokes a `post_mark` hook when a collection
    /// actually fires (threshold reached). Hook is a no-op on the
    /// short-circuit path. The runtime uses this to bridge weak-table
    /// pruning into implicit GC steps fired from inside the VM loop.
    pub fn step_with_post_mark<F: FnMut(&mut Marker)>(&self, roots: &dyn Trace, post_mark: F) {
        if self.collection_inert() {
            return;
        }
        if !self.stress && self.bytes.get() < self.threshold.get() {
            return;
        }
        self.full_collect_with_post_mark(roots, post_mark);
    }

    /// Stop-the-world full collect. Marks every reachable object from
    /// `roots`, then sweeps white (unreachable) boxes from the heap owner lists.
    pub fn full_collect(&self, roots: &dyn Trace) {
        self.full_collect_with_post_mark(roots, |_: &mut Marker| {});
    }

    /// Run only the mark/atomic hook portion of a collection, without sweeping.
    ///
    /// This is used by runtimes that need an atomic reachability snapshot for
    /// weak-table cleanup while they are deliberately avoiding object freeing.
    pub fn mark_only_with_post_mark<F: FnMut(&mut Marker)>(
        &self,
        roots: &dyn Trace,
        mut post_mark: F,
    ) {
        if self.collection_inert() {
            return;
        }
        let mut marker = self.marker_from_pool(MarkerMode::Full);
        roots.trace(&mut marker);
        marker.drain_gray_queue();
        post_mark(&mut marker);
        marker.drain_gray_queue();
        self.last_mark_stats.set(marker.stats());
        self.recycle_marker(marker);
    }

    /// Metadata transition used when entering generational mode after a full
    /// mark: all currently live objects become old.
    pub fn promote_all_to_old(&self) {
        self.for_each_header(|header| {
            header.age.set(GcAge::Old);
            header.color.set(Color::Black);
        });
        self.allgc.borrow_mut().promote_all_old();
        self.finobj.borrow_mut().promote_all_old();
        self.clear_grayagain();
    }

    /// Metadata transition used when returning to incremental mode: Lua clears
    /// age information and treats all objects as new again.
    pub fn reset_all_ages(&self) {
        let current_white = self.current_white();
        self.for_each_header(|header| {
            header.age.set(GcAge::New);
            header.color.set(current_white);
        });
        self.clear_generation_cursors();
    }

    /// Run a complete young-generation collection.
    ///
    /// This is the first generational path: it uses the normal root tracer for
    /// correctness, then limits sweep/freeing to young objects. Later work can
    /// replace the full root traversal with cohort-list traversal without
    /// changing the age/sweep contract introduced here.
    pub fn minor_collect_with_post_mark<F: FnMut(&mut Marker)>(
        &self,
        roots: &dyn Trace,
        mut post_mark: F,
    ) {
        if self.collection_inert() {
            return;
        }
        if !self.state.get().is_pause() {
            self.abort_cycle();
        }

        self.state.set(GcState::Propagate);
        let mut marker = self.marker_from_pool(MarkerMode::Minor);
        self.last_sweep_stats.set(SweepStats::default());
        self.mark_minor_revisit_objects(&mut marker);
        roots.trace(&mut marker);
        marker.drain_gray_queue();

        self.state.set(GcState::EnterAtomic);
        self.state.set(GcState::Atomic);
        post_mark(&mut marker);
        marker.drain_gray_queue();
        self.last_mark_stats.set(marker.stats());
        self.recycle_marker(marker);

        self.state.set(GcState::SweepAllGc);
        self.sweep_young();
        self.recycle_marker_cell();
        self.sweep_index.set(0);
        self.sweep_watermark.set(0);
        self.state.set(GcState::Pause);
        self.collections.set(self.collections.get() + 1);
        self.minor_collections.set(self.minor_collections.get() + 1);
    }

    /// Stop-the-world full collect with a post-mark hook.
    ///
    /// Internally drives the incremental state machine to completion with
    /// an unbounded budget — equivalent to repeatedly calling
    /// [`incremental_step_with_post_mark`] until it returns `Paused`. The
    /// post-mark hook is invoked exactly once, during the atomic transition.
    pub fn full_collect_with_post_mark<F: FnMut(&mut Marker)>(
        &self,
        roots: &dyn Trace,
        mut post_mark: F,
    ) {
        if self.collection_inert() {
            return;
        }
        if !self.state.get().is_pause() {
            self.abort_cycle();
        }
        self.full_collections.set(self.full_collections.get() + 1);
        let unlimited = StepBudget {
            remaining_work: isize::MAX,
            max_credit: isize::MAX,
        };
        loop {
            let outcome = self.incremental_step_with_post_mark(roots, unlimited, &mut post_mark);
            if matches!(outcome, StepOutcome::Paused | StepOutcome::SkippedStopped) {
                break;
            }
        }
    }

    /// Run one budgeted step of the incremental collector.
    ///
    /// The state machine advances `Pause → Propagate → EnterAtomic → Atomic →
    /// SweepAllGc → SweepFinObj → SweepToBeFnz → SweepEnd → CallFin → Pause`.
    /// Each phase consumes budget; the call returns when the budget runs out
    /// or the cycle reaches `Pause`. The `post_mark`
    /// hook is invoked exactly once per cycle, during the `Atomic`
    /// transition (after the initial gray-queue drain, before sweep starts).
    ///
    /// Returns:
    /// - [`StepOutcome::Paused`] — the cycle completed.
    /// - [`StepOutcome::InProgress`] — budget exhausted before the cycle
    ///   finished; caller may step again.
    /// - [`StepOutcome::SkippedStopped`] — heap is paused; nothing happened.
    pub fn incremental_step_with_post_mark<F: FnMut(&mut Marker)>(
        &self,
        roots: &dyn Trace,
        mut budget: StepBudget,
        mut post_mark: F,
    ) -> StepOutcome {
        if self.collection_inert() {
            return StepOutcome::SkippedStopped;
        }
        self.run_budgeted(roots, &mut budget, &mut post_mark);
        if self.state.get().is_pause() {
            StepOutcome::Paused
        } else {
            StepOutcome::InProgress
        }
    }

    fn run_budgeted(
        &self,
        roots: &dyn Trace,
        budget: &mut StepBudget,
        post_mark: &mut dyn FnMut(&mut Marker),
    ) -> bool {
        self.run_budgeted_until(roots, budget, post_mark, None)
    }

    fn run_budgeted_until(
        &self,
        roots: &dyn Trace,
        budget: &mut StepBudget,
        post_mark: &mut dyn FnMut(&mut Marker),
        stop_at: Option<GcState>,
    ) -> bool {
        let mut did_work = false;
        loop {
            if stop_at == Some(self.state.get()) {
                return did_work;
            }
            if budget.remaining_work <= -budget.max_credit {
                return did_work;
            }
            match self.state.get() {
                GcState::Pause => {
                    self.start_cycle(roots);
                    self.state.set(GcState::Propagate);
                    budget.remaining_work -= 1;
                    did_work = true;
                    if stop_at == Some(GcState::Propagate) {
                        return did_work;
                    }
                }
                GcState::Propagate => {
                    let work = self.drain_gray_budgeted(budget.remaining_work.max(1));
                    budget.remaining_work -= work as isize;
                    did_work = did_work || work > 0;
                    let empty = {
                        let m = self.marker.borrow();
                        m.as_ref().map(|m| m.gray_queue.is_empty()).unwrap_or(true)
                    };
                    if empty {
                        self.state.set(GcState::EnterAtomic);
                        if stop_at == Some(GcState::EnterAtomic) {
                            return did_work;
                        }
                    } else if budget.remaining_work <= 0 {
                        return did_work;
                    }
                }
                GcState::EnterAtomic => {
                    self.state.set(GcState::Atomic);
                    budget.remaining_work -= 1;
                    did_work = true;
                    if stop_at == Some(GcState::Atomic) || budget.remaining_work <= 0 {
                        return did_work;
                    }
                }
                GcState::Atomic => {
                    self.run_atomic(post_mark);
                    self.state.set(GcState::SweepAllGc);
                    budget.remaining_work -= 1;
                    did_work = true;
                    if stop_at == Some(GcState::SweepAllGc) {
                        return did_work;
                    }
                }
                GcState::SweepAllGc => {
                    let work = self.sweep_budgeted(budget.remaining_work.max(1));
                    budget.remaining_work -= work as isize;
                    did_work = did_work || work > 0;
                    if self.sweep_phase_done() {
                        self.state.set(GcState::SweepFinObj);
                        self.arm_sweep(self.finobj.borrow().len());
                        if stop_at == Some(GcState::SweepFinObj) {
                            return did_work;
                        }
                    } else if budget.remaining_work <= 0 {
                        return did_work;
                    }
                }
                GcState::SweepFinObj => {
                    let work = self.sweep_budgeted(budget.remaining_work.max(1));
                    budget.remaining_work -= work as isize;
                    did_work = did_work || work > 0;
                    if self.sweep_phase_done() {
                        self.state.set(GcState::SweepToBeFnz);
                        self.arm_sweep(self.tobefnz.borrow().len());
                        if stop_at == Some(GcState::SweepToBeFnz) {
                            return did_work;
                        }
                    } else if budget.remaining_work <= 0 {
                        return did_work;
                    }
                }
                GcState::SweepToBeFnz => {
                    let work = self.sweep_budgeted(budget.remaining_work.max(1));
                    budget.remaining_work -= work as isize;
                    did_work = did_work || work > 0;
                    if self.sweep_phase_done() {
                        self.state.set(GcState::SweepEnd);
                        if stop_at == Some(GcState::SweepEnd) {
                            return did_work;
                        }
                    } else if budget.remaining_work <= 0 {
                        return did_work;
                    }
                }
                GcState::SweepEnd => {
                    self.state.set(GcState::CallFin);
                    budget.remaining_work -= 1;
                    did_work = true;
                    if stop_at == Some(GcState::CallFin) || budget.remaining_work <= 0 {
                        return did_work;
                    }
                }
                GcState::CallFin => {
                    self.finish_cycle();
                    self.state.set(GcState::Pause);
                    if stop_at == Some(GcState::Pause) {
                        return did_work;
                    }
                    return did_work;
                }
            }
        }
    }

    /// Drive an incremental cycle until `target` is entered, stopping before any
    /// subsequent phase work. Intended for testC-style inspection of mid-cycle
    /// color/barrier invariants; normal collector pacing uses
    /// [`Self::incremental_step_with_post_mark`].
    pub fn incremental_run_until_state_with_post_mark<F: FnMut(&mut Marker)>(
        &self,
        roots: &dyn Trace,
        target: GcState,
        max_work: isize,
        mut post_mark: F,
    ) -> StepOutcome {
        if self.collection_inert() {
            return StepOutcome::SkippedStopped;
        }
        let work = max_work.max(1);
        let mut budget = StepBudget {
            remaining_work: work,
            max_credit: work,
        };
        self.run_budgeted_until(roots, &mut budget, &mut post_mark, Some(target));
        if self.state.get().is_pause() {
            StepOutcome::Paused
        } else {
            StepOutcome::InProgress
        }
    }

    /// Take the pooled mark buffers (or build fresh ones sized to the live
    /// set). Pair with [`Heap::recycle_marker`] when the mark phase ends.
    fn marker_from_pool(&self, mode: MarkerMode) -> Marker {
        match self.marker_pool.borrow_mut().take() {
            Some((gray_queue, visited)) => Marker {
                gray_queue,
                visited,
                stats: MarkerStats::default(),
                mode,
            },
            None => Marker::new_with_capacity(mode, self.objects.get()),
        }
    }

    fn recycle_marker(&self, marker: Marker) {
        let Marker {
            mut gray_queue,
            mut visited,
            ..
        } = marker;
        gray_queue.clear();
        visited.clear();
        *self.marker_pool.borrow_mut() = Some((gray_queue, visited));
    }

    fn recycle_marker_cell(&self) {
        if let Some(marker) = self.marker.borrow_mut().take() {
            self.recycle_marker(marker);
        }
    }

    fn start_cycle(&self, roots: &dyn Trace) {
        self.compact_if_over_density(&self.allgc);
        self.compact_if_over_density(&self.finobj);
        self.compact_if_over_density(&self.tobefnz);
        self.flip_current_white();
        let dead_white = self.other_white();
        self.for_each_header(|header| {
            header.color.set(dead_white);
        });
        let mut marker = self.marker_from_pool(MarkerMode::Full);
        roots.trace(&mut marker);
        *self.marker.borrow_mut() = Some(marker);
        self.sweep_index.set(0);
        self.sweep_watermark.set(0);
    }

    /// Threshold-compaction point (no sweep cursor live): compact `ov` if its
    /// tombstone density exceeds 25%.
    fn compact_if_over_density(&self, ov: &RefCell<OwnerVec>) {
        let mut ov = ov.borrow_mut();
        if ov.over_density() {
            ov.compact();
        }
    }

    /// Mandatory-compaction point: compact `ov` unconditionally and cap its
    /// retained capacity near 2x live.
    fn compact_and_shrink(&self, ov: &RefCell<OwnerVec>) {
        let mut ov = ov.borrow_mut();
        ov.compact();
        ov.shrink_high_water();
    }

    /// The owner vector the incremental sweep is currently walking, selected by
    /// `state`. Only valid in a sweep phase.
    fn owner_for_sweep_state(&self) -> &RefCell<OwnerVec> {
        match self.state.get() {
            GcState::SweepAllGc => &self.allgc,
            GcState::SweepFinObj => &self.finobj,
            GcState::SweepToBeFnz => &self.tobefnz,
            other => unreachable!("sweep_budgeted called outside a sweep phase: {other:?}"),
        }
    }

    /// Arm the incremental sweep cursor for a phase over a vector of `len`
    /// slots: start at index 0, watermark at the current length (slots appended
    /// beyond it this cycle are never visited).
    fn arm_sweep(&self, len: usize) {
        self.sweep_index.set(0);
        self.sweep_watermark.set(len);
    }

    /// True once the incremental sweep cursor has reached its watermark for the
    /// current phase.
    fn sweep_phase_done(&self) -> bool {
        self.sweep_index.get() >= self.sweep_watermark.get()
    }

    /// Release phase (all owner-vector borrows released): drain `pending_release`
    /// one box at a time, settling *that object's* accounting (byte refund,
    /// token removal, object-count decrement) before calling `release_box`.
    ///
    /// Per-object-in-pop-order is load-bearing for pacer correctness under
    /// payload-`Drop` reentrancy (R2 finding 1): a peer still pending has not
    /// been refunded, so a reentrant `account_buffer` lands in its header and
    /// heap `bytes` together and is refunded in full when it is popped. The
    /// borrow is dropped between the pop and `release_box`.
    ///
    /// `releasing` is held for the whole drain so a payload `Drop` cannot start
    /// a nested collection that traces a pending box; an RAII guard restores the
    /// previous value even on panic, so a panicking destructor leaves the
    /// remaining pending pointers owned by `pending_release` (freed by the next
    /// drain or `drop_all`) and never wedges the collector shut.
    fn drain_pending_release(&self) {
        struct ReleasingReset<'a> {
            flag: &'a Cell<bool>,
            prev: bool,
        }
        impl Drop for ReleasingReset<'_> {
            fn drop(&mut self) {
                self.flag.set(self.prev);
            }
        }
        let _reset = ReleasingReset {
            prev: self.releasing.replace(true),
            flag: &self.releasing,
        };
        loop {
            let ptr = match self.pending_release.borrow_mut().pop() {
                Some(p) => p,
                None => break,
            };
            let header = self.header_from_ptr(ptr);
            let size = header.size();
            self.bytes.set(self.bytes.get().saturating_sub(size));
            self.allocation_tokens
                .borrow_mut()
                .remove(&(ptr.as_ptr() as *const () as usize));
            self.objects.set(self.objects.get().saturating_sub(1));
            self.release_box(ptr);
        }
    }

    fn drain_gray_budgeted(&self, max_units: isize) -> usize {
        let mut m_opt = self.marker.borrow_mut();
        let marker = match m_opt.as_mut() {
            Some(m) => m,
            None => return 0,
        };
        let mut work = 0usize;
        let mut budget = max_units;
        while budget > 0 {
            let next = match marker.gray_queue.pop() {
                Some(p) => p,
                None => break,
            };
            unsafe {
                let bx = next.as_ref();
                marker.stats.traced += 1;
                if bx.header.age.get().is_old() {
                    marker.stats.traced_old += 1;
                } else {
                    marker.stats.traced_young += 1;
                }
                bx.header.color.set(Color::Black);
                bx.value.trace(marker);
            }
            work += 1;
            budget -= 1;
        }
        work
    }

    fn run_atomic(&self, post_mark: &mut dyn FnMut(&mut Marker)) {
        let mut m_opt = self.marker.borrow_mut();
        if let Some(marker) = m_opt.as_mut() {
            marker.drain_gray_queue();
            post_mark(marker);
            marker.drain_gray_queue();
        }
        drop(m_opt);
        self.arm_sweep(self.allgc.borrow().len());
        self.last_sweep_stats.set(SweepStats::default());
    }

    /// One budgeted incremental-sweep step over the OwnerVec selected by
    /// `state`, two-phase (#113 Wave 2): a scan phase under a short borrow
    /// tombstones dead-white slots and transfers their pointers into
    /// `pending_release` (no `release_box` under the borrow, which an OwnerVec
    /// `RefCell` would forbid), then a release phase drains `pending_release`
    /// with per-object accounting. A tombstone slot counts as one work unit, so
    /// `StepBudget::from_work(1)` still advances past holes and terminates.
    fn sweep_budgeted(&self, max_units: isize) -> usize {
        let mut work = 0usize;
        let mut stats = SweepStats::default();
        let current_white = self.current_white();
        let dead_white = self.other_white();
        {
            let ov_cell = self.owner_for_sweep_state();
            let mut ov = ov_cell.borrow_mut();
            let watermark = self.sweep_watermark.get().min(ov.slots.len());
            let mut i = self.sweep_index.get();
            let mut budget = max_units;
            let mut pending = self.pending_release.borrow_mut();
            while budget > 0 && i < watermark {
                if let Some(ptr) = ov.slots[i] {
                    let header = self.header_from_ptr(ptr);
                    let age = header.age.get();
                    stats.record_visit(age);
                    let color = header.color.get();
                    if color == dead_white {
                        ov.slots[i] = None;
                        ov.tombstones += 1;
                        stats.record_free(header.size());
                        if header.gray_listed() {
                            self.unlink_grayagain(ptr);
                        }
                        pending.push(ptr);
                    } else if matches!(color, Color::Black | Color::Gray) {
                        header.color.set(current_white);
                    }
                }
                i += 1;
                work += 1;
                budget -= 1;
            }
            self.sweep_index.set(i);
        }
        self.drain_pending_release();
        if stats.visited > 0 {
            let mut total = self.last_sweep_stats.get();
            total.add(stats);
            self.last_sweep_stats.set(total);
        }
        work
    }

    fn push_next_revisit(
        next_revisit: &mut Vec<NonNull<GcBox<dyn Trace>>>,
        seen: &mut IdentityHashSet,
        ptr: NonNull<GcBox<dyn Trace>>,
        age: GcAge,
    ) {
        if matches!(
            age,
            GcAge::Old0 | GcAge::Old1 | GcAge::Touched1 | GcAge::Touched2
        ) {
            let id = ptr.as_ptr() as *const () as usize;
            if seen.insert(id) {
                next_revisit.push(ptr);
            }
        }
    }

    /// Scan the survival + nursery slice `slots[start..]` of one owner vector
    /// during a minor collection: tombstone dead young (`is_white() &&
    /// !age.is_old()`) slots and transfer their pointers into `pending_release`,
    /// age survivors (`next_after_minor`, recolor `New`→current-white /
    /// `Touched*`→Black), and feed `Old0/Old1/Touched*` survivors into the
    /// next-cycle revisit set. No `release_box` runs here — the drain is
    /// deferred to after all borrows are dropped (#113 Wave 2 two-phase). The
    /// `ov` + `pending_release` borrows are held across the scan; the only other
    /// borrow touched is `grayagain` (via `unlink_grayagain`), a distinct
    /// `RefCell`.
    fn scan_young_slice(
        &self,
        ov_cell: &RefCell<OwnerVec>,
        start: usize,
        next_revisit: &mut Vec<NonNull<GcBox<dyn Trace>>>,
        next_revisit_seen: &mut IdentityHashSet,
        processed: &mut Option<OldRevisitTracker>,
        stats: &mut SweepStats,
    ) {
        let current_white = self.current_white();
        let mut ov = ov_cell.borrow_mut();
        let mut pending = self.pending_release.borrow_mut();
        for i in start..ov.slots.len() {
            let Some(ptr) = ov.slots[i] else {
                continue;
            };
            let header = self.header_from_ptr(ptr);
            let age = header.age.get();
            stats.record_visit(age);
            if let Some(processed) = processed.as_mut() {
                processed.record_processed(ptr.as_ptr() as *const () as usize);
            }
            if header.color.get().is_white() && !age.is_old() {
                ov.slots[i] = None;
                ov.tombstones += 1;
                stats.record_free(header.size());
                if header.gray_listed() {
                    self.unlink_grayagain(ptr);
                }
                pending.push(ptr);
                continue;
            }
            if !header.color.get().is_white() {
                let next_age = age.next_after_minor();
                header.age.set(next_age);
                match age {
                    GcAge::New => header.color.set(current_white),
                    GcAge::Touched1 | GcAge::Touched2 => header.color.set(Color::Black),
                    _ => {}
                }
                Self::push_next_revisit(next_revisit, next_revisit_seen, ptr, next_age);
            }
        }
    }

    fn sweep_young(&self) {
        let mut next_revisit = std::mem::take(&mut *self.grayagain_scratch.borrow_mut());
        debug_assert!(next_revisit.is_empty(), "grayagain scratch must be parked empty");
        let mut next_revisit_seen = IdentityHashSet::default();
        let mut stats = SweepStats::default();
        let old_revisit = self.take_grayagain();
        let mut processed = OldRevisitTracker::new(&old_revisit);

        let allgc_young = self.allgc.borrow().young_start();
        self.scan_young_slice(
            &self.allgc,
            allgc_young,
            &mut next_revisit,
            &mut next_revisit_seen,
            &mut processed,
            &mut stats,
        );
        let finobj_young = self.finobj.borrow().young_start();
        self.scan_young_slice(
            &self.finobj,
            finobj_young,
            &mut next_revisit,
            &mut next_revisit_seen,
            &mut processed,
            &mut stats,
        );
        self.scan_young_slice(
            &self.tobefnz,
            0,
            &mut next_revisit,
            &mut next_revisit_seen,
            &mut processed,
            &mut stats,
        );

        if let Some(processed) = processed.as_mut() {
            processed.finish();
        }

        for &ptr in &old_revisit {
            let id = ptr.as_ptr() as *const () as usize;
            if processed
                .as_ref()
                .is_some_and(|processed| processed.was_processed(id))
            {
                continue;
            }
            stats.revisit += 1;
            let header = self.header_from_ptr(ptr);
            if header.color.get().is_white() {
                continue;
            }
            let age = header.age.get();
            let next_age = age.next_after_minor();
            header.age.set(next_age);
            if matches!(age, GcAge::Touched1 | GcAge::Touched2) {
                header.color.set(Color::Black);
            }
            Self::push_next_revisit(&mut next_revisit, &mut next_revisit_seen, ptr, next_age);
        }

        {
            let mut allgc = self.allgc.borrow_mut();
            allgc.compact();
            allgc.rotate_after_minor();
            allgc.shrink_high_water();
        }
        {
            let mut finobj = self.finobj.borrow_mut();
            finobj.compact();
            finobj.rotate_after_minor();
            finobj.shrink_high_water();
        }
        {
            let mut tobefnz = self.tobefnz.borrow_mut();
            tobefnz.compact();
            tobefnz.shrink_high_water();
        }

        self.drain_pending_release();

        let mut recycled = old_revisit;
        recycled.clear();
        *self.grayagain_scratch.borrow_mut() = recycled;
        self.replace_grayagain(next_revisit);
        self.last_sweep_stats.set(stats);
    }

    fn finish_cycle(&self) {
        let stats = self
            .marker
            .borrow()
            .as_ref()
            .map(|marker| marker.stats())
            .unwrap_or_default();
        self.last_mark_stats.set(stats);
        self.recycle_marker_cell();
        self.compact_and_shrink(&self.allgc);
        self.compact_and_shrink(&self.finobj);
        self.compact_and_shrink(&self.tobefnz);
        self.sweep_index.set(0);
        self.sweep_watermark.set(0);
        let next = self.bytes.get().saturating_mul(self.pause_multiplier.get()) / 100;
        self.threshold.set(next.max(GC_MIN_THRESHOLD));
        self.collections.set(self.collections.get() + 1);
    }

    /// Finish an idle `CallFin` phase after the runtime has drained any
    /// pending to-be-finalized objects.
    pub fn finish_callfin_phase(&self) -> bool {
        if self.state.get() != GcState::CallFin {
            return false;
        }
        self.finish_cycle();
        self.state.set(GcState::Pause);
        true
    }

    fn abort_cycle(&self) {
        if !self.state.get().is_pause() {
            self.recycle_marker_cell();
            self.sweep_index.set(0);
            self.sweep_watermark.set(0);
            let current_white = self.current_white();
            self.for_each_header(|header| {
                header.color.set(current_white);
            });
            self.compact_and_shrink(&self.allgc);
            self.compact_and_shrink(&self.finobj);
            self.compact_and_shrink(&self.tobefnz);
            self.state.set(GcState::Pause);
        }
    }

    /// Returns the current state of the incremental collector.
    pub fn gc_state(&self) -> GcState {
        self.state.get()
    }

    /// Approximate number of live GC boxes across all heap owner lists.
    pub fn allgc_count(&self) -> usize {
        self.objects.get()
    }

    /// Count live allgc objects whose concrete Rust type name matches
    /// `predicate`. This is diagnostic/testC telemetry only; collector logic
    /// must not depend on Rust type names.
    pub fn type_name_count(&self, mut predicate: impl FnMut(&'static str) -> bool) -> usize {
        let mut count = 0usize;
        for ov in [&self.allgc, &self.finobj, &self.tobefnz] {
            let ov = ov.borrow();
            for slot in ov.slots.iter() {
                let Some(ptr) = slot else {
                    continue;
                };
                let bx = unsafe { ptr.as_ref() };
                if predicate(bx.value().type_name()) {
                    count += 1;
                }
            }
        }
        count
    }

    /// Drop every allocation, ignoring reachability. Called at shutdown
    /// (`Heap::drop`, and the VM's `close_state` / `free_all_objects`).
    ///
    /// **Drain until stable.** A single pass over the owner lists is not
    /// enough: a payload `Drop` may itself allocate — a `GcRef::new` in a
    /// destructor relinks a fresh box onto `head` (or onto `uncollected`
    /// inside a bootstrap window) — and that box would be stranded on a
    /// just-emptied list and leaked outright if the walk were `Heap::drop`'s
    /// own. So this loops over every owner list until a full pass frees
    /// nothing, then zeroes accounting.
    ///
    /// `closed` is set at the top and never cleared; `tearing_down` is held
    /// across the loop so those re-entrant allocations are accepted rather
    /// than panicking against the now-closed heap, and re-arms the panic once
    /// the loop settles. Grayagain is emptied and its flags cleared (via
    /// [`clear_generation_cursors`](Self::clear_generation_cursors)) before any
    /// free, preserving the pre-existing ordering.
    ///
    /// **Nonconvergence is a panic, in every build.** A destructor that
    /// allocates a new box on every pass would spin this loop forever; after
    /// 10,000 passes the loop panics naming that cause. An unconditional
    /// panic is deliberate — a silent infinite hang inside `close()` in a
    /// release build is strictly worse than a loud failure.
    ///
    /// **Panic safety.** `tearing_down` is restored by an RAII guard, so a
    /// panicking destructor (or the pass-cap panic) leaves the heap `closed`
    /// with the closed-heap allocation panic correctly re-armed, never stuck
    /// in a state that accepts allocations forever. A destructor that panics
    /// mid-teardown does, however, leak the not-yet-freed remainder of the
    /// list chain being drained: the local cursor into that chain dies with
    /// the unwinding frame. Panicking in `Drop` is programmer error; the
    /// guarantee kept here is that the heap's state machine stays sound, not
    /// that a panicking destructor is leak-free.
    ///
    /// After this returns, every outstanding `Gc<T>` is dangling — callers
    /// must ensure no `Gc<T>` outlives the `Heap`. **Any** operation that
    /// dereferences a pre-close `Gc`/`GcRef` after this returns — deref,
    /// `downgrade` header reads, `account_buffer` — is use-after-free,
    /// exactly as it always was after `Heap::drop`; close does not widen or
    /// narrow that contract, it only makes the free deterministic. Note the
    /// quarantine caveat: `LUA_RS_GC_QUARANTINE=1` catches use-after-*sweep*
    /// while the heap lives, but this teardown drains the quarantined list
    /// and frees its boxes for real — post-close stale dereferences have no
    /// `HDR_FREED` tripwire left to hit. The VM's own close paths
    /// cannot hit this (`free_all_objects` clears every registry that holds
    /// handles); giving boxes a checkable owner identity at `downgrade` time
    /// is the #252-follow-up ownership redesign, out of scope here.
    pub fn drop_all(&self) {
        /// Restores `tearing_down` on scope exit — including panic unwind —
        /// but only for the outermost `drop_all` frame (`armed`), so a
        /// re-entrant `drop_all` from inside a payload `Drop` cannot clear
        /// the flag out from under the outer drain.
        struct TearingDownReset<'a> {
            flag: &'a Cell<bool>,
            armed: bool,
        }
        impl Drop for TearingDownReset<'_> {
            fn drop(&mut self) {
                if self.armed {
                    self.flag.set(false);
                }
            }
        }

        self.closed.set(true);
        let _reset = TearingDownReset {
            armed: !self.tearing_down.replace(true),
            flag: &self.tearing_down,
        };
        self.recycle_marker_cell();
        self.sweep_index.set(0);
        self.sweep_watermark.set(0);
        self.clear_generation_cursors();
        self.state.set(GcState::Pause);
        self.allocation_tokens.borrow_mut().clear();

        let mut passes = 0usize;
        loop {
            let mut freed_any = false;
            freed_any |= self.drop_pending_release();
            freed_any |= self.drop_owner_vec(&self.allgc);
            freed_any |= self.drop_owner_vec(&self.finobj);
            freed_any |= self.drop_owner_vec(&self.tobefnz);
            freed_any |= self.drop_ptr_vec(&self.quarantined);
            freed_any |= self.drop_ptr_vec(&self.uncollected);
            if !freed_any {
                break;
            }
            passes = passes.saturating_add(1);
            if passes >= 10_000 {
                panic!(
                    "Heap::drop_all did not converge after 10000 passes — a \
                     nonconvergent destructor is allocating a new GC box on \
                     every teardown pass"
                );
            }
        }

        self.allocation_tokens.borrow_mut().clear();
        self.bytes.set(0);
        self.objects.set(0);
    }

    /// Free every live box in one owner vector, returning whether any was
    /// freed. `take_slots` moves the slots out (leaving the vector empty and
    /// consistent) before any `Box::from_raw`, so a payload `Drop` that
    /// re-allocates into this same vector lands in the fresh empty vector and is
    /// caught by a later drain pass in [`drop_all`](Self::drop_all).
    fn drop_owner_vec(&self, ov: &RefCell<OwnerVec>) -> bool {
        let slots = ov.borrow_mut().take_slots();
        let mut freed = false;
        for slot in slots {
            if let Some(ptr) = slot {
                freed = true;
                // SAFETY: the vector was the sole owner of this box; taking the
                // slots out means no owner structure references it now.
                unsafe {
                    let _ = Box::from_raw(ptr.as_ptr());
                }
            }
        }
        freed
    }

    /// Free every box on one plain owner `Vec` (`quarantined`/`uncollected`),
    /// returning whether any was freed. Same take-before-free discipline as
    /// [`drop_owner_vec`](Self::drop_owner_vec).
    fn drop_ptr_vec(&self, list: &RefCell<Vec<NonNull<GcBox<dyn Trace>>>>) -> bool {
        let boxes = std::mem::take(&mut *list.borrow_mut());
        let mut freed = false;
        for ptr in boxes {
            freed = true;
            // SAFETY: sole owner; taken out of the list before the free.
            unsafe {
                let _ = Box::from_raw(ptr.as_ptr());
            }
        }
        freed
    }

    /// Drain `pending_release` at teardown, freeing each box directly (not via
    /// `release_box`, which under quarantine would re-park it). This is rev-3's
    /// "drain pending_release first" teardown step: a sweep that panicked
    /// mid-release could have stranded boxes here.
    fn drop_pending_release(&self) -> bool {
        let boxes = std::mem::take(&mut *self.pending_release.borrow_mut());
        let mut freed = false;
        for ptr in boxes {
            freed = true;
            // SAFETY: pending_release is the sole owner of a dead-in-transit
            // box; taken out before the free.
            unsafe {
                let _ = Box::from_raw(ptr.as_ptr());
            }
        }
        freed
    }
}

/// Last-resort teardown when the final `Rc<Heap>` drops without an explicit
/// close. Runs the same drain as [`Heap::drop_all`], but with one contract
/// difference callers must know: no `HeapGuard` can be active for *this*
/// heap here — the `Rc` is mid-destruction, so there is no sound handle to
/// push (do not try to smuggle one in). A payload destructor that allocates
/// via `GcRef::new` during a plain unguarded `Heap::drop` therefore panics
/// loudly with the #253 no-guard message instead of allocating into a dying
/// heap. The supported deterministic-teardown path for VM heaps is
/// state-level close (`close_state` → `free_all_objects`), which pushes the
/// guard around the drain so such destructors work.
impl Drop for Heap {
    fn drop(&mut self) {
        self.drop_all();
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Tests — confirm the skeleton's invariants before agents ever touch it.
// ──────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Test-only observability into the owner-vector spine (#113 Wave 2). Kept
    /// in the test module so it is `cfg(test)` automatically; inherent impls may
    /// live in any module of the defining crate.
    impl Heap {
        fn sweep_index_for_test(&self) -> usize {
            self.sweep_index.get()
        }
        fn sweep_watermark_for_test(&self) -> usize {
            self.sweep_watermark.get()
        }
        fn allgc_slot_of(&self, ptr: NonNull<GcBox<dyn Trace>>) -> Option<usize> {
            self.allgc.borrow().find(ptr)
        }
        fn allgc_slots_len(&self) -> usize {
            self.allgc.borrow().len()
        }
        fn allgc_live_len(&self) -> usize {
            self.allgc.borrow().live_len()
        }
        fn finobj_contains(&self, ptr: NonNull<GcBox<dyn Trace>>) -> bool {
            self.finobj.borrow().find(ptr).is_some()
        }
        fn tobefnz_contains(&self, ptr: NonNull<GcBox<dyn Trace>>) -> bool {
            self.tobefnz.borrow().find(ptr).is_some()
        }
        fn quarantined_len(&self) -> usize {
            self.quarantined.borrow().len()
        }
        fn releasing_for_test(&self) -> bool {
            self.releasing.get()
        }
        fn pending_release_len(&self) -> usize {
            self.pending_release.borrow().len()
        }
    }

    /// A tiny GC-tracked type for the smoke test.
    struct Cell0 {
        next: Cell<Option<Gc<Cell0>>>,
        marker_calls: Cell<usize>,
    }

    impl Trace for Cell0 {
        fn trace(&self, m: &mut Marker) {
            self.marker_calls.set(self.marker_calls.get() + 1);
            if let Some(n) = self.next.get() {
                m.mark(n);
            }
        }
    }

    /// Roots for tests: just a single Gc<Cell0>, or none.
    struct OneRoot(Option<Gc<Cell0>>);
    impl Trace for OneRoot {
        fn trace(&self, m: &mut Marker) {
            if let Some(g) = self.0 {
                m.mark(g);
            }
        }
    }

    struct TwoRoots {
        first: Option<Gc<Cell0>>,
        second: Option<Gc<Cell0>>,
    }

    impl Trace for TwoRoots {
        fn trace(&self, m: &mut Marker) {
            if let Some(g) = self.first {
                m.mark(g);
            }
            if let Some(g) = self.second {
                m.mark(g);
            }
        }
    }

    #[derive(Clone)]
    struct FinalizerCell {
        id: usize,
        age: GcAge,
        finalized: std::rc::Rc<Cell<bool>>,
    }

    impl FinalizerCell {
        fn new(id: usize) -> Self {
            Self {
                id,
                age: GcAge::New,
                finalized: std::rc::Rc::new(Cell::new(false)),
            }
        }
    }

    impl FinalizerEntry for FinalizerCell {
        fn identity(&self) -> usize {
            self.id
        }

        fn age(&self) -> GcAge {
            self.age
        }

        fn is_finalized(&self) -> bool {
            self.finalized.get()
        }

        fn set_finalized(&self, finalized: bool) {
            self.finalized.set(finalized);
        }
    }

    fn finalizer_ids(objects: &[FinalizerCell]) -> Vec<usize> {
        objects.iter().map(|object| object.id).collect()
    }

    #[derive(Clone)]
    struct WeakCell {
        id: usize,
        live: bool,
        kind: WeakListKind,
    }

    impl WeakEntry for WeakCell {
        type Strong = usize;

        fn identity(&self) -> usize {
            self.id
        }

        fn list_kind(&self) -> WeakListKind {
            self.kind
        }

        fn upgrade(&self) -> Option<Self::Strong> {
            self.live.then_some(self.id)
        }
    }

    // ── issue #113 Wave 2: OwnerVec standalone kit (Phase 1) ──────────────
    //
    // OwnerVec never dereferences the pointers it stores — it is pure slot
    // bookkeeping — so these tests mint distinct, non-dangling
    // `NonNull<GcBox<dyn Trace>>` via a throwaway heap (the boxes stay live on
    // the heap's own list and are freed when the heap drops).

    fn owner_ptr(heap: &std::rc::Rc<Heap>) -> NonNull<GcBox<dyn Trace>> {
        heap.allocate(Cell0 {
            next: Cell::new(None),
            marker_calls: Cell::new(0),
        })
        .as_trace_ptr()
    }

    #[test]
    fn ownervec_push_appends_at_tail() {
        let heap = Heap::new();
        let ptrs: Vec<_> = (0..4).map(|_| owner_ptr(&heap)).collect();
        let mut ov = OwnerVec::default();
        for p in &ptrs {
            ov.push(*p);
        }
        assert_eq!(ov.len(), 4);
        assert_eq!(ov.live_len(), 4);
        for (i, p) in ptrs.iter().enumerate() {
            assert_eq!(ov.find(*p), Some(i));
        }
    }

    #[test]
    fn ownervec_tombstone_keeps_boundaries_physical() {
        let heap = Heap::new();
        let ptrs: Vec<_> = (0..6).map(|_| owner_ptr(&heap)).collect();
        let mut ov = OwnerVec::default();
        for p in &ptrs {
            ov.push(*p);
        }
        ov.reallyold = 2;
        ov.old1 = 2;
        ov.survival = 1;
        assert_eq!(ov.young_start(), 4);

        assert_eq!(ov.tombstone(ptrs[1]), Some(1));
        assert_eq!(ov.tombstones, 1);
        assert_eq!(ov.live_len(), 5);
        assert_eq!(ov.len(), 6);
        assert_eq!(ov.reallyold, 2);
        assert_eq!(ov.old1, 2);
        assert_eq!(ov.survival, 1);
        assert_eq!(ov.find(ptrs[1]), None);
    }

    #[test]
    fn ownervec_compact_preserves_order_and_recounts_cohorts() {
        let heap = Heap::new();
        let ptrs: Vec<_> = (0..10).map(|_| owner_ptr(&heap)).collect();
        let mut ov = OwnerVec::default();
        for p in &ptrs {
            ov.push(*p);
        }
        ov.reallyold = 3;
        ov.old1 = 3;
        ov.survival = 2;

        ov.tombstone_at(1);
        ov.tombstone_at(4);
        ov.tombstone_at(8);
        ov.compact();

        assert_eq!(ov.tombstones, 0);
        assert_eq!(ov.len(), 7);
        let expected = [0usize, 2, 3, 5, 6, 7, 9];
        for (dense_i, orig_i) in expected.iter().enumerate() {
            assert_eq!(ov.find(ptrs[*orig_i]), Some(dense_i));
        }
        assert_eq!(ov.reallyold, 2, "survivors below old index 3");
        assert_eq!(ov.old1, 2, "survivors in [3,6)");
        assert_eq!(ov.survival, 2, "survivors in [6,8)");
        let (nursery, survival, old1, reallyold) = ov.cohort_live_counts();
        assert_eq!((nursery, survival, old1, reallyold), (1, 2, 2, 2));
    }

    #[test]
    fn ownervec_density_threshold_fires_over_25_percent() {
        let heap = Heap::new();
        let ptrs: Vec<_> = (0..4).map(|_| owner_ptr(&heap)).collect();
        let mut ov = OwnerVec::default();
        for p in &ptrs {
            ov.push(*p);
        }
        assert!(!ov.over_density());
        ov.tombstone_at(0);
        assert!(!ov.over_density(), "1/4 tombstones is exactly 25%, not over");
        ov.tombstone_at(1);
        assert!(ov.over_density(), "2/4 tombstones is over 25%");
    }

    #[test]
    fn ownervec_rotate_after_minor_matches_registry_arithmetic() {
        let heap = Heap::new();
        let ptrs: Vec<_> = (0..10).map(|_| owner_ptr(&heap)).collect();
        let mut ov = OwnerVec::default();
        for p in &ptrs {
            ov.push(*p);
        }
        ov.reallyold = 2;
        ov.old1 = 3;
        ov.survival = 4;
        ov.rotate_after_minor();
        assert_eq!(ov.reallyold, 5, "reallyold += old1");
        assert_eq!(ov.old1, 4, "old1 = survival");
        assert_eq!(ov.survival, 1, "survival = nursery survivors (10-2-3-4)");
    }

    #[test]
    fn ownervec_promote_all_old_after_compaction() {
        let heap = Heap::new();
        let ptrs: Vec<_> = (0..5).map(|_| owner_ptr(&heap)).collect();
        let mut ov = OwnerVec::default();
        for p in &ptrs {
            ov.push(*p);
        }
        ov.tombstone_at(2);
        ov.promote_all_old();
        assert_eq!(ov.reallyold, 4);
        assert_eq!(ov.old1, 0);
        assert_eq!(ov.survival, 0);
        assert_eq!(ov.tombstones, 0);
        assert_eq!(ov.len(), 4);
    }

    #[test]
    fn ownervec_b2_adversarial_churn_stays_bounded() {
        let heap = Heap::new();
        const LIVE: usize = 64;
        let mut pool: Vec<_> = (0..LIVE).map(|_| owner_ptr(&heap)).collect();
        let mut ov = OwnerVec::default();
        for p in &pool {
            ov.push(*p);
        }
        for round in 0..20_000usize {
            let victim = pool[round % LIVE];
            let idx = ov.find(victim).expect("victim is a live member");
            ov.tombstone_at(idx);
            let replacement = owner_ptr(&heap);
            pool[round % LIVE] = replacement;
            ov.push(replacement);
            if ov.over_density() {
                ov.compact();
            }
            assert_eq!(ov.live_len(), LIVE, "exactly LIVE live members always");
            assert!(
                ov.len() <= 2 * LIVE,
                "slot count {} must stay within 2x live ({}) — density bound",
                ov.len(),
                2 * LIVE
            );
        }
    }

    #[test]
    fn weak_registry_dedups_snapshots_and_retains_live_ids() {
        let mut registry = WeakRegistry::default();
        registry.push_unique(WeakCell {
            id: 1,
            live: true,
            kind: WeakListKind::WeakValues,
        });
        registry.push_unique(WeakCell {
            id: 1,
            live: true,
            kind: WeakListKind::Ephemeron,
        });
        registry.push_unique(WeakCell {
            id: 2,
            live: false,
            kind: WeakListKind::AllWeak,
        });
        registry.push_unique(WeakCell {
            id: 3,
            live: true,
            kind: WeakListKind::WeakValues,
        });

        let stats = registry.stats();
        assert_eq!(stats.weak_values, 1);
        assert_eq!(stats.ephemeron, 1);
        assert_eq!(stats.all_weak, 1);

        let snapshot = registry.live_snapshot();
        assert_eq!(snapshot, vec![3, 1]);
        assert_eq!(
            registry.stats(),
            WeakRegistryStats {
                tracked: 3,
                snapshot_live: 2,
                snapshot_dead: 1,
                retained: 2,
                weak_values: 1,
                ephemeron: 1,
                all_weak: 0,
            }
        );

        let keep: std::collections::HashSet<usize> = [3usize].into_iter().collect();
        registry.retain_identities(&keep);
        assert_eq!(registry.len(), 1);
        assert_eq!(registry.stats().retained, 1);
        assert_eq!(registry.stats().weak_values, 1);
        registry.remove_identity(3);
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn finalizer_registry_tracks_generational_cohorts() {
        let mut registry = FinalizerRegistry::default();
        registry.push_pending_unique(FinalizerCell::new(1));
        registry.push_pending_unique(FinalizerCell::new(2));

        let stats = registry.stats();
        assert_eq!(stats.finobj_new, 2);
        assert_eq!(stats.finobj_survival, 0);
        assert_eq!(stats.finobj_old1, 0);
        assert_eq!(stats.finobj_reallyold, 0);
        assert_eq!(stats.finobj_minor_scan, 2);

        registry.finish_minor_collection();
        let stats = registry.stats();
        assert_eq!(stats.finobj_new, 0);
        assert_eq!(stats.finobj_survival, 2);
        assert_eq!(stats.finobj_old1, 0);
        assert_eq!(stats.finobj_reallyold, 0);
        assert_eq!(stats.finobj_minor_scan, 2);

        registry.push_pending_unique(FinalizerCell::new(3));
        registry.finish_minor_collection();
        let stats = registry.stats();
        assert_eq!(stats.finobj_new, 0);
        assert_eq!(stats.finobj_survival, 1);
        assert_eq!(stats.finobj_old1, 2);
        assert_eq!(stats.finobj_reallyold, 0);
        assert_eq!(stats.finobj_minor_scan, 1);

        registry.finish_minor_collection();
        let stats = registry.stats();
        assert_eq!(stats.finobj_new, 0);
        assert_eq!(stats.finobj_survival, 0);
        assert_eq!(stats.finobj_old1, 1);
        assert_eq!(stats.finobj_reallyold, 2);
        assert_eq!(stats.finobj_minor_scan, 0);
    }

    #[test]
    fn finalizer_registry_minor_snapshot_uses_cohort_boundaries() {
        let mut registry = FinalizerRegistry::default();
        registry.push_pending_unique(FinalizerCell::new(1));
        registry.push_pending_unique(FinalizerCell::new(2));
        registry.push_pending_unique(FinalizerCell::new(3));
        registry.finish_minor_collection();
        registry.finish_minor_collection();
        registry.push_pending_unique(FinalizerCell::new(4));
        registry.push_pending_unique(FinalizerCell::new(5));

        assert_eq!(
            finalizer_ids(&registry.pending_minor_snapshot()),
            vec![4, 5],
            "minor finalizer scan must skip the old1/reallyold prefix"
        );

        registry.push_to_be_finalized(FinalizerCell::new(99));
        registry.promote_pending_to_finalized(vec![
            FinalizerCell::new(1),
            FinalizerCell::new(2),
            FinalizerCell::new(4),
        ]);

        let stats = registry.stats();
        assert_eq!(stats.finobj_old1, 1);
        assert_eq!(stats.finobj_new, 1);
        assert_eq!(stats.finobj_minor_scan, 1);
        assert_eq!(finalizer_ids(registry.pending()), vec![3, 5]);
        assert_eq!(
            finalizer_ids(registry.to_be_finalized()),
            vec![99, 4, 2, 1],
            "new to-be-finalized batches append behind older queued finalizers"
        );
    }

    #[test]
    fn finalizer_registry_marks_and_clears_finalized_bit() {
        let mut registry = FinalizerRegistry::default();
        let object = FinalizerCell::new(1);

        assert!(!object.is_finalized());
        registry.push_pending_unique(object.clone());
        assert!(object.is_finalized());

        registry.push_pending_unique(object.clone());
        assert_eq!(registry.pending_len(), 1);

        registry.promote_pending_to_finalized(vec![object.clone()]);
        assert!(object.is_finalized());
        assert_eq!(registry.pending_len(), 0);
        assert_eq!(registry.to_be_finalized_len(), 1);

        let popped = registry.pop_to_be_finalized().unwrap();
        assert_eq!(popped.id, 1);
        assert!(!object.is_finalized());

        registry.push_pending_unique(object.clone());
        assert!(object.is_finalized());
        assert_eq!(registry.pending_len(), 1);
    }

    #[test]
    fn alloc_and_drop_all() {
        let heap = Heap::new();
        heap.unpause();
        let _a = heap.allocate(Cell0 {
            next: Cell::new(None),
            marker_calls: Cell::new(0),
        });
        let _b = heap.allocate(Cell0 {
            next: Cell::new(None),
            marker_calls: Cell::new(0),
        });
        assert!(heap.bytes_used() > 0);
        heap.drop_all();
        assert_eq!(heap.bytes_used(), 0);
    }

    fn owner_live_len(ov: &RefCell<OwnerVec>) -> usize {
        ov.borrow().live_len()
    }

    #[test]
    fn finalizer_intrusive_lists_sweep_and_drop() {
        let heap = Heap::new();
        heap.unpause();
        let _normal = heap.allocate(Cell0 {
            next: Cell::new(None),
            marker_calls: Cell::new(0),
        });
        let finobj = heap.allocate(Cell0 {
            next: Cell::new(None),
            marker_calls: Cell::new(0),
        });
        let tobefnz = heap.allocate(Cell0 {
            next: Cell::new(None),
            marker_calls: Cell::new(0),
        });

        assert!(heap.move_allgc_to_finobj(finobj.as_trace_ptr()));
        assert!(heap.move_allgc_to_finobj(tobefnz.as_trace_ptr()));
        assert!(heap.move_finobj_to_tobefnz(tobefnz.as_trace_ptr()));
        assert_eq!(owner_live_len(&heap.allgc), 1);
        assert_eq!(owner_live_len(&heap.finobj), 1);
        assert_eq!(owner_live_len(&heap.tobefnz), 1);
        assert_eq!(heap.allgc_count(), 3);

        heap.full_collect(&TwoRoots {
            first: Some(finobj),
            second: Some(tobefnz),
        });
        assert_eq!(owner_live_len(&heap.allgc), 0);
        assert_eq!(owner_live_len(&heap.finobj), 1);
        assert_eq!(owner_live_len(&heap.tobefnz), 1);
        assert_eq!(heap.allgc_count(), 2);

        assert!(heap.move_tobefnz_to_allgc(tobefnz.as_trace_ptr()));
        heap.full_collect(&OneRoot(Some(tobefnz)));
        assert_eq!(owner_live_len(&heap.allgc), 1);
        assert_eq!(owner_live_len(&heap.finobj), 0);
        assert_eq!(owner_live_len(&heap.tobefnz), 0);
        assert_eq!(heap.allgc_count(), 1);

        heap.drop_all();
        assert_eq!(heap.bytes_used(), 0);
    }

    #[test]
    fn account_buffer_refunded_on_sweep() {
        let heap = Heap::new();
        heap.unpause();
        let baseline = heap.bytes_used();
        {
            let a = heap.allocate(Cell0 {
                next: Cell::new(None),
                marker_calls: Cell::new(0),
            });
            assert!(heap.bytes_used() > baseline);
            a.account_buffer(&heap, 4096);
            assert_eq!(
                a.header().size(),
                std::mem::size_of::<GcBox<Cell0>>() + 4096
            );
        }
        // Drop the only root path (a is no longer Trace-visible). The +4096
        // must be refunded via header.size when the box is swept.
        heap.full_collect(&OneRoot(None));
        assert_eq!(
            heap.bytes_used(),
            baseline,
            "account_buffer charge must be fully refunded by sweep"
        );
    }

    #[test]
    fn account_buffer_noop_on_uncollected() {
        let heap = Heap::new();
        heap.unpause();
        let g = Gc::new_uncollected(Cell0 {
            next: Cell::new(None),
            marker_calls: Cell::new(0),
        });
        let before = heap.bytes_used();
        g.account_buffer(&heap, 8192);
        assert_eq!(
            heap.bytes_used(),
            before,
            "uncollected box must not charge the pacer"
        );
    }

    #[test]
    fn collect_unreachable_frees_bytes() {
        let heap = Heap::new();
        heap.unpause();
        let _a = heap.allocate(Cell0 {
            next: Cell::new(None),
            marker_calls: Cell::new(0),
        });
        let bytes_before = heap.bytes_used();
        assert!(bytes_before > 0);
        // No roots — everything should sweep.
        heap.full_collect(&OneRoot(None));
        assert_eq!(heap.bytes_used(), 0);
        assert_eq!(heap.collections(), 1);
    }

    #[test]
    fn collect_keeps_reachable() {
        let heap = Heap::new();
        heap.unpause();
        let root = heap.allocate(Cell0 {
            next: Cell::new(None),
            marker_calls: Cell::new(0),
        });
        let bytes_before = heap.bytes_used();
        heap.full_collect(&OneRoot(Some(root)));
        assert_eq!(heap.bytes_used(), bytes_before);
        assert_eq!(root.marker_calls.get(), 1);
    }

    #[test]
    fn allocations_start_new_and_white() {
        let heap = Heap::new();
        heap.unpause();
        let obj = heap.allocate(Cell0 {
            next: Cell::new(None),
            marker_calls: Cell::new(0),
        });
        assert_eq!(obj.age(), GcAge::New);
        assert!(obj.color().is_white());
    }

    #[test]
    fn allocation_tokens_track_exact_live_box() {
        let heap = Heap::new();
        heap.unpause();
        let obj = heap.allocate(Cell0 {
            next: Cell::new(None),
            marker_calls: Cell::new(0),
        });
        let id = obj.identity();

        assert_eq!(
            heap.allocation_token(id),
            None,
            "lazy registration: a freshly allocated box has no token until it is downgraded"
        );

        let token = heap.register_allocation_token(id);
        assert_eq!(
            heap.register_allocation_token(id),
            token,
            "registration is get-or-insert: re-registering a live identity returns the same token"
        );
        assert_eq!(heap.allocation_token(id), Some(token));
        assert!(heap.contains_allocation(id, token));
        assert!(!heap.contains_allocation(id, token + 1));

        heap.full_collect(&OneRoot(None));
        assert_eq!(heap.allocation_token(id), None);
        assert!(!heap.contains_allocation(id, token));
    }

    #[test]
    fn registering_after_sweep_yields_a_distinct_token_on_address_reuse() {
        let heap = Heap::new();
        heap.unpause();
        let first = heap.allocate(Cell0 {
            next: Cell::new(None),
            marker_calls: Cell::new(0),
        });
        let id = first.identity();
        let first_token = heap.register_allocation_token(id);

        heap.full_collect(&OneRoot(None));
        assert_eq!(
            heap.allocation_token(id),
            None,
            "sweep removes the token for the dead identity"
        );

        let second_token = heap.register_allocation_token(id);
        assert_ne!(
            first_token, second_token,
            "monotonic tokens keep address reuse from resurrecting a stale weak handle"
        );
        assert!(!heap.contains_allocation(id, first_token));
        assert!(heap.contains_allocation(id, second_token));
    }

    #[test]
    fn allocation_during_incremental_sweep_survives_current_cycle() {
        let heap = Heap::new();
        heap.unpause();
        let old_dead = heap.allocate(Cell0 {
            next: Cell::new(None),
            marker_calls: Cell::new(0),
        });
        let old_id = old_dead.identity();
        heap.register_allocation_token(old_id);

        let outcome = heap.incremental_run_until_state_with_post_mark(
            &OneRoot(None),
            GcState::SweepAllGc,
            1024,
            |_| {},
        );
        assert_eq!(outcome, StepOutcome::InProgress);
        assert_eq!(heap.gc_state(), GcState::SweepAllGc);

        let new_during_sweep = heap.allocate(Cell0 {
            next: Cell::new(None),
            marker_calls: Cell::new(0),
        });
        let new_id = new_during_sweep.identity();
        heap.register_allocation_token(new_id);

        loop {
            let outcome = heap.incremental_step_with_post_mark(
                &OneRoot(None),
                StepBudget::from_work(64),
                |_| {},
            );
            if matches!(outcome, StepOutcome::Paused) {
                break;
            }
        }

        assert_eq!(heap.allocation_token(old_id), None);
        assert!(heap.allocation_token(new_id).is_some());
        assert_eq!(heap.allgc_count(), 1);

        heap.full_collect(&OneRoot(None));
        assert_eq!(heap.allocation_token(new_id), None);
        assert_eq!(heap.allgc_count(), 0);
    }

    // ── issue #113 Wave 2: owner-move-vs-sweep-cursor matrix (Phase 6) ────

    struct ManyRoots(Vec<Gc<Cell0>>);
    impl Trace for ManyRoots {
        fn trace(&self, m: &mut Marker) {
            for g in &self.0 {
                m.mark(*g);
            }
        }
    }

    fn fresh_cell(heap: &std::rc::Rc<Heap>) -> Gc<Cell0> {
        heap.allocate(Cell0 {
            next: Cell::new(None),
            marker_calls: Cell::new(0),
        })
    }

    fn drain_to_pause(heap: &Heap, roots: &dyn Trace) {
        loop {
            if matches!(
                heap.incremental_step_with_post_mark(roots, StepBudget::from_work(64), |_| {}),
                StepOutcome::Paused
            ) {
                break;
            }
        }
    }

    /// M1/M2/M3 for `allgc → finobj`: run a `move_allgc_to_finobj` with the
    /// target slot positioned `advance` past / at / before the SweepAllGc
    /// cursor, and return whether the (rooted) object survived in finobj after
    /// the cycle finished. The `current_white()` recolor must carry it through
    /// both lists' sweeps in every position.
    fn allgc_to_finobj_move_case(advance: usize, target_slot: usize) -> bool {
        let heap = Heap::new();
        heap.unpause();
        let objs: Vec<_> = (0..5).map(|_| fresh_cell(&heap)).collect();
        let roots = ManyRoots(objs.clone());
        let target = objs[target_slot].as_trace_ptr();
        heap.incremental_run_until_state_with_post_mark(
            &roots,
            GcState::SweepAllGc,
            1024,
            |_| {},
        );
        assert_eq!(heap.gc_state(), GcState::SweepAllGc);
        assert_eq!(heap.allgc_slot_of(target), Some(target_slot));
        if advance > 0 {
            heap.incremental_step_with_post_mark(
                &roots,
                StepBudget::from_work(advance as isize),
                |_| {},
            );
            assert_eq!(heap.gc_state(), GcState::SweepAllGc, "still in allgc sweep");
        }
        assert!(
            heap.move_allgc_to_finobj(target),
            "move must return true (advance={advance}, target_slot={target_slot})"
        );
        assert_eq!(heap.allgc_slot_of(target), None, "left allgc");
        assert!(heap.finobj_contains(target), "entered finobj");
        drain_to_pause(&heap, &roots);
        heap.finobj_contains(target)
    }

    #[test]
    fn m_allgc_to_finobj_survives_before_at_ahead_cursor() {
        assert!(
            allgc_to_finobj_move_case(3, 1),
            "M1 before-cursor: recolor carries the object through the finobj sweep"
        );
        assert!(
            allgc_to_finobj_move_case(2, 2),
            "M2 at-cursor: the tombstone is skipped and the recolored object survives"
        );
        assert!(
            allgc_to_finobj_move_case(1, 3),
            "M3 ahead-of-cursor: recolored current-white, swept-safe in finobj"
        );
    }

    /// M (recolor isolation): a **dead** (unmarked, other-white) object moved
    /// `allgc → finobj` *during* SweepAllGc survives the cycle **only because**
    /// of the `is_sweep()` `current_white()` recolor — this is the
    /// finalizer-resurrection path (an object found dead but with `__gc`).
    /// Without the recolor it stays other-white and SweepFinObj frees it. The
    /// keeper is rooted so allgc is non-empty and the dead object sits ahead of
    /// the cursor.
    #[test]
    fn m_dead_object_moved_to_finobj_during_sweep_survives_via_recolor() {
        let heap = Heap::new();
        heap.unpause();
        let keeper = fresh_cell(&heap);
        let dead = fresh_cell(&heap);
        let d = dead.identity();
        heap.register_allocation_token(d);
        let dptr = dead.as_trace_ptr();
        let roots = ManyRoots(vec![keeper]);

        heap.incremental_run_until_state_with_post_mark(
            &roots,
            GcState::SweepAllGc,
            1024,
            |_| {},
        );
        assert_eq!(heap.gc_state(), GcState::SweepAllGc);
        assert_eq!(heap.sweep_index_for_test(), 0, "cursor at start");
        assert_eq!(heap.allgc_slot_of(dptr), Some(1), "dead object is ahead of the cursor");

        assert!(heap.move_allgc_to_finobj(dptr), "move must return true");

        drain_to_pause(&heap, &roots);
        assert!(
            heap.finobj_contains(dptr),
            "the is_sweep recolor kept the dead-but-finalizable object alive in finobj"
        );
        assert!(
            heap.allocation_token(d).is_some(),
            "the moved object was not freed this cycle"
        );
    }

    /// M (tobefnz → allgc) during `SweepToBeFnz`: the `current_white()` recolor
    /// carries a live object moved back to allgc through the rest of the cycle
    /// (SweepAllGc already passed, so it is not re-swept).
    #[test]
    fn m_tobefnz_to_allgc_during_sweep_recolors_and_survives() {
        let heap = Heap::new();
        heap.unpause();
        let obj = fresh_cell(&heap);
        let t = obj.as_trace_ptr();
        assert!(heap.move_allgc_to_finobj(t));
        assert!(heap.move_finobj_to_tobefnz(t));
        let roots = ManyRoots(vec![obj]);
        heap.incremental_run_until_state_with_post_mark(
            &roots,
            GcState::SweepToBeFnz,
            1024,
            |_| {},
        );
        assert_eq!(heap.gc_state(), GcState::SweepToBeFnz);
        assert!(heap.tobefnz_contains(t));
        assert_eq!(heap.sweep_index_for_test(), 0, "tobefnz cursor at entry");

        assert!(heap.move_tobefnz_to_allgc(t), "move must return true");
        assert!(!heap.tobefnz_contains(t), "left tobefnz");
        assert!(heap.allgc_slot_of(t).is_some(), "entered allgc");

        drain_to_pause(&heap, &roots);
        assert!(heap.allgc_slot_of(t).is_some(), "survived in allgc");
        assert_eq!(heap.allgc_count(), 1);
    }

    /// A1: allocation during `SweepFinObj`, `SweepToBeFnz`, and the
    /// `EnterAtomic`→`Atomic` gap survives the current cycle (append at/beyond
    /// the watermark + current-white color both protect it).
    #[test]
    fn allocation_during_each_sweep_phase_survives() {
        for target in [
            GcState::Atomic,
            GcState::SweepFinObj,
            GcState::SweepToBeFnz,
        ] {
            let heap = Heap::new();
            heap.unpause();
            let keep = fresh_cell(&heap);
            let roots = ManyRoots(vec![keep]);
            heap.incremental_run_until_state_with_post_mark(&roots, target, 1024, |_| {});
            assert_eq!(heap.gc_state(), target, "reached {target:?}");
            let fresh = fresh_cell(&heap);
            let id = fresh.identity();
            heap.register_allocation_token(id);
            drain_to_pause(&heap, &roots);
            assert!(
                heap.allocation_token(id).is_some(),
                "an allocation made during {target:?} survives the current cycle"
            );
        }
    }

    /// B1: `StepBudget::from_work(1)` drives a cycle to completion even while a
    /// move and an allocation are interleaved at every in-progress step —
    /// proving tombstone-skip termination and that churn cannot wedge the
    /// budget-1 loop. Rooted survivors must all live.
    #[test]
    fn budget_one_resumption_with_interleaved_move_and_alloc() {
        let heap = Heap::new();
        heap.unpause();
        let survivors: Vec<_> = (0..6).map(|_| fresh_cell(&heap)).collect();
        let roots = ManyRoots(survivors.clone());
        let mover = survivors[0].as_trace_ptr();
        let mut moved_once = false;
        let mut steps = 0usize;
        loop {
            let outcome =
                heap.incremental_step_with_post_mark(&roots, StepBudget::from_work(1), |_| {});
            steps += 1;
            assert!(steps < 100_000, "budget-1 loop must terminate");
            if matches!(outcome, StepOutcome::Paused) {
                break;
            }
            if heap.gc_state().is_sweep() {
                let _fresh = fresh_cell(&heap);
                if !moved_once && heap.allgc_slot_of(mover).is_some() {
                    assert!(heap.move_allgc_to_finobj(mover));
                    moved_once = true;
                }
            }
        }
        assert!(moved_once, "the interleaved move happened during a sweep phase");
        for g in &survivors {
            let p = g.as_trace_ptr();
            assert!(
                heap.allgc_slot_of(p).is_some() || heap.finobj_contains(p),
                "every rooted survivor is still live after the churned cycle"
            );
        }
    }

    /// B2 (heap level): cycle one live object `allgc → finobj → tobefnz →
    /// allgc` for far more rounds than the tombstone threshold, at `Pause`.
    /// The move-time compaction trigger keeps every vector's slot count near
    /// its single live member — R2's unbounded-growth counterexample is
    /// bounded. Every move returns true throughout.
    #[test]
    fn heap_level_move_churn_stays_bounded() {
        let heap = Heap::new();
        heap.unpause();
        let obj = fresh_cell(&heap);
        let t = obj.as_trace_ptr();
        for round in 0..5000usize {
            assert!(heap.move_allgc_to_finobj(t), "allgc->finobj round {round}");
            assert!(heap.move_finobj_to_tobefnz(t), "finobj->tobefnz round {round}");
            assert!(heap.move_tobefnz_to_allgc(t), "tobefnz->allgc round {round}");
        }
        assert_eq!(heap.allgc_count(), 1, "still exactly one live object");
        assert!(heap.allgc_slot_of(t).is_some(), "back in allgc");
        assert!(
            heap.allgc_slots_len() <= 8,
            "allgc slot count stays bounded near live (=1); got {}",
            heap.allgc_slots_len()
        );
        assert_eq!(heap.allgc_live_len(), 1);
    }

    /// F1 (heap-side membership sync): drive several objects through
    /// `allgc → finobj → tobefnz` and back, asserting each move returns true
    /// and the tobefnz membership set stays exactly the registered set — the
    /// invariant the typed `FinalizerRegistry` FIFO authority leans on.
    #[test]
    fn tobefnz_membership_stays_synced_through_moves() {
        let heap = Heap::new();
        heap.unpause();
        let objs: Vec<_> = (0..5).map(|_| fresh_cell(&heap)).collect();
        let ptrs: Vec<_> = objs.iter().map(|g| g.as_trace_ptr()).collect();
        for p in &ptrs {
            assert!(heap.move_allgc_to_finobj(*p));
            assert!(heap.move_finobj_to_tobefnz(*p));
        }
        for p in &ptrs {
            assert!(heap.tobefnz_contains(*p), "each finalizable object is in tobefnz");
        }
        for p in &ptrs {
            assert!(heap.move_tobefnz_to_allgc(*p), "pop back to allgc succeeds");
            assert!(!heap.tobefnz_contains(*p), "left tobefnz on pop");
        }
        assert_eq!(heap.allgc_count(), 5);
    }

    /// A `Trace` payload whose `Drop` panics — for the panic-safety row.
    struct PanicOnDrop;
    impl Trace for PanicOnDrop {
        fn trace(&self, _m: &mut Marker) {}
    }
    impl Drop for PanicOnDrop {
        fn drop(&mut self) {
            panic!("boom in Drop during the release drain");
        }
    }

    /// Teardown/panic row: a payload `Drop` that panics mid-release-drain must
    /// leave the collector sound — `releasing` restored by its RAII guard (not
    /// wedged shut), the box in flight freed (not double-freed), and a
    /// subsequent collection working normally.
    #[test]
    fn panicking_drop_during_release_leaves_collector_sound() {
        let heap = Heap::new();
        heap.unpause();
        heap.allocate(PanicOnDrop);

        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            heap.full_collect(&OneRoot(None));
        }));
        std::panic::set_hook(prev_hook);

        assert!(result.is_err(), "the panicking Drop propagated out of the drain");
        assert!(
            !heap.releasing_for_test(),
            "releasing must be restored after a panicking drop (RAII guard), not wedged"
        );
        assert_eq!(heap.pending_release_len(), 0, "no stranded pending pointers");

        let live = fresh_cell(&heap);
        let roots = ManyRoots(vec![live]);
        heap.full_collect(&roots);
        assert!(
            heap.allgc_slot_of(live.as_trace_ptr()).is_some(),
            "the collector is not wedged: a later collection keeps a rooted object"
        );
    }

    #[test]
    fn promote_and_reset_all_ages() {
        let heap = Heap::new();
        heap.unpause();
        let a = heap.allocate(Cell0 {
            next: Cell::new(None),
            marker_calls: Cell::new(0),
        });
        let b = heap.allocate(Cell0 {
            next: Cell::new(None),
            marker_calls: Cell::new(0),
        });

        heap.promote_all_to_old();
        assert_eq!(a.age(), GcAge::Old);
        assert_eq!(b.age(), GcAge::Old);
        assert_eq!(a.color(), Color::Black);
        assert_eq!(b.color(), Color::Black);

        heap.reset_all_ages();
        assert_eq!(a.age(), GcAge::New);
        assert_eq!(b.age(), GcAge::New);
        assert!(a.color().is_white());
        assert!(b.color().is_white());
    }

    #[test]
    fn generational_barriers_update_ages() {
        let heap = Heap::new();
        heap.unpause();
        let parent = heap.allocate(Cell0 {
            next: Cell::new(None),
            marker_calls: Cell::new(0),
        });
        let child = heap.allocate(Cell0 {
            next: Cell::new(None),
            marker_calls: Cell::new(0),
        });

        parent.set_age(GcAge::Old);
        parent.set_color(Color::Black);
        heap.generational_backward_barrier(parent);
        assert_eq!(parent.age(), GcAge::Touched1);
        assert_eq!(parent.color(), Color::Gray);

        heap.generational_forward_barrier(parent, child);
        assert_eq!(child.age(), GcAge::Old0);
    }

    #[test]
    fn minor_collect_frees_young_and_keeps_old() {
        let heap = Heap::new();
        heap.unpause();
        let old_unreachable = heap.allocate(Cell0 {
            next: Cell::new(None),
            marker_calls: Cell::new(0),
        });
        old_unreachable.set_age(GcAge::Old);
        old_unreachable.set_color(Color::Black);
        let _young_unreachable = heap.allocate(Cell0 {
            next: Cell::new(None),
            marker_calls: Cell::new(0),
        });
        let young_survivor = heap.allocate(Cell0 {
            next: Cell::new(None),
            marker_calls: Cell::new(0),
        });

        heap.minor_collect_with_post_mark(&OneRoot(Some(young_survivor)), |_| {});

        assert_eq!(heap.allgc_count(), 2);
        assert_eq!(old_unreachable.age(), GcAge::Old);
        assert_eq!(young_survivor.age(), GcAge::Survival);
        assert!(young_survivor.color().is_white());
    }

    #[test]
    fn minor_collect_skips_untouched_old_root_scan_work() {
        let heap = Heap::new();
        heap.unpause();
        let old_root = heap.allocate(Cell0 {
            next: Cell::new(None),
            marker_calls: Cell::new(0),
        });
        old_root.set_age(GcAge::Old);
        old_root.set_color(Color::Black);
        let young_root = heap.allocate(Cell0 {
            next: Cell::new(None),
            marker_calls: Cell::new(0),
        });

        heap.minor_collect_with_post_mark(
            &TwoRoots {
                first: Some(old_root),
                second: Some(young_root),
            },
            |_| {},
        );

        let stats = heap.last_mark_stats();
        assert_eq!(stats.marked, 2);
        assert_eq!(stats.marked_old, 1);
        assert_eq!(stats.marked_young, 1);
        assert_eq!(stats.traced, 1);
        assert_eq!(stats.traced_old, 0);
        assert_eq!(stats.traced_young, 1);
        assert_eq!(old_root.marker_calls.get(), 0);
        assert_eq!(young_root.marker_calls.get(), 1);
        assert_eq!(old_root.age(), GcAge::Old);
        assert_eq!(young_root.age(), GcAge::Survival);
    }

    #[test]
    fn minor_collect_traces_touched_old_parent() {
        let heap = Heap::new();
        heap.unpause();
        let old_root = heap.allocate(Cell0 {
            next: Cell::new(None),
            marker_calls: Cell::new(0),
        });
        old_root.set_age(GcAge::Old);
        old_root.set_color(Color::Black);
        let young_child = heap.allocate(Cell0 {
            next: Cell::new(None),
            marker_calls: Cell::new(0),
        });
        old_root.next.set(Some(young_child));
        heap.generational_backward_barrier(old_root);

        heap.minor_collect_with_post_mark(&OneRoot(Some(old_root)), |_| {});

        let stats = heap.last_mark_stats();
        assert_eq!(stats.marked, 2);
        assert_eq!(stats.marked_old, 1);
        assert_eq!(stats.marked_young, 1);
        assert_eq!(stats.traced, 2);
        assert_eq!(stats.traced_old, 1);
        assert_eq!(stats.traced_young, 1);
        assert_eq!(old_root.marker_calls.get(), 1);
        assert_eq!(young_child.marker_calls.get(), 1);
        assert_eq!(old_root.age(), GcAge::Touched2);
        assert_eq!(young_child.age(), GcAge::Survival);
    }

    #[test]
    fn minor_sweep_uses_generation_cursors_to_skip_old_tail() {
        let heap = Heap::new();
        heap.unpause();
        let mut old_objects = Vec::new();
        for _ in 0..64 {
            old_objects.push(heap.allocate(Cell0 {
                next: Cell::new(None),
                marker_calls: Cell::new(0),
            }));
        }
        heap.promote_all_to_old();
        let young_root = heap.allocate(Cell0 {
            next: Cell::new(None),
            marker_calls: Cell::new(0),
        });

        heap.minor_collect_with_post_mark(&OneRoot(Some(young_root)), |_| {});

        let stats = heap.last_sweep_stats();
        assert_eq!(stats.visited, 1);
        assert_eq!(stats.visited_young, 1);
        assert_eq!(stats.visited_old, 0);
        assert_eq!(heap.allgc_count(), old_objects.len() + 1);
        assert_eq!(young_root.age(), GcAge::Survival);
    }

    /// C1 (cohort rotation / scan bounding), translated for #113 Wave 2:
    /// the old cursor-pointer asserts (`survival`/`old1`/`reallyold`/`firstold1`
    /// `.get()`) become cohort-count asserts, because the generational
    /// boundaries are now `OwnerVec` counts, not `Cell<Option<NonNull>>`
    /// cursors, and `firstold1` is deleted. Promote one object to old (it lands
    /// in the reallyold cohort), full-sweep it away, assert every cohort count
    /// drops to zero.
    #[test]
    fn full_sweep_recounts_generation_cohorts_when_object_is_freed() {
        let heap = Heap::new();
        heap.unpause();
        let _old = heap.allocate(Cell0 {
            next: Cell::new(None),
            marker_calls: Cell::new(0),
        });
        heap.promote_all_to_old();
        let stats = heap.allgc_cohort_stats();
        assert_eq!(stats.old, 1, "the promoted object is in the reallyold cohort");
        assert_eq!((stats.new, stats.survival, stats.old1), (0, 0, 0));

        heap.full_collect(&OneRoot(None));

        assert_eq!(heap.allgc_count(), 0);
        let stats = heap.allgc_cohort_stats();
        assert_eq!(
            (stats.new, stats.survival, stats.old1, stats.old),
            (0, 0, 0, 0),
            "all cohort counts drop to zero after the sole object is swept"
        );
        assert_eq!(heap.last_sweep_stats().freed, 1);
    }

    /// W1 deletion invariant (test matrix G3, list-mechanics half): a
    /// cross-list move of a gray-listed object deletes its grayagain entry and
    /// clears `HDR_GRAY_LISTED`, via `unlink_from_list` ->
    /// `correct_generation_pointers` -> `unlink_grayagain`. Covers both the
    /// `allgc -> finobj` and `tobefnz -> allgc` transitions, and every move
    /// returns `true` (R1 item 7 sync note). Deterministic; identical on
    /// pristine and W1 code.
    #[test]
    fn cross_list_move_deletes_grayagain_entry() {
        let heap = Heap::new();
        heap.unpause();
        let obj = heap.allocate(Cell0 {
            next: Cell::new(None),
            marker_calls: Cell::new(0),
        });
        obj.set_age(GcAge::Old1);
        obj.set_color(Color::Gray);
        heap.remember_minor_revisit(obj.as_trace_ptr());
        assert_eq!(heap.grayagain_count(), 1);
        assert!(heap.header_from_ptr(obj.as_trace_ptr()).gray_listed());
        assert!(heap.move_allgc_to_finobj(obj.as_trace_ptr()));
        assert_eq!(
            heap.grayagain_count(),
            0,
            "allgc->finobj move deletes the grayagain entry"
        );
        assert!(!heap.header_from_ptr(obj.as_trace_ptr()).gray_listed());

        let heap = Heap::new();
        heap.unpause();
        let obj = heap.allocate(Cell0 {
            next: Cell::new(None),
            marker_calls: Cell::new(0),
        });
        assert!(heap.move_allgc_to_finobj(obj.as_trace_ptr()));
        assert!(heap.move_finobj_to_tobefnz(obj.as_trace_ptr()));
        obj.set_age(GcAge::Old1);
        obj.set_color(Color::Gray);
        heap.remember_minor_revisit(obj.as_trace_ptr());
        assert_eq!(heap.grayagain_count(), 1);
        assert!(heap.move_tobefnz_to_allgc(obj.as_trace_ptr()));
        assert_eq!(
            heap.grayagain_count(),
            0,
            "tobefnz->allgc move deletes the grayagain entry"
        );
        assert!(!heap.header_from_ptr(obj.as_trace_ptr()).gray_listed());
    }

    /// Test matrix G3 (graph-shaped, baseline-first). Root -> exact-`Old`
    /// parent (skipped by `Marker::should_trace_age` in a minor) -> a
    /// gray-listed `Old1` transitional object -> a `New` young child. Moving
    /// the transitional off `allgc` deletes its grayagain entry (the only
    /// revisit rooting the child), then a minor runs. This is a pure
    /// generational-liveness property of *today's* collector that W1 inherits
    /// unchanged — `correct_generation_pointers`' deletion, `should_trace_age`,
    /// and the minor mark/sweep are all untouched by W1.
    ///
    /// BASELINE (recorded against pristine origin/main, via a throwaway
    /// worktree, 2026-07-13): the young child is FREED by the minor
    /// (`allgc_count` 3 -> 2). With the transitional's grayagain entry deleted
    /// by the move and the parent exact-`Old` (so the minor marker never
    /// descends into it), nothing re-marks the transitional and its `New`
    /// child is swept. This is the "reachable young child dying" case the spec
    /// anticipates (§"Every owner-list move"): a pre-existing latent hole in
    /// the generational collector, filed as issue #263, NOT a W1 regression and
    /// NOT something W1 fixes. W1 reproduces the baseline exactly. The test is
    /// `#[ignore]`d because it asserts the *correct* invariant (child should
    /// survive), which the pre-existing baseline violates; running it
    /// (`cargo test -p lua-gc -- --ignored g3_`) documents the live gap. This
    /// test is the acceptance test for the #263 fix — un-`#[ignore]` it then.
    #[test]
    #[ignore = "pre-existing generational-liveness gap (issue #263): reachable \
                young child dies when a gray-listed transitional is moved \
                cross-list; identical on pristine and W1 — not a W1 bug"]
    fn g3_moved_transitional_keeps_young_child_reachable() {
        let heap = Heap::new();
        heap.unpause();
        let child = heap.allocate(Cell0 {
            next: Cell::new(None),
            marker_calls: Cell::new(0),
        });
        let transitional = heap.allocate(Cell0 {
            next: Cell::new(Some(child)),
            marker_calls: Cell::new(0),
        });
        let parent = heap.allocate(Cell0 {
            next: Cell::new(Some(transitional)),
            marker_calls: Cell::new(0),
        });
        parent.set_age(GcAge::Old);
        parent.set_color(Color::Black);
        transitional.set_age(GcAge::Old1);
        transitional.set_color(Color::Gray);
        heap.remember_minor_revisit(transitional.as_trace_ptr());
        assert_eq!(heap.grayagain_count(), 1);
        assert_eq!(heap.allgc_count(), 3);

        assert!(heap.move_allgc_to_finobj(transitional.as_trace_ptr()));
        assert_eq!(
            heap.grayagain_count(),
            0,
            "cross-list move deletes the grayagain entry"
        );

        heap.minor_collect_with_post_mark(&OneRoot(Some(parent)), |_| {});

        assert_eq!(
            heap.allgc_count(),
            3,
            "reachable young child should survive the minor"
        );
    }

    /// Baseline-preservation pin for the #263 scenario (codex W1 review,
    /// finding 1): the SAME graph as `g3_moved_transitional_keeps_young_child_
    /// reachable`, but asserting what the collector does TODAY — the
    /// reachable young child is freed. Runs in normal CI so any W1-era
    /// divergence in this scenario is caught immediately. When #263 is fixed,
    /// this test must be inverted/removed together with un-ignoring G3.
    #[test]
    fn g3_baseline_child_freed_after_move_allgc_to_finobj() {
        let heap = Heap::new();
        heap.unpause();
        let child = heap.allocate(Cell0 {
            next: Cell::new(None),
            marker_calls: Cell::new(0),
        });
        let transitional = heap.allocate(Cell0 {
            next: Cell::new(Some(child)),
            marker_calls: Cell::new(0),
        });
        let parent = heap.allocate(Cell0 {
            next: Cell::new(Some(transitional)),
            marker_calls: Cell::new(0),
        });
        parent.set_age(GcAge::Old);
        parent.set_color(Color::Black);
        transitional.set_age(GcAge::Old1);
        transitional.set_color(Color::Gray);
        heap.remember_minor_revisit(transitional.as_trace_ptr());

        assert!(heap.move_allgc_to_finobj(transitional.as_trace_ptr()));
        assert_eq!(heap.grayagain_count(), 0);

        heap.minor_collect_with_post_mark(&OneRoot(Some(parent)), |_| {});

        assert_eq!(
            heap.allgc_count(),
            2,
            "baseline (pre-#263-fix): the young child is freed; if this \
             changes, either #263 was fixed (invert this test and un-ignore \
             G3) or minor liveness regressed"
        );
    }

    /// Baseline-preservation pin, `move_tobefnz_to_allgc` variant (codex W1
    /// review, finding 1): the transitional travels allgc -> finobj ->
    /// tobefnz, is re-remembered for revisit there, and the move BACK to
    /// allgc deletes the entry; the following minor frees the reachable
    /// young child, same as the finobj variant. Invert together with the
    /// #263 fix.
    #[test]
    fn g3_baseline_child_freed_after_move_tobefnz_to_allgc() {
        let heap = Heap::new();
        heap.unpause();
        let child = heap.allocate(Cell0 {
            next: Cell::new(None),
            marker_calls: Cell::new(0),
        });
        let transitional = heap.allocate(Cell0 {
            next: Cell::new(Some(child)),
            marker_calls: Cell::new(0),
        });
        let parent = heap.allocate(Cell0 {
            next: Cell::new(Some(transitional)),
            marker_calls: Cell::new(0),
        });
        parent.set_age(GcAge::Old);
        parent.set_color(Color::Black);
        transitional.set_age(GcAge::Old1);
        transitional.set_color(Color::Gray);

        assert!(heap.move_allgc_to_finobj(transitional.as_trace_ptr()));
        assert!(heap.move_finobj_to_tobefnz(transitional.as_trace_ptr()));
        heap.remember_minor_revisit(transitional.as_trace_ptr());
        assert_eq!(heap.grayagain_count(), 1);

        assert!(heap.move_tobefnz_to_allgc(transitional.as_trace_ptr()));
        assert_eq!(
            heap.grayagain_count(),
            0,
            "tobefnz->allgc move deletes the grayagain entry"
        );

        heap.minor_collect_with_post_mark(&OneRoot(Some(parent)), |_| {});

        assert_eq!(
            heap.allgc_count(),
            2,
            "baseline (pre-#263-fix): the young child is freed after the \
             tobefnz->allgc move deletes the revisit entry; invert together \
             with the #263 fix"
        );
    }

    /// W1 teardown ordering (test matrix G3 teardown case / drop_all). A
    /// gray-listed object is present when `drop_all` runs;
    /// `clear_generation_cursors` -> `clear_grayagain` empties the grayagain
    /// `Vec` (and clears flags) *before* any `drop_list` frees a box, so no
    /// grayagain entry can point at a freed box. After teardown the set is
    /// empty and the heap holds nothing.
    #[test]
    fn drop_all_clears_grayagain_before_freeing() {
        let heap = Heap::new();
        heap.unpause();
        let obj = heap.allocate(Cell0 {
            next: Cell::new(None),
            marker_calls: Cell::new(0),
        });
        obj.set_age(GcAge::Old);
        heap.generational_backward_barrier(obj);
        assert_eq!(heap.grayagain_count(), 1);

        heap.drop_all();

        assert_eq!(heap.grayagain_count(), 0);
        assert_eq!(heap.allgc_count(), 0);
    }

    /// Header-size regression for #113 Wave 2. Removing the last intrusive
    /// link (`next`) shrinks `GcHeader` from 24 B to **8 B** — `color + age +
    /// flags + pad + size(u32)`, align 4. The header now contains no pointer,
    /// so the size is width-independent (8 B on wasm32 too); this runtime test
    /// backs the `const _` compile-time assert next to the struct, which pins
    /// it ungated on both widths.
    #[test]
    fn gcheader_is_8_bytes_after_owner_vector_diet() {
        assert_eq!(std::mem::size_of::<GcHeader>(), 8);
    }

    #[test]
    fn full_sweep_unlinks_freed_grayagain_entries() {
        let heap = Heap::new();
        heap.unpause();
        let parent = heap.allocate(Cell0 {
            next: Cell::new(None),
            marker_calls: Cell::new(0),
        });
        heap.promote_all_to_old();
        heap.generational_backward_barrier(parent);
        assert_eq!(heap.grayagain_count(), 1);

        heap.full_collect(&OneRoot(None));

        assert_eq!(heap.allgc_count(), 0);
        assert_eq!(heap.grayagain_count(), 0);

        let young = heap.allocate(Cell0 {
            next: Cell::new(None),
            marker_calls: Cell::new(0),
        });
        heap.minor_collect_with_post_mark(&OneRoot(Some(young)), |_| {});
        assert_eq!(young.age(), GcAge::Survival);
    }

    #[test]
    fn grayagain_links_object_once() {
        let heap = Heap::new();
        heap.unpause();
        let parent = heap.allocate(Cell0 {
            next: Cell::new(None),
            marker_calls: Cell::new(0),
        });
        parent.set_age(GcAge::Old);
        parent.set_color(Color::Black);

        heap.generational_backward_barrier(parent);
        heap.generational_backward_barrier(parent);

        assert_eq!(heap.grayagain_count(), 1);
    }

    #[test]
    fn grayagain_list_carries_old1_until_old() {
        let heap = Heap::new();
        heap.unpause();
        let survivor = heap.allocate(Cell0 {
            next: Cell::new(None),
            marker_calls: Cell::new(0),
        });

        heap.minor_collect_with_post_mark(&OneRoot(Some(survivor)), |_| {});
        assert_eq!(survivor.age(), GcAge::Survival);

        heap.minor_collect_with_post_mark(&OneRoot(Some(survivor)), |_| {});
        assert_eq!(survivor.age(), GcAge::Old1);

        heap.minor_collect_with_post_mark(&OneRoot(None), |_| {});
        assert_eq!(survivor.age(), GcAge::Old);
        assert_eq!(heap.allgc_count(), 1);
    }

    #[test]
    fn grayagain_list_carries_touched2_until_old() {
        let heap = Heap::new();
        heap.unpause();
        let parent = heap.allocate(Cell0 {
            next: Cell::new(None),
            marker_calls: Cell::new(0),
        });
        parent.set_age(GcAge::Old);
        parent.set_color(Color::Black);
        heap.generational_backward_barrier(parent);

        heap.minor_collect_with_post_mark(&OneRoot(None), |_| {});
        assert_eq!(parent.age(), GcAge::Touched2);

        heap.minor_collect_with_post_mark(&OneRoot(None), |_| {});
        assert_eq!(parent.age(), GcAge::Old);
        assert_eq!(parent.marker_calls.get(), 2);
    }

    #[test]
    fn collect_traverses_cycles() {
        let heap = Heap::new();
        heap.unpause();
        let a = heap.allocate(Cell0 {
            next: Cell::new(None),
            marker_calls: Cell::new(0),
        });
        let b = heap.allocate(Cell0 {
            next: Cell::new(Some(a)),
            marker_calls: Cell::new(0),
        });
        a.next.set(Some(b)); // cycle
                             // With a as root, both should survive.
        heap.full_collect(&OneRoot(Some(a)));
        assert_eq!(a.marker_calls.get(), 1);
        assert_eq!(b.marker_calls.get(), 1);
        // Drop the only root path; cycle should now be collected.
        // (Note: `a` and `b` are still on the stack as Gc<Cell0> handles, but
        // they're not in a Trace-visible position.)
        heap.full_collect(&OneRoot(None));
        assert_eq!(heap.bytes_used(), 0);
    }

    #[test]
    fn heap_guard_stacks() {
        assert!(
            with_current_heap(|heap| heap.is_none()),
            "no guard initially"
        );
        let h1 = Heap::new();
        h1.unpause();
        {
            let _g1 = HeapGuard::push(&h1);
            assert!(with_current_heap(|heap| heap.is_some()));
            let h2 = Heap::new();
            h2.unpause();
            {
                let _g2 = HeapGuard::push(&h2);
                // top of stack is h2
                with_current_heap(|heap| {
                    assert!(std::rc::Rc::ptr_eq(heap.unwrap(), &h2));
                });
            }
            // _g2 dropped — top is back to h1
            with_current_heap(|heap| {
                assert!(std::rc::Rc::ptr_eq(heap.unwrap(), &h1));
            });
        }
        assert!(
            with_current_heap(|heap| heap.is_none()),
            "stack empty after all guards drop"
        );
    }

    #[test]
    fn step_threshold_triggers_collect() {
        let heap = Heap::new();
        heap.unpause();
        // Allocate enough boxes to exceed the default 64KB threshold.
        let mut keeps = Vec::new();
        for _ in 0..64 {
            // ~1KB per box (Cell0 is small, but allocating many headers
            // accumulates). For the threshold test we'd typically allocate
            // larger objects; this is a smoke test.
            keeps.push(heap.allocate(Cell0 {
                next: Cell::new(None),
                marker_calls: Cell::new(0),
            }));
        }
        // Build roots that retain all of keeps via individual marks.
        struct ManyRoots<'a>(&'a [Gc<Cell0>]);
        impl<'a> Trace for ManyRoots<'a> {
            fn trace(&self, m: &mut Marker) {
                for g in self.0.iter() {
                    m.mark(*g);
                }
            }
        }
        heap.step(&ManyRoots(&keeps));
        // step is a no-op below threshold; all roots retained.
        assert!(heap.bytes_used() > 0);
    }

    #[test]
    fn threshold_floored_after_collecting_tiny_heap() {
        let heap = Heap::new();
        heap.unpause();
        struct NoRoots;
        impl Trace for NoRoots {
            fn trace(&self, _m: &mut Marker) {}
        }
        for _ in 0..200 {
            heap.allocate(Cell0 {
                next: Cell::new(None),
                marker_calls: Cell::new(0),
            });
        }
        heap.full_collect(&NoRoots);
        assert!(
            heap.threshold_bytes() >= GC_MIN_THRESHOLD,
            "threshold {} collapsed below floor {}; a churning program would full-collect per allocation",
            heap.threshold_bytes(),
            GC_MIN_THRESHOLD
        );
    }

    fn build_chain(heap: &Heap, len: usize) -> Gc<Cell0> {
        let head = heap.allocate(Cell0 {
            next: Cell::new(None),
            marker_calls: Cell::new(0),
        });
        let mut cur = head;
        for _ in 1..len {
            let n = heap.allocate(Cell0 {
                next: Cell::new(None),
                marker_calls: Cell::new(0),
            });
            cur.next.set(Some(n));
            cur = n;
        }
        head
    }

    #[test]
    fn budget_zero_does_some_work() {
        let heap = Heap::new();
        heap.unpause();
        let head = build_chain(&heap, 8);
        let roots = OneRoot(Some(head));
        let budget = StepBudget::from_work(0);
        let outcome = heap.incremental_step_with_post_mark(&roots, budget, |_| {});
        assert_ne!(outcome, StepOutcome::SkippedStopped);
        assert_ne!(heap.gc_state(), GcState::Pause);
    }

    #[test]
    fn run_until_state_stops_before_next_phase_work() {
        let heap = Heap::new();
        heap.unpause();
        let head = build_chain(&heap, 8);
        let roots = OneRoot(Some(head));
        let atomic_calls = Cell::new(0);

        let outcome =
            heap.incremental_run_until_state_with_post_mark(&roots, GcState::Atomic, 1024, |_| {
                atomic_calls.set(atomic_calls.get() + 1)
            });
        assert_eq!(outcome, StepOutcome::InProgress);
        assert_eq!(heap.gc_state(), GcState::Atomic);
        assert_eq!(
            atomic_calls.get(),
            0,
            "atomic hook must not run before inspection"
        );

        let outcome = heap.incremental_run_until_state_with_post_mark(
            &roots,
            GcState::SweepAllGc,
            1024,
            |_| atomic_calls.set(atomic_calls.get() + 1),
        );
        assert_eq!(outcome, StepOutcome::InProgress);
        assert_eq!(heap.gc_state(), GcState::SweepAllGc);
        assert_eq!(
            atomic_calls.get(),
            1,
            "entering sweep must run the atomic hook once"
        );
    }

    #[test]
    fn larger_budget_drains_more_gray_than_smaller() {
        let small_heap = Heap::new();
        small_heap.unpause();
        let h1 = build_chain(&small_heap, 64);
        let r1 = OneRoot(Some(h1));
        let mut small_calls = 0;
        loop {
            small_calls += 1;
            let outcome =
                small_heap.incremental_step_with_post_mark(&r1, StepBudget::from_work(2), |_| {});
            if outcome == StepOutcome::Paused {
                break;
            }
            assert!(small_calls < 10_000, "did not converge");
        }

        let big_heap = Heap::new();
        big_heap.unpause();
        let h2 = build_chain(&big_heap, 64);
        let r2 = OneRoot(Some(h2));
        let mut big_calls = 0;
        loop {
            big_calls += 1;
            let outcome =
                big_heap.incremental_step_with_post_mark(&r2, StepBudget::from_work(64), |_| {});
            if outcome == StepOutcome::Paused {
                break;
            }
            assert!(big_calls < 10_000, "did not converge");
        }

        assert!(
            big_calls < small_calls,
            "expected big_calls={} < small_calls={}",
            big_calls,
            small_calls
        );
    }

    #[test]
    fn sweep_can_pause_and_resume() {
        let heap = Heap::new();
        heap.unpause();
        for _ in 0..16 {
            let _ = heap.allocate(Cell0 {
                next: Cell::new(None),
                marker_calls: Cell::new(0),
            });
        }
        let roots = OneRoot(None);
        let bytes_before = heap.bytes_used();
        assert!(bytes_before > 0);
        let mut step_count = 0;
        let mut saw_in_progress_during_sweep = false;
        loop {
            step_count += 1;
            let outcome =
                heap.incremental_step_with_post_mark(&roots, StepBudget::from_work(2), |_| {});
            if heap.gc_state().is_sweep() && outcome == StepOutcome::InProgress {
                saw_in_progress_during_sweep = true;
            }
            if outcome == StepOutcome::Paused {
                break;
            }
            assert!(step_count < 10_000, "did not converge");
        }
        assert!(saw_in_progress_during_sweep, "sweep never paused mid-list");
        assert_eq!(heap.bytes_used(), 0);
    }

    #[test]
    fn post_mark_runs_once_per_atomic() {
        let heap = Heap::new();
        heap.unpause();
        for _ in 0..32 {
            let _ = heap.allocate(Cell0 {
                next: Cell::new(None),
                marker_calls: Cell::new(0),
            });
        }
        let roots = OneRoot(None);
        let call_count = std::cell::Cell::new(0);
        let mut step_count = 0;
        loop {
            step_count += 1;
            let outcome =
                heap.incremental_step_with_post_mark(&roots, StepBudget::from_work(2), |_| {
                    call_count.set(call_count.get() + 1);
                });
            if outcome == StepOutcome::Paused {
                break;
            }
            assert!(step_count < 10_000, "did not converge");
        }
        assert_eq!(
            call_count.get(),
            1,
            "post_mark must run exactly once per cycle"
        );
    }

    #[test]
    fn full_collect_equivalent_to_incremental_to_pause() {
        let h1 = Heap::new();
        h1.unpause();
        let head1 = h1.allocate(Cell0 {
            next: Cell::new(None),
            marker_calls: Cell::new(0),
        });
        let _orphan1 = h1.allocate(Cell0 {
            next: Cell::new(None),
            marker_calls: Cell::new(0),
        });
        let _orphan2 = h1.allocate(Cell0 {
            next: Cell::new(None),
            marker_calls: Cell::new(0),
        });
        let roots1 = OneRoot(Some(head1));
        h1.full_collect(&roots1);
        let bytes_full = h1.bytes_used();

        let h2 = Heap::new();
        h2.unpause();
        let head2 = h2.allocate(Cell0 {
            next: Cell::new(None),
            marker_calls: Cell::new(0),
        });
        let _orphan3 = h2.allocate(Cell0 {
            next: Cell::new(None),
            marker_calls: Cell::new(0),
        });
        let _orphan4 = h2.allocate(Cell0 {
            next: Cell::new(None),
            marker_calls: Cell::new(0),
        });
        let roots2 = OneRoot(Some(head2));
        loop {
            let outcome =
                h2.incremental_step_with_post_mark(&roots2, StepBudget::from_work(1), |_| {});
            if outcome == StepOutcome::Paused {
                break;
            }
        }
        assert_eq!(h2.bytes_used(), bytes_full);
    }

    // ── issue #249: guard-less/bootstrap allocations must not outlive Heap::drop ──

    /// Sets an `Rc<Cell<bool>>` flag when the value it wraps drops, so a test
    /// can observe whether a box was actually freed.
    struct DropFlag(std::rc::Rc<Cell<bool>>);
    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.set(true);
        }
    }
    struct Tracked(#[allow(dead_code)] DropFlag);
    impl Trace for Tracked {
        fn trace(&self, _m: &mut Marker) {}
    }

    #[test]
    fn allocate_uncollected_survives_collection_but_is_freed_on_heap_drop() {
        let heap = Heap::new();
        heap.unpause();
        let dropped = std::rc::Rc::new(Cell::new(false));
        let _gc = heap.allocate_uncollected(Tracked(DropFlag(dropped.clone())));

        // Not on head/finobj/tobefnz, so a full collection with no roots at
        // all must not touch it.
        heap.full_collect(&OneRoot(None));
        assert!(
            !dropped.get(),
            "allocate_uncollected box must survive a full collection while the heap is alive"
        );

        drop(heap);
        assert!(
            dropped.get(),
            "allocate_uncollected box must be freed once its heap drops (issue #249)"
        );
    }

    #[test]
    fn bootstrapping_routes_allocate_to_the_uncollected_list() {
        let heap = Heap::new();
        heap.unpause();
        assert!(!heap.is_bootstrapping());

        heap.begin_bootstrap();
        let dropped = std::rc::Rc::new(Cell::new(false));
        let _gc = heap.allocate(Tracked(DropFlag(dropped.clone())));

        // Routed through allocate_uncollected: invisible to the normal
        // collectable count and immune to a same-cycle collection.
        assert_eq!(
            heap.allgc_count(),
            0,
            "a bootstrap allocation must not join the normal collectable list"
        );
        heap.full_collect(&OneRoot(None));
        assert!(
            !dropped.get(),
            "a bootstrap allocation must survive collection while bootstrapping is active"
        );

        heap.end_bootstrap();
        drop(heap);
        assert!(
            dropped.get(),
            "a bootstrap allocation must still be freed when the heap drops"
        );
    }

    /// Pins the ownership-flag semantics the strict-guard checks key on
    /// (`GcRef::downgrade`/`account_buffer` panic for guard-less operations
    /// on any heap-owned box): a bootstrap box is heap-owned but not
    /// sweepable, so treating "not sweepable" as "not hazardous" would let
    /// a weak handle to it dangle after `drop_all` frees the box at heap
    /// teardown.
    #[test]
    fn ownership_flags_distinguish_all_three_allocation_paths() {
        let heap = Heap::new();
        let tracked = heap.allocate(Tracked(DropFlag(std::rc::Rc::new(Cell::new(false)))));
        assert!(tracked.is_heap_owned());
        assert!(tracked.is_heap_tracked());

        let bootstrap =
            heap.allocate_uncollected(Tracked(DropFlag(std::rc::Rc::new(Cell::new(false)))));
        assert!(
            bootstrap.is_heap_owned(),
            "bootstrap boxes die in drop_all — strict-guard hazard checks must see them"
        );
        assert!(!bootstrap.is_heap_tracked());

        let detached = Gc::new_uncollected(Tracked(DropFlag(std::rc::Rc::new(Cell::new(false)))));
        assert!(!detached.is_heap_owned());
        assert!(!detached.is_heap_tracked());
    }

    #[test]
    fn end_bootstrap_restores_normal_sweepable_allocation() {
        let heap = Heap::new();
        heap.unpause();
        heap.begin_bootstrap();
        heap.end_bootstrap();

        let dropped = std::rc::Rc::new(Cell::new(false));
        let _gc = heap.allocate(Tracked(DropFlag(dropped.clone())));
        assert_eq!(heap.allgc_count(), 1);

        heap.full_collect(&OneRoot(None));
        // Not a drop-flag assertion: under LUA_RS_GC_QUARANTINE=1, sweep
        // unlinks the box and parks it (poisoned) instead of freeing it
        // immediately, so its value's Drop doesn't run until heap teardown.
        // `allgc_count` reflects the unlink either way — quarantine mode
        // settles owner-list accounting the same as a real free.
        assert_eq!(
            heap.allgc_count(),
            0,
            "after end_bootstrap, an unreachable allocation must be swept normally"
        );
    }

    // ── issue #260: deterministic close — drain-until-stable drop_all + closed flag ──

    /// A `Trace` payload whose `Drop` flips its flag and then allocates a
    /// fresh drop-flag box into the currently-active heap. Exercises
    /// `drop_all`'s drain-until-stable loop: a destructor that allocates
    /// mid-teardown must have its box freed by a later pass, not stranded.
    struct AllocOnDrop {
        flag: std::rc::Rc<Cell<bool>>,
        inner: std::rc::Rc<Cell<bool>>,
    }
    impl Trace for AllocOnDrop {
        fn trace(&self, _m: &mut Marker) {}
    }
    impl Drop for AllocOnDrop {
        fn drop(&mut self) {
            self.flag.set(true);
            with_current_heap(|heap| {
                if let Some(heap) = heap {
                    let _ = heap.allocate(Tracked(DropFlag(self.inner.clone())));
                }
            });
        }
    }

    #[test]
    fn close_frees_objects_while_outer_guard_alive() {
        let heap = Heap::new();
        heap.unpause();
        let guard = HeapGuard::push(&heap);

        let dropped = std::rc::Rc::new(Cell::new(false));
        let _gc = heap.allocate(Tracked(DropFlag(dropped.clone())));
        assert!(!dropped.get());

        heap.drop_all();
        assert!(
            dropped.get(),
            "drop_all must run the object's destructor during the close call, \
             while the outer guard is still alive"
        );
        assert!(heap.is_closed());
        assert_eq!(heap.bytes_used(), 0);
        drop(guard);
    }

    #[test]
    #[should_panic(expected = "closed heap")]
    fn allocate_into_closed_heap_panics() {
        let heap = Heap::new();
        heap.unpause();
        let _guard = HeapGuard::push(&heap);
        heap.drop_all();
        let _ = heap.allocate(Tracked(DropFlag(std::rc::Rc::new(Cell::new(false)))));
    }

    #[test]
    fn destructor_allocating_during_teardown_does_not_leak() {
        let heap = Heap::new();
        heap.unpause();
        let _guard = HeapGuard::push(&heap);

        let outer = std::rc::Rc::new(Cell::new(false));
        let inner = std::rc::Rc::new(Cell::new(false));
        let _gc = heap.allocate(AllocOnDrop {
            flag: outer.clone(),
            inner: inner.clone(),
        });

        heap.drop_all();
        assert!(outer.get(), "the destructor itself must have run");
        assert!(
            inner.get(),
            "the box the destructor allocated mid-teardown must be freed by a \
             later drain pass, not leaked"
        );
        assert_eq!(heap.bytes_used(), 0);
    }

    // ── issue #113 Wave 2: destruction ownership (Phase 4) ────────────────

    /// A dead `Trace` payload whose `Drop` re-enters the collector via
    /// `full_collect` and records the collector state it observed. Used to prove
    /// the `releasing` window makes a nested collection inert.
    struct NestedCollectOnDrop {
        observed: std::rc::Rc<Cell<Option<GcState>>>,
    }
    impl Trace for NestedCollectOnDrop {
        fn trace(&self, _m: &mut Marker) {}
    }
    impl Drop for NestedCollectOnDrop {
        fn drop(&mut self) {
            with_current_heap(|heap| {
                if let Some(heap) = heap {
                    heap.full_collect(&OneRoot(None));
                    self.observed.set(Some(heap.gc_state()));
                }
            });
        }
    }

    /// P4: a collection entered from inside the `pending_release` drain is inert
    /// (`releasing` folded into `collection_inert`). If it were not, the nested
    /// `full_collect` would `abort_cycle` and complete a fresh cycle, leaving the
    /// state at `Pause`; inert, it returns immediately with the outer sweep phase
    /// still current. This is the R2-finding-1 nested-collection hazard: a
    /// destructor must not trace-then-free a still-pending peer.
    #[test]
    fn nested_collection_during_release_drain_is_inert() {
        let heap = Heap::new();
        heap.unpause();
        let _guard = HeapGuard::push(&heap);
        let observed = std::rc::Rc::new(Cell::new(None));
        heap.allocate(NestedCollectOnDrop {
            observed: observed.clone(),
        });

        heap.full_collect(&OneRoot(None));

        let state = observed
            .get()
            .expect("the dead probe's Drop ran during the release drain");
        assert!(
            state.is_sweep(),
            "a collection entered from inside the release drain must be inert \
             (releasing set); saw {state:?}"
        );
        assert_eq!(heap.allgc_count(), 0);
    }

    /// A dead `Trace` payload whose `Drop` charges a *peer* box's buffer bytes
    /// while that peer is still owned by `pending_release`. The peer keeps
    /// `HDR_COLLECTED`, so `account_buffer` succeeds — the R2 reentrancy
    /// counterexample.
    struct ChargePeerOnDrop {
        peer: Gc<Cell0>,
        charge: isize,
    }
    impl Trace for ChargePeerOnDrop {
        fn trace(&self, _m: &mut Marker) {}
    }
    impl Drop for ChargePeerOnDrop {
        fn drop(&mut self) {
            with_current_heap(|heap| {
                if let Some(heap) = heap {
                    self.peer.account_buffer(heap, self.charge);
                }
            });
        }
    }

    /// P4: per-object-in-pop-order release accounting defeats the R2 pacer-drift
    /// counterexample. Dead peers `b` (slot 0) and the charger (slot 1) land in
    /// one `pending_release` batch; the charger pops first (LIFO) and charges
    /// `b` +4096 while `b` is still pending and unrefunded, so `b`'s later
    /// refund cancels the charge exactly. Under the withdrawn batch-accounting
    /// design (refund both up front) the +4096 would drift `bytes` permanently.
    #[test]
    fn per_object_release_accounting_survives_reentrant_account_buffer() {
        let heap = Heap::new();
        heap.unpause();
        let _guard = HeapGuard::push(&heap);
        let baseline = heap.bytes_used();
        let b = heap.allocate(Cell0 {
            next: Cell::new(None),
            marker_calls: Cell::new(0),
        });
        heap.allocate(ChargePeerOnDrop {
            peer: b,
            charge: 4096,
        });

        heap.full_collect(&OneRoot(None));

        assert_eq!(heap.allgc_count(), 0);
        assert_eq!(
            heap.bytes_used(),
            baseline,
            "a reentrant account_buffer on a still-pending dead peer must be \
             refunded when that peer is drained — no pacer drift"
        );
    }

    #[test]
    fn weak_handles_dead_immediately_after_close() {
        let heap = Heap::new();
        heap.unpause();
        let _guard = HeapGuard::push(&heap);

        let gc = heap.allocate(Tracked(DropFlag(std::rc::Rc::new(Cell::new(false)))));
        let identity = gc.identity();
        let token = heap.register_allocation_token(identity);
        let weak = HeapRef::from_heap(&heap);
        assert!(
            weak.contains_allocation(identity, token),
            "weak handle upgrades while the box is live"
        );

        heap.drop_all();

        assert!(
            !weak.contains_allocation(identity, token),
            "weak handle must fail to upgrade the instant close clears the \
             allocation tokens, while the heap Rc is still alive"
        );
    }

    /// A `Trace` payload whose `Drop` allocates a fresh box and fires the
    /// generational-barrier path (`remember_minor_revisit`) on it, mid-drain.
    /// Codex finding 1 on #260: before the closed-heap gate, that pushed a
    /// pointer into grayagain which a later drain pass freed — a second
    /// `drop_all` (or `Heap::drop` after an explicit close) then dereferenced
    /// the freed header in `clear_grayagain`.
    struct BarrierOnDrop {
        flag: std::rc::Rc<Cell<bool>>,
        inner: std::rc::Rc<Cell<bool>>,
    }
    impl Trace for BarrierOnDrop {
        fn trace(&self, _m: &mut Marker) {}
    }
    impl Drop for BarrierOnDrop {
        fn drop(&mut self) {
            self.flag.set(true);
            with_current_heap(|heap| {
                if let Some(heap) = heap {
                    let fresh = heap.allocate(Tracked(DropFlag(self.inner.clone())));
                    heap.remember_minor_revisit(fresh.as_trace_ptr());
                }
            });
        }
    }

    #[test]
    fn teardown_barrier_cannot_repopulate_grayagain() {
        let heap = Heap::new();
        heap.unpause();
        let _guard = HeapGuard::push(&heap);

        let outer = std::rc::Rc::new(Cell::new(false));
        let inner = std::rc::Rc::new(Cell::new(false));
        let _gc = heap.allocate(BarrierOnDrop {
            flag: outer.clone(),
            inner: inner.clone(),
        });

        heap.drop_all();
        assert!(outer.get(), "the destructor itself must have run");
        assert!(
            inner.get(),
            "the box the destructor allocated must still be drained"
        );
        assert_eq!(
            heap.grayagain_count(),
            0,
            "a barrier fired during teardown must not park a soon-freed \
             pointer in grayagain"
        );

        heap.drop_all();
        assert_eq!(heap.grayagain_count(), 0);
        assert_eq!(heap.bytes_used(), 0);
    }

    /// A `Trace` payload whose `Drop` allocates a fresh box, roots it, and
    /// invokes minor + full collections mid-drain. Codex round 2 finding 1 on
    /// #260: with only the barrier path gated, a destructor-run
    /// `minor_collect` reaches `sweep_young` → `replace_grayagain` and
    /// re-installs grayagain entries a later drain pass frees — recreating
    /// the second-`drop_all` UAF. Every collection entry point must be inert
    /// on a closed heap.
    struct CollectOnDrop {
        flag: std::rc::Rc<Cell<bool>>,
        inner: std::rc::Rc<Cell<bool>>,
    }
    struct TrackedRoot(Option<Gc<Tracked>>);
    impl Trace for TrackedRoot {
        fn trace(&self, m: &mut Marker) {
            if let Some(g) = self.0 {
                m.mark(g);
            }
        }
    }
    impl Trace for CollectOnDrop {
        fn trace(&self, _m: &mut Marker) {}
    }
    impl Drop for CollectOnDrop {
        fn drop(&mut self) {
            self.flag.set(true);
            with_current_heap(|heap| {
                if let Some(heap) = heap {
                    let fresh = heap.allocate(Tracked(DropFlag(self.inner.clone())));
                    let root = TrackedRoot(Some(fresh));
                    heap.minor_collect_with_post_mark(&root, |_: &mut Marker| {});
                    heap.full_collect(&root);
                    heap.step(&root);
                }
            });
        }
    }

    #[test]
    fn collections_invoked_by_teardown_destructor_are_inert() {
        let heap = Heap::new();
        heap.unpause();
        let _guard = HeapGuard::push(&heap);

        let outer = std::rc::Rc::new(Cell::new(false));
        let inner = std::rc::Rc::new(Cell::new(false));
        let _gc = heap.allocate(CollectOnDrop {
            flag: outer.clone(),
            inner: inner.clone(),
        });

        heap.drop_all();
        assert!(outer.get(), "the destructor itself must have run");
        assert!(
            inner.get(),
            "the box the destructor allocated must still be drained"
        );
        assert_eq!(
            heap.minor_collections(),
            0,
            "a minor collection invoked from a teardown destructor must be inert"
        );
        assert_eq!(
            heap.full_collections(),
            0,
            "a full collection invoked from a teardown destructor must be inert"
        );
        assert_eq!(heap.grayagain_count(), 0);

        heap.drop_all();
        assert_eq!(heap.grayagain_count(), 0);
        assert_eq!(heap.bytes_used(), 0);
        assert!(!heap.would_collect());
    }

    /// Codex finding 2 on #260, heap-level half: after close, the token
    /// machinery must refuse to mint (0 is never a valid token) and
    /// `contains_allocation` must be unconditionally false, so a post-close
    /// downgrade cannot resurrect a freed box as an upgradable weak target.
    #[test]
    fn post_close_token_minting_is_refused() {
        let heap = Heap::new();
        heap.unpause();
        let _guard = HeapGuard::push(&heap);

        let gc = heap.allocate(Tracked(DropFlag(std::rc::Rc::new(Cell::new(false)))));
        let identity = gc.identity();

        heap.drop_all();

        let token = heap.register_allocation_token(identity);
        assert_eq!(token, 0, "a closed heap must refuse to mint a token");
        assert!(
            !heap.contains_allocation(identity, token),
            "the refused token must never validate"
        );
        assert!(
            heap.allocation_token(identity).is_none(),
            "the refusal must not have touched the token map"
        );
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        production Rust heap/collector substrate
//   target_crate:  lua-gc
//   confidence:    medium
//   todos:         0
//   port_notes:    0
//   unsafe_blocks: 13
//   notes:         Mark-and-sweep collector heap + the Gc<T> raw-pointer substrate. Uses
//                  NonNull<GcBox<T>> with linked-list allgc walking; unsafe is
//                  required for raw pointer ops and Box::into_raw/from_raw. See
//                  unsafe-budgets.toml for the per-crate ceiling.
// ──────────────────────────────────────────────────────────────────────────────
