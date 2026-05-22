//! Phase-D `Trace` implementations for GC-rooted types defined in this
//! crate. Types in `lua-types` (LuaValue, LuaString, UpVal) have their
//! Trace impls in `lua-types/src/trace_impls.rs` because of Rust's orphan
//! rule.
//!
//! Each impl below is a `todo!("phase-d: trace X")` stub. The
//! panic-driven mega-loop surfaces each one when a runtime path triggers
//! `Heap::full_collect`. Each agent works on ONE type — no family
//! expansion (Trace impls have subtle invariants).
//!
//! Implementation guidance for agents:
//!   1. Read the type definition; enumerate every field
//!   2. For every `Gc<T>`, `GcRef<T>`, or container (Vec/Option/HashMap)
//!      thereof, call `m.mark(field)` or `field.trace(m)` appropriately
//!   3. Skip non-GC fields (primitives, `String`, `Vec<u8>`)
//!   4. Skip "intentionally not traced" fields (weak refs)
//!   5. Reference `reference/lua-5.4.7/src/lgc.c`'s `reallymarkobject`

use lua_gc::{Marker, Trace};
use crate::state::{LuaState, GlobalState};

impl Trace for LuaState {
    fn trace(&self, _m: &mut Marker) {
        todo!("phase-d: trace LuaState — stack, openupval, call_stack proto refs; do NOT trace global (held by Rc<RefCell<GlobalState>>)");
    }
}

impl Trace for GlobalState {
    fn trace(&self, _m: &mut Marker) {
        todo!("phase-d: trace GlobalState — l_registry, mainthread, strt pool, mt[..], tmname[..], memerrmsg, twups, fixedgc");
    }
}
