//! Phase-D `Trace` implementations for types defined in this crate.
//!
//! Each impl enumerates the type's GC-bearing fields and either calls
//! `field.trace(m)` (delegating to the field's own `Trace` impl) or
//! `m.mark(field)` (when the field is a `Gc<T>` from `lua-gc`). During the
//! Phase A/B/C/D-0 window `GcRef<T>` is still an `Rc<T>` newtype rather
//! than the real `Gc<T>`, so the mark-queue path is not yet reachable —
//! method resolution dispatches through `Deref` to each underlying type's
//! own `trace` method.

use lua_gc::{Marker, Trace};
use crate::gc::GcRef;
use crate::value::{LuaValue, LuaTable};
use crate::upval::{UpVal, UpValState};
use crate::string::LuaString;
use crate::proto::LuaProto;
use crate::closure::{LuaClosure, LuaLClosure, LuaCClosure};
use crate::userdata::LuaUserData;
use crate::value::LuaThread;

/// Forwarder for `GcRef<T>`. Now that `GcRef` wraps a real `lua_gc::Gc<T>`
/// (D-1e), tracing must enqueue the box onto the gray queue via
/// `Marker::mark` — that is what flips its header color from White to Gray
/// and ultimately to Black during gray-queue drainage. The previous
/// `try_visit` short-circuit was a Phase A-D-0 workaround for the
/// `Rc`-backed handle (no header, no color), and produced a silent bug
/// post-D-1e: every GC-tracked allocation stayed White and was freed in
/// the sweep on the first `collectgarbage()`. Cycles are now handled
/// natively by the heap's gray-queue (Color::Gray check in `mark` makes
/// re-visits idempotent).
impl<T: Trace + 'static> Trace for GcRef<T> {
    fn trace(&self, m: &mut Marker) {
        m.mark(self.0);
    }
}

/// LuaValue — central enum. Variants Nil/Bool/Int/Float/LightUserData carry
/// no GC; Str/Table/Function/UserData/Thread carry collectable payloads.
impl Trace for LuaValue {
    fn trace(&self, m: &mut Marker) {
        match self {
            LuaValue::Nil
            | LuaValue::Bool(_)
            | LuaValue::Int(_)
            | LuaValue::Float(_)
            | LuaValue::LightUserData(_) => {}
            LuaValue::Str(s) => s.trace(m),
            LuaValue::Table(t) => t.trace(m),
            LuaValue::Function(c) => c.trace(m),
            LuaValue::UserData(u) => {
                u.trace(m);
            }
            LuaValue::Thread(_t) => {
                // PORT NOTE: GcRef<LuaThread> is a placeholder unit type in
                // lua-types; the real LuaState lives in lua-vm and is traced
                // through GlobalState::mainthread / state.openupval, not
                // here.
            }
        }
    }
}

/// LuaString — interned byte string. The `Rc<[u8]>` backing buffer is
/// owned, not GC-managed, so this impl is intentionally empty.
impl Trace for LuaString {
    fn trace(&self, _m: &mut Marker) {}
}

/// UpVal — Open (refers to a thread stack slot by index) or Closed (owns a
/// LuaValue). The Open variant carries no direct GC reference; the slot it
/// points at is traced through the owning thread's stack walk.
impl Trace for UpVal {
    fn trace(&self, m: &mut Marker) {
        match &*self.state.borrow() {
            UpValState::Open { .. } => {}
            UpValState::Closed(v) => v.trace(m),
        }
    }
}

/// LuaTable — array+hash entries plus optional metatable.
///
/// Weak-table semantics (Phase D-2):
///   * `__mode = "v"` — values are weak; mark keys only. Dead values get
///     pruned in the post-mark hook via [`crate::value::LuaTable::prune_weak_dead`].
///   * `__mode = "kv"` — both sides weak; trace neither. Both get pruned.
///   * `__mode = "k"` — keys are weak; trace NEITHER initially. The
///     post-mark ephemeron convergence pass walks each weak-key table's
///     entries and marks values only for entries whose keys are
///     independently reachable. Iterating to fixed point makes
///     reachability of value_i transitively depend on key_i — that's the
///     defining property of ephemerons.
///   * No `__mode` — trace both unconditionally.
impl Trace for LuaTable {
    fn trace(&self, m: &mut Marker) {
        const WEAK_KEYS: u8 = 1;
        const WEAK_VALUES: u8 = 1 << 1;
        let mode = self.weak_mode();
        let trace_keys = (mode & WEAK_KEYS) == 0;
        let trace_values = (mode & WEAK_VALUES) == 0 && trace_keys;
        let mut key = LuaValue::Nil;
        while let Some((k, v)) = self.next_pair(&key) {
            if trace_keys {
                k.trace(m);
            }
            if trace_values {
                v.trace(m);
            }
            key = k;
        }
        if let Some(mt) = self.metatable() {
            mt.trace(m);
        }
    }
}

/// LuaProto — bytecode prototype. k (constants), p (child protos),
/// source, upvalue names, locvar names.
impl Trace for LuaProto {
    fn trace(&self, m: &mut Marker) {
        for v in self.k.iter() {
            v.trace(m);
        }
        for p in self.p.iter() {
            p.trace(m);
        }
        if let Some(src) = &self.source {
            src.trace(m);
        }
        for uv in self.upvalues.iter() {
            if let Some(name) = &uv.name {
                name.trace(m);
            }
        }
        for lv in self.locvars.iter() {
            lv.varname.trace(m);
        }
    }
}

/// LuaLClosure — Lua closure carrying a Proto and its captured upvalues.
impl Trace for LuaLClosure {
    fn trace(&self, m: &mut Marker) {
        self.proto.trace(m);
        for uv in self.upvals.iter() {
            uv.borrow().trace(m);
        }
    }
}

/// LuaClosure — dispatch to Lua/C variants; LightC is a bare function-ptr
/// index with no payload.
impl Trace for LuaClosure {
    fn trace(&self, m: &mut Marker) {
        match self {
            LuaClosure::Lua(l) => l.trace(m),
            LuaClosure::C(c) => c.trace(m),
            LuaClosure::LightC(_) => {}
        }
    }
}

/// LuaCClosure — Rust-side C closure carrying captured upvalues.
impl Trace for LuaCClosure {
    fn trace(&self, m: &mut Marker) {
        for v in self.upvalues.iter() {
            v.trace(m);
        }
    }
}

/// LuaUserData — boxed payload + optional metatable + user values.
impl Trace for LuaUserData {
    fn trace(&self, m: &mut Marker) {
        if let Some(mt) = self.metatable() {
            mt.trace(m);
        }
        for v in self.uv.iter() {
            v.trace(m);
        }
    }
}

/// LuaThread — placeholder unit type in lua-types; the real LuaState lives
/// in lua-vm. No GC-bearing fields here.
impl Trace for LuaThread {
    fn trace(&self, _m: &mut Marker) {}
}
