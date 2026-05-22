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
use crate::value::{LuaValue, LuaTable};
use crate::upval::{UpVal, UpValState};
use crate::string::LuaString;
use crate::proto::LuaProto;
use crate::closure::{LuaClosure, LuaLClosure};

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
                if let Some(mt) = u.metatable() {
                    mt.trace(m);
                }
                for v in u.uv.iter() {
                    v.trace(m);
                }
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

/// LuaTable — array+hash entries plus optional metatable. Weak-key/value
/// pruning is not implemented here; the Phase A/B/C strong_count check at
/// `get` / `next_pair` is the current stand-in for the atomic
/// weak-table pass.
impl Trace for LuaTable {
    fn trace(&self, m: &mut Marker) {
        let mut key = LuaValue::Nil;
        while let Some((k, v)) = self.next_pair(&key) {
            k.trace(m);
            v.trace(m);
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
            uv.trace(m);
        }
    }
}

/// LuaClosure — dispatch to Lua/C variants; LightC is a bare function-ptr
/// index with no payload.
impl Trace for LuaClosure {
    fn trace(&self, m: &mut Marker) {
        match self {
            LuaClosure::Lua(l) => l.trace(m),
            LuaClosure::C(c) => {
                for v in c.upvalues.iter() {
                    v.trace(m);
                }
            }
            LuaClosure::LightC(_) => {}
        }
    }
}
