//! `GcRef<T>` — a reference-counted GC handle. Phase A-C aliases to `Rc<T>`
//! plus a few convenience methods. Phase D's `Gc<T>` lives in `lua-gc`.

use std::rc::Rc;

/// A reference-counted pointer to a Lua collectable object. Wrapper around
/// `Rc<T>` for now; Phase D will give this real GC semantics.
#[derive(Debug)]
pub struct GcRef<T: ?Sized>(pub Rc<T>);

impl<T> GcRef<T> {
    pub fn new(value: T) -> Self { GcRef(Rc::new(value)) }
}

impl<T: ?Sized> GcRef<T> {
    pub fn ptr_eq(a: &Self, b: &Self) -> bool { Rc::ptr_eq(&a.0, &b.0) }
    pub fn identity(&self) -> usize { Rc::as_ptr(&self.0) as *const () as usize }
}

impl<T: ?Sized + lua_gc::Trace> GcRef<T> {
    /// Cycle-aware trace dispatch for the Phase A/B/C/D-0 window.
    ///
    /// `GcRef<T>` is an `Rc<T>` during this window, so calling `trace`
    /// dispatches directly through `Deref` to the underlying value's
    /// own `trace` method — there is no gray queue and no color flag.
    /// Without protection, object graphs containing cycles (such as
    /// `_G._G == _G`) recurse until the OS stack overflows. This helper
    /// records the underlying allocation's identity in `Marker` and
    /// skips the recursive call when the same object is encountered
    /// again in the same cycle. Phase D's real GC subsumes this via
    /// `Color::Gray`; the visited set then becomes redundant.
    pub fn trace_obj(&self, m: &mut lua_gc::Marker) {
        if m.try_visit(self.identity()) {
            (**self).trace(m);
        }
    }
}

impl<T: ?Sized> Clone for GcRef<T> {
    fn clone(&self) -> Self { GcRef(self.0.clone()) }
}

impl<T: ?Sized> std::ops::Deref for GcRef<T> {
    type Target = T;
    fn deref(&self) -> &T { &self.0 }
}

impl<T: ?Sized> AsRef<T> for GcRef<T> {
    fn as_ref(&self) -> &T { &self.0 }
}

impl<T: PartialEq + ?Sized> PartialEq for GcRef<T> {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0) || **self == **other
    }
}
