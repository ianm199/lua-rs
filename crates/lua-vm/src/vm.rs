//! Lua virtual machine — port of `src/lvm.c` (1899 lines, 32 functions).
//!
//! This module implements:
//! - Number coercion helpers (tonumber_, flttointeger, tointegerns, tointeger)
//! - Numeric `for`-loop preparation and stepping (forlimit, forprep, floatforloop)
//! - Table get/set with metamethod chaining (finishget, finishset)
//! - String comparison respecting embedded NULs (l_strcmp)
//! - Relational operators: lessthan, lessequal, equalobj (with metamethods)
//! - String concatenation (concat)
//! - Object length operator (objlen)
//! - Integer arithmetic: idiv, mod, modf, shiftl
//! - Closure creation (pushclosure)
//! - Yield-resume bridge (finishOp)
//! - Main interpreter loop (execute) — the Lua bytecode dispatch engine.
//!
//! # Control flow note
//! The C source uses `goto startfunc` / `goto returning` / `goto ret` across
//! labelled points in `luaV_execute`. These are modelled with Rust's labelled
//! loops (`'startfunc`, `'returning`, `'dispatch`) and `continue`/`break`
//! on those labels.  See inline `PORT NOTE` comments.


#[allow(unused_imports)] use crate::prelude::*;
use lua_types::{
    CallInfoIdx, GcRef, LuaError, LuaValue, StackIdx,
};
use lua_types::tagmethod::TagMethod;
use lua_types::opcode::Instruction;
use crate::state::LuaState;

/// TODO(phase-b): lua-types does not yet expose `OpCode`. Stubbed locally with
/// all 5.4 opcodes so call sites in vm.rs/debug.rs resolve; the real numeric
/// values and per-opcode mode flags live in `lua-types/src/opcode.rs` once
/// translated.
///
/// `#[repr(u8)]` with explicit discriminants matching C-Lua's `lopcodes.h`
/// numbering (0=OP_MOVE, 1=OP_LOADI, ..., 82=OP_EXTRAARG). The ordered, dense
/// 0..=82 layout lets LLVM compile `opcode()` to a bounds-checked cast on the
/// low 7 bits of the instruction word and fuse it with the dispatch `match`
/// downstream. Discriminant order intentionally matches the integer keys in
/// `InstructionExt::opcode`, not the prior compile-order grouping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(non_camel_case_types)]
#[repr(u8)]
pub enum OpCode {
    Move = 0,
    LoadI = 1,
    LoadF = 2,
    LoadK = 3,
    LoadKX = 4,
    LoadFalse = 5,
    LFalseSkip = 6,
    LoadTrue = 7,
    LoadNil = 8,
    GetUpVal = 9,
    SetUpVal = 10,
    GetTabUp = 11,
    GetTable = 12,
    GetI = 13,
    GetField = 14,
    SetTabUp = 15,
    SetTable = 16,
    SetI = 17,
    SetField = 18,
    NewTable = 19,
    Self_ = 20,
    AddI = 21,
    AddK = 22,
    SubK = 23,
    MulK = 24,
    ModK = 25,
    PowK = 26,
    DivK = 27,
    IDivK = 28,
    BAndK = 29,
    BOrK = 30,
    BXOrK = 31,
    ShrI = 32,
    ShlI = 33,
    Add = 34,
    Sub = 35,
    Mul = 36,
    Mod = 37,
    Pow = 38,
    Div = 39,
    IDiv = 40,
    BAnd = 41,
    BOr = 42,
    BXOr = 43,
    Shl = 44,
    Shr = 45,
    MmBin = 46,
    MmBinI = 47,
    MmBinK = 48,
    Unm = 49,
    BNot = 50,
    Not = 51,
    Len = 52,
    Concat = 53,
    Close = 54,
    Tbc = 55,
    Jmp = 56,
    Eq = 57,
    Lt = 58,
    Le = 59,
    EqK = 60,
    EqI = 61,
    LtI = 62,
    LeI = 63,
    GtI = 64,
    GeI = 65,
    Test = 66,
    TestSet = 67,
    Call = 68,
    TailCall = 69,
    Return = 70,
    Return0 = 71,
    Return1 = 72,
    ForLoop = 73,
    ForPrep = 74,
    TForPrep = 75,
    TForCall = 76,
    TForLoop = 77,
    SetList = 78,
    Closure = 79,
    VarArg = 80,
    VarArgPrep = 81,
    ExtraArg = 82,
}

/// Number of distinct opcodes (matches C-Lua's `NUM_OPCODES`). Held for
/// downstream debug/dump callers that count opcodes by name; the dispatch
/// hot path in `InstructionExt::opcode` does its own per-arm match.
#[allow(dead_code)]
const NUM_OPCODES: u8 = 83;

impl OpCode {
    /// Legacy alias retained because the prior duplicate enum variant
    /// `LoadKx` (case-typo of `LoadKX`) is still referenced from
    /// `crates/lua-vm/src/debug.rs`. Both names denote the same C
    /// `OP_LOADKX` opcode. Kept as an associated `const` so existing call
    /// sites compile unchanged while the enum remains a clean 0..=82 dense
    /// discriminant set required by `#[repr(u8)]`.
    #[allow(non_upper_case_globals)]
    pub const LoadKx: OpCode = OpCode::LoadKX;

    /// Legacy alias for `GetUpVal` retained for the same reason as `LoadKx`.
    #[allow(non_upper_case_globals)]
    pub const GetUpval: OpCode = OpCode::GetUpVal;
}

/// TODO(phase-b): Instruction accessor extension trait. The real per-mode
/// decode helpers live in `lua-types::opcode` once translated. Stubbed locally
/// so call sites resolve; bodies are inferred from `lopcodes.h` macro shapes.
pub trait InstructionExt {
    fn opcode(&self) -> OpCode;
    fn arg_a(&self) -> i32;
    fn arg_b(&self) -> i32;
    fn arg_c(&self) -> i32;
    fn arg_k(&self) -> i32;
    fn arg_ax(&self) -> i32;
    fn arg_bx(&self) -> i32;
    fn arg_s_b(&self) -> i32;
    fn arg_s_c(&self) -> i32;
    fn arg_s_j(&self) -> i32;
    fn arg_s_bx(&self) -> i32;
    fn test_k(&self) -> bool;
    fn test_a_mode(&self) -> bool;
    fn is_mm_mode(&self) -> bool;
    fn is_vararg_prep(&self) -> bool;
    fn is_in_top(&self) -> bool;
}

impl InstructionExt for Instruction {
    ///
    /// The 83-arm match looks expensive, but because `OpCode` is
    /// `#[repr(u8)]` with explicit discriminants 0..=82 matching each match
    /// arm's integer key exactly, LLVM compiles this to a single bounds
    /// check + identity cast — no jump table, no memory indirection. The
    /// previous array-lookup form forced an extra `OPCODE_TABLE` byte load
    /// per dispatch tick that LLVM could not see through.
    #[inline(always)]
    fn opcode(&self) -> OpCode {
        match (self.raw() & 0x7F) as u8 {
            0 => OpCode::Move,
            1 => OpCode::LoadI,
            2 => OpCode::LoadF,
            3 => OpCode::LoadK,
            4 => OpCode::LoadKX,
            5 => OpCode::LoadFalse,
            6 => OpCode::LFalseSkip,
            7 => OpCode::LoadTrue,
            8 => OpCode::LoadNil,
            9 => OpCode::GetUpVal,
            10 => OpCode::SetUpVal,
            11 => OpCode::GetTabUp,
            12 => OpCode::GetTable,
            13 => OpCode::GetI,
            14 => OpCode::GetField,
            15 => OpCode::SetTabUp,
            16 => OpCode::SetTable,
            17 => OpCode::SetI,
            18 => OpCode::SetField,
            19 => OpCode::NewTable,
            20 => OpCode::Self_,
            21 => OpCode::AddI,
            22 => OpCode::AddK,
            23 => OpCode::SubK,
            24 => OpCode::MulK,
            25 => OpCode::ModK,
            26 => OpCode::PowK,
            27 => OpCode::DivK,
            28 => OpCode::IDivK,
            29 => OpCode::BAndK,
            30 => OpCode::BOrK,
            31 => OpCode::BXOrK,
            32 => OpCode::ShrI,
            33 => OpCode::ShlI,
            34 => OpCode::Add,
            35 => OpCode::Sub,
            36 => OpCode::Mul,
            37 => OpCode::Mod,
            38 => OpCode::Pow,
            39 => OpCode::Div,
            40 => OpCode::IDiv,
            41 => OpCode::BAnd,
            42 => OpCode::BOr,
            43 => OpCode::BXOr,
            44 => OpCode::Shl,
            45 => OpCode::Shr,
            46 => OpCode::MmBin,
            47 => OpCode::MmBinI,
            48 => OpCode::MmBinK,
            49 => OpCode::Unm,
            50 => OpCode::BNot,
            51 => OpCode::Not,
            52 => OpCode::Len,
            53 => OpCode::Concat,
            54 => OpCode::Close,
            55 => OpCode::Tbc,
            56 => OpCode::Jmp,
            57 => OpCode::Eq,
            58 => OpCode::Lt,
            59 => OpCode::Le,
            60 => OpCode::EqK,
            61 => OpCode::EqI,
            62 => OpCode::LtI,
            63 => OpCode::LeI,
            64 => OpCode::GtI,
            65 => OpCode::GeI,
            66 => OpCode::Test,
            67 => OpCode::TestSet,
            68 => OpCode::Call,
            69 => OpCode::TailCall,
            70 => OpCode::Return,
            71 => OpCode::Return0,
            72 => OpCode::Return1,
            73 => OpCode::ForLoop,
            74 => OpCode::ForPrep,
            75 => OpCode::TForPrep,
            76 => OpCode::TForCall,
            77 => OpCode::TForLoop,
            78 => OpCode::SetList,
            79 => OpCode::Closure,
            80 => OpCode::VarArg,
            81 => OpCode::VarArgPrep,
            82 => OpCode::ExtraArg,
            _ => OpCode::ExtraArg,
        }
    }
    #[inline] fn arg_a(&self) -> i32 { ((self.raw() >> 7) & 0xFF) as i32 }
    #[inline] fn arg_b(&self) -> i32 { ((self.raw() >> 16) & 0xFF) as i32 }
    #[inline] fn arg_c(&self) -> i32 { ((self.raw() >> 24) & 0xFF) as i32 }
    #[inline] fn arg_k(&self) -> i32 { ((self.raw() >> 15) & 0x1) as i32 }
    #[inline] fn arg_ax(&self) -> i32 { (self.raw() >> 7) as i32 }
    #[inline] fn arg_bx(&self) -> i32 { (self.raw() >> 15) as i32 }
    #[inline] fn arg_s_b(&self) -> i32 { self.arg_b() - 0x7F }
    #[inline] fn arg_s_c(&self) -> i32 { self.arg_c() - 0x7F }
    #[inline] fn arg_s_j(&self) -> i32 { self.arg_ax() - 0xFFFFFF }
    #[inline] fn arg_s_bx(&self) -> i32 { self.arg_bx() - 0xFFFF }
    #[inline] fn test_k(&self) -> bool { (self.raw() & (1 << 15)) != 0 }
    #[inline]
    fn test_a_mode(&self) -> bool {
        (op_mode_byte(self.opcode()) & (1 << 3)) != 0
    }
    #[inline]
    fn is_mm_mode(&self) -> bool {
        (op_mode_byte(self.opcode()) & (1 << 7)) != 0
    }
    #[inline]
    fn is_vararg_prep(&self) -> bool {
        matches!(self.opcode(), OpCode::VarArgPrep)
    }
    #[inline]
    fn is_in_top(&self) -> bool {
        (op_mode_byte(self.opcode()) & (1 << 5)) != 0 && self.arg_b() == 0
    }
}

///
/// Layout (from lopcodes.h `opmode` macro):
///   bit 7: MM (metamethod call)
///   bit 6: OT (instruction sets `L->top` for next when C == 0)
///   bit 5: IT (instruction reads `L->top` from prev when B == 0)
///   bit 4: T  (test; next instruction must be a jump)
///   bit 3: A  (instruction writes register A)
///   bits 0-2: op format mode (iABC, iABx, iAsBx, iAx, isJ)
///
/// PORT NOTE: lua-types does not yet expose the canonical `OP_MODES` table; this
/// is a local stand-in keyed off the vm.rs `OpCode` stub so the four mode
/// predicates above can answer correctly until the real table lands.
const OP_MODE_BYTES: [u8; NUM_OPCODES as usize] = [
    0x08, // Move
    0x0a, // LoadI
    0x0a, // LoadF
    0x09, // LoadK
    0x09, // LoadKX
    0x08, // LoadFalse
    0x08, // LFalseSkip
    0x08, // LoadTrue
    0x08, // LoadNil
    0x08, // GetUpVal
    0x00, // SetUpVal
    0x08, // GetTabUp
    0x08, // GetTable
    0x08, // GetI
    0x08, // GetField
    0x00, // SetTabUp
    0x00, // SetTable
    0x00, // SetI
    0x00, // SetField
    0x08, // NewTable
    0x08, // Self_
    0x08, // AddI
    0x08, // AddK
    0x08, // SubK
    0x08, // MulK
    0x08, // ModK
    0x08, // PowK
    0x08, // DivK
    0x08, // IDivK
    0x08, // BAndK
    0x08, // BOrK
    0x08, // BXOrK
    0x08, // ShrI
    0x08, // ShlI
    0x08, // Add
    0x08, // Sub
    0x08, // Mul
    0x08, // Mod
    0x08, // Pow
    0x08, // Div
    0x08, // IDiv
    0x08, // BAnd
    0x08, // BOr
    0x08, // BXOr
    0x08, // Shl
    0x08, // Shr
    0x80, // MmBin
    0x80, // MmBinI
    0x80, // MmBinK
    0x08, // Unm
    0x08, // BNot
    0x08, // Not
    0x08, // Len
    0x08, // Concat
    0x00, // Close
    0x00, // Tbc
    0x04, // Jmp
    0x10, // Eq
    0x10, // Lt
    0x10, // Le
    0x10, // EqK
    0x10, // EqI
    0x10, // LtI
    0x10, // LeI
    0x10, // GtI
    0x10, // GeI
    0x10, // Test
    0x18, // TestSet
    0x68, // Call
    0x68, // TailCall
    0x20, // Return
    0x00, // Return0
    0x00, // Return1
    0x09, // ForLoop
    0x09, // ForPrep
    0x01, // TForPrep
    0x00, // TForCall
    0x09, // TForLoop
    0x20, // SetList
    0x09, // Closure
    0x48, // VarArg
    0x28, // VarArgPrep
    0x03, // ExtraArg
];

#[inline(always)]
fn op_mode_byte(op: OpCode) -> u8 {
    OP_MODE_BYTES[op as usize]
}

// ─── Constants ───────────────────────────────────────────────────────────────

/// Limit for tag-method chains to avoid infinite loops.
const MAX_TAG_LOOP: i32 = 2000;

const NBITS: u32 = 64;

// ─── F2Imod — float-to-integer rounding mode ────────────────────────────────

/// Rounding mode for float→integer coercions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum F2Imod {
    /// Accept only exact integral values (no rounding).
    Eq,
    /// Round toward negative infinity.
    Floor,
    /// Round toward positive infinity.
    Ceil,
}

// ─── Integer-overflow-safe helpers ──────────────────────────────────────────

#[inline]
fn intop_add(a: i64, b: i64) -> i64 {
    (a as u64).wrapping_add(b as u64) as i64
}

#[inline]
fn intop_sub(a: i64, b: i64) -> i64 {
    (a as u64).wrapping_sub(b as u64) as i64
}

#[inline]
fn intop_mul(a: i64, b: i64) -> i64 {
    (a as u64).wrapping_mul(b as u64) as i64
}

/// Shifts via unsigned intermediate to get logical (not arithmetic) semantics.
#[inline]
fn intop_shr(x: i64, n: u32) -> i64 {
    // PERF(port): logical right shift via unsigned; matches C unsigned semantics
    (x as u64 >> n) as i64
}

#[inline]
fn intop_shl(x: i64, n: u32) -> i64 {
    (x as u64).wrapping_shl(n) as i64
}

#[inline]
fn intop_band(a: i64, b: i64) -> i64 { ((a as u64) & (b as u64)) as i64 }
#[inline]
fn intop_bor(a: i64, b: i64) -> i64  { ((a as u64) | (b as u64)) as i64 }
#[inline]
fn intop_bxor(a: i64, b: i64) -> i64 { ((a as u64) ^ (b as u64)) as i64 }

// ─── l_intfitsf ─────────────────────────────────────────────────────────────

/// f64 has 53 bits of mantissa (including implicit leading 1).
/// All i64 values with |i| <= 2^53 are exactly representable.
#[inline]
fn int_fits_float(i: i64) -> bool {
    const MAXINTFITSF: u64 = 1u64 << f64::MANTISSA_DIGITS;
    (MAXINTFITSF.wrapping_add(i as u64)) <= 2 * MAXINTFITSF
}

// ─── Private helper: string-to-number coercion ──────────────────────────────

/// Attempt to convert a string value to a number in-place.
/// Returns `Some(LuaValue)` with the numeric result, or `None` if the
/// value is not a string or cannot be parsed as a numeral.
fn str_to_number(obj: &LuaValue) -> Option<LuaValue> {
    // cvt2num(o) = matches!(o, LuaValue::Str(_))
    let s = match obj {
        LuaValue::Str(ts) => ts.as_bytes().to_vec(),
        _ => return None,
    };
    // Trim whitespace as Lua allows spaces around numerals in coercions.
    let trimmed = trim_whitespace(&s);
    if trimmed.is_empty() {
        return None;
    }
    let mut result = LuaValue::Nil;
    if crate::object::str2num(trimmed, &mut result) != 0 {
        return Some(result);
    }
    None
}

fn trim_whitespace(s: &[u8]) -> &[u8] {
    let start = s.iter().position(|&b| !b.is_ascii_whitespace()).unwrap_or(s.len());
    let end = s.iter().rposition(|&b| !b.is_ascii_whitespace()).map(|i| i + 1).unwrap_or(0);
    if start <= end { &s[start..end] } else { &s[0..0] }
}

// ─── Number coercion (public API matching lvm.h exports) ────────────────────

/// Convert `obj` to f64, with string coercion.  Returns `Some(f64)` on
/// success.  The fast path (already float) is handled by the caller's
/// `tonumber` macro (inlined at call sites).
pub(crate) fn tonumber_(obj: &LuaValue) -> Option<f64> {
    if let LuaValue::Int(i) = obj {
        return Some(*i as f64);
    }
    if let Some(v) = str_to_number(obj) {
        return match v {
            LuaValue::Float(f) => Some(f),
            LuaValue::Int(i) => Some(i as f64),
            _ => None,
        };
    }
    None
}

/// Full numeric coercion including the float fast-path that `tonumber_` omits.
fn tonumber(obj: &LuaValue) -> Option<f64> {
    if let LuaValue::Float(f) = obj {
        return Some(*f);
    }
    tonumber_(obj)
}

/// Convert float `n` to an integer according to `mode`.
/// Returns `Some(i64)` on success.
pub(crate) fn flt_to_integer(n: f64, mode: F2Imod) -> Option<i64> {
    let f = n.floor();
    if n != f {
        match mode {
            F2Imod::Eq => return None,
            F2Imod::Ceil => {
                // f = floor(n) + 1 = ceil(n) since n is not integral
                let f = f + 1.0;
                // lua_numbertointeger checks i64::MIN <= f <= i64::MAX
                if f >= i64::MIN as f64 && f < (i64::MAX as f64 + 1.0) {
                    return Some(f as i64);
                }
                return None;
            }
            F2Imod::Floor => { /* f is already floor(n) */ }
        }
    }
    if f >= i64::MIN as f64 && f < (i64::MAX as f64 + 1.0) {
        Some(f as i64)
    } else {
        None
    }
}

/// Convert a value to integer without string coercion.
pub(crate) fn to_integer_ns(obj: &LuaValue, mode: F2Imod) -> Option<i64> {
    if let LuaValue::Float(f) = obj {
        return flt_to_integer(*f, mode);
    }
    if let LuaValue::Int(i) = obj {
        return Some(*i);
    }
    None
}

/// Convert a value to integer, with string coercion.
pub(crate) fn to_integer(obj: &LuaValue, mode: F2Imod) -> Option<i64> {
    let coerced;
    let obj = if let Some(v) = str_to_number(obj) {
        coerced = v;
        &coerced
    } else {
        obj
    };
    to_integer_ns(obj, mode)
}

// ─── for-loop helpers ────────────────────────────────────────────────────────

///                          lua_Integer *p, lua_Integer step)`
/// Compute the integer loop limit.  Returns `Ok(true)` to skip the loop,
/// `Ok(false)` with `*p` set to the limit, or `Err` if the limit is not a
/// number at all.
fn forlimit(
    state: &mut LuaState,
    init: i64,
    lim: &LuaValue,
    step: i64,
) -> Result<(bool, i64), LuaError> {
    let round = if step < 0 { F2Imod::Ceil } else { F2Imod::Floor };
    if let Some(p) = to_integer(lim, round) {
        let skip = if step > 0 { init > p } else { init < p };
        return Ok((skip, p));
    }
    let flim = match tonumber(lim) {
        Some(f) => f,
        None => return Err(crate::debug::for_error(state, lim, b"limit")),
    };
    if 0.0_f64 < flim {
        // positive → too large
        if step < 0 {
            return Ok((true, 0));
        }
        Ok((false, i64::MAX))
    } else {
        // negative → less than min integer
        if step > 0 {
            return Ok((true, 0));
        }
        Ok((false, i64::MIN))
    }
}

/// Prepare a numeric `for` loop (OP_FORPREP).
/// Stack layout at `ra`:
///   ra+0: init, ra+1: limit, ra+2: step, ra+3: control variable (written here)
/// Returns `Ok(true)` to skip the loop body entirely.
pub(crate) fn forprep(state: &mut LuaState, ra: StackIdx) -> Result<bool, LuaError> {
    let pinit  = state.get_at(ra);
    let plimit = state.get_at(ra + 1);
    let pstep  = state.get_at(ra + 2);

    if let (LuaValue::Int(init), LuaValue::Int(step)) = (&pinit, &pstep) {
        let init = *init;
        let step = *step;
        if step == 0 {
            return Err(LuaError::runtime(format_args!("'for' step is zero")));
        }
        state.set_at(ra + 3, LuaValue::Int(init));

        let (skip, limit) = forlimit(state, init, &plimit, step)?;
        if skip {
            return Ok(true);
        }
        let count: u64 = if step > 0 {
            let c = (limit as u64).wrapping_sub(init as u64);
            if step != 1 { c / (step as u64) } else { c }
        } else {
            let c = (init as u64).wrapping_sub(limit as u64);
            c / (((-(step + 1)) as u64).wrapping_add(1))
        };
        state.set_at(ra + 1, LuaValue::Int(count as i64));
        Ok(false)
    } else {
        let limit_f = match tonumber(&plimit) {
            Some(f) => f,
            None => return Err(crate::debug::for_error(state, &plimit, b"limit")),
        };
        let step_f = match tonumber(&pstep) {
            Some(f) => f,
            None => return Err(crate::debug::for_error(state, &pstep, b"step")),
        };
        let init_f = match tonumber(&pinit) {
            Some(f) => f,
            None => return Err(crate::debug::for_error(state, &pinit, b"initial value")),
        };
        if step_f == 0.0 {
            return Err(LuaError::runtime(format_args!("'for' step is zero")));
        }
        let skip = if step_f > 0.0 { limit_f < init_f } else { init_f < limit_f };
        if skip {
            return Ok(true);
        }
        //    setfltvalue(s2v(ra), init); setfltvalue(s2v(ra+3), init);
        state.set_at(ra + 1, LuaValue::Float(limit_f));
        state.set_at(ra + 2, LuaValue::Float(step_f));
        state.set_at(ra,     LuaValue::Float(init_f));
        state.set_at(ra + 3, LuaValue::Float(init_f));
        Ok(false)
    }
}

/// Increments the float loop index and returns `true` if the loop continues.
fn float_for_loop(state: &mut LuaState, ra: StackIdx) -> bool {
    //    idx  = fltvalue(s2v(ra));
    let step = match state.get_at(ra + 2) {
        LuaValue::Float(f) => f,
        _ => return false,
    };
    let limit = match state.get_at(ra + 1) {
        LuaValue::Float(f) => f,
        _ => return false,
    };
    let idx = match state.get_at(ra) {
        LuaValue::Float(f) => f,
        _ => return false,
    };
    let idx = idx + step;
    if if step > 0.0 { idx <= limit } else { limit <= idx } {
        state.set_at(ra,     LuaValue::Float(idx));
        state.set_at(ra + 3, LuaValue::Float(idx));
        true
    } else {
        false
    }
}

// ─── Table get/set with metamethod chains ────────────────────────────────────

///                          StkId val, const TValue *slot)`
/// Finish a table-get with metamethod lookup.  `slot_was_none = true` means
/// `t` is not a table and we should look for `__index` on `t` itself.
pub(crate) fn finish_get(
    state: &mut LuaState,
    t_val: LuaValue,
    key: LuaValue,
    result_idx: StackIdx,
    slot_empty: bool,
    t_idx: Option<StackIdx>,
) -> Result<(), LuaError> {
    let mut t = t_val;
    let mut t_idx = t_idx;
    for _loop in 0..MAX_TAG_LOOP {
        let tm: LuaValue;
        if slot_empty && !matches!(t, LuaValue::Table(_)) {
            tm = state.get_tm_by_obj(&t, TagMethod::Index);
            if matches!(tm, LuaValue::Nil) {
                return Err(match t_idx {
                    Some(idx) => crate::debug::type_error(state, &t, idx, b"index"),
                    None => LuaError::type_error(&t, "index"),
                });
            }
        } else {
            let mt = state.table_metatable(&t);
            tm = state.fast_tm_table(mt.as_ref(), TagMethod::Index);
            if matches!(tm, LuaValue::Nil) {
                state.set_at(result_idx, LuaValue::Nil);
                return Ok(());
            }
        }
        if matches!(tm, LuaValue::Function(_)) {
            state.call_tm_res(tm, &t, &key, result_idx)?;
            return Ok(());
        }
        t = tm.clone();
        t_idx = None;
        if let Some(v) = state.fast_get(&t, &key)? {
            state.set_at(result_idx, v);
            return Ok(());
        }
        // else: loop — tail-call luaV_finishget
    }
    Err(LuaError::runtime(format_args!("'__index' chain too long; possible loop")))
}

///                          TValue *val, const TValue *slot)`
/// Finish a table-set with `__newindex` metamethod lookup.
///
/// `var_hint` carries a `(kind, name)` pair (e.g. `(b"upvalue", b"a")`) used
/// only when `t_idx` is None and the target is non-indexable — typically
/// when the LHS is an upvalue (OP_SETTABUP). Pointer-identifying var_info
/// won't recover the upvalue's name in that case, so the caller passes it
/// in directly.
pub(crate) fn finish_set(
    state: &mut LuaState,
    t_val: LuaValue,
    key: LuaValue,
    val: LuaValue,
    _slot_present: bool,
    t_idx: Option<StackIdx>,
    var_hint: Option<(&[u8], &[u8])>,
) -> Result<(), LuaError> {
    let mut t = t_val;
    let mut t_idx = t_idx;
    for _loop in 0..MAX_TAG_LOOP {
        let tm: LuaValue;
        if matches!(t, LuaValue::Table(_)) {
            let mt = state.table_metatable(&t);
            tm = state.fast_tm_table(mt.as_ref(), TagMethod::NewIndex);
            if matches!(tm, LuaValue::Nil) {
                state.table_raw_set(&t, key, val.clone())?;
                state.gc_barrier_back(&t, &val);
                return Ok(());
            }
        } else {
            tm = state.get_tm_by_obj(&t, TagMethod::NewIndex);
            if matches!(tm, LuaValue::Nil) {
                return Err(match (t_idx, var_hint) {
                    (Some(idx), _) => crate::debug::type_error(state, &t, idx, b"index"),
                    (None, Some((kind, name))) => {
                        crate::debug::type_error_with_hint(state, &t, b"index", kind, name)
                    }
                    (None, None) => LuaError::type_error(&t, "index"),
                });
            }
        }
        if matches!(tm, LuaValue::Function(_)) {
            state.call_tm(tm, &t, &key, &val)?;
            return Ok(());
        }
        t = tm.clone();
        t_idx = None;
        if state.fast_get(&t, &key)?.is_some() {
            state.table_raw_set(&t, key.clone(), val.clone())?;
            state.gc_barrier_back(&t, &val);
            return Ok(());
        }
    }
    Err(LuaError::runtime(format_args!("'__newindex' chain too long; possible loop")))
}

// ─── String comparison ───────────────────────────────────────────────────────

/// Lexicographic string comparison that handles embedded NULs by segmenting.
/// Returns negative / zero / positive like `strcmp`.
///
/// PORT NOTE: C uses `strcoll` for locale-aware comparison within each NUL-free
/// segment.  Rust's standard library has no locale support, so we use
/// `slice::cmp` (byte-by-byte lexicographic order, equivalent to `memcmp`).
/// This means locale-specific ordering (e.g. accented characters) differs from
/// the C reference.  Mark as TODO for a later `libc::strcoll` bridge if needed.
fn str_cmp(s1: &[u8], s2: &[u8]) -> std::cmp::Ordering {
    // TODO(port): C uses strcoll per-segment; here we use byte-lexicographic
    // order.  This affects locale-sensitive string comparisons.
    let mut s1 = s1;
    let mut s2 = s2;
    loop {
        // Find the first NUL in each slice to delimit a segment.
        let z1 = s1.iter().position(|&b| b == 0).unwrap_or(s1.len());
        let z2 = s2.iter().position(|&b| b == 0).unwrap_or(s2.len());
        // Compare segment up to first NUL using byte order (not strcoll).
        let seg_cmp = s1[..z1].cmp(&s2[..z2]);
        if seg_cmp != std::cmp::Ordering::Equal {
            return seg_cmp;
        }
        // Both segments compare equal up to the NUL position.
        if z2 == s2.len() {
            // s2 is finished
            if z1 == s1.len() {
                return std::cmp::Ordering::Equal;
            }
            return std::cmp::Ordering::Greater; // s1 has more
        }
        if z1 == s1.len() {
            return std::cmp::Ordering::Less; // s1 finished, s2 has more
        }
        // Both have NULs; advance past them.
        s1 = &s1[z1 + 1..];
        s2 = &s2[z2 + 1..];
    }
}

// ─── Comparison helpers (int vs float mixed comparisons) ────────────────────

#[inline]
fn lt_int_float(i: i64, f: f64) -> bool {
    if int_fits_float(i) {
        (i as f64) < f
    } else {
        match flt_to_integer(f, F2Imod::Ceil) {
            Some(fi) => i < fi,
            None => f > 0.0, // f is out of integer range; positive means i < f
        }
    }
}

#[inline]
fn le_int_float(i: i64, f: f64) -> bool {
    if int_fits_float(i) {
        (i as f64) <= f
    } else {
        match flt_to_integer(f, F2Imod::Floor) {
            Some(fi) => i <= fi,
            None => f > 0.0,
        }
    }
}

#[inline]
fn lt_float_int(f: f64, i: i64) -> bool {
    if int_fits_float(i) {
        f < (i as f64)
    } else {
        match flt_to_integer(f, F2Imod::Floor) {
            Some(fi) => fi < i,
            None => f < 0.0,
        }
    }
}

#[inline]
fn le_float_int(f: f64, i: i64) -> bool {
    if int_fits_float(i) {
        f <= (i as f64)
    } else {
        match flt_to_integer(f, F2Imod::Ceil) {
            Some(fi) => fi <= i,
            None => f < 0.0,
        }
    }
}

#[inline]
fn lt_num(l: &LuaValue, r: &LuaValue) -> bool {
    debug_assert!(matches!(l, LuaValue::Int(_) | LuaValue::Float(_)));
    debug_assert!(matches!(r, LuaValue::Int(_) | LuaValue::Float(_)));
    match (l, r) {
        (LuaValue::Int(li), LuaValue::Int(ri))     => li < ri,
        (LuaValue::Int(li), LuaValue::Float(rf))   => lt_int_float(*li, *rf),
        (LuaValue::Float(lf), LuaValue::Float(rf)) => lf < rf,
        (LuaValue::Float(lf), LuaValue::Int(ri))   => lt_float_int(*lf, *ri),
        _ => false,
    }
}

#[inline]
fn le_num(l: &LuaValue, r: &LuaValue) -> bool {
    debug_assert!(matches!(l, LuaValue::Int(_) | LuaValue::Float(_)));
    debug_assert!(matches!(r, LuaValue::Int(_) | LuaValue::Float(_)));
    match (l, r) {
        (LuaValue::Int(li), LuaValue::Int(ri))     => li <= ri,
        (LuaValue::Int(li), LuaValue::Float(rf))   => le_int_float(*li, *rf),
        (LuaValue::Float(lf), LuaValue::Float(rf)) => lf <= rf,
        (LuaValue::Float(lf), LuaValue::Int(ri))   => le_float_int(*lf, *ri),
        _ => false,
    }
}

/// `l < r` for non-numbers (strings or metamethod).
fn less_than_others(state: &mut LuaState, l: &LuaValue, r: &LuaValue) -> Result<bool, LuaError> {
    debug_assert!(!(matches!(l, LuaValue::Int(_) | LuaValue::Float(_))
                  && matches!(r, LuaValue::Int(_) | LuaValue::Float(_))));
    match (l, r) {
        (LuaValue::Str(ts1), LuaValue::Str(ts2)) => {
            Ok(str_cmp(ts1.as_bytes(), ts2.as_bytes()) == std::cmp::Ordering::Less)
        }
        _ => state.call_order_tm(l, r, TagMethod::Lt),
    }
}

pub(crate) fn less_than(state: &mut LuaState, l: &LuaValue, r: &LuaValue) -> Result<bool, LuaError> {
    if matches!(l, LuaValue::Int(_) | LuaValue::Float(_))
        && matches!(r, LuaValue::Int(_) | LuaValue::Float(_))
    {
        Ok(lt_num(l, r))
    } else {
        less_than_others(state, l, r)
    }
}

fn less_equal_others(state: &mut LuaState, l: &LuaValue, r: &LuaValue) -> Result<bool, LuaError> {
    match (l, r) {
        (LuaValue::Str(ts1), LuaValue::Str(ts2)) => {
            Ok(str_cmp(ts1.as_bytes(), ts2.as_bytes()) != std::cmp::Ordering::Greater)
        }
        _ => state.call_order_tm(l, r, TagMethod::Le),
    }
}

pub(crate) fn less_equal(state: &mut LuaState, l: &LuaValue, r: &LuaValue) -> Result<bool, LuaError> {
    if matches!(l, LuaValue::Int(_) | LuaValue::Float(_))
        && matches!(r, LuaValue::Int(_) | LuaValue::Float(_))
    {
        Ok(le_num(l, r))
    } else {
        less_equal_others(state, l, r)
    }
}

// ─── Equality ────────────────────────────────────────────────────────────────

/// Main equality test.  `raw = true` means no metamethods (L == NULL in C).
pub(crate) fn equal_obj(
    state: Option<&mut LuaState>,
    t1: &LuaValue,
    t2: &LuaValue,
) -> Result<bool, LuaError> {
    // In Rust, same variant = same tag.  If variant differs, check the number
    // special case (Int and Float can be equal).
    let same_variant = std::mem::discriminant(t1) == std::mem::discriminant(t2);
    if !same_variant {
        let t1_is_num = matches!(t1, LuaValue::Int(_) | LuaValue::Float(_));
        let t2_is_num = matches!(t2, LuaValue::Int(_) | LuaValue::Float(_));
        if !(t1_is_num && t2_is_num) {
            return Ok(false);
        }
        // luaV_tointegerns(t1, &i1, F2Ieq) && luaV_tointegerns(t2, &i2, F2Ieq) && i1==i2
        let i1 = to_integer_ns(t1, F2Imod::Eq);
        let i2 = to_integer_ns(t2, F2Imod::Eq);
        return Ok(i1.is_some() && i2.is_some() && i1 == i2);
    }

    match (t1, t2) {
        (LuaValue::Nil,  LuaValue::Nil)  => Ok(true),
        (LuaValue::Bool(b1), LuaValue::Bool(b2)) => Ok(b1 == b2),
        (LuaValue::Int(i1), LuaValue::Int(i2)) => Ok(i1 == i2),
        (LuaValue::Float(f1), LuaValue::Float(f2)) => Ok(f1 == f2),
        (LuaValue::LightUserData(p1), LuaValue::LightUserData(p2)) => Ok(p1 == p2),
        (LuaValue::Function(f1), LuaValue::Function(f2)) => {
            use lua_types::closure::LuaClosure;
            let same = match (f1, f2) {
                (LuaClosure::Lua(a), LuaClosure::Lua(b)) => GcRef::ptr_eq(a, b),
                (LuaClosure::C(a), LuaClosure::C(b)) => GcRef::ptr_eq(a, b),
                (LuaClosure::LightC(a), LuaClosure::LightC(b)) => a == b,
                _ => false,
            };
            Ok(same)
        }
        (LuaValue::Str(s1), LuaValue::Str(s2)) => {
            //    luaS_eqlngstr for long strings (content eq).
            // In Rust, LuaString PartialEq handles both.
            Ok(s1 == s2)
        }
        (LuaValue::UserData(u1), LuaValue::UserData(u2)) => {
            //    else if (L == NULL) return 0;
            //    tm = fasttm(L, uvalue(t1)->metatable, TM_EQ);
            if std::ptr::eq(u1.as_ptr(), u2.as_ptr()) {
                return Ok(true);
            }
            let Some(state) = state else { return Ok(false); };
            let tm1 = state.fast_tm_ud(u1, TagMethod::Eq);
            let tm = if matches!(tm1, LuaValue::Nil) {
                state.fast_tm_ud(u2, TagMethod::Eq)
            } else {
                tm1
            };
            if matches!(tm, LuaValue::Nil) {
                return Ok(false);
            }
            let result = state.call_tm_res_bool(tm, t1, t2)?;
            Ok(result)
        }
        (LuaValue::Table(h1), LuaValue::Table(h2)) => {
            if std::ptr::eq(h1.as_ptr(), h2.as_ptr()) {
                return Ok(true);
            }
            let Some(state) = state else { return Ok(false); };
            //    if (tm == NULL) tm = fasttm(L, hvalue(t2)->metatable, TM_EQ);
            let mt1 = h1.metatable();
            let mt2 = h2.metatable();
            let tm1 = state.fast_tm_table(mt1.as_ref(), TagMethod::Eq);
            let tm = if matches!(tm1, LuaValue::Nil) {
                state.fast_tm_table(mt2.as_ref(), TagMethod::Eq)
            } else {
                tm1
            };
            if matches!(tm, LuaValue::Nil) {
                return Ok(false);
            }
            let result = state.call_tm_res_bool(tm, t1, t2)?;
            Ok(result)
        }
        (LuaValue::Thread(a), LuaValue::Thread(b)) => Ok(GcRef::ptr_eq(a, b)),
        _ => Ok(std::ptr::eq(t1 as *const _, t2 as *const _)),
    }
}

// ─── Concatenation ───────────────────────────────────────────────────────────

/// Copy `n` strings from `top-n .. top-1` into `buff`.
fn copy_to_buf(state: &LuaState, top: StackIdx, n: u32, buf: &mut Vec<u8>) {
    buf.clear();
    let mut remaining = n;
    loop {
        let idx = top - remaining as i32;
        let v = state.get_at(idx);
        if let LuaValue::Str(ts) = v {
            buf.extend_from_slice(ts.as_bytes());
        }
        if remaining <= 1 {
            break;
        }
        remaining -= 1;
    }
}

/// Concatenate `total` values on the top of the stack, leaving one result.
pub(crate) fn concat(state: &mut LuaState, total: i32) -> Result<(), LuaError> {
    if total == 1 {
        return Ok(());
    }
    let mut total = total;
    loop {
        let top = state.top_idx();
        let v_tm1 = state.get_at(top - 1); // top-1
        let v_tm2 = state.get_at(top - 2); // top-2

        //    luaT_tryconcatTM(L);
        let top2_coercible = matches!(v_tm2, LuaValue::Str(_))
            || matches!(v_tm2, LuaValue::Int(_) | LuaValue::Float(_));
        // tostring converts numbers to strings; we check top-1 too
        let top1_stringlike = matches!(v_tm1, LuaValue::Str(_))
            || matches!(v_tm1, LuaValue::Int(_) | LuaValue::Float(_));
        if !top2_coercible || !top1_stringlike {
            state.try_concat_tm(&v_tm1, &v_tm2)?;
            // at the bottom of the do-while runs for this branch too.
            // The metamethod writes its single result to top-2, leaving
            // top-1 stale; popping that stale slot is what makes the next
            // iteration see the just-computed result at the new top-1.
            total -= 1;
            let top = state.top_idx();
            state.set_top(top - 1);
            if total <= 1 {
                break;
            }
            continue;
        }

        let is_empty = |v: &LuaValue| -> bool {
            matches!(v, LuaValue::Str(s) if s.as_bytes().is_empty())
        };

        let n: u32;
        if is_empty(&v_tm1) {
            state.coerce_to_string(top - 2)?;
            n = 2;
        } else if is_empty(&v_tm2) {
            // so top-1 is guaranteed to be a string here. We replicate that
            // conversion before the copy so numbers don't leak through.
            state.coerce_to_string(top - 1)?;
            let v = state.get_at(top - 1);
            state.set_at(top - 2, v);
            n = 2;
        } else {
            // Ensure top-1 is a string (coerce if number)
            state.coerce_to_string(top - 1)?;
            let s1 = match state.get_at(top - 1) {
                LuaValue::Str(ts) => ts.as_bytes().len(),
                _ => 0,
            };
            let mut total_len = s1;
            let mut count: u32 = 1;
            let top = state.top_idx();
            loop {
                if count as i32 >= total {
                    break;
                }
                let idx = top - (count as i32 + 1);
                let v = state.get_at(idx);
                if !matches!(v, LuaValue::Str(_) | LuaValue::Int(_) | LuaValue::Float(_)) {
                    break;
                }
                state.coerce_to_string(idx)?;
                let l = match state.get_at(idx) {
                    LuaValue::Str(ts) => ts.as_bytes().len(),
                    _ => 0,
                };
                if l >= usize::MAX - total_len {
                    // pop strings to avoid wasting stack
                    state.set_top(top - total as i32);
                    return Err(LuaError::runtime(format_args!("string length overflow")));
                }
                total_len += l;
                count += 1;
            }
            n = count;

            // Build concatenated result
            let mut buf: Vec<u8> = Vec::with_capacity(total_len);
            let top = state.top_idx();
            copy_to_buf(state, top, n, &mut buf);
            let ts = state.intern_or_create_str(&buf)?;
            state.set_at(top - n as i32, LuaValue::Str(ts));
        }
        total -= n as i32 - 1;
        let top = state.top_idx();
        state.set_top(top - ((n - 1) as i32));

        if total <= 1 {
            break;
        }
    }
    Ok(())
}

// ─── Object length ───────────────────────────────────────────────────────────

/// Main implementation of the `#` operator.
pub(crate) fn obj_len(state: &mut LuaState, ra: StackIdx, rb: LuaValue) -> Result<(), LuaError> {
    match &rb {
        LuaValue::Table(_) => {
            //    if (tm) break; else setivalue(s2v(ra), luaH_getn(h));
            let mt = state.table_metatable(&rb);
            let tm = state.fast_tm_table(mt.as_ref(), TagMethod::Len);
            if matches!(tm, LuaValue::Nil) {
                let n = state.table_length(&rb)?;
                state.set_at(ra, LuaValue::Int(n as i64));
                return Ok(());
            }
            // Fall through to call metamethod
            state.call_tm_res(tm, &rb, &rb, ra)?;
        }
        LuaValue::Str(ts) => {
            //    case LUA_VLNGSTR: setivalue(s2v(ra), tsvalue(rb)->u.lnglen);
            // Unified in Rust — just get length
            let n = ts.len();
            state.set_at(ra, LuaValue::Int(n as i64));
        }
        other => {
            //    if (notm(tm)) luaG_typeerror(L, rb, "get length of");
            let tm = state.get_tm_by_obj(other, TagMethod::Len);
            if matches!(tm, LuaValue::Nil) {
                return Err(LuaError::type_error(other, "get length of"));
            }
            state.call_tm_res(tm, &rb, &rb, ra)?;
        }
    }
    Ok(())
}

// ─── Integer arithmetic ──────────────────────────────────────────────────────

/// Integer floor-division.
pub(crate) fn idiv(m: i64, n: i64) -> Result<i64, LuaError> {
    if (n as u64).wrapping_add(1) <= 1 {
        if n == 0 {
            return Err(LuaError::runtime(format_args!("attempt to divide by zero")));
        }
        return Ok(intop_sub(0, m));
    }
    let q = m / n;
    // Correct toward floor (C division truncates toward zero)
    if (m ^ n) < 0 && m % n != 0 {
        Ok(q - 1)
    } else {
        Ok(q)
    }
}

/// Integer modulus (Lua semantics: same sign as divisor).
pub(crate) fn imod(m: i64, n: i64) -> Result<i64, LuaError> {
    if (n as u64).wrapping_add(1) <= 1 {
        if n == 0 {
            return Err(LuaError::runtime(format_args!("attempt to perform 'n%0'")));
        }
        return Ok(0);
    }
    let r = m % n;
    if r != 0 && (r ^ n) < 0 {
        Ok(r + n)
    } else {
        Ok(r)
    }
}

/// Float modulus (Lua semantics).
pub(crate) fn fmodf(m: f64, n: f64) -> f64 {
    let r = m % n;
    let opposite_signs = if r > 0.0 { n < 0.0 } else { r < 0.0 && n > 0.0 };
    if opposite_signs {
        r + n
    } else {
        r
    }
}

/// Phase-B helper: map a u8 raw value to a `TagMethod`. Mirrors C's
/// `cast(TMS, x)` direct cast; out-of-range returns `TagMethod::Index`.
pub(crate) fn tagmethod_from_index(i: usize) -> TagMethod {
    use TagMethod::*;
    match i {
        0 => Index, 1 => NewIndex, 2 => Gc, 3 => Mode, 4 => Len, 5 => Eq,
        6 => Add, 7 => Sub, 8 => Mul, 9 => Mod, 10 => Pow, 11 => Div,
        12 => Idiv, 13 => Band, 14 => Bor, 15 => Bxor, 16 => Shl, 17 => Shr,
        18 => Unm, 19 => Bnot, 20 => Lt, 21 => Le, 22 => Concat, 23 => Call,
        24 => Close,
        _ => Index,
    }
}

/// Integer floor-mod: Lua's `%` operator on integers. Result has the same sign
/// as the divisor. Raises on `n == 0`.
pub(crate) fn int_floor_mod(_state: &mut LuaState, a: i64, b: i64) -> Result<i64, LuaError> {
    imod(a, b)
}

/// Integer floor-div: Lua's `//` operator on integers. Truncates toward
/// negative infinity. Raises on `n == 0`.
pub(crate) fn int_floor_div(_state: &mut LuaState, a: i64, b: i64) -> Result<i64, LuaError> {
    idiv(a, b)
}

/// Float floor-mod: Lua's `%` operator on floats. Result has the same sign as
/// the divisor.  NaN / division-by-zero behavior mirrors C `fmod`.
pub(crate) fn float_floor_mod(_state: &mut LuaState, a: f64, b: f64) -> Result<f64, LuaError> {
    Ok(fmodf(a, b))
}

/// Left shift; right shift is shift-left by negated count.
pub(crate) fn shiftl(x: i64, y: i64) -> i64 {
    if y < 0 {
        if y <= -(NBITS as i64) {
            0
        } else {
            intop_shr(x, (-y) as u32)
        }
    } else {
        if y >= NBITS as i64 {
            0
        } else {
            intop_shl(x, y as u32)
        }
    }
}

// ─── Closure creation ────────────────────────────────────────────────────────

///                              StkId base, StkId ra)`
/// Create a new Lua closure from prototype `p`, initialise its upvalues,
/// and push it onto the stack at `ra`.
fn push_closure(
    state: &mut LuaState,
    proto_idx: usize,   // index into current closure's proto.p[]
    ci: CallInfoIdx,
    base: StackIdx,
    ra: StackIdx,
) -> Result<(), LuaError> {
    // TODO(port): pushclosure needs access to the enclosing closure's upvals and
    // the child proto from the current frame.  This stub forwards to a LuaState
    // method that has the required context.
    state.push_closure(proto_idx, ci, base, ra)
}

// ─── Yield recovery ──────────────────────────────────────────────────────────

/// Resume the opcode that was interrupted by a yield.
/// Called when a coroutine is resumed after yielding mid-instruction.
pub(crate) fn finish_op(state: &mut LuaState) -> Result<(), LuaError> {
    //    StkId base = ci->func.p + 1;
    //    Instruction inst = *(ci->u.l.savedpc - 1);
    //    OpCode op = GET_OPCODE(inst);
    let ci = state.current_ci_idx();
    let base = state.ci_base(ci);
    let inst = state.ci_prev_instruction(ci);
    let op = inst.opcode();

    match op {
        //    setobjs2s(L, base + GETARG_A(*(ci->u.l.savedpc - 2)), --L->top.p);
        OpCode::MmBin | OpCode::MmBinI | OpCode::MmBinK => {
            let prev_inst = state.ci_prev2_instruction(ci);
            let a = prev_inst.arg_a();
            state.dec_top();
            let top = state.top_idx();
            let v = state.get_at(top);
            state.set_at(base + a, v);
        }
        //    setobjs2s(L, base + GETARG_A(inst), --L->top.p);
        OpCode::Unm | OpCode::BNot | OpCode::Len
        | OpCode::GetTabUp | OpCode::GetTable | OpCode::GetI
        | OpCode::GetField | OpCode::Self_ => {
            let a = inst.arg_a();
            state.dec_top();
            let top = state.top_idx();
            let v = state.get_at(top);
            state.set_at(base + a, v);
        }
        //    case OP_GTI: case OP_GEI: case OP_EQ:
        //    int res = !l_isfalse(s2v(L->top.p - 1)); L->top.p--;
        //    if (res != GETARG_k(inst)) ci->u.l.savedpc++;
        OpCode::Lt | OpCode::Le | OpCode::LtI | OpCode::LeI
        | OpCode::GtI | OpCode::GeI | OpCode::Eq => {
            let top_minus1 = state.top_idx() - 1;
            let v = state.get_at(top_minus1);
            let res = !matches!(v, LuaValue::Nil | LuaValue::Bool(false));
            state.dec_top();
            if (res as i32) != inst.arg_k() {
                state.ci_skip_next_instruction(ci);
            }
            // Note: CIST_LEQ compatibility not supported (LUA_COMPAT_LT_LE dropped)
        }
        //    StkId top = L->top.p - 1;
        //    int a = GETARG_A(inst);
        //    int total = cast_int(top - 1 - (base + a));
        //    setobjs2s(L, top - 2, top);  L->top.p = top - 1;
        //    luaV_concat(L, total);
        OpCode::Concat => {
            let top = state.top_idx() - 1; // top when luaT_tryconcatTM was called
            let a = inst.arg_a();
            let total_concat = (top - 1 - (base + a)) as i32;
            let v = state.get_at(top);
            state.set_at(top - 2, v);
            state.set_top(top - 1);
            concat(state, total_concat)?;
        }
        OpCode::Close => {
            state.ci_step_pc_back(ci);
        }
        //    StkId ra = base + GETARG_A(inst);
        //    L->top.p = ra + ci->u2.nres;
        //    ci->u.l.savedpc--;
        OpCode::Return => {
            let a = inst.arg_a();
            let ra = base + a;
            let nres = state.ci_nres(ci);
            state.set_top(ra + nres);
            state.ci_step_pc_back(ci);
        }
        other => {
            debug_assert!(
                matches!(
                    other,
                    OpCode::TForCall | OpCode::Call | OpCode::TailCall
                    | OpCode::SetTabUp | OpCode::SetTable | OpCode::SetI | OpCode::SetField
                ),
                "unexpected opcode in finish_op: {:?}",
                other
            );
        }
    }
    Ok(())
}

// ─── Main interpreter loop ───────────────────────────────────────────────────

/// Main Lua bytecode interpreter loop.
///
/// # Control flow modelling
/// The C function uses goto labels: `startfunc`, `returning`, `ret`,
/// `l_tforcall`, `l_tforloop`.  These are modelled as follows:
/// - `'startfunc: loop { ... }` — outer loop; `continue 'startfunc` = goto startfunc
/// - `'returning: loop { ... }` — inner loop; `continue 'returning` = goto returning
/// - `break 'dispatch` from the inner dispatch loop → runs `ret:` logic
/// - `l_tforcall` / `l_tforloop` — inlined at TFORPREP / TFORCALL handlers
pub(crate) fn execute(state: &mut LuaState, mut ci: CallInfoIdx) -> Result<(), LuaError> {
    let mut trap: bool;

    // PORT NOTE: `startfunc:` is the entry point that (re)sets `trap`.
    'startfunc: loop {
        trap = state.hook_mask() != 0;

        // PORT NOTE: `returning:` is the re-entry after a Lua call returns.
        // Re-enters 'returning without resetting trap.
        'returning: loop {
            let cl = match state.ci_lua_closure(ci) {
                Some(c) => c,
                None => {
                    return Err(LuaError::runtime(format_args!(
                        "internal: execute called on non-Lua frame"
                    )));
                }
            };
            // pc is an index into proto.code (u32)
            let mut pc: u32 = state.ci_savedpc(ci);

            if trap {
                trap = state.trace_call(ci)?;
            }
            let mut base: StackIdx = state.ci_base(ci);

            // ── Main dispatch loop ──────────────────────────────────────────
            'dispatch: loop {
                if trap {
                    trap = state.trace_exec(ci, pc)?;
                    base = state.ci_base(ci); // updatebase
                }
                let i: Instruction = state.proto_code(&cl, pc);
                pc += 1;
                let op = i.opcode();

                debug_assert!(base == state.ci_base(ci));

                // In normal C-Lua builds, `lua_assert` compiles away; keep the
                // stack-top invalidation only for debug parity so release
                // dispatch avoids an opcode-mode lookup and a `top` write.
                #[cfg(debug_assertions)]
                {
                    let op_mode = op_mode_byte(op);
                    if (op_mode & (1 << 5)) == 0 || i.arg_b() != 0 {
                        state.set_top(base);
                    }
                }

                match op {
                    // ── OP_MOVE ──────────────────────────────────────────────
                    OpCode::Move => {
                        let ra = base + i.arg_a();
                        let rb = base + i.arg_b();
                        let v = state.stack[rb.0 as usize].val;
                        state.stack[ra.0 as usize].val = v;
                    }
                    // ── OP_LOADI ─────────────────────────────────────────────
                    OpCode::LoadI => {
                        let ra = base + i.arg_a();
                        let b = i.arg_s_bx() as i64;
                        state.set_at(ra, LuaValue::Int(b));
                    }
                    // ── OP_LOADF ─────────────────────────────────────────────
                    OpCode::LoadF => {
                        let ra = base + i.arg_a();
                        let b = i.arg_s_bx() as f64;
                        state.set_at(ra, LuaValue::Float(b));
                    }
                    // ── OP_LOADK ─────────────────────────────────────────────
                    OpCode::LoadK => {
                        let ra = base + i.arg_a();
                        let k_idx = i.arg_bx() as usize;
                        let v = state.proto_const(&cl, k_idx).clone();
                        state.set_at(ra, v);
                    }
                    // ── OP_LOADKX ────────────────────────────────────────────
                    OpCode::LoadKX => {
                        let ra = base + i.arg_a();
                        let extra = state.proto_code(&cl, pc);
                        pc += 1;
                        let k_idx = extra.arg_ax() as usize;
                        let v = state.proto_const(&cl, k_idx).clone();
                        state.set_at(ra, v);
                    }
                    // ── OP_LOADFALSE ─────────────────────────────────────────
                    OpCode::LoadFalse => {
                        let ra = base + i.arg_a();
                        state.set_at(ra, LuaValue::Bool(false));
                    }
                    // ── OP_LFALSESKIP ────────────────────────────────────────
                    OpCode::LFalseSkip => {
                        let ra = base + i.arg_a();
                        state.set_at(ra, LuaValue::Bool(false));
                        pc += 1;
                    }
                    // ── OP_LOADTRUE ──────────────────────────────────────────
                    OpCode::LoadTrue => {
                        let ra = base + i.arg_a();
                        state.set_at(ra, LuaValue::Bool(true));
                    }
                    // ── OP_LOADNIL ───────────────────────────────────────────
                    OpCode::LoadNil => {
                        let ra = base + i.arg_a();
                        let b = i.arg_b();
                        for k in 0..=b {
                            state.set_at(ra + k, LuaValue::Nil);
                        }
                    }
                    // ── OP_GETUPVAL ──────────────────────────────────────────
                    OpCode::GetUpVal => {
                        let ra = base + i.arg_a();
                        let b = i.arg_b() as usize;
                        let v = state.upvalue_get(&cl, b);
                        state.set_at(ra, v);
                    }
                    // ── OP_SETUPVAL ──────────────────────────────────────────
                    //    setobj(L, uv->v.p, s2v(ra)); luaC_barrier(L, uv, s2v(ra));
                    OpCode::SetUpVal => {
                        let ra = base + i.arg_a();
                        let b = i.arg_b() as usize;
                        let v = state.stack[ra.0 as usize].val;
                        let uv = cl.upval(b);
                        match uv.try_open_payload() {
                            Some((thread_id, idx)) if thread_id as u64 == state.cached_thread_id => {
                                state.stack[idx.0 as usize].val = v;
                            }
                            _ => {
                                state.upvalue_set(&cl, b, v)?;
                            }
                        }
                    }
                    // ── OP_GETTABUP ──────────────────────────────────────────
                    //    if (luaV_fastget(..., luaH_getshortstr)) setobj2s(L, ra, slot)
                    //    else Protect(luaV_finishget(...))
                    OpCode::GetTabUp => {
                        let ra = base + i.arg_a();
                        let b = i.arg_b() as usize;
                        let k_idx = i.arg_c() as usize;
                        let upval = state.upvalue_get(&cl, b);
                        let key = state.proto_const(&cl, k_idx).clone();
                        match state.fast_get_short_str(&upval, &key)? {
                            Some(v) => state.set_at(ra, v),
                            None => {
                                state.set_ci_savedpc(ci, pc);
                                state.set_top(state.ci_top(ci));
                                finish_get(state, upval, key, ra, true, None)?;
                                trap = state.ci_trap(ci);
                            }
                        }
                    }
                    // ── OP_GETTABLE ──────────────────────────────────────────
                    //    if (integer key) fastgeti else fastget
                    OpCode::GetTable => {
                        let ra = base + i.arg_a();
                        let rb_idx = base + i.arg_b();
                        let rb_v = state.get_at(rb_idx);
                        let rc_v = state.get_at(base + i.arg_c());
                        let fast_result = if let LuaValue::Int(n) = &rc_v {
                            state.fast_get_int(&rb_v, *n)?
                        } else {
                            state.fast_get(&rb_v, &rc_v)?
                        };
                        match fast_result {
                            Some(v) => state.set_at(ra, v),
                            None => {
                                state.set_ci_savedpc(ci, pc);
                                state.set_top(state.ci_top(ci));
                                finish_get(state, rb_v, rc_v, ra, true, Some(rb_idx))?;
                                trap = state.ci_trap(ci);
                            }
                        }
                    }
                    // ── OP_GETI ──────────────────────────────────────────────
                    //    if (luaV_fastgeti(L, rb, c, slot)) setobj2s(L, ra, slot)
                    //    else { TValue key; setivalue(&key, c); Protect(finishget) }
                    OpCode::GetI => {
                        let ra = base + i.arg_a();
                        let rb_idx = base + i.arg_b();
                        let rb_v = state.get_at(rb_idx);
                        let c = i.arg_c() as i64;
                        match state.fast_get_int(&rb_v, c)? {
                            Some(v) => state.set_at(ra, v),
                            None => {
                                let key = LuaValue::Int(c);
                                state.set_ci_savedpc(ci, pc);
                                state.set_top(state.ci_top(ci));
                                finish_get(state, rb_v, key, ra, true, Some(rb_idx))?;
                                trap = state.ci_trap(ci);
                            }
                        }
                    }
                    // ── OP_GETFIELD ──────────────────────────────────────────
                    OpCode::GetField => {
                        let ra = base + i.arg_a();
                        let rb_idx = base + i.arg_b();
                        let rb_v = state.get_at(rb_idx);
                        let k_idx = i.arg_c() as usize;
                        let key = state.proto_const(&cl, k_idx).clone();
                        match state.fast_get_short_str(&rb_v, &key)? {
                            Some(v) => state.set_at(ra, v),
                            None => {
                                state.set_ci_savedpc(ci, pc);
                                state.set_top(state.ci_top(ci));
                                finish_get(state, rb_v, key, ra, true, Some(rb_idx))?;
                                trap = state.ci_trap(ci);
                            }
                        }
                    }
                    // ── OP_SETTABUP ──────────────────────────────────────────
                    OpCode::SetTabUp => {
                        let a = i.arg_a() as usize;
                        let b_idx = i.arg_b() as usize; // key is KB(i)
                        let rc_v = if i.test_k() {
                            state.proto_const(&cl, i.arg_c() as usize).clone()
                        } else {
                            state.get_at(base + i.arg_c())
                        };
                        let upval = state.upvalue_get(&cl, a);
                        let key = state.proto_const(&cl, b_idx).clone();
                        match state.fast_get_short_str(&upval, &key)? {
                            Some(_slot) => {
                                state.table_raw_set(&upval, key, rc_v.clone())?;
                                state.gc_barrier_back(&upval, &rc_v);
                            }
                            None => {
                                state.set_ci_savedpc(ci, pc);
                                state.set_top(state.ci_top(ci));
                                let upval_name: Vec<u8> = cl
                                    .proto
                                    .upvalues
                                    .get(a)
                                    .and_then(|uv| uv.name.as_ref())
                                    .map(|s| s.as_bytes().to_vec())
                                    .unwrap_or_else(|| b"?".to_vec());
                                let hint: Option<(&[u8], &[u8])> =
                                    Some((b"upvalue", &upval_name));
                                finish_set(state, upval, key, rc_v, false, None, hint)?;
                                trap = state.ci_trap(ci);
                            }
                        }
                    }
                    // ── OP_SETTABLE ───────────────────────────────────────────
                    OpCode::SetTable => {
                        let ra_idx = base + i.arg_a();
                        let ra_v = state.get_at(ra_idx);
                        let rb_v = state.get_at(base + i.arg_b());
                        let rc_v = if i.test_k() {
                            state.proto_const(&cl, i.arg_c() as usize).clone()
                        } else {
                            state.get_at(base + i.arg_c())
                        };
                        let fast = if let LuaValue::Int(n) = &rb_v {
                            state.fast_get_int(&ra_v, *n)?
                        } else {
                            state.fast_get(&ra_v, &rb_v)?
                        };
                        if fast.is_some() {
                            state.table_raw_set(&ra_v, rb_v, rc_v.clone())?;
                            state.gc_barrier_back(&ra_v, &rc_v);
                        } else {
                            state.set_ci_savedpc(ci, pc);
                            state.set_top(state.ci_top(ci));
                            finish_set(state, ra_v, rb_v, rc_v, false, Some(ra_idx), None)?;
                            trap = state.ci_trap(ci);
                        }
                    }
                    // ── OP_SETI ───────────────────────────────────────────────
                    OpCode::SetI => {
                        let ra_idx = base + i.arg_a();
                        let ra_v = state.get_at(ra_idx);
                        let c = i.arg_b() as i64;
                        let rc_v = if i.test_k() {
                            state.proto_const(&cl, i.arg_c() as usize).clone()
                        } else {
                            state.get_at(base + i.arg_c())
                        };
                        let fast = state.fast_get_int(&ra_v, c)?;
                        if fast.is_some() {
                            state.table_raw_set(&ra_v, LuaValue::Int(c), rc_v.clone())?;
                            state.gc_barrier_back(&ra_v, &rc_v);
                        } else {
                            state.set_ci_savedpc(ci, pc);
                            state.set_top(state.ci_top(ci));
                            finish_set(state, ra_v, LuaValue::Int(c), rc_v, false, Some(ra_idx), None)?;
                            trap = state.ci_trap(ci);
                        }
                    }
                    // ── OP_SETFIELD ───────────────────────────────────────────
                    OpCode::SetField => {
                        let ra_idx = base + i.arg_a();
                        let ra_v = state.get_at(ra_idx);
                        let b_idx = i.arg_b() as usize;
                        let key = state.proto_const(&cl, b_idx).clone();
                        let rc_v = if i.test_k() {
                            state.proto_const(&cl, i.arg_c() as usize).clone()
                        } else {
                            state.get_at(base + i.arg_c())
                        };
                        match state.fast_get_short_str(&ra_v, &key)? {
                            Some(_) => {
                                state.table_raw_set(&ra_v, key, rc_v.clone())?;
                                state.gc_barrier_back(&ra_v, &rc_v);
                            }
                            None => {
                                state.set_ci_savedpc(ci, pc);
                                state.set_top(state.ci_top(ci));
                                finish_set(state, ra_v, key, rc_v, false, Some(ra_idx), None)?;
                                trap = state.ci_trap(ci);
                            }
                        }
                    }
                    // ── OP_NEWTABLE ───────────────────────────────────────────
                    //    if (TESTARG_k(i)) c += GETARG_Ax(*pc) * (MAXARG_C + 1); pc++;
                    OpCode::NewTable => {
                        let ra = base + i.arg_a();
                        let mut b = i.arg_b();
                        let mut c = i.arg_c();
                        if b > 0 {
                            b = 1 << (b - 1);
                        }
                        if i.test_k() {
                            let extra = state.proto_code(&cl, pc);
                            pc += 1;
                            const MAXARG_C: i32 = (1 << 8) - 1;
                            c += extra.arg_ax() * (MAXARG_C + 1);
                        } else {
                            pc += 1; // skip extra argument even if zero
                        }
                        state.set_top(ra + 1);
                        let t = if b != 0 || c != 0 {
                            state.new_table_with_sizes(b as u32, c as u32)?
                        } else {
                            state.new_table()
                        };
                        state.set_at(ra, LuaValue::Table(t.clone()));
                        state.set_ci_savedpc(ci, pc);
                        state.set_top(ra + 1);
                        state.gc_cond_step();
                        if state.hookmask != 0 {
                            trap = state.ci_trap(ci);
                        }
                    }
                    // ── OP_SELF ───────────────────────────────────────────────
                    OpCode::Self_ => {
                        let ra = base + i.arg_a();
                        let rb_idx = base + i.arg_b();
                        let rb_v = state.get_at(rb_idx);
                        let k_idx = i.arg_c() as usize; // RKC key (always a string)
                        let key = if i.test_k() {
                            state.proto_const(&cl, k_idx).clone()
                        } else {
                            state.get_at(base + i.arg_c())
                        };
                        state.set_at(ra + 1, rb_v.clone());
                        match state.fast_get_short_str(&rb_v, &key)? {
                            Some(v) => state.set_at(ra, v),
                            None => {
                                state.set_ci_savedpc(ci, pc);
                                state.set_top(state.ci_top(ci));
                                finish_get(state, rb_v, key, ra, true, Some(rb_idx))?;
                                trap = state.ci_trap(ci);
                            }
                        }
                    }
                    // ── Arithmetic immediates ──────────────────────────────────
                    OpCode::AddI => {
                        let ra = base + i.arg_a();
                        let rb = base + i.arg_b();
                        let imm = i.arg_s_c() as i64;
                        let rb_v = state.stack[rb.0 as usize].val;
                        match rb_v {
                            LuaValue::Int(iv1) => {
                                pc += 1;
                                state.stack[ra.0 as usize].val = LuaValue::Int(intop_add(iv1, imm));
                            }
                            LuaValue::Float(nb) => {
                                pc += 1;
                                state.stack[ra.0 as usize].val = LuaValue::Float(nb + imm as f64);
                            }
                            _ => {}
                        }
                    }
                    // ── Arithmetic with K constant operand ─────────────────────
                    OpCode::AddK => {
                        let ra = base + i.arg_a();
                        let rb = base + i.arg_b();
                        let kidx = i.arg_c() as usize;
                        if let (Some(i1), Some(i2)) = (state.get_int_at(rb), state.proto_const_int(&cl, kidx)) {
                            pc += 1;
                            state.set_at(ra, LuaValue::Int(intop_add(i1, i2)));
                        } else if let (Some(n1), Some(n2)) = (state.get_num_at(rb), state.proto_const_num(&cl, kidx)) {
                            pc += 1;
                            state.set_at(ra, LuaValue::Float(n1 + n2));
                        }
                    }
                    OpCode::SubK => {
                        let ra = base + i.arg_a();
                        let rb = base + i.arg_b();
                        let kidx = i.arg_c() as usize;
                        if let (Some(i1), Some(i2)) = (state.get_int_at(rb), state.proto_const_int(&cl, kidx)) {
                            pc += 1;
                            state.set_at(ra, LuaValue::Int(intop_sub(i1, i2)));
                        } else if let (Some(n1), Some(n2)) = (state.get_num_at(rb), state.proto_const_num(&cl, kidx)) {
                            pc += 1;
                            state.set_at(ra, LuaValue::Float(n1 - n2));
                        }
                    }
                    OpCode::MulK => {
                        let ra = base + i.arg_a();
                        let rb = base + i.arg_b();
                        let kidx = i.arg_c() as usize;
                        if let (Some(i1), Some(i2)) = (state.get_int_at(rb), state.proto_const_int(&cl, kidx)) {
                            pc += 1;
                            state.set_at(ra, LuaValue::Int(intop_mul(i1, i2)));
                        } else if let (Some(n1), Some(n2)) = (state.get_num_at(rb), state.proto_const_num(&cl, kidx)) {
                            pc += 1;
                            state.set_at(ra, LuaValue::Float(n1 * n2));
                        }
                    }
                    OpCode::ModK => {
                        let ra = base + i.arg_a();
                        let v1 = state.get_at(base + i.arg_b());
                        let v2 = state.proto_const(&cl, i.arg_c() as usize).clone();
                        state.set_ci_savedpc(ci, pc); // savestate for div-by-zero
                        state.set_top(state.ci_top(ci));
                        arith_op_checked(state, ra, &v1, &v2, &mut pc,
                            |a, b| imod(a, b), fmodf)?;
                    }
                    OpCode::PowK => {
                        let ra = base + i.arg_a();
                        let rb = base + i.arg_b();
                        let kidx = i.arg_c() as usize;
                        if let (Some(n1), Some(n2)) = (state.get_num_at(rb), state.proto_const_num(&cl, kidx)) {
                            pc += 1;
                            let r = if n2 == 2.0 { n1 * n1 } else { n1.powf(n2) };
                            state.set_at(ra, LuaValue::Float(r));
                        }
                    }
                    OpCode::DivK => {
                        let ra = base + i.arg_a();
                        let rb = base + i.arg_b();
                        let kidx = i.arg_c() as usize;
                        if let (Some(n1), Some(n2)) = (state.get_num_at(rb), state.proto_const_num(&cl, kidx)) {
                            pc += 1;
                            state.set_at(ra, LuaValue::Float(n1 / n2));
                        }
                    }
                    OpCode::IDivK => {
                        let ra = base + i.arg_a();
                        let v1 = state.get_at(base + i.arg_b());
                        let v2 = state.proto_const(&cl, i.arg_c() as usize).clone();
                        state.set_ci_savedpc(ci, pc);
                        state.set_top(state.ci_top(ci));
                        arith_op_checked(state, ra, &v1, &v2, &mut pc,
                            |a, b| idiv(a, b), |a, b| (a / b).floor())?;
                    }
                    OpCode::BAndK => {
                        let ra = base + i.arg_a();
                        let v1 = state.get_at(base + i.arg_b());
                        let v2 = state.proto_const(&cl, i.arg_c() as usize).clone();
                        bitwise_op_k(state, ra, &v1, &v2, &mut pc, intop_band);
                    }
                    OpCode::BOrK => {
                        let ra = base + i.arg_a();
                        let v1 = state.get_at(base + i.arg_b());
                        let v2 = state.proto_const(&cl, i.arg_c() as usize).clone();
                        bitwise_op_k(state, ra, &v1, &v2, &mut pc, intop_bor);
                    }
                    OpCode::BXOrK => {
                        let ra = base + i.arg_a();
                        let v1 = state.get_at(base + i.arg_b());
                        let v2 = state.proto_const(&cl, i.arg_c() as usize).clone();
                        bitwise_op_k(state, ra, &v1, &v2, &mut pc, intop_bxor);
                    }
                    OpCode::ShrI => {
                        let ra = base + i.arg_a();
                        let v = state.get_at(base + i.arg_b());
                        let ic = i.arg_s_c() as i64;
                        if let Some(ib) = to_integer_ns(&v, F2Imod::Eq) {
                            pc += 1;
                            state.set_at(ra, LuaValue::Int(shiftl(ib, -ic)));
                        }
                    }
                    OpCode::ShlI => {
                        let ra = base + i.arg_a();
                        let v = state.get_at(base + i.arg_b());
                        let ic = i.arg_s_c() as i64;
                        if let Some(ib) = to_integer_ns(&v, F2Imod::Eq) {
                            pc += 1;
                            state.set_at(ra, LuaValue::Int(shiftl(ic, ib)));
                        }
                    }
                    // ── Arithmetic with register operands ──────────────────────
                    OpCode::Add => {
                        let ra = base + i.arg_a();
                        let rb = base + i.arg_b();
                        let rc = base + i.arg_c();
                        let ra_u = ra.0 as usize;
                        let rb_v = state.stack[rb.0 as usize].val;
                        let rc_v = state.stack[rc.0 as usize].val;
                        if let (LuaValue::Int(i1), LuaValue::Int(i2)) = (rb_v, rc_v) {
                            pc += 1;
                            state.stack[ra_u].val = LuaValue::Int(intop_add(i1, i2));
                        } else if let (Some(n1), Some(n2)) = (number_value(rb_v), number_value(rc_v)) {
                            pc += 1;
                            state.stack[ra_u].val = LuaValue::Float(n1 + n2);
                        }
                    }
                    OpCode::Sub => {
                        let ra = base + i.arg_a();
                        let rb = base + i.arg_b();
                        let rc = base + i.arg_c();
                        let ra_u = ra.0 as usize;
                        let rb_v = state.stack[rb.0 as usize].val;
                        let rc_v = state.stack[rc.0 as usize].val;
                        if let (LuaValue::Int(i1), LuaValue::Int(i2)) = (rb_v, rc_v) {
                            pc += 1;
                            state.stack[ra_u].val = LuaValue::Int(intop_sub(i1, i2));
                        } else if let (Some(n1), Some(n2)) = (number_value(rb_v), number_value(rc_v)) {
                            pc += 1;
                            state.stack[ra_u].val = LuaValue::Float(n1 - n2);
                        }
                    }
                    OpCode::Mul => {
                        let ra = base + i.arg_a();
                        let rb = base + i.arg_b();
                        let rc = base + i.arg_c();
                        if let Some((i1, i2)) = state.get_int_pair_at(rb, rc) {
                            pc += 1;
                            state.set_at(ra, LuaValue::Int(intop_mul(i1, i2)));
                        } else if let Some((n1, n2)) = state.get_num_pair_at(rb, rc) {
                            pc += 1;
                            state.set_at(ra, LuaValue::Float(n1 * n2));
                        }
                    }
                    OpCode::Mod => {
                        let ra = base + i.arg_a();
                        let v1 = state.get_at(base + i.arg_b());
                        let v2 = state.get_at(base + i.arg_c());
                        state.set_ci_savedpc(ci, pc);
                        state.set_top(state.ci_top(ci));
                        arith_op_checked(state, ra, &v1, &v2, &mut pc,
                            |a, b| imod(a, b), fmodf)?;
                    }
                    OpCode::Pow => {
                        let ra = base + i.arg_a();
                        let rb = base + i.arg_b();
                        let rc = base + i.arg_c();
                        if let Some((n1, n2)) = state.get_num_pair_at(rb, rc) {
                            pc += 1;
                            let r = if n2 == 2.0 { n1 * n1 } else { n1.powf(n2) };
                            state.set_at(ra, LuaValue::Float(r));
                        }
                    }
                    OpCode::Div => {
                        let ra = base + i.arg_a();
                        let rb = base + i.arg_b();
                        let rc = base + i.arg_c();
                        if let Some((n1, n2)) = state.get_num_pair_at(rb, rc) {
                            pc += 1;
                            state.set_at(ra, LuaValue::Float(n1 / n2));
                        }
                    }
                    OpCode::IDiv => {
                        let ra = base + i.arg_a();
                        let v1 = state.get_at(base + i.arg_b());
                        let v2 = state.get_at(base + i.arg_c());
                        state.set_ci_savedpc(ci, pc);
                        state.set_top(state.ci_top(ci));
                        arith_op_checked(state, ra, &v1, &v2, &mut pc,
                            |a, b| idiv(a, b), |a, b| (a / b).floor())?;
                    }
                    // ── Bitwise with register operands ─────────────────────────
                    // if (tointegerns(v1, &i1) && tointegerns(v2, &i2)) { pc++; setivalue... }
                    OpCode::BAnd => {
                        let ra = base + i.arg_a();
                        let v1 = state.get_at(base + i.arg_b());
                        let v2 = state.get_at(base + i.arg_c());
                        bitwise_op_rr(state, ra, &v1, &v2, &mut pc, intop_band);
                    }
                    OpCode::BOr => {
                        let ra = base + i.arg_a();
                        let v1 = state.get_at(base + i.arg_b());
                        let v2 = state.get_at(base + i.arg_c());
                        bitwise_op_rr(state, ra, &v1, &v2, &mut pc, intop_bor);
                    }
                    OpCode::BXOr => {
                        let ra = base + i.arg_a();
                        let v1 = state.get_at(base + i.arg_b());
                        let v2 = state.get_at(base + i.arg_c());
                        bitwise_op_rr(state, ra, &v1, &v2, &mut pc, intop_bxor);
                    }
                    OpCode::Shr => {
                        let ra = base + i.arg_a();
                        let v1 = state.get_at(base + i.arg_b());
                        let v2 = state.get_at(base + i.arg_c());
                        bitwise_shift_rr(state, ra, &v1, &v2, &mut pc, true);
                    }
                    OpCode::Shl => {
                        let ra = base + i.arg_a();
                        let v1 = state.get_at(base + i.arg_b());
                        let v2 = state.get_at(base + i.arg_c());
                        bitwise_shift_rr(state, ra, &v1, &v2, &mut pc, false);
                    }
                    // ── OP_MMBIN ─────────────────────────────────────────────
                    // Instruction pi = *(pc - 2); TMS tm = (TMS)GETARG_C(i);
                    // StkId result = RA(pi);
                    // Protect(luaT_trybinTM(L, s2v(ra), rb, result, tm));
                    OpCode::MmBin => {
                        let ra_idx = base + i.arg_a();
                        let rb_idx = base + i.arg_b();
                        let ra_v = state.get_at(ra_idx);
                        let rb_v = state.get_at(rb_idx);
                        let tm = tagmethod_from_index(i.arg_c() as usize);
                        let prev_inst = state.proto_code(&cl, pc - 2);
                        let result_idx = base + prev_inst.arg_a();
                        state.set_ci_savedpc(ci, pc);
                        state.set_top(state.ci_top(ci));
                        state.try_bin_tm(&ra_v, Some(ra_idx), &rb_v, Some(rb_idx), result_idx, tm)?;
                        trap = state.ci_trap(ci);
                    }
                    OpCode::MmBinI => {
                        let ra_idx = base + i.arg_a();
                        let ra_v = state.get_at(ra_idx);
                        let imm = i.arg_s_b() as i64;
                        let tm = tagmethod_from_index(i.arg_c() as usize);
                        let flip = i.arg_k() != 0;
                        let prev_inst = state.proto_code(&cl, pc - 2);
                        let result_idx = base + prev_inst.arg_a();
                        state.set_ci_savedpc(ci, pc);
                        state.set_top(state.ci_top(ci));
                        state.try_bin_i_tm(&ra_v, Some(ra_idx), imm, flip, result_idx, tm)?;
                        trap = state.ci_trap(ci);
                    }
                    OpCode::MmBinK => {
                        let ra_idx = base + i.arg_a();
                        let ra_v = state.get_at(ra_idx);
                        let imm = state.proto_const(&cl, i.arg_b() as usize).clone();
                        let tm = tagmethod_from_index(i.arg_c() as usize);
                        let flip = i.arg_k() != 0;
                        let prev_inst = state.proto_code(&cl, pc - 2);
                        let result_idx = base + prev_inst.arg_a();
                        state.set_ci_savedpc(ci, pc);
                        state.set_top(state.ci_top(ci));
                        state.try_bin_assoc_tm(&ra_v, Some(ra_idx), &imm, None, flip, result_idx, tm)?;
                        trap = state.ci_trap(ci);
                    }
                    // ── OP_UNM ───────────────────────────────────────────────
                    //    else if (tonumberns(rb, nb)) setfltvalue(s2v(ra), -nb)
                    //    else Protect(luaT_trybinTM(L, rb, rb, ra, TM_UNM))
                    OpCode::Unm => {
                        let ra = base + i.arg_a();
                        let rb_idx = base + i.arg_b();
                        let rb_v = state.get_at(rb_idx);
                        match &rb_v {
                            LuaValue::Int(ib) => {
                                state.set_at(ra, LuaValue::Int(intop_sub(0, *ib)));
                            }
                            LuaValue::Float(nb) => {
                                state.set_at(ra, LuaValue::Float(-nb));
                            }
                            _ => {
                                state.set_ci_savedpc(ci, pc);
                                state.set_top(state.ci_top(ci));
                                state.try_bin_tm(&rb_v, Some(rb_idx), &rb_v, Some(rb_idx), ra, TagMethod::Unm)?;
                                trap = state.ci_trap(ci);
                            }
                        }
                    }
                    // ── OP_BNOT ──────────────────────────────────────────────
                    OpCode::BNot => {
                        let ra = base + i.arg_a();
                        let rb_idx = base + i.arg_b();
                        let rb_v = state.get_at(rb_idx);
                        if let Some(ib) = to_integer_ns(&rb_v, F2Imod::Eq) {
                            state.set_at(ra, LuaValue::Int(!ib));
                        } else {
                            state.set_ci_savedpc(ci, pc);
                            state.set_top(state.ci_top(ci));
                            state.try_bin_tm(&rb_v, Some(rb_idx), &rb_v, Some(rb_idx), ra, TagMethod::Bnot)?;
                            trap = state.ci_trap(ci);
                        }
                    }
                    // ── OP_NOT ───────────────────────────────────────────────
                    OpCode::Not => {
                        let ra = base + i.arg_a();
                        let rb_v = state.get_at(base + i.arg_b());
                        let falsy = matches!(rb_v, LuaValue::Nil | LuaValue::Bool(false));
                        state.set_at(ra, LuaValue::Bool(falsy));
                    }
                    // ── OP_LEN ───────────────────────────────────────────────
                    OpCode::Len => {
                        let ra = base + i.arg_a();
                        let rb_v = state.get_at(base + i.arg_b());
                        state.set_ci_savedpc(ci, pc);
                        state.set_top(state.ci_top(ci));
                        obj_len(state, ra, rb_v)?;
                        trap = state.ci_trap(ci);
                    }
                    // ── OP_CONCAT ─────────────────────────────────────────────
                    OpCode::Concat => {
                        let ra = base + i.arg_a();
                        let n = i.arg_b() as i32;
                        state.set_top(ra + n as i32);
                        state.set_ci_savedpc(ci, pc); // ProtectNT: save pc only
                        concat(state, n)?;
                        let top = state.top_idx();
                        state.set_ci_savedpc(ci, pc);
                        state.set_top(top);
                        state.gc_cond_step();
                        trap = state.ci_trap(ci);
                    }
                    // ── OP_CLOSE ──────────────────────────────────────────────
                    OpCode::Close => {
                        let ra = base + i.arg_a();
                        state.set_ci_savedpc(ci, pc);
                        state.set_top(state.ci_top(ci));
                        crate::func::close(state, ra, lua_types::status::LuaStatus::Ok as i32, true)?;
                        trap = state.ci_trap(ci);
                    }
                    // ── OP_TBC ────────────────────────────────────────────────
                    OpCode::Tbc => {
                        let ra = base + i.arg_a();
                        state.set_ci_savedpc(ci, pc);
                        state.set_top(state.ci_top(ci));
                        state.new_tbc_upval(ra)?;
                    }
                    // ── OP_JMP ────────────────────────────────────────────────
                    OpCode::Jmp => {
                        pc = (pc as i64 + i.arg_s_j() as i64) as u32;
                        trap = state.ci_trap(ci);
                    }
                    // ── OP_EQ ─────────────────────────────────────────────────
                    OpCode::Eq => {
                        let ra_v = state.get_at(base + i.arg_a());
                        let rb_v = state.get_at(base + i.arg_b());
                        state.set_ci_savedpc(ci, pc);
                        state.set_top(state.ci_top(ci));
                        let cond = equal_obj(Some(state), &ra_v, &rb_v)? as u32;
                        trap = state.ci_trap(ci);
                        if (cond as i32) != i.arg_k() {
                            pc += 1;
                        } else {
                            let next = state.proto_code(&cl, pc);
                            pc = (pc as i64 + next.arg_s_j() as i64 + 1) as u32;
                            trap = state.ci_trap(ci);
                        }
                    }
                    // ── OP_LT ─────────────────────────────────────────────────
                    OpCode::Lt => {
                        let ra_v = state.get_at(base + i.arg_a());
                        let rb_v = state.get_at(base + i.arg_b());
                        let cond = if let (LuaValue::Int(ia), LuaValue::Int(ib)) = (&ra_v, &rb_v) {
                            *ia < *ib
                        } else if matches!((&ra_v, &rb_v),
                            (LuaValue::Int(_) | LuaValue::Float(_),
                             LuaValue::Int(_) | LuaValue::Float(_))) {
                            lt_num(&ra_v, &rb_v)
                        } else {
                            state.set_ci_savedpc(ci, pc);
                            state.set_top(state.ci_top(ci));
                            let r = less_than_others(state, &ra_v, &rb_v)?;
                            trap = state.ci_trap(ci);
                            r
                        };
                        if (cond as i32) != i.arg_k() {
                            pc += 1;
                        } else {
                            let next = state.proto_code(&cl, pc);
                            pc = (pc as i64 + next.arg_s_j() as i64 + 1) as u32;
                            trap = state.ci_trap(ci);
                        }
                    }
                    // ── OP_LE ─────────────────────────────────────────────────
                    OpCode::Le => {
                        let ra_v = state.get_at(base + i.arg_a());
                        let rb_v = state.get_at(base + i.arg_b());
                        let cond = if let (LuaValue::Int(ia), LuaValue::Int(ib)) = (&ra_v, &rb_v) {
                            *ia <= *ib
                        } else if matches!((&ra_v, &rb_v),
                            (LuaValue::Int(_) | LuaValue::Float(_),
                             LuaValue::Int(_) | LuaValue::Float(_))) {
                            le_num(&ra_v, &rb_v)
                        } else {
                            state.set_ci_savedpc(ci, pc);
                            state.set_top(state.ci_top(ci));
                            let r = less_equal_others(state, &ra_v, &rb_v)?;
                            trap = state.ci_trap(ci);
                            r
                        };
                        if (cond as i32) != i.arg_k() {
                            pc += 1;
                        } else {
                            let next = state.proto_code(&cl, pc);
                            pc = (pc as i64 + next.arg_s_j() as i64 + 1) as u32;
                            trap = state.ci_trap(ci);
                        }
                    }
                    // ── OP_EQK ────────────────────────────────────────────────
                    OpCode::EqK => {
                        let ra_v = state.get_at(base + i.arg_a());
                        let rb_v = state.proto_const(&cl, i.arg_b() as usize).clone();
                        let cond = equal_obj(None, &ra_v, &rb_v)? as u32;
                        if (cond as i32) != i.arg_k() {
                            pc += 1;
                        } else {
                            let next = state.proto_code(&cl, pc);
                            pc = (pc as i64 + next.arg_s_j() as i64 + 1) as u32;
                            trap = state.ci_trap(ci);
                        }
                    }
                    // ── OP_EQI ────────────────────────────────────────────────
                    //    if (ttisinteger) cond = ivalue == im
                    //    elif (ttisfloat) cond = numeq(fltvalue, cast_num(im))
                    //    else cond = 0
                    OpCode::EqI => {
                        let ra_v = state.get_at(base + i.arg_a());
                        let im = i.arg_s_b() as i64;
                        let cond: bool = match &ra_v {
                            LuaValue::Int(iv) => *iv == im,
                            LuaValue::Float(fv) => *fv == im as f64,
                            _ => false,
                        };
                        if (cond as i32) != i.arg_k() {
                            pc += 1;
                        } else {
                            let next = state.proto_code(&cl, pc);
                            pc = (pc as i64 + next.arg_s_j() as i64 + 1) as u32;
                            trap = state.ci_trap(ci);
                        }
                    }
                    // ── OP_LTI / OP_LEI / OP_GTI / OP_GEI ───────────────────
                    //              inv=0/0/1/1, tm=TM_LT/TM_LE/TM_LT/TM_LE)
                    OpCode::LtI => {
                        let ra = base + i.arg_a();
                        let im = i.arg_s_b() as i64;
                        let fast_cond = match &state.stack[ra.0 as usize].val {
                            LuaValue::Int(ia) => Some(*ia < im),
                            LuaValue::Float(fa) => Some(*fa < im as f64),
                            _ => None,
                        };
                        let cond = match fast_cond {
                            Some(cond) => cond,
                            None => order_imm_slow(state, ra, pc, &mut trap, ci, i, im, false, TagMethod::Lt)?,
                        };
                        finish_order_imm_jump(state, &cl, &mut pc, &mut trap, ci, i, cond);
                    }
                    OpCode::LeI => {
                        let ra = base + i.arg_a();
                        let im = i.arg_s_b() as i64;
                        let fast_cond = match &state.stack[ra.0 as usize].val {
                            LuaValue::Int(ia) => Some(*ia <= im),
                            LuaValue::Float(fa) => Some(*fa <= im as f64),
                            _ => None,
                        };
                        let cond = match fast_cond {
                            Some(cond) => cond,
                            None => order_imm_slow(state, ra, pc, &mut trap, ci, i, im, false, TagMethod::Le)?,
                        };
                        finish_order_imm_jump(state, &cl, &mut pc, &mut trap, ci, i, cond);
                    }
                    OpCode::GtI => {
                        let ra = base + i.arg_a();
                        let im = i.arg_s_b() as i64;
                        let fast_cond = match &state.stack[ra.0 as usize].val {
                            LuaValue::Int(ia) => Some(*ia > im),
                            LuaValue::Float(fa) => Some(*fa > im as f64),
                            _ => None,
                        };
                        let cond = match fast_cond {
                            Some(cond) => cond,
                            None => order_imm_slow(state, ra, pc, &mut trap, ci, i, im, true, TagMethod::Lt)?,
                        };
                        finish_order_imm_jump(state, &cl, &mut pc, &mut trap, ci, i, cond);
                    }
                    OpCode::GeI => {
                        let ra = base + i.arg_a();
                        let im = i.arg_s_b() as i64;
                        let fast_cond = match &state.stack[ra.0 as usize].val {
                            LuaValue::Int(ia) => Some(*ia >= im),
                            LuaValue::Float(fa) => Some(*fa >= im as f64),
                            _ => None,
                        };
                        let cond = match fast_cond {
                            Some(cond) => cond,
                            None => order_imm_slow(state, ra, pc, &mut trap, ci, i, im, true, TagMethod::Le)?,
                        };
                        finish_order_imm_jump(state, &cl, &mut pc, &mut trap, ci, i, cond);
                    }
                    // ── OP_TEST ────────────────────────────────────────────────
                    OpCode::Test => {
                        let ra_v = state.get_at(base + i.arg_a());
                        let cond = !matches!(ra_v, LuaValue::Nil | LuaValue::Bool(false));
                        if (cond as i32) != i.arg_k() {
                            pc += 1;
                        } else {
                            let next = state.proto_code(&cl, pc);
                            pc = (pc as i64 + next.arg_s_j() as i64 + 1) as u32;
                            trap = state.ci_trap(ci);
                        }
                    }
                    // ── OP_TESTSET ─────────────────────────────────────────────
                    //    else { setobj2s(L, ra, rb); donextjump(ci); }
                    OpCode::TestSet => {
                        let ra = base + i.arg_a();
                        let rb_v = state.get_at(base + i.arg_b());
                        let falsy = matches!(rb_v, LuaValue::Nil | LuaValue::Bool(false));
                        if (falsy as i32) == i.arg_k() {
                            pc += 1;
                        } else {
                            state.set_at(ra, rb_v);
                            let next = state.proto_code(&cl, pc);
                            pc = (pc as i64 + next.arg_s_j() as i64 + 1) as u32;
                            trap = state.ci_trap(ci);
                        }
                    }
                    // ── OP_CALL ────────────────────────────────────────────────
                    //      updatetrap(ci);
                    //    else { ci = newci; goto startfunc; }
                    OpCode::Call => {
                        let ra = base + i.arg_a();
                        let b = i.arg_b();
                        let nresults = i.arg_c() as i32 - 1;
                        if b != 0 {
                            state.set_top(ra + b);
                        }
                        state.set_ci_savedpc(ci, pc); // savepc
                        let had_hook = state.hookmask != 0;
                        match state.precall(ra, nresults)? {
                            None => {
                                // C functions such as debug.sethook can change
                                // hook state during the call, so refresh the VM
                                // trap when hooks were or became relevant.
                                if had_hook || state.hookmask != 0 {
                                    trap = state.ci_trap(ci); // updatetrap
                                }
                            }
                            Some(new_ci) => {
                                // Lua function — goto startfunc
                                ci = new_ci;
                                continue 'startfunc;
                            }
                        }
                    }
                    // ── OP_TAILCALL ────────────────────────────────────────────
                    //      goto startfunc;
                    //    else { ci->func.p -= delta; luaD_poscall(L, ci, n);
                    //            updatetrap; goto ret; }
                    OpCode::TailCall => {
                        let ra = base + i.arg_a();
                        let b = i.arg_b();
                        let nparams1 = i.arg_c();
                        let delta = if nparams1 != 0 {
                            state.ci_nextraargs(ci) + nparams1 as i32
                        } else {
                            0
                        };
                        let top_b: i32 = if b != 0 {
                            state.set_top(ra + b);
                            b
                        } else {
                            state.top_idx() - ra
                        };
                        state.set_ci_savedpc(ci, pc);
                        if i.test_k() {
                            state.close_upvals_from_base(ci)?;
                        }
                        let n = state.pretailcall(ci, ra, top_b, delta)?;
                        if n < 0 {
                            // Lua function — goto startfunc
                            continue 'startfunc;
                        } else {
                            // C function — ci->func.p -= delta; luaD_poscall; goto ret
                            state.ci_adjust_func(ci, delta);
                            state.poscall(ci, n as u32)?;
                            if state.hookmask != 0 {
                                trap = state.ci_trap(ci);
                            }
                            break 'dispatch; // goto ret
                        }
                    }
                    // ── OP_RETURN ──────────────────────────────────────────────
                    //    savepc; if TESTARG_k: close upvals;
                    //    if nparams1: ci->func -= nextraargs+nparams1;
                    //    L->top.p = ra+n; luaD_poscall; goto ret
                    OpCode::Return => {
                        let ra = base + i.arg_a();
                        let n_raw = i.arg_b() as i32 - 1;
                        let nparams1 = i.arg_c();
                        let n: u32 = if n_raw < 0 {
                            (state.top_idx() - ra) as u32
                        } else {
                            n_raw as u32
                        };
                        state.set_ci_savedpc(ci, pc);
                        if i.test_k() {
                            state.ci_nres_set(ci, n as i32);
                            let ci_top = state.ci_top(ci);
                            if state.top_idx().0 < ci_top.0 {
                                state.set_top(ci_top);
                            }
                            crate::func::close(state, base, crate::func::CLOSE_K_TOP, true)?;
                            if state.hookmask != 0 {
                                trap = state.ci_trap(ci);
                            }
                            base = state.ci_base(ci); // updatestack
                        }
                        if nparams1 != 0 {
                            let nextraargs = state.ci_nextraargs(ci) as u32;
                            state.ci_adjust_func(ci, nextraargs as i32 + nparams1 as i32);
                        }
                        state.set_top(ra + n as i32);
                        state.poscall(ci, n)?;
                        if state.hookmask != 0 {
                            trap = state.ci_trap(ci);
                        }
                        break 'dispatch; // goto ret
                    }
                    // ── OP_RETURN0 ─────────────────────────────────────────────
                    //    else { L->ci = ci->previous; L->top = base-1;
                    //           for (nres = ci->nresults; nres > 0; nres--)
                    //             setnilvalue(L->top++) }
                    //    goto ret;
                    OpCode::Return0 => {
                        if state.hookmask == 0 {
                            let ci_slot = ci.as_usize();
                            let nres = state.call_info[ci_slot].nresults as i32;
                            state.ci = state.call_info[ci_slot]
                                .previous
                                .expect("RETURN0: returning frame has no previous CallInfo");
                            state.top = base - 1;
                            for _ in 0..nres.max(0) {
                                state.push(LuaValue::Nil);
                            }
                        } else {
                            return0_hook(state, ci, base, i, pc, &mut trap)?;
                        }
                        break 'dispatch; // goto ret
                    }
                    // ── OP_RETURN1 ─────────────────────────────────────────────
                    //    else { nres = ci->nresults; ci = ci->previous; ...handle results... }
                    //    goto ret;
                    OpCode::Return1 => {
                        if state.hookmask == 0 {
                            let ci_slot = ci.as_usize();
                            let nres = state.call_info[ci_slot].nresults as i32;
                            state.ci = state.call_info[ci_slot]
                                .previous
                                .expect("RETURN1: returning frame has no previous CallInfo");
                            if nres == 0 {
                                state.top = base - 1;
                            } else {
                                let ra = base + i.arg_a();
                                state.stack[(base - 1).0 as usize].val =
                                    state.stack[ra.0 as usize].val; // at least this result
                                state.top = base;
                                for _ in 1..nres.max(0) {
                                    state.push(LuaValue::Nil);
                                }
                            }
                        } else {
                            return1_hook(state, ci, base, i, pc, &mut trap)?;
                        }
                        break 'dispatch; // goto ret
                    }
                    // ── OP_FORLOOP ─────────────────────────────────────────────
                    //    else if (floatforloop(ra)) pc -= GETARG_Bx(i)
                    //    updatetrap(ci);
                    OpCode::ForLoop => {
                        let ra = base + i.arg_a();
                        let ra_u = ra.0 as usize;
                        if let LuaValue::Int(step) = state.stack[ra_u + 2].val {
                            let count = match state.stack[ra_u + 1].val {
                                LuaValue::Int(c) => c as u64,
                                _ => 0,
                            };
                            if count > 0 {
                                let idx = match state.stack[ra_u].val {
                                    LuaValue::Int(x) => x,
                                    _ => 0,
                                };
                                state.stack[ra_u + 1].val = LuaValue::Int((count - 1) as i64);
                                let new_idx = intop_add(idx, step);
                                state.stack[ra_u].val = LuaValue::Int(new_idx);
                                state.stack[ra_u + 3].val = LuaValue::Int(new_idx);
                                pc = (pc as i64 - i.arg_bx() as i64) as u32;
                            }
                        } else if float_for_loop(state, ra) {
                            pc = (pc as i64 - i.arg_bx() as i64) as u32;
                        }
                        trap = state.ci_trap(ci);
                    }
                    // ── OP_FORPREP ─────────────────────────────────────────────
                    OpCode::ForPrep => {
                        let ra = base + i.arg_a();
                        state.set_ci_savedpc(ci, pc);
                        state.set_top(state.ci_top(ci));
                        if forprep(state, ra)? {
                            pc = (pc as i64 + i.arg_bx() as i64 + 1) as u32;
                        }
                    }
                    // ── OP_TFORPREP ────────────────────────────────────────────
                    //    pc += GETARG_Bx(i); i = *pc++; assert(OP_TFORCALL && ra==RA(i));
                    //    goto l_tforcall;
                    OpCode::TForPrep => {
                        let ra = base + i.arg_a();
                        state.set_ci_savedpc(ci, pc);
                        state.set_top(state.ci_top(ci));
                        state.new_tbc_upval(ra + 3)?;
                        pc = (pc as i64 + i.arg_bx() as i64) as u32;
                        let tfc_i = state.proto_code(&cl, pc);
                        pc += 1;
                        debug_assert!(tfc_i.opcode() == OpCode::TForCall);
                        // inline l_tforcall:
                        let tfc_ra = base + tfc_i.arg_a();
                        for k in 0..3u32 {
                            let v = state.get_at(tfc_ra + k as i32);
                            state.set_at(tfc_ra + 4 + k as i32, v);
                        }
                        state.set_top(tfc_ra + 4 + 3);
                        state.set_ci_savedpc(ci, pc);
                        state.call_at(tfc_ra + 4, tfc_i.arg_c() as i32)?;
                        trap = state.ci_trap(ci);
                        base = state.ci_base(ci); // updatestack
                        let tfl_i = state.proto_code(&cl, pc);
                        pc += 1;
                        debug_assert!(tfl_i.opcode() == OpCode::TForLoop);
                        let tfl_ra = base + tfl_i.arg_a();
                        // inline l_tforloop:
                        if !matches!(state.get_at(tfl_ra + 4), LuaValue::Nil) {
                            let v = state.get_at(tfl_ra + 4);
                            state.set_at(tfl_ra + 2, v);
                            pc = (pc as i64 - tfl_i.arg_bx() as i64) as u32;
                        }
                    }
                    // ── OP_TFORCALL ────────────────────────────────────────────
                    OpCode::TForCall => {
                        let ra = base + i.arg_a();
                        for k in 0..3u32 {
                            let v = state.get_at(ra + k as i32);
                            state.set_at(ra + 4 + k as i32, v);
                        }
                        state.set_top(ra + 4 + 3);
                        state.set_ci_savedpc(ci, pc);
                        state.call_at(ra + 4, i.arg_c() as i32)?;
                        trap = state.ci_trap(ci);
                        base = state.ci_base(ci); // updatestack
                        let tfl_i = state.proto_code(&cl, pc);
                        pc += 1;
                        debug_assert!(tfl_i.opcode() == OpCode::TForLoop);
                        let tfl_ra = base + tfl_i.arg_a();
                        if !matches!(state.get_at(tfl_ra + 4), LuaValue::Nil) {
                            let v = state.get_at(tfl_ra + 4);
                            state.set_at(tfl_ra + 2, v);
                            pc = (pc as i64 - tfl_i.arg_bx() as i64) as u32;
                        }
                    }
                    // ── OP_TFORLOOP ────────────────────────────────────────────
                    OpCode::TForLoop => {
                        let ra = base + i.arg_a();
                        if !matches!(state.get_at(ra + 4), LuaValue::Nil) {
                            let v = state.get_at(ra + 4);
                            state.set_at(ra + 2, v);
                            pc = (pc as i64 - i.arg_bx() as i64) as u32;
                        }
                    }
                    // ── OP_SETLIST ─────────────────────────────────────────────
                    //    if TESTARG_k: last += Ax * (MAXARG_C+1); pc++;
                    //    for (; n > 0; n--) h->array[last-1] = val; luaC_barrierback
                    OpCode::SetList => {
                        let ra = base + i.arg_a();
                        let n_raw = i.arg_b();
                        let mut last = i.arg_c();
                        let t_val = state.get_at(ra);
                        let n: i32 = if n_raw == 0 {
                            state.top_idx() - ra - 1
                        } else {
                            state.set_top(state.ci_top(ci));
                            n_raw
                        };
                        last += n;
                        if i.test_k() {
                            let extra = state.proto_code(&cl, pc);
                            pc += 1;
                            const MAXARG_C: i32 = (1 << 8) - 1;
                            last += extra.arg_ax() * (MAXARG_C + 1);
                        }
                        state.table_ensure_array(&t_val, last as usize)?;
                        for k in (1..=n).rev() {
                            let val = state.get_at(ra + k as i32);
                            state.table_array_set(&t_val, (last - 1) as usize, val.clone())?;
                            last -= 1;
                            state.gc_barrier_back(&t_val, &val);
                        }
                    }
                    // ── OP_CLOSURE ─────────────────────────────────────────────
                    //    halfProtect(pushclosure(L, p, cl->upvals, base, ra));
                    //    checkGC(L, ra+1);
                    OpCode::Closure => {
                        let ra = base + i.arg_a();
                        let proto_idx = i.arg_bx() as usize;
                        state.set_ci_savedpc(ci, pc);
                        state.set_top(state.ci_top(ci));
                        push_closure(state, proto_idx, ci, base, ra)?;
                        // checkGC
                        state.set_ci_savedpc(ci, pc);
                        state.set_top(ra + 1);
                        state.gc_cond_step();
                        trap = state.ci_trap(ci);
                    }
                    // ── OP_VARARG ──────────────────────────────────────────────
                    OpCode::VarArg => {
                        let ra = base + i.arg_a();
                        let n = i.arg_c() as i32 - 1;
                        state.set_ci_savedpc(ci, pc);
                        state.set_top(state.ci_top(ci));
                        state.get_varargs(ci, ra, n)?;
                        trap = state.ci_trap(ci);
                    }
                    // ── OP_VARARGPREP ──────────────────────────────────────────
                    //    if (trap) luaD_hookcall(L, ci); L->oldpc = 1;
                    //    updatebase(ci);
                    OpCode::VarArgPrep => {
                        let nparams = i.arg_a();
                        state.set_ci_savedpc(ci, pc);
                        state.adjust_varargs(ci, nparams, &cl)?;
                        trap = state.ci_trap(ci);
                        if trap {
                            state.hook_call(ci)?;
                            state.set_oldpc(1);
                        }
                        base = state.ci_base(ci);
                    }
                    // ── OP_EXTRAARG ────────────────────────────────────────────
                    OpCode::ExtraArg => {
                        debug_assert!(false, "OP_EXTRAARG executed directly");
                    }
                } // end match opcode
            } // end 'dispatch loop

            // ── ret: label ──────────────────────────────────────────────────
            if state.ci_is_fresh(ci) {
                return Ok(());
            } else {
                ci = state.ci_previous(ci).expect("ci_previous: not fresh frame must have previous");
                continue 'returning;
            }
        } // end 'returning loop
    } // end 'startfunc loop
}

// ─── Local opcode dispatch helpers ───────────────────────────────────────────

#[inline(always)]
fn number_value(v: LuaValue) -> Option<f64> {
    match v {
        LuaValue::Float(f) => Some(f),
        LuaValue::Int(i) => Some(i as f64),
        _ => None,
    }
}

/// Increments `pc` on success (the `pc++` in the C macros).
#[allow(dead_code)]
#[inline]
fn arith_op_aux_rr(
    state: &mut LuaState,
    ra: StackIdx,
    v1: &LuaValue,
    v2: &LuaValue,
    pc: &mut u32,
    iop: fn(i64, i64) -> i64,
    fop: fn(f64, f64) -> f64,
) {
    if let (LuaValue::Int(i1), LuaValue::Int(i2)) = (v1, v2) {
        *pc += 1;
        state.set_at(ra, LuaValue::Int(iop(*i1, *i2)));
    } else {
        arith_float_aux(state, ra, v1, v2, pc, fop);
    }
}

#[allow(dead_code)]
#[inline]
fn arith_float_aux(
    state: &mut LuaState,
    ra: StackIdx,
    v1: &LuaValue,
    v2: &LuaValue,
    pc: &mut u32,
    fop: fn(f64, f64) -> f64,
) {
    let n1 = match v1 {
        LuaValue::Float(f) => Some(*f),
        LuaValue::Int(i) => Some(*i as f64),
        _ => None,
    };
    let n2 = match v2 {
        LuaValue::Float(f) => Some(*f),
        LuaValue::Int(i) => Some(*i as f64),
        _ => None,
    };
    if let (Some(n1), Some(n2)) = (n1, n2) {
        *pc += 1;
        state.set_at(ra, LuaValue::Float(fop(n1, n2)));
    }
}

#[allow(dead_code)]
#[inline]
fn arith_op_checked(
    state: &mut LuaState,
    ra: StackIdx,
    v1: &LuaValue,
    v2: &LuaValue,
    pc: &mut u32,
    iop: fn(i64, i64) -> Result<i64, LuaError>,
    fop: fn(f64, f64) -> f64,
) -> Result<(), LuaError> {
    if let (LuaValue::Int(i1), LuaValue::Int(i2)) = (v1, v2) {
        *pc += 1;
        let result = iop(*i1, *i2).map_err(|e| match e {
            LuaError::Runtime(LuaValue::Str(s)) => {
                crate::debug::prefixed_runtime_pub(state, s.as_bytes().to_vec())
            }
            other => other,
        })?;
        state.set_at(ra, LuaValue::Int(result));
    } else {
        arith_float_aux(state, ra, v1, v2, pc, fop);
    }
    Ok(())
}

#[allow(dead_code)]
#[inline]
fn bitwise_op_k(
    state: &mut LuaState,
    ra: StackIdx,
    v1: &LuaValue,
    v2: &LuaValue, // must be integer (K constant)
    pc: &mut u32,
    op: fn(i64, i64) -> i64,
) {
    let i2 = match v2 {
        LuaValue::Int(i) => *i,
        _ => return,
    };
    if let Some(i1) = to_integer_ns(v1, F2Imod::Eq) {
        *pc += 1;
        state.set_at(ra, LuaValue::Int(op(i1, i2)));
    }
}

#[allow(dead_code)]
#[inline]
fn bitwise_op_rr(
    state: &mut LuaState,
    ra: StackIdx,
    v1: &LuaValue,
    v2: &LuaValue,
    pc: &mut u32,
    op: fn(i64, i64) -> i64,
) {
    if let (Some(i1), Some(i2)) = (
        to_integer_ns(v1, F2Imod::Eq),
        to_integer_ns(v2, F2Imod::Eq),
    ) {
        *pc += 1;
        state.set_at(ra, LuaValue::Int(op(i1, i2)));
    }
}

/// `right = true` negates `y` for right-shift semantics.
#[allow(dead_code)]
#[inline]
fn bitwise_shift_rr(
    state: &mut LuaState,
    ra: StackIdx,
    v1: &LuaValue,
    v2: &LuaValue,
    pc: &mut u32,
    right: bool,
) {
    if let (Some(i1), Some(i2)) = (
        to_integer_ns(v1, F2Imod::Eq),
        to_integer_ns(v2, F2Imod::Eq),
    ) {
        let y = if right { intop_sub(0, i2) } else { i2 };
        *pc += 1;
        state.set_at(ra, LuaValue::Int(shiftl(i1, y)));
    }
}

/// Cold half of C's `op_orderI` macro: only reached when the operand is not a
/// plain integer/float and a metamethod lookup may be needed.
#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn order_imm_slow(
    state: &mut LuaState,
    ra: StackIdx,
    pc: u32,
    trap: &mut bool,
    ci: CallInfoIdx,
    i: Instruction,
    im: i64,
    inv: bool,
    tm: TagMethod,
) -> Result<bool, LuaError> {
    let ra_v = state.get_at(ra);
    let isf = i.arg_c() != 0;
    state.set_ci_savedpc(ci, pc);
    state.set_top(state.ci_top(ci));
    let r = state.call_order_i_tm(&ra_v, im, inv, isf, tm)?;
    *trap = state.ci_trap(ci);
    Ok(r)
}

#[inline(always)]
fn finish_order_imm_jump(
    state: &mut LuaState,
    cl: &lua_types::GcRef<lua_types::LuaLClosure>,
    pc: &mut u32,
    trap: &mut bool,
    ci: CallInfoIdx,
    i: Instruction,
    cond: bool,
) {
    if (cond as i32) != i.arg_k() {
        *pc += 1;
    } else {
        let next = state.proto_code(&cl, *pc);
        *pc = (*pc as i64 + next.arg_s_j() as i64 + 1) as u32;
        *trap = state.ci_trap(ci);
    }
}

#[cold]
#[inline(never)]
fn return0_hook(
    state: &mut LuaState,
    ci: CallInfoIdx,
    base: StackIdx,
    i: Instruction,
    pc: u32,
    trap: &mut bool,
) -> Result<(), LuaError> {
    let ra = base + i.arg_a();
    state.set_top(ra);
    state.set_ci_savedpc(ci, pc);
    state.poscall(ci, 0)?;
    *trap = true;
    Ok(())
}

#[cold]
#[inline(never)]
fn return1_hook(
    state: &mut LuaState,
    ci: CallInfoIdx,
    base: StackIdx,
    i: Instruction,
    pc: u32,
    trap: &mut bool,
) -> Result<(), LuaError> {
    let ra = base + i.arg_a();
    state.set_top(ra + 1);
    state.set_ci_savedpc(ci, pc);
    state.poscall(ci, 1)?;
    *trap = true;
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        src/lvm.c  (1899 lines, 32 functions)
//   target_crate:  lua-vm
//   confidence:    medium
//   todos:         6
//   port_notes:    4
//   unsafe_blocks: 0   (must be 0 outside explicit unsafe-budget crates)
//   notes:         All opcode handlers and helpers translated; LuaState methods
//                  referenced (fast_get, precall, poscall, etc.) are stubs that
//                  Phase B will land.  The execute() goto flow is modelled with
//                  labelled Rust loops ('startfunc/'returning/'dispatch).
//                  str_to_number is a stub pending luaO_str2num port (TODO #1).
//                  strcoll replaced with byte-lexicographic order (TODO #2).
//                  order_imm_op uses LuaValue as a stand-in for GcRef<LuaClosure>
//                  (TODO #3).  ClosureRef type alias not yet defined (TODO #4-6).
// ──────────────────────────────────────────────────────────────────────────
