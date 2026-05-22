//! Global State — port of `lstate.c` (445 lines, 25 functions) + `lstate.h` (merged).
//!
//! Manages per-thread ([`LuaState`]) and process-wide ([`GlobalState`]) Lua state:
//! creation, initialization, teardown, and coroutine lifecycle helpers.
//!
//! The `lstate.h` header is merged into this module per PORTING.md §1.
//!
//! # C source files
//! - `reference/lua-5.4.7/src/lstate.c`  (445 lines, 25 functions)
//! - `reference/lua-5.4.7/src/lstate.h`  (408 lines; struct + macro definitions merged)

// C: #define lstate_c
// C: #define LUA_CORE

// PORT NOTE: The C `LX` (thread + extra space) and `LG` (LX + global state) layout
// wrappers are C-only pointer-arithmetic helpers for allocating the main thread and
// GlobalState as one contiguous block. In Rust, `GlobalState` and `LuaState` are
// separate heap-allocated values linked via `Rc<RefCell<GlobalState>>`. No LX/LG
// equivalents are needed.

// PORT NOTE: C macro `fromstate(L)` (cast LX* from lua_State*) is C-only pointer
// arithmetic and is not translated. Rust owns the allocations via Rc/Box.

use std::cell::RefCell;
use std::rc::Rc;

use crate::string::StringPool;
pub use lua_types::error::LuaError;
pub use lua_types::{CallInfoIdx, StackIdx};

/// Internal: a thin wrapper used so stubbed methods can accept either
/// `StackIdx` or `u32` (Phase A code mixes both). Phase B will normalise.
pub struct StackIdxConv(pub StackIdx);

/// Phase-A code casts `StackIdx as i32`; provide a `From` so it compiles.
/// TODO(phase-b): expressions like `state.top_idx().0 as i32` should become
/// `state.top_idx().raw() as i32`. The non-primitive-cast error is silenced
/// here by promoting the StackIdx through a free-function conversion.
#[inline]
pub fn stack_idx_to_i32(i: StackIdx) -> i32 { i.0 as i32 }

impl From<u32> for StackIdxConv {
    fn from(v: u32) -> Self { StackIdxConv(StackIdx(v)) }
}
impl From<i32> for StackIdxConv {
    fn from(v: i32) -> Self { StackIdxConv(StackIdx(v.max(0) as u32)) }
}
impl From<usize> for StackIdxConv {
    fn from(v: usize) -> Self { StackIdxConv(StackIdx(v as u32)) }
}
impl From<StackIdx> for StackIdxConv {
    fn from(v: StackIdx) -> Self { StackIdxConv(v) }
}
pub use lua_types::value::{LuaTable, LuaValue, F2Imod};
pub use lua_types::string::LuaString;
pub use lua_types::userdata::LuaUserData;
pub use lua_types::closure::{LuaClosure, LuaLClosure as LuaClosureLua, LuaCClosure as LuaClosureC};
pub use lua_types::proto::LuaProto;
pub use lua_types::upval::UpVal;
pub use lua_types::gc::GcRef;

/// A Lua-callable function pointer. C: `lua_CFunction`.
///
/// TODO(phase-b): the lua-types crate uses a placeholder
/// `LuaCFnPtr = fn() -> i32` since it can't reference `LuaState` without a
/// circular dep. The real signature is `fn(&mut LuaState) -> Result<usize, LuaError>`,
/// kept here as the lua-vm-facing type alias.
pub type LuaCFunction = fn(&mut LuaState) -> Result<usize, LuaError>;

// ─── Constants (from macros.tsv) ──────────────────────────────────────────────

// C: #define EXTRA_STACK 5  (lstate.h)
// macros.tsv: EXTRA_STACK → const EXTRA_STACK: u32 = 5
pub(crate) const EXTRA_STACK: usize = 5;

// C: LUA_MINSTACK = 20  (lua.h)
// macros.tsv: LUA_MINSTACK → const LUA_MINSTACK: u32 = 20
pub(crate) const LUA_MINSTACK: usize = 20;

// C: #define BASIC_STACK_SIZE (2 * LUA_MINSTACK)  (lstate.h)
// macros.tsv: BASIC_STACK_SIZE → const BASIC_STACK_SIZE: u32 = 2 * LUA_MINSTACK
pub(crate) const BASIC_STACK_SIZE: usize = 2 * LUA_MINSTACK;

// C: LUAI_MAXCCALLS = 200  (luaconf.h)
pub(crate) const LUAI_MAXCCALLS: u32 = 200;

// C: #define CIST_C (1 << 1)  (lstate.h)
// macros.tsv: CIST_C → const CIST_C: u16 = 1 << 1
pub(crate) const CIST_C: u16 = 1 << 1;

// Remaining CIST_* bits from macros.tsv
pub(crate) const CIST_OAH: u16 = 1 << 0;
pub(crate) const CIST_FRESH: u16 = 1 << 2;
pub(crate) const CIST_HOOKED: u16 = 1 << 3;
pub(crate) const CIST_YPCALL: u16 = 1 << 4;
pub(crate) const CIST_TAIL: u16 = 1 << 5;
pub(crate) const CIST_HOOKYIELD: u16 = 1 << 6;
pub(crate) const CIST_FIN: u16 = 1 << 7;
pub(crate) const CIST_TRAN: u16 = 1 << 8;
pub(crate) const CIST_CLSRET: u16 = 1 << 9;
pub(crate) const CIST_RECST: u32 = 10;

// C: LUA_RIDX_MAINTHREAD = 1, LUA_RIDX_GLOBALS = 2  (lua.h)
// macros.tsv: LUA_RIDX_MAINTHREAD → const LUA_RIDX_MAINTHREAD: i64 = 1
pub(crate) const LUA_RIDX_MAINTHREAD: i64 = 1;
pub(crate) const LUA_RIDX_GLOBALS: i64 = 2;
// C: LUA_RIDX_LAST = LUA_RIDX_GLOBALS = 2
pub(crate) const LUA_RIDX_LAST: usize = 2;

// C: LUA_NUMTAGS = 9  (lua.h)
// macros.tsv: LUA_NUMTYPES → const LUA_NUMTYPES: usize = 9
const LUA_NUMTYPES: usize = 9;

// C: LUA_EXTRASPACE  (lua.h) — sizeof(void *) on most platforms
const LUA_EXTRASPACE: usize = std::mem::size_of::<*mut ()>();

// C: GCSTPGC — GC stopped for state building (lgc.h constant)
// TODO(port): import from crate::gc (lgc.c → gc.rs) once it exists in Phase D
const GCSTPGC: u8 = 1;

// C: GCSpause (lgc.h) — initial GC state
// TODO(port): import from crate::gc in Phase D
const GCS_PAUSE: u8 = 0;

// C: LUAI_GCPAUSE, LUAI_GCMUL, LUAI_GCSTEPSIZE, LUAI_GENMAJORMUL, LUAI_GENMINORMUL (luaconf.h)
const LUAI_GCPAUSE: u32 = 200;
const LUAI_GCMUL: u32 = 100;
const LUAI_GCSTEPSIZE: u8 = 13;
const LUAI_GENMAJORMUL: u32 = 100;
const LUAI_GENMINORMUL: u8 = 20;

// C: WHITE0BIT = 0  (lgc.h)
const WHITE0BIT: u8 = 0;

// C: STRCACHE_N, STRCACHE_M  (llimits.h)
const STRCACHE_N: usize = 53;
const STRCACHE_M: usize = 2;

// ─── GcKind enum ─────────────────────────────────────────────────────────────

/// Garbage collector operating mode.
///
/// C: `KGC_INC` / `KGC_GEN` constants in `lstate.h`.
/// macros.tsv: `KGC_INC → GcKind::Incremental`, `KGC_GEN → GcKind::Generational`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcKind {
    // C: KGC_INC = 0
    Incremental = 0,
    // C: KGC_GEN = 1
    Generational = 1,
}

// ─── LuaStatus enum ──────────────────────────────────────────────────────────

/// Thread / call status codes.
///
/// C: `LUA_OK`, `LUA_YIELD`, `LUA_ERRRUN`, … constants in `lua.h`.
pub use lua_types::status::LuaStatus;

// ─── StackValue ───────────────────────────────────────────────────────────────

/// One slot on the Lua value stack.  Wraps a `LuaValue` and an optional
/// to-be-closed delta (for the `tbclist` mechanism).
///
/// C: `StackValue` in `lobject.h`.
/// types.tsv: `StackValue → StackValue { val: LuaValue, tbclist.delta: u16 }`
#[derive(Clone)]
pub struct StackValue {
    // C: StackValue.val — the payload TValue
    pub val: LuaValue,
    // C: StackValue.tbclist.delta — to-be-closed linked-list delta
    pub tbc_delta: u16,
}

impl Default for StackValue {
    fn default() -> Self {
        StackValue {
            val: LuaValue::Nil,
            tbc_delta: 0,
        }
    }
}

// ─── CallInfo ────────────────────────────────────────────────────────────────

/// Saved state for a Lua or C call frame.
///
/// C: `struct CallInfo` in `lstate.h`.
/// types.tsv: CallInfo → CallInfo (several fields renamed / adapted).
///
/// The C intrusive doubly-linked list (`previous`, `next` as raw pointers) is
/// replaced by `Option<CallInfoIdx>` indices into `LuaState::call_info`.
#[derive(Clone)]
pub struct CallInfo {
    // C: StkIdRel func — stack index of the called function value
    // types.tsv: CallInfo.func → StackIdx
    pub func: StackIdx,

    // C: StkIdRel top — stack-top reservation for this call
    // types.tsv: CallInfo.top → StackIdx
    pub top: StackIdx,

    // C: struct CallInfo *previous
    // types.tsv: CallInfo.previous → CallInfoIdx (Option at boundary)
    pub previous: Option<CallInfoIdx>,

    // C: struct CallInfo *next
    // types.tsv: CallInfo.next → CallInfoIdx (Option at tail)
    pub next: Option<CallInfoIdx>,

    // C: union { struct { savedpc, trap, nextraargs } l; struct { k, old_errfunc, ctx } c; } u
    pub u: CallInfoFrame,

    // C: union { funcidx, nyield, nres, transferinfo } u2
    pub u2: CallInfoExtra,

    // C: short nresults
    // types.tsv: CallInfo.nresults → i16
    pub nresults: i16,

    // C: unsigned short callstatus
    // types.tsv: CallInfo.callstatus → u16 (bit-packed CIST_* flags)
    pub callstatus: u16,
}

/// Payload of `CallInfo.u`.
///
/// C: `union { struct l { savedpc, trap, nextraargs }; struct c { k, old_errfunc, ctx } } u`
#[derive(Clone, Copy)]
pub enum CallInfoFrame {
    // C: ci->u.l — Lua function call
    Lua {
        // C: const Instruction *savedpc → u32 offset into Proto.code
        // types.tsv: CallInfo.u.l.savedpc → u32
        savedpc: u32,
        // C: volatile l_signalT trap
        // types.tsv: CallInfo.u.l.trap → bool
        trap: bool,
        // C: int nextraargs
        // types.tsv: CallInfo.u.l.nextraargs → i32
        nextraargs: i32,
    },
    // C: ci->u.c — C function call
    C {
        // C: lua_KFunction k — continuation for yields
        // types.tsv: CallInfo.u.c.k → Option<lua_KFunction>
        k: Option<LuaKFunction>,
        // C: ptrdiff_t old_errfunc
        // types.tsv: CallInfo.u.c.old_errfunc → isize
        old_errfunc: isize,
        // C: lua_KContext ctx
        // types.tsv: CallInfo.u.c.ctx → isize
        ctx: isize,
    },
}

/// Continuation function for yieldable C calls.  C: `lua_KFunction`.
pub type LuaKFunction = fn(&mut LuaState, status: i32, ctx: isize) -> Result<usize, LuaError>;

/// Payload of `CallInfo.u2`.
///
/// C: `union { funcidx, nyield, nres, transferinfo } u2`
/// types.tsv: CallInfo.u2 → CallInfoExtra (Rust: struct with all fields, interpretation by context)
#[derive(Default, Clone, Copy)]
pub struct CallInfoExtra {
    // C: int funcidx / nyield / nres — overloaded single int field
    pub value: i32,
    // C: struct transferinfo { unsigned short ftransfer, ntransfer }
    pub ftransfer: u16,
    pub ntransfer: u16,
}

impl CallInfoFrame {
    /// Default C-call frame (no continuation, zero context).
    pub fn c_default() -> Self {
        CallInfoFrame::C {
            k: None,
            old_errfunc: 0,
            ctx: 0,
        }
    }

    /// Default Lua-call frame (pc=0, no trap, no extra args).
    pub fn lua_default() -> Self {
        CallInfoFrame::Lua {
            savedpc: 0,
            trap: false,
            nextraargs: 0,
        }
    }
}

impl Default for CallInfo {
    fn default() -> Self {
        CallInfo {
            func: StackIdx(0),
            top: StackIdx(0),
            previous: None,
            next: None,
            u: CallInfoFrame::c_default(),
            u2: CallInfoExtra::default(),
            nresults: 0,
            callstatus: 0,
        }
    }
}

impl CallInfo {
    pub fn is_lua(&self) -> bool { matches!(self.u, CallInfoFrame::Lua { .. }) }
    pub fn is_lua_code(&self) -> bool { self.is_lua() }
    pub fn is_vararg_func(&self) -> bool { todo!("phase-b: CallInfo::is_vararg_func") }
    pub fn saved_pc(&self) -> u32 {
        if let CallInfoFrame::Lua { savedpc, .. } = self.u { savedpc } else { 0 }
    }
    pub fn set_saved_pc(&mut self, pc: u32) {
        if let CallInfoFrame::Lua { ref mut savedpc, .. } = self.u { *savedpc = pc; }
    }
    pub fn nextra_args(&self) -> i32 {
        if let CallInfoFrame::Lua { nextraargs, .. } = self.u { nextraargs } else { 0 }
    }
    pub fn transfer_ftransfer(&self) -> u16 { self.u2.ftransfer }
    pub fn transfer_ntransfer(&self) -> u16 { self.u2.ntransfer }
    pub fn set_trap(&mut self, t: bool) {
        if let CallInfoFrame::Lua { ref mut trap, .. } = self.u { *trap = t; }
    }
    pub fn set_recover_status<T>(&mut self, _status: T) { todo!("phase-b: CallInfo::set_recover_status") }
    pub fn recover_status(&self) -> i32 { todo!("phase-b: CallInfo::recover_status") }
    pub fn get_oah(&self) -> bool { (self.callstatus & CIST_OAH) != 0 }
    pub fn u_c_old_errfunc(&self) -> isize {
        if let CallInfoFrame::C { old_errfunc, .. } = self.u { old_errfunc } else { 0 }
    }
    pub fn u_c_ctx(&self) -> isize {
        if let CallInfoFrame::C { ctx, .. } = self.u { ctx } else { 0 }
    }
    pub fn u_c_k(&self) -> Option<LuaKFunction> {
        if let CallInfoFrame::C { k, .. } = self.u { k } else { None }
    }
}

// ─── Phase-B value/proto/instruction helpers ──────────────────────────────────

/// Extension methods on `LuaValue`. TODO(phase-b): move these to
/// `lua_types::value` (or wherever the canonical impl lives) once the type
/// helpers stabilise.
pub trait LuaValueExt {
    fn base_type(&self) -> lua_types::LuaType;
    fn to_number_no_strconv(&self) -> Option<f64>;
    fn to_number_with_strconv(&self) -> Option<f64>;
    fn to_integer_no_strconv(&self) -> Option<i64>;
    fn to_integer_with_strconv(&self) -> Option<i64>;
    fn full_type_tag(&self) -> u8;
}

impl LuaValueExt for LuaValue {
    fn base_type(&self) -> lua_types::LuaType { self.type_tag() }
    fn to_number_no_strconv(&self) -> Option<f64> {
        match self {
            LuaValue::Float(f) => Some(*f),
            LuaValue::Int(i) => Some(*i as f64),
            _ => None,
        }
    }
    fn to_number_with_strconv(&self) -> Option<f64> {
        if let Some(n) = self.to_number_no_strconv() { return Some(n); }
        todo!("phase-b: LuaValue::to_number_with_strconv (string coercion)")
    }
    fn to_integer_no_strconv(&self) -> Option<i64> {
        match self {
            LuaValue::Int(i) => Some(*i),
            LuaValue::Float(f) if f.fract() == 0.0 && f.is_finite() => Some(*f as i64),
            _ => None,
        }
    }
    fn to_integer_with_strconv(&self) -> Option<i64> {
        if let Some(i) = self.to_integer_no_strconv() { return Some(i); }
        todo!("phase-b: LuaValue::to_integer_with_strconv (string coercion)")
    }
    fn full_type_tag(&self) -> u8 { self.type_tag() as u8 }
}

/// Extension methods on `lua_types::LuaType`.
pub trait LuaTypeExt {
    fn type_name(&self) -> &'static [u8];
}

impl LuaTypeExt for lua_types::LuaType {
    fn type_name(&self) -> &'static [u8] {
        use lua_types::LuaType::*;
        match self {
            None => b"no value",
            Nil => b"nil",
            Boolean => b"boolean",
            LightUserData => b"userdata",
            Number => b"number",
            String => b"string",
            Table => b"table",
            Function => b"function",
            UserData => b"userdata",
            Thread => b"thread",
        }
    }
}

/// StackIdx checked-arithmetic helpers. Returns the raw `u32` because Phase A
/// callers use the result in arithmetic comparisons against other `u32`
/// quantities (stack-distance offsets).
pub trait StackIdxExt {
    fn saturating_sub(self, n: impl Into<StackIdxConv>) -> u32;
    fn wrapping_sub(self, n: impl Into<StackIdxConv>) -> u32;
    fn raw(self) -> u32;
}
impl StackIdxExt for StackIdx {
    fn saturating_sub(self, n: impl Into<StackIdxConv>) -> u32 { self.0.saturating_sub(n.into().0.0) }
    fn wrapping_sub(self, n: impl Into<StackIdxConv>) -> u32 { self.0.wrapping_sub(n.into().0.0) }
    fn raw(self) -> u32 { self.0 }
}

/// `GcRef<LuaTable>` / `GcRef<LuaUserData>` field-access helpers. These
/// methods are needed by api.rs and tagmethods.rs but the lua-types
/// placeholders don't yet expose them. TODO(phase-b): replace with real
/// accessor methods on the canonical types in lua-types.
pub trait LuaTableRefExt {
    fn metatable(&self) -> Option<GcRef<LuaTable>>;
    fn as_ptr(&self) -> *const ();
    fn get(&self, _k: &LuaValue) -> LuaValue;
    fn get_int(&self, _k: i64) -> LuaValue;
    fn get_short_str(&self, _k: &GcRef<LuaString>) -> LuaValue;
    fn raw_set(&self, _state: &mut LuaState, _k: &LuaValue, _v: LuaValue) -> Result<(), LuaError>;
    fn raw_set_int(&self, _state: &mut LuaState, _k: i64, _v: LuaValue) -> Result<(), LuaError>;
    fn invalidate_tm_cache(&self);
    fn resize(&self, _state: &mut LuaState, _na: usize, _nh: usize) -> Result<(), LuaError>;
    fn next(&self, _k: LuaValue) -> Result<Option<(LuaValue, LuaValue)>, LuaError>;
}
impl LuaTableRefExt for GcRef<LuaTable> {
    fn metatable(&self) -> Option<GcRef<LuaTable>> {
        // PORT NOTE: `LuaValue::Table` carries `GcRef<lua_types::value::LuaTable>`
        // (the placeholder with no storage). The rich `lua_vm::table::LuaTable`
        // with a metatable field is a separate type whose wire-up is a later
        // reconcile-step task. C: `luaH_new` initializes Table.metatable to
        // NULL and no `setmetatable` path currently reaches placeholder
        // tables, so `None` is the faithful answer.
        // TODO(port): re-implement once `LuaTable` placeholder reconciliation
        // (type-vocabulary.tsv: LuaTable audit→enforce) is complete.
        None
    }
    fn as_ptr(&self) -> *const () { GcRef::identity(self) as *const () }
    fn get(&self, _k: &LuaValue) -> LuaValue {
        // PORT NOTE: `LuaValue::Table` carries `GcRef<lua_types::value::LuaTable>`
        // (the placeholder with no storage). The rich `lua_vm::table::LuaTable`
        // implementing `get_slot` / `slot_value` is a separate type whose
        // wire-up to `LuaValue` is a later reconcile-step task. Until then
        // every lookup is empty, so a faithful C: `luaH_get` returns Nil here.
        // TODO(port): re-implement once `LuaTable` placeholder reconciliation
        // (type-vocabulary.tsv: LuaTable audit→enforce) is complete.
        LuaValue::Nil
    }
    fn get_int(&self, _k: i64) -> LuaValue { todo!("phase-b: LuaTable::get_int") }
    fn get_short_str(&self, _k: &GcRef<LuaString>) -> LuaValue { todo!("phase-b: LuaTable::get_short_str") }
    fn raw_set(&self, _state: &mut LuaState, _k: &LuaValue, _v: LuaValue) -> Result<(), LuaError> {
        // PORT NOTE: `LuaValue::Table` carries `GcRef<lua_types::value::LuaTable>`
        // (the placeholder with no storage). The rich `lua_vm::table::LuaTable`
        // implementing the hybrid array+hash and `luaH_set` / `luaH_finishset`
        // is a separate type whose wire-up to `LuaValue` is a later
        // reconcile-step task. Until then every store is a no-op, matching
        // the symmetric `get` no-op above so Phase B's `pcall` of stdlib
        // bootstrap code doesn't panic — C: `luaH_set` would write into the
        // slot returned by `luaH_get`, but there is no slot here.
        // TODO(port): re-implement once `LuaTable` placeholder reconciliation
        // (type-vocabulary.tsv: LuaTable audit→enforce) is complete.
        Ok(())
    }
    fn raw_set_int(&self, _state: &mut LuaState, _k: i64, _v: LuaValue) -> Result<(), LuaError> {
        // PORT NOTE: same rationale as `raw_set` above — placeholder has no
        // storage, so C: `luaH_setint` becomes a no-op until reconciliation.
        // TODO(port): re-implement once `LuaTable` placeholder reconciliation
        // (type-vocabulary.tsv: LuaTable audit→enforce) is complete.
        Ok(())
    }
    fn invalidate_tm_cache(&self) { /* phase-b no-op */ }
    fn resize(&self, _state: &mut LuaState, _na: usize, _nh: usize) -> Result<(), LuaError> { todo!("phase-b: LuaTable::resize") }
    fn next(&self, _k: LuaValue) -> Result<Option<(LuaValue, LuaValue)>, LuaError> { todo!("phase-b: LuaTable::next") }
}

pub trait LuaUserDataRefExt {
    fn metatable(&self) -> Option<GcRef<LuaTable>>;
    fn as_ptr(&self) -> *const ();
    fn len(&self) -> usize;
}
impl LuaUserDataRefExt for GcRef<LuaUserData> {
    fn metatable(&self) -> Option<GcRef<LuaTable>> { self.0.metatable.clone() }
    fn as_ptr(&self) -> *const () { GcRef::identity(self) as *const () }
    fn len(&self) -> usize { self.0.data.len() }
}

pub trait LuaStringRefExt {
    fn is_white(&self) -> bool;
    fn hash(&self) -> u32;
    fn as_gc_ref(&self) -> GcRef<LuaString>;
}
impl LuaStringRefExt for GcRef<LuaString> {
    fn is_white(&self) -> bool { false }
    fn hash(&self) -> u32 { self.0.hash() }
    fn as_gc_ref(&self) -> GcRef<LuaString> { self.clone() }
}

pub trait LuaLClosureRefExt {
    fn proto(&self) -> &GcRef<LuaProto>;
    fn nupvalues(&self) -> usize;
}
impl LuaLClosureRefExt for GcRef<lua_types::closure::LuaLClosure> {
    fn proto(&self) -> &GcRef<LuaProto> { &self.0.proto }
    fn nupvalues(&self) -> usize { self.0.upvals.len() }
}

/// `LuaClosure` accessor — `nupvalues()` reports the upvalue count uniformly.
pub trait LuaClosureExt {
    fn nupvalues(&self) -> usize;
}
impl LuaClosureExt for LuaClosure {
    fn nupvalues(&self) -> usize {
        match self {
            LuaClosure::Lua(l) => l.0.upvals.len(),
            LuaClosure::C(c) => c.0.upvalues.len(),
            LuaClosure::LightC(_) => 0,
        }
    }
}

/// `LuaProto` source bytes accessor.
pub trait LuaProtoExt {
    fn source_bytes(&self) -> &[u8];
    fn source_string(&self) -> Option<&GcRef<LuaString>>;
}
impl LuaProtoExt for LuaProto {
    fn source_bytes(&self) -> &[u8] {
        match &self.source { Some(s) => s.0.as_bytes(), None => &[] }
    }
    fn source_string(&self) -> Option<&GcRef<LuaString>> { self.source.as_ref() }
}

// ─── Collectable trait (GC interface) ────────────────────────────────────────

/// Marker trait for GC-managed objects.
///
/// C: `GCObject` in `lobject.h` / `lstate.h`. Phase A–C: objects are Rc-tracked;
/// Phase D: real tracing GC.
/// types.tsv: `GCObject → (trait Collectable; concrete = GcRef<T>)`
pub trait Collectable: std::fmt::Debug {}

impl std::fmt::Debug for LuaState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LuaState")
    }
}
impl Collectable for LuaState {}

// ─── GlobalState ─────────────────────────────────────────────────────────────

/// Function-pointer signature for the text-source parser, installed on
/// [`GlobalState::parser_hook`] by the embedder.
///
/// The implementation lives in `lua-parse`; `lua-vm` cannot depend on it
/// directly (that would form a cycle), so the parser is reached via this
/// function pointer registered at startup.
pub type ParserHook = fn(
    state: &mut LuaState,
    source: &[u8],
    name: &[u8],
    firstchar: i32,
) -> Result<GcRef<lua_types::closure::LuaLClosure>, LuaError>;

/// Process-wide state shared by all Lua threads.
///
/// C: `global_State` in `lstate.h`.
/// types.tsv: `global_State → GlobalState`
///
/// Not exposed directly at the API; accessed via `state.global()` / `state.global_mut()`.
pub struct GlobalState {
    /// Phase-B hook for the Lua text parser. Set by the embedder (`lua-cli`
    /// or stdlib host) to bridge the cyclic crate split between `lua-vm` and
    /// `lua-parse`: when `f_parser` decides the chunk is text, it invokes
    /// this hook instead of the parser stub. `None` leaves the stub in place
    /// so unit tests that never load text still work.
    pub parser_hook: Option<ParserHook>,

    // C: l_mem totalbytes — Phase D memory accounting
    // types.tsv: global_State.totalbytes → isize
    pub totalbytes: isize,

    // C: l_mem GCdebt — Phase D GC pacing
    // types.tsv: global_State.GCdebt → isize
    pub gc_debt: isize,

    // C: lu_mem GCestimate — Phase D
    pub gc_estimate: usize,

    // C: lu_mem lastatomic — Phase D
    // types.tsv: global_State.lastatomic → usize
    pub lastatomic: usize,

    // C: stringtable strt — intern table for short strings
    // types.tsv: global_State.strt → StringPool
    pub strt: StringPool,

    // C: TValue l_registry — the Lua registry (always a table once state is complete)
    // types.tsv: global_State.l_registry → LuaValue
    pub l_registry: LuaValue,

    // PORT NOTE (phase-b-reconcile): The lua-types LuaTable placeholder has
    // no storage, so we cannot persist `registry[LUA_RIDX_GLOBALS] = globals`
    // via the canonical registry path. Until the placeholder reconciles with
    // lua-vm::table::LuaTable, the globals table lives in a direct field
    // and `get_global_table` reads it from here. Same for `loaded` (the
    // module cache normally at `registry[_LOADED]`).
    pub globals: LuaValue,
    pub loaded: LuaValue,

    // C: TValue nilvalue — nil sentinel; non-nil signals state not yet fully built
    // types.tsv: global_State.nilvalue → LuaValue
    // PORT NOTE: In Rust we use a dedicated `is_complete: bool` flag rather than
    // the C trick of checking `ttisnil(&g->nilvalue)`. See `is_complete()`.
    pub nilvalue: LuaValue,

    // C: unsigned int seed — randomized seed for hashes
    // types.tsv: global_State.seed → u32
    pub seed: u32,

    // C: lu_byte currentwhite — Phase D GC color
    // types.tsv: global_State.currentwhite → u8
    pub currentwhite: u8,

    // C: lu_byte gcstate — Phase D GC FSM state
    pub gcstate: u8,

    // C: lu_byte gckind — Phase D KGC_INC vs KGC_GEN
    pub gckind: u8,

    // C: lu_byte gcstopem — Phase D
    pub gcstopem: bool,

    // C: lu_byte genminormul — Phase D generational tuning
    // types.tsv: global_State.genminormul → u8
    pub genminormul: u8,

    // C: lu_byte genmajormul — Phase D
    pub genmajormul: u8,

    // C: lu_byte gcstp — controls GC running (GCSTPGC etc.)
    pub gcstp: u8,

    // C: lu_byte gcemergency — Phase D emergency collection flag
    pub gcemergency: bool,

    // C: lu_byte gcpause — pause size between GCs (/4 stored)
    // types.tsv: global_State.gcpause → u8
    pub gcpause: u8,

    // C: lu_byte gcstepmul — GC speed (/4 stored)
    // types.tsv: global_State.gcstepmul → u8
    pub gcstepmul: u8,

    // C: lu_byte gcstepsize — log2 of GC granularity
    pub gcstepsize: u8,

    // C: GCObject *allgc — all collectable objects; Phase A–C leaks (Rc cycles)
    // types.tsv: global_State.allgc → Vec<GcRef<dyn Collectable>>
    pub allgc: Vec<GcRef<dyn Collectable>>,

    // C: GCObject **sweepgc — sweep cursor into allgc; Phase D
    // types.tsv: global_State.sweepgc → (removed); replaced by usize cursor
    pub sweepgc_cursor: usize,

    // C: GCObject *finobj — finalizable objects; Phase D
    // types.tsv: global_State.finobj → Vec<GcRef<dyn Collectable>>
    pub finobj: Vec<GcRef<dyn Collectable>>,

    // C: GCObject *gray — GC gray list; Phase D
    // types.tsv: global_State.gray → Vec<GcRef<dyn Collectable>>
    pub gray: Vec<GcRef<dyn Collectable>>,

    // C: GCObject *grayagain — Phase D
    pub grayagain: Vec<GcRef<dyn Collectable>>,

    // C: GCObject *weak — weak-value tables; Phase D
    // types.tsv: global_State.weak → Vec<GcRef<LuaTable>>
    pub weak: Vec<GcRef<LuaTable>>,

    // C: GCObject *ephemeron — ephemeron tables; Phase D
    pub ephemeron: Vec<GcRef<LuaTable>>,

    // C: GCObject *allweak — all-weak tables; Phase D
    pub allweak: Vec<GcRef<LuaTable>>,

    // C: GCObject *tobefnz — pending finalizers; Phase D
    // types.tsv: global_State.tobefnz → Vec<GcRef<dyn Collectable>>
    pub tobefnz: Vec<GcRef<dyn Collectable>>,

    // C: GCObject *fixedgc — non-collectable objects (reserved-word strings, etc.)
    // types.tsv: global_State.fixedgc → Vec<GcRef<dyn Collectable>>
    pub fixedgc: Vec<GcRef<dyn Collectable>>,

    // Generational cohort markers — Phase D only
    // types.tsv: global_State.survival/old1/reallyold/firstold1/finobjsur/finobjold1/finobjrold
    //   → (removed; replaced by index cursors in Phase D)

    // C: struct lua_State *twups — threads with open upvalues
    // types.tsv: global_State.twups → Vec<GcRef<LuaState>>
    pub twups: Vec<GcRef<LuaState>>,

    // C: lua_CFunction panic — panic handler; Phase B
    // types.tsv: global_State.panic → Option<lua_CFunction>
    pub panic: Option<LuaCFunction>,

    // C: struct lua_State *mainthread
    // types.tsv: global_State.mainthread → GcRef<LuaState>
    // TODO(port): self-referential Rc cycle; Phase D GC handles cycles properly
    pub mainthread: Option<GcRef<LuaState>>,

    // C: TString *memerrmsg — preallocated OOM error message
    // types.tsv: global_State.memerrmsg → GcRef<LuaString>
    pub memerrmsg: GcRef<LuaString>,

    // C: TString *tmname[TM_N] — tag-method names indexed by TMS enum
    // types.tsv: global_State.tmname → [GcRef<LuaString>; TM_N]
    // TODO(port): TM_N constant and TagMethod enum come from ltm.c → tagmethods.rs
    pub tmname: Vec<GcRef<LuaString>>,

    // C: struct Table *mt[LUA_NUMTYPES] — per-type metatables
    // types.tsv: global_State.mt → [Option<GcRef<LuaTable>>; LUA_NUMTYPES]
    pub mt: [Option<GcRef<LuaTable>>; LUA_NUMTYPES],

    // C: TString *strcache[STRCACHE_N][STRCACHE_M] — string cache for luaS_new
    // types.tsv: global_State.strcache → [[GcRef<LuaString>; STRCACHE_M]; STRCACHE_N]
    pub strcache: [[GcRef<LuaString>; STRCACHE_M]; STRCACHE_N],

    // C: lua_WarnFunction warnf — warning function sink
    // types.tsv: global_State.warnf → Option<Box<dyn FnMut(&[u8], bool)>>
    pub warnf: Option<Box<dyn FnMut(&[u8], bool)>>,
    // C: void *ud_warn — folded into the `warnf` closure capture; removed
}

impl GlobalState {
    /// Total live bytes allocated (GCdebt + totalbytes).
    ///
    /// C: `gettotalbytes(g)` macro → `cast(lu_mem, (g)->totalbytes + (g)->GCdebt)`
    /// macros.tsv: `gettotalbytes → g.total_bytes()`
    pub fn total_bytes(&self) -> usize {
        // C: cast(lu_mem, (g)->totalbytes + (g)->GCdebt)
        (self.totalbytes + self.gc_debt) as usize
    }

    /// Returns `true` when the state has been fully initialized.
    ///
    /// C: `completestate(g)` macro → `ttisnil(&g->nilvalue)`
    /// macros.tsv: `completestate → g.is_complete()`
    ///
    /// PORT NOTE: C uses `g->nilvalue` being nil as the "complete" signal.
    /// We replicate the same logic: `nilvalue == Nil` means complete.
    pub fn is_complete(&self) -> bool {
        // C: ttisnil(&g->nilvalue)
        matches!(self.nilvalue, LuaValue::Nil)
    }

    /// Returns the "current white" GC color bitmask.
    ///
    /// C: `luaC_white(g)` macro.
    /// macros.tsv: `luaC_white → g.current_white()`
    ///
    /// PORT NOTE: GC color management deferred to Phase D; always returns
    /// the initial white bit.
    pub fn current_white(&self) -> u8 {
        self.currentwhite
    }

    /// Returns the "other white" GC color bitmask.
    ///
    /// C: `otherwhite(g)` macro.
    /// macros.tsv: `otherwhite → g.other_white()`
    pub fn other_white(&self) -> u8 {
        // TODO(port): Phase D — toggle white bit properly
        self.currentwhite ^ 0x03
    }

    /// Returns `true` if the GC is in generational mode.
    ///
    /// C: `isdecGCmodegen(g)` macro.
    /// macros.tsv: `isdecGCmodegen → g.is_gen_mode()`
    pub fn is_gen_mode(&self) -> bool {
        self.gckind == GcKind::Generational as u8
    }

    /// Returns `true` if the GC is currently running.
    ///
    /// C: `gcrunning(g)` macro.
    /// macros.tsv: `gcrunning → g.gc_running()`
    pub fn gc_running(&self) -> bool {
        self.gcstp == 0
    }

    /// Returns `true` while the GC is in its propagation phase.
    ///
    /// C: `keepinvariant(g)` macro.
    /// macros.tsv: `keepinvariant → g.keep_invariant()`
    pub fn keep_invariant(&self) -> bool {
        // TODO(port): Phase D — check gcstate for propagation phases
        false
    }

    /// Returns `true` while the GC is in a sweep phase.
    ///
    /// C: `issweepphase(g)` macro.
    /// macros.tsv: `issweepphase → g.is_sweep_phase()`
    pub fn is_sweep_phase(&self) -> bool {
        // TODO(port): Phase D — check gcstate for sweep states (GCSswpallgc etc.)
        false
    }

    // ── Phase-B stubs ─────────────────────────────────────────────────────────
    pub fn gc_debt(&self) -> isize { self.gc_debt }
    pub fn set_gc_debt(&mut self, d: isize) { self.gc_debt = d; }
    pub fn gc_at_pause(&self) -> bool { self.gcstate == 0 }
    pub fn gc_pause_param(&self) -> u8 { self.gcpause }
    pub fn set_gc_pause_param(&mut self, p: u8) { self.gcpause = p; }
    pub fn gc_stepmul_param(&self) -> u8 { self.gcstepmul }
    pub fn set_gc_stepmul_param(&mut self, p: u8) { self.gcstepmul = p; }
    pub fn set_gc_genmajormul(&mut self, p: u8) { self.genmajormul = p; }
    pub fn gc_stop_flags(&self) -> u8 { self.gcstp }
    pub fn set_gc_stop_flags(&mut self, f: u8) { self.gcstp = f; }
    pub fn set_gc_stop_user(&mut self) { todo!("phase-b: set_gc_stop_user") }
    pub fn clear_gc_stop(&mut self) { self.gcstp = 0; }
    pub fn is_gc_stopped_internally(&self) -> bool { self.gcstp != 0 }
    pub fn tm_name<T>(&self, _tm: T) -> Option<GcRef<LuaString>> { todo!("phase-b: tm_name") }
}

use lua_types::tagmethod::TagMethod;

// ─── LuaState ────────────────────────────────────────────────────────────────

/// Per-thread Lua execution state.
///
/// C: `struct lua_State` in `lstate.h`.
/// types.tsv: `lua_State → LuaState`
///
/// All stack-pointer fields in C (`StkIdRel`, `StkId`) become `StackIdx` (u32
/// index into `stack: Vec<StackValue>`).  The C intrusive `CallInfo` linked list
/// becomes `call_info: Vec<CallInfo>` indexed by `CallInfoIdx`.
pub struct LuaState {
    // ── Thread status ──

    // C: lu_byte status — thread status (LUA_OK / LUA_YIELD / LUA_ERR*)
    // types.tsv: lua_State.status → u8
    pub status: u8,

    // C: lu_byte allowhook — hook-enabled flag
    // types.tsv: lua_State.allowhook → bool
    pub allowhook: bool,

    // C: unsigned short nci — number of CallInfo entries in use
    // types.tsv: lua_State.nci → u32
    pub nci: u32,

    // ── Stack ──

    // C: StkIdRel top — first free stack slot
    // types.tsv: lua_State.top → StackIdx
    pub top: StackIdx,

    // C: StkIdRel stack_last — end-of-stack sentinel (stack.p + BASIC_STACK_SIZE)
    // types.tsv: lua_State.stack_last → StackIdx (redundant once Vec; kept for parity)
    pub stack_last: StackIdx,

    // C: StkIdRel stack — the stack base pointer; in Rust this is the Vec itself
    // types.tsv: lua_State.stack → Vec<StackValue>
    pub stack: Vec<StackValue>,

    // ── Call info ──

    // C: CallInfo *ci — current call frame; raw pointer in C
    // types.tsv: lua_State.ci → CallInfoIdx
    pub ci: CallInfoIdx,

    // C: CallInfo base_ci — bottom CallInfo (C→Lua entry); element 0 of the Vec
    // types.tsv: lua_State.base_ci → CallInfo  (Vec element 0)
    // PORT NOTE: In Rust, base_ci is call_info[0]. There is no separate field.
    pub call_info: Vec<CallInfo>,

    // ── Upvalues / to-be-closed ──

    // C: UpVal *openupval — open upvalue list (was intrusive; now a Vec)
    // types.tsv: lua_State.openupval → Vec<GcRef<UpVal>>
    pub openupval: Vec<GcRef<UpVal>>,

    // C: StkIdRel tbclist — to-be-closed list (was StkIdRel pointer; now Vec of idx)
    // types.tsv: lua_State.tbclist → Vec<StackIdx>
    pub tbclist: Vec<StackIdx>,

    // ── Global state ──

    // C: global_State *l_G — pointer to shared GlobalState
    // types.tsv: lua_State.l_G → (accessed via method)
    // PORT NOTE: Rc<RefCell<>> for shared ownership across coroutine threads.
    pub(crate) global: Rc<RefCell<GlobalState>>,

    // ── Hooks ──

    // C: volatile lua_Hook hook
    // types.tsv: lua_State.hook → Option<Box<dyn FnMut(&mut LuaState, &LuaDebug)>>
    // TODO(port): LuaDebug defined in ldebug.c → debug.rs (Phase E)
    pub hook: Option<Box<dyn Fn()>>,

    // C: volatile l_signalT hookmask
    // types.tsv: lua_State.hookmask → u8
    pub hookmask: u8,

    // C: int basehookcount
    // types.tsv: lua_State.basehookcount → i32
    pub basehookcount: i32,

    // C: int hookcount
    // types.tsv: lua_State.hookcount → i32
    pub hookcount: i32,

    // ── Error handling ──

    // C: struct lua_longjmp *errorJmp — C longjmp recovery point
    // types.tsv: lua_State.errorJmp → (removed; replaced by Result<T, LuaError>)
    // PORT NOTE: Entirely removed. The `?` operator replaces setjmp/longjmp.

    // C: ptrdiff_t errfunc — error-handler stack position (0 = none)
    // types.tsv: lua_State.errfunc → isize
    pub errfunc: isize,

    // ── C-call depth ──

    // C: l_uint32 nCcalls — packed (recursion_count | non_yieldable_count << 16)
    // types.tsv: lua_State.nCcalls → u32
    pub nCcalls: u32,

    // ── Debug / hooks ──

    // C: int oldpc — last pc traced (for hooks)
    // types.tsv: lua_State.oldpc → u32
    pub oldpc: u32,

    // ── GC color (Phase D) ──

    // C: lu_byte marked — GC color/age bits; Phase D only
    // types.tsv: GCObject.marked → u8
    pub marked: u8,
}

impl LuaState {
    /// Access the process-wide `GlobalState` immutably.
    ///
    /// C: `G(L)` macro → `state.global()`.
    /// macros.tsv: `G → state.global()`
    ///
    /// PORT NOTE: Returns `std::cell::Ref<GlobalState>` because GlobalState is held in
    /// `Rc<RefCell<...>>`. Call sites that do `state.global().field` should work fine
    /// via `Deref`. Callers must not hold the `Ref` across a `global_mut()` call.
    pub fn global(&self) -> std::cell::Ref<'_, GlobalState> {
        self.global.borrow()
    }

    /// Access the process-wide `GlobalState` mutably.
    ///
    /// C: `G(L)` + indirect write → `state.global_mut()`.
    /// macros.tsv: `G → state.global()` (writes use `state.global_mut()`)
    pub fn global_mut(&self) -> std::cell::RefMut<'_, GlobalState> {
        self.global.borrow_mut()
    }

    /// Clone the `Rc` handle to the GlobalState for sharing with a new coroutine.
    ///
    /// Used in `new_thread` to give the child thread access to the same GlobalState.
    pub fn global_rc(&self) -> Rc<RefCell<GlobalState>> {
        Rc::clone(&self.global)
    }

    /// Return the current C-call recursion depth (lower 16 bits of `nCcalls`).
    ///
    /// C: `getCcalls(L)` macro → `(L)->nCcalls & 0xffff`
    /// macros.tsv: `getCcalls → state.c_calls()`
    pub fn c_calls(&self) -> u32 {
        self.nCcalls & 0xffff
    }

    /// Increment the non-yieldable call count (upper 16 bits of `nCcalls`).
    ///
    /// C: `incnny(L)` macro → `(L)->nCcalls += 0x10000`
    /// macros.tsv: `incnny → state.inc_nny()`
    pub fn inc_nny(&mut self) {
        self.nCcalls += 0x10000;
    }

    /// Decrement the non-yieldable call count.
    ///
    /// C: `decnny(L)` macro → `(L)->nCcalls -= 0x10000`
    /// macros.tsv: `decnny → state.dec_nny()`
    pub fn dec_nny(&mut self) {
        self.nCcalls -= 0x10000;
    }

    /// Returns `true` if the thread can yield (no non-yieldable frames on the stack).
    ///
    /// C: `yieldable(L)` macro → `((L)->nCcalls & 0xffff0000) == 0`
    /// macros.tsv: `yieldable → state.is_yieldable()`
    pub fn is_yieldable(&self) -> bool {
        (self.nCcalls & 0xffff0000) == 0
    }

    /// Reset the hook countdown to the baseline.
    ///
    /// C: `resethookcount(L)` macro → `L->hookcount = L->basehookcount`
    /// macros.tsv: `resethookcount → state.reset_hook_count()`
    pub fn reset_hook_count(&mut self) {
        self.hookcount = self.basehookcount;
    }

    /// Returns the current stack capacity (slots between base and stack_last).
    ///
    /// C: `stacksize(th)` macro → `cast_int((th)->stack_last.p - (th)->stack.p)`
    /// macros.tsv: `stacksize → state.stack_size()`
    pub fn stack_size(&self) -> usize {
        self.stack_last.0 as usize
    }

    /// Push a value onto the stack, incrementing `top`.
    ///
    /// C: `*L->top++ = val` (various push patterns)
    /// macros.tsv: `api_incr_top → gone — state.push() already increments`
    pub fn push(&mut self, val: LuaValue) {
        let top = self.top.0 as usize;
        if top < self.stack.len() {
            self.stack[top] = StackValue { val, tbc_delta: 0 };
        } else {
            self.stack.push(StackValue { val, tbc_delta: 0 });
        }
        self.top = StackIdx(self.top.0 + 1);
    }

    /// Pop the top value from the stack, decrementing `top`.
    ///
    /// C: `L->top--` + dereference.
    pub fn pop(&mut self) -> LuaValue {
        if self.top.0 == 0 {
            return LuaValue::Nil;
        }
        self.top = StackIdx(self.top.0 - 1);
        self.stack[self.top.0 as usize].val.clone()
    }

    /// Retrieve the value at the given stack index without removing it.
    ///
    /// C: `s2v(L->stack.p + idx)` / stack slot access.
    /// macros.tsv: `s2v → state.stack_at(idx)` → returns `&LuaValue`
    pub fn stack_val(&self, idx: StackIdx) -> &LuaValue {
        &self.stack[idx.0 as usize].val
    }

    /// Write a value to a specific stack slot.
    pub fn set_stack_val(&mut self, idx: StackIdx, val: LuaValue) {
        self.stack[idx.0 as usize].val = val;
    }

    /// Returns a no-op GC handle.
    ///
    /// C: Various `luaC_*` calls → `state.gc().*`
    /// macros.tsv: `luaC_checkGC → state.gc().check_step()`, etc.
    ///
    /// PORT NOTE: In Phases A–C the GC is `Rc`-based and all GC operations are
    /// no-ops. Phase D replaces this with real GC logic in `lua-gc`.
    pub fn gc(&mut self) -> GcHandle<'_> {
        GcHandle { _state: self }
    }

    /// Create a new empty table and register it with the GC.
    ///
    /// C: `luaH_new(L)` → `state.new_table()` returning `GcRef<LuaTable>`
    /// macros.tsv: `lua_newtable → state.new_table()`
    pub fn new_table(&mut self) -> GcRef<LuaTable> {
        // TODO(port): register with GC tracking (state.global_mut().allgc) in Phase D
        GcRef::new(LuaTable::placeholder())
    }

    /// Intern a byte string in the global string pool.
    ///
    /// C: `luaS_new(L, s)` → `state.intern_str(s: &[u8])`
    /// macros.tsv: `luaS_new → state.intern_str(s)`
    pub fn intern_str(&mut self, bytes: &[u8]) -> Result<GcRef<LuaString>, LuaError> {
        let local = crate::string::new(self, bytes)?;
        Ok(GcRef::new(LuaString::from_bytes(local.as_bytes().to_vec())))
    }

    /// Returns the current CallInfo index (the active call frame).
    pub fn top_idx(&self) -> StackIdx {
        self.top
    }
}

// ─── Phase-B stub methods ─────────────────────────────────────────────────────
//
// The methods in the impl blocks below were referenced by api.rs, debug.rs,
// do_.rs, vm.rs, tagmethods.rs etc. during Phase A. Each body is a `todo!()`
// pinned to a phase-b task; once the corresponding C function is faithfully
// ported the stub will be replaced. Signatures are inferred from call sites
// and should be treated as Phase-B-grade approximations.

impl LuaState {
    pub fn get_at(&self, idx: impl Into<StackIdxConv>) -> LuaValue {
        let i: StackIdx = idx.into().0;
        match self.stack.get(i.0 as usize) {
            Some(slot) => slot.val.clone(),
            None => LuaValue::Nil,
        }
    }
    pub fn set_at(&mut self, idx: impl Into<StackIdxConv>, v: LuaValue) {
        let i: StackIdx = idx.into().0;
        self.stack[i.0 as usize].val = v;
    }
    /// Set `top` to an absolute stack index, nil-filling any new slots.
    ///
    /// C: portion of `lua_settop` (lapi.c) — `L->top.p = newtop` plus the
    /// `for (; diff > 0; diff--) setnilvalue(s2v(L->top.p++))` clear loop.
    /// PORT NOTE: callers pass an absolute `StackIdx` (e.g. `top + 1`,
    /// `ci_top`, `ra + nres`), not the relative `idx` of the public
    /// `lua_settop` API. The to-be-closed (`tbclist`) close path is Phase E
    /// and not handled here.
    pub fn set_top(&mut self, idx: impl Into<StackIdxConv>) {
        let new_top: StackIdx = idx.into().0;
        let new_top_u = new_top.0 as usize;
        let cur = self.top.0 as usize;
        if new_top_u > cur {
            let existing_end = self.stack.len().min(new_top_u);
            for i in cur..existing_end {
                self.stack[i] = StackValue::default();
            }
            if new_top_u > self.stack.len() {
                self.stack.resize_with(new_top_u, StackValue::default);
            }
        }
        self.top = new_top;
    }
    /// Primitive "set top index" — just writes `self.top`, no nil-fill.
    ///
    /// C: tail of `lua_settop` (lapi.c) — `L->top.p = newtop;`
    /// PORT NOTE: callers (`api.rs::set_top`, `raw_set`, etc.) pre-nil-fill or
    /// only shrink, so this routine intentionally does no clearing or resizing.
    /// The to-be-closed (`tbclist`) close path is Phase E.
    pub fn set_top_idx(&mut self, idx: impl Into<StackIdxConv>) {
        let new_top: StackIdx = idx.into().0;
        self.top = new_top;
    }
    pub fn dec_top(&mut self) { todo!("phase-b: dec_top") }
    pub fn pop_n(&mut self, n: usize) {
        let cur = self.top.0 as usize;
        let new = cur.saturating_sub(n);
        self.top = StackIdx(new as u32);
    }
    pub fn peek_at(&mut self, _idx: impl Into<StackIdxConv>) -> LuaValue { todo!("phase-b: peek_at") }
    pub fn peek_top(&mut self) -> LuaValue { todo!("phase-b: peek_top") }
    pub fn peek_string_at_top(&mut self) -> GcRef<LuaString> { todo!("phase-b: peek_string_at_top") }
    pub fn stack_at(&mut self, _idx: impl Into<StackIdxConv>) -> &mut LuaValue { todo!("phase-b: stack_at") }
    pub fn stack_set_nil(&mut self, _idx: impl Into<StackIdxConv>) { todo!("phase-b: stack_set_nil") }
    pub fn stack_resize(&mut self, _size: usize) -> Result<(), LuaError> { todo!("phase-b: stack_resize") }
    pub fn stack_available(&mut self) -> usize { todo!("phase-b: stack_available") }
    pub fn check_stack(&mut self, n: i32) -> Result<(), LuaError> {
        debug_assert!(n >= 0, "negative 'n'");
        let free = (self.stack_last.0 as i32) - (self.top.0 as i32);
        if free <= n {
            self.grow_stack(n, true)?;
        }
        Ok(())
    }
    pub fn grow_stack(&mut self, _n: i32, _raiseerror: bool) -> Result<(), LuaError> { todo!("phase-b: grow_stack") }

    pub fn get_ci(&self, idx: CallInfoIdx) -> &CallInfo { &self.call_info[idx.as_usize()] }
    pub fn get_ci_mut(&mut self, idx: CallInfoIdx) -> &mut CallInfo { &mut self.call_info[idx.as_usize()] }
    pub fn current_call_info(&self) -> &CallInfo { &self.call_info[self.ci.as_usize()] }
    pub fn current_call_info_mut(&mut self) -> &mut CallInfo { let i = self.ci.as_usize(); &mut self.call_info[i] }
    pub fn current_ci_idx(&self) -> CallInfoIdx { self.ci }
    pub fn call_stack_mut(&mut self) -> &mut Vec<CallInfo> { &mut self.call_info }
    pub fn next_ci(&mut self) -> Result<CallInfoIdx, LuaError> {
        match self.call_info[self.ci.as_usize()].next {
            Some(idx) => Ok(idx),
            None => Ok(extend_ci(self)),
        }
    }
    pub fn prev_ci(&self, idx: CallInfoIdx) -> Option<CallInfoIdx> { self.call_info[idx.as_usize()].previous }
    pub fn get_prev_ci(&self, _idx: CallInfoIdx) -> Option<&CallInfo> { todo!("phase-b: get_prev_ci") }
    pub fn is_base_ci(&self, idx: CallInfoIdx) -> bool { idx.as_usize() == 0 }
    pub fn is_current_ci(&self, idx: CallInfoIdx) -> bool { idx == self.ci }
    pub fn ci_next_func(&self, _idx: CallInfoIdx) -> StackIdx { todo!("phase-b: ci_next_func") }
    pub fn ci_top(&self, idx: CallInfoIdx) -> StackIdx { self.call_info[idx.as_usize()].top }
    pub fn ci_trap(&mut self, _idx: CallInfoIdx) -> bool { todo!("phase-b: ci_trap") }
    pub fn ci_savedpc(&self, _idx: CallInfoIdx) -> u32 { todo!("phase-b: ci_savedpc") }
    pub fn set_ci_savedpc(&mut self, _idx: CallInfoIdx, _pc: u32) { todo!("phase-b: set_ci_savedpc") }
    pub fn set_ci_previous(&mut self, _idx: CallInfoIdx) { todo!("phase-b: set_ci_previous") }
    pub fn ci_previous(&self, idx: CallInfoIdx) -> Option<CallInfoIdx> { self.call_info[idx.as_usize()].previous }
    pub fn ci_adjust_func<D>(&mut self, _idx: CallInfoIdx, _delta: D) { todo!("phase-b: ci_adjust_func") }
    pub fn ci_base(&self, _idx: CallInfoIdx) -> StackIdx { todo!("phase-b: ci_base") }
    pub fn ci_is_fresh(&self, _idx: CallInfoIdx) -> bool { todo!("phase-b: ci_is_fresh") }
    pub fn ci_lua_closure(&self, _idx: CallInfoIdx) -> Option<GcRef<lua_types::closure::LuaLClosure>> { todo!("phase-b: ci_lua_closure") }
    pub fn ci_nextraargs(&self, _idx: CallInfoIdx) -> i32 { todo!("phase-b: ci_nextraargs") }
    pub fn ci_nres(&self, _idx: CallInfoIdx) -> i32 { todo!("phase-b: ci_nres") }
    pub fn ci_nres_set(&mut self, _idx: CallInfoIdx, _n: i32) { todo!("phase-b: ci_nres_set") }
    pub fn ci_nresults(&self, _idx: CallInfoIdx) -> i32 { todo!("phase-b: ci_nresults") }
    pub fn ci_prev_instruction(&self, _idx: CallInfoIdx) -> lua_types::opcode::Instruction { todo!("phase-b: ci_prev_instruction") }
    pub fn ci_prev2_instruction(&self, _idx: CallInfoIdx) -> lua_types::opcode::Instruction { todo!("phase-b: ci_prev2_instruction") }
    pub fn ci_skip_next_instruction(&mut self, _idx: CallInfoIdx) { todo!("phase-b: ci_skip_next_instruction") }
    pub fn ci_step_pc_back(&mut self, _idx: CallInfoIdx) { todo!("phase-b: ci_step_pc_back") }
    pub fn get_ci_pcrel(&mut self, _idx: CallInfoIdx) -> u32 { todo!("phase-b: get_ci_pcrel") }
    pub fn get_ci_u2_funcidx(&mut self, _idx: CallInfoIdx) -> i32 { todo!("phase-b: get_ci_u2_funcidx") }
    pub fn get_ci_u2_nres(&mut self, _idx: CallInfoIdx) -> i32 { todo!("phase-b: get_ci_u2_nres") }
    pub fn get_ci_u2_nyield(&mut self, _idx: CallInfoIdx) -> i32 { todo!("phase-b: get_ci_u2_nyield") }
    pub fn get_ci_vararg_info(&mut self, _idx: CallInfoIdx) -> (bool, i32, i32) { todo!("phase-b: get_ci_vararg_info") }
    pub fn get_ci_lua_proto_numparams(&mut self, _idx: CallInfoIdx) -> u8 { todo!("phase-b: get_ci_lua_proto_numparams") }
    pub fn set_ci_u2_nres(&mut self, _idx: CallInfoIdx, _n: i32) { todo!("phase-b: set_ci_u2_nres") }
    pub fn set_ci_u2_nyield(&mut self, _idx: CallInfoIdx, _n: i32) { todo!("phase-b: set_ci_u2_nyield") }
    pub fn set_ci_transfer_info(&mut self, _idx: CallInfoIdx, _ftransfer: u16, _ntransfer: u16) { todo!("phase-b: set_ci_transfer_info") }
    pub fn shrink_ci(&mut self) { shrink_ci(self) }
    pub fn check_c_stack(&mut self) -> Result<(), LuaError> { check_c_stack(self) }

    pub fn status(&mut self) -> LuaStatus { LuaStatus::from_raw(self.status as i32) }
    pub fn errfunc(&mut self) -> isize { self.errfunc }
    pub fn old_pc(&mut self) -> u32 { self.oldpc }
    pub fn set_old_pc(&mut self, pc: u32) { self.oldpc = pc; }
    pub fn set_oldpc(&mut self, pc: u32) { self.oldpc = pc; }
    pub fn _hook_call_noargs(&mut self) { todo!("phase-b: hook_call_noargs") }
    pub fn hook(&self) -> Option<&dyn Fn()> { todo!("phase-b: hook") }
    pub fn has_hook(&mut self) -> bool { self.hook.is_some() }
    pub fn hook_count(&mut self) -> i32 { self.hookcount }
    pub fn set_hook_count(&mut self, n: i32) { self.hookcount = n; }
    pub fn hook_mask(&self) -> u8 { self.hookmask }
    pub fn set_hook_mask(&mut self, m: u8) { self.hookmask = m; }
    pub fn base_hook_count(&self) -> i32 { self.basehookcount }
    pub fn set_base_hook_count(&mut self, n: i32) { self.basehookcount = n; }
    pub fn set_hook<T>(&mut self, _h: T) { todo!("phase-b: set_hook") }
    pub fn call_hook_event(&mut self, _event: i32, _line: i32) -> Result<(), LuaError> { todo!("phase-b: call_hook_event") }

    pub fn registry_value(&self) -> LuaValue { self.global().l_registry.clone() }
    pub fn registry_get(&self, key: usize) -> LuaValue {
        let reg = self.global().l_registry.clone();
        match reg {
            LuaValue::Table(t) => t.get(&LuaValue::Int(key as i64)),
            _ => LuaValue::Nil,
        }
    }

    pub fn new_string(&mut self, bytes: &[u8]) -> Result<GcRef<LuaString>, LuaError> { self.intern_str(bytes) }
    pub fn intern_or_create_str(&mut self, bytes: &[u8]) -> Result<GcRef<LuaString>, LuaError> { self.intern_str(bytes) }
    pub fn new_userdata(&mut self, _size: usize, _nuvalue: usize) -> Result<GcRef<LuaUserData>, LuaError> { todo!("phase-b: new_userdata") }
    pub fn new_c_closure(&mut self, _f: LuaCFunction, _n: i32) -> Result<LuaClosure, LuaError> { todo!("phase-b: new_c_closure") }
    pub fn push_closure<A, B, C, D>(&mut self, _a: A, _b: B, _c: C, _d: D) -> Result<(), LuaError> { todo!("phase-b: push_closure") }
    pub fn new_tbc_upval(&mut self, _idx: StackIdx) -> Result<(), LuaError> { todo!("phase-b: new_tbc_upval") }

    pub fn upvalue_get<F, N>(&mut self, _funcindex: F, _n: N) -> LuaValue { todo!("phase-b: upvalue_get") }
    pub fn upvalue_set<F, N>(&mut self, _funcindex: F, _n: N, _val: LuaValue) -> Result<(), LuaError> { todo!("phase-b: upvalue_set") }

    pub fn protected_call_raw(&mut self, _func: StackIdx, _nresults: i32, _errfunc: StackIdx) -> Result<(), LuaError> { todo!("phase-b: protected_call_raw") }
    pub fn protected_parser(&mut self, z: crate::zio::ZIO, name: &[u8], mode: Option<&[u8]>) -> LuaStatus {
        crate::do_::protected_parser(self, z, name, mode)
    }
    pub fn do_call(&mut self, _func: StackIdx, _nresults: i32) -> Result<(), LuaError> { todo!("phase-b: do_call") }
    pub fn do_call_no_yield(&mut self, _func: StackIdx, _nresults: i32) -> Result<(), LuaError> { todo!("phase-b: do_call_no_yield") }
    pub fn call_no_yield(&mut self, func: StackIdx, nresults: i32) -> Result<(), LuaError> {
        crate::do_::callnoyield(self, func, nresults)
    }
    pub fn call_at(&mut self, _func: StackIdx, _nresults: i32) -> Result<(), LuaError> { todo!("phase-b: call_at") }
    pub fn precall(&mut self, _func: StackIdx, _nresults: i32) -> Result<Option<CallInfoIdx>, LuaError> { todo!("phase-b: precall") }
    pub fn pretailcall<C, F, T, D>(&mut self, _ci: C, _func: F, _top: T, _delta: D) -> Result<i32, LuaError> { todo!("phase-b: pretailcall") }
    pub fn poscall<N>(&mut self, _ci: CallInfoIdx, _nres: N) -> Result<(), LuaError> { todo!("phase-b: poscall") }
    pub fn adjust_results(&mut self, nresults: i32) {
        const LUA_MULTRET: i32 = -1;
        if nresults <= LUA_MULTRET {
            let ci_idx = self.ci.as_usize();
            if self.call_info[ci_idx].top.0 < self.top.0 {
                self.call_info[ci_idx].top = self.top;
            }
        }
    }
    pub fn adjust_varargs<C, N, CL>(&mut self, _ci: C, _nfixparams: N, _cl: &CL) -> Result<(), LuaError> { todo!("phase-b: adjust_varargs") }
    pub fn get_varargs<C, R, N>(&mut self, _ci: C, _ra: R, _n: N) -> Result<i32, LuaError> { todo!("phase-b: get_varargs") }

    pub fn close_upvals<L>(&mut self, _level: L) -> Result<(), LuaError> { todo!("phase-b: close_upvals") }
    pub fn close_upvals_status<L, S>(&mut self, _level: L, _status: S) -> Result<(), LuaError> { todo!("phase-b: close_upvals_status") }
    pub fn close_upvals_from_base<B>(&mut self, _base: B) -> Result<(), LuaError> { todo!("phase-b: close_upvals_from_base") }

    pub fn arith_op(&mut self, _op: i32, _p1: &LuaValue, _p2: &LuaValue) -> Result<LuaValue, LuaError> { todo!("phase-b: arith_op") }
    pub fn concat(&mut self, _n: i32) -> Result<(), LuaError> { todo!("phase-b: concat") }
    pub fn less_than(&mut self, _l: &LuaValue, _r: &LuaValue) -> Result<bool, LuaError> { todo!("phase-b: less_than") }
    pub fn less_equal(&mut self, _l: &LuaValue, _r: &LuaValue) -> Result<bool, LuaError> { todo!("phase-b: less_equal") }
    pub fn equal_obj(&self, _ctx: Option<&LuaValue>, _l: &LuaValue, _r: &LuaValue) -> bool { todo!("phase-b: equal_obj") }
    pub fn equal_obj_with_tm(&mut self, _l: &LuaValue, _r: &LuaValue) -> Result<bool, LuaError> { todo!("phase-b: equal_obj_with_tm") }
    pub fn obj_len(&mut self, _v: &LuaValue) -> Result<LuaValue, LuaError> { todo!("phase-b: obj_len") }
    pub fn obj_to_string(&mut self, _idx_or_val: i32) -> Result<GcRef<LuaString>, LuaError> { todo!("phase-b: obj_to_string") }
    pub fn coerce_to_string(&mut self, _idx: StackIdx) -> Result<GcRef<LuaString>, LuaError> { todo!("phase-b: coerce_to_string") }
    pub fn str_to_num(&mut self, _s: &[u8]) -> Option<(LuaValue, usize)> { todo!("phase-b: str_to_num") }

    pub fn fast_get(&mut self, t: &LuaValue, k: &LuaValue) -> Result<Option<LuaValue>, LuaError> {
        let LuaValue::Table(tbl) = t else { return Ok(None); };
        let v = tbl.get(k);
        if matches!(v, LuaValue::Nil) { Ok(None) } else { Ok(Some(v)) }
    }
    pub fn fast_get_int<T>(&mut self, _t: T, _k: i64) -> Result<Option<LuaValue>, LuaError> { todo!("phase-b: fast_get_int") }
    pub fn fast_get_short_str<T, K>(&mut self, _t: T, _k: K) -> Result<Option<LuaValue>, LuaError> { todo!("phase-b: fast_get_short_str") }
    pub fn fast_tm_table(&mut self, t: Option<&GcRef<LuaTable>>, tm: TagMethod) -> LuaValue {
        let Some(mt) = t else { return LuaValue::Nil; };
        debug_assert!((tm as u8) <= TagMethod::Eq as u8);
        let ename = self.global().tmname[tm as usize].clone();
        mt.get_short_str(&ename)
    }
    pub fn fast_tm_ud<U, T>(&mut self, _u: U, _tm: T) -> LuaValue { todo!("phase-b: fast_tm_ud") }

    pub fn table_get_with_tm(&mut self, t: &LuaValue, k: &LuaValue) -> Result<LuaValue, LuaError> {
        if let Some(v) = self.fast_get(t, k)? {
            return Ok(v);
        }
        let res = self.top_idx();
        self.push(LuaValue::Nil);
        crate::vm::finish_get(self, t.clone(), k.clone(), res, true)?;
        let value = self.get_at(res);
        self.pop();
        Ok(value)
    }
    pub fn table_set_with_tm(&mut self, t: &LuaValue, k: LuaValue, v: LuaValue) -> Result<(), LuaError> {
        if self.fast_get(t, &k)?.is_some() {
            self.table_raw_set(t, k, v.clone())?;
            self.gc_barrier_back(t, &v);
            return Ok(());
        }
        crate::vm::finish_set(self, t.clone(), k, v, true)
    }
    pub fn table_raw_set(&mut self, t: &LuaValue, k: LuaValue, v: LuaValue) -> Result<(), LuaError> {
        let LuaValue::Table(tbl) = t else {
            return Err(LuaError::type_error(t, "index"));
        };
        let tbl = tbl.clone();
        tbl.raw_set(self, &k, v)
    }
    pub fn table_array_set<T>(&mut self, _t: T, _idx: usize, _v: LuaValue) -> Result<(), LuaError> { todo!("phase-b: table_array_set") }
    pub fn table_ensure_array<T>(&mut self, _t: T, _n: usize) -> Result<(), LuaError> { todo!("phase-b: table_ensure_array") }
    pub fn table_length<T>(&mut self, _t: T) -> Result<i64, LuaError> { todo!("phase-b: table_length") }
    pub fn table_metatable(&mut self, v: &LuaValue) -> Option<GcRef<LuaTable>> {
        match v {
            LuaValue::Table(t) => t.metatable(),
            LuaValue::UserData(u) => u.metatable(),
            other => {
                let idx = other.base_type() as usize;
                self.global().mt[idx].clone()
            }
        }
    }
    pub fn table_resize(&mut self, _t: &GcRef<LuaTable>, _na: usize, _nh: usize) -> Result<(), LuaError> { todo!("phase-b: table_resize") }
    pub fn table_getn(&self, _t: &GcRef<LuaTable>) -> i64 { todo!("phase-b: table_getn") }

    pub fn try_bin_tm<T, U, V, W>(&mut self, _p1: T, _p2: U, _res: V, _tm: W) -> Result<(), LuaError> { todo!("phase-b: try_bin_tm") }
    pub fn try_bin_i_tm<T, U, V, W, X>(&mut self, _p1: T, _imm: U, _flip: V, _res: W, _tm: X) -> Result<(), LuaError> { todo!("phase-b: try_bin_i_tm") }
    pub fn try_bin_assoc_tm<T, U, V, W, X>(&mut self, _p1: T, _p2: U, _flip: V, _res: W, _tm: X) -> Result<(), LuaError> { todo!("phase-b: try_bin_assoc_tm") }
    pub fn try_concat_tm<T, U>(&mut self, _p1: T, _p2: U) -> Result<Option<LuaValue>, LuaError> { todo!("phase-b: try_concat_tm") }
    pub fn call_tm<T, U, V, W>(&mut self, _f: T, _p1: U, _p2: V, _result: W) -> Result<(), LuaError> { todo!("phase-b: call_tm") }
    pub fn call_tm_res<T, U, V, W>(&mut self, _f: T, _p1: U, _p2: V, _result: W) -> Result<LuaValue, LuaError> { todo!("phase-b: call_tm_res") }
    pub fn call_tm_res_bool<T, U, V>(&mut self, _f: T, _p1: U, _p2: V) -> Result<bool, LuaError> { todo!("phase-b: call_tm_res_bool") }
    pub fn call_order_tm<T, U, V>(&mut self, _p1: T, _p2: U, _tm: V) -> Result<bool, LuaError> { todo!("phase-b: call_order_tm") }
    pub fn call_order_i_tm<T, U, V, W, X>(&mut self, _p1: T, _p2: U, _inv: V, _isf: W, _tm: X) -> Result<bool, LuaError> { todo!("phase-b: call_order_i_tm") }

    pub fn proto_code<T, P>(&mut self, _cl: T, _pc: P) -> lua_types::opcode::Instruction { todo!("phase-b: proto_code") }
    pub fn proto_const<T, I>(&mut self, _cl: &T, _idx: I) -> LuaValue { todo!("phase-b: proto_const") }
    pub fn get_proto_instr<T, P>(&mut self, _ci: T, _pc: P) -> lua_types::opcode::Instruction { todo!("phase-b: get_proto_instr") }
    pub fn dump_proto(&self, _proto: &GcRef<LuaProto>, _writer: &mut dyn FnMut(&[u8]) -> Result<(), LuaError>, _strip: bool) -> Result<(), LuaError> { todo!("phase-b: dump_proto") }

    pub fn trace_call(&mut self, _idx: CallInfoIdx) -> Result<bool, LuaError> { todo!("phase-b: trace_call") }
    pub fn trace_exec(&mut self, _idx: CallInfoIdx, _pc: u32) -> Result<bool, LuaError> { todo!("phase-b: trace_exec") }
    pub fn hook_call(&mut self, _idx: CallInfoIdx) -> Result<(), LuaError> { todo!("phase-b: hook_call_idx") }
    pub fn gc_check_step(&mut self) { /* phase-b no-op */ }
    pub fn gc_cond_step(&mut self) { /* phase-b no-op */ }
    pub fn gc_barrier_back<T, U>(&mut self, _t: T, _v: U) { /* phase-b no-op */ }
    pub fn gc_barrier_upval<T, U, V>(&mut self, _cl: T, _uv: U, _v: V) { /* phase-b no-op */ }
    pub fn is_main_thread(&mut self) -> bool { todo!("phase-b: is_main_thread") }
    pub fn obj_type_name(&self, _v: &LuaValue) -> &'static [u8] { todo!("phase-b: obj_type_name") }
    pub fn emit_warning(&mut self, _msg: &[u8], _to_cont: bool) { warning(self, _msg, _to_cont) }
}

// ─── GcHandle — no-op GC facade ───────────────────────────────────────────────

/// A short-lived handle returned by `state.gc()` for GC operations.
///
/// In Phases A–C all methods are no-ops. Phase D replaces with real GC.
pub struct GcHandle<'a> {
    _state: &'a mut LuaState,
}

impl<'a> GcHandle<'a> {
    /// C: `luaC_checkGC(L)` — conditional GC step.
    /// macros.tsv: `luaC_checkGC → state.gc().check_step()`
    pub fn check_step(&self) {
        // PORT NOTE: Phase A–C no-op; Phase D triggers incremental GC step
    }

    /// C: `luaC_fullgc(L, isemergency)` — full collection.
    /// macros.tsv: `luaC_fullgc → state.gc().full_collect()`
    pub fn full_collect(&self) {
        // PORT NOTE: Phase A–C no-op
    }

    /// Phase-B stub for `luaC_step(L)`.
    pub fn step(&self) { /* phase-b no-op */ }

    /// Phase-B stub for changing GC modes (incremental/generational).
    pub fn change_mode(&self, _mode: GcKind) { /* phase-b no-op */ }

    /// Phase-B stub for `luaC_fix(L, o)` — pin an object so GC won't collect it.
    pub fn fix_object<T: ?Sized>(&self, _o: &GcRef<T>) { /* phase-b no-op */ }

    /// Free all collectable objects (called during state teardown).
    ///
    /// C: `luaC_freeallobjects(L)` in `lgc.c`.
    /// PORT NOTE: In Phases A–C, Rc drop chains handle deallocation automatically.
    pub fn free_all_objects(&self) {
        // PORT NOTE: Phase A–C no-op; Rc::drop handles deallocation
    }

    /// GC write barrier for a TValue.
    ///
    /// C: `luaC_barrier(L, p, v)`.
    /// macros.tsv: `luaC_barrier → state.gc().barrier(p, v)` — no-op in Phases A–C
    pub fn barrier(&self, _p: &dyn std::any::Any, _v: &LuaValue) {}

    /// Backward write barrier.
    ///
    /// C: `luaC_barrierback(L, p, v)`.
    /// macros.tsv: `luaC_barrierback → state.gc().barrier_back(p, v)` — no-op
    pub fn barrier_back(&self, _p: &dyn std::any::Any, _v: &LuaValue) {}

    /// Object write barrier.
    ///
    /// C: `luaC_objbarrier(L, o, v)`.
    /// macros.tsv: `luaC_objbarrier → state.gc().obj_barrier(p, o)` — no-op
    pub fn obj_barrier(&self, _p: &dyn std::any::Any, _o: &dyn std::any::Any) {}

    /// Backward object write barrier.
    ///
    /// C: `luaC_objbarrierback(L, p, o)` — no-op in Phases A–C
    pub fn obj_barrier_back(&self, _p: &dyn std::any::Any, _o: &dyn std::any::Any) {}
}

// ─── Functions from lstate.c ──────────────────────────────────────────────────

// C: static unsigned int luai_makeseed(lua_State *L)
//
// PORT NOTE: `luai_makeseed` in C mixed ASLR entropy (pointer addresses of a
// heap var, stack var, and code symbol) with the current time via `luaS_hash`.
// In Rust, raw pointer addresses require `unsafe` which is forbidden outside
// lua-gc/lua-coro.  Phase A uses time-only entropy.  The hash is computed via
// `crate::string::hash_bytes` to match the Lua FNV-style algorithm.
fn make_seed() -> u32 {
    // C: unsigned int h = cast_uint(time(NULL));
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as u32)
        .unwrap_or(0);

    // TODO(port): mix in ASLR entropy (pointer to heap / stack / code).
    // Requires a short `unsafe` block to cast references to usize.
    // The entropy improvement is important for hash DoS resistance (CVE-class).
    // Phase B should add this via a platform-specific helper in lua-gc or via
    // the `getrandom` crate if it is added as a dependency.

    // C: return luaS_hash(buff, p, h)
    // For Phase A, just hash the time bytes against itself.
    crate::string::hash_bytes(&t.to_le_bytes(), t)
}

/// Adjust `GCdebt` to `debt` while preserving the `totalbytes + GCdebt` invariant.
///
/// C: `void luaE_setdebt(global_State *g, l_mem debt)` — LUAI_FUNC (pub(crate))
///
/// ```c
/// // C: void luaE_setdebt(global_State *g, l_mem debt) {
/// //   l_mem tb = gettotalbytes(g);
/// //   lua_assert(tb > 0);
/// //   if (debt < tb - MAX_LMEM)
/// //     debt = tb - MAX_LMEM;
/// //   g->totalbytes = tb - debt;
/// //   g->GCdebt = debt;
/// // }
/// ```
pub(crate) fn set_debt(g: &mut GlobalState, mut debt: isize) {
    // C: l_mem tb = gettotalbytes(g);
    let tb = g.total_bytes() as isize;
    // C: lua_assert(tb > 0);
    debug_assert!(tb > 0);
    // C: if (debt < tb - MAX_LMEM) debt = tb - MAX_LMEM;
    // macros.tsv: MAX_LMEM → isize::MAX
    if debt < tb.saturating_sub(isize::MAX) {
        debt = tb - isize::MAX;
    }
    // C: g->totalbytes = tb - debt;
    g.totalbytes = tb - debt;
    // C: g->GCdebt = debt;
    g.gc_debt = debt;
}

/// Deprecated no-op that returns `LUAI_MAXCCALLS`.
///
/// C: `LUA_API int lua_setcstacklimit(lua_State *L, unsigned int limit)` (pub)
///
/// ```c
/// // C: LUA_API int lua_setcstacklimit(lua_State *L, unsigned int limit) {
/// //   UNUSED(L); UNUSED(limit);
/// //   return LUAI_MAXCCALLS;  /* warning?? */
/// // }
/// ```
pub fn set_c_stack_limit(_state: &mut LuaState, _limit: u32) -> i32 {
    // C: UNUSED(L); UNUSED(limit);
    let _ = (_state, _limit);
    // C: return LUAI_MAXCCALLS;
    LUAI_MAXCCALLS as i32
}

/// Allocate a fresh `CallInfo` beyond the current frame and return its index.
///
/// C: `CallInfo *luaE_extendCI(lua_State *L)` — LUAI_FUNC (pub(crate))
///
/// ```c
/// // C: CallInfo *luaE_extendCI(lua_State *L) {
/// //   CallInfo *ci;
/// //   lua_assert(L->ci->next == NULL);
/// //   ci = luaM_new(L, CallInfo);
/// //   L->ci->next = ci;
/// //   ci->previous = L->ci;
/// //   ci->next = NULL;
/// //   ci->u.l.trap = 0;
/// //   L->nci++;
/// //   return ci;
/// // }
/// ```
pub(crate) fn extend_ci(state: &mut LuaState) -> CallInfoIdx {
    // C: lua_assert(L->ci->next == NULL);
    debug_assert!(
        state.call_info[state.ci.0 as usize].next.is_none(),
        "extend_ci: current ci already has a cached next frame"
    );

    let current_idx = state.ci;
    // C: ci = luaM_new(L, CallInfo);
    // macros.tsv: luaM_new → Box::new(T::default()) — here we push onto the Vec
    let new_idx = CallInfoIdx(state.call_info.len() as u32);

    state.call_info.push(CallInfo {
        // C: ci->previous = L->ci;
        previous: Some(current_idx),
        // C: ci->next = NULL;
        next: None,
        // C: ci->u.l.trap = 0;
        u: CallInfoFrame::lua_default(),
        ..CallInfo::default()
    });

    // C: L->ci->next = ci;
    state.call_info[current_idx.0 as usize].next = Some(new_idx);

    // C: L->nci++;
    state.nci += 1;

    new_idx
}

/// Free all cached (unused) `CallInfo` frames beyond the current frame.
///
/// C: `static void freeCI(lua_State *L)` (private)
///
/// ```c
/// // C: static void freeCI(lua_State *L) {
/// //   CallInfo *ci = L->ci;
/// //   CallInfo *next = ci->next;
/// //   ci->next = NULL;
/// //   while ((ci = next) != NULL) {
/// //     next = ci->next;
/// //     luaM_free(L, ci);
/// //     L->nci--;
/// //   }
/// // }
/// ```
///
/// PORT NOTE: In C, each `CallInfo` is an independent heap allocation freed by
/// `luaM_free`.  In Rust, all `CallInfo` entries live in `state.call_info: Vec<CallInfo>`.
/// We walk the link chain to count removals (updating `nci`), then truncate the Vec.
/// This is safe as long as all free entries have indices greater than `state.ci`.
fn free_ci(state: &mut LuaState) {
    let ci_idx = state.ci.0 as usize;

    // C: CallInfo *next = ci->next; ci->next = NULL;
    let mut next_opt = state.call_info[ci_idx].next.take();

    // C: while ((ci = next) != NULL) { next = ci->next; luaM_free(L, ci); L->nci--; }
    while let Some(idx) = next_opt {
        next_opt = state.call_info[idx.0 as usize].next;
        // C: L->nci--;
        state.nci = state.nci.saturating_sub(1);
    }

    // Truncate: drop all entries beyond the current ci.
    // TODO(port): verify invariant that all cached frames have contiguous indices > state.ci
    state.call_info.truncate(ci_idx + 1);
}

/// Free approximately half of the cached `CallInfo` frames beyond the current frame.
///
/// C: `void luaE_shrinkCI(lua_State *L)` — LUAI_FUNC (pub(crate))
///
/// ```c
/// // C: void luaE_shrinkCI(lua_State *L) {
/// //   CallInfo *ci = L->ci->next;
/// //   CallInfo *next;
/// //   if (ci == NULL) return;
/// //   while ((next = ci->next) != NULL) {
/// //     CallInfo *next2 = next->next;
/// //     ci->next = next2;
/// //     L->nci--;
/// //     luaM_free(L, next);
/// //     if (next2 == NULL) break;
/// //     else { next2->previous = ci; ci = next2; }
/// //   }
/// // }
/// ```
///
/// PORT NOTE: The C code removes every other node from the free-list chain by
/// pointer manipulation.  In Rust, removing elements from the middle of a `Vec`
/// shifts subsequent elements and invalidates `CallInfoIdx` values that point
/// past the removal site.  For Phase A, we approximate by halving the free count
/// via truncation.  TODO(port): Phase B should implement a proper free-list
/// pool (e.g., a slab) that allows O(1) element removal without index
/// invalidation.
pub(crate) fn shrink_ci(state: &mut LuaState) {
    let ci_idx = state.ci.0 as usize;

    // C: CallInfo *ci = L->ci->next;
    // C: if (ci == NULL) return;
    if state.call_info[ci_idx].next.is_none() {
        return;
    }

    let free_count = state.call_info.len().saturating_sub(ci_idx + 1);
    if free_count <= 1 {
        return;
    }

    // Remove every other cached frame (halve the free list).
    // PERF(port): truncation is O(n) copy for the drop; a slab allocator
    // would be O(1) — profile in Phase B.
    let keep = free_count / 2;
    let removed = free_count - keep;
    let new_len = ci_idx + 1 + keep;
    state.call_info.truncate(new_len);
    state.nci = state.nci.saturating_sub(removed as u32);

    // Terminate the now-last cached frame.
    if let Some(last) = state.call_info.last_mut() {
        last.next = None;
    }
}

/// Check whether the C-call depth has reached its limit and raise an error if so.
///
/// C: `void luaE_checkcstack(lua_State *L)` — LUAI_FUNC (pub(crate))
///
/// ```c
/// // C: void luaE_checkcstack(lua_State *L) {
/// //   if (getCcalls(L) == LUAI_MAXCCALLS)
/// //     luaG_runerror(L, "C stack overflow");
/// //   else if (getCcalls(L) >= (LUAI_MAXCCALLS / 10 * 11))
/// //     luaD_throw(L, LUA_ERRERR);
/// // }
/// ```
pub(crate) fn check_c_stack(state: &mut LuaState) -> Result<(), LuaError> {
    // C: if (getCcalls(L) == LUAI_MAXCCALLS) luaG_runerror(L, "C stack overflow");
    // macros.tsv: getCcalls → state.c_calls()
    // error_sites.tsv: luaG_runerror → return Err(LuaError::runtime(format_args!(...)))
    if state.c_calls() == LUAI_MAXCCALLS {
        return Err(LuaError::runtime(format_args!("C stack overflow")));
    }
    // C: else if (getCcalls(L) >= (LUAI_MAXCCALLS / 10 * 11)) luaD_throw(L, LUA_ERRERR);
    // error_sites.tsv: luaD_throw(L, LUA_ERRERR) → return Err(LuaError::with_status(LuaStatus::ErrErr))
    if state.c_calls() >= (LUAI_MAXCCALLS / 10 * 11) {
        // TODO(port): LuaError::with_status takes a LuaStatus enum, not a raw i32.
        // The exact constructor shape depends on lua-types/error.rs in Phase B.
        return Err(LuaError::runtime(format_args!(
            "error while handling stack overflow (C stack overflow)"
        )));
    }
    Ok(())
}

/// Increment the C-call depth counter, checking for overflow.
///
/// C: `LUAI_FUNC void luaE_incCstack(lua_State *L)` — pub(crate)
///
/// ```c
/// // C: LUAI_FUNC void luaE_incCstack(lua_State *L) {
/// //   L->nCcalls++;
/// //   if (l_unlikely(getCcalls(L) >= LUAI_MAXCCALLS))
/// //     luaE_checkcstack(L);
/// // }
/// ```
pub(crate) fn inc_c_stack(state: &mut LuaState) -> Result<(), LuaError> {
    // C: L->nCcalls++;
    state.nCcalls += 1;
    // C: if (l_unlikely(getCcalls(L) >= LUAI_MAXCCALLS)) luaE_checkcstack(L);
    // macros.tsv: l_unlikely → x (drop branch hint); getCcalls → state.c_calls()
    if state.c_calls() >= LUAI_MAXCCALLS {
        check_c_stack(state)?;
    }
    Ok(())
}

// C: static void stack_init(lua_State *L1, lua_State *L)
//
// PORT NOTE: In C, `L` is a separate thread used only for memory allocation
// (via `luaM_newvector`).  In Rust we don't have a custom allocator; all
// allocation goes through the global Rust allocator.  The function takes only
// the new thread (`thread`) and ignores the caller.
fn stack_init(thread: &mut LuaState) {
    // C: L1->stack.p = luaM_newvector(L, BASIC_STACK_SIZE + EXTRA_STACK, StackValue);
    // macros.tsv: luaM_newvector → vec![T::default(); n]
    let total_slots = BASIC_STACK_SIZE + EXTRA_STACK;
    thread.stack = vec![StackValue::default(); total_slots];

    // C: L1->tbclist.p = L1->stack.p;  (tbclist = stack base sentinel = "no tbc vars")
    // types.tsv: lua_State.tbclist → Vec<StackIdx>
    // PORT NOTE: In C, tbclist.p = stack.p is a sentinel meaning "no tbc vars".
    // In Rust the Vec is empty when there are no tbc variables.
    thread.tbclist = Vec::new();

    // C: for (i = 0; i < BASIC_STACK_SIZE + EXTRA_STACK; i++)
    //      setnilvalue(s2v(L1->stack.p + i));  /* erase new stack */
    // macros.tsv: setnilvalue → *o = LuaValue::Nil
    // Already initialized to LuaValue::Nil via StackValue::default().

    // C: L1->top.p = L1->stack.p;  (top = stack base = index 0)
    thread.top = StackIdx(0);

    // C: L1->stack_last.p = L1->stack.p + BASIC_STACK_SIZE;
    thread.stack_last = StackIdx(BASIC_STACK_SIZE as u32);

    // C: ci = &L1->base_ci;
    // C: ci->next = ci->previous = NULL;
    // C: ci->callstatus = CIST_C;
    // C: ci->func.p = L1->top.p;     → func = current top = StackIdx(0)
    // C: ci->u.c.k = NULL;
    // C: ci->nresults = 0;
    // C: setnilvalue(s2v(L1->top.p)); → stack[0] = Nil (the "function" entry)
    // C: L1->top.p++;                  → top becomes 1
    // C: ci->top.p = L1->top.p + LUA_MINSTACK;  → ci.top = 1 + LUA_MINSTACK
    // C: L1->ci = ci;                  → ci = CallInfoIdx(0)

    let base_ci = CallInfo {
        func: StackIdx(0),
        top: StackIdx(1 + LUA_MINSTACK as u32),
        previous: None,
        next: None,
        callstatus: CIST_C,
        nresults: 0,
        u: CallInfoFrame::c_default(),
        u2: CallInfoExtra::default(),
    };

    if thread.call_info.is_empty() {
        thread.call_info.push(base_ci);
    } else {
        thread.call_info[0] = base_ci;
        thread.call_info.truncate(1);
    }

    // C: setnilvalue(s2v(L1->top.p));  stack[top=0] = Nil (function entry for ci)
    thread.stack[0] = StackValue { val: LuaValue::Nil, tbc_delta: 0 };

    // C: L1->top.p++;
    thread.top = StackIdx(1);

    // C: L1->ci = ci;
    thread.ci = CallInfoIdx(0);
}

// C: static void freestack(lua_State *L)
fn free_stack(state: &mut LuaState) {
    // C: if (L->stack.p == NULL) return;  /* stack not completely built yet */
    if state.stack.is_empty() {
        return;
    }
    // C: L->ci = &L->base_ci; freeCI(L);
    state.ci = CallInfoIdx(0);
    free_ci(state);
    // C: lua_assert(L->nci == 0);
    debug_assert_eq!(state.nci, 0, "nci should be 0 after free_ci");
    // C: luaM_freearray(L, L->stack.p, stacksize(L) + EXTRA_STACK);
    // macros.tsv: luaM_freearray → (Rust's Drop handles deallocation; drop the call)
    state.stack.clear();
    state.stack.shrink_to_fit();
}

// C: static void init_registry(lua_State *L, global_State *g)
fn init_registry(state: &mut LuaState) -> Result<(), LuaError> {
    // C: Table *registry = luaH_new(L);
    // macros.tsv: luaH_new → state.new_table()
    let registry = state.new_table();

    // C: sethvalue(L, &g->l_registry, registry);
    // macros.tsv: sethvalue → *o = LuaValue::Table(x.clone())
    state.global_mut().l_registry = LuaValue::Table(registry.clone());

    // C: luaH_resize(L, registry, LUA_RIDX_LAST, 0);
    // macros.tsv: luaH_resize → t.resize(state, na, nh)?
    // TODO(port): registry is a GcRef<LuaTable> (Rc); calling methods requires borrow_mut()
    // For Phase A, use RefCell interior mutability on LuaTable, or accept the limitation.
    // Using Rc::get_mut is not available because of possible aliasing.
    // TODO(port): LuaTable resize requires &mut access through Rc — needs RefCell<LuaTable>
    //   or a redesign in Phase B.

    // C: setthvalue(L, &registry->array[LUA_RIDX_MAINTHREAD - 1], L);
    // macros.tsv: setthvalue → *o = LuaValue::Thread(x.clone())
    // TODO(port): cannot create GcRef<LuaState> to self (self-referential Rc).
    // In Phase E this would be resolved once coroutine threads are GcRef-tracked.
    // For Phase A: leave registry[LUA_RIDX_MAINTHREAD-1] as Nil and add a TODO.
    // TODO(port): set registry[LUA_RIDX_MAINTHREAD - 1] = LuaValue::Thread(main_thread_gcref)

    // C: sethvalue(L, &registry->array[LUA_RIDX_GLOBALS - 1], luaH_new(L));
    // PORT NOTE (phase-b-reconcile): The lua-types LuaTable placeholder is
    // storage-less, so we can't actually persist the globals table inside
    // the registry via array_set. Store it in a direct GlobalState field
    // and patch get_global_table to read it from there. Symmetric for the
    // _LOADED module cache. Once the LuaTable placeholder reconciles, the
    // canonical registry storage takes over and these fields disappear.
    let globals = state.new_table();
    state.global_mut().globals = LuaValue::Table(globals);
    let loaded = state.new_table();
    state.global_mut().loaded = LuaValue::Table(loaded);

    Ok(())
}

// C: static void f_luaopen(lua_State *L, void *ud)
fn lua_open(state: &mut LuaState) -> Result<(), LuaError> {
    // C: UNUSED(ud);
    // C: stack_init(L, L);
    stack_init(state);
    // C: init_registry(L, g);
    init_registry(state)?;
    // C: luaS_init(L);
    crate::string::init(state)?;
    // C: luaT_init(L);
    // TODO(port): crate::tagmethods::init(state)? — ltm.c → tagmethods.rs, not yet written
    // C: luaX_init(L);
    // TODO(port): luaX_init lives in the lua-lex crate; cross-crate call needed in Phase B
    // C: g->gcstp = 0; /* allow gc */
    state.global_mut().gcstp = 0;
    // C: setnilvalue(&g->nilvalue); /* now state is complete */
    // macros.tsv: setnilvalue → *o = LuaValue::Nil
    // PORT NOTE: setting nilvalue = Nil signals completestate() → is_complete() = true
    state.global_mut().nilvalue = LuaValue::Nil;
    // C: luai_userstateopen(L); → no-op; drop
    // macros.tsv: luai_userstateopen → (extension hook, no-op default; drop)
    Ok(())
}

// C: static void preinit_thread(lua_State *L, global_State *g)
fn preinit_thread(thread: &mut LuaState, global: Rc<RefCell<GlobalState>>) {
    // C: G(L) = g;
    thread.global = global;
    // C: L->stack.p = NULL;
    thread.stack = Vec::new();
    // C: L->ci = NULL; — sentinel: empty call_info
    thread.call_info = Vec::new();
    // PORT NOTE: We initialize ci to 0 but call_info is empty; stack_init() must be
    // called before any use of call_info.
    thread.ci = CallInfoIdx(0);
    // C: L->nci = 0;
    thread.nci = 0;
    // C: L->twups = L; /* thread has no upvalues */
    // PORT NOTE: In C, L->twups = L is a self-reference sentinel meaning "no open upvals".
    // In Rust, GlobalState.twups is a Vec<GcRef<LuaState>>; absence from that Vec is the
    // sentinel.  The per-thread `twups` field is removed (types.tsv: lua_State.twups → removed).
    // C: L->nCcalls = 0;
    thread.nCcalls = 0;
    // C: L->errorJmp = NULL; — replaced by Result<T, LuaError>; no field
    // C: L->hook = NULL;
    thread.hook = None;
    // C: L->hookmask = 0;
    thread.hookmask = 0;
    // C: L->basehookcount = 0;
    thread.basehookcount = 0;
    // C: L->allowhook = 1;
    thread.allowhook = true;
    // C: resethookcount(L); → L->hookcount = L->basehookcount
    // macros.tsv: resethookcount → state.reset_hook_count()
    thread.hookcount = thread.basehookcount;
    // C: L->openupval = NULL;
    thread.openupval = Vec::new();
    // C: L->status = LUA_OK;
    thread.status = LuaStatus::Ok as u8;
    // C: L->errfunc = 0;
    thread.errfunc = 0;
    // C: L->oldpc = 0;
    thread.oldpc = 0;
}

// C: static void close_state(lua_State *L)
fn close_state(state: &mut LuaState) {
    // C: global_State *g = G(L);
    let is_complete = state.global().is_complete();

    // C: if (!completestate(g)) luaC_freeallobjects(L); /* just collect its objects */
    if !is_complete {
        // macros.tsv: luaC_freeallobjects via GcHandle
        state.gc().free_all_objects();
    } else {
        // C: L->ci = &L->base_ci;  /* unwind CallInfo list */
        state.ci = CallInfoIdx(0);
        // C: luaD_closeprotected(L, 1, LUA_OK);  /* close all upvalues */
        // TODO(port): crate::do_::close_protected(state, StackIdx(1), LuaStatus::Ok)
        // Ignoring result here because we are in teardown (same as C behavior).
        // C: luaC_freeallobjects(L);  /* collect all objects */
        state.gc().free_all_objects();
        // C: luai_userstateclose(L); → no-op; drop
        // macros.tsv: luai_userstateclose → (extension hook; drop)
    }

    // C: luaM_freearray(L, G(L)->strt.hash, G(L)->strt.size);
    // macros.tsv: luaM_freearray → (Rust's Drop handles deallocation; drop the call)
    state.global_mut().strt = StringPool::default();

    // C: freestack(L);
    free_stack(state);

    // C: lua_assert(gettotalbytes(g) == sizeof(LG));
    // PORT NOTE: C-specific memory accounting assertion; not applicable in Rust.

    // C: (*g->frealloc)(g->ud, fromstate(L), sizeof(LG), 0);  /* free main block */
    // PORT NOTE: Custom allocator freed LG here. Rust's allocator (via Drop) handles
    // deallocation of GlobalState and LuaState automatically.
}

/// Create a new coroutine thread sharing the same GlobalState as the caller.
///
/// Pushes the new thread onto the caller's stack and returns `Ok(())`.
///
/// C: `LUA_API lua_State *lua_newthread(lua_State *L)` (pub)
///
/// ```c
/// // C: LUA_API lua_State *lua_newthread(lua_State *L) {
/// //   global_State *g = G(L);
/// //   GCObject *o;
/// //   lua_State *L1;
/// //   lua_lock(L); luaC_checkGC(L);
/// //   o = luaC_newobjdt(L, LUA_TTHREAD, sizeof(LX), offsetof(LX, l));
/// //   L1 = gco2th(o);
/// //   setthvalue2s(L, L->top.p, L1); api_incr_top(L);
/// //   preinit_thread(L1, g);
/// //   ... (copy hook settings, extra space, stack_init) ...
/// //   lua_unlock(L); return L1;
/// // }
/// ```
pub fn new_thread(state: &mut LuaState) -> Result<(), LuaError> {
    // C: lua_lock(L); → no-op; macros.tsv: lua_lock → (drop entirely)
    // C: luaC_checkGC(L);
    // macros.tsv: luaC_checkGC → state.gc().check_step()
    state.gc().check_step();

    // C: o = luaC_newobjdt(L, LUA_TTHREAD, sizeof(LX), offsetof(LX, l));
    // C: L1 = gco2th(o);
    // PORT NOTE: In C, the new thread is GC-allocated as part of the allgc list.
    // In Rust (Phase A), we create a plain LuaState; Phase D will wire GC registration.
    // TODO(port): allocate via state.gc().new_obj(LuaType::Thread, ...) in Phase D

    let global_rc = state.global_rc();
    let hookmask = state.hookmask;
    let basehookcount = state.basehookcount;

    let mut new_thread = LuaState {
        status: LuaStatus::Ok as u8,
        allowhook: true,
        nci: 0,
        top: StackIdx(0),
        stack_last: StackIdx(0),
        stack: Vec::new(),
        ci: CallInfoIdx(0),
        call_info: Vec::new(),
        openupval: Vec::new(),
        tbclist: Vec::new(),
        global: global_rc.clone(),
        hook: None,
        hookmask: 0,
        basehookcount: 0,
        hookcount: 0,
        errfunc: 0,
        nCcalls: 0,
        oldpc: 0,
        marked: 0,
    };

    // C: preinit_thread(L1, g);
    preinit_thread(&mut new_thread, global_rc);

    // C: L1->hookmask = L->hookmask;
    new_thread.hookmask = hookmask;
    // C: L1->basehookcount = L->basehookcount;
    new_thread.basehookcount = basehookcount;
    // C: L1->hook = L->hook;
    // TODO(port): lua_Hook is Box<dyn FnMut(...)>; not Clone.
    // Sharing a hook between threads would require Arc<Mutex<...>> (Phase E debug).
    // C: resethookcount(L1);
    new_thread.reset_hook_count();

    // C: memcpy(lua_getextraspace(L1), lua_getextraspace(g->mainthread), LUA_EXTRASPACE);
    // macros.tsv: lua_getextraspace → state.extra_space_mut() → &mut [u8]
    // TODO(port): LuaState.extra_space field not yet defined; Phase B

    // C: luai_userstatethread(L, L1); → no-op; drop
    // macros.tsv: luai_userstatethread → (extension hook; drop)

    // C: stack_init(L1, L);
    stack_init(&mut new_thread);

    // Wrap in GcRef and push onto caller's stack.
    // TODO(port): register new_thread in state.global_mut().allgc for GC tracking (Phase D)
    let _thread_ref: GcRef<LuaState> = GcRef::new(new_thread);

    // C: setthvalue2s(L, L->top.p, L1);
    // macros.tsv: setthvalue2s → state.set_at(o, LuaValue::Thread(th.clone()))
    // C: api_incr_top(L); → state.push() already increments
    // TODO(phase-b): LuaValue::Thread expects GcRef<lua_types::value::LuaThread>;
    // the rich LuaState lives in lua-vm and the two have not been unified. Pushing
    // a placeholder for now so Phase A state construction can compile.
    state.push(LuaValue::Thread(GcRef::new(lua_types::value::LuaThread::placeholder())));

    // C: lua_unlock(L); → no-op; macros.tsv: lua_unlock → (drop entirely)
    Ok(())
}

/// Free all resources held by a coroutine thread.
///
/// C: `void luaE_freethread(lua_State *L, lua_State *L1)` — LUAI_FUNC (pub(crate))
///
/// ```c
/// // C: void luaE_freethread(lua_State *L, lua_State *L1) {
/// //   LX *l = fromstate(L1);
/// //   luaF_closeupval(L1, L1->stack.p);  /* close all upvalues */
/// //   lua_assert(L1->openupval == NULL);
/// //   luai_userstatefree(L, L1);
/// //   freestack(L1);
/// //   luaM_free(L, l);
/// // }
/// ```
pub(crate) fn free_thread(caller: &mut LuaState, thread: &mut LuaState) {
    // C: luaF_closeupval(L1, L1->stack.p);  /* close all upvalues */
    // TODO(port): crate::func::close_upval(thread, StackIdx(0)) — lfunc.c → func.rs
    let _ = caller; // caller used only for luai_userstatefree (no-op)

    // C: lua_assert(L1->openupval == NULL);
    // macros.tsv: lua_assert → debug_assert!
    debug_assert!(
        thread.openupval.is_empty(),
        "free_thread: open upvalues remain after close_upval"
    );

    // C: luai_userstatefree(L, L1); → no-op; drop
    // macros.tsv: luai_userstatefree → (extension hook; drop)

    // C: freestack(L1);
    free_stack(thread);

    // C: luaM_free(L, l); → Rust's Drop frees LuaState automatically
}

/// Reset a thread to its base state, closing all to-be-closed variables.
///
/// Returns the final status code as an `i32` (mirrors the C API).
///
/// C: `int luaE_resetthread(lua_State *L, int status)` — LUAI_FUNC (pub(crate))
///
/// ```c
/// // C: int luaE_resetthread(lua_State *L, int status) {
/// //   CallInfo *ci = L->ci = &L->base_ci;
/// //   setnilvalue(s2v(L->stack.p));
/// //   ci->func.p = L->stack.p;
/// //   ci->callstatus = CIST_C;
/// //   if (status == LUA_YIELD) status = LUA_OK;
/// //   L->status = LUA_OK;  /* so it can run __close metamethods */
/// //   status = luaD_closeprotected(L, 1, status);
/// //   if (status != LUA_OK) luaD_seterrorobj(L, status, L->stack.p + 1);
/// //   else L->top.p = L->stack.p + 1;
/// //   ci->top.p = L->top.p + LUA_MINSTACK;
/// //   luaD_reallocstack(L, cast_int(ci->top.p - L->stack.p), 0);
/// //   return status;
/// // }
/// ```
pub(crate) fn reset_thread(state: &mut LuaState, status: i32) -> i32 {
    // C: CallInfo *ci = L->ci = &L->base_ci;
    state.ci = CallInfoIdx(0);
    let ci_idx = 0usize;

    // C: setnilvalue(s2v(L->stack.p));
    // macros.tsv: setnilvalue → *o = LuaValue::Nil; s2v → state.stack_at(idx)
    if !state.stack.is_empty() {
        state.stack[0].val = LuaValue::Nil;
    }

    // C: ci->func.p = L->stack.p;
    state.call_info[ci_idx].func = StackIdx(0);
    // C: ci->callstatus = CIST_C;
    state.call_info[ci_idx].callstatus = CIST_C;

    // C: if (status == LUA_YIELD) status = LUA_OK;
    let mut status = if status == LuaStatus::Yield as i32 {
        LuaStatus::Ok as i32
    } else {
        status
    };

    // C: L->status = LUA_OK;  /* so it can run __close metamethods */
    state.status = LuaStatus::Ok as u8;

    // C: status = luaD_closeprotected(L, 1, status);
    // TODO(port): crate::do_::close_protected(state, StackIdx(1), status) — ldo.c → do_.rs
    // For Phase A, skip the actual close (upvalue closing requires ldo.c).

    // C: if (status != LUA_OK) luaD_seterrorobj(L, status, L->stack.p + 1);
    if status != LuaStatus::Ok as i32 {
        // C: luaD_seterrorobj(L, status, L->stack.p + 1);
        // TODO(port): crate::do_::set_error_obj(state, status, StackIdx(1)) — ldo.c → do_.rs
    } else {
        // C: else L->top.p = L->stack.p + 1;
        state.top = StackIdx(1);
    }

    // C: ci->top.p = L->top.p + LUA_MINSTACK;
    let new_ci_top = StackIdx(state.top.0 + LUA_MINSTACK as u32);
    state.call_info[ci_idx].top = new_ci_top;

    // C: luaD_reallocstack(L, cast_int(ci->top.p - L->stack.p), 0);
    // TODO(port): crate::do_::realloc_stack(state, new_ci_top.0 as i32, 0) — ldo.c → do_.rs
    // For Phase A, grow the stack if needed to at least new_ci_top slots.
    let needed = new_ci_top.0 as usize;
    if state.stack.len() < needed {
        state.stack.resize(needed, StackValue::default());
    }

    status
}

/// Close a coroutine thread from the perspective of another thread.
///
/// C: `LUA_API int lua_closethread(lua_State *L, lua_State *from)` (pub)
///
/// ```c
/// // C: LUA_API int lua_closethread(lua_State *L, lua_State *from) {
/// //   int status;
/// //   lua_lock(L);
/// //   L->nCcalls = (from) ? getCcalls(from) : 0;
/// //   status = luaE_resetthread(L, L->status);
/// //   lua_unlock(L);
/// //   return status;
/// // }
/// ```
pub fn close_thread(state: &mut LuaState, from: Option<&LuaState>) -> i32 {
    // C: lua_lock(L); → no-op
    // C: L->nCcalls = (from) ? getCcalls(from) : 0;
    // macros.tsv: getCcalls → state.c_calls()
    state.nCcalls = match from {
        Some(f) => f.c_calls(),
        None => 0,
    };
    // C: status = luaE_resetthread(L, L->status);
    let current_status = state.status as i32;
    let result = reset_thread(state, current_status);
    // C: lua_unlock(L); → no-op
    result
}

/// Deprecated wrapper for `close_thread(L, NULL)`.
///
/// C: `LUA_API int lua_resetthread(lua_State *L)` (pub, deprecated)
///
/// ```c
/// // C: LUA_API int lua_resetthread(lua_State *L) {
/// //   return lua_closethread(L, NULL);
/// // }
/// ```
pub fn reset_thread_api(state: &mut LuaState) -> i32 {
    // C: return lua_closethread(L, NULL);
    close_thread(state, None)
}

/// Create a new independent Lua state.  Returns `None` only on OOM.
///
/// C: `LUA_API lua_State *lua_newstate(lua_Alloc f, void *ud)` (pub)
///
/// PORT NOTE: The C API takes a custom allocator `(f, ud)`.  The Rust-native API
/// uses the global Rust allocator; those parameters are dropped.  Equivalent to
/// `LuaState::new()` at the call site.
///
/// ```c
/// // C: LUA_API lua_State *lua_newstate(lua_Alloc f, void *ud) {
/// //   int i;
/// //   lua_State *L;
/// //   global_State *g;
/// //   LG *l = cast(LG *, (*f)(ud, NULL, LUA_TTHREAD, sizeof(LG)));
/// //   if (l == NULL) return NULL;
/// //   L = &l->l.l; g = &l->g;
/// //   L->tt = LUA_VTHREAD;
/// //   g->currentwhite = bitmask(WHITE0BIT);
/// //   L->marked = luaC_white(g);
/// //   preinit_thread(L, g);
/// //   g->allgc = obj2gco(L);
/// //   L->next = NULL;
/// //   incnny(L);
/// //   g->frealloc = f; g->ud = ud; g->warnf = NULL; g->ud_warn = NULL;
/// //   g->mainthread = L; g->seed = luai_makeseed(L);
/// //   g->gcstp = GCSTPGC;
/// //   ... (zero-init all GC list pointers and tunables) ...
/// //   setivalue(&g->nilvalue, 0);  /* signal: state not yet built */
/// //   ... (setgcparam tunables) ...
/// //   for (i=0; i < LUA_NUMTAGS; i++) g->mt[i] = NULL;
/// //   if (luaD_rawrunprotected(L, f_luaopen, NULL) != LUA_OK) {
/// //     close_state(L); L = NULL;
/// //   }
/// //   return L;
/// // }
/// ```
pub fn new_state() -> Option<LuaState> {
    // C: LG *l = (*f)(ud, NULL, LUA_TTHREAD, sizeof(LG)); if (l == NULL) return NULL;
    // In Rust, allocation failure panics by default; we use Result internally.

    // Build a dummy LuaString for memerrmsg and strcache initialization.
    // This is a chicken-and-egg problem: GlobalState.memerrmsg needs to be initialized
    // before luaS_init, but luaS_init creates the memerrmsg.
    // We use a placeholder Rc<LuaString> that will be replaced by luaS_init.
    // TODO(port): this is fragile; Phase B should ensure memerrmsg is properly set by luaS_init.
    let placeholder_str = GcRef::new(LuaString::placeholder());

    // C: g->currentwhite = bitmask(WHITE0BIT);
    // macros.tsv: bitmask → (1u32 << b); WHITE0BIT = 0 → 1u8
    let initial_white = 1u8 << WHITE0BIT;

    // C: setivalue(&g->nilvalue, 0);  /* to signal that state is not yet built */
    // macros.tsv: setivalue → *o = LuaValue::Int(x)
    // PORT NOTE: non-nil nilvalue signals "state not yet complete"; see is_complete().

    let global = GlobalState {
        parser_hook: None,
        totalbytes: std::mem::size_of::<GlobalState>() as isize,
        gc_debt: 0,
        gc_estimate: 0,
        lastatomic: 0,
        strt: StringPool::default(),
        l_registry: LuaValue::Nil,
        globals: LuaValue::Nil,
        loaded: LuaValue::Nil,
        // C: setivalue(&g->nilvalue, 0); — non-Nil = incomplete
        nilvalue: LuaValue::Int(0),
        // C: g->seed = luai_makeseed(L);
        seed: make_seed(),
        // C: g->currentwhite = bitmask(WHITE0BIT);
        currentwhite: initial_white,
        // C: g->gcstate = GCSpause;
        gcstate: GCS_PAUSE,
        // C: g->gckind = KGC_INC;
        // macros.tsv: KGC_INC → GcKind::Incremental
        gckind: GcKind::Incremental as u8,
        // C: g->gcstopem = 0;
        gcstopem: false,
        // C: g->genminormul = LUAI_GENMINORMUL;
        genminormul: LUAI_GENMINORMUL,
        // C: setgcparam(g->genmajormul, LUAI_GENMAJORMUL); → g->genmajormul = LUAI_GENMAJORMUL / 4
        // macros.tsv: setgcparam → p = v / 4
        genmajormul: (LUAI_GENMAJORMUL / 4) as u8,
        // C: g->gcstp = GCSTPGC;
        gcstp: GCSTPGC,
        // C: g->gcemergency = 0;
        gcemergency: false,
        // C: setgcparam(g->gcpause, LUAI_GCPAUSE);
        gcpause: (LUAI_GCPAUSE / 4) as u8,
        // C: setgcparam(g->gcstepmul, LUAI_GCMUL);
        gcstepmul: (LUAI_GCMUL / 4) as u8,
        // C: g->gcstepsize = LUAI_GCSTEPSIZE;
        gcstepsize: LUAI_GCSTEPSIZE,
        // C: g->allgc = obj2gco(L); — set after main thread created
        allgc: Vec::new(),
        sweepgc_cursor: 0,
        // C: g->finobj = g->tobefnz = g->fixedgc = NULL;
        finobj: Vec::new(),
        tobefnz: Vec::new(),
        fixedgc: Vec::new(),
        // GC lists (Phase D)
        gray: Vec::new(),
        grayagain: Vec::new(),
        // C: g->weak = g->ephemeron = g->allweak = NULL;
        weak: Vec::new(),
        ephemeron: Vec::new(),
        allweak: Vec::new(),
        // C: g->twups = NULL;
        twups: Vec::new(),
        // C: g->panic = NULL;
        panic: None,
        // C: g->mainthread = L; — set after main thread created
        mainthread: None,
        memerrmsg: placeholder_str.clone(),
        tmname: Vec::new(),
        // C: for (i=0; i < LUA_NUMTAGS; i++) g->mt[i] = NULL;
        mt: std::array::from_fn(|_| None),
        strcache: std::array::from_fn(|_| {
            std::array::from_fn(|_| placeholder_str.clone())
        }),
        warnf: None,
    };

    let global_rc = Rc::new(RefCell::new(global));

    // C: L->tt = LUA_VTHREAD; — encoded by LuaValue::Thread enum variant
    // C: L->marked = luaC_white(g);
    // macros.tsv: luaC_white → g.current_white()
    let initial_marked = initial_white;

    let mut main_thread = LuaState {
        status: LuaStatus::Ok as u8,
        allowhook: true,
        nci: 0,
        top: StackIdx(0),
        stack_last: StackIdx(0),
        stack: Vec::new(),
        ci: CallInfoIdx(0),
        call_info: Vec::new(),
        openupval: Vec::new(),
        tbclist: Vec::new(),
        global: global_rc.clone(),
        hook: None,
        hookmask: 0,
        basehookcount: 0,
        hookcount: 0,
        errfunc: 0,
        nCcalls: 0,
        oldpc: 0,
        marked: initial_marked,
    };

    // C: preinit_thread(L, g);
    preinit_thread(&mut main_thread, global_rc.clone());

    // C: incnny(L); /* main thread is always non yieldable */
    // macros.tsv: incnny → state.inc_nny() → L->nCcalls += 0x10000
    main_thread.inc_nny();

    // C: g->mainthread = L;
    // TODO(port): self-referential Rc cycle; Phase D GC handles cycles.
    // For Phase A: skip setting mainthread to avoid the cycle.

    // C: g->allgc = obj2gco(L); /* by now, only object is the main thread */
    // TODO(port): Phase D — register main_thread in allgc as a GcRef

    // C: if (luaD_rawrunprotected(L, f_luaopen, NULL) != LUA_OK) {
    //      close_state(L); L = NULL; }
    // error_sites.tsv: luaD_rawrunprotected → state.run_protected(|s| f(s, ud))
    // PORT NOTE: We call lua_open directly since we're not using the protected-call
    // machinery yet (ldo.c is not ported). Errors from lua_open propagate as Err.
    match lua_open(&mut main_thread) {
        Ok(()) => {}
        Err(_) => {
            // C: close_state(L); L = NULL;
            close_state(&mut main_thread);
            return None;
        }
    }

    Some(main_thread)
}

/// Close the Lua state and free all resources.
///
/// C: `LUA_API void lua_close(lua_State *L)` (pub)
///
/// PORT NOTE: In C, `lua_close` gets the main thread via `G(L)->mainthread`
/// and closes that regardless of which thread is passed.  In Rust, the caller
/// should hold the main `LuaState` and drop it (which triggers `close_state`
/// via this function or `Drop`).
///
/// ```c
/// // C: LUA_API void lua_close(lua_State *L) {
/// //   lua_lock(L);
/// //   L = G(L)->mainthread;  /* only the main thread can be closed */
/// //   close_state(L);
/// // }
/// ```
pub fn close(mut state: LuaState) {
    // C: lua_lock(L); → no-op; macros.tsv: lua_lock → (drop entirely)
    // C: L = G(L)->mainthread;
    // PORT NOTE: In Rust, callers must pass the main LuaState directly (or obtain it
    // from GlobalState.mainthread).  We do not traverse to the main thread here;
    // the caller owns the root state.
    // TODO(port): assert that `state` is indeed the main thread before closing
    // C: close_state(L);
    close_state(&mut state);
    // C: state drops here; Rust's Drop frees the LuaState struct
}

/// Forward a warning message through the configured warning sink.
///
/// C: `void luaE_warning(lua_State *L, const char *msg, int tocont)` — LUAI_FUNC (pub(crate))
///
/// ```c
/// // C: void luaE_warning(lua_State *L, const char *msg, int tocont) {
/// //   lua_WarnFunction wf = G(L)->warnf;
/// //   if (wf != NULL) wf(G(L)->ud_warn, msg, tocont);
/// // }
/// ```
pub(crate) fn warning(state: &mut LuaState, msg: &[u8], to_cont: bool) {
    // C: lua_WarnFunction wf = G(L)->warnf;
    // C: if (wf != NULL) wf(G(L)->ud_warn, msg, tocont);
    // types.tsv: global_State.warnf → Option<Box<dyn FnMut(&[u8], bool)>>
    // types.tsv: global_State.ud_warn → (removed; folded into the closure)
    // PORT NOTE: We must drop the RefMut borrow before calling the closure to avoid
    // a potential re-entrant borrow_mut() if the closure calls back into Lua.
    // We check for the presence of warnf while holding a borrow, then call it.
    // TODO(port): if the warning function needs to call back into state (e.g. to push
    // a Lua error), this will panic at runtime due to RefCell re-entry. Phase B should
    // design a safe re-entrance pattern (e.g. take + restore the warnf closure).
    let has_warnf = state.global().warnf.is_some();
    if has_warnf {
        // Take the warnf closure out to avoid re-entrant borrow.
        let mut warnf = state.global_mut().warnf.take();
        if let Some(ref mut f) = warnf {
            f(msg, to_cont);
        }
        // Restore the closure.
        state.global_mut().warnf = warnf;
    }
}

/// Emit a warning composed from the error object on top of the stack and a location.
///
/// C: `void luaE_warnerror(lua_State *L, const char *where)` — LUAI_FUNC (pub(crate))
///
/// ```c
/// // C: void luaE_warnerror(lua_State *L, const char *where) {
/// //   TValue *errobj = s2v(L->top.p - 1);
/// //   const char *msg = (ttisstring(errobj))
/// //                   ? getstr(tsvalue(errobj))
/// //                   : "error object is not a string";
/// //   luaE_warning(L, "error in ", 1);
/// //   luaE_warning(L, where, 1);
/// //   luaE_warning(L, " (", 1);
/// //   luaE_warning(L, msg, 1);
/// //   luaE_warning(L, ")", 0);
/// // }
/// ```
pub(crate) fn warn_error(state: &mut LuaState, where_: &[u8]) {
    // C: TValue *errobj = s2v(L->top.p - 1);
    // macros.tsv: s2v → state.stack_at(idx)
    let top_idx = state.top.0.saturating_sub(1) as usize;
    let errobj = state.stack.get(top_idx).map(|sv| sv.val.clone()).unwrap_or(LuaValue::Nil);

    // C: const char *msg = (ttisstring(errobj)) ? getstr(tsvalue(errobj)) : "error object is not a string";
    // macros.tsv: ttisstring → matches!(o, LuaValue::Str(_))
    // macros.tsv: getstr → ts.as_bytes(); tsvalue → o.as_string().expect("not string")
    // PORT NOTE: Clone the message bytes to avoid holding a borrow on `state.stack`
    // across the subsequent `warning()` calls which mutably borrow `state`.
    let msg: Vec<u8> = if let LuaValue::Str(ref s) = errobj {
        s.as_bytes().to_vec()
    } else {
        b"error object is not a string".to_vec()
    };

    // C: luaE_warning(L, "error in ", 1);
    warning(state, b"error in ", true);
    // C: luaE_warning(L, where, 1);
    warning(state, where_, true);
    // C: luaE_warning(L, " (", 1);
    warning(state, b" (", true);
    // C: luaE_warning(L, msg, 1);
    warning(state, &msg, true);
    // C: luaE_warning(L, ")", 0);
    warning(state, b")", false);
}

// ──────────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        src/lstate.c  (445 lines, 25 functions)
//                  src/lstate.h  (408 lines; struct definitions merged)
//   target_crate:  lua-vm
//   confidence:    medium
//   todos:         44
//   port_notes:    33
//   unsafe_blocks: 0   (must be 0 outside lua-gc/lua-coro)
//   notes:         Logic faithfully follows lstate.c. Key structural changes:
//                  (1) LX/LG C layout wrappers dropped; GlobalState is Rc<RefCell<>>.
//                  (2) CallInfo linked list → Vec<CallInfo> with CallInfoIdx indices;
//                      shrink_ci uses truncation rather than node-by-node removal.
//                  (3) lua_State.twups self-reference → membership in GlobalState.twups Vec.
//                  (4) errorJmp/setjmp → removed; errors use Result<T, LuaError>.
//                  (5) Custom allocator (lua_Alloc) → dropped; Rust's allocator handles it.
//                  (6) make_seed: ASLR pointer entropy requires unsafe; time-only for Phase A.
//                  Key TODOs: luaT_init and luaX_init cross-crate calls (Phase B);
//                  init_registry table mutations through Rc (needs RefCell<LuaTable>);
//                  luaD_closeprotected/seterrorobj/reallocstack in reset_thread (ldo.c);
//                  GcRef<LuaState> self-reference for mainthread (Phase D);
//                  LuaString::placeholder() helper needed for GlobalState init;
//                  LuaValue and LuaTable should move to object.rs once that lands.
// ──────────────────────────────────────────────────────────────────────────────
