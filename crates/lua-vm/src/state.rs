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
pub use lua_types::closure::{LuaCFnPtr, LuaClosure, LuaLClosure as LuaClosureLua, LuaCClosure as LuaClosureC};
pub use lua_types::proto::LuaProto;
pub use lua_types::upval::{UpVal, UpValState};
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
const GCSTPUSR: u8 = 1;
const GCSTPGC: u8 = 2;

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
    pub fn is_lua(&self) -> bool { (self.callstatus & CIST_C) == 0 }
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
        if let LuaValue::Str(s) = self {
            let mut tmp = LuaValue::Nil;
            let sz = crate::object::str2num(s.as_bytes(), &mut tmp);
            if sz == 0 { return None; }
            return match tmp {
                LuaValue::Int(i) => Some(i as f64),
                LuaValue::Float(f) => Some(f),
                _ => None,
            };
        }
        None
    }
    fn to_integer_no_strconv(&self) -> Option<i64> {
        match self {
            LuaValue::Int(i) => Some(*i),
            LuaValue::Float(f) if f.fract() == 0.0 && f.is_finite() => {
                // C: lua_numbertointeger range check —
                //   d >= LUA_MININTEGER && d < -(lua_Number)LUA_MININTEGER.
                // Without this, Rust's `as i64` saturates and silently
                // produces i64::MAX / i64::MIN for out-of-range floats.
                let min_f = i64::MIN as f64;
                let max_plus1_f = -(i64::MIN as f64);
                if *f >= min_f && *f < max_plus1_f {
                    Some(*f as i64)
                } else {
                    None
                }
            }
            _ => None,
        }
    }
    fn to_integer_with_strconv(&self) -> Option<i64> {
        if let Some(i) = self.to_integer_no_strconv() { return Some(i); }
        if let LuaValue::Str(s) = self {
            let mut tmp = LuaValue::Nil;
            let sz = crate::object::str2num(s.as_bytes(), &mut tmp);
            if sz == 0 { return None; }
            return tmp.to_integer_no_strconv();
        }
        None
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
    fn metatable(&self) -> Option<GcRef<LuaTable>> { (**self).metatable() }
    fn as_ptr(&self) -> *const () { GcRef::identity(self) as *const () }
    fn get(&self, k: &LuaValue) -> LuaValue { (**self).get(k) }
    fn get_int(&self, k: i64) -> LuaValue { (**self).get(&LuaValue::Int(k)) }
    fn get_short_str(&self, k: &GcRef<LuaString>) -> LuaValue { (**self).get_short_str(k) }
    fn raw_set(&self, _state: &mut LuaState, k: &LuaValue, v: LuaValue) -> Result<(), LuaError> {
        (**self).raw_set(k.clone(), v);
        Ok(())
    }
    fn raw_set_int(&self, _state: &mut LuaState, k: i64, v: LuaValue) -> Result<(), LuaError> {
        (**self).raw_set(LuaValue::Int(k), v);
        Ok(())
    }
    fn invalidate_tm_cache(&self) {}
    fn resize(&self, _state: &mut LuaState, _na: usize, _nh: usize) -> Result<(), LuaError> {
        Ok(())
    }
    fn next(&self, k: LuaValue) -> Result<Option<(LuaValue, LuaValue)>, LuaError> {
        if !matches!(k, LuaValue::Nil) && !(**self).contains_key(&k) {
            return Err(LuaError::runtime(format_args!("invalid key to 'next'")));
        }
        Ok((**self).next_pair(&k))
    }
}

pub trait LuaUserDataRefExt {
    fn metatable(&self) -> Option<GcRef<LuaTable>>;
    fn set_metatable(&self, mt: Option<GcRef<LuaTable>>);
    fn as_ptr(&self) -> *const ();
    fn len(&self) -> usize;
}
impl LuaUserDataRefExt for GcRef<LuaUserData> {
    fn metatable(&self) -> Option<GcRef<LuaTable>> { (**self).metatable() }
    fn set_metatable(&self, mt: Option<GcRef<LuaTable>>) { (**self).set_metatable(mt); }
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

/// Function-pointer signature for reading a file's full contents into memory,
/// installed on [`GlobalState::file_loader_hook`] by the embedder.
///
/// `std::fs` is banned outside `lua-cli`, so `lua-stdlib`'s `loadfile` and
/// `searcher_lua` reach the filesystem via this hook. `None` keeps the file
/// system unreachable, which is appropriate for embeddings where modules are
/// served exclusively from `package.preload`.
pub type FileLoaderHook = fn(filename: &[u8]) -> Result<Vec<u8>, LuaError>;

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

    /// Phase-B hook for reading a Lua source file from disk. Set by `lua-cli`
    /// (or any embedder that wants `require`/`loadfile` to reach the file
    /// system) since `std::fs` is banned in `lua-stdlib`. `None` makes
    /// `loadfile` and the Lua-file searcher report a file-not-found error.
    pub file_loader_hook: Option<FileLoaderHook>,

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

    // Phase-D NOTE: the C-Lua intrusive GC lists (allgc, sweepgc, finobj,
    // gray, grayagain, weak, ephemeron, allweak) were declared here as
    // `Vec<GcRef<dyn Collectable>>` during Phase A but never populated or
    // read. The real GC owns its own allgc chain inside `self.heap`
    // (lua_gc::Heap). Removed during D-1e-prep to clear the `?Sized` blocker
    // for swapping `GcRef<T> = Gc<T>` (Gc requires T: Sized for unsizing).
    // sweepgc_cursor stayed because non-list bookkeeping kept it.
    pub sweepgc_cursor: usize,

    /// Phase-B cross-table weak-sweep registry.
    ///
    /// `lua_types::value::sweep_weak_tables` iterates this list at
    /// `collectgarbage("collect")` time to clear entries whose weak target
    /// is held only by other weak slots. Holds `Weak<LuaTable>` so the
    /// registry itself does not pin tables that the user has dropped.
    /// Replaced by the proper `weak` / `ephemeron` / `allweak` lists when
    /// Phase D's incremental sweep lands.
    pub weak_tables_registry: Vec<lua_types::gc::GcWeak<lua_types::value::LuaTable>>,

    /// Phase-B long-string allocation tracker.
    ///
    /// Each entry pairs a `Weak<LuaString>` with the byte count that was
    /// added to `gc_debt` at allocation time. `collectgarbage("count")` walks
    /// the list and reclaims `gc_debt` for entries whose weak target has been
    /// dropped, so the Lua-visible memory total tracks live long-string bytes.
    /// Short strings are interned and bounded in size, so they are not tracked
    /// individually. Replaced by Phase D's real allocator accounting.
    pub gc_tracked_long_strings: Vec<(lua_types::gc::GcWeak<lua_types::string::LuaString>, usize)>,

    /// Phase-B pending-finalizer registry.
    ///
    /// Each entry is a strong `GcRef<LuaTable>` to a table whose metatable
    /// carried `__gc` at the time `setmetatable` was called. The strong ref
    /// pins the table so a normal `Rc::drop` does not destroy it before its
    /// `__gc` metamethod runs. The Phase-B finalizer sweep
    /// (`crate::api::run_pending_finalizers`) scans this list, takes any
    /// entry whose strong count is 1 (only this list holds it — i.e. the
    /// user has dropped every reference), and invokes its `__gc` before
    /// releasing the ref. Replaced by `finobj` / `tobefnz` when the real
    /// incremental GC lands in Phase D.
    pub pending_finalizers: Vec<GcRef<lua_types::value::LuaTable>>,

    // Phase-D NOTE: tobefnz + fixedgc removed (dead since Phase A — see
    // sibling note above re allgc et al). Pending finalizers live in
    // `pending_finalizers` above; fixed objects live in heap.allgc with the
    // GC's own `fixed` bit.

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

    /// Stable intern map for the public [`LuaString`] type. Distinct from
    /// `strt` (which keys internal `LuaStringImpl`) because the parser and
    /// stdlib need pointer-equality across `intern_str` calls so
    /// `GcRef::ptr_eq` can resolve variable identity. Without this map each
    /// call allocates a fresh `GcRef` and locals/upvalues fail to resolve.
    pub interned_lt: std::collections::HashMap<Box<[u8]>, GcRef<LuaString>>,

    // C: lua_WarnFunction warnf — warning function sink
    // types.tsv: global_State.warnf → Option<Box<dyn FnMut(&[u8], bool)>>
    pub warnf: Option<Box<dyn FnMut(&[u8], bool)>>,
    // C: void *ud_warn — folded into the `warnf` closure capture; removed

    /// Registry of native `LuaCFunction` pointers. Lua-types cannot reference
    /// `LuaState`, so `LuaClosure::LightC` carries a `usize` index into this
    /// vector instead of the real function pointer. `push_c_function`
    /// registers the function and stores the resulting index in the closure.
    pub c_functions: Vec<LuaCFunction>,

    /// Phase-D heap. Owns the allgc intrusive list and runs collections.
    /// During Phase A-C this is `paused=true`, so allocations don't auto-
    /// register and `step` is a no-op. Phase D-1d wires `unpause()` after
    /// state initialization, at which point `step` runs during VM dispatch.
    pub heap: lua_gc::Heap,
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
    pub fn set_gc_stop_user(&mut self) {
        // C: g->gcstp = GCSTPUSR;  (lapi.c:1143)
        // GCSTPUSR (lgc.h:155) = 1 — bit set when GC is stopped by user (lua_gc(L, LUA_GCSTOP)).
        self.gcstp = GCSTPUSR;
    }
    pub fn clear_gc_stop(&mut self) { self.gcstp = 0; }
    /// True when the GC has been disabled internally (state setup, mid-GC,
    /// or while closing); user-stop via `collectgarbage("stop")` does NOT
    /// set this bit, so `lua_gc` continues to honour Count/Step/etc.
    ///
    /// C: `g->gcstp & GCSTPGC` (lapi.c:1137).
    pub fn is_gc_stopped_internally(&self) -> bool { (self.gcstp & GCSTPGC) != 0 }

    /// Returns the interned `__xxx` name string for tag method `tm`, or
    /// `None` if `tmname` has not yet been initialised (early bootstrap).
    ///
    /// C: `G(L)->tmname[tm]` (lookup via macro in ltm.h).
    /// macros.tsv: `getshrstr(G(L)->tmname[tm]) → g.tm_name(tm)`.
    ///
    /// PORT NOTE: The lua-vm crate carries two distinct `TagMethod` enums
    /// (one in `lua-types`, one in `crate::tagmethods`) with identical
    /// `#[repr(u8)]` ordering. The [`TmIndex`] trait bridges them so callers
    /// from either side can index `tmname` uniformly.
    pub fn tm_name<T: TmIndex>(&self, tm: T) -> Option<GcRef<LuaString>> {
        self.tmname.get(tm.tm_index()).cloned()
    }
}

/// Discriminant-to-index conversion for the two parallel `TagMethod` enums.
///
/// Both `lua_types::tagmethod::TagMethod` and `crate::tagmethods::TagMethod`
/// are `#[repr(u8)]` with the same ORDER TM layout, so casting through `u8`
/// yields the correct `GlobalState.tmname` index for either type.
pub trait TmIndex: Copy {
    fn tm_index(self) -> usize;
}
impl TmIndex for lua_types::tagmethod::TagMethod {
    fn tm_index(self) -> usize { self as u8 as usize }
}
impl TmIndex for crate::tagmethods::TagMethod {
    fn tm_index(self) -> usize { self as u8 as usize }
}
impl TmIndex for usize {
    fn tm_index(self) -> usize { self }
}
impl TmIndex for u8 {
    fn tm_index(self) -> usize { self as usize }
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

    /// Stack of yield buffers used by the Phase A–D buffering coroutine
    /// emulation. Each `coroutine.wrap` invocation pushes an empty buffer
    /// before running the wrapped function, and pops it when the function
    /// returns. While a buffer is on top, `coroutine.yield(v...)` appends
    /// its arguments instead of suspending. Phase E replaces this with the
    /// real stackful coroutine runtime (corosensei).
    pub yield_buffers: Vec<Vec<LuaValue>>,
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

    /// Push an empty yield buffer onto the buffer stack. Subsequent
    /// `coroutine.yield(v...)` calls will append into this buffer instead
    /// of suspending. Part of the Phase A–D buffering coroutine emulation;
    /// Phase E replaces this with real corosensei coroutines.
    pub fn push_yield_buffer(&mut self) {
        self.yield_buffers.push(Vec::new());
    }

    /// Pop the top yield buffer, returning its accumulated values.
    /// Caller must ensure the buffer stack is non-empty (i.e. a matching
    /// `push_yield_buffer` was issued earlier).
    pub fn pop_yield_buffer(&mut self) -> Vec<LuaValue> {
        self.yield_buffers
            .pop()
            .expect("pop_yield_buffer with empty buffer stack")
    }

    /// Returns `true` if a yield buffer is currently active (a wrap'd
    /// coroutine body is running).
    pub fn has_yield_buffer(&self) -> bool {
        !self.yield_buffers.is_empty()
    }

    /// Append a value to the top yield buffer. Caller must ensure the
    /// buffer stack is non-empty (typically guarded by `has_yield_buffer`).
    pub fn yield_buffer_push(&mut self, v: LuaValue) {
        let n = self.yield_buffers.len();
        self.yield_buffers[n - 1].push(v);
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
    /// In C, short strings (≤ LUAI_MAXSHORTLEN = 40 bytes) are interned globally
    /// via `luaS_newlstr`, while long strings allocate a fresh TString each
    /// call so distinct long strings keep distinct object identity (observable
    /// via `string.format("%p", s)`). The parser separately deduplicates
    /// long-string literals within a single chunk through `luaX_newstring`'s
    /// `ls->h` anchor table.
    ///
    /// C: `luaS_newlstr` (and `luaS_new`, which calls `luaS_newlstr`)
    /// macros.tsv: `luaS_new → state.intern_str(s)`
    pub fn intern_str(&mut self, bytes: &[u8]) -> Result<GcRef<LuaString>, LuaError> {
        if bytes.len() <= crate::string::MAX_SHORT_LEN {
            if let Some(existing) = self.global().interned_lt.get(bytes) {
                return Ok(existing.clone());
            }
            let _local = crate::string::new(self, bytes)?;
            let new_ref = GcRef::new(LuaString::from_bytes(bytes.to_vec()));
            self.global_mut()
                .interned_lt
                .insert(bytes.to_vec().into_boxed_slice(), new_ref.clone());
            Ok(new_ref)
        } else {
            let new_ref = GcRef::new(LuaString::from_bytes(bytes.to_vec()));
            // PORT NOTE: Phase-B byte tracking for `collectgarbage("count")`.
            // C-Lua's `luaC_newobj` calls `luaM_malloc`, which adds
            // `sizeof(TString) + len + 1` to `g->GCdebt`. Phases A–C bypass
            // that allocator, so without explicit accounting the Lua-visible
            // memory total never reflects string payload — gc.lua's
            // string-keys-in-weak-tables block depends on observing the >8MB
            // jump after allocating two 4MB strings. Short strings are
            // interned (bounded in size) so they are not tracked here.
            // `reclaim_dead_long_strings` later subtracts the size back out
            // when the underlying `Rc` is dropped.
            let size = bytes.len()
                + std::mem::size_of::<LuaString>()
                + std::mem::size_of::<usize>();
            let mut g = self.global_mut();
            g.gc_debt += size as isize;
            g.gc_tracked_long_strings
                .push((new_ref.downgrade(), size));
            Ok(new_ref)
        }
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
    /// Set `top` to an absolute stack index. Grows the backing stack vector
    /// (filling new slots with `Nil`) when `idx` is past `stack.len()`, but
    /// never clobbers existing slots between the old top and the new top —
    /// VM opcodes (Call, ForPrep, etc.) write registers via `set_at` and then
    /// raise `top` to signal "these are now live"; nil-filling here would
    /// erase the just-written values.
    ///
    /// C: internal `L->top.p = newtop` assignment. The `for (; diff > 0; …)
    /// setnilvalue(s2v(L->top.p++))` clear loop in `lua_settop` (lapi.c) is
    /// part of the public API path and lives in `api::set_top` instead.
    /// PORT NOTE: callers pass an absolute `StackIdx`, not the relative `idx`
    /// of the public `lua_settop`. The to-be-closed (`tbclist`) close path
    /// is Phase E and not handled here.
    pub fn set_top(&mut self, idx: impl Into<StackIdxConv>) {
        let new_top: StackIdx = idx.into().0;
        let new_top_u = new_top.0 as usize;
        if new_top_u > self.stack.len() {
            self.stack.resize_with(new_top_u, StackValue::default);
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
    /// Decrement `top` by 1 (saturating at zero).
    ///
    /// C: `L->top.p--` — drop one slot from the stack without reading it.
    pub fn dec_top(&mut self) {
        if self.top.0 > 0 {
            self.top = StackIdx(self.top.0 - 1);
        }
    }
    pub fn pop_n(&mut self, n: usize) {
        let cur = self.top.0 as usize;
        let new = cur.saturating_sub(n);
        self.top = StackIdx(new as u32);
    }
    /// Returns the value at the given stack index without removing it.
    ///
    /// C: `s2v(L->stack.p + idx)` for a fixed absolute index.
    pub fn peek_at(&mut self, idx: impl Into<StackIdxConv>) -> LuaValue {
        let i: StackIdx = idx.into().0;
        match self.stack.get(i.0 as usize) {
            Some(slot) => slot.val.clone(),
            None => LuaValue::Nil,
        }
    }
    /// Returns the value just below `top` (the topmost live slot) without
    /// removing it.
    ///
    /// C: `s2v(L->top.p - 1)`.
    pub fn peek_top(&mut self) -> LuaValue {
        if self.top.0 == 0 {
            return LuaValue::Nil;
        }
        self.stack[(self.top.0 - 1) as usize].val.clone()
    }
    /// Returns the topmost slot interpreted as a string. Panics if the slot
    /// is not a `LuaValue::Str`. Callers (e.g. `luaO_pushvfstring`) guarantee
    /// the value has been pushed as an interned string immediately prior.
    ///
    /// C: `getstr(tsvalue(s2v(L->top.p - 1)))`.
    pub fn peek_string_at_top(&mut self) -> GcRef<LuaString> {
        match self.peek_top() {
            LuaValue::Str(s) => s,
            _ => panic!("peek_string_at_top: top of stack is not a string"),
        }
    }
    /// Mutable reference to the value at the given stack slot.
    ///
    /// C: `s2v(L->stack.p + idx)` used as an lvalue.
    pub fn stack_at(&mut self, idx: impl Into<StackIdxConv>) -> &mut LuaValue {
        let i: StackIdx = idx.into().0;
        &mut self.stack[i.0 as usize].val
    }
    /// Writes `Nil` to the given stack slot.
    ///
    /// C: `setnilvalue(s2v(L->stack.p + idx))`.
    pub fn stack_set_nil(&mut self, idx: impl Into<StackIdxConv>) {
        let i: StackIdx = idx.into().0;
        let slot = i.0 as usize;
        if slot < self.stack.len() {
            self.stack[slot].val = LuaValue::Nil;
        }
    }
    /// Resizes the underlying stack vector to `size` slots, padding new slots
    /// with `StackValue::default()` (which is `Nil`). Returns `Ok(())` on
    /// success — `Vec::resize_with` in Rust does not have a fallible path the
    /// way `luaM_reallocvector` does in C, so the `Result` is here for
    /// signature parity with future fallible allocators.
    ///
    /// C: `luaM_reallocvector(L, L->stack.p, oldsize+EXTRA_STACK,
    ///                         newsize+EXTRA_STACK, StackValue)`.
    pub fn stack_resize(&mut self, size: usize) -> Result<(), LuaError> {
        self.stack.resize_with(size, StackValue::default);
        Ok(())
    }
    pub fn stack_available(&mut self) -> usize {
        (self.stack_last.0 as usize).saturating_sub(self.top.0 as usize)
    }
    pub fn check_stack(&mut self, n: i32) -> Result<(), LuaError> {
        let free = (self.stack_last.0 as i32) - (self.top.0 as i32);
        if free <= n {
            self.grow_stack(n, true)?;
        }
        Ok(())
    }
    /// Inherent method wrapper around the free function `do_::grow_stack`,
    /// preserving the historical `Result<(), LuaError>` signature used by
    /// `check_stack` and other VM call sites. The bool returned by the
    /// underlying implementation distinguishes soft failure (when
    /// `raise_error` is false) from success; that distinction is dropped here
    /// because every current caller passes `raise_error = true` and only
    /// cares about error propagation.
    ///
    /// C: `int luaD_growstack(lua_State *L, int n, int raiseerror)`.
    pub fn grow_stack(&mut self, n: i32, raise_error: bool) -> Result<(), LuaError> {
        crate::do_::grow_stack(self, n, raise_error).map(|_| ())
    }

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
    pub fn ci_trap(&mut self, idx: CallInfoIdx) -> bool {
        if let CallInfoFrame::Lua { trap, .. } = self.call_info[idx.as_usize()].u {
            trap
        } else {
            false
        }
    }
    pub fn ci_savedpc(&self, idx: CallInfoIdx) -> u32 { self.call_info[idx.as_usize()].saved_pc() }
    pub fn set_ci_savedpc(&mut self, idx: CallInfoIdx, pc: u32) {
        self.call_info[idx.as_usize()].set_saved_pc(pc);
    }
    pub fn set_ci_previous(&mut self, idx: CallInfoIdx) {
        self.ci = self.call_info[idx.as_usize()]
            .previous
            .expect("set_ci_previous: returning frame has no previous CallInfo");
    }
    pub fn ci_previous(&self, idx: CallInfoIdx) -> Option<CallInfoIdx> { self.call_info[idx.as_usize()].previous }
    pub fn ci_adjust_func(&mut self, idx: CallInfoIdx, delta: i32) {
        let ci = &mut self.call_info[idx.as_usize()];
        ci.func = StackIdx((ci.func.0 as i32 - delta) as u32);
    }
    pub fn ci_base(&self, idx: CallInfoIdx) -> StackIdx { self.call_info[idx.as_usize()].func + 1 }
    pub fn ci_is_fresh(&self, idx: CallInfoIdx) -> bool {
        (self.call_info[idx.as_usize()].callstatus & CIST_FRESH) != 0
    }
    pub fn ci_lua_closure(&self, idx: CallInfoIdx) -> Option<GcRef<lua_types::closure::LuaLClosure>> {
        let func_idx = self.call_info[idx.as_usize()].func;
        match self.get_at(func_idx) {
            LuaValue::Function(lua_types::closure::LuaClosure::Lua(cl)) => Some(cl),
            _ => None,
        }
    }
    pub fn ci_nextraargs(&self, idx: CallInfoIdx) -> i32 {
        self.call_info[idx.as_usize()].nextra_args()
    }
    pub fn ci_nres(&self, idx: CallInfoIdx) -> i32 {
        self.call_info[idx.as_usize()].u2.value
    }
    pub fn ci_nres_set(&mut self, idx: CallInfoIdx, n: i32) {
        self.call_info[idx.as_usize()].u2.value = n;
    }
    pub fn ci_nresults(&self, idx: CallInfoIdx) -> i32 { self.call_info[idx.as_usize()].nresults as i32 }
    pub fn ci_prev_instruction(&self, _idx: CallInfoIdx) -> lua_types::opcode::Instruction { todo!("phase-b: ci_prev_instruction") }
    pub fn ci_prev2_instruction(&self, _idx: CallInfoIdx) -> lua_types::opcode::Instruction { todo!("phase-b: ci_prev2_instruction") }
    pub fn ci_skip_next_instruction(&mut self, _idx: CallInfoIdx) { todo!("phase-b: ci_skip_next_instruction") }
    pub fn ci_step_pc_back(&mut self, _idx: CallInfoIdx) { todo!("phase-b: ci_step_pc_back") }
    pub fn get_ci_pcrel(&mut self, _idx: CallInfoIdx) -> u32 { todo!("phase-b: get_ci_pcrel") }
    pub fn get_ci_u2_funcidx(&mut self, idx: CallInfoIdx) -> i32 {
        self.call_info[idx.as_usize()].u2.value
    }
    pub fn get_ci_u2_nres(&mut self, idx: CallInfoIdx) -> i32 {
        self.call_info[idx.as_usize()].u2.value
    }
    pub fn get_ci_u2_nyield(&mut self, idx: CallInfoIdx) -> i32 {
        self.call_info[idx.as_usize()].u2.value
    }
    pub fn get_ci_vararg_info(&mut self, _idx: CallInfoIdx) -> (bool, i32, i32) { todo!("phase-b: get_ci_vararg_info") }
    pub fn get_ci_lua_proto_numparams(&mut self, _idx: CallInfoIdx) -> u8 { todo!("phase-b: get_ci_lua_proto_numparams") }
    pub fn set_ci_u2_nres(&mut self, idx: CallInfoIdx, n: i32) {
        self.call_info[idx.as_usize()].u2.value = n;
    }
    pub fn set_ci_u2_nyield(&mut self, idx: CallInfoIdx, n: i32) {
        self.call_info[idx.as_usize()].u2.value = n;
    }
    pub fn set_ci_transfer_info(&mut self, idx: CallInfoIdx, ftransfer: u16, ntransfer: u16) {
        let ci = &mut self.call_info[idx.as_usize()];
        ci.u2.ftransfer = ftransfer;
        ci.u2.ntransfer = ntransfer;
    }
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

    pub fn new_string(&mut self, bytes: &[u8]) -> Result<GcRef<LuaString>, LuaError> { self.intern_or_create_str(bytes) }

    // ── Phase D-1a: state-owned allocation API ──────────────────────────────
    // These methods are the canonical allocation surface. They wrap
    // `GcRef::new` today; at D-1e they route through `state.global.heap.allocate`.
    // Callers must reach them through `&mut LuaState`, which mirrors C-Lua's
    // requirement that every allocation passes `lua_State *L`.

    /// Allocate a new Lua function prototype.
    ///
    /// Caller mutates the returned proto in place (it's behind GcRef, which is
    /// Rc during Phase D-1; mutable access via `Rc::get_mut` only works while
    /// no other GcRefs alias it — true at construction).
    pub fn new_proto(&mut self) -> GcRef<LuaProto> {
        GcRef::new(LuaProto::placeholder())
    }

    /// Allocate a Lua-side closure (compiled function + upvalue slots).
    pub fn new_lclosure(&mut self, proto: GcRef<LuaProto>, nupvals: usize) -> GcRef<LuaClosureLua> {
        let mut upvals = Vec::with_capacity(nupvals);
        for _ in 0..nupvals {
            upvals.push(std::cell::RefCell::new(self.new_upval_closed(LuaValue::Nil)));
        }
        GcRef::new(LuaClosureLua { proto, upvals })
    }

    /// Allocate a closed upvalue holding the given value.
    pub fn new_upval_closed(&mut self, v: LuaValue) -> GcRef<UpVal> {
        GcRef::new(UpVal::closed(v))
    }

    /// Allocate an open upvalue referring to a thread's stack slot.
    pub fn new_upval_open(&mut self, thread_id: usize, level: StackIdx) -> GcRef<UpVal> {
        GcRef::new(UpVal::open(thread_id, level))
    }
    /// Mirrors `luaS_newlstr`: short strings are interned globally so equal
    /// content shares a single TString; long strings (> LUAI_MAXSHORTLEN = 40)
    /// always create a fresh TString without interning. This is what lets
    /// `string.format("%p", "long" .. "concat")` differ from a same-content
    /// literal — concat must produce a new object even when the literal already
    /// lives in the lexer's constant pool.
    pub fn intern_or_create_str(&mut self, bytes: &[u8]) -> Result<GcRef<LuaString>, LuaError> {
        self.intern_str(bytes)
    }
    pub fn new_userdata(&mut self, _size: usize, _nuvalue: usize) -> Result<GcRef<LuaUserData>, LuaError> { todo!("phase-b: new_userdata") }
    pub fn new_c_closure(&mut self, _f: LuaCFunction, _n: i32) -> Result<LuaClosure, LuaError> { todo!("phase-b: new_c_closure") }
    pub fn push_closure(
        &mut self,
        proto_idx: usize,
        ci: CallInfoIdx,
        base: StackIdx,
        ra: StackIdx,
    ) -> Result<(), LuaError> {
        let parent_cl = self.ci_lua_closure(ci).expect(
            "push_closure: current frame is not a Lua closure",
        );
        let child_proto = parent_cl.proto.p[proto_idx].clone();
        let nup = child_proto.upvalues.len();
        let mut upvals: Vec<std::cell::RefCell<GcRef<UpVal>>> = Vec::with_capacity(nup);
        for i in 0..nup {
            let desc = &child_proto.upvalues[i];
            let uv = if desc.instack {
                let level = base + desc.idx as i32;
                crate::func::find_upval(self, level)
            } else {
                parent_cl.upval(desc.idx as usize)
            };
            upvals.push(std::cell::RefCell::new(uv));
        }
        // TODO(D-1c-bridge): upvals are pre-populated from parent frame; state.new_lclosure
        // fills with fresh Nil upvals which would drop the captured bindings.
        let new_cl = GcRef::new(LuaClosureLua {
            proto: child_proto,
            upvals,
        });
        self.set_at(ra, LuaValue::Function(LuaClosure::Lua(new_cl)));
        Ok(())
    }
    pub fn new_tbc_upval(&mut self, idx: StackIdx) -> Result<(), LuaError> {
        crate::func::new_tbc_upval(self, idx)
    }

    pub fn upvalue_get(&self, cl: &GcRef<LuaClosureLua>, n: usize) -> LuaValue {
        let uv = cl.upval(n);
        let slot = uv.slot().clone();
        match slot {
            lua_types::UpValState::Closed(v) => v,
            lua_types::UpValState::Open { thread_id: _, idx } => self.stack[idx.0 as usize].val.clone(),
        }
    }
    pub fn upvalue_set(&mut self, cl: &GcRef<LuaClosureLua>, n: usize, val: LuaValue) -> Result<(), LuaError> {
        let uv = cl.upval(n);
        let slot_idx = match &*uv.slot() {
            lua_types::UpValState::Open { idx, .. } => Some(*idx),
            lua_types::UpValState::Closed(_) => None,
        };
        match slot_idx {
            Some(idx) => self.set_at(idx, val),
            None => {
                let mut g = uv.state.borrow_mut();
                *g = lua_types::UpValState::Closed(val);
            }
        }
        Ok(())
    }

    pub fn protected_call_raw(&mut self, func: StackIdx, nresults: i32, errfunc: StackIdx) -> Result<(), LuaError> {
        let ef = errfunc.0 as isize;
        let status = crate::do_::pcall(
            self,
            |s| s.call_no_yield(func, nresults),
            func,
            ef,
        );
        match status {
            LuaStatus::Ok => Ok(()),
            LuaStatus::ErrSyntax => {
                let err_val = self.get_at(func);
                self.set_top(func);
                Err(LuaError::Syntax(err_val))
            }
            LuaStatus::Yield => {
                self.set_top(func);
                Err(LuaError::Yield)
            }
            _ => {
                let err_val = self.get_at(func);
                self.set_top(func);
                Err(LuaError::Runtime(err_val))
            }
        }
    }
    pub fn protected_parser(&mut self, z: crate::zio::ZIO, name: &[u8], mode: Option<&[u8]>) -> LuaStatus {
        crate::do_::protected_parser(self, z, name, mode)
    }
    pub fn do_call(&mut self, func: StackIdx, nresults: i32) -> Result<(), LuaError> {
        crate::do_::call(self, func, nresults)
    }
    pub fn do_call_no_yield(&mut self, func: StackIdx, nresults: i32) -> Result<(), LuaError> {
        crate::do_::callnoyield(self, func, nresults)
    }
    pub fn call_no_yield(&mut self, func: StackIdx, nresults: i32) -> Result<(), LuaError> {
        crate::do_::callnoyield(self, func, nresults)
    }
    pub fn call_at(&mut self, func: StackIdx, nresults: i32) -> Result<(), LuaError> {
        crate::do_::call(self, func, nresults)
    }
    pub fn precall(&mut self, func: StackIdx, nresults: i32) -> Result<Option<CallInfoIdx>, LuaError> {
        crate::do_::precall(self, func, nresults)
    }
    pub fn pretailcall(
        &mut self,
        ci: CallInfoIdx,
        func: StackIdx,
        narg1: i32,
        delta: i32,
    ) -> Result<i32, LuaError> {
        crate::do_::pretailcall(self, ci, func, narg1, delta)
    }
    pub fn poscall<N: TryInto<i32>>(&mut self, ci: CallInfoIdx, nres: N) -> Result<(), LuaError>
    where
        <N as TryInto<i32>>::Error: std::fmt::Debug,
    {
        let n = nres.try_into().expect("poscall: nres out of i32 range");
        crate::do_::poscall(self, ci, n)
    }
    pub fn adjust_results(&mut self, nresults: i32) {
        const LUA_MULTRET: i32 = -1;
        if nresults <= LUA_MULTRET {
            let ci_idx = self.ci.as_usize();
            if self.call_info[ci_idx].top.0 < self.top.0 {
                self.call_info[ci_idx].top = self.top;
            }
        }
    }
    pub fn adjust_varargs(
        &mut self,
        ci: CallInfoIdx,
        nfixparams: i32,
        cl: &GcRef<lua_types::closure::LuaLClosure>,
    ) -> Result<(), LuaError> {
        crate::tagmethods::adjust_varargs(self, nfixparams, ci, &cl.0.proto)
    }
    pub fn get_varargs(
        &mut self,
        ci: CallInfoIdx,
        ra: StackIdx,
        n: i32,
    ) -> Result<i32, LuaError> {
        crate::tagmethods::get_varargs(self, ci, ra, n)?;
        Ok(0)
    }

    pub fn close_upvals(&mut self, level: StackIdx) -> Result<(), LuaError> {
        crate::func::close_upval(self, level);
        Ok(())
    }
    pub fn close_upvals_status(&mut self, level: StackIdx, _status: i32) -> Result<(), LuaError> {
        crate::func::close_upval(self, level);
        Ok(())
    }
    pub fn close_upvals_from_base(&mut self, ci: CallInfoIdx) -> Result<(), LuaError> {
        let base = self.ci_base(ci);
        crate::func::close_upval(self, base);
        Ok(())
    }

    pub fn arith_op(&mut self, op: i32, p1: &LuaValue, p2: &LuaValue) -> Result<LuaValue, LuaError> {
        let arith_op = match op {
            0  => lua_types::arith::ArithOp::Add,
            1  => lua_types::arith::ArithOp::Sub,
            2  => lua_types::arith::ArithOp::Mul,
            3  => lua_types::arith::ArithOp::Mod,
            4  => lua_types::arith::ArithOp::Pow,
            5  => lua_types::arith::ArithOp::Div,
            6  => lua_types::arith::ArithOp::Idiv,
            7  => lua_types::arith::ArithOp::Band,
            8  => lua_types::arith::ArithOp::Bor,
            9  => lua_types::arith::ArithOp::Bxor,
            10 => lua_types::arith::ArithOp::Shl,
            11 => lua_types::arith::ArithOp::Shr,
            12 => lua_types::arith::ArithOp::Unm,
            13 => lua_types::arith::ArithOp::Bnot,
            _  => return Err(LuaError::runtime(format_args!("invalid arith op {}", op))),
        };
        let mut res = LuaValue::Nil;
        if crate::object::raw_arith(self, arith_op, p1, p2, &mut res)? {
            Ok(res)
        } else {
            Err(LuaError::arith_error(p1, p2, "perform arithmetic on"))
        }
    }
    pub fn concat(&mut self, n: i32) -> Result<(), LuaError> {
        crate::vm::concat(self, n)
    }
    pub fn less_than(&mut self, l: &LuaValue, r: &LuaValue) -> Result<bool, LuaError> {
        crate::vm::less_than(self, l, r)
    }
    pub fn less_equal(&mut self, l: &LuaValue, r: &LuaValue) -> Result<bool, LuaError> {
        crate::vm::less_equal(self, l, r)
    }
    pub fn equal_obj(&self, _ctx: Option<&LuaValue>, l: &LuaValue, r: &LuaValue) -> bool {
        crate::vm::equal_obj(None, l, r).unwrap_or(false)
    }
    pub fn equal_obj_with_tm(&mut self, l: &LuaValue, r: &LuaValue) -> Result<bool, LuaError> {
        crate::vm::equal_obj(Some(self), l, r)
    }
    pub fn obj_len(&mut self, v: &LuaValue) -> Result<LuaValue, LuaError> {
        match v {
            LuaValue::Table(_) => {
                let mt = self.table_metatable(v);
                let tm = self.fast_tm_table(mt.as_ref(), TagMethod::Len);
                if matches!(tm, LuaValue::Nil) {
                    let n = self.table_length(v)?;
                    return Ok(LuaValue::Int(n));
                }
                self.push(LuaValue::Nil);
                let slot = StackIdx(self.top.0 - 1);
                crate::tagmethods::call_tm_res(self, tm, v.clone(), v.clone(), slot)?;
                Ok(self.pop())
            }
            LuaValue::Str(s) => Ok(LuaValue::Int(s.len() as i64)),
            other => {
                let tm = crate::tagmethods::get_tm_by_obj(self, other, crate::tagmethods::TagMethod::Len);
                if matches!(tm, LuaValue::Nil) {
                    return Err(LuaError::type_error(other, "get length of"));
                }
                self.push(LuaValue::Nil);
                let slot = StackIdx(self.top.0 - 1);
                crate::tagmethods::call_tm_res(self, tm, v.clone(), v.clone(), slot)?;
                Ok(self.pop())
            }
        }
    }
    pub fn obj_to_string(&mut self, idx: i32) -> Result<GcRef<LuaString>, LuaError> {
        let slot: StackIdx = if idx > 0 {
            let ci_func = self.current_call_info().func;
            ci_func + idx
        } else {
            debug_assert!(idx != 0, "invalid index");
            StackIdx((self.top_idx().0 as i32 + idx) as u32)
        };
        let val = self.get_at(slot);
        match val {
            LuaValue::Str(s) => Ok(s),
            LuaValue::Int(_) | LuaValue::Float(_) => {
                let s = crate::object::num_to_string(self, &val)?;
                self.set_at(slot, LuaValue::Str(s.clone()));
                Ok(s)
            }
            _ => Err(LuaError::type_error(&val, "convert to string")),
        }
    }
    pub fn coerce_to_string(&mut self, idx: StackIdx) -> Result<GcRef<LuaString>, LuaError> {
        let val = self.get_at(idx);
        match val {
            LuaValue::Str(s) => Ok(s),
            LuaValue::Int(_) | LuaValue::Float(_) => {
                let s = crate::object::num_to_string(self, &val)?;
                self.set_at(idx, LuaValue::Str(s.clone()));
                Ok(s)
            }
            _ => Err(LuaError::type_error(&val, "convert to string")),
        }
    }
    pub fn str_to_num(&mut self, s: &[u8]) -> Option<(LuaValue, usize)> {
        let mut out = LuaValue::Nil;
        let sz = crate::object::str2num(s, &mut out);
        if sz == 0 { None } else { Some((out, sz)) }
    }

    pub fn fast_get(&mut self, t: &LuaValue, k: &LuaValue) -> Result<Option<LuaValue>, LuaError> {
        let LuaValue::Table(tbl) = t else { return Ok(None); };
        let v = tbl.get(k);
        if matches!(v, LuaValue::Nil) { Ok(None) } else { Ok(Some(v)) }
    }
    pub fn fast_get_int(&mut self, t: &LuaValue, k: i64) -> Result<Option<LuaValue>, LuaError> {
        let LuaValue::Table(tbl) = t else { return Ok(None); };
        let v = tbl.get_int(k);
        if matches!(v, LuaValue::Nil) { Ok(None) } else { Ok(Some(v)) }
    }
    pub fn fast_get_short_str(&mut self, t: &LuaValue, k: &LuaValue) -> Result<Option<LuaValue>, LuaError> {
        let LuaValue::Table(tbl) = t else { return Ok(None); };
        let LuaValue::Str(s) = k else { return Ok(None); };
        let v = tbl.get_short_str(s);
        if matches!(v, LuaValue::Nil) { Ok(None) } else { Ok(Some(v)) }
    }
    pub fn fast_tm_table(&mut self, t: Option<&GcRef<LuaTable>>, tm: TagMethod) -> LuaValue {
        let Some(mt) = t else { return LuaValue::Nil; };
        debug_assert!((tm as u8) <= TagMethod::Eq as u8);
        let ename = self.global().tmname[tm as usize].clone();
        mt.get_short_str(&ename)
    }
    pub fn fast_tm_ud(&mut self, u: &GcRef<LuaUserData>, tm: TagMethod) -> LuaValue {
        // C: fasttm(L, uvalue(o)->metatable, event) — read the userdata's
        // metatable then index by the interned `__xxx` name.
        let mt = u.metatable();
        self.fast_tm_table(mt.as_ref(), tm)
    }

    pub fn table_get_with_tm(&mut self, t: &LuaValue, k: &LuaValue) -> Result<LuaValue, LuaError> {
        if let Some(v) = self.fast_get(t, k)? {
            return Ok(v);
        }
        let res = self.top_idx();
        self.push(LuaValue::Nil);
        crate::vm::finish_get(self, t.clone(), k.clone(), res, true, None)?;
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
        crate::vm::finish_set(self, t.clone(), k, v, true, None)
    }
    pub fn table_raw_set(&mut self, t: &LuaValue, k: LuaValue, v: LuaValue) -> Result<(), LuaError> {
        let LuaValue::Table(tbl) = t else {
            return Err(LuaError::type_error(t, "index"));
        };
        let tbl = tbl.clone();
        tbl.raw_set(self, &k, v)
    }
    pub fn table_array_set(&mut self, t: &LuaValue, idx: usize, v: LuaValue) -> Result<(), LuaError> {
        let LuaValue::Table(tbl) = t else {
            return Err(LuaError::type_error(t, "index"));
        };
        let tbl = tbl.clone();
        tbl.raw_set_int(self, idx as i64 + 1, v)
    }
    pub fn table_ensure_array<T>(&mut self, _t: T, _n: usize) -> Result<(), LuaError> {
        // PORT NOTE: C's luaH_resizearray preallocates the table's contiguous
        // array region (h->array). Phase B's LuaTable (lua-types/src/value.rs)
        // is a single Vec<(K,V)> placeholder with no separate array part, so
        // there is nothing to preallocate — actual storage happens lazily in
        // table_array_set via raw_set. The rich array+hash impl in
        // crates/lua-vm/src/table.rs lights up in Phase D.
        Ok(())
    }
    pub fn table_length(&mut self, t: &LuaValue) -> Result<i64, LuaError> {
        let LuaValue::Table(tbl) = t else {
            return Err(LuaError::type_error(t, "get length of"));
        };
        // PORT NOTE: C's `luaH_getn` returns a boundary i such that t[i] is
        // present and t[i+1] is absent (or 0 if t[1] is absent), exploiting the
        // hybrid array+hash layout. Phase B's LuaTable is a flat Vec<(K,V)> with
        // no array part, so we linearly probe integer keys starting at 1. The
        // rich impl in crates/lua-vm/src/table.rs lights up in Phase D.
        // PERF(port): O(n) linear scan with O(n) lookups → O(n²); Phase D fixes.
        let mut i: i64 = 1;
        loop {
            let v = tbl.get_int(i);
            if matches!(v, LuaValue::Nil) {
                return Ok(i - 1);
            }
            i += 1;
        }
    }
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
    pub fn table_resize(&mut self, _t: &GcRef<LuaTable>, _na: usize, _nh: usize) -> Result<(), LuaError> {
        // PORT NOTE: Phase B's LuaTable (lua-types/src/value.rs) is a single
        // Vec<(K,V)> placeholder with no separate array/hash parts, so the
        // OP_NEWTABLE pre-sizing hint has nothing to act on. The rich
        // array+hash impl in crates/lua-vm/src/table.rs lights up in Phase D.
        Ok(())
    }
    pub fn table_getn(&self, t: &GcRef<LuaTable>) -> i64 {
        // PORT NOTE: C's `luaH_getn` returns a boundary i such that t[i] is
        // present and t[i+1] is absent (or 0 if t[1] is absent), exploiting the
        // hybrid array+hash layout. Phase B's LuaTable (lua-types/src/value.rs)
        // is a flat Vec<(K,V)> with no array part, so we linearly probe integer
        // keys starting at 1. The rich array+hash impl in
        // crates/lua-vm/src/table.rs lights up in Phase D.
        // PERF(port): O(n) linear scan with O(n) lookups → O(n²); Phase D fixes.
        let mut i: i64 = 1;
        loop {
            let v = t.get_int(i);
            if matches!(v, LuaValue::Nil) {
                return i - 1;
            }
            i += 1;
        }
    }

    pub fn try_bin_tm(&mut self, p1: &LuaValue, p1_idx: Option<StackIdx>, p2: &LuaValue, p2_idx: Option<StackIdx>, res: StackIdx, tm: lua_types::tagmethod::TagMethod) -> Result<(), LuaError> {
        let event = crate::tagmethods::TagMethod::from_u8(tm as u8);
        crate::tagmethods::try_bin_tm(self, p1, p1_idx, p2, p2_idx, res, event)
    }
    pub fn try_bin_i_tm(&mut self, p1: &LuaValue, p1_idx: Option<StackIdx>, imm: i64, flip: bool, res: StackIdx, tm: lua_types::tagmethod::TagMethod) -> Result<(), LuaError> {
        let event = crate::tagmethods::TagMethod::from_u8(tm as u8);
        crate::tagmethods::try_bini_tm(self, p1, p1_idx, imm, flip, res, event)
    }
    pub fn try_bin_assoc_tm(&mut self, p1: &LuaValue, p1_idx: Option<StackIdx>, p2: &LuaValue, p2_idx: Option<StackIdx>, flip: bool, res: StackIdx, tm: lua_types::tagmethod::TagMethod) -> Result<(), LuaError> {
        let event = crate::tagmethods::TagMethod::from_u8(tm as u8);
        crate::tagmethods::try_bin_assoc_tm(self, p1, p1_idx, p2, p2_idx, flip, res, event)
    }
    pub fn try_concat_tm(&mut self, _p1: &LuaValue, _p2: &LuaValue) -> Result<(), LuaError> {
        crate::tagmethods::try_concat_tm(self)
    }
    pub fn call_tm(&mut self, f: LuaValue, p1: &LuaValue, p2: &LuaValue, p3: &LuaValue) -> Result<(), LuaError> {
        crate::tagmethods::call_tm(self, f, p1.clone(), p2.clone(), p3.clone())
    }
    pub fn call_tm_res(&mut self, f: LuaValue, p1: &LuaValue, p2: &LuaValue, res: StackIdx) -> Result<(), LuaError> {
        crate::tagmethods::call_tm_res(self, f, p1.clone(), p2.clone(), res)
    }
    pub fn call_tm_res_bool(&mut self, f: LuaValue, p1: &LuaValue, p2: &LuaValue) -> Result<bool, LuaError> {
        let res = self.top_idx();
        self.push(LuaValue::Nil);
        crate::tagmethods::call_tm_res(self, f, p1.clone(), p2.clone(), res)?;
        let result = self.get_at(res).clone();
        self.pop();
        Ok(!matches!(result, LuaValue::Nil | LuaValue::Bool(false)))
    }
    pub fn call_order_tm(&mut self, p1: &LuaValue, p2: &LuaValue, tm: lua_types::tagmethod::TagMethod) -> Result<bool, LuaError> {
        let event = crate::tagmethods::TagMethod::from_u8(tm as u8);
        crate::tagmethods::call_order_tm(self, p1, p2, event)
    }
    pub fn call_order_i_tm(&mut self, p1: &LuaValue, v2: i64, flip: bool, isfloat: bool, tm: lua_types::tagmethod::TagMethod) -> Result<bool, LuaError> {
        let event = crate::tagmethods::TagMethod::from_u8(tm as u8);
        crate::tagmethods::call_orderi_tm(self, p1, v2 as i32, flip, isfloat, event)
    }

    pub fn proto_code(&mut self, cl: &GcRef<lua_types::closure::LuaLClosure>, pc: u32) -> lua_types::opcode::Instruction {
        cl.proto.code[pc as usize]
    }
    pub fn proto_const(&mut self, cl: &GcRef<lua_types::closure::LuaLClosure>, idx: usize) -> LuaValue {
        cl.proto.k[idx].clone()
    }
    pub fn get_proto_instr<T, P>(&mut self, _ci: T, _pc: P) -> lua_types::opcode::Instruction { todo!("phase-b: get_proto_instr") }
    pub fn dump_proto(&self, _proto: &GcRef<LuaProto>, _writer: &mut dyn FnMut(&[u8]) -> Result<(), LuaError>, _strip: bool) -> Result<(), LuaError> { todo!("phase-b: dump_proto") }

    pub fn trace_call(&mut self, _idx: CallInfoIdx) -> Result<bool, LuaError> { todo!("phase-b: trace_call") }
    pub fn trace_exec(&mut self, _idx: CallInfoIdx, _pc: u32) -> Result<bool, LuaError> { todo!("phase-b: trace_exec") }
    pub fn hook_call(&mut self, _idx: CallInfoIdx) -> Result<(), LuaError> { todo!("phase-b: hook_call_idx") }
    pub fn gc_check_step(&mut self) {
        // Phase-B: drive the __gc finalizer queue from common allocation
        // sites so loops like `repeat u = {} until finish` make progress
        // without an explicit `collectgarbage()`. No-op when the queue is
        // empty (the common case).
        if !self.global().pending_finalizers.is_empty() {
            crate::api::run_pending_finalizers(self);
        }
    }
    pub fn gc_cond_step(&mut self) {
        if !self.global().pending_finalizers.is_empty() {
            crate::api::run_pending_finalizers(self);
        }
    }
    pub fn gc_barrier_back<T, U>(&mut self, _t: T, _v: U) { /* phase-b no-op */ }
    pub fn gc_barrier_upval<T, U, V>(&mut self, _cl: T, _uv: U, _v: V) { /* phase-b no-op */ }
    /// C: `(G(L)->mainthread == L)` — true if `self` is the main thread.
    ///
    /// PORT NOTE: In C this is a pointer-identity check against the global
    /// state's `mainthread` slot. In Phase B we have no GcRef cycle from the
    /// main thread to itself (see TODO at `state_open`) and coroutines are
    /// stubbed (design decision #6), so the only running `LuaState` is the
    /// main thread. The `mainthread` slot in `GlobalState` is `None` while
    /// the cycle is unsupported; treat that as "self is the main thread".
    /// Once Phase E wires real coroutines, this must compare a GcRef pointer
    /// against `global.mainthread`.
    pub fn is_main_thread(&mut self) -> bool {
        self.global().mainthread.is_none()
    }
    pub fn obj_type_name(&self, v: &LuaValue) -> &'static [u8] {
        crate::tagmethods::type_name(v.base_type())
    }
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
        // Phase D: trigger real mark-and-sweep. Holds an immutable borrow of
        // GlobalState as both the heap-owner and the root set (heap is
        // interior-mutable; roots are read-only). The Trace impl for
        // GlobalState walks its registry/stringpool/etc.; each unimplemented
        // sub-Trace fires its `todo!("phase-d:")` panic so the mega-loop
        // surfaces it as work.
        let global = self._state.global.borrow();
        global.heap.unpause();
        global.heap.full_collect(&*global);
    }

    /// Phase-B stub for `luaC_step(L)`.
    pub fn step(&self) { /* phase-b no-op */ }

    /// Set the GC kind (incremental/generational).
    ///
    /// C: `luaC_changemode(L, newmode)` in `lgc.c` — in Phases A–C the heap
    /// itself is `Rc`-based, so the only observable effect is the mode flag
    /// returned by `lua_gc(LUA_GCGEN)` / `lua_gc(LUA_GCINC)` on the next call.
    pub fn change_mode(&self, mode: GcKind) {
        self._state.global_mut().gckind = mode as u8;
    }

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

/// Sweep the Phase-B long-string tracker and decrement `gc_debt` by the
/// recorded byte count of any entry whose underlying `Rc` has been dropped.
///
/// PORT NOTE: Phase D will replace this with the real allocator's per-object
/// accounting through `luaM_realloc`. For now, long-string creation pushes a
/// `(Weak, size)` pair onto `gc_tracked_long_strings`, and this helper
/// reclaims the bytes lazily — at every `collectgarbage("count")` query and
/// at the end of `collectgarbage("collect")` — so the Lua-visible memory
/// total reflects live string bytes rather than peak allocation.
pub(crate) fn reclaim_dead_long_strings(g: &mut GlobalState) {
    let mut freed: isize = 0;
    g.gc_tracked_long_strings.retain(|(w, sz)| {
        if w.strong_count() == 0 {
            freed += *sz as isize;
            false
        } else {
            true
        }
    });
    g.gc_debt -= freed;
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
    crate::tagmethods::init(state)?;
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
        yield_buffers: Vec::new(),
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
    // TODO(D-1c-bridge): no state.new_thread helper; wraps a LuaState directly
    let _thread_ref: GcRef<LuaState> = GcRef::new(new_thread);

    // C: setthvalue2s(L, L->top.p, L1);
    // macros.tsv: setthvalue2s → state.set_at(o, LuaValue::Thread(th.clone()))
    // C: api_incr_top(L); → state.push() already increments
    // TODO(phase-b): LuaValue::Thread expects GcRef<lua_types::value::LuaThread>;
    // the rich LuaState lives in lua-vm and the two have not been unified. Pushing
    // a placeholder for now so Phase A state construction can compile.
    // TODO(D-1c-bridge): no state.new_thread_value helper; LuaThread placeholder
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
    // TODO(D-1c-bridge): allocation outside state context (new_state() free fn — no LuaState yet)
    let placeholder_str = GcRef::new(LuaString::placeholder());

    // C: g->currentwhite = bitmask(WHITE0BIT);
    // macros.tsv: bitmask → (1u32 << b); WHITE0BIT = 0 → 1u8
    let initial_white = 1u8 << WHITE0BIT;

    // C: setivalue(&g->nilvalue, 0);  /* to signal that state is not yet built */
    // macros.tsv: setivalue → *o = LuaValue::Int(x)
    // PORT NOTE: non-nil nilvalue signals "state not yet complete"; see is_complete().

    let global = GlobalState {
        parser_hook: None,
        file_loader_hook: None,
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
        sweepgc_cursor: 0,
        weak_tables_registry: Vec::new(),
        gc_tracked_long_strings: Vec::new(),
        pending_finalizers: Vec::new(),
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
        interned_lt: std::collections::HashMap::new(),
        warnf: None,
        c_functions: Vec::new(),
        heap: lua_gc::Heap::new(),
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
        yield_buffers: Vec::new(),
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
