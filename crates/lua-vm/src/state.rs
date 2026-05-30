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
#[inline(always)]
pub fn stack_idx_to_i32(i: StackIdx) -> i32 { i.0 as i32 }

impl From<u32> for StackIdxConv {
    #[inline(always)]
    fn from(v: u32) -> Self { StackIdxConv(StackIdx(v)) }
}
impl From<i32> for StackIdxConv {
    #[inline(always)]
    fn from(v: i32) -> Self { StackIdxConv(StackIdx(v.max(0) as u32)) }
}
impl From<usize> for StackIdxConv {
    #[inline(always)]
    fn from(v: usize) -> Self { StackIdxConv(StackIdx(v as u32)) }
}
impl From<StackIdx> for StackIdxConv {
    #[inline(always)]
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

pub type LuaRustFunction = Rc<dyn Fn(&mut LuaState) -> Result<usize, LuaError>>;

#[derive(Clone)]
pub enum LuaCallable {
    Bare(LuaCFunction),
    Rust(LuaRustFunction),
}

impl std::fmt::Debug for LuaCallable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LuaCallable::Bare(_) => f.write_str("LuaCallable::Bare(..)"),
            LuaCallable::Rust(_) => f.write_str("LuaCallable::Rust(..)"),
        }
    }
}

impl LuaCallable {
    pub fn bare(f: LuaCFunction) -> Self {
        LuaCallable::Bare(f)
    }

    pub fn rust(f: LuaRustFunction) -> Self {
        LuaCallable::Rust(f)
    }

    pub fn as_bare(&self) -> Option<LuaCFunction> {
        match self {
            LuaCallable::Bare(f) => Some(*f),
            LuaCallable::Rust(_) => None,
        }
    }

    pub fn call(&self, state: &mut LuaState) -> Result<usize, LuaError> {
        match self {
            LuaCallable::Bare(f) => f(state),
            LuaCallable::Rust(f) => f(state),
        }
    }
}

// ─── Constants (from macros.tsv) ──────────────────────────────────────────────

// macros.tsv: EXTRA_STACK → const EXTRA_STACK: u32 = 5
pub(crate) const EXTRA_STACK: usize = 5;

// macros.tsv: LUA_MINSTACK → const LUA_MINSTACK: u32 = 20
pub(crate) const LUA_MINSTACK: usize = 20;

// macros.tsv: BASIC_STACK_SIZE → const BASIC_STACK_SIZE: u32 = 2 * LUA_MINSTACK
pub(crate) const BASIC_STACK_SIZE: usize = 2 * LUA_MINSTACK;

/// Maximum nested non-yielding C-call recursion depth — the single source of
/// truth for the call-depth guard (also used by `do_::ccall_inner` and
/// `do_::lua_resume`).
///
/// This is the structural defense that keeps a recursive interpreter sound for
/// untrusted code: a recursive Rust interpreter consumes host (Rust) stack per
/// nested Lua→Lua call, so unbounded Lua recursion would otherwise overflow the
/// OS thread stack and crash the process. Tripping this limit instead raises a
/// catchable `"stack overflow"` / `"C stack overflow"` Lua error.
///
/// Safe margin: each nested call frame consumes a bounded amount of Rust stack,
/// so `MAXCCALLS` frames fit within the default ~8 MiB thread stack with room to
/// spare — verified on macOS/Linux release builds against deep non-tail
/// recursion, infinite `__index`/`__concat`/`__tostring` metamethod chains, and
/// nested-coroutine `__close` cascades, all of which error cleanly rather than
/// SIGSEGV (see the `recursion_*` sandbox tests). Embedders that run the VM on a
/// smaller thread stack should lower this constant proportionally (roughly
/// `stack_bytes / 40_000`).

pub(crate) const LUAI_MAXCCALLS: u32 = 200;

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
pub(crate) const CIST_RECST: u32 = 10;

// macros.tsv: LUA_NUMTYPES → const LUA_NUMTYPES: usize = 9
const LUA_NUMTYPES: usize = 9;

// TODO(port): import from crate::gc (lgc.c → gc.rs) once it exists in Phase D
const GCSTPUSR: u8 = 1;
const GCSTPGC: u8 = 2;

// TODO(port): import from crate::gc in Phase D
const GCS_PAUSE: u8 = 0;

const LUAI_GCPAUSE: u32 = 200;
const LUAI_GCMUL: u32 = 100;
const LUAI_GCSTEPSIZE: u8 = 13;
const LUAI_GENMAJORMUL: u32 = 100;
const LUAI_GENMINORMUL: u8 = 20;

const WHITE0BIT: u8 = 0;

const STRCACHE_N: usize = 53;
const STRCACHE_M: usize = 2;

// ─── GcKind enum ─────────────────────────────────────────────────────────────

/// Garbage collector operating mode.
///
/// macros.tsv: `KGC_INC → GcKind::Incremental`, `KGC_GEN → GcKind::Generational`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcKind {
    Incremental = 0,
    Generational = 1,
}

// ─── LuaStatus enum ──────────────────────────────────────────────────────────

/// Thread / call status codes.
///
pub use lua_types::status::LuaStatus;

// ─── StackValue ───────────────────────────────────────────────────────────────

/// One slot on the Lua value stack.  Wraps a `LuaValue` and an optional
/// to-be-closed delta (for the `tbclist` mechanism).
///
/// types.tsv: `StackValue → StackValue { val: LuaValue, tbclist.delta: u16 }`
#[derive(Clone)]
pub struct StackValue {
    pub val: LuaValue,
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
/// types.tsv: CallInfo → CallInfo (several fields renamed / adapted).
///
/// The C intrusive doubly-linked list (`previous`, `next` as raw pointers) is
/// replaced by `Option<CallInfoIdx>` indices into `LuaState::call_info`.
#[derive(Clone)]
pub struct CallInfo {
    // types.tsv: CallInfo.func → StackIdx
    pub func: StackIdx,

    // types.tsv: CallInfo.top → StackIdx
    pub top: StackIdx,

    // types.tsv: CallInfo.previous → CallInfoIdx (Option at boundary)
    pub previous: Option<CallInfoIdx>,

    // types.tsv: CallInfo.next → CallInfoIdx (Option at tail)
    pub next: Option<CallInfoIdx>,

    pub u: CallInfoFrame,

    pub u2: CallInfoExtra,

    // types.tsv: CallInfo.nresults → i16
    pub nresults: i16,

    // types.tsv: CallInfo.callstatus → u16 (bit-packed CIST_* flags)
    pub callstatus: u16,
}

/// Payload of `CallInfo.u`.
///
#[derive(Clone, Copy)]
pub enum CallInfoFrame {
    Lua {
        // types.tsv: CallInfo.u.l.savedpc → u32
        savedpc: u32,
        // types.tsv: CallInfo.u.l.trap → bool
        trap: bool,
        // types.tsv: CallInfo.u.l.nextraargs → i32
        nextraargs: i32,
    },
    C {
        // types.tsv: CallInfo.u.c.k → Option<lua_KFunction>
        k: Option<LuaKFunction>,
        // types.tsv: CallInfo.u.c.old_errfunc → isize
        old_errfunc: isize,
        // types.tsv: CallInfo.u.c.ctx → isize
        ctx: isize,
    },
}

/// Continuation function for yieldable C calls.  C: `lua_KFunction`.
pub type LuaKFunction = fn(&mut LuaState, status: i32, ctx: isize) -> Result<usize, LuaError>;

/// Payload of `CallInfo.u2`.
///
/// types.tsv: CallInfo.u2 → CallInfoExtra (Rust: struct with all fields, interpretation by context)
#[derive(Default, Clone, Copy)]
pub struct CallInfoExtra {
    pub value: i32,
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
    /// Whether the active function is a vararg function.
    ///
    /// Currently returns `false` unconditionally — vararg introspection via
    /// `debug.getinfo` reports no vararg info instead of panicking.
    ///
    /// TODO(port): wire when CallInfo carries proto access for vararg detection.
    pub fn is_vararg_func(&self) -> bool { false }
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
    /// Read the 3-bit recover-status field packed into bits 10-12 of callstatus.
    ///
    pub fn recover_status(&self) -> i32 {
        ((self.callstatus >> CIST_RECST) & 7) as i32
    }
    /// Write the 3-bit recover-status field. `status` must fit in three bits.
    ///
    pub fn set_recover_status<T: Into<i32>>(&mut self, status: T) {
        let st = (status.into() & 7) as u16;
        self.callstatus = (self.callstatus & !(7u16 << CIST_RECST)) | (st << CIST_RECST);
    }
    pub fn get_oah(&self) -> bool { (self.callstatus & CIST_OAH) != 0 }
    /// Store the current `allowhook` value into callstatus bit 0 (CIST_OAH).
    ///
    pub fn set_oah(&mut self, allow: bool) {
        self.callstatus = (self.callstatus & !CIST_OAH) | (if allow { CIST_OAH } else { 0 });
    }
    pub fn u_c_old_errfunc(&self) -> isize {
        if let CallInfoFrame::C { old_errfunc, .. } = self.u { old_errfunc } else { 0 }
    }
    pub fn u_c_ctx(&self) -> isize {
        if let CallInfoFrame::C { ctx, .. } = self.u { ctx } else { 0 }
    }
    pub fn u_c_k(&self) -> Option<LuaKFunction> {
        if let CallInfoFrame::C { k, .. } = self.u { k } else { None }
    }
    /// Set continuation function on a C-call frame.
    ///
    /// Panics if invoked on a Lua frame (callers must check `is_lua()` first).
    pub fn set_u_c_k(&mut self, k: Option<LuaKFunction>) {
        if let CallInfoFrame::C { k: ref mut slot, .. } = self.u {
            *slot = k;
        }
    }
    /// Set continuation context on a C-call frame.
    pub fn set_u_c_ctx(&mut self, ctx: isize) {
        if let CallInfoFrame::C { ctx: ref mut slot, .. } = self.u {
            *slot = ctx;
        }
    }
    /// Set saved old_errfunc on a C-call frame.
    pub fn set_u_c_old_errfunc(&mut self, old_errfunc: isize) {
        if let CallInfoFrame::C { old_errfunc: ref mut slot, .. } = self.u {
            *slot = old_errfunc;
        }
    }
    /// Set the `u2.funcidx` field, used by yieldable pcall for error recovery.
    ///
    pub fn set_u2_funcidx(&mut self, idx: i32) {
        self.u2.value = idx;
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
    fn full_type_tag(&self) -> u8 {
        match self {
            LuaValue::Nil => 0x00,
            LuaValue::Bool(false) => 0x01,
            LuaValue::Bool(true) => 0x11,
            LuaValue::Int(_) => 0x03,
            LuaValue::Float(_) => 0x13,
            LuaValue::Str(s) if s.is_short() => 0x04,
            LuaValue::Str(_) => 0x14,
            LuaValue::LightUserData(_) => 0x02,
            LuaValue::Table(_) => 0x05,
            LuaValue::Function(LuaClosure::Lua(_)) => 0x06,
            LuaValue::Function(LuaClosure::LightC(_)) => 0x16,
            LuaValue::Function(LuaClosure::C(_)) => 0x26,
            LuaValue::UserData(_) => 0x07,
            LuaValue::Thread(_) => 0x08,
        }
    }
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
    #[inline(always)]
    fn saturating_sub(self, n: impl Into<StackIdxConv>) -> u32 { self.0.saturating_sub(n.into().0.0) }
    #[inline(always)]
    fn wrapping_sub(self, n: impl Into<StackIdxConv>) -> u32 { self.0.wrapping_sub(n.into().0.0) }
    #[inline(always)]
    fn raw(self) -> u32 { self.0 }
}

/// `GcRef<LuaTable>` / `GcRef<LuaUserData>` field-access helpers. These
/// methods are needed by api.rs and tagmethods.rs but the lua-types
/// placeholders don't yet expose them. TODO(phase-b): replace with real
/// accessor methods on the canonical types in lua-types.
///
/// PORT NOTE: the historical `reject_invalid_table_key` precheck used to
/// guard nil/NaN keys at this layer; it has moved inside
/// [`LuaTable::try_raw_set`] (alongside the integer-fast-path match) so
/// the lua-vm wrapper does not double-check.
pub trait LuaTableRefExt {
    fn metatable(&self) -> Option<GcRef<LuaTable>>;
    fn as_ptr(&self) -> *const ();
    fn get(&self, _k: &LuaValue) -> LuaValue;
    fn get_int(&self, _k: i64) -> LuaValue;
    fn get_short_str(&self, _k: &GcRef<LuaString>) -> LuaValue;
    fn raw_set(&self, _state: &mut LuaState, _k: LuaValue, _v: LuaValue) -> Result<(), LuaError>;
    fn raw_set_int(&self, _state: &mut LuaState, _k: i64, _v: LuaValue) -> Result<(), LuaError>;
    fn invalidate_tm_cache(&self);
    fn resize(&self, _state: &mut LuaState, _na: usize, _nh: usize) -> Result<(), LuaError>;
    fn next(&self, _k: LuaValue) -> Result<Option<(LuaValue, LuaValue)>, LuaError>;
}
impl LuaTableRefExt for GcRef<LuaTable> {
    #[inline]
    fn metatable(&self) -> Option<GcRef<LuaTable>> { (**self).metatable() }
    #[inline]
    fn as_ptr(&self) -> *const () { GcRef::identity(self) as *const () }
    #[inline]
    fn get(&self, k: &LuaValue) -> LuaValue { (**self).get(k) }
    #[inline]
    fn get_int(&self, k: i64) -> LuaValue { (**self).get_int(k) }
    #[inline]
    fn get_short_str(&self, k: &GcRef<LuaString>) -> LuaValue { (**self).get_short_str(k) }
    /// Forwards to [`LuaTable::try_raw_set`], which performs the nil/NaN
    /// key validation internally as part of its integer-fast-path match.
    #[inline]
    fn raw_set(&self, _state: &mut LuaState, k: LuaValue, v: LuaValue) -> Result<(), LuaError> {
        (**self).try_raw_set(k, v)
    }
    #[inline]
    fn raw_set_int(&self, _state: &mut LuaState, k: i64, v: LuaValue) -> Result<(), LuaError> {
        (**self).try_raw_set_int(k, v)
    }
    fn invalidate_tm_cache(&self) {}
    fn resize(&self, _state: &mut LuaState, na: usize, nh: usize) -> Result<(), LuaError> {
        let na32 = na.min(u32::MAX as usize) as u32;
        let nh32 = nh.min(u32::MAX as usize) as u32;
        (**self).resize(na32, nh32)
    }
    fn next(&self, k: LuaValue) -> Result<Option<(LuaValue, LuaValue)>, LuaError> {
        (**self).try_next_pair(&k)
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

/// Function-pointer signature for opening a file handle, installed on
/// [`GlobalState::file_open_hook`] by the embedder.
///
/// `std::fs` is banned outside `lua-cli`, so `lua-stdlib`'s io library reaches
/// the filesystem via this hook. `None` causes `io.open` and `io.output(name)`
/// to return a "file system not available" error, which is appropriate for
/// sandboxed embeddings.
///
/// `mode` is a Lua fopen-style mode string (e.g. `b"r"`, `b"w"`, `b"a"`,
/// `b"r+"`, etc.). The hook must honour at least `r`, `w`, and `a`.
pub type FileOpenHook =
    fn(filename: &[u8], mode: &[u8]) -> Result<Box<dyn lua_types::LuaFileHandle>, LuaError>;

/// Function-pointer signature for writing bytes to a host-provided output
/// stream, installed on [`GlobalState::stdout_hook`] or
/// [`GlobalState::stderr_hook`] by the embedder.
///
/// Bare `wasm32-unknown-unknown` has no ambient stdout/stderr. Keeping output
/// behind explicit hooks lets sandboxed and WASM hosts decide whether output is
/// unavailable, buffered, or bridged to something like a browser console.
pub type OutputHook = fn(bytes: &[u8]) -> std::io::Result<()>;

/// Function-pointer signature for reading bytes from a host-provided input
/// stream, installed on [`GlobalState::stdin_hook`] by the embedder.
pub type InputHook = fn(buf: &mut [u8]) -> std::io::Result<usize>;

/// Function-pointer signature for reading a host environment variable.
///
/// Returning `None` maps naturally to Lua's `os.getenv` result for a missing
/// variable and is also the sandbox/bare-WASM default when no environment is
/// exposed.
pub type EnvHook = fn(name: &[u8]) -> Option<Vec<u8>>;

/// Function-pointer signature for retrieving the current Unix time in seconds.
pub type UnixTimeHook = fn() -> i64;

/// Function-pointer signature for retrieving program CPU time in seconds.
///
/// Backs `os.clock`. C's `clock()` reads `CLOCK_PROCESS_CPUTIME_ID`, which has no
/// `std` equivalent and is unavailable on bare WASM; the stdlib falls back to a
/// monotonic wall-clock baseline (matching wasi-libc/Emscripten's emulation) when
/// this hook is unset. A host wanting true CPU time can install one (e.g. via the
/// `cpu-time` crate) without changing the sandboxed crates.
pub type CpuClockHook = fn() -> f64;

/// Function-pointer signature for the host's local timezone offset.
///
/// Given a Unix timestamp (seconds, UTC), returns the offset in seconds that the
/// host's local timezone applies at that instant, such that
/// `local_broken_down = gmtime(timestamp + offset)`. Positive east of UTC (e.g.
/// `+3600` for CET), negative west (e.g. `-14400` for US EDT). This backs the
/// local-time semantics of `os.date` (non-`!` formats) and `os.time`, which C
/// implements with `localtime_r`/`mktime`. Reading the host timezone database
/// requires `libc` FFI (`unsafe`), banned in `lua-stdlib`, so the host installs
/// this hook. When unset the stdlib uses UTC (offset 0), keeping the
/// `os.date`/`os.time` round-trip exact on hosts without a timezone.
pub type LocalOffsetHook = fn(timestamp: i64) -> i64;

/// Function-pointer signature for host entropy used by default PRNG seeds and
/// table-sort pivot randomisation. Hosts without entropy may leave it unset; the
/// stdlib then uses deterministic fallback values instead of touching OS stubs.
pub type EntropyHook = fn() -> u64;

/// Function-pointer signature for generating a host temporary filename.
///
/// Used by `os.tmpname` and `io.tmpfile`. The hook should return a path-like byte
/// string that the host's `file_open_hook` can understand.
pub type TempNameHook = fn() -> Result<Vec<u8>, LuaError>;

/// Function-pointer signature for spawning a child process with a connected
/// pipe, installed on [`GlobalState::popen_hook`] by the embedder.
///
/// `std::process::Command` is banned outside `lua-cli`, so `lua-stdlib`'s
/// `io.popen` reaches the OS through this hook. `None` causes `io.popen` to
/// raise a clean Lua error ("popen not enabled in this build"), which is
/// appropriate for sandboxed embeddings.
///
/// `mode` is the Lua popen mode string — `b"r"` for reading the child's
/// stdout, `b"w"` for writing to the child's stdin.
pub type PopenHook =
    fn(cmd: &[u8], mode: &[u8]) -> Result<Box<dyn lua_types::LuaFileHandle>, LuaError>;

/// Function-pointer signature for removing a file, installed on
/// [`GlobalState::file_remove_hook`] by the embedder.
///
/// `std::fs` is banned outside `lua-cli`, so `lua-stdlib`'s `os.remove`
/// reaches the filesystem via this hook. Returns `Ok(())` on success.
pub type FileRemoveHook = fn(filename: &[u8]) -> Result<(), LuaError>;

/// Function-pointer signature for renaming a file, installed on
/// [`GlobalState::file_rename_hook`] by the embedder.
///
/// `std::fs` is banned outside `lua-cli`, so `lua-stdlib`'s `os.rename`
/// reaches the filesystem via this hook. Returns `Ok(())` on success.
pub type FileRenameHook = fn(from: &[u8], to: &[u8]) -> Result<(), LuaError>;

/// Reason a shell command terminated, returned by [`OsExecuteHook`].
///
/// Mirrors the two string literals that C-Lua's `l_inspectstat` / `luaL_execresult`
/// can produce: `"exit"` for normal process exit, `"signal"` for signal termination
/// (POSIX only).
#[derive(Clone, Copy, Debug)]
pub enum OsExecuteReason {
    /// Process exited with an exit code (`WIFEXITED` / `ExitStatus::code()` is `Some`).
    Exit,
    /// Process was terminated by a signal (`WIFSIGNALED` / `ExitStatus::signal()` is `Some`).
    Signal,
}

/// Result returned by [`OsExecuteHook`], carrying the three values that
/// C-Lua's `luaL_execresult` pushes: `(boolean|nil, "exit"|"signal", int)`.
#[derive(Debug)]
pub struct OsExecuteResult {
    /// `true` when the command exited successfully (exit code 0).
    pub success: bool,
    /// How the process terminated.
    pub reason: OsExecuteReason,
    /// Exit code (for `Exit`) or signal number (for `Signal`).
    pub code: i32,
}

/// Function-pointer signature for executing a shell command, installed on
/// [`GlobalState::os_execute_hook`] by the embedder.
///
/// `std::process` is banned outside `lua-cli`, so `lua-stdlib`'s `os.execute`
/// reaches the shell via this hook. Returns an [`OsExecuteResult`] on success,
/// or a [`LuaError`] when the spawn itself fails.
pub type OsExecuteHook = fn(cmd: &[u8]) -> Result<OsExecuteResult, LuaError>;

/// Opaque handle to a dynamically loaded library, allocated by a
/// [`DynLibLoadHook`] backend and stored in `package._CLIBS`.
///
/// The handle is a backend-owned `u64`; the embedder is free to use it as an
/// index into a `Vec<libloading::Library>` or a `HashMap` key. `lua-stdlib`
/// stores the value verbatim and never inspects it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DynLibId(pub u64);

/// Resolved dynamic-library symbol.
///
/// Only `RustNative` is callable by this build of the VM. `LuaCAbi` resolves
/// to a real C function pointer compiled against stock Lua 5.4's `lua_State *`
/// ABI but cannot be safely invoked here — it is reported as an `"init"`
/// failure with a clear message. `Unsupported` carries an embedder-provided
/// reason byte-string.
pub enum DynamicSymbol {
    /// Function pointer that follows this build's Rust-native module ABI:
    /// `fn(&mut LuaState) -> Result<usize, LuaError>`.
    RustNative(LuaCFunction),
    /// Symbol exported against stock Lua 5.4's C ABI. The function pointer is
    /// resolved but never called from this build, since `lua_State *` is not
    /// our `LuaState`. Kept as a payload so a future C-ABI facade can pick it
    /// up; the embedder is responsible for ensuring the underlying library
    /// outlives this value.
    LuaCAbi(*const ()),
    /// Embedder-provided refusal reason, e.g. "symbol resolved but ABI version
    /// mismatch". Reported verbatim as an `"init"` failure.
    Unsupported { reason: Vec<u8> },
}

/// Function-pointer signature for loading a dynamic library, installed on
/// [`GlobalState::dynlib_load_hook`] by the embedder.
///
/// `libloading`/`dlopen`/`LoadLibraryEx` are FFI calls and require `unsafe`,
/// which is banned in `lua-stdlib`. `lua-cli` installs a `libloading`-backed
/// implementation. `None` causes `package.loadlib` to return the C-Lua
/// `"absent"` failure shape, matching the fallback platform stub.
///
/// `see_global` mirrors C-Lua's `seeglb` (POSIX `RTLD_GLOBAL`): set when the
/// caller invokes `package.loadlib(path, "*")`.
pub type DynLibLoadHook =
    fn(state: &mut LuaState, path: &[u8], see_global: bool) -> Result<DynLibId, LuaError>;

/// Function-pointer signature for resolving a symbol in a previously loaded
/// dynamic library, installed on [`GlobalState::dynlib_symbol_hook`].
///
/// The hook receives the [`DynLibId`] returned by [`DynLibLoadHook`] and the
/// requested symbol name. Returning `DynamicSymbol::RustNative` makes the
/// symbol callable; `LuaCAbi`/`Unsupported` propagate to `package.loadlib`
/// as an `"init"` failure with a clear message.
pub type DynLibSymbolHook =
    fn(state: &mut LuaState, handle: DynLibId, symbol: &[u8]) -> Result<DynamicSymbol, LuaError>;

/// Function-pointer signature for unloading a dynamic library, installed on
/// [`GlobalState::dynlib_unload_hook`].
///
/// Called from the `_CLIBS` `__gc` metamethod when the Lua state closes.
/// `libloading`'s safety model requires every loaded library to outlive the
/// last symbol it exports; the CLI backend is therefore free to ignore this
/// hook and keep libraries alive until process exit.
pub type DynLibUnloadHook = fn(handle: DynLibId);

/// One row of [`GlobalState::threads`]. Pairs the per-thread `LuaState`
/// with the canonical `GcRef<LuaThread>` so every `push_thread` for the
/// same id shares pointer-identity. Phase E-1 adds this; Phase E-2
/// extends it with interior-mutability bookkeeping when `resume`/`yield`
/// need to mutate the child thread while the parent holds a borrow.
pub struct ThreadRegistryEntry {
    /// The owned coroutine `LuaState`. Wrapped in `Rc<RefCell<...>>` so
    /// that `coroutine.resume` can borrow the child mutably while the
    /// parent is still in scope. Single-threaded — borrows never overlap
    /// in practice because only one resume path is live at a time.
    pub state: Rc<RefCell<LuaState>>,
    /// Canonical thread-value handle. Reused on every push so
    /// `GcRef::ptr_eq` is true across pushes.
    pub value: GcRef<lua_types::value::LuaThread>,
}

/// Stable key for a value pinned in [`ExternalRootSet`].
///
/// The generation is part of the key so a handle that has already unrooted its
/// slot cannot accidentally observe a later handle's value after slot reuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExternalRootKey {
    index: usize,
    generation: u64,
}

#[derive(Debug)]
struct ExternalRootSlot {
    value: Option<LuaValue>,
    generation: u64,
}

/// Values held alive by external Rust handles.
///
/// This is the embedding API's GC anchor. It intentionally lives directly on
/// `GlobalState` instead of inside the Lua registry table: handle drop/unroot
/// must be cheap, infallible, and independent of the Lua stack protocol.
#[derive(Debug, Default)]
pub struct ExternalRootSet {
    slots: Vec<ExternalRootSlot>,
    free: Vec<usize>,
    live: usize,
}

impl ExternalRootSet {
    pub fn insert(&mut self, value: LuaValue) -> ExternalRootKey {
        if let Some(index) = self.free.pop() {
            let slot = &mut self.slots[index];
            debug_assert!(
                slot.value.is_none(),
                "free external-root slot is occupied"
            );
            slot.generation = slot.generation.wrapping_add(1).max(1);
            slot.value = Some(value);
            self.live += 1;
            ExternalRootKey {
                index,
                generation: slot.generation,
            }
        } else {
            let index = self.slots.len();
            self.slots.push(ExternalRootSlot {
                value: Some(value),
                generation: 1,
            });
            self.live += 1;
            ExternalRootKey {
                index,
                generation: 1,
            }
        }
    }

    pub fn get(&self, key: ExternalRootKey) -> Option<&LuaValue> {
        let slot = self.slots.get(key.index)?;
        if slot.generation == key.generation {
            slot.value.as_ref()
        } else {
            None
        }
    }

    pub fn replace(&mut self, key: ExternalRootKey, value: LuaValue) -> Option<LuaValue> {
        let slot = self.slots.get_mut(key.index)?;
        if slot.generation != key.generation || slot.value.is_none() {
            return None;
        }
        slot.value.replace(value)
    }

    pub fn remove(&mut self, key: ExternalRootKey) -> Option<LuaValue> {
        let slot = self.slots.get_mut(key.index)?;
        if slot.generation != key.generation {
            return None;
        }
        let old = slot.value.take()?;
        self.free.push(key.index);
        self.live -= 1;
        Some(old)
    }

    pub fn iter_values(&self) -> impl Iterator<Item = &LuaValue> {
        self.slots.iter().filter_map(|slot| slot.value.as_ref())
    }

    pub fn len(&self) -> usize {
        self.live
    }

    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    pub fn vacant_len(&self) -> usize {
        self.free.len()
    }
}

/// Process-wide state shared by all Lua threads.
///
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

    /// Phase-B hook for opening a file handle for read/write/append. Set by
    /// `lua-cli` since `std::fs` is banned in `lua-stdlib`. `None` causes
    /// `io.open` and `io.output(name)` to return an error; standard output and
    /// error are controlled separately through output hooks/native fallbacks.
    pub file_open_hook: Option<FileOpenHook>,

    /// Hook for host stdout. When absent, native builds fall back to Rust stdout
    /// for compatibility; bare `wasm32-unknown-unknown` reports stdout
    /// unavailable instead of touching a stubbed stdio implementation.
    pub stdout_hook: Option<OutputHook>,

    /// Hook for host stderr. See [`GlobalState::stdout_hook`].
    pub stderr_hook: Option<OutputHook>,

    /// Hook for host stdin. When absent, native builds fall back to Rust stdin
    /// for compatibility; bare `wasm32-unknown-unknown` behaves like EOF.
    pub stdin_hook: Option<InputHook>,

    /// Hook for host environment lookups. `None` makes `os.getenv` return nil.
    pub env_hook: Option<EnvHook>,

    /// Hook for host wall-clock time. Required for `os.time()` and `os.date()`
    /// without an explicit timestamp under bare WASM.
    pub unix_time_hook: Option<UnixTimeHook>,

    /// Hook for host program CPU time. Backs `os.clock`. When unset, native builds
    /// use a monotonic wall-clock baseline and bare WASM reports it unavailable.
    pub cpu_clock_hook: Option<CpuClockHook>,

    /// Hook for the host's local timezone offset at a given instant. Backs the
    /// local-time semantics of `os.date` (non-`!` formats) and `os.time`. When
    /// unset, both use UTC, matching the prior behaviour and keeping the
    /// `os.date`/`os.time` round-trip exact under bare WASM.
    pub local_offset_hook: Option<LocalOffsetHook>,

    /// Hook for host entropy. Used by default `math.randomseed` and table sort
    /// pivot randomisation; absent hooks fall back to deterministic seeds.
    pub entropy_hook: Option<EntropyHook>,

    /// Hook for host temporary filenames. Used by `os.tmpname` and `io.tmpfile`.
    pub temp_name_hook: Option<TempNameHook>,

    /// Phase-G hook for spawning a child process and connecting one stream
    /// (stdin or stdout) to a Lua file handle. Set by `lua-cli` since
    /// `std::process::Command` is banned in `lua-stdlib`. `None` causes
    /// `io.popen` to raise a Lua error rather than panic.
    pub popen_hook: Option<PopenHook>,

    /// Phase-B hook for removing a file. Set by `lua-cli` since `std::fs` is
    /// banned in `lua-stdlib`. `None` causes `os.remove` to return an error.
    pub file_remove_hook: Option<FileRemoveHook>,

    /// Phase-B hook for renaming a file. Set by `lua-cli` since `std::fs` is
    /// banned in `lua-stdlib`. `None` causes `os.rename` to return an error.
    pub file_rename_hook: Option<FileRenameHook>,

    /// Phase-G hook for executing a shell command. Set by `lua-cli` since
    /// `std::process` is banned in `lua-stdlib`. `None` causes `os.execute`
    /// to report no shell available (matching C-Lua's `system(NULL) == 0`).
    pub os_execute_hook: Option<OsExecuteHook>,

    /// Phase-D-3.5 hook for loading a dynamic library (`dlopen` /
    /// `LoadLibraryEx`). Set by `lua-cli` since `libloading` is FFI and
    /// requires `unsafe`, which is banned in `lua-stdlib`. `None` causes
    /// `package.loadlib` to return the `"absent"` fallback shape.
    pub dynlib_load_hook: Option<DynLibLoadHook>,

    /// Phase-D-3.5 hook for resolving a symbol in a previously loaded
    /// dynamic library (`dlsym` / `GetProcAddress`). Set by `lua-cli`.
    /// `None` is treated as "absent" by `package.loadlib`.
    pub dynlib_symbol_hook: Option<DynLibSymbolHook>,

    /// Phase-D-3.5 hook for unloading a dynamic library (`dlclose` /
    /// `FreeLibrary`). Set by `lua-cli`. `None` keeps libraries loaded
    /// until process exit, which matches `libloading`'s safety model.
    pub dynlib_unload_hook: Option<DynLibUnloadHook>,

    // types.tsv: global_State.totalbytes → isize
    pub totalbytes: isize,

    /// Per-runtime sandbox budget shared across all threads. Inactive by
    /// default (`interval == 0`); see [`SandboxLimits`].
    pub sandbox: SandboxLimits,

    // types.tsv: global_State.GCdebt → isize
    pub gc_debt: isize,

    pub gc_estimate: usize,

    // types.tsv: global_State.lastatomic → usize
    pub lastatomic: usize,

    // types.tsv: global_State.strt → StringPool
    pub strt: StringPool,

    // types.tsv: global_State.l_registry → LuaValue
    pub l_registry: LuaValue,

    /// External Rust handles root their referents here while they are live.
    /// Traced from `GlobalState::trace`.
    pub external_roots: ExternalRootSet,

    // PORT NOTE (phase-b-reconcile): The lua-types LuaTable placeholder has
    // no storage, so we cannot persist `registry[LUA_RIDX_GLOBALS] = globals`
    // via the canonical registry path. Until the placeholder reconciles with
    // lua-vm::table::LuaTable, the globals table lives in a direct field
    // and `get_global_table` reads it from here. Same for `loaded` (the
    // module cache normally at `registry[_LOADED]`).
    pub globals: LuaValue,
    pub loaded: LuaValue,

    // types.tsv: global_State.nilvalue → LuaValue
    // PORT NOTE: In Rust we use a dedicated `is_complete: bool` flag rather than
    // the C trick of checking `ttisnil(&g->nilvalue)`. See `is_complete()`.
    pub nilvalue: LuaValue,

    // types.tsv: global_State.seed → u32
    pub seed: u32,

    // types.tsv: global_State.currentwhite → u8
    pub currentwhite: u8,

    pub gcstate: u8,

    pub gckind: u8,

    pub gcstopem: bool,

    // types.tsv: global_State.genminormul → u8
    pub genminormul: u8,

    pub genmajormul: u8,

    pub gcstp: u8,

    pub gcemergency: bool,

    // types.tsv: global_State.gcpause → u8
    pub gcpause: u8,

    // types.tsv: global_State.gcstepmul → u8
    pub gcstepmul: u8,

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

    /// Tables identified by the most recent `collect_via_heap` mark phase as
    /// reachable only through `pending_finalizers` (i.e. the user has dropped
    /// every reference). Their `__gc` runs the next time
    /// `run_pending_finalizers` executes; entries are then cleared. Traced as
    /// strong roots so they survive the sweep that scheduled them.
    pub to_be_finalized: Vec<GcRef<lua_types::value::LuaTable>>,

    // Phase-D NOTE: tobefnz + fixedgc removed (dead since Phase A — see
    // sibling note above re allgc et al). Pending finalizers live in
    // `pending_finalizers` above; fixed objects live in heap.allgc with the
    // GC's own `fixed` bit.

    // Generational cohort markers — Phase D only
    // types.tsv: global_State.survival/old1/reallyold/firstold1/finobjsur/finobjold1/finobjrold
    //   → (removed; replaced by index cursors in Phase D)

    // types.tsv: global_State.twups → Vec<GcRef<LuaState>>
    pub twups: Vec<GcRef<LuaState>>,

    // types.tsv: global_State.panic → Option<lua_CFunction>
    pub panic: Option<LuaCFunction>,

    // types.tsv: global_State.mainthread → GcRef<LuaState>
    // TODO(port): self-referential Rc cycle; Phase D GC handles cycles properly
    pub mainthread: Option<GcRef<LuaState>>,

    /// Registry of all live coroutine threads, keyed by `ThreadId`. Phase E-1
    /// replaces the `thread_token` placeholder with a real id-indexed map so
    /// `coroutine.create` allocates a fresh `LuaState`, registers it, and
    /// returns a value that resolves back to the same state on every
    /// `coroutine.status` / `coroutine.resume` call.
    ///
    /// Each entry pairs the per-thread `LuaState` with the canonical
    /// `GcRef<LuaThread>` value, so two `LuaValue::Thread` pushes of the
    /// same id share `GcRef::ptr_eq` identity. The main thread is NOT
    /// stored here — its `LuaState` is owned externally by the embedder.
    /// `main_thread_id` is reserved as `0` and a `LuaValue::Thread`
    /// carrying id `0` is recognized as the main thread by lookup helpers.
    pub threads: std::collections::HashMap<u64, ThreadRegistryEntry>,

    /// Cached `LuaValue::Thread` payload for the main thread (id 0).
    /// Built once during `new_state` so every `push_thread` on the main
    /// thread shares the same `GcRef<LuaThread>` and thus compares
    /// pointer-equal under `LuaValue::PartialEq`.
    pub main_thread_value: GcRef<lua_types::value::LuaThread>,

    /// Identity of the currently-running thread. `0` (main) until a
    /// coroutine resume swaps it in slice 02b. The Phase E-1 slice
    /// always leaves this at `main_thread_id` because resume is not yet
    /// implemented.
    pub current_thread_id: u64,

    /// Identity of the main thread. Convention: `0`. Held as a field so
    /// the lookup helpers can read it without hard-coding the constant.
    pub main_thread_id: u64,

    /// Monotonic counter handing out fresh ids in `new_thread`. Starts
    /// at `1` because `0` is reserved for the main thread.
    pub next_thread_id: u64,

    // types.tsv: global_State.memerrmsg → GcRef<LuaString>
    pub memerrmsg: GcRef<LuaString>,

    // types.tsv: global_State.tmname → [GcRef<LuaString>; TM_N]
    // TODO(port): TM_N constant and TagMethod enum come from ltm.c → tagmethods.rs
    pub tmname: Vec<GcRef<LuaString>>,

    // types.tsv: global_State.mt → [Option<GcRef<LuaTable>>; LUA_NUMTYPES]
    pub mt: [Option<GcRef<LuaTable>>; LUA_NUMTYPES],

    // types.tsv: global_State.strcache → [[GcRef<LuaString>; STRCACHE_M]; STRCACHE_N]
    pub strcache: [[GcRef<LuaString>; STRCACHE_M]; STRCACHE_N],

    /// Stable intern map for the public [`LuaString`] type. Distinct from
    /// `strt` (which keys internal `LuaStringImpl`) because the parser and
    /// stdlib need pointer-equality across `intern_str` calls so
    /// `GcRef::ptr_eq` can resolve variable identity. Without this map each
    /// call allocates a fresh `GcRef` and locals/upvalues fail to resolve.
    pub interned_lt: std::collections::HashMap<Box<[u8]>, GcRef<LuaString>>,

    // types.tsv: global_State.warnf → Option<Box<dyn FnMut(&[u8], bool)>>
    pub warnf: Option<Box<dyn FnMut(&[u8], bool)>>,

    /// Registry of native `LuaCFunction` pointers. Lua-types cannot reference
    /// `LuaState`, so `LuaClosure::LightC` carries a `usize` index into this
    /// vector instead of the real function pointer. `push_c_function`
    /// registers the function and stores the resulting index in the closure.
    pub c_functions: Vec<LuaCallable>,

    /// Phase-D heap. Owns the allgc intrusive list and runs collections.
    /// During Phase A-C this is `paused=true`, so allocations don't auto-
    /// register and `step` is a no-op. Phase D-1d wires `unpause()` after
    /// state initialization, at which point `step` runs during VM dispatch.
    pub heap: lua_gc::Heap,

    /// Phase E-3 cross-thread open-upvalue mirror. Maps `(thread_id, stack_idx)`
    /// to the live value of an open upvalue whose home thread is currently
    /// suspended while another thread runs. `coroutine.resume` snapshots the
    /// parent's open upvalues into this map before yielding control to the
    /// child, and reads the (possibly mutated) values back into the parent's
    /// stack when the child suspends or returns. From the running thread's
    /// perspective, `upvalue_get` / `upvalue_set` consult the mirror whenever
    /// an open upvalue's `thread_id` does not match `current_thread_id`.
    ///
    /// This avoids a stack refactor: the parent's `LuaState` is held by a
    /// `&mut` reference up the call stack during resume, so its stack cannot
    /// be reached directly through any `Rc<RefCell<_>>`. The mirror is the
    /// shared scratchpad that bridges the gap for the duration of a resume.
    pub cross_thread_upvals: std::collections::HashMap<(u64, StackIdx), LuaValue>,

    /// Phase F-1.a workaround for GC use-after-free across coroutine boundaries.
    /// When `aux_resume` switches to a child thread, the parent's live stack
    /// values would otherwise become unreachable to the tracer for the duration
    /// of the resume (the parent `LuaState` is held only as a stack-borrowed
    /// `&mut` up the call chain and is not part of any traced root set). To
    /// keep those values alive, `aux_resume` pushes a snapshot of the parent
    /// stack here before transferring control, and pops it on suspension or
    /// completion. The tracer visits every snapshot as a GC root via the
    /// `Trace for GlobalState` impl in `trace_impls.rs`.
    ///
    /// Phase F-2.b added a reachability-driven thread sweep that supersedes
    /// most of this, but the snapshot still guards values that live only on
    /// the parent's stack (i.e. not yet rooted by any thread node).
    pub suspended_parent_stacks: Vec<Vec<LuaValue>>,

    /// Open-upvalue handles belonging to the same suspended parent windows as
    /// `suspended_parent_stacks`. Stack snapshots keep the pointed-to values
    /// alive; this roots the `UpVal` objects themselves so a GC inside the
    /// child coroutine cannot sweep entries still present in the parent's
    /// `openupval` list.
    pub suspended_parent_open_upvals: Vec<Vec<GcRef<UpVal>>>,
}

/// `LUA_MASKCOUNT` (`1 << LUA_HOOKCOUNT`) — the count-hook event mask the
/// sandbox arms on every thread to drive per-interval budget enforcement.
const SANDBOX_COUNT_MASK: u8 = 1 << 3;

/// Sandbox trip code: not tripped.
pub const SANDBOX_TRIP_NONE: u8 = 0;
/// Sandbox trip code: the instruction budget reached zero.
pub const SANDBOX_TRIP_INSTRUCTIONS: u8 = 1;
/// Sandbox trip code: GC-tracked memory exceeded the configured ceiling.
pub const SANDBOX_TRIP_MEMORY: u8 = 2;

/// Per-runtime sandbox budget, shared by every thread (main + coroutines) via
/// the `Rc<RefCell<GlobalState>>` they all hold. Every field is a `Cell` so the
/// VM can charge the budget through the shared `Ref` it borrows in the
/// count-hook path — no `&mut` and no write-borrow on the hot path.
/// `interval == 0` means inactive; in that case the VM never sets the
/// count-hook mask, so there is zero overhead.
#[derive(Default)]
pub struct SandboxLimits {
    /// Count-hook interval in instructions; `0` = sandbox inactive.
    pub interval: std::cell::Cell<i32>,
    /// Whether an instruction budget is enforced.
    pub instr_limited: std::cell::Cell<bool>,
    /// Instructions left before the budget trips.
    pub instr_remaining: std::cell::Cell<u64>,
    /// Configured instruction limit, retained so `reset` can refill.
    pub instr_limit: std::cell::Cell<u64>,
    /// GC-byte ceiling; `None` = no memory limit.
    pub mem_limit: std::cell::Cell<Option<usize>>,
    /// One of the `SANDBOX_TRIP_*` codes.
    pub tripped: std::cell::Cell<u8>,
    /// Sticky once a limit trips: the abort is *uncatchable*. While set,
    /// `pcall`/`xpcall`/`coroutine.resume` re-raise the trip error instead of
    /// swallowing it, so untrusted code cannot defeat the budget by catching
    /// it in a loop. Cleared only by [`LuaState::sandbox_reset`].
    pub aborting: std::cell::Cell<bool>,
}

impl GlobalState {
    /// True while a sandbox instruction/memory budget is active on this runtime.
    pub fn sandbox_active(&self) -> bool {
        self.sandbox.interval.get() != 0
    }

    /// Total live bytes allocated (GCdebt + totalbytes).
    ///
    /// macros.tsv: `gettotalbytes → g.total_bytes()`
    pub fn total_bytes(&self) -> usize {
        (self.totalbytes + self.gc_debt) as usize
    }

    /// Look up the coroutine `LuaState` registered under `id`. Returns
    /// `None` for the main-thread id (the main `LuaState` is owned by
    /// the embedder, not stored in `threads`) and for ids that were
    /// never issued or have already been closed.
    pub fn get_thread(&self, id: u64) -> Option<&ThreadRegistryEntry> {
        self.threads.get(&id)
    }

    /// Return the canonical `GcRef<LuaThread>` for `id`. For the main
    /// thread that's `main_thread_value`; for a coroutine it's the
    /// value stored in the registry. Returns `None` if `id` is unknown.
    pub fn thread_value_for(&self, id: u64) -> Option<GcRef<lua_types::value::LuaThread>> {
        if id == self.main_thread_id {
            Some(self.main_thread_value.clone())
        } else {
            self.threads.get(&id).map(|e| e.value.clone())
        }
    }

    /// Returns `true` when the state has been fully initialized.
    ///
    /// macros.tsv: `completestate → g.is_complete()`
    ///
    /// PORT NOTE: C uses `g->nilvalue` being nil as the "complete" signal.
    /// We replicate the same logic: `nilvalue == Nil` means complete.
    pub fn is_complete(&self) -> bool {
        matches!(self.nilvalue, LuaValue::Nil)
    }

    /// Returns the "current white" GC color bitmask.
    ///
    /// macros.tsv: `luaC_white → g.current_white()`
    ///
    /// PORT NOTE: GC color management deferred to Phase D; always returns
    /// the initial white bit.
    pub fn current_white(&self) -> u8 {
        self.currentwhite
    }

    /// Returns the "other white" GC color bitmask.
    ///
    /// macros.tsv: `otherwhite → g.other_white()`
    pub fn other_white(&self) -> u8 {
        // TODO(port): Phase D — toggle white bit properly
        self.currentwhite ^ 0x03
    }

    /// Returns `true` if the GC is in generational mode.
    ///
    /// macros.tsv: `isdecGCmodegen → g.is_gen_mode()`
    pub fn is_gen_mode(&self) -> bool {
        self.gckind == GcKind::Generational as u8
    }

    /// Returns `true` if the GC is currently running.
    ///
    /// macros.tsv: `gcrunning → g.gc_running()`
    pub fn gc_running(&self) -> bool {
        self.gcstp == 0
    }

    /// Returns `true` while the GC is in its propagation phase.
    ///
    /// macros.tsv: `keepinvariant → g.keep_invariant()`
    pub fn keep_invariant(&self) -> bool {
        // TODO(port): Phase D — check gcstate for propagation phases
        false
    }

    /// Returns `true` while the GC is in a sweep phase.
    ///
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
    pub fn stop_gc_internal(&mut self) -> u8 {
        let old = self.gcstp;
        self.gcstp |= GCSTPGC;
        old
    }
    pub fn set_gc_stop_user(&mut self) {
        // GCSTPUSR (lgc.h:155) = 1 — bit set when GC is stopped by user (lua_gc(L, LUA_GCSTOP)).
        self.gcstp = GCSTPUSR;
    }
    pub fn clear_gc_stop(&mut self) { self.gcstp = 0; }
    pub fn is_gc_running(&self) -> bool { self.gcstp == 0 }
    /// True when the GC has been disabled internally (state setup, mid-GC,
    /// or while closing); user-stop via `collectgarbage("stop")` does NOT
    /// set this bit, so `lua_gc` continues to honour Count/Step/etc.
    ///
    pub fn is_gc_stopped_internally(&self) -> bool { (self.gcstp & GCSTPGC) != 0 }

    /// Returns the interned `__xxx` name string for tag method `tm`, or
    /// `None` if `tmname` has not yet been initialised (early bootstrap).
    ///
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
/// types.tsv: `lua_State → LuaState`
///
/// All stack-pointer fields in C (`StkIdRel`, `StkId`) become `StackIdx` (u32
/// index into `stack: Vec<StackValue>`).  The C intrusive `CallInfo` linked list
/// becomes `call_info: Vec<CallInfo>` indexed by `CallInfoIdx`.
pub struct LuaState {
    // ── Thread status ──

    // types.tsv: lua_State.status → u8
    pub status: u8,

    // types.tsv: lua_State.allowhook → bool
    pub allowhook: bool,

    // types.tsv: lua_State.nci → u32
    pub nci: u32,

    // ── Stack ──

    // types.tsv: lua_State.top → StackIdx
    pub top: StackIdx,

    // types.tsv: lua_State.stack_last → StackIdx (redundant once Vec; kept for parity)
    pub stack_last: StackIdx,

    // types.tsv: lua_State.stack → Vec<StackValue>
    pub stack: Vec<StackValue>,

    // ── Call info ──

    // types.tsv: lua_State.ci → CallInfoIdx
    pub ci: CallInfoIdx,

    // types.tsv: lua_State.base_ci → CallInfo  (Vec element 0)
    // PORT NOTE: In Rust, base_ci is call_info[0]. There is no separate field.
    pub call_info: Vec<CallInfo>,

    // ── Upvalues / to-be-closed ──

    // types.tsv: lua_State.openupval → Vec<GcRef<UpVal>>
    pub openupval: Vec<GcRef<UpVal>>,

    // types.tsv: lua_State.tbclist → Vec<StackIdx>
    pub tbclist: Vec<StackIdx>,

    // ── Global state ──

    // types.tsv: lua_State.l_G → (accessed via method)
    // PORT NOTE: Rc<RefCell<>> for shared ownership across coroutine threads.
    pub(crate) global: Rc<RefCell<GlobalState>>,

    // ── Hooks ──

    // types.tsv: lua_State.hook → Option<Box<dyn FnMut(&mut LuaState, &LuaDebug)>>
    pub hook: Option<Box<dyn FnMut(&mut LuaState, &crate::debug::LuaDebug)>>,

    // types.tsv: lua_State.hookmask → u8
    pub hookmask: u8,

    // types.tsv: lua_State.basehookcount → i32
    pub basehookcount: i32,

    // types.tsv: lua_State.hookcount → i32
    pub hookcount: i32,

    // ── Error handling ──

    // types.tsv: lua_State.errorJmp → (removed; replaced by Result<T, LuaError>)
    // PORT NOTE: Entirely removed. The `?` operator replaces setjmp/longjmp.

    // types.tsv: lua_State.errfunc → isize
    pub errfunc: isize,

    // ── C-call depth ──

    // types.tsv: lua_State.n_ccalls → u32
    pub n_ccalls: u32,

    // ── Debug / hooks ──

    // types.tsv: lua_State.oldpc → u32
    pub oldpc: u32,

    // ── GC color (Phase D) ──

    // types.tsv: GCObject.marked → u8
    pub marked: u8,

    /// Owner thread id for this `LuaState`, cached as a plain `u64` so the
    /// hot path of `upvalue_get` can compare against an open upvalue's
    /// `thread_id` without taking a `RefCell::borrow` on the shared
    /// `GlobalState`.
    ///
    /// Invariant: while this `LuaState` is the actively running thread,
    /// `GlobalState::current_thread_id == self.cached_thread_id`. This is
    /// maintained structurally by `new_state`/`new_thread` (which set
    /// `cached_thread_id` to the thread's own id once at construction)
    /// combined with the coroutine resume protocol: `coro_lib::resume`
    /// writes `co_state.global.current_thread_id = co_id` before the
    /// coroutine runs, and restores `parent_thread_id` on yield/return.
    /// Because each thread caches its own id (not the global's id), the
    /// invariant survives every context switch without an explicit refresh
    /// at the resume site.
    pub cached_thread_id: u64,

    /// Local GC gate.
    ///
    /// Avoids borrowing `GlobalState` on every call edge when GC/finalizers
    /// are not currently due.
    pub gc_check_needed: bool,

}

impl LuaState {
    /// Access the process-wide `GlobalState` immutably.
    ///
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

    /// Return the current C-call recursion depth (lower 16 bits of `n_ccalls`).
    ///
    /// macros.tsv: `getCcalls → state.c_calls()`
    pub fn c_calls(&self) -> u32 {
        self.n_ccalls & 0xffff
    }

    /// Increment the non-yieldable call count (upper 16 bits of `n_ccalls`).
    ///
    /// macros.tsv: `incnny → state.inc_nny()`
    pub fn inc_nny(&mut self) {
        self.n_ccalls += 0x10000;
    }

    /// Decrement the non-yieldable call count.
    ///
    /// macros.tsv: `decnny → state.dec_nny()`
    pub fn dec_nny(&mut self) {
        self.n_ccalls -= 0x10000;
    }

    /// Returns `true` if the thread can yield (no non-yieldable frames on the stack).
    ///
    /// macros.tsv: `yieldable → state.is_yieldable()`
    pub fn is_yieldable(&self) -> bool {
        (self.n_ccalls & 0xffff0000) == 0
    }

    /// Reset the hook countdown to the baseline.
    ///
    /// macros.tsv: `resethookcount → state.reset_hook_count()`
    pub fn reset_hook_count(&mut self) {
        self.hookcount = self.basehookcount;
    }

    /// Activate the per-runtime sandbox budget and arm the current thread.
    ///
    /// Stores the budget in `GlobalState` (shared across every thread) and
    /// sets the count-hook mask on this thread so the dispatch loop traps every
    /// `interval` instructions. Coroutines created afterwards inherit the mask
    /// via `preinit_thread`, so metering spans all threads — closing the
    /// coroutine-escape that a per-thread closure could not. Pass `None` for a
    /// limit to leave that dimension unbounded.
    pub fn install_sandbox_limits(
        &mut self,
        interval: i32,
        instr_limit: Option<u64>,
        mem_limit: Option<usize>,
    ) {
        let interval = interval.max(1);
        {
            let g = self.global();
            g.sandbox.interval.set(interval);
            g.sandbox.instr_limited.set(instr_limit.is_some());
            g.sandbox.instr_remaining.set(instr_limit.unwrap_or(0));
            g.sandbox.instr_limit.set(instr_limit.unwrap_or(0));
            g.sandbox.mem_limit.set(mem_limit);
            g.sandbox.tripped.set(SANDBOX_TRIP_NONE);
        }
        self.hookmask |= SANDBOX_COUNT_MASK;
        self.basehookcount = interval;
        self.hookcount = interval;
        crate::debug::arm_traps(self);
    }

    /// Charge the shared budget for one count-hook interval. Returns the abort
    /// error if a limit has been crossed (and records why in `tripped`).
    /// Called from `trace_exec` on every thread, once per `interval`
    /// instructions — never on the budget-disabled hot path.
    pub fn sandbox_charge_interval(&self) -> Option<LuaError> {
        let interval = self.global().sandbox.interval.get();
        self.sandbox_charge(interval as u64)
    }

    /// Charge `amount` instructions against the runtime-wide budget and sample
    /// the memory ceiling. Returns the uncatchable abort error if a limit is
    /// crossed (recording the reason and arming the sticky `aborting` flag), or
    /// `None` otherwise. No-op when no sandbox is active.
    ///
    /// Used both by the per-interval VM charge and by loop-heavy stdlib
    /// functions (the pattern matcher) so a single native call cannot run for
    /// longer than the instruction budget allows.
    pub fn sandbox_charge(&self, amount: u64) -> Option<LuaError> {
        let g = self.global();
        if g.sandbox.interval.get() == 0 {
            return None;
        }
        if g.sandbox.instr_limited.get() {
            let rem = g.sandbox.instr_remaining.get().saturating_sub(amount);
            g.sandbox.instr_remaining.set(rem);
            if rem == 0 {
                g.sandbox.tripped.set(SANDBOX_TRIP_INSTRUCTIONS);
                g.sandbox.aborting.set(true);
                return Some(LuaError::runtime(format_args!(
                    "sandbox: instruction budget exhausted"
                )));
            }
        }
        if let Some(limit) = g.sandbox.mem_limit.get() {
            if g.total_bytes() > limit {
                g.sandbox.tripped.set(SANDBOX_TRIP_MEMORY);
                g.sandbox.aborting.set(true);
                return Some(LuaError::runtime(format_args!(
                    "sandbox: memory limit exceeded"
                )));
            }
        }
        None
    }

    /// Reject a size-known-upfront allocation that would push GC-tracked memory
    /// past the ceiling, *before* the buffer is built. Returns the uncatchable
    /// memory abort if `total_bytes() + additional` exceeds the limit. Used by
    /// stdlib functions that allocate a large buffer of a computed size in one
    /// instruction (e.g. `string.rep`, `string.pack`, `table.concat`), where the
    /// per-instruction `sandbox_check_memory` would only fire *after* the
    /// allocation already happened.
    pub fn sandbox_reserve(&self, additional: usize) -> Option<LuaError> {
        let g = self.global();
        if g.sandbox.interval.get() == 0 {
            return None;
        }
        if let Some(limit) = g.sandbox.mem_limit.get() {
            let projected = g.total_bytes().saturating_add(additional);
            if projected > limit {
                g.sandbox.tripped.set(SANDBOX_TRIP_MEMORY);
                g.sandbox.aborting.set(true);
                return Some(LuaError::runtime(format_args!(
                    "sandbox: memory limit exceeded"
                )));
            }
        }
        None
    }

    /// Upper bound on the work a single pattern-match call may do before it must
    /// stop and let the caller charge the budget. Equal to the remaining
    /// instruction budget when an instruction limit is active, else `0` meaning
    /// "unlimited" (preserving non-sandboxed behavior exactly).
    pub fn sandbox_match_step_limit(&self) -> u64 {
        let g = self.global();
        if g.sandbox.interval.get() != 0 && g.sandbox.instr_limited.get() {
            g.sandbox.instr_remaining.get()
        } else {
            0
        }
    }

    /// Whether a sandbox abort is in flight. While true, protected-call builtins
    /// (`pcall`/`xpcall`/`coroutine.resume`) must re-raise rather than catch, so
    /// the budget trip is uncatchable. Set on trip, cleared by `sandbox_reset`.
    pub fn sandbox_aborting(&self) -> bool {
        self.global().sandbox.aborting.get()
    }

    /// Whether an instruction budget is active (vs. only a memory limit / none).
    pub fn sandbox_instr_limited(&self) -> bool {
        self.global().sandbox.instr_limited.get()
    }

    /// Instructions left before the budget trips (meaningful only when
    /// [`sandbox_instr_limited`](Self::sandbox_instr_limited)).
    pub fn sandbox_instr_remaining(&self) -> u64 {
        self.global().sandbox.instr_remaining.get()
    }

    /// The configured instruction limit (for computing "used").
    pub fn sandbox_instr_limit(&self) -> u64 {
        self.global().sandbox.instr_limit.get()
    }

    /// The current trip code (one of the `SANDBOX_TRIP_*` constants).
    pub fn sandbox_tripped_code(&self) -> u8 {
        self.global().sandbox.tripped.get()
    }

    /// Refill the instruction budget to its configured limit and clear the
    /// trip flag, so the same runtime can run another chunk.
    pub fn sandbox_reset(&self) {
        let g = self.global();
        if g.sandbox.instr_limited.get() {
            g.sandbox.instr_remaining.set(g.sandbox.instr_limit.get());
        }
        g.sandbox.tripped.set(SANDBOX_TRIP_NONE);
        g.sandbox.aborting.set(false);
    }

    /// Returns the current stack capacity (slots between base and stack_last).
    ///
    /// macros.tsv: `stacksize → state.stack_size()`
    pub fn stack_size(&self) -> usize {
        self.stack_last.0 as usize
    }

    /// Push a value onto the stack, incrementing `top`.
    ///
    /// macros.tsv: `api_incr_top → gone — state.push() already increments`
    #[inline(always)]
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
    #[inline(always)]
    pub fn pop(&mut self) -> LuaValue {
        if self.top.0 == 0 {
            return LuaValue::Nil;
        }
        self.top = StackIdx(self.top.0 - 1);
        self.stack[self.top.0 as usize].val.clone()
    }

    /// Retrieve the value at the given stack index without removing it.
    ///
    /// macros.tsv: `s2v → state.stack_at(idx)` → returns `&LuaValue`
    #[inline(always)]
    pub fn stack_val(&self, idx: StackIdx) -> &LuaValue {
        &self.stack[idx.0 as usize].val
    }

    /// Write a value to a specific stack slot.
    #[inline(always)]
    pub fn set_stack_val(&mut self, idx: StackIdx, val: LuaValue) {
        self.stack[idx.0 as usize].val = val;
    }

    /// Returns a no-op GC handle.
    ///
    /// macros.tsv: `luaC_checkGC → state.gc().check_step()`, etc.
    ///
    /// PORT NOTE: In Phases A–C the GC is `Rc`-based and all GC operations are
    /// no-ops. Phase D replaces this with real GC logic in `lua-gc`.
    pub fn gc(&mut self) -> GcHandle<'_> {
        GcHandle { _state: self }
    }

    /// Pin a Lua value in the external root set and return its stable key.
    pub fn external_root_value(&mut self, value: LuaValue) -> ExternalRootKey {
        self.global_mut().external_roots.insert(value)
    }

    /// Read a value currently pinned by an external root key.
    pub fn external_rooted_value(&self, key: ExternalRootKey) -> Option<LuaValue> {
        self.global().external_roots.get(key).cloned()
    }

    /// Replace the value pinned by an external root key.
    pub fn external_replace_root(
        &mut self,
        key: ExternalRootKey,
        value: LuaValue,
    ) -> Option<LuaValue> {
        self.global_mut().external_roots.replace(key, value)
    }

    /// Remove an external root. Returns `None` for stale or already-removed keys.
    pub fn external_unroot_value(&mut self, key: ExternalRootKey) -> Option<LuaValue> {
        self.global_mut().external_roots.remove(key)
    }

    /// Best-effort external root removal for destructors that may run while
    /// the collector holds an immutable `GlobalState` borrow.
    pub fn try_external_unroot_value(
        &mut self,
        key: ExternalRootKey,
    ) -> std::result::Result<Option<LuaValue>, std::cell::BorrowMutError> {
        self.global
            .try_borrow_mut()
            .map(|mut global| global.external_roots.remove(key))
    }

    /// Create a new empty table and register it with the GC.
    ///
    /// macros.tsv: `lua_newtable → state.new_table()`
    pub fn new_table(&mut self) -> GcRef<LuaTable> {
        // TODO(port): register with GC tracking (state.global_mut().allgc) in Phase D
        self.mark_gc_check_needed();
        GcRef::new(LuaTable::placeholder())
    }

    /// Create a fresh table with pre-sized array/hash parts.
    ///
    /// mirrors the `luaH_new` + `luaH_resize` pair in one call so we don't
    /// pay an extra resize path for hot construction sites.
    pub fn new_table_with_sizes(
        &mut self,
        array_size: u32,
        hash_size: u32,
    ) -> Result<GcRef<LuaTable>, LuaError> {
        self.mark_gc_check_needed();
        let t = GcRef::new(LuaTable::placeholder());
        self.table_resize(&t, array_size as usize, hash_size as usize)?;
        t.account_buffer(t.buffer_bytes() as isize);
        Ok(t)
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
    /// macros.tsv: `luaS_new → state.intern_str(s)`
    pub fn intern_str(&mut self, bytes: &[u8]) -> Result<GcRef<LuaString>, LuaError> {
        if bytes.len() <= crate::string::MAX_SHORT_LEN {
            if let Some(existing) = self.global().interned_lt.get(bytes) {
                return Ok(existing.clone());
            }
            self.mark_gc_check_needed();
            let new_ref = GcRef::new(LuaString::from_bytes(bytes.to_vec()));
            self.global_mut()
                .interned_lt
                .insert(bytes.to_vec().into_boxed_slice(), new_ref.clone());
            Ok(new_ref)
        } else {
            self.mark_gc_check_needed();
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
    #[inline(always)]
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
    #[inline(always)]
    pub fn get_at(&self, idx: impl Into<StackIdxConv>) -> LuaValue {
        let i: StackIdx = idx.into().0;
        match self.stack.get(i.0 as usize) {
            Some(slot) => slot.val.clone(),
            None => LuaValue::Nil,
        }
    }
    #[inline(always)]
    pub fn set_at(&mut self, idx: impl Into<StackIdxConv>, v: LuaValue) {
        let i: StackIdx = idx.into().0;
        self.stack[i.0 as usize].val = v;
    }

    /// Clear stack slots in `[start, end)` without changing `top`.
    ///
    /// Internal call setup reserves space up to `ci.top`; while GC tracing is
    /// conservative over that range, the unused tail must not retain stale
    /// collectable values from previous frames.
    pub fn clear_stack_range(&mut self, start: StackIdx, end: StackIdx) {
        if end.0 <= start.0 {
            return;
        }
        let end_u = end.0 as usize;
        if end_u > self.stack.len() {
            self.stack.resize_with(end_u, StackValue::default);
        }
        for i in start.0..end.0 {
            self.stack[i as usize].val = LuaValue::Nil;
            self.stack[i as usize].tbc_delta = 0;
        }
    }
    /// Hot-path accessor: returns `Some(i)` only when the stack slot at `idx`
    /// holds a `LuaValue::Int(i)`. Returns `None` for any other tag (including
    /// out-of-bounds, which behaves as `Nil`).
    ///
    /// `ttisinteger` predicate that gates the integer arithmetic fast path in
    /// `lvm.c`'s `op_arith_aux` macro. Avoids the full `LuaValue` clone that
    /// `get_at` performs — the operand is only needed for its `i64` payload.
    #[inline(always)]
    pub fn get_int_at(&self, idx: impl Into<StackIdxConv>) -> Option<i64> {
        let i: StackIdx = idx.into().0;
        match self.stack.get(i.0 as usize) {
            Some(slot) => match &slot.val {
                LuaValue::Int(v) => Some(*v),
                _ => None,
            },
            None => None,
        }
    }
    /// Hot-path accessor: returns `Some((a, b))` only when both stack slots
    /// at `rb` and `rc` hold integers. Equivalent to two `get_int_at` calls
    /// but is shaped so the arithmetic opcode dispatch arms can pattern-match
    /// the common case with a single `if let`.
    ///
    /// the `op_arith_aux` macro.
    #[inline(always)]
    pub fn get_int_pair_at(
        &self,
        rb: impl Into<StackIdxConv>,
        rc: impl Into<StackIdxConv>,
    ) -> Option<(i64, i64)> {
        let rb: StackIdx = rb.into().0;
        let rc: StackIdx = rc.into().0;
        match (
            self.stack[rb.0 as usize].val,
            self.stack[rc.0 as usize].val,
        ) {
            (LuaValue::Int(ib), LuaValue::Int(ic)) => Some((ib, ic)),
            _ => None,
        }
    }
    /// Hot-path accessor: returns `Some(f)` when the slot holds a `Float(f)`
    /// or coerces an `Int(i)` to `f64`. Returns `None` for any other tag.
    /// No `LuaValue` clone — only the primitive payload travels back.
    ///
    #[inline(always)]
    pub fn get_num_at(&self, idx: impl Into<StackIdxConv>) -> Option<f64> {
        let i: StackIdx = idx.into().0;
        match self.stack.get(i.0 as usize) {
            Some(slot) => match &slot.val {
                LuaValue::Float(f) => Some(*f),
                LuaValue::Int(v) => Some(*v as f64),
                _ => None,
            },
            None => None,
        }
    }
    /// Hot-path accessor: returns `Some(f)` only when the slot holds a
    /// `LuaValue::Float(f)`. Does NOT coerce integers; the integer branch is
    /// the caller's responsibility. Used by opcode arms that have already
    /// ruled out the integer fast path.
    #[inline(always)]
    pub fn get_float_at(&self, idx: impl Into<StackIdxConv>) -> Option<f64> {
        let i: StackIdx = idx.into().0;
        match self.stack.get(i.0 as usize) {
            Some(slot) => match &slot.val {
                LuaValue::Float(f) => Some(*f),
                _ => None,
            },
            None => None,
        }
    }
    /// Hot-path accessor: pair version of `get_num_at` — returns `Some((a,b))`
    /// when both slots coerce to `f64` (Float or Int), `None` if either does
    /// not. Used by the float fast path of the arith opcodes.
    ///
    #[inline(always)]
    pub fn get_num_pair_at(
        &self,
        rb: impl Into<StackIdxConv>,
        rc: impl Into<StackIdxConv>,
    ) -> Option<(f64, f64)> {
        let rb: StackIdx = rb.into().0;
        let rc: StackIdx = rc.into().0;
        match (
            self.stack[rb.0 as usize].val,
            self.stack[rc.0 as usize].val,
        ) {
            (LuaValue::Float(nb), LuaValue::Float(nc)) => Some((nb, nc)),
            (LuaValue::Int(ib), LuaValue::Int(ic)) => Some((ib as f64, ic as f64)),
            (LuaValue::Int(ib), LuaValue::Float(nc)) => Some((ib as f64, nc)),
            (LuaValue::Float(nb), LuaValue::Int(ic)) => Some((nb, ic as f64)),
            _ => None,
        }
    }
    /// Set `top` to an absolute stack index. Grows the backing stack vector
    /// (filling new slots with `Nil`) when `idx` is past `stack.len()`, but
    /// never clobbers existing slots between the old top and the new top —
    /// VM opcodes (Call, ForPrep, etc.) write registers via `set_at` and then
    /// raise `top` to signal "these are now live"; nil-filling here would
    /// erase the just-written values.
    ///
    /// setnilvalue(s2v(L->top.p++))` clear loop in `lua_settop` (lapi.c) is
    /// part of the public API path and lives in `api::set_top` instead.
    /// PORT NOTE: callers pass an absolute `StackIdx`, not the relative `idx`
    /// of the public `lua_settop`. The to-be-closed (`tbclist`) close path
    /// is Phase E and not handled here.
    #[inline(always)]
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
    /// PORT NOTE: callers (`api.rs::set_top`, `raw_set`, etc.) pre-nil-fill or
    /// only shrink, so this routine intentionally does no clearing or resizing.
    /// The to-be-closed (`tbclist`) close path is Phase E.
    #[inline(always)]
    pub fn set_top_idx(&mut self, idx: impl Into<StackIdxConv>) {
        let new_top: StackIdx = idx.into().0;
        self.top = new_top;
    }
    /// Decrement `top` by 1 (saturating at zero).
    ///
    #[inline(always)]
    pub fn dec_top(&mut self) {
        if self.top.0 > 0 {
            self.top = StackIdx(self.top.0 - 1);
        }
    }
    #[inline(always)]
    pub fn pop_n(&mut self, n: usize) {
        let cur = self.top.0 as usize;
        let new = cur.saturating_sub(n);
        self.top = StackIdx(new as u32);
    }
    /// Returns the value at the given stack index without removing it.
    ///
    #[inline(always)]
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
    #[inline(always)]
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
    pub fn peek_string_at_top(&mut self) -> GcRef<LuaString> {
        match self.peek_top() {
            LuaValue::Str(s) => s,
            _ => panic!("peek_string_at_top: top of stack is not a string"),
        }
    }
    /// Mutable reference to the value at the given stack slot.
    ///
    pub fn stack_at(&mut self, idx: impl Into<StackIdxConv>) -> &mut LuaValue {
        let i: StackIdx = idx.into().0;
        &mut self.stack[i.0 as usize].val
    }
    /// Writes `Nil` to the given stack slot.
    ///
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
    pub fn grow_stack(&mut self, n: i32, raise_error: bool) -> Result<(), LuaError> {
        crate::do_::grow_stack(self, n, raise_error).map(|_| ())
    }

    #[inline(always)]
    pub fn get_ci(&self, idx: CallInfoIdx) -> &CallInfo { &self.call_info[idx.as_usize()] }
    #[inline(always)]
    pub fn get_ci_mut(&mut self, idx: CallInfoIdx) -> &mut CallInfo { &mut self.call_info[idx.as_usize()] }
    #[inline(always)]
    pub fn current_call_info(&self) -> &CallInfo { &self.call_info[self.ci.as_usize()] }
    #[inline(always)]
    pub fn current_call_info_mut(&mut self) -> &mut CallInfo { let i = self.ci.as_usize(); &mut self.call_info[i] }
    #[inline(always)]
    pub fn current_ci_idx(&self) -> CallInfoIdx { self.ci }
    pub fn call_stack_mut(&mut self) -> &mut Vec<CallInfo> { &mut self.call_info }
    #[inline(always)]
    pub fn next_ci(&mut self) -> Result<CallInfoIdx, LuaError> {
        match self.call_info[self.ci.as_usize()].next {
            Some(idx) => Ok(idx),
            None => Ok(extend_ci(self)),
        }
    }
    #[inline(always)]
    pub fn prev_ci(&self, idx: CallInfoIdx) -> Option<CallInfoIdx> { self.call_info[idx.as_usize()].previous }
    pub fn get_prev_ci(&self, idx: CallInfoIdx) -> Option<&CallInfo> {
        self.call_info[idx.as_usize()]
            .previous
            .map(|p| &self.call_info[p.as_usize()])
    }
    #[inline(always)]
    pub fn is_base_ci(&self, idx: CallInfoIdx) -> bool { idx.as_usize() == 0 }
    #[inline(always)]
    pub fn is_current_ci(&self, idx: CallInfoIdx) -> bool { idx == self.ci }
    pub fn ci_next_func(&self, idx: CallInfoIdx) -> StackIdx {
        let next = self.call_info[idx.as_usize()]
            .next
            .expect("ci_next_func: no next CallInfo");
        self.call_info[next.as_usize()].func
    }
    #[inline(always)]
    pub fn ci_top(&self, idx: CallInfoIdx) -> StackIdx { self.call_info[idx.as_usize()].top }
    #[inline(always)]
    pub fn ci_trap(&mut self, idx: CallInfoIdx) -> bool {
        if let CallInfoFrame::Lua { trap, .. } = self.call_info[idx.as_usize()].u {
            trap
        } else {
            false
        }
    }
    #[inline(always)]
    pub fn ci_savedpc(&self, idx: CallInfoIdx) -> u32 { self.call_info[idx.as_usize()].saved_pc() }
    #[inline(always)]
    pub fn set_ci_savedpc(&mut self, idx: CallInfoIdx, pc: u32) {
        self.call_info[idx.as_usize()].set_saved_pc(pc);
    }
    #[inline(always)]
    pub fn set_ci_previous(&mut self, idx: CallInfoIdx) {
        self.ci = self.call_info[idx.as_usize()]
            .previous
            .expect("set_ci_previous: returning frame has no previous CallInfo");
    }
    #[inline(always)]
    pub fn ci_previous(&self, idx: CallInfoIdx) -> Option<CallInfoIdx> { self.call_info[idx.as_usize()].previous }
    #[inline(always)]
    pub fn ci_adjust_func(&mut self, idx: CallInfoIdx, delta: i32) {
        let ci = &mut self.call_info[idx.as_usize()];
        ci.func = StackIdx((ci.func.0 as i32 - delta) as u32);
    }
    #[inline(always)]
    pub fn ci_base(&self, idx: CallInfoIdx) -> StackIdx { self.call_info[idx.as_usize()].func + 1 }
    #[inline(always)]
    pub fn ci_is_fresh(&self, idx: CallInfoIdx) -> bool {
        (self.call_info[idx.as_usize()].callstatus & CIST_FRESH) != 0
    }
    #[inline(always)]
    pub fn ci_lua_closure(&self, idx: CallInfoIdx) -> Option<GcRef<lua_types::closure::LuaLClosure>> {
        let func_idx = self.call_info[idx.as_usize()].func;
        match self.get_at(func_idx) {
            LuaValue::Function(lua_types::closure::LuaClosure::Lua(cl)) => Some(cl),
            _ => None,
        }
    }
    #[inline(always)]
    pub fn ci_nextraargs(&self, idx: CallInfoIdx) -> i32 {
        self.call_info[idx.as_usize()].nextra_args()
    }
    #[inline(always)]
    pub fn ci_nres(&self, idx: CallInfoIdx) -> i32 {
        self.call_info[idx.as_usize()].u2.value
    }
    #[inline(always)]
    pub fn ci_nres_set(&mut self, idx: CallInfoIdx, n: i32) {
        self.call_info[idx.as_usize()].u2.value = n;
    }
    #[inline(always)]
    pub fn ci_nresults(&self, idx: CallInfoIdx) -> i32 { self.call_info[idx.as_usize()].nresults as i32 }
    pub fn ci_prev_instruction(&self, idx: CallInfoIdx) -> lua_types::opcode::Instruction {
        let pc = self.call_info[idx.as_usize()].saved_pc();
        let cl = self.ci_lua_closure(idx)
            .expect("ci_prev_instruction: CallInfo does not hold a Lua closure");
        cl.proto.code[(pc - 1) as usize]
    }
    pub fn ci_prev2_instruction(&self, idx: CallInfoIdx) -> lua_types::opcode::Instruction {
        let pc = self.call_info[idx.as_usize()].saved_pc();
        let cl = self.ci_lua_closure(idx)
            .expect("ci_prev2_instruction: CallInfo does not hold a Lua closure");
        cl.proto.code[(pc - 2) as usize]
    }
    pub fn ci_skip_next_instruction(&mut self, idx: CallInfoIdx) {
        let pc = self.call_info[idx.as_usize()].saved_pc();
        self.call_info[idx.as_usize()].set_saved_pc(pc + 1);
    }
    pub fn ci_step_pc_back(&mut self, idx: CallInfoIdx) {
        let pc = self.call_info[idx.as_usize()].saved_pc();
        self.call_info[idx.as_usize()].set_saved_pc(pc - 1);
    }
    pub fn get_ci_pcrel(&mut self, idx: CallInfoIdx) -> u32 {
        self.call_info[idx.as_usize()].saved_pc().saturating_sub(1)
    }
    pub fn get_ci_u2_funcidx(&mut self, idx: CallInfoIdx) -> i32 {
        self.call_info[idx.as_usize()].u2.value
    }
    pub fn get_ci_u2_nres(&mut self, idx: CallInfoIdx) -> i32 {
        self.call_info[idx.as_usize()].u2.value
    }
    pub fn get_ci_u2_nyield(&mut self, idx: CallInfoIdx) -> i32 {
        self.call_info[idx.as_usize()].u2.value
    }
    pub fn get_ci_vararg_info(&mut self, idx: CallInfoIdx) -> (bool, i32, i32) {
        let nextraargs = self.call_info[idx.as_usize()].nextra_args();
        match self.ci_lua_closure(idx) {
            Some(cl) => (cl.proto.is_vararg, nextraargs, cl.proto.numparams as i32),
            None => (false, nextraargs, 0),
        }
    }
    pub fn get_ci_lua_proto_numparams(&mut self, idx: CallInfoIdx) -> u8 {
        self.ci_lua_closure(idx)
            .map(|cl| cl.proto.numparams)
            .unwrap_or(0)
    }
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
    pub fn _hook_call_noargs(&mut self) {}
    pub fn hook(&self) -> Option<&Box<dyn FnMut(&mut LuaState, &crate::debug::LuaDebug)>> {
        self.hook.as_ref()
    }
    pub fn has_hook(&mut self) -> bool { self.hook.is_some() }
    pub fn hook_count(&mut self) -> i32 { self.hookcount }
    pub fn set_hook_count(&mut self, n: i32) { self.hookcount = n; }
    pub fn hook_mask(&self) -> u8 { self.hookmask }
    pub fn set_hook_mask(&mut self, m: u8) { self.hookmask = m; }
    pub fn base_hook_count(&self) -> i32 { self.basehookcount }
    pub fn set_base_hook_count(&mut self, n: i32) { self.basehookcount = n; }
    pub fn set_hook(&mut self, h: Option<Box<dyn FnMut(&mut LuaState, &crate::debug::LuaDebug)>>) {
        self.hook = h;
    }
    pub fn call_hook_event(&mut self, event: i32, line: i32) -> Result<(), LuaError> {
        crate::do_::hook(self, event, line, 0, 0)
    }

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
        self.mark_gc_check_needed();
        GcRef::new(LuaProto::placeholder())
    }

    /// Allocate a Lua-side closure (compiled function + upvalue slots).
    pub fn new_lclosure(&mut self, proto: GcRef<LuaProto>, nupvals: usize) -> GcRef<LuaClosureLua> {
        self.mark_gc_check_needed();
        let mut upvals = Vec::with_capacity(nupvals);
        for _ in 0..nupvals {
            upvals.push(std::cell::Cell::new(self.new_upval_closed(LuaValue::Nil)));
        }
        GcRef::new(LuaClosureLua { proto, upvals })
    }

    /// Allocate a closed upvalue holding the given value.
    pub fn new_upval_closed(&mut self, v: LuaValue) -> GcRef<UpVal> {
        self.mark_gc_check_needed();
        GcRef::new(UpVal::closed(v))
    }

    /// Allocate an open upvalue referring to a thread's stack slot.
    pub fn new_upval_open(&mut self, thread_id: usize, level: StackIdx) -> GcRef<UpVal> {
        self.mark_gc_check_needed();
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
    pub fn new_userdata(&mut self, _size: usize, _nuvalue: usize) -> Result<GcRef<LuaUserData>, LuaError> {
        Err(LuaError::runtime(format_args!("new_userdata not implemented in this Phase-B build; use new_userdata_typed instead")))
    }
    pub fn new_c_closure(&mut self, _f: LuaCFunction, _n: i32) -> Result<LuaClosure, LuaError> {
        Err(LuaError::runtime(format_args!("new_c_closure not implemented in this Phase-B build; use push_cclosure in lua_vm::api instead")))
    }
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
        let mut upvals: Vec<std::cell::Cell<GcRef<UpVal>>> = Vec::with_capacity(nup);
        for i in 0..nup {
            let desc = &child_proto.upvalues[i];
            let uv = if desc.instack {
                let level = base + desc.idx as i32;
                crate::func::find_upval(self, level)
            } else {
                parent_cl.upval(desc.idx as usize)
            };
            upvals.push(std::cell::Cell::new(uv));
        }
        // TODO(D-1c-bridge): upvals are pre-populated from parent frame; state.new_lclosure
        // fills with fresh Nil upvals which would drop the captured bindings.
        self.mark_gc_check_needed();
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

    /// Read an open or closed upvalue.
    ///
    /// Closed upvalues own their value and read trivially. Open upvalues
    /// point at a stack slot on the home thread that captured them.
    ///
    /// Resolution order for an open upvalue whose home is not the current
    /// thread:
    ///
    /// 1. If the home thread is registered in `GlobalState::threads` and
    ///    its `RefCell` is currently borrowable, read straight from its
    ///    stack. This is the path used when the main thread reads a
    ///    closure created inside a now-suspended coroutine, or when one
    ///    coroutine reads an upvalue homed on a sibling suspended
    ///    coroutine.
    /// 2. Otherwise fall back to `GlobalState::cross_thread_upvals`. This
    ///    is the path used while inside a `coroutine.resume`: the parent
    ///    thread's `LuaState` is held by an outer `&mut` and is not
    ///    reachable through any `Rc<RefCell<_>>`, so `aux_resume`
    ///    snapshots the parent's open upvalues into the mirror across the
    ///    resume boundary.
    #[inline(always)]
    pub fn upvalue_get(&self, cl: &GcRef<LuaClosureLua>, n: usize) -> LuaValue {
        let uv = cl.upval(n);
        let (thread_id, idx) = match uv.try_open_payload() {
            Some(p) => p,
            None => return *uv.closed_value(),
        };
        let current = self.cached_thread_id;
        let tid = thread_id as u64;
        if tid == current {
            return self.stack[idx.0 as usize].val;
        }
        self.upvalue_get_cross_thread(tid, idx)
    }

    #[cold]
    #[inline(never)]
    fn upvalue_get_cross_thread(&self, tid: u64, idx: StackIdx) -> LuaValue {
        let entry_rc = {
            let g = self.global();
            g.threads.get(&tid).map(|e| e.state.clone())
        };
        if let Some(rc) = entry_rc {
            if let Ok(home_state) = rc.try_borrow() {
                return home_state.get_at(idx);
            }
        }
        let g = self.global();
        match g.cross_thread_upvals.get(&(tid, idx)) {
            Some(v) => *v,
            None => LuaValue::Nil,
        }
    }
    /// Write an open or closed upvalue.
    ///
    /// Mirrors [`upvalue_get`]: open upvalues homed on the current thread
    /// write through `self.stack`. For cross-thread open upvalues, the
    /// home thread's stack is written directly when its `RefCell` is
    /// borrowable, otherwise the write lands in
    /// `GlobalState::cross_thread_upvals` (the active-resume case where
    /// the home thread is borrow-locked further up the call stack).
    #[inline(always)]
    pub fn upvalue_set(&mut self, cl: &GcRef<LuaClosureLua>, n: usize, val: LuaValue) -> Result<(), LuaError> {
        let uv = cl.upval(n);
        match uv.try_open_payload() {
            Some((thread_id, idx)) => {
                let tid = thread_id as u64;
                let current = self.cached_thread_id;
                if tid == current {
                    self.stack[idx.0 as usize].val = val;
                    return Ok(());
                }
                return self.upvalue_set_cross_thread(tid, idx, val);
            }
            None => {
                uv.set_closed_value(val);
            }
        }
        Ok(())
    }

    #[cold]
    #[inline(never)]
    fn upvalue_set_cross_thread(
        &mut self,
        tid: u64,
        idx: StackIdx,
        val: LuaValue,
    ) -> Result<(), LuaError> {
        let entry_rc = {
            let g = self.global();
            g.threads.get(&tid).map(|e| e.state.clone())
        };
        if let Some(rc) = entry_rc {
            if let Ok(mut home_state) = rc.try_borrow_mut() {
                home_state.set_at(idx, val);
                return Ok(());
            }
        }
        let mut g = self.global_mut();
        g.cross_thread_upvals.insert((tid, idx), val);
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
    #[inline(always)]
    pub fn precall(&mut self, func: StackIdx, nresults: i32) -> Result<Option<CallInfoIdx>, LuaError> {
        crate::do_::precall(self, func, nresults)
    }
    #[inline(always)]
    pub fn pretailcall(
        &mut self,
        ci: CallInfoIdx,
        func: StackIdx,
        narg1: i32,
        delta: i32,
    ) -> Result<i32, LuaError> {
        crate::do_::pretailcall(self, ci, func, narg1, delta)
    }
    #[inline(always)]
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
        // metatable then index by the interned `__xxx` name.
        let mt = u.metatable();
        self.fast_tm_table(mt.as_ref(), tm)
    }

    pub fn table_get_with_tm(&mut self, t: &LuaValue, k: &LuaValue) -> Result<LuaValue, LuaError> {
        // Fast path: when the table has no metatable, `__index` can never
        // fire — so we can return the raw slot value (Nil if absent) without
        // routing through finish_get's push/pop scaffolding. Halves the
        // get-hot-path cost on tables without metamethods, which is the
        // common case in table.remove/insert shift loops and most user code.
        if let LuaValue::Table(tbl) = t {
            if tbl.metatable().is_none() {
                return Ok(tbl.get(k));
            }
        }
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
    /// Set `t[k] = v` with `__newindex` metamethod awareness.
    ///
    /// Fast path: when the table has no metatable, `__newindex` can never
    /// fire, so the existence check via `fast_get` is pure waste —
    /// `try_raw_set` handles both "key exists" and "key absent" cases via
    /// a single lookup internally. Removing the `fast_get` halves the
    /// lookups per set on the metamethod-free path (table.remove/insert
    /// hot loops, most user code).
    ///
    /// The GC backward barrier is invoked before the store (with `&v`)
    /// instead of after; the barrier only inspects the value's color, not
    /// its location, so the order is semantically equivalent to upstream
    /// C-Lua and lets us move `v` straight into `table_raw_set` without
    /// the extra `v.clone()` that the post-store ordering forced.
    #[inline]
    pub fn table_set_with_tm(&mut self, t: &LuaValue, k: LuaValue, v: LuaValue) -> Result<(), LuaError> {
        if let LuaValue::Table(tbl) = t {
            if tbl.metatable().is_none() {
                self.gc_barrier_back(t, &v);
                return self.table_raw_set(t, k, v);
            }
        }
        if self.fast_get(t, &k)?.is_some() {
            self.gc_barrier_back(t, &v);
            return self.table_raw_set(t, k, v);
        }
        crate::vm::finish_set(self, t.clone(), k, v, true, None, None)
    }
    #[inline]
    pub fn table_raw_set(&mut self, t: &LuaValue, k: LuaValue, v: LuaValue) -> Result<(), LuaError> {
        let LuaValue::Table(tbl) = t else {
            return Err(LuaError::type_error(t, "index"));
        };
        let tbl = tbl.clone();
        tbl.raw_set(self, k, v)
    }
    #[inline]
    pub fn table_array_set(&mut self, t: &LuaValue, idx: usize, v: LuaValue) -> Result<(), LuaError> {
        let LuaValue::Table(tbl) = t else {
            return Err(LuaError::type_error(t, "index"));
        };
        let tbl = tbl.clone();
        tbl.raw_set_int(self, idx as i64 + 1, v)
    }
    pub fn table_ensure_array(&mut self, t: &LuaValue, n: usize) -> Result<(), LuaError> {
        let LuaValue::Table(tbl) = t else {
            return Err(LuaError::type_error(t, "index"));
        };
        if n > tbl.array_len() {
            tbl.resize(self, n, 0)?;
        }
        Ok(())
    }
    pub fn table_length(&mut self, t: &LuaValue) -> Result<i64, LuaError> {
        let LuaValue::Table(tbl) = t else {
            return Err(LuaError::type_error(t, "get length of"));
        };
        Ok(tbl.getn() as i64)
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
    pub fn table_resize(&mut self, t: &GcRef<LuaTable>, na: usize, nh: usize) -> Result<(), LuaError> {
        self.mark_gc_check_needed();
        t.resize(self, na, nh)
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

    #[inline(always)]
    pub fn proto_code(&self, cl: &GcRef<lua_types::closure::LuaLClosure>, pc: u32) -> lua_types::opcode::Instruction {
        cl.proto.code[pc as usize]
    }
    #[inline(always)]
    pub fn proto_const(&self, cl: &GcRef<lua_types::closure::LuaLClosure>, idx: usize) -> LuaValue {
        cl.proto.k[idx].clone()
    }
    /// Hot-path accessor: returns `Some(i)` only when the constant pool entry
    /// at `idx` is an `Int`. Avoids the full `LuaValue` clone that
    /// `proto_const` performs.
    ///
    /// arithmetic opcode macros (`op_arithK`).
    #[inline(always)]
    pub fn proto_const_int(&self, cl: &GcRef<lua_types::closure::LuaLClosure>, idx: usize) -> Option<i64> {
        match &cl.proto.k[idx] {
            LuaValue::Int(v) => Some(*v),
            _ => None,
        }
    }
    /// Hot-path accessor: returns `Some(f)` for `Float(f)` or `Int(i)` (coerced)
    /// constants. Avoids the full `LuaValue` clone. Used by the float fast
    /// path of `OP_ADDK`/`OP_SUBK`/`OP_MULK`/`OP_DIVK`/`OP_POWK`.
    #[inline(always)]
    pub fn proto_const_num(&self, cl: &GcRef<lua_types::closure::LuaLClosure>, idx: usize) -> Option<f64> {
        match &cl.proto.k[idx] {
            LuaValue::Float(f) => Some(*f),
            LuaValue::Int(v) => Some(*v as f64),
            _ => None,
        }
    }
    pub fn get_proto_instr(&self, ci: CallInfoIdx, pc: u32) -> lua_types::opcode::Instruction {
        let cl = self.ci_lua_closure(ci)
            .expect("get_proto_instr: CallInfo does not hold a Lua closure");
        cl.proto.code[pc as usize]
    }
    /// flag as `bool` (C returns `int` 0/1).
    ///
    /// The C function reads `L->ci` directly, so the `_idx` argument is unused;
    /// the VM passes its locally tracked `ci` for symmetry with `trace_exec`.
    pub fn trace_call(&mut self, _idx: CallInfoIdx) -> Result<bool, LuaError> {
        Ok(crate::debug::trace_call(self)? != 0)
    }
    /// returning `bool` for the trap flag. `_idx` is unused for the same reason
    /// as `trace_call`; `pc` is the 0-based index of the next instruction.
    pub fn trace_exec(&mut self, _idx: CallInfoIdx, pc: u32) -> Result<bool, LuaError> {
        Ok(crate::debug::trace_exec(self, pc)? != 0)
    }
    pub fn hook_call(&mut self, idx: CallInfoIdx) -> Result<(), LuaError> {
        crate::do_::hookcall(self, idx)
    }
    #[inline(always)]
    fn gc_step_flags(&self) -> Option<(bool, bool)> {
        let g = self.global();
        if !g.is_gc_running() {
            return None;
        }
        let should_collect = g.heap.would_collect();
        let has_finalizers = !g.to_be_finalized.is_empty();
        if should_collect || has_finalizers {
            Some((should_collect, has_finalizers))
        } else {
            None
        }
    }

    #[inline(always)]
    fn should_check_gc(&mut self) -> bool {
        if self.gc_check_needed {
            return true;
        }
        if !self.global().to_be_finalized.is_empty() {
            self.gc_check_needed = true;
            return true;
        }
        false
    }

    #[inline(always)]
    pub(crate) fn mark_gc_check_needed(&mut self) {
        self.gc_check_needed = true;
    }

    #[inline(always)]
    pub fn gc_check_step(&mut self) {
        if !self.allowhook {
            return;
        }
        if !self.should_check_gc() {
            return;
        }
        let Some((should_collect, has_finalizers)) = self.gc_step_flags() else {
            self.gc_check_needed = false;
            return;
        };
        if should_collect || has_finalizers {
            if should_collect {
                self.gc().check_step();
            }
            crate::api::run_pending_finalizers(self);
            self.gc_check_needed = true;
        }
        let should_keep_checking = {
            let g = self.global();
            g.heap.would_collect() || !g.to_be_finalized.is_empty()
        };
        self.gc_check_needed = should_keep_checking;
    }
    #[inline(always)]
    pub fn gc_cond_step(&mut self) {
        if !self.allowhook {
            return;
        }
        if !self.should_check_gc() {
            return;
        }
        let Some((should_collect, has_finalizers)) = self.gc_step_flags() else {
            self.gc_check_needed = false;
            return;
        };
        if should_collect || has_finalizers {
            if should_collect {
                self.gc().check_step();
            }
            crate::api::run_pending_finalizers(self);
            self.gc_check_needed = true;
        }
        let should_keep_checking = {
            let g = self.global();
            g.heap.would_collect() || !g.to_be_finalized.is_empty()
        };
        self.gc_check_needed = should_keep_checking;
    }
    pub fn gc_barrier_back<T, U>(&mut self, _t: T, _v: U) { /* phase-b no-op */ }
    pub fn gc_barrier_upval<T, U, V>(&mut self, _cl: T, _uv: U, _v: V) { /* phase-b no-op */ }
    ///
    /// Phase E-1: compares `GlobalState::current_thread_id` against
    /// `main_thread_id`. Coroutine resume (slice 02b) is what will swap
    /// `current_thread_id` in and out; until then the running thread is
    /// always the main thread and this returns `true`.
    pub fn is_main_thread(&mut self) -> bool {
        let g = self.global();
        g.current_thread_id == g.main_thread_id
    }
    pub fn obj_type_name<'v>(&self, v: &'v LuaValue) -> std::borrow::Cow<'static, [u8]> {
        match v {
            LuaValue::LightUserData(_) => std::borrow::Cow::Borrowed(b"light userdata"),
            LuaValue::Table(t) => {
                if let Some(mt) = t.metatable() {
                    if let LuaValue::Str(s) = mt.get_str_bytes(b"__name") {
                        return std::borrow::Cow::Owned(s.as_bytes().to_vec());
                    }
                }
                std::borrow::Cow::Borrowed(crate::tagmethods::type_name(v.base_type()))
            }
            LuaValue::UserData(u) => {
                if let Some(mt) = u.metatable() {
                    if let LuaValue::Str(s) = mt.get_str_bytes(b"__name") {
                        return std::borrow::Cow::Owned(s.as_bytes().to_vec());
                    }
                }
                std::borrow::Cow::Borrowed(crate::tagmethods::type_name(v.base_type()))
            }
            _ => std::borrow::Cow::Borrowed(crate::tagmethods::type_name(v.base_type())),
        }
    }

    pub fn full_type_name(&mut self, v: &LuaValue) -> Result<Vec<u8>, LuaError> {
        crate::tagmethods::obj_type_name(self, v)
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

/// Composite root passed to `Heap::full_collect`. The Phase-A workaround in
/// `new_state` leaves `GlobalState.mainthread = None` (to break the
/// self-referential Rc cycle pre-D), so the running thread's stack and
/// openupval list are not reachable from `GlobalState::trace`. Wrapping both
/// references in a single `Trace`-implementing root injects the active
/// thread as a second mark source for the duration of the collection.
struct CollectRoots<'a> {
    global: &'a GlobalState,
    thread: &'a LuaState,
}

impl<'a> lua_gc::Trace for CollectRoots<'a> {
    fn trace(&self, m: &mut lua_gc::Marker) {
        self.global.trace(m);
        self.thread.trace(m);
    }
}

fn trace_reachable_threads(
    global: &GlobalState,
    _current_thread_id: u64,
    marker: &mut lua_gc::Marker,
) {
    use lua_gc::Trace;

    loop {
        let visited_before = marker.visited_count();
        for (id, entry) in global.threads.iter() {
            if thread_entry_marked_alive(marker, *id, entry) {
                if let Ok(thread) = entry.state.try_borrow() {
                    thread.trace(marker);
                }
            }
        }
        marker.drain_gray_queue();
        if marker.visited_count() == visited_before {
            break;
        }
    }
}

fn thread_entry_marked_alive(
    marker: &lua_gc::Marker,
    id: u64,
    entry: &ThreadRegistryEntry,
) -> bool {
    marker.is_visited(entry.value.identity()) && entry.value.id == id
}

fn close_open_upvalues_for_unreachable_threads(
    global: &GlobalState,
    marker: &mut lua_gc::Marker,
) {
    use lua_gc::Trace;

    let mut closed_values = Vec::<LuaValue>::new();
    for (id, entry) in global.threads.iter() {
        if entry.value.id != *id {
            continue;
        }
        if thread_entry_marked_alive(marker, *id, entry) {
            continue;
        }
        let Ok(thread) = entry.state.try_borrow() else {
            continue;
        };
        for uv in thread.openupval.iter() {
            if !marker.is_visited(uv.identity()) {
                continue;
            }
            let Some((thread_id, idx)) = uv.try_open_payload() else {
                continue;
            };
            if thread_id as u64 != *id {
                continue;
            }
            let value = thread.get_at(idx);
            uv.close_with(value.clone());
            closed_values.push(value);
        }
    }
    for value in closed_values {
        value.trace(marker);
    }
    marker.drain_gray_queue();
}

impl<'a> GcHandle<'a> {
    /// macros.tsv: `luaC_checkGC → state.gc().check_step()`
    ///
    /// Phase D-2: drives implicit collection when the heap's byte threshold
    /// is exceeded. Without this hook, loops that allocate without an
    /// explicit `collectgarbage()` call (e.g. `closure.lua`'s
    /// `while x[1] do local a = A..A end` GC-driven loop) never settle.
    pub fn check_step(&self) {
        if !self._state.global().is_gc_running() {
            return;
        }
        self.collect_via_heap(/* force = */ false);
    }

    /// macros.tsv: `luaC_fullgc → state.gc().full_collect()`
    pub fn full_collect(&self) {
        self.collect_via_heap(/* force = */ true);
    }

    /// Shared driver behind both `full_collect` (force-collect) and
    /// `check_step` (collect only if heap byte threshold exceeded).
    ///
    /// Snapshots the weak-tables registry, invokes the heap's collect path
    /// with a post-mark weak-prune hook, and rebuilds the registry by
    /// retaining only entries whose target was reachable. The same hook
    /// works for both modes — the heap short-circuits when force=false and
    /// the threshold isn't met.
    fn collect_via_heap(&self, force: bool) {
        use lua_gc::Trace;
        let state_ref: &LuaState = &*self._state;

        // Fast path: when the caller did not force a collection, skip all
        // the snapshot work (3 Vec allocations + 3 HashSet allocations) if
        // the heap is paused or under threshold — a `step()` in that state
        // is a no-op, so the snapshot would be pure waste. Called millions
        // of times per recursive workload via `gc_check_step` in `precall`.
        if !force {
            let g = state_ref.global.borrow();
            if !g.heap.would_collect() {
                return;
            }
        }

        // Snapshot weak tables BEFORE the collect. `identity()` reads only
        // the pointer address — safe even on still-dangling weak handles —
        // and dedup by identity keeps the iteration linear.
        let weak_tables_snapshot: Vec<lua_types::gc::GcRef<lua_types::value::LuaTable>> = {
            let g = state_ref.global.borrow();
            let mut seen = std::collections::HashSet::<usize>::new();
            g.weak_tables_registry
                .iter()
                .filter_map(|w| w.upgrade())
                .filter(|t| seen.insert(t.identity()))
                .collect()
        };

        // Snapshot pending finalizers. `GlobalState::trace` deliberately
        // does NOT root these — that's how the post-mark hook below can
        // distinguish "still reachable from program state" from "only kept
        // alive by the finalizer registry."
        let pending_snapshot: Vec<lua_types::gc::GcRef<lua_types::value::LuaTable>> = {
            let g = state_ref.global.borrow();
            g.pending_finalizers.clone()
        };

        // Snapshot tracked long-string identities + byte sizes BEFORE the
        // collect. The post-mark hook compares each identity against the
        // marker's visited set; anything not visited is unreachable and
        // its bytes get reclaimed from `gc_debt` after the heap collect
        // returns. Bare `usize` is safe to carry across the hook — long
        // strings use `new_uncollected` so the pointer never dangles.
        let long_string_snapshot: Vec<(usize, usize)> = {
            let g = state_ref.global.borrow();
            g.gc_tracked_long_strings
                .iter()
                .map(|(w, sz)| (w.0.identity(), *sz))
                .collect()
        };

        let alive_ids: std::cell::RefCell<std::collections::HashSet<usize>> =
            std::cell::RefCell::new(std::collections::HashSet::new());
        let newly_unreachable: std::cell::RefCell<Vec<lua_types::gc::GcRef<lua_types::value::LuaTable>>> =
            std::cell::RefCell::new(Vec::new());
        let dead_long_strings: std::cell::RefCell<std::collections::HashSet<usize>> =
            std::cell::RefCell::new(std::collections::HashSet::new());
        let alive_thread_ids: std::cell::RefCell<std::collections::HashSet<u64>> =
            std::cell::RefCell::new(std::collections::HashSet::new());
        let collect_ran = std::cell::Cell::new(false);

        {
            let global = state_ref.global.borrow();
            global.heap.unpause();
            let roots = CollectRoots { global: &*global, thread: state_ref };
            let hook = |marker: &mut lua_gc::Marker| {
                collect_ran.set(true);
                trace_reachable_threads(&*global, global.current_thread_id, marker);
                close_open_upvalues_for_unreachable_threads(&*global, marker);
                loop {
                    let visited_before = marker.visited_count();
                    for t in &weak_tables_snapshot {
                        let t_id = t.identity();
                        if !marker.is_visited(t_id) {
                            continue;
                        }
                        let to_mark = t.ephemeron_values_to_mark(
                            &|id| marker.is_visited(id),
                        );
                        for v in &to_mark {
                            v.trace(marker);
                        }
                    }
                    marker.drain_gray_queue();
                    if marker.visited_count() == visited_before {
                        break;
                    }
                }
                for pf in &pending_snapshot {
                    if !marker.is_visited(pf.identity()) {
                        marker.mark(pf.0);
                        newly_unreachable.borrow_mut().push(pf.clone());
                    }
                }
                marker.drain_gray_queue();
                loop {
                    let visited_before = marker.visited_count();
                    for t in &weak_tables_snapshot {
                        let t_id = t.identity();
                        if !marker.is_visited(t_id) {
                            continue;
                        }
                        let to_mark = t.ephemeron_values_to_mark(
                            &|id| marker.is_visited(id),
                        );
                        for v in &to_mark {
                            v.trace(marker);
                        }
                    }
                    marker.drain_gray_queue();
                    if marker.visited_count() == visited_before {
                        break;
                    }
                }
                for t in &weak_tables_snapshot {
                    let id = t.identity();
                    if marker.is_visited(id) {
                        let to_mark = t.prune_weak_dead(&|id| marker.is_visited(id));
                        for v in &to_mark {
                            v.trace(marker);
                        }
                        alive_ids.borrow_mut().insert(id);
                    }
                }
                marker.drain_gray_queue();
                // Long-string Phase-B reclaim. With `new_uncollected`
                // allocation, long strings never enter the heap's sweep
                // path, so we rely on the marker's visited set: any
                // tracked long-string identity that wasn't reached by mark
                // is unreferenced and its bytes can be returned to
                // `gc_debt`. Done here (inside the hook) so it sees the
                // visited set BEFORE drop of the marker.
                {
                    let mut dead = dead_long_strings.borrow_mut();
                    for (id, _sz) in &long_string_snapshot {
                        if !marker.is_visited(*id) {
                            dead.insert(*id);
                        }
                    }
                }
                {
                    let mut alive = alive_thread_ids.borrow_mut();
                    for (id, entry) in global.threads.iter() {
                        if thread_entry_marked_alive(marker, *id, entry) {
                            alive.insert(*id);
                        }
                    }
                }
            };
            if force {
                global.heap.full_collect_with_post_mark(&roots, hook);
            } else {
                global.heap.step_with_post_mark(&roots, hook);
            }
        }

        if !collect_ran.get() {
            return;
        }

        // After collect, drop weak-table-registry entries whose target was
        // swept. Without this filter the registry leaks one dangling
        // `GcWeak<LuaTable>` per dead weak table; the next collect would
        // upgrade those handles (current placeholder GcWeak always returns
        // Some) and the prune walk would deref freed memory.
        let alive_set = alive_ids.into_inner();
        let promote: Vec<lua_types::gc::GcRef<lua_types::value::LuaTable>> =
            newly_unreachable.into_inner();
        let promote_ids: std::collections::HashSet<usize> =
            promote.iter().map(|t| t.identity()).collect();
        let dead_ls_ids = dead_long_strings.into_inner();
        let alive_thread_ids = alive_thread_ids.into_inner();
        let mut g = state_ref.global.borrow_mut();
        g.weak_tables_registry
            .retain(|w| alive_set.contains(&w.0.identity()));
        let main_thread_id = g.main_thread_id;
        g.threads.retain(|id, _| alive_thread_ids.contains(id));
        g.cross_thread_upvals
            .retain(|(id, _), _| *id == main_thread_id || alive_thread_ids.contains(id));
        // Move newly-unreachable finalizables from `pending_finalizers` to
        // `to_be_finalized`. The latter is rooted by `GlobalState::trace`,
        // so these tables remain alive until their `__gc` runs.
        g.pending_finalizers
            .retain(|t| !promote_ids.contains(&t.identity()));
        g.to_be_finalized.extend(promote);
        // Reclaim long-string byte accounting for entries the marker said
        // were unreachable. The underlying `Gc<LuaString>` was allocated
        // via `new_uncollected` and stays live in process memory; only
        // `gc_debt` is adjusted so `collectgarbage("count")` reflects the
        // drop in user-visible live bytes.
        if !dead_ls_ids.is_empty() {
            let mut freed: isize = 0;
            g.gc_tracked_long_strings.retain(|(w, sz)| {
                if dead_ls_ids.contains(&w.0.identity()) {
                    freed += *sz as isize;
                    false
                } else {
                    true
                }
            });
            g.gc_debt -= freed;
        }
    }

    /// Phase-B stub for `luaC_step(L)`.
    pub fn step(&self) { /* phase-b no-op */ }

    /// Run one budgeted incremental step of the GC.
    ///
    /// `work_units` is the number of GC work units the step is allowed to
    /// perform (one gray trace, one sweep visit, or one phase transition).
    /// Returns `true` if the step completed a cycle and the collector is
    /// now in the `Pause` state; `false` otherwise.
    ///
    /// Mirrors `collect_via_heap` for the post-mark weak-table /
    /// finalizer-promotion logic, but only the atomic-phase transition will
    /// invoke the snapshot-walking hook — propagate and sweep steps reuse
    /// the snapshot but never execute it. The snapshot is rebuilt on every
    /// call; the cost is `O(weak_tables_registry)` per step.
    pub fn incremental_step(&self, work_units: isize) -> bool {
        use lua_gc::{StepBudget, StepOutcome, Trace};
        let state_ref: &LuaState = &*self._state;

        let weak_tables_snapshot: Vec<lua_types::gc::GcRef<lua_types::value::LuaTable>> = {
            let g = state_ref.global.borrow();
            let mut seen = std::collections::HashSet::<usize>::new();
            g.weak_tables_registry
                .iter()
                .filter_map(|w| w.upgrade())
                .filter(|t| seen.insert(t.identity()))
                .collect()
        };

        let pending_snapshot: Vec<lua_types::gc::GcRef<lua_types::value::LuaTable>> = {
            let g = state_ref.global.borrow();
            g.pending_finalizers.clone()
        };

        let long_string_snapshot: Vec<(usize, usize)> = {
            let g = state_ref.global.borrow();
            g.gc_tracked_long_strings
                .iter()
                .map(|(w, sz)| (w.0.identity(), *sz))
                .collect()
        };

        let alive_ids: std::cell::RefCell<std::collections::HashSet<usize>> =
            std::cell::RefCell::new(std::collections::HashSet::new());
        let newly_unreachable: std::cell::RefCell<Vec<lua_types::gc::GcRef<lua_types::value::LuaTable>>> =
            std::cell::RefCell::new(Vec::new());
        let dead_long_strings: std::cell::RefCell<std::collections::HashSet<usize>> =
            std::cell::RefCell::new(std::collections::HashSet::new());
        let alive_thread_ids: std::cell::RefCell<std::collections::HashSet<u64>> =
            std::cell::RefCell::new(std::collections::HashSet::new());
        let atomic_ran = std::cell::Cell::new(false);

        let outcome = {
            let global = state_ref.global.borrow();
            global.heap.unpause();
            let roots = CollectRoots { global: &*global, thread: state_ref };
            let hook = |marker: &mut lua_gc::Marker| {
                atomic_ran.set(true);
                trace_reachable_threads(&*global, global.current_thread_id, marker);
                close_open_upvalues_for_unreachable_threads(&*global, marker);
                loop {
                    let visited_before = marker.visited_count();
                    for t in &weak_tables_snapshot {
                        let t_id = t.identity();
                        if !marker.is_visited(t_id) {
                            continue;
                        }
                        let to_mark = t.ephemeron_values_to_mark(
                            &|id| marker.is_visited(id),
                        );
                        for v in &to_mark {
                            v.trace(marker);
                        }
                    }
                    marker.drain_gray_queue();
                    if marker.visited_count() == visited_before {
                        break;
                    }
                }
                for pf in &pending_snapshot {
                    if !marker.is_visited(pf.identity()) {
                        marker.mark(pf.0);
                        newly_unreachable.borrow_mut().push(pf.clone());
                    }
                }
                marker.drain_gray_queue();
                loop {
                    let visited_before = marker.visited_count();
                    for t in &weak_tables_snapshot {
                        let t_id = t.identity();
                        if !marker.is_visited(t_id) {
                            continue;
                        }
                        let to_mark = t.ephemeron_values_to_mark(
                            &|id| marker.is_visited(id),
                        );
                        for v in &to_mark {
                            v.trace(marker);
                        }
                    }
                    marker.drain_gray_queue();
                    if marker.visited_count() == visited_before {
                        break;
                    }
                }
                for t in &weak_tables_snapshot {
                    let id = t.identity();
                    if marker.is_visited(id) {
                        let to_mark = t.prune_weak_dead(&|id| marker.is_visited(id));
                        for v in &to_mark {
                            v.trace(marker);
                        }
                        alive_ids.borrow_mut().insert(id);
                    }
                }
                marker.drain_gray_queue();
                {
                    let mut dead = dead_long_strings.borrow_mut();
                    for (id, _sz) in &long_string_snapshot {
                        if !marker.is_visited(*id) {
                            dead.insert(*id);
                        }
                    }
                }
                {
                    let mut alive = alive_thread_ids.borrow_mut();
                    for (id, entry) in global.threads.iter() {
                        if thread_entry_marked_alive(marker, *id, entry) {
                            alive.insert(*id);
                        }
                    }
                }
            };
            let budget = StepBudget::from_work(work_units);
            global.heap.incremental_step_with_post_mark(&roots, budget, hook)
        };

        if atomic_ran.get() {
            let alive_set = alive_ids.into_inner();
            let promote: Vec<lua_types::gc::GcRef<lua_types::value::LuaTable>> =
                newly_unreachable.into_inner();
            let promote_ids: std::collections::HashSet<usize> =
                promote.iter().map(|t| t.identity()).collect();
            let dead_ls_ids = dead_long_strings.into_inner();
            let alive_thread_ids = alive_thread_ids.into_inner();
            let mut g = state_ref.global.borrow_mut();
            g.weak_tables_registry
                .retain(|w| alive_set.contains(&w.0.identity()));
            let main_thread_id = g.main_thread_id;
            g.threads.retain(|id, _| alive_thread_ids.contains(id));
            g.cross_thread_upvals
                .retain(|(id, _), _| *id == main_thread_id || alive_thread_ids.contains(id));
            g.pending_finalizers
                .retain(|t| !promote_ids.contains(&t.identity()));
            g.to_be_finalized.extend(promote);
            if !dead_ls_ids.is_empty() {
                let mut freed: isize = 0;
                g.gc_tracked_long_strings.retain(|(w, sz)| {
                    if dead_ls_ids.contains(&w.0.identity()) {
                        freed += *sz as isize;
                        false
                    } else {
                        true
                    }
                });
                g.gc_debt -= freed;
            }
        }

        matches!(outcome, StepOutcome::Paused)
    }

    /// Run only the weak-table atomic cleanup used by a generational step.
    ///
    /// C-Lua's `genstep` performs young/full generational work and includes
    /// weak-table clearing at the atomic boundary. This heap does not model
    /// ages yet; this mark-only pass gives explicit generational steps the
    /// weak cleanup they need without sweeping objects from suspended threads.
    pub fn prune_weak_tables_mark_only(&self) {
        use lua_gc::Trace;
        let state_ref: &LuaState = &*self._state;

        let weak_tables_snapshot: Vec<lua_types::gc::GcRef<lua_types::value::LuaTable>> = {
            let g = state_ref.global.borrow();
            let mut seen = std::collections::HashSet::<usize>::new();
            g.weak_tables_registry
                .iter()
                .filter_map(|w| w.upgrade())
                .filter(|t| seen.insert(t.identity()))
                .collect()
        };

        let global = state_ref.global.borrow();
        global.heap.unpause();
        let roots = CollectRoots { global: &*global, thread: state_ref };
        let hook = |marker: &mut lua_gc::Marker| {
            trace_reachable_threads(&*global, global.current_thread_id, marker);
            loop {
                let visited_before = marker.visited_count();
                for t in &weak_tables_snapshot {
                    let t_id = t.identity();
                    if !marker.is_visited(t_id) {
                        continue;
                    }
                    let to_mark = t.ephemeron_values_to_mark(
                        &|id| marker.is_visited(id),
                    );
                    for v in &to_mark {
                        v.trace(marker);
                    }
                }
                marker.drain_gray_queue();
                if marker.visited_count() == visited_before {
                    break;
                }
            }
            for t in &weak_tables_snapshot {
                if marker.is_visited(t.identity()) {
                    let to_mark = t.prune_weak_dead(&|id| marker.is_visited(id));
                    for v in &to_mark {
                        v.trace(marker);
                    }
                }
            }
        };
        global.heap.mark_only_with_post_mark(&roots, hook);
    }

    /// Set the GC kind (incremental/generational).
    ///
    /// itself is `Rc`-based, so the only observable effect is the mode flag
    /// returned by `lua_gc(LUA_GCGEN)` / `lua_gc(LUA_GCINC)` on the next call.
    pub fn change_mode(&self, mode: GcKind) {
        self._state.global_mut().gckind = mode as u8;
    }

    /// Phase-B stub for `luaC_fix(L, o)` — pin an object so GC won't collect it.
    pub fn fix_object<T: lua_gc::Trace + 'static>(&self, _o: &GcRef<T>) { /* phase-b no-op */ }

    /// Free all collectable objects (called during state teardown).
    ///
    /// PORT NOTE: In Phases A–C, Rc drop chains handle deallocation automatically.
    pub fn free_all_objects(&self) {
        // PORT NOTE: Phase A–C no-op; Rc::drop handles deallocation
    }

    /// GC write barrier for a TValue.
    ///
    /// macros.tsv: `luaC_barrier → state.gc().barrier(p, v)` — no-op in Phases A–C
    pub fn barrier(&self, _p: &dyn std::any::Any, _v: &LuaValue) {}

    /// Backward write barrier.
    ///
    /// macros.tsv: `luaC_barrierback → state.gc().barrier_back(p, v)` — no-op
    pub fn barrier_back(&self, _p: &dyn std::any::Any, _v: &LuaValue) {}

    /// Object write barrier.
    ///
    /// macros.tsv: `luaC_objbarrier → state.gc().obj_barrier(p, o)` — no-op
    pub fn obj_barrier(&self, _p: &dyn std::any::Any, _o: &dyn std::any::Any) {}

    /// Backward object write barrier.
    ///
    pub fn obj_barrier_back(&self, _p: &dyn std::any::Any, _o: &dyn std::any::Any) {}
}

// ─── Functions from lstate.c ──────────────────────────────────────────────────

//
// PORT NOTE: `luai_makeseed` in C mixed ASLR entropy (pointer addresses of a
// heap var, stack var, and code symbol) with the current time via `luaS_hash`.
// In Rust, raw pointer addresses require `unsafe` which is forbidden outside
// lua-gc/lua-coro. Native builds use time-only entropy for now; bare WASM uses
// a fixed seed so state creation never touches a stubbed host clock.
fn make_seed() -> u32 {
    #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
    {
        return crate::string::hash_bytes(b"lua-rs-wasm-seed", 0x9e37_79b9);
    }

    #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
    {
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

        // For Phase A, just hash the time bytes against itself.
        crate::string::hash_bytes(&t.to_le_bytes(), t)
    }
}

/// Adjust `GCdebt` to `debt` while preserving the `totalbytes + GCdebt` invariant.
///
///
/// ```c
///
/// //   l_mem tb = gettotalbytes(g);
/// //   lua_assert(tb > 0);
/// //   if (debt < tb - MAX_LMEM)
/// //     debt = tb - MAX_LMEM;
/// //   g->totalbytes = tb - debt;
/// //   g->GCdebt = debt;
/// // }
/// ```
pub(crate) fn set_debt(g: &mut GlobalState, mut debt: isize) {
    let tb = g.total_bytes() as isize;
    debug_assert!(tb > 0);
    // macros.tsv: MAX_LMEM → isize::MAX
    if debt < tb.saturating_sub(isize::MAX) {
        debt = tb - isize::MAX;
    }
    g.totalbytes = tb - debt;
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
///
/// ```c
///
/// //   UNUSED(L); UNUSED(limit);
/// //   return LUAI_MAXCCALLS;  /* warning?? */
/// // }
/// ```
pub fn set_c_stack_limit(_state: &mut LuaState, _limit: u32) -> i32 {
    let _ = (_state, _limit);
    LUAI_MAXCCALLS as i32
}

/// Allocate a fresh `CallInfo` beyond the current frame and return its index.
///
///
/// ```c
///
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
    debug_assert!(
        state.call_info[state.ci.0 as usize].next.is_none(),
        "extend_ci: current ci already has a cached next frame"
    );

    let current_idx = state.ci;
    // macros.tsv: luaM_new → Box::new(T::default()) — here we push onto the Vec
    let new_idx = CallInfoIdx(state.call_info.len() as u32);

    state.call_info.push(CallInfo {
        previous: Some(current_idx),
        next: None,
        u: CallInfoFrame::lua_default(),
        ..CallInfo::default()
    });

    state.call_info[current_idx.0 as usize].next = Some(new_idx);

    state.nci += 1;

    new_idx
}

/// Free all cached (unused) `CallInfo` frames beyond the current frame.
///
///
/// ```c
///
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

    let mut next_opt = state.call_info[ci_idx].next.take();

    while let Some(idx) = next_opt {
        next_opt = state.call_info[idx.0 as usize].next;
        state.nci = state.nci.saturating_sub(1);
    }

    // Truncate: drop all entries beyond the current ci.
    // TODO(port): verify invariant that all cached frames have contiguous indices > state.ci
    state.call_info.truncate(ci_idx + 1);
}

/// Free approximately half of the cached `CallInfo` frames beyond the current frame.
///
///
/// ```c
///
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
///
/// ```c
///
/// //   if (getCcalls(L) == LUAI_MAXCCALLS)
/// //     luaG_runerror(L, "C stack overflow");
/// //   else if (getCcalls(L) >= (LUAI_MAXCCALLS / 10 * 11))
/// //     luaD_throw(L, LUA_ERRERR);
/// // }
/// ```
pub(crate) fn check_c_stack(state: &mut LuaState) -> Result<(), LuaError> {
    // macros.tsv: getCcalls → state.c_calls()
    // error_sites.tsv: luaG_runerror → return Err(LuaError::runtime(format_args!(...)))
    if state.c_calls() == LUAI_MAXCCALLS {
        return Err(LuaError::runtime(format_args!("C stack overflow")));
    }
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
///
/// ```c
///
/// //   L->n_ccalls++;
/// //   if (l_unlikely(getCcalls(L) >= LUAI_MAXCCALLS))
/// //     luaE_checkcstack(L);
/// // }
/// ```
pub fn inc_c_stack(state: &mut LuaState) -> Result<(), LuaError> {
    state.n_ccalls += 1;
    // macros.tsv: l_unlikely → x (drop branch hint); getCcalls → state.c_calls()
    if state.c_calls() >= LUAI_MAXCCALLS {
        check_c_stack(state)?;
    }
    Ok(())
}

//
// PORT NOTE: In C, `L` is a separate thread used only for memory allocation
// (via `luaM_newvector`).  In Rust we don't have a custom allocator; all
// allocation goes through the global Rust allocator.  The function takes only
// the new thread (`thread`) and ignores the caller.
fn stack_init(thread: &mut LuaState) {
    // macros.tsv: luaM_newvector → vec![T::default(); n]
    let total_slots = BASIC_STACK_SIZE + EXTRA_STACK;
    thread.stack = vec![StackValue::default(); total_slots];

    // types.tsv: lua_State.tbclist → Vec<StackIdx>
    // PORT NOTE: In C, tbclist.p = stack.p is a sentinel meaning "no tbc vars".
    // In Rust the Vec is empty when there are no tbc variables.
    thread.tbclist = Vec::new();

    //      setnilvalue(s2v(L1->stack.p + i));  /* erase new stack */
    // macros.tsv: setnilvalue → *o = LuaValue::Nil
    // Already initialized to LuaValue::Nil via StackValue::default().

    thread.top = StackIdx(0);

    thread.stack_last = StackIdx(BASIC_STACK_SIZE as u32);


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

    thread.stack[0] = StackValue { val: LuaValue::Nil, tbc_delta: 0 };

    thread.top = StackIdx(1);

    thread.ci = CallInfoIdx(0);
}

fn free_stack(state: &mut LuaState) {
    if state.stack.is_empty() {
        return;
    }
    state.ci = CallInfoIdx(0);
    free_ci(state);
    debug_assert_eq!(state.nci, 0, "nci should be 0 after free_ci");
    // macros.tsv: luaM_freearray → (Rust's Drop handles deallocation; drop the call)
    state.stack.clear();
    state.stack.shrink_to_fit();
}

fn init_registry(state: &mut LuaState) -> Result<(), LuaError> {
    // macros.tsv: luaH_new → state.new_table()
    let registry = state.new_table();

    // macros.tsv: sethvalue → *o = LuaValue::Table(x.clone())
    state.global_mut().l_registry = LuaValue::Table(registry.clone());

    // macros.tsv: luaH_resize → t.resize(state, na, nh)?
    // TODO(port): registry is a GcRef<LuaTable> (Rc); calling methods requires borrow_mut()
    // For Phase A, use RefCell interior mutability on LuaTable, or accept the limitation.
    // Using Rc::get_mut is not available because of possible aliasing.
    // TODO(port): LuaTable resize requires &mut access through Rc — needs RefCell<LuaTable>
    //   or a redesign in Phase B.

    // macros.tsv: setthvalue → *o = LuaValue::Thread(x.clone())
    // TODO(port): cannot create GcRef<LuaState> to self (self-referential Rc).
    // In Phase E this would be resolved once coroutine threads are GcRef-tracked.
    // For Phase A: leave registry[LUA_RIDX_MAINTHREAD-1] as Nil and add a TODO.
    // TODO(port): set registry[LUA_RIDX_MAINTHREAD - 1] = LuaValue::Thread(main_thread_gcref)

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

fn lua_open(state: &mut LuaState) -> Result<(), LuaError> {
    stack_init(state);
    init_registry(state)?;
    crate::string::init(state)?;
    crate::tagmethods::init(state)?;
    // TODO(port): luaX_init lives in the lua-lex crate; cross-crate call needed in Phase B
    state.global_mut().gcstp = 0;
    state.global().heap.unpause();
    // macros.tsv: setnilvalue → *o = LuaValue::Nil
    // PORT NOTE: setting nilvalue = Nil signals completestate() → is_complete() = true
    state.global_mut().nilvalue = LuaValue::Nil;
    // macros.tsv: luai_userstateopen → (extension hook, no-op default; drop)
    Ok(())
}

fn preinit_thread(thread: &mut LuaState, global: Rc<RefCell<GlobalState>>) {
    thread.global = global;
    thread.stack = Vec::new();
    thread.call_info = Vec::new();
    // PORT NOTE: We initialize ci to 0 but call_info is empty; stack_init() must be
    // called before any use of call_info.
    thread.ci = CallInfoIdx(0);
    thread.nci = 0;
    // PORT NOTE: In C, L->twups = L is a self-reference sentinel meaning "no open upvals".
    // In Rust, GlobalState.twups is a Vec<GcRef<LuaState>>; absence from that Vec is the
    // sentinel.  The per-thread `twups` field is removed (types.tsv: lua_State.twups → removed).
    thread.n_ccalls = 0;
    thread.hook = None;
    thread.hookmask = 0;
    thread.basehookcount = 0;
    thread.allowhook = true;
    // macros.tsv: resethookcount → state.reset_hook_count()
    thread.hookcount = thread.basehookcount;

    // Sandbox inheritance: a coroutine joins the runtime-wide instruction/memory
    // budget so metering spans every thread, not just the main one. The budget
    // itself lives in `GlobalState` (shared); the new thread only needs the
    // count-hook mask armed so the dispatch loop traps and charges it.
    {
        let (active, interval) = {
            let g = thread.global.borrow();
            (g.sandbox_active(), g.sandbox.interval.get())
        };
        if active {
            thread.hookmask = SANDBOX_COUNT_MASK;
            thread.basehookcount = interval;
            thread.hookcount = interval;
        }
    }
    thread.openupval = Vec::new();
    thread.status = LuaStatus::Ok as u8;
    thread.errfunc = 0;
    thread.oldpc = 0;
    thread.gc_check_needed = true;
}

fn close_state(state: &mut LuaState) {
    let is_complete = state.global().is_complete();

    if !is_complete {
        // macros.tsv: luaC_freeallobjects via GcHandle
        state.gc().free_all_objects();
    } else {
        state.ci = CallInfoIdx(0);
        // TODO(port): crate::do_::close_protected(state, StackIdx(1), LuaStatus::Ok)
        // Ignoring result here because we are in teardown (same as C behavior).
        state.gc().free_all_objects();
        // macros.tsv: luai_userstateclose → (extension hook; drop)
    }

    // macros.tsv: luaM_freearray → (Rust's Drop handles deallocation; drop the call)
    state.global_mut().strt = StringPool::default();

    free_stack(state);

    // PORT NOTE: C-specific memory accounting assertion; not applicable in Rust.

    // PORT NOTE: Custom allocator freed LG here. Rust's allocator (via Drop) handles
    // deallocation of GlobalState and LuaState automatically.
}

/// Create a new coroutine thread sharing the same GlobalState as the caller.
///
/// Pushes the new thread onto the caller's stack and returns `Ok(())`.
///
///
/// ```c
///
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
/// Allocate a fresh coroutine `LuaState`, register it under a new
/// `ThreadId`, and push the resulting `LuaValue::Thread(value)` onto
/// `state`'s stack.
///
/// If `initial_body` is `Some(f)`, `f` is also pushed onto the new
/// thread's stack so that `coroutine.status` reports `"suspended"`
/// rather than `"dead"`. The full cross-thread `xmove` from caller to
/// coroutine arrives in slice 02b; `co_create` uses `initial_body` to
/// stage the body without needing a real `xmove`.
pub fn new_thread(state: &mut LuaState, initial_body: Option<LuaValue>) -> Result<(), LuaError> {
    state.gc().check_step();

    // PORT NOTE: In C, the new thread is GC-allocated as part of the allgc list.
    // In Rust (Phase A), we create a plain LuaState; Phase D will wire GC registration.
    // TODO(port): allocate via state.gc().new_obj(LuaType::Thread, ...) in Phase D

    let global_rc = state.global_rc();
    let hookmask = state.hookmask;
    let basehookcount = state.basehookcount;

    let reserved_id = {
        let mut g = state.global_mut();
        let id = g.next_thread_id;
        g.next_thread_id += 1;
        id
    };

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
        n_ccalls: 0,
        oldpc: 0,
        marked: 0,
        cached_thread_id: reserved_id,
        gc_check_needed: false,
    };

    preinit_thread(&mut new_thread, global_rc);

    new_thread.hookmask = hookmask;
    new_thread.basehookcount = basehookcount;
    // TODO(port): lua_Hook is Box<dyn FnMut(...)>; not Clone.
    // Sharing a hook between threads would require Arc<Mutex<...>> (Phase E debug).
    new_thread.reset_hook_count();

    // macros.tsv: lua_getextraspace → state.extra_space_mut() → &mut [u8]
    // TODO(port): LuaState.extra_space field not yet defined; Phase B

    // macros.tsv: luai_userstatethread → (extension hook; drop)

    stack_init(&mut new_thread);

    if let Some(body) = initial_body {
        new_thread.push(body);
    }

    let thread_ref: Rc<RefCell<LuaState>> = Rc::new(RefCell::new(new_thread));

    let value = {
        let mut g = state.global_mut();
        let id = reserved_id;
        let value = GcRef::new(lua_types::value::LuaThread::new(id));
        g.threads.insert(
            id,
            ThreadRegistryEntry { state: thread_ref, value: value.clone() },
        );
        value
    };

    state.push(LuaValue::Thread(value));

    Ok(())
}

/// Reset a thread to its base state, closing all to-be-closed variables.
///
/// Returns the final status code as an `i32` (mirrors the C API).
///
///
/// ```c
///
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
pub fn reset_thread(state: &mut LuaState, status: i32) -> i32 {
    state.ci = CallInfoIdx(0);
    let ci_idx = 0usize;

    // macros.tsv: setnilvalue → *o = LuaValue::Nil; s2v → state.stack_at(idx)
    if !state.stack.is_empty() {
        state.stack[0].val = LuaValue::Nil;
    }

    state.call_info[ci_idx].func = StackIdx(0);
    state.call_info[ci_idx].callstatus = CIST_C;

    let mut status = if status == LuaStatus::Yield as i32 {
        LuaStatus::Ok as i32
    } else {
        status
    };

    state.status = LuaStatus::Ok as u8;

    let close_status = crate::do_::close_protected(
        state,
        StackIdx(1),
        LuaStatus::from_raw(status),
    );
    status = close_status as i32;

    if status != LuaStatus::Ok as i32 {
        crate::do_::set_error_obj(state, LuaStatus::from_raw(status), StackIdx(1));
    } else {
        state.top = StackIdx(1);
    }

    let new_ci_top = StackIdx(state.top.0 + LUA_MINSTACK as u32);
    state.call_info[ci_idx].top = new_ci_top;

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
///
/// ```c
///
/// //   int status;
/// //   lua_lock(L);
/// //   L->n_ccalls = (from) ? getCcalls(from) : 0;
/// //   status = luaE_resetthread(L, L->status);
/// //   lua_unlock(L);
/// //   return status;
/// // }
/// ```
pub fn close_thread(state: &mut LuaState, from: Option<&LuaState>) -> i32 {
    // macros.tsv: getCcalls → state.c_calls()
    state.n_ccalls = match from {
        Some(f) => f.c_calls(),
        None => 0,
    };
    let current_status = state.status as i32;
    let result = reset_thread(state, current_status);
    result
}

/// Deprecated wrapper for `close_thread(L, NULL)`.
///
///
/// ```c
///
/// //   return lua_closethread(L, NULL);
/// // }
/// ```
pub fn reset_thread_api(state: &mut LuaState) -> i32 {
    close_thread(state, None)
}

/// Create a new independent Lua state.  Returns `None` only on OOM.
///
///
/// PORT NOTE: The C API takes a custom allocator `(f, ud)`.  The Rust-native API
/// uses the global Rust allocator; those parameters are dropped.  Equivalent to
/// `LuaState::new()` at the call site.
///
/// ```c
///
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
    // In Rust, allocation failure panics by default; we use Result internally.

    // Build a dummy LuaString for memerrmsg and strcache initialization.
    // This is a chicken-and-egg problem: GlobalState.memerrmsg needs to be initialized
    // before luaS_init, but luaS_init creates the memerrmsg.
    // We use a placeholder Rc<LuaString> that will be replaced by luaS_init.
    // TODO(port): this is fragile; Phase B should ensure memerrmsg is properly set by luaS_init.
    // TODO(D-1c-bridge): allocation outside state context (new_state() free fn — no LuaState yet)
    let placeholder_str = GcRef::new(LuaString::placeholder());

    // macros.tsv: bitmask → (1u32 << b); WHITE0BIT = 0 → 1u8
    let initial_white = 1u8 << WHITE0BIT;

    // macros.tsv: setivalue → *o = LuaValue::Int(x)
    // PORT NOTE: non-nil nilvalue signals "state not yet complete"; see is_complete().

    let global = GlobalState {
        parser_hook: None,
        file_loader_hook: None,
        file_open_hook: None,
        stdout_hook: None,
        stderr_hook: None,
        stdin_hook: None,
        env_hook: None,
        unix_time_hook: None,
        cpu_clock_hook: None,
        local_offset_hook: None,
        entropy_hook: None,
        temp_name_hook: None,
        popen_hook: None,
        file_remove_hook: None,
        file_rename_hook: None,
        os_execute_hook: None,
        dynlib_load_hook: None,
        dynlib_symbol_hook: None,
        dynlib_unload_hook: None,
        totalbytes: std::mem::size_of::<GlobalState>() as isize,
        sandbox: SandboxLimits::default(),
        gc_debt: 0,
        gc_estimate: 0,
        lastatomic: 0,
        strt: StringPool::default(),
        l_registry: LuaValue::Nil,
        external_roots: ExternalRootSet::default(),
        globals: LuaValue::Nil,
        loaded: LuaValue::Nil,
        nilvalue: LuaValue::Int(0),
        seed: make_seed(),
        currentwhite: initial_white,
        gcstate: GCS_PAUSE,
        // macros.tsv: KGC_INC → GcKind::Incremental
        gckind: GcKind::Incremental as u8,
        gcstopem: false,
        genminormul: LUAI_GENMINORMUL,
        // macros.tsv: setgcparam → p = v / 4
        genmajormul: (LUAI_GENMAJORMUL / 4) as u8,
        gcstp: GCSTPGC,
        gcemergency: false,
        gcpause: (LUAI_GCPAUSE / 4) as u8,
        gcstepmul: (LUAI_GCMUL / 4) as u8,
        gcstepsize: LUAI_GCSTEPSIZE,
        sweepgc_cursor: 0,
        weak_tables_registry: Vec::new(),
        gc_tracked_long_strings: Vec::new(),
        pending_finalizers: Vec::new(),
        to_be_finalized: Vec::new(),
        twups: Vec::new(),
        panic: None,
        mainthread: None,
        threads: std::collections::HashMap::new(),
        main_thread_value: GcRef::new(lua_types::value::LuaThread::new(0)),
        current_thread_id: 0,
        main_thread_id: 0,
        next_thread_id: 1,
        memerrmsg: placeholder_str.clone(),
        tmname: Vec::new(),
        mt: std::array::from_fn(|_| None),
        strcache: std::array::from_fn(|_| {
            std::array::from_fn(|_| placeholder_str.clone())
        }),
        interned_lt: std::collections::HashMap::new(),
        warnf: None,
        c_functions: Vec::new(),
        heap: lua_gc::Heap::new(),
        cross_thread_upvals: std::collections::HashMap::new(),
        suspended_parent_stacks: Vec::new(),
        suspended_parent_open_upvals: Vec::new(),
    };

    let global_rc = Rc::new(RefCell::new(global));

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
        n_ccalls: 0,
        oldpc: 0,
        marked: initial_marked,
        cached_thread_id: 0,
        gc_check_needed: false,
    };

    preinit_thread(&mut main_thread, global_rc.clone());

    // macros.tsv: incnny → state.inc_nny() → L->n_ccalls += 0x10000
    main_thread.inc_nny();

    // TODO(port): self-referential Rc cycle; Phase D GC handles cycles.
    // For Phase A: skip setting mainthread to avoid the cycle.

    // TODO(port): Phase D — register main_thread in allgc as a GcRef

    //      close_state(L); L = NULL; }
    // error_sites.tsv: luaD_rawrunprotected → state.run_protected(|s| f(s, ud))
    // PORT NOTE: We call lua_open directly since we're not using the protected-call
    // machinery yet (ldo.c is not ported). Errors from lua_open propagate as Err.
    match lua_open(&mut main_thread) {
        Ok(()) => {}
        Err(_) => {
            close_state(&mut main_thread);
            return None;
        }
    }

    Some(main_thread)
}

/// Close the Lua state and free all resources.
///
///
/// PORT NOTE: In C, `lua_close` gets the main thread via `G(L)->mainthread`
/// and closes that regardless of which thread is passed.  In Rust, the caller
/// should hold the main `LuaState` and drop it (which triggers `close_state`
/// via this function or `Drop`).
///
/// ```c
///
/// //   lua_lock(L);
/// //   L = G(L)->mainthread;  /* only the main thread can be closed */
/// //   close_state(L);
/// // }
/// ```
pub fn close(mut state: LuaState) {
    // PORT NOTE: In Rust, callers must pass the main LuaState directly (or obtain it
    // from GlobalState.mainthread).  We do not traverse to the main thread here;
    // the caller owns the root state.
    // TODO(port): assert that `state` is indeed the main thread before closing
    close_state(&mut state);
}

/// Forward a warning message through the configured warning sink.
///
///
/// ```c
///
/// //   lua_WarnFunction wf = G(L)->warnf;
/// //   if (wf != NULL) wf(G(L)->ud_warn, msg, tocont);
/// // }
/// ```
pub(crate) fn warning(state: &mut LuaState, msg: &[u8], to_cont: bool) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_root_keys_reject_stale_slot_after_reuse() {
        let mut roots = ExternalRootSet::default();

        let first = roots.insert(LuaValue::Int(1));
        assert_eq!(roots.len(), 1);
        assert_eq!(roots.get(first), Some(&LuaValue::Int(1)));

        assert_eq!(roots.remove(first), Some(LuaValue::Int(1)));
        assert!(roots.get(first).is_none());
        assert!(roots.remove(first).is_none());
        assert_eq!(roots.len(), 0);
        assert_eq!(roots.vacant_len(), 1);
        assert!(roots.replace(first, LuaValue::Int(9)).is_none());
        assert!(roots.is_empty());

        let second = roots.insert(LuaValue::Int(2));
        assert_eq!(first.index, second.index);
        assert_ne!(first, second);
        assert!(roots.get(first).is_none());
        assert_eq!(roots.get(second), Some(&LuaValue::Int(2)));
        assert!(roots.replace(first, LuaValue::Int(3)).is_none());
    }

    #[test]
    fn external_roots_keep_heap_value_alive_until_unrooted() {
        let mut state = new_state().expect("state should initialize");
        let _heap_guard = {
            let g = state.global();
            lua_gc::HeapGuard::push(&g.heap)
        };

        let table = state.new_table();
        assert_eq!(state.global().heap.allgc_count(), 1);

        let key = state.external_root_value(LuaValue::Table(table));
        state.gc().full_collect();
        assert_eq!(state.global().heap.allgc_count(), 1);
        assert_eq!(state.global().external_roots.len(), 1);

        assert!(state.external_unroot_value(key).is_some());
        state.gc().full_collect();
        assert_eq!(state.global().heap.allgc_count(), 0);
        assert!(state.global().external_roots.is_empty());
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        src/lstate.c  (445 lines, 25 functions)
//                  src/lstate.h  (408 lines; struct definitions merged)
//   target_crate:  lua-vm
//   confidence:    medium
//   todos:         44
//   port_notes:    34
//   unsafe_blocks: 0   (must be 0 outside explicit unsafe-budget crates)
//   notes:         Logic faithfully follows lstate.c. Key structural changes:
//                  (1) LX/LG C layout wrappers dropped; GlobalState is Rc<RefCell<>>.
//                  (2) CallInfo linked list → Vec<CallInfo> with CallInfoIdx indices;
//                      shrink_ci uses truncation rather than node-by-node removal.
//                  (3) lua_State.twups self-reference → membership in GlobalState.twups Vec.
//                  (4) errorJmp/setjmp → removed; errors use Result<T, LuaError>.
//                  (5) Custom allocator (lua_Alloc) → dropped; Rust's allocator handles it.
//                  (6) make_seed: ASLR pointer entropy requires unsafe; time-only for Phase A.
//                  (7) Perf: LuaState.cached_thread_id stores the thread's own id once at
//                      construction; upvalue_get/_set compare against this u64 field
//                      instead of borrowing global.current_thread_id on every read.
//                      Invariant survives coroutine resume because each thread caches its
//                      OWN id, not the global's id (see field doc on cached_thread_id).
//                  (8) Perf: LuaTableRefExt::{raw_set, raw_set_int, get, get_int,
//                      get_short_str, metatable, as_ptr} and table_{raw,set_with_tm,
//                      array_set} carry #[inline] so the per-set dispatch chain
//                      collapses into set_i_value / vm.rs OP_SETI callers. The
//                      historical reject_invalid_table_key precheck moved into
//                      LuaTable::try_raw_set (lua-types) and was dropped at this
//                      layer; raw_set now takes the key by value, eliminating a
//                      24-byte LuaValue clone per set. gc_barrier_back is invoked
//                      before the store in table_set_with_tm (semantically
//                      equivalent: the barrier only inspects the value's color,
//                      not its location), letting v be moved directly into
//                      table_raw_set without an intermediate clone.
//                  Key TODOs: luaT_init and luaX_init cross-crate calls (Phase B);
//                  init_registry table mutations through Rc (needs RefCell<LuaTable>);
//                  luaD_closeprotected/seterrorobj/reallocstack in reset_thread (ldo.c);
//                  GcRef<LuaState> self-reference for mainthread (Phase D);
//                  LuaString::placeholder() helper needed for GlobalState init;
//                  LuaValue and LuaTable should move to object.rs once that lands.
// ──────────────────────────────────────────────────────────────────────────────
