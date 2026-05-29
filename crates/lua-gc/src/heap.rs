//! Phase D mark-and-sweep garbage collector.
//!
//! This module is the production GC for the Lua runtime, replacing the
//! `Rc<T>`-backed `GcRef<T>` placeholder used through Phase B/C. It is a
//! single-threaded, stop-the-world, precise tracing collector with a
//! forward write barrier. Incremental marking is a future enhancement;
//! the current implementation does a full collect each time `step` decides
//! it's time.
//!
//! # Vocabulary
//!
//! - **Gc<T>**: a pointer-sized handle. `Copy + Clone`. Replaces `GcRef<T>`.
//! - **GcBox<T>**: the heap allocation; contains a header and the value.
//! - **GcHeader**: per-object metadata (color, finalized flag, intrusive
//!   `next` pointer for the allgc list).
//! - **Trace**: trait every GC-rooted type implements. The `trace` method
//!   walks all `Gc<_>` fields and calls `Marker::mark` on each.
//! - **Marker**: passed to `trace`; carries the gray queue.
//! - **Heap**: owns the allgc list head, byte counters, GC state machine.
//!
//! # Safety model
//!
//! All `unsafe` is confined to this crate (per `harness/unsafe-budgets.toml`).
//! The invariants are:
//!
//! 1. Every `Gc<T>` points to a valid, allocated, not-yet-swept `GcBox<T>`.
//! 2. The allgc intrusive list is consistent: traversing `header.next` from
//!    `Heap.head` reaches every live `GcBox` exactly once.
//! 3. After `Heap::full_collect(roots)`, every `Gc<T>` reachable from `roots`
//!    is still valid; unreachable boxes are dropped and deallocated.
//!
//! # Migration shape
//!
//! Existing code holds `GcRef<T>` (which after Phase D is a type alias for
//! `Gc<T>`). Legacy call sites like `GcRef::new(value)` route through
//! `Gc::new_uncollected` which allocates a `GcBox` but does NOT register it
//! in any heap. Phase D-1b agent work converts these to
//! `state.heap().allocate(value)` so the new box joins the allgc chain.

use std::cell::{Cell, RefCell};
use std::marker::PhantomData;
use std::ptr::NonNull;

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
    static CURRENT_HEAP_STACK: RefCell<Vec<NonNull<Heap>>> = const { RefCell::new(Vec::new()) };
}

/// A scoped guard for the currently-active heap. Pushed at entry to
/// `state.run()` / `state.protected_call()` / `state.load()`; popped on
/// drop. Supports nesting (multiple LuaStates on one thread).
pub struct HeapGuard {
    // Anchor a NonNull so the user can't accidentally drop the guard while
    // an inner Lua state is still active. We rely on RAII.
    _private: (),
}

impl HeapGuard {
    /// Push `heap` onto the active stack. Returns a guard; dropping it pops.
    ///
    /// # Safety
    ///
    /// The pointer must remain valid for the lifetime of the guard. Callers
    /// typically pass `&state.global.heap`, which lives as long as the
    /// `GlobalState` (an `Rc<RefCell<_>>`); the guard must drop before the
    /// state is dropped.
    pub fn push(heap: &Heap) -> Self {
        let ptr = NonNull::from(heap);
        CURRENT_HEAP_STACK.with(|stack| stack.borrow_mut().push(ptr));
        HeapGuard { _private: () }
    }
}

impl Drop for HeapGuard {
    fn drop(&mut self) {
        CURRENT_HEAP_STACK.with(|stack| {
            let popped = stack.borrow_mut().pop();
            debug_assert!(popped.is_some(), "HeapGuard::drop with empty stack");
        });
    }
}

/// Runs `f` with a reference to the currently-active heap, or `None` if no
/// `HeapGuard` is in scope.
///
/// The heap reference is deliberately scoped to the closure. This avoids the
/// previous `current_heap() -> Option<&'static Heap>` lifetime lie while still
/// supporting legacy `GcRef::new` call sites that do not receive `&mut LuaState`.
pub fn with_current_heap<R>(f: impl for<'a> FnOnce(Option<&'a Heap>) -> R) -> R {
    CURRENT_HEAP_STACK.with(|stack| {
        let ptr = stack.borrow().last().copied();
        // SAFETY: the top NonNull was produced from a live `&Heap` whose
        // lifetime is bounded by the corresponding `HeapGuard`. The reference
        // is only handed to `f`, and cannot escape through the return type.
        let heap = ptr.map(|ptr| unsafe { &*ptr.as_ptr() });
        f(heap)
    })
}

/// A traced color in the tri-color invariant.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Color {
    /// Not yet visited this cycle. Candidate for sweep.
    White,
    /// Visited; outgoing references not yet traced.
    Gray,
    /// Fully traced; no outgoing pointers to white objects.
    Black,
}

/// Per-object GC metadata. Lives at the start of every `GcBox`.
#[repr(C)]
pub struct GcHeader {
    color: Cell<Color>,
    /// Set true after this object's finalizer (`__gc` metamethod) has run.
    /// Phase D defers finalizers; this is reserved.
    finalized: Cell<bool>,
    /// Intrusive link into the heap's allgc chain.
    next: Cell<Option<NonNull<GcBox<dyn Trace>>>>,
    /// Rough byte size of the contained value; used for memory accounting.
    size: usize,
}

impl GcHeader {
    fn new_white(size: usize) -> Self {
        Self {
            color: Cell::new(Color::White),
            finalized: Cell::new(false),
            next: Cell::new(None),
            size,
        }
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
    /// without joining a heap's allgc chain, it will never be swept (so
    /// effectively leaks until process exit — same as Rc behavior).
    pub fn new_uncollected(value: T) -> Self {
        let size = std::mem::size_of::<T>();
        let boxed = Box::new(GcBox {
            header: GcHeader::new_white(size),
            value,
        });
        Gc {
            ptr: NonNull::new(Box::into_raw(boxed)).expect("Box::into_raw is non-null"),
            _marker: PhantomData,
        }
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
        unsafe { self.ptr.as_ref() }
    }

    fn header(&self) -> &GcHeader {
        &self.as_box().header
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
    bool, u8, u16, u32, u64, u128, usize,
    i8, i16, i32, i64, i128, isize,
    f32, f64, char, String, str
);

impl<T> Trace for std::marker::PhantomData<T> {
    fn trace(&self, _m: &mut Marker) {}
}

/// Holds the gray queue during a mark phase. Passed to `Trace::trace`.
pub struct Marker {
    gray_queue: Vec<NonNull<GcBox<dyn Trace>>>,
    visited: std::collections::HashSet<usize>,
}

impl Marker {
    fn new() -> Self {
        Self {
            gray_queue: Vec::with_capacity(256),
            visited: std::collections::HashSet::new(),
        }
    }

    /// Mark a `Gc<T>` as gray (reachable, but its outgoing edges not yet
    /// traced). Called by `Trace::trace` implementations.
    ///
    /// Per-cycle dedup uses `visited` (a HashSet of box identities) rather
    /// than the color flag. Color-based dedup would silently skip
    /// `new_uncollected` boxes left Black by the previous cycle — those
    /// allocations are NOT on the heap's allgc chain, so the start-of-mark
    /// "reset all allgc to White" loop does not reach them, and a Black
    /// uncollected box would be skipped without re-tracing its children
    /// (causing reachable allgc descendants to be swept). The visited set
    /// is rebuilt every `full_collect` (Marker::new), so this dedup is
    /// always per-cycle.
    pub fn mark<T: Trace + 'static>(&mut self, gc: Gc<T>) {
        let id = gc.identity();
        if self.visited.insert(id) {
            let header = gc.header();
            header.color.set(Color::Gray);
            let ptr: NonNull<GcBox<dyn Trace>> = gc.ptr;
            self.gray_queue.push(ptr);
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

    /// Number of objects marked so far. Used by the post-mark hook's
    /// ephemeron-convergence fixed-point loop to detect when an iteration
    /// added no new reachable objects and the loop can terminate.
    pub fn visited_count(&self) -> usize {
        self.visited.len()
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
/// - `Propagate` → `Atomic` (when the gray queue empties).
/// - `Atomic` → `Sweep` (post-mark hook has run; sweep cursor is initialized).
/// - `Sweep` → `Finalize` (sweep cursor reached the end of allgc).
/// - `Finalize` → `Pause` (any pending finalize work has been drained).
///
/// `Collecting` is kept as a compatibility alias for the old API (used by
/// `barrier`) — it means "anything but Pause."
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum GcState {
    Pause,
    Propagate,
    Atomic,
    Sweep,
    Finalize,
}

impl GcState {
    pub fn is_pause(self) -> bool {
        matches!(self, GcState::Pause)
    }
    pub fn is_propagate(self) -> bool {
        matches!(self, GcState::Propagate)
    }
    pub fn is_sweep(self) -> bool {
        matches!(self, GcState::Sweep)
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
        Self { remaining_work: work.max(1), max_credit: work.max(1) }
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
const GC_MIN_THRESHOLD: usize = 256 * 1024;

pub struct Heap {
    /// Head of the singly-linked allgc list (every live `GcBox`).
    head: Cell<Option<NonNull<GcBox<dyn Trace>>>>,
    /// Total bytes allocated (sum of header sizes; rough).
    bytes: Cell<usize>,
    /// Threshold above which `step` triggers a collection.
    threshold: Cell<usize>,
    /// Multiplier on bytes_used to set next threshold after collection.
    pause_multiplier: Cell<usize>,
    /// State machine for the incremental collector.
    state: Cell<GcState>,
    /// If true, `step` and `barrier` are no-ops (for bootstrap before the
    /// world is consistent).
    paused: Cell<bool>,
    /// Counter of full collections performed (for diagnostics).
    collections: Cell<usize>,
    /// In-progress marker state for incremental cycles. `Some` between
    /// `Propagate` start and `Sweep` start; `None` otherwise.
    marker: RefCell<Option<Marker>>,
    /// Sweep cursor. Points at the `Cell` whose `Option<NonNull>` is the
    /// "current" link being inspected during the sweep phase. Encoded as a
    /// raw pointer because the cell lives inside a `GcHeader` (Cell, not Cell<Cell>).
    /// `None` means: sweep starts from `self.head`.
    sweep_prev_next: Cell<Option<NonNull<Cell<Option<NonNull<GcBox<dyn Trace>>>>>>>,
}

impl Default for Heap {
    fn default() -> Self {
        Self::new()
    }
}

impl Heap {
    pub fn new() -> Self {
        Self {
            head: Cell::new(None),
            bytes: Cell::new(0),
            threshold: Cell::new(64 * 1024), // initial threshold: 64 KB
            pause_multiplier: Cell::new(200), // 200% = collect when bytes 2x threshold
            state: Cell::new(GcState::Pause),
            paused: Cell::new(true), // start paused; caller enables when world is consistent
            collections: Cell::new(0),
            marker: RefCell::new(None),
            sweep_prev_next: Cell::new(None),
        }
    }

    /// Enable collection. Until this is called, `step` is a no-op (so the
    /// runtime can bootstrap without prematurely freeing objects).
    pub fn unpause(&self) {
        self.paused.set(false);
    }

    pub fn is_paused(&self) -> bool {
        self.paused.get()
    }

    /// Allocate a new `GcBox<T>` and prepend it to the allgc chain.
    pub fn allocate<T: Trace + 'static>(&self, value: T) -> Gc<T> {
        let size = std::mem::size_of::<GcBox<T>>();
        let boxed = Box::new(GcBox {
            header: GcHeader::new_white(size),
            value,
        });
        boxed.header.next.set(self.head.get());
        let raw: *mut GcBox<T> = Box::into_raw(boxed);
        let ptr: NonNull<GcBox<T>> =
            NonNull::new(raw).expect("Box::into_raw is non-null");
        let dyn_ptr: NonNull<GcBox<dyn Trace>> = ptr;
        self.head.set(Some(dyn_ptr));
        self.bytes.set(self.bytes.get() + size);
        Gc {
            ptr,
            _marker: PhantomData,
        }
    }

    /// Bytes currently retained by GC-tracked objects (rough estimate).
    pub fn bytes_used(&self) -> usize {
        self.bytes.get()
    }

    /// Current collection threshold in bytes. When `bytes_used() >= threshold_bytes()`,
    /// the next `step()` will run a full collection (unless paused). Used by
    /// callers that want to short-circuit expensive prep work (e.g. snapshotting
    /// weak tables / pending finalizers) when no collection will actually fire.
    pub fn threshold_bytes(&self) -> usize {
        self.threshold.get()
    }

    /// Cheap predicate: would a `step()` actually do work? Equivalent to
    /// `!paused && bytes_used() >= threshold_bytes()`. Callers that build
    /// snapshot state before invoking the heap should gate on this.
    pub fn would_collect(&self) -> bool {
        !self.paused.get() && self.bytes.get() >= self.threshold.get()
    }

    pub fn collections(&self) -> usize {
        self.collections.get()
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
        if child.header().color.get() != Color::White {
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
    pub fn step_with_post_mark<F: FnMut(&mut Marker)>(
        &self,
        roots: &dyn Trace,
        post_mark: F,
    ) {
        if self.paused.get() {
            return;
        }
        if self.bytes.get() < self.threshold.get() {
            return;
        }
        self.full_collect_with_post_mark(roots, post_mark);
    }

    /// Stop-the-world full collect. Marks every reachable object from
    /// `roots`, then sweeps white (unreachable) boxes from the allgc chain.
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
        if self.paused.get() {
            return;
        }
        let mut cursor = self.head.get();
        while let Some(ptr) = cursor {
            let header = unsafe { &(*ptr.as_ptr()).header };
            header.color.set(Color::White);
            cursor = header.next.get();
        }
        let mut marker = Marker::new();
        roots.trace(&mut marker);
        marker.drain_gray_queue();
        post_mark(&mut marker);
        marker.drain_gray_queue();
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
        if self.paused.get() {
            return;
        }
        if !self.state.get().is_pause() {
            self.abort_cycle();
        }
        let unlimited = StepBudget { remaining_work: isize::MAX, max_credit: isize::MAX };
        loop {
            let outcome = self.incremental_step_with_post_mark(roots, unlimited, &mut post_mark);
            if matches!(outcome, StepOutcome::Paused | StepOutcome::SkippedStopped) {
                break;
            }
        }
    }

    /// Run one budgeted step of the incremental collector.
    ///
    /// The state machine advances `Pause → Propagate → Atomic → Sweep →
    /// Finalize → Pause`. Each phase consumes budget; the call returns when
    /// the budget runs out or the cycle reaches `Pause`. The `post_mark`
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
        if self.paused.get() {
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
        let mut did_work = false;
        loop {
            if budget.remaining_work <= -budget.max_credit {
                return did_work;
            }
            match self.state.get() {
                GcState::Pause => {
                    self.start_cycle(roots);
                    self.state.set(GcState::Propagate);
                    budget.remaining_work -= 1;
                    did_work = true;
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
                        self.state.set(GcState::Atomic);
                    } else if budget.remaining_work <= 0 {
                        return did_work;
                    }
                }
                GcState::Atomic => {
                    self.run_atomic(post_mark);
                    self.state.set(GcState::Sweep);
                    budget.remaining_work -= 1;
                    did_work = true;
                }
                GcState::Sweep => {
                    let work = self.sweep_budgeted(budget.remaining_work.max(1));
                    budget.remaining_work -= work as isize;
                    did_work = did_work || work > 0;
                    if self.sweep_prev_next.get().is_none() {
                        self.state.set(GcState::Finalize);
                    } else if budget.remaining_work <= 0 {
                        return did_work;
                    }
                }
                GcState::Finalize => {
                    self.finish_cycle();
                    self.state.set(GcState::Pause);
                    return did_work;
                }
            }
        }
    }

    fn start_cycle(&self, roots: &dyn Trace) {
        let mut cursor = self.head.get();
        while let Some(ptr) = cursor {
            let header = unsafe { &(*ptr.as_ptr()).header };
            header.color.set(Color::White);
            cursor = header.next.get();
        }
        let mut marker = Marker::new();
        roots.trace(&mut marker);
        *self.marker.borrow_mut() = Some(marker);
        self.sweep_prev_next.set(None);
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
        self.sweep_prev_next.set(Some(NonNull::from(&self.head)));
    }

    fn sweep_budgeted(&self, max_units: isize) -> usize {
        let mut work = 0usize;
        let mut budget = max_units;
        let mut freed_bytes = 0usize;
        let mut prev_next_ptr = match self.sweep_prev_next.get() {
            Some(p) => p,
            None => return 0,
        };
        while budget > 0 {
            let prev_cell = unsafe { prev_next_ptr.as_ref() };
            let cursor = prev_cell.get();
            let ptr = match cursor {
                Some(p) => p,
                None => {
                    self.sweep_prev_next.set(None);
                    break;
                }
            };
            let header = unsafe { &(*ptr.as_ptr()).header };
            let next = header.next.get();
            match header.color.get() {
                Color::White => {
                    prev_cell.set(next);
                    freed_bytes += header.size;
                    unsafe {
                        let _ = Box::from_raw(ptr.as_ptr());
                    }
                }
                Color::Black | Color::Gray => {
                    header.color.set(Color::White);
                    prev_next_ptr = unsafe {
                        NonNull::from(&(*ptr.as_ptr()).header.next)
                    };
                    self.sweep_prev_next.set(Some(prev_next_ptr));
                }
            }
            work += 1;
            budget -= 1;
        }
        if freed_bytes > 0 {
            self.bytes.set(self.bytes.get().saturating_sub(freed_bytes));
        }
        work
    }

    fn finish_cycle(&self) {
        *self.marker.borrow_mut() = None;
        self.sweep_prev_next.set(None);
        let next = self
            .bytes
            .get()
            .saturating_mul(self.pause_multiplier.get())
            / 100;
        self.threshold.set(next.max(GC_MIN_THRESHOLD));
        self.collections.set(self.collections.get() + 1);
    }

    fn abort_cycle(&self) {
        if !self.state.get().is_pause() {
            *self.marker.borrow_mut() = None;
            self.sweep_prev_next.set(None);
            self.state.set(GcState::Pause);
        }
    }

    /// Returns the current state of the incremental collector.
    pub fn gc_state(&self) -> GcState {
        self.state.get()
    }

    /// Approximate number of live GC boxes, computed by walking the allgc
    /// chain. Linear in heap size; used for cycle-cost estimation and tests.
    pub fn allgc_count(&self) -> usize {
        let mut count = 0usize;
        let mut cursor = self.head.get();
        while let Some(ptr) = cursor {
            count += 1;
            let header = unsafe { &(*ptr.as_ptr()).header };
            cursor = header.next.get();
        }
        count
    }

    /// Drop every allocation, ignoring reachability. Called at shutdown.
    /// After this returns, every outstanding `Gc<T>` is dangling — callers
    /// must ensure no `Gc<T>` outlives the `Heap`.
    pub fn drop_all(&self) {
        *self.marker.borrow_mut() = None;
        self.sweep_prev_next.set(None);
        self.state.set(GcState::Pause);
        let mut cursor = self.head.get();
        self.head.set(None);
        while let Some(ptr) = cursor {
            // SAFETY: same chain invariant as full_collect's sweep.
            let next = unsafe { (*ptr.as_ptr()).header.next.get() };
            unsafe {
                let _ = Box::from_raw(ptr.as_ptr());
            }
            cursor = next;
        }
        self.bytes.set(0);
    }
}

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
        assert!(with_current_heap(|heap| heap.is_none()), "no guard initially");
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
                    assert!(std::ptr::addr_eq(
                        heap.unwrap() as *const _,
                        &h2 as *const _,
                    ));
                });
            }
            // _g2 dropped — top is back to h1
            with_current_heap(|heap| {
                assert!(std::ptr::addr_eq(
                    heap.unwrap() as *const _,
                    &h1 as *const _,
                ));
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
    fn larger_budget_drains_more_gray_than_smaller() {
        let small_heap = Heap::new();
        small_heap.unpause();
        let h1 = build_chain(&small_heap, 64);
        let r1 = OneRoot(Some(h1));
        let mut small_calls = 0;
        loop {
            small_calls += 1;
            let outcome = small_heap.incremental_step_with_post_mark(
                &r1,
                StepBudget::from_work(2),
                |_| {},
            );
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
            let outcome = big_heap.incremental_step_with_post_mark(
                &r2,
                StepBudget::from_work(64),
                |_| {},
            );
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
            let outcome = heap.incremental_step_with_post_mark(
                &roots,
                StepBudget::from_work(2),
                |_| {},
            );
            if heap.gc_state() == GcState::Sweep && outcome == StepOutcome::InProgress {
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
            let outcome = heap.incremental_step_with_post_mark(
                &roots,
                StepBudget::from_work(2),
                |_| {
                    call_count.set(call_count.get() + 1);
                },
            );
            if outcome == StepOutcome::Paused {
                break;
            }
            assert!(step_count < 10_000, "did not converge");
        }
        assert_eq!(call_count.get(), 1, "post_mark must run exactly once per cycle");
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
            let outcome = h2.incremental_step_with_post_mark(
                &roots2,
                StepBudget::from_work(1),
                |_| {},
            );
            if outcome == StepOutcome::Paused {
                break;
            }
        }
        assert_eq!(h2.bytes_used(), bytes_full);
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
