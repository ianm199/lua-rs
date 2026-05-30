//! Lua value types, error types, and shared newtypes.
//!
//! Phase B foundation: this crate defines the types referenced by all Phase A
//! files. Implementations are stubs (`todo!()`) where they exist; the goal is
//! that `use lua_types::...` imports resolve so `cargo check` can surface real
//! errors instead of name-resolution noise.
//!
//! See `PORT_STRATEGY.md` §3 for the design decisions encoded here.

pub mod arith;
pub mod closure;
pub mod error;
pub mod filehandle;
pub mod gc;
pub mod opcode;
pub mod proto;
pub mod status;
pub mod string;
pub mod table;
pub mod tagmethod;
pub mod trace_impls;
pub mod upval;
pub mod userdata;
pub mod value;
pub mod version;

// ── Top-level re-exports (most consumers use these flat names) ──────────
pub use closure::{LuaClosure, LuaLClosure};
pub use error::{LuaError, LuaExit};
pub use filehandle::LuaFileHandle;
pub use gc::GcRef;
pub use proto::{AbsLineInfo, LocalVar, LuaProto, UpvalDesc};
pub use status::LuaStatus;
pub use string::LuaString;
pub use table::LuaTable;
pub use upval::{UpVal, UpValState};
pub use userdata::LuaUserData;
pub use value::{F2Imod, LuaValue};
pub use version::{LuaVersion, NumberModel};

// ── Top-level newtypes ──────────────────────────────────────────────────

/// Index into the Lua value stack. **Never a pointer or borrow.** Stack
/// reallocates; only indices are stable across mutations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct StackIdx(pub u32);

impl StackIdx {
    pub const ZERO: Self = StackIdx(0);
    #[inline(always)]
    pub fn get(self) -> u32 { self.0 }
    #[inline(always)]
    pub fn as_usize(self) -> usize { self.0 as usize }
}

impl std::ops::Add<i32> for StackIdx {
    type Output = Self;
    #[inline(always)]
    fn add(self, rhs: i32) -> Self { StackIdx((self.0 as i32 + rhs) as u32) }
}
impl std::ops::Sub<i32> for StackIdx {
    type Output = Self;
    #[inline(always)]
    fn sub(self, rhs: i32) -> Self { StackIdx((self.0 as i32 - rhs) as u32) }
}
impl std::ops::Sub<StackIdx> for StackIdx {
    type Output = i32;
    #[inline(always)]
    fn sub(self, rhs: StackIdx) -> i32 { self.0 as i32 - rhs.0 as i32 }
}

/// Index into the call-info stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct CallInfoIdx(pub u32);

impl CallInfoIdx {
    pub const ZERO: Self = CallInfoIdx(0);
    #[inline(always)]
    pub fn get(self) -> u32 { self.0 }
    #[inline(always)]
    pub fn as_usize(self) -> usize { self.0 as usize }
}

/// The base type tag for a Lua value. Matches C-Lua's LUA_T* constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(i8)]
pub enum LuaType {
    None          = -1,
    Nil           = 0,
    Boolean       = 1,
    LightUserData = 2,
    Number        = 3,
    String        = 4,
    Table         = 5,
    Function      = 6,
    UserData      = 7,
    Thread        = 8,
}

impl LuaType {
    pub const NUM_TYPES: usize = 9;
}

// ──────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        (foundation — designed in Phase B, not ported from .c)
//   target_crate:  lua-types
//   confidence:    high
//   todos:         many — submodule impls are stubs
//   port_notes:    0
//   unsafe_blocks: 0
//   notes:         shapes match what Phase A files reference. Method bodies
//                  panic with todo!() / unimplemented!(); compile-fixer
//                  iterations will land real impls.
// ──────────────────────────────────────────────────────────────────────────
