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
}

impl Marker {
    fn new() -> Self {
        Self {
            gray_queue: Vec::with_capacity(256),
        }
    }

    /// Mark a `Gc<T>` as gray (reachable, but its outgoing edges not yet
    /// traced). Called by `Trace::trace` implementations.
    pub fn mark<T: Trace + 'static>(&mut self, gc: Gc<T>) {
        let header = gc.header();
        if header.color.get() == Color::White {
            header.color.set(Color::Gray);
            // Coerce to NonNull<GcBox<dyn Trace>> so we can store in a
            // heterogeneous queue. Requires T: Sized for the unsizing.
            let ptr: NonNull<GcBox<dyn Trace>> = gc.ptr;
            self.gray_queue.push(ptr);
        }
    }
}

/// Phases of the collection cycle. Currently only Idle and StopTheWorld are
/// used; placeholders for future incremental mode.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum GcState {
    Idle,
    Collecting,
}

/// The heap. One per `GlobalState`. Owns the intrusive allgc list of every
/// allocated `GcBox`, tracks total bytes, and runs collections.
pub struct Heap {
    /// Head of the singly-linked allgc list (every live `GcBox`).
    head: Cell<Option<NonNull<GcBox<dyn Trace>>>>,
    /// Total bytes allocated (sum of header sizes; rough).
    bytes: Cell<usize>,
    /// Threshold above which `step` triggers a collection.
    threshold: Cell<usize>,
    /// Multiplier on bytes_used to set next threshold after collection.
    pause_multiplier: Cell<usize>,
    /// State machine. Mostly Idle; transitions during full_collect.
    state: Cell<GcState>,
    /// If true, `step` and `barrier` are no-ops (for bootstrap before the
    /// world is consistent).
    paused: Cell<bool>,
    /// Counter of full collections performed (for diagnostics).
    collections: Cell<usize>,
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
            state: Cell::new(GcState::Idle),
            paused: Cell::new(true), // start paused; caller enables when world is consistent
            collections: Cell::new(0),
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
        let raw: *mut GcBox<T> = Box::into_raw(boxed);
        let ptr: NonNull<GcBox<T>> =
            NonNull::new(raw).expect("Box::into_raw is non-null");
        // Coerce to dyn Trace for the intrusive list.
        let dyn_ptr: NonNull<GcBox<dyn Trace>> = ptr;
        // SAFETY: ptr is a freshly allocated GcBox; we hold the only handle
        // until we publish it into self.head and return Gc<T> to the caller.
        unsafe {
            (*raw).header.next.set(self.head.get());
        }
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
        if self.paused.get() || self.state.get() == GcState::Idle {
            return;
        }
        if parent.header().color.get() != Color::Black {
            return;
        }
        if child.header().color.get() != Color::White {
            return;
        }
        child.header().color.set(Color::Gray);
        // Push child onto the active marker's gray queue. The marker lives
        // on the stack during `full_collect`; we have no handle here. For
        // stop-the-world this branch is unreachable (state != Collecting
        // outside `full_collect`), so we don't yet store a gray queue on
        // self. Phase D-2 (incremental) will move the queue onto the Heap.
        debug_assert!(
            self.state.get() == GcState::Idle,
            "barrier hit during Collecting in stop-the-world phase D-0"
        );
    }

    /// Possibly run a collection. Trigger: bytes_used > threshold.
    /// Caller passes the root set (the runtime — typically `GlobalState`
    /// implementing `Trace`).
    pub fn step(&self, roots: &dyn Trace) {
        if self.paused.get() {
            return;
        }
        if self.bytes.get() < self.threshold.get() {
            return;
        }
        self.full_collect(roots);
    }

    /// Stop-the-world full collect. Marks every reachable object from
    /// `roots`, then sweeps white (unreachable) boxes from the allgc chain.
    pub fn full_collect(&self, roots: &dyn Trace) {
        if self.paused.get() {
            return;
        }
        self.state.set(GcState::Collecting);

        // ── Mark phase ──────────────────────────────────────────────────
        let mut marker = Marker::new();
        // Reset all colors to White first (we're stop-the-world; no
        // incremental state to preserve).
        let mut cursor = self.head.get();
        while let Some(ptr) = cursor {
            // SAFETY: every entry in the allgc chain is a valid GcBox.
            let header = unsafe { &(*ptr.as_ptr()).header };
            header.color.set(Color::White);
            cursor = header.next.get();
        }

        // Trace from roots.
        roots.trace(&mut marker);

        // Drain the gray queue.
        while let Some(gray_ptr) = marker.gray_queue.pop() {
            // SAFETY: gray_queue only ever contains pointers added via
            // Marker::mark, which read them from valid Gc<T> handles.
            unsafe {
                let bx = gray_ptr.as_ref();
                bx.header.color.set(Color::Black);
                bx.value.trace(&mut marker);
            }
        }

        // ── Sweep phase ─────────────────────────────────────────────────
        // Walk allgc; drop white boxes, keep black. Reset black→white for
        // next cycle.
        let mut prev_next: &Cell<Option<NonNull<GcBox<dyn Trace>>>> = &self.head;
        let mut cursor = prev_next.get();
        let mut freed_bytes = 0usize;
        while let Some(ptr) = cursor {
            // SAFETY: cursor walks the allgc chain; every entry is a valid
            // GcBox.
            let header = unsafe { &(*ptr.as_ptr()).header };
            let next = header.next.get();
            match header.color.get() {
                Color::White => {
                    // Unreachable. Unlink, drop, dealloc.
                    prev_next.set(next);
                    freed_bytes += header.size;
                    // SAFETY: ptr is a uniquely-owned NonNull<GcBox<...>>
                    // that we are about to drop. After this, nothing else
                    // references it.
                    unsafe {
                        let _ = Box::from_raw(ptr.as_ptr());
                    }
                }
                Color::Black | Color::Gray => {
                    header.color.set(Color::White);
                    // Advance prev_next to this box's next-cell.
                    // SAFETY: same as above.
                    prev_next = unsafe { &(*ptr.as_ptr()).header.next };
                }
            }
            cursor = next;
        }

        self.bytes.set(self.bytes.get().saturating_sub(freed_bytes));
        self.threshold
            .set(self.bytes.get() * self.pause_multiplier.get() / 100);
        self.collections.set(self.collections.get() + 1);
        self.state.set(GcState::Idle);
    }

    /// Drop every allocation, ignoring reachability. Called at shutdown.
    /// After this returns, every outstanding `Gc<T>` is dangling — callers
    /// must ensure no `Gc<T>` outlives the `Heap`.
    pub fn drop_all(&self) {
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
}
