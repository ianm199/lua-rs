//! Phase-D `Trace` implementations for types defined in this crate.
//!
//! Each stub is `todo!("phase-d: trace X")`. Agents fill them in by
//! enumerating the type's GC-bearing fields and calling `m.mark` or
//! `field.trace(m)`.

use lua_gc::{Marker, Trace};
use crate::value::{LuaValue, LuaTable};
use crate::upval::UpVal;
use crate::string::LuaString;
use crate::proto::LuaProto;
use crate::closure::{LuaClosure, LuaLClosure};

/// LuaValue — central enum. Variants Nil/Bool/Int/Float carry no GC.
/// Variants Str/Table/Function/UserData/Thread carry `GcRef<_>` and must
/// be marked.
impl Trace for LuaValue {
    fn trace(&self, _m: &mut Marker) {
        todo!("phase-d: trace LuaValue — match each GC-bearing variant (Str, Table, Function, UserData, Thread) and mark via m.mark or field.trace");
    }
}

/// LuaString — interned byte string. The `Rc<[u8]>` backing is not GC.
impl Trace for LuaString {
    fn trace(&self, _m: &mut Marker) {
        todo!("phase-d: trace LuaString — verify no GC fields; body should be empty (the Rc<[u8]> backing is not GC-managed)");
    }
}

/// UpVal — Open (points into a thread stack) or Closed (owns a LuaValue).
impl Trace for UpVal {
    fn trace(&self, _m: &mut Marker) {
        todo!("phase-d: trace UpVal — Closed(value).trace; Open variant references stack via thread+idx, no direct GC field to mark here");
    }
}

/// LuaTable — array + hash + metatable. Hot path of GC.
impl Trace for LuaTable {
    fn trace(&self, _m: &mut Marker) {
        todo!("phase-d: trace LuaTable — array, hash (k+v), metatable");
    }
}

/// LuaProto — bytecode prototype. k (constants), p (child protos),
/// upvalue names, locvar names, source string.
impl Trace for LuaProto {
    fn trace(&self, _m: &mut Marker) {
        todo!("phase-d: trace LuaProto — k constants, child protos, source, upvalue names, locvar names");
    }
}

/// LuaLClosure — Lua closure (proto + captured upvalues).
impl Trace for LuaLClosure {
    fn trace(&self, _m: &mut Marker) {
        todo!("phase-d: trace LuaLClosure — proto + upvalues vec");
    }
}

/// LuaClosure — enum dispatching to Lua/C/LightC variants.
impl Trace for LuaClosure {
    fn trace(&self, _m: &mut Marker) {
        todo!("phase-d: trace LuaClosure — match Lua/C variants; LightC carries only a usize index");
    }
}
