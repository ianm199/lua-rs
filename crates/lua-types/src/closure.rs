//! `LuaClosure` — the function variant of `LuaValue`. Three sub-kinds:
//! Lua closure (compiled Proto + upvalues), C closure (function pointer +
//! upvalues), light C function (function pointer, no upvalues).

use std::cell::Cell;

use crate::gc::GcRef;
use crate::proto::LuaProto;
use crate::upval::UpVal;
use crate::value::LuaValue;

/// Opaque registry index into `GlobalState.c_functions`, where the real
/// `lua_CFunction` (`fn(&mut LuaState) -> Result<usize, LuaError>`) is stored.
/// Lua-types can't reference `LuaState` without a circular dep, so we keep
/// the closure variant type-erased here and resolve through the registry at
/// call time.
pub type LuaCFnPtr = usize;

#[derive(Debug, Clone, Copy)]
pub enum LuaClosure {
    Lua(GcRef<LuaLClosure>),
    C(GcRef<LuaCClosure>),
    LightC(LuaCFnPtr),
}

#[derive(Debug)]
pub struct LuaLClosure {
    pub proto: GcRef<LuaProto>,
    /// Each upvalue slot is held in a `Cell` so that `debug.upvaluejoin`
    /// can replace an entry with another closure's slot without rebuilding
    /// the (shared) closure. `GcRef<UpVal>` is `Copy` (thin wrapper over
    /// `Gc<UpVal>`), so a plain `Cell` is sufficient and skips RefCell
    /// borrow tracking on every upvalue read — critical for the
    /// `upvalue_get` hot path.
    pub upvals: Vec<Cell<GcRef<UpVal>>>,
}

#[derive(Debug)]
pub struct LuaCClosure {
    pub func: LuaCFnPtr,
    pub upvalues: Vec<LuaValue>,
}

impl LuaLClosure {
    pub fn placeholder() -> Self {
        LuaLClosure {
            proto: GcRef::new(LuaProto::placeholder()),
            upvals: Vec::new(),
        }
    }

    /// Returns the upvalue slot at index `i`. Cheap (Copy of a one-pointer
    /// `GcRef<UpVal>`).
    #[inline]
    pub fn upval(&self, i: usize) -> GcRef<UpVal> {
        self.upvals[i].get()
    }

    /// Replaces the upvalue slot at index `i` with `new`. Used by
    /// `debug.upvaluejoin` to share an upvalue between two closures.
    pub fn set_upval(&self, i: usize, new: GcRef<UpVal>) {
        self.upvals[i].set(new);
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        src/lobject.h (CClosure / LClosure / Closure union)
//   target_crate:  lua-types
//   confidence:    high
//   todos:         0
//   port_notes:    0
//   unsafe_blocks: 0
//   notes:         LuaClosure enum covering the C-Lua C/LightC/Lua closure variants.
//                  C uses a union with a common header; we use a tagged enum.
//                  LuaLClosure.upvals uses Cell<GcRef<UpVal>> (not RefCell) so per-
//                  upvalue reads avoid borrow-tracking; GcRef<UpVal> is Copy.
// ──────────────────────────────────────────────────────────────────────────────
