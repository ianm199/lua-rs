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
