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

// C: #include "lprefix.h"
// C: #include "lua.h"
// C: #include "ldebug.h" "ldo.h" "lfunc.h" "lgc.h" "lobject.h"
// C: #include "lopcodes.h" "lstate.h" "lstring.h" "ltable.h" "ltm.h" "lvm.h"

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(non_camel_case_types)]
pub enum OpCode {
    Move, LoadI, LoadF, LoadK, LoadKX, LoadKx, LoadFalse, LFalseSkip, LoadTrue, LoadNil,
    GetUpVal, GetUpval, SetUpVal, GetTabUp, GetTable, GetI, GetField, SetTabUp, SetTable, SetI, SetField,
    NewTable, Self_,
    AddI, AddK, SubK, MulK, ModK, PowK, DivK, IDivK, BAndK, BOrK, BXOrK,
    Add, Sub, Mul, Mod, Pow, Div, IDiv, BAnd, BOr, BXOr, Shl, Shr, ShlI, ShrI,
    MmBin, MmBinI, MmBinK,
    Unm, BNot, Not, Len, Concat,
    Close, Tbc, Jmp,
    Eq, Lt, Le, EqK, EqI, LtI, LeI, GtI, GeI, Test, TestSet,
    Call, TailCall, Return,
    ForLoop, ForPrep, TForPrep, TForCall, TForLoop,
    SetList, Closure, VarArg, VarArgPrep, ExtraArg,
    Return0, Return1,
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
    fn opcode(&self) -> OpCode {
        match self.raw() & 0x7F {
            0  => OpCode::Move,
            1  => OpCode::LoadI,
            2  => OpCode::LoadF,
            3  => OpCode::LoadK,
            4  => OpCode::LoadKX,
            5  => OpCode::LoadFalse,
            6  => OpCode::LFalseSkip,
            7  => OpCode::LoadTrue,
            8  => OpCode::LoadNil,
            9  => OpCode::GetUpVal,
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
            n  => unreachable!("invalid opcode 0x{:02x} in instruction word 0x{:08x}", n, self.raw()),
        }
    }
    fn arg_a(&self) -> i32 { ((self.raw() >> 7) & 0xFF) as i32 }
    fn arg_b(&self) -> i32 { ((self.raw() >> 16) & 0xFF) as i32 }
    fn arg_c(&self) -> i32 { ((self.raw() >> 24) & 0xFF) as i32 }
    fn arg_k(&self) -> i32 { ((self.raw() >> 15) & 0x1) as i32 }
    fn arg_ax(&self) -> i32 { (self.raw() >> 7) as i32 }
    fn arg_bx(&self) -> i32 { (self.raw() >> 15) as i32 }
    fn arg_s_b(&self) -> i32 { self.arg_b() - 0x7F }
    fn arg_s_c(&self) -> i32 { self.arg_c() - 0x7F }
    fn arg_s_j(&self) -> i32 { self.arg_ax() - 0xFFFFFF }
    fn arg_s_bx(&self) -> i32 { self.arg_bx() - 0xFFFF }
    fn test_k(&self) -> bool { (self.raw() & (1 << 15)) != 0 }
    fn test_a_mode(&self) -> bool { todo!("phase-b: test_a_mode") }
    fn is_mm_mode(&self) -> bool { todo!("phase-b: is_mm_mode") }
    fn is_vararg_prep(&self) -> bool { todo!("phase-b: is_vararg_prep") }
    fn is_in_top(&self) -> bool { todo!("phase-b: is_in_top") }
}

// ─── Constants ───────────────────────────────────────────────────────────────

/// C: #define MAXTAGLOOP 2000
/// Limit for tag-method chains to avoid infinite loops.
const MAX_TAG_LOOP: i32 = 2000;

/// C: NBITS — number of bits in lua_Integer (i64).
const NBITS: u32 = 64;

// ─── F2Imod — float-to-integer rounding mode ────────────────────────────────

/// C: `typedef enum { F2Ieq, F2Ifloor, F2Iceil } F2Imod;`
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

/// C: `intop(+, a, b)` — wrapping add on i64 operands.
#[inline]
fn intop_add(a: i64, b: i64) -> i64 {
    (a as u64).wrapping_add(b as u64) as i64
}

/// C: `intop(-, a, b)` — wrapping sub.
#[inline]
fn intop_sub(a: i64, b: i64) -> i64 {
    (a as u64).wrapping_sub(b as u64) as i64
}

/// C: `intop(*, a, b)` — wrapping mul.
#[inline]
fn intop_mul(a: i64, b: i64) -> i64 {
    (a as u64).wrapping_mul(b as u64) as i64
}

/// C: `intop(>>, x, -y)` or `intop(<<, x, y)` — wrapping shift.
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

/// C: `intop(&, a, b)`, `intop(|, a, b)`, `intop(^, a, b)`
#[inline]
fn intop_band(a: i64, b: i64) -> i64 { ((a as u64) & (b as u64)) as i64 }
#[inline]
fn intop_bor(a: i64, b: i64) -> i64  { ((a as u64) | (b as u64)) as i64 }
#[inline]
fn intop_bxor(a: i64, b: i64) -> i64 { ((a as u64) ^ (b as u64)) as i64 }

// ─── l_intfitsf ─────────────────────────────────────────────────────────────

/// C: `l_intfitsf(i)` — does integer `i` fit exactly in an f64 mantissa?
/// f64 has 53 bits of mantissa (including implicit leading 1).
/// All i64 values with |i| <= 2^53 are exactly representable.
#[inline]
fn int_fits_float(i: i64) -> bool {
    // C: MAXINTFITSF = 1u64 << NBM (NBM = f64::MANTISSA_DIGITS = 53)
    const MAXINTFITSF: u64 = 1u64 << f64::MANTISSA_DIGITS;
    // C: (MAXINTFITSF + l_castS2U(i)) <= (2 * MAXINTFITSF)
    (MAXINTFITSF.wrapping_add(i as u64)) <= 2 * MAXINTFITSF
}

// ─── Private helper: string-to-number coercion ──────────────────────────────

/// C: `static int l_strton(const TValue *obj, TValue *result)`
/// Attempt to convert a string value to a number in-place.
/// Returns `Some(LuaValue)` with the numeric result, or `None` if the
/// value is not a string or cannot be parsed as a numeral.
fn str_to_number(obj: &LuaValue) -> Option<LuaValue> {
    // C: if (!cvt2num(obj)) return 0;
    // cvt2num(o) = matches!(o, LuaValue::Str(_))
    if !matches!(obj, LuaValue::Str(_)) {
        return None;
    }
    let s = match obj {
        LuaValue::Str(ts) => ts.as_bytes().to_vec(),
        _ => return None,
    };
    // C: return (luaO_str2num(getstr(st), result) == tsslen(st) + 1)
    // TODO(port): call state.str2num(&s) — needs access to LuaState for
    // luaO_str2num; placeholder returns None until string->num parsing lands.
    // The full form is: parse the byte slice as a Lua numeral, return Some if
    // the entire slice was consumed.
    let _ = s;
    None
}

// ─── Number coercion (public API matching lvm.h exports) ────────────────────

/// C: `int luaV_tonumber_(const TValue *obj, lua_Number *n)`
/// Convert `obj` to f64, with string coercion.  Returns `Some(f64)` on
/// success.  The fast path (already float) is handled by the caller's
/// `tonumber` macro (inlined at call sites).
pub(crate) fn tonumber_(obj: &LuaValue) -> Option<f64> {
    // C: if (ttisinteger(obj)) { *n = cast_num(ivalue(obj)); return 1; }
    if let LuaValue::Int(i) = obj {
        return Some(*i as f64);
    }
    // C: else if (l_strton(obj, &v)) { *n = nvalue(&v); return 1; }
    if let Some(v) = str_to_number(obj) {
        return match v {
            LuaValue::Float(f) => Some(f),
            LuaValue::Int(i) => Some(i as f64),
            _ => None,
        };
    }
    None
}

/// C: `int luaV_flttointeger(lua_Number n, lua_Integer *p, F2Imod mode)`
/// Convert float `n` to an integer according to `mode`.
/// Returns `Some(i64)` on success.
pub(crate) fn flt_to_integer(n: f64, mode: F2Imod) -> Option<i64> {
    // C: lua_Number f = l_floor(n);
    let f = n.floor();
    // C: if (n != f) { if (mode == F2Ieq) return 0; else if (mode == F2Iceil) f += 1; }
    if n != f {
        match mode {
            F2Imod::Eq => return None,
            F2Imod::Ceil => {
                // f = floor(n) + 1 = ceil(n) since n is not integral
                let f = f + 1.0;
                // C: return lua_numbertointeger(f, p)
                // lua_numbertointeger checks i64::MIN <= f <= i64::MAX
                if f >= i64::MIN as f64 && f < (i64::MAX as f64 + 1.0) {
                    return Some(f as i64);
                }
                return None;
            }
            F2Imod::Floor => { /* f is already floor(n) */ }
        }
    }
    // C: return lua_numbertointeger(f, p)
    if f >= i64::MIN as f64 && f < (i64::MAX as f64 + 1.0) {
        Some(f as i64)
    } else {
        None
    }
}

/// C: `int luaV_tointegerns(const TValue *obj, lua_Integer *p, F2Imod mode)`
/// Convert a value to integer without string coercion.
pub(crate) fn to_integer_ns(obj: &LuaValue, mode: F2Imod) -> Option<i64> {
    // C: if (ttisfloat(obj)) return luaV_flttointeger(fltvalue(obj), p, mode);
    if let LuaValue::Float(f) = obj {
        return flt_to_integer(*f, mode);
    }
    // C: else if (ttisinteger(obj)) { *p = ivalue(obj); return 1; }
    if let LuaValue::Int(i) = obj {
        return Some(*i);
    }
    None
}

/// C: `int luaV_tointeger(const TValue *obj, lua_Integer *p, F2Imod mode)`
/// Convert a value to integer, with string coercion.
pub(crate) fn to_integer(obj: &LuaValue, mode: F2Imod) -> Option<i64> {
    // C: TValue v; if (l_strton(obj, &v)) obj = &v;
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

/// C: `static int forlimit(lua_State *L, lua_Integer init, const TValue *lim,
///                          lua_Integer *p, lua_Integer step)`
/// Compute the integer loop limit.  Returns `Ok(true)` to skip the loop,
/// `Ok(false)` with `*p` set to the limit, or `Err` if the limit is not a
/// number at all.
fn forlimit(
    init: i64,
    lim: &LuaValue,
    step: i64,
) -> Result<(bool, i64), LuaError> {
    // C: if (!luaV_tointeger(lim, p, (step < 0 ? F2Iceil : F2Ifloor)))
    let round = if step < 0 { F2Imod::Ceil } else { F2Imod::Floor };
    if let Some(p) = to_integer(lim, round) {
        // C: return (step > 0 ? init > *p : init < *p);
        let skip = if step > 0 { init > p } else { init < p };
        return Ok((skip, p));
    }
    // C: not coercible to int; try float
    // C: if (!tonumber(lim, &flim)) luaG_forerror(L, lim, "limit");
    let flim = match tonumber_(lim) {
        Some(f) => f,
        None => return Err(LuaError::for_error(lim, "limit")),
    };
    // C: float out of integer bounds — clip to LUA_MAXINTEGER or MININTEGER
    if 0.0_f64 < flim {
        // positive → too large
        if step < 0 {
            // C: if (step < 0) return 1;  /* initial value must be less than it */
            return Ok((true, 0));
        }
        Ok((false, i64::MAX))
    } else {
        // negative → less than min integer
        if step > 0 {
            // C: if (step > 0) return 1;
            return Ok((true, 0));
        }
        Ok((false, i64::MIN))
    }
}

/// C: `static int forprep(lua_State *L, StkId ra)`
/// Prepare a numeric `for` loop (OP_FORPREP).
/// Stack layout at `ra`:
///   ra+0: init, ra+1: limit, ra+2: step, ra+3: control variable (written here)
/// Returns `Ok(true)` to skip the loop body entirely.
pub(crate) fn forprep(state: &mut LuaState, ra: StackIdx) -> Result<bool, LuaError> {
    // C: TValue *pinit = s2v(ra); *plimit = s2v(ra+1); *pstep = s2v(ra+2);
    let pinit  = state.get_at(ra).clone();
    let plimit = state.get_at(ra + 1).clone();
    let pstep  = state.get_at(ra + 2).clone();

    if let (LuaValue::Int(init), LuaValue::Int(step)) = (&pinit, &pstep) {
        // C: integer loop
        let init = *init;
        let step = *step;
        if step == 0 {
            return Err(LuaError::runtime(format_args!("'for' step is zero")));
        }
        // C: setivalue(s2v(ra+3), init)
        state.set_at(ra + 3, LuaValue::Int(init));

        let (skip, limit) = forlimit(init, &plimit, step)?;
        if skip {
            return Ok(true);
        }
        // C: compute loop counter (iteration count) from limit, init, step
        let count: u64 = if step > 0 {
            // C: count = l_castS2U(limit) - l_castS2U(init);
            let c = (limit as u64).wrapping_sub(init as u64);
            // C: if (step != 1) count /= l_castS2U(step);
            if step != 1 { c / (step as u64) } else { c }
        } else {
            // C: count = l_castS2U(init) - l_castS2U(limit);
            let c = (init as u64).wrapping_sub(limit as u64);
            // C: count /= l_castS2U(-(step + 1)) + 1u
            c / (((-(step + 1)) as u64).wrapping_add(1))
        };
        // C: setivalue(plimit, l_castU2S(count))  — store counter in limit slot
        state.set_at(ra + 1, LuaValue::Int(count as i64));
        Ok(false)
    } else {
        // C: float loop — coerce all three values to floats
        let limit_f = match tonumber_(&plimit) {
            Some(f) => f,
            None => return Err(LuaError::for_error(&plimit, "limit")),
        };
        let step_f = match tonumber_(&pstep) {
            Some(f) => f,
            None => return Err(LuaError::for_error(&pstep, "step")),
        };
        let init_f = match tonumber_(&pinit) {
            Some(f) => f,
            None => return Err(LuaError::for_error(&pinit, "initial value")),
        };
        if step_f == 0.0 {
            return Err(LuaError::runtime(format_args!("'for' step is zero")));
        }
        // C: if (step>0 ? limit<init : init<limit) return 1
        let skip = if step_f > 0.0 { limit_f < init_f } else { init_f < limit_f };
        if skip {
            return Ok(true);
        }
        // C: setfltvalue(plimit, limit); setfltvalue(pstep, step);
        //    setfltvalue(s2v(ra), init); setfltvalue(s2v(ra+3), init);
        state.set_at(ra + 1, LuaValue::Float(limit_f));
        state.set_at(ra + 2, LuaValue::Float(step_f));
        state.set_at(ra,     LuaValue::Float(init_f));
        state.set_at(ra + 3, LuaValue::Float(init_f));
        Ok(false)
    }
}

/// C: `static int floatforloop(StkId ra)` — float for-loop step.
/// Increments the float loop index and returns `true` if the loop continues.
fn float_for_loop(state: &mut LuaState, ra: StackIdx) -> bool {
    // C: step = fltvalue(s2v(ra+2)); limit = fltvalue(s2v(ra+1));
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
    // C: idx = luai_numadd(L, idx, step);
    let idx = idx + step;
    // C: if (step>0 ? idx<=limit : limit<=idx)
    if if step > 0.0 { idx <= limit } else { limit <= idx } {
        // C: chgfltvalue(s2v(ra), idx); setfltvalue(s2v(ra+3), idx);
        state.set_at(ra,     LuaValue::Float(idx));
        state.set_at(ra + 3, LuaValue::Float(idx));
        true
    } else {
        false
    }
}

// ─── Table get/set with metamethod chains ────────────────────────────────────

/// C: `void luaV_finishget(lua_State *L, const TValue *t, TValue *key,
///                          StkId val, const TValue *slot)`
/// Finish a table-get with metamethod lookup.  `slot_was_none = true` means
/// `t` is not a table and we should look for `__index` on `t` itself.
pub(crate) fn finish_get(
    state: &mut LuaState,
    t_val: LuaValue,
    key: LuaValue,
    result_idx: StackIdx,
    slot_empty: bool,
) -> Result<(), LuaError> {
    // C: for (loop = 0; loop < MAXTAGLOOP; loop++)
    let mut t = t_val;
    for _loop in 0..MAX_TAG_LOOP {
        let tm: LuaValue;
        if slot_empty && !matches!(t, LuaValue::Table(_)) {
            // C: if (slot == NULL) { tm = luaT_gettmbyobj(L, t, TM_INDEX); }
            // C: if (l_unlikely(notm(tm))) luaG_typeerror(L, t, "index");
            tm = state.get_tm_by_obj(&t, TagMethod::Index);
            if matches!(tm, LuaValue::Nil) {
                return Err(LuaError::type_error(&t, "index"));
            }
        } else {
            // C: t is a table; tm = fasttm(L, hvalue(t)->metatable, TM_INDEX)
            // C: if (tm == NULL) { setnilvalue(s2v(val)); return; }
            let mt = state.table_metatable(&t);
            tm = state.fast_tm_table(mt.as_ref(), TagMethod::Index);
            if matches!(tm, LuaValue::Nil) {
                state.set_at(result_idx, LuaValue::Nil);
                return Ok(());
            }
        }
        // C: if (ttisfunction(tm)) { luaT_callTMres(...); return; }
        if matches!(tm, LuaValue::Function(_)) {
            state.call_tm_res(tm, &t, &key, result_idx)?;
            return Ok(());
        }
        // C: t = tm; try t[key] again
        t = tm.clone();
        // C: if (luaV_fastget(L, t, key, slot, luaH_get))
        if let Some(v) = state.fast_get(&t, &key)? {
            state.set_at(result_idx, v);
            return Ok(());
        }
        // else: loop — tail-call luaV_finishget
    }
    Err(LuaError::runtime(format_args!("'__index' chain too long; possible loop")))
}

/// C: `void luaV_finishset(lua_State *L, const TValue *t, TValue *key,
///                          TValue *val, const TValue *slot)`
/// Finish a table-set with `__newindex` metamethod lookup.
pub(crate) fn finish_set(
    state: &mut LuaState,
    t_val: LuaValue,
    key: LuaValue,
    val: LuaValue,
    _slot_present: bool,
) -> Result<(), LuaError> {
    let mut t = t_val;
    for _loop in 0..MAX_TAG_LOOP {
        let tm: LuaValue;
        if matches!(t, LuaValue::Table(_)) {
            // C: tm = fasttm(L, h->metatable, TM_NEWINDEX)
            let mt = state.table_metatable(&t);
            tm = state.fast_tm_table(mt.as_ref(), TagMethod::NewIndex);
            if matches!(tm, LuaValue::Nil) {
                // C: luaH_finishset(L, h, key, slot, val); invalidate; barrier; return;
                state.table_raw_set(&t, key, val.clone())?;
                state.gc_barrier_back(&t, &val);
                return Ok(());
            }
        } else {
            // C: tm = luaT_gettmbyobj(L, t, TM_NEWINDEX)
            // C: if (notm(tm)) luaG_typeerror(L, t, "index");
            tm = state.get_tm_by_obj(&t, TagMethod::NewIndex);
            if matches!(tm, LuaValue::Nil) {
                return Err(LuaError::type_error(&t, "index"));
            }
        }
        // C: if (ttisfunction(tm)) { luaT_callTM(L, tm, t, key, val); return; }
        if matches!(tm, LuaValue::Function(_)) {
            state.call_tm(tm, &t, &key, &val)?;
            return Ok(());
        }
        // C: t = tm; luaV_fastget again
        t = tm.clone();
        if state.fast_get(&t, &key)?.is_some() {
            // C: luaV_finishfastset(L, t, slot, val)
            state.table_raw_set(&t, key.clone(), val.clone())?;
            state.gc_barrier_back(&t, &val);
            return Ok(());
        }
    }
    Err(LuaError::runtime(format_args!("'__newindex' chain too long; possible loop")))
}

// ─── String comparison ───────────────────────────────────────────────────────

/// C: `static int l_strcmp(const TString *ts1, const TString *ts2)`
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

/// C: `l_sinline int LTintfloat(lua_Integer i, lua_Number f)` — `i < f`
#[inline]
fn lt_int_float(i: i64, f: f64) -> bool {
    // C: if (l_intfitsf(i)) return luai_numlt(cast_num(i), f);
    if int_fits_float(i) {
        (i as f64) < f
    } else {
        // C: i < f <=> i < ceil(f)
        match flt_to_integer(f, F2Imod::Ceil) {
            Some(fi) => i < fi,
            None => f > 0.0, // f is out of integer range; positive means i < f
        }
    }
}

/// C: `l_sinline int LEintfloat(lua_Integer i, lua_Number f)` — `i <= f`
#[inline]
fn le_int_float(i: i64, f: f64) -> bool {
    if int_fits_float(i) {
        (i as f64) <= f
    } else {
        // C: i <= f <=> i <= floor(f)
        match flt_to_integer(f, F2Imod::Floor) {
            Some(fi) => i <= fi,
            None => f > 0.0,
        }
    }
}

/// C: `l_sinline int LTfloatint(lua_Number f, lua_Integer i)` — `f < i`
#[inline]
fn lt_float_int(f: f64, i: i64) -> bool {
    if int_fits_float(i) {
        f < (i as f64)
    } else {
        // C: f < i <=> floor(f) < i
        match flt_to_integer(f, F2Imod::Floor) {
            Some(fi) => fi < i,
            None => f < 0.0,
        }
    }
}

/// C: `l_sinline int LEfloatint(lua_Number f, lua_Integer i)` — `f <= i`
#[inline]
fn le_float_int(f: f64, i: i64) -> bool {
    if int_fits_float(i) {
        f <= (i as f64)
    } else {
        // C: f <= i <=> ceil(f) <= i
        match flt_to_integer(f, F2Imod::Ceil) {
            Some(fi) => fi <= i,
            None => f < 0.0,
        }
    }
}

/// C: `l_sinline int LTnum(const TValue *l, const TValue *r)` — `l < r` for numbers.
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

/// C: `l_sinline int LEnum(const TValue *l, const TValue *r)` — `l <= r` for numbers.
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

/// C: `static int lessthanothers(lua_State *L, const TValue *l, const TValue *r)`
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

/// C: `int luaV_lessthan(lua_State *L, const TValue *l, const TValue *r)`
pub(crate) fn less_than(state: &mut LuaState, l: &LuaValue, r: &LuaValue) -> Result<bool, LuaError> {
    if matches!(l, LuaValue::Int(_) | LuaValue::Float(_))
        && matches!(r, LuaValue::Int(_) | LuaValue::Float(_))
    {
        Ok(lt_num(l, r))
    } else {
        less_than_others(state, l, r)
    }
}

/// C: `static int lessequalothers(lua_State *L, const TValue *l, const TValue *r)`
fn less_equal_others(state: &mut LuaState, l: &LuaValue, r: &LuaValue) -> Result<bool, LuaError> {
    match (l, r) {
        (LuaValue::Str(ts1), LuaValue::Str(ts2)) => {
            Ok(str_cmp(ts1.as_bytes(), ts2.as_bytes()) != std::cmp::Ordering::Greater)
        }
        _ => state.call_order_tm(l, r, TagMethod::Le),
    }
}

/// C: `int luaV_lessequal(lua_State *L, const TValue *l, const TValue *r)`
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

/// C: `int luaV_equalobj(lua_State *L, const TValue *t1, const TValue *t2)`
/// Main equality test.  `raw = true` means no metamethods (L == NULL in C).
pub(crate) fn equal_obj(
    state: Option<&mut LuaState>,
    t1: &LuaValue,
    t2: &LuaValue,
) -> Result<bool, LuaError> {
    // C: if (ttypetag(t1) != ttypetag(t2)) — different full type tags?
    // In Rust, same variant = same tag.  If variant differs, check the number
    // special case (Int and Float can be equal).
    let same_variant = std::mem::discriminant(t1) == std::mem::discriminant(t2);
    if !same_variant {
        // C: if (ttype(t1) != ttype(t2) || ttype(t1) != LUA_TNUMBER) return 0;
        let t1_is_num = matches!(t1, LuaValue::Int(_) | LuaValue::Float(_));
        let t2_is_num = matches!(t2, LuaValue::Int(_) | LuaValue::Float(_));
        if !(t1_is_num && t2_is_num) {
            return Ok(false);
        }
        // C: two numbers with different variants — compare via integer conversion
        // luaV_tointegerns(t1, &i1, F2Ieq) && luaV_tointegerns(t2, &i2, F2Ieq) && i1==i2
        let i1 = to_integer_ns(t1, F2Imod::Eq);
        let i2 = to_integer_ns(t2, F2Imod::Eq);
        return Ok(i1.is_some() && i2.is_some() && i1 == i2);
    }

    // C: same variant — switch on type tag
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
            // C: eqshrstr for short strings (pointer eq after interning),
            //    luaS_eqlngstr for long strings (content eq).
            // In Rust, LuaString PartialEq handles both.
            Ok(s1 == s2)
        }
        (LuaValue::UserData(u1), LuaValue::UserData(u2)) => {
            // C: if (uvalue(t1) == uvalue(t2)) return 1;
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
            // C: luaT_callTMres(L, tm, t1, t2, L->top.p); return !l_isfalse(s2v(L->top.p));
            let result = state.call_tm_res_bool(tm, t1, t2)?;
            Ok(result)
        }
        (LuaValue::Table(h1), LuaValue::Table(h2)) => {
            // C: if (hvalue(t1) == hvalue(t2)) return 1;
            if std::ptr::eq(h1.as_ptr(), h2.as_ptr()) {
                return Ok(true);
            }
            let Some(state) = state else { return Ok(false); };
            let tm1 = state.fast_tm_table(Some(h1), TagMethod::Eq);
            let tm = if matches!(tm1, LuaValue::Nil) {
                state.fast_tm_table(Some(h2), TagMethod::Eq)
            } else {
                tm1
            };
            if matches!(tm, LuaValue::Nil) {
                return Ok(false);
            }
            let result = state.call_tm_res_bool(tm, t1, t2)?;
            Ok(result)
        }
        // C: default: return gcvalue(t1) == gcvalue(t2)
        _ => Ok(std::ptr::eq(t1 as *const _, t2 as *const _)),
    }
}

// ─── Concatenation ───────────────────────────────────────────────────────────

/// C: `static void copy2buff(StkId top, int n, char *buff)`
/// Copy `n` strings from `top-n .. top-1` into `buff`.
fn copy_to_buf(state: &LuaState, top: StackIdx, n: u32, buf: &mut Vec<u8>) {
    buf.clear();
    // C: do { TString *st = tsvalue(s2v(top - n)); ... } while (--n > 0)
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

/// C: `void luaV_concat(lua_State *L, int total)`
/// Concatenate `total` values on the top of the stack, leaving one result.
pub(crate) fn concat(state: &mut LuaState, total: i32) -> Result<(), LuaError> {
    if total == 1 {
        return Ok(()); // C: "all values already concatenated"
    }
    let mut total = total;
    // C: do { ... } while (total > 1)
    loop {
        let top = state.top_idx();
        let v_tm1 = state.get_at(top - 1).clone(); // top-1
        let v_tm2 = state.get_at(top - 2).clone(); // top-2

        // C: if (!(ttisstring(s2v(top-2)) || cvt2str(s2v(top-2))) || !tostring(L, s2v(top-1)))
        //    luaT_tryconcatTM(L);
        let top2_coercible = matches!(v_tm2, LuaValue::Str(_))
            || matches!(v_tm2, LuaValue::Int(_) | LuaValue::Float(_));
        // tostring converts numbers to strings; we check top-1 too
        let top1_stringlike = matches!(v_tm1, LuaValue::Str(_))
            || matches!(v_tm1, LuaValue::Int(_) | LuaValue::Float(_));
        if !top2_coercible || !top1_stringlike {
            state.try_concat_tm(&v_tm1, &v_tm2)?;
            // C: luaT_tryconcatTM may invalidate `top` — update
            total -= 1;
            if total <= 1 {
                break;
            }
            continue;
        }

        // C: isemptystr — short string with shrlen == 0
        let is_empty = |v: &LuaValue| -> bool {
            matches!(v, LuaValue::Str(s) if s.as_bytes().is_empty())
        };

        let n: u32;
        if is_empty(&v_tm1) {
            // C: result is top-2 (tostring it if needed); consumed 2 inputs → 1 result
            state.coerce_to_string(top - 2)?;
            n = 2;
        } else if is_empty(&v_tm2) {
            // C: setobjs2s(L, top-2, top-1); consumed 2 inputs → 1 result
            let v = state.get_at(top - 1).clone();
            state.set_at(top - 2, v);
            n = 2;
        } else {
            // C: collect as many consecutive string/number values as possible
            // Ensure top-1 is a string (coerce if number)
            state.coerce_to_string(top - 1)?;
            let s1 = match state.get_at(top - 1) {
                LuaValue::Str(ts) => ts.as_bytes().len(),
                _ => 0,
            };
            let mut total_len = s1;
            let mut count: u32 = 1;
            // C: for (n = 1; n < total && tostring(L, s2v(top - n - 1)); n++)
            let top = state.top_idx();
            loop {
                if count as i32 >= total {
                    break;
                }
                let idx = top - (count as i32 + 1);
                let v = state.get_at(idx).clone();
                if !matches!(v, LuaValue::Str(_) | LuaValue::Int(_) | LuaValue::Float(_)) {
                    break;
                }
                state.coerce_to_string(idx)?;
                let l = match state.get_at(idx) {
                    LuaValue::Str(ts) => ts.as_bytes().len(),
                    _ => 0,
                };
                // C: if (l >= MAX_SIZE - sizeof(TString) - tl) luaG_runerror
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
            // C: if (tl <= LUAI_MAXSHORTLEN) short string; else luaS_createlngstrobj
            let mut buf: Vec<u8> = Vec::with_capacity(total_len);
            let top = state.top_idx();
            copy_to_buf(state, top, n, &mut buf);
            let ts = state.intern_or_create_str(&buf)?;
            state.set_at(top - n as i32, LuaValue::Str(ts));
        }
        // C: total -= n - 1; L->top.p -= n - 1;
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

/// C: `void luaV_objlen(lua_State *L, StkId ra, const TValue *rb)`
/// Main implementation of the `#` operator.
pub(crate) fn obj_len(state: &mut LuaState, ra: StackIdx, rb: LuaValue) -> Result<(), LuaError> {
    // C: switch (ttypetag(rb))
    match &rb {
        LuaValue::Table(_) => {
            // C: tm = fasttm(L, h->metatable, TM_LEN)
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
            // C: case LUA_VSHRSTR: setivalue(s2v(ra), tsvalue(rb)->shrlen);
            //    case LUA_VLNGSTR: setivalue(s2v(ra), tsvalue(rb)->u.lnglen);
            // Unified in Rust — just get length
            let n = ts.len();
            state.set_at(ra, LuaValue::Int(n as i64));
        }
        other => {
            // C: default: tm = luaT_gettmbyobj(L, rb, TM_LEN)
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

/// C: `lua_Integer luaV_idiv(lua_State *L, lua_Integer m, lua_Integer n)`
/// Integer floor-division.
pub(crate) fn idiv(m: i64, n: i64) -> Result<i64, LuaError> {
    // C: if (l_unlikely(l_castS2U(n) + 1u <= 1u)) — handles n==0 and n==-1
    if (n as u64).wrapping_add(1) <= 1 {
        if n == 0 {
            return Err(LuaError::runtime(format_args!("attempt to divide by zero")));
        }
        // C: n == -1; avoid overflow with 0x80000...// -1 → intop(-, 0, m)
        return Ok(intop_sub(0, m));
    }
    // C: q = m / n; if ((m ^ n) < 0 && m % n != 0) q -= 1;
    let q = m / n;
    // Correct toward floor (C division truncates toward zero)
    if (m ^ n) < 0 && m % n != 0 {
        Ok(q - 1)
    } else {
        Ok(q)
    }
}

/// C: `lua_Integer luaV_mod(lua_State *L, lua_Integer m, lua_Integer n)`
/// Integer modulus (Lua semantics: same sign as divisor).
pub(crate) fn imod(m: i64, n: i64) -> Result<i64, LuaError> {
    if (n as u64).wrapping_add(1) <= 1 {
        if n == 0 {
            return Err(LuaError::runtime(format_args!("attempt to perform 'n%%0'")));
        }
        // C: m % -1 == 0; avoid overflow
        return Ok(0);
    }
    let r = m % n;
    // C: if (r != 0 && (r ^ n) < 0) r += n
    if r != 0 && (r ^ n) < 0 {
        Ok(r + n)
    } else {
        Ok(r)
    }
}

/// C: `lua_Number luaV_modf(lua_State *L, lua_Number m, lua_Number n)`
/// Float modulus (Lua semantics).
pub(crate) fn fmodf(m: f64, n: f64) -> f64 {
    // C: luai_nummod(L, m, n, r) — fmod with sign correction
    // state.float_mod(m, n)
    let r = m % n;
    if r != 0.0 && r * n < 0.0 {
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

/// C: `lua_Integer luaV_mod(lua_State *L, lua_Integer m, lua_Integer n)`
/// Integer floor-mod: Lua's `%` operator on integers. Result has the same sign
/// as the divisor. Raises on `n == 0`.
pub(crate) fn int_floor_mod(_state: &mut LuaState, a: i64, b: i64) -> Result<i64, LuaError> {
    imod(a, b)
}

/// C: `lua_Integer luaV_idiv(lua_State *L, lua_Integer m, lua_Integer n)`
/// Integer floor-div: Lua's `//` operator on integers. Truncates toward
/// negative infinity. Raises on `n == 0`.
pub(crate) fn int_floor_div(_state: &mut LuaState, a: i64, b: i64) -> Result<i64, LuaError> {
    idiv(a, b)
}

/// C: `lua_Number luaV_modf(lua_State *L, lua_Number m, lua_Number n)`
/// Float floor-mod: Lua's `%` operator on floats. Result has the same sign as
/// the divisor.  NaN / division-by-zero behavior mirrors C `fmod`.
pub(crate) fn float_floor_mod(_state: &mut LuaState, a: f64, b: f64) -> Result<f64, LuaError> {
    Ok(fmodf(a, b))
}

/// C: `lua_Integer luaV_shiftl(lua_Integer x, lua_Integer y)`
/// Left shift; right shift is shift-left by negated count.
pub(crate) fn shiftl(x: i64, y: i64) -> i64 {
    if y < 0 {
        // C: shift right (luaV_shiftr via intop negation)
        if y <= -(NBITS as i64) {
            0
        } else {
            intop_shr(x, (-y) as u32)
        }
    } else {
        // C: shift left
        if y >= NBITS as i64 {
            0
        } else {
            intop_shl(x, y as u32)
        }
    }
}

// ─── Closure creation ────────────────────────────────────────────────────────

/// C: `static void pushclosure(lua_State *L, Proto *p, UpVal **encup,
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
    // C: int nup = p->sizeupvalues; Upvaldesc *uv = p->upvalues;
    // C: LClosure *ncl = luaF_newLclosure(L, nup); ncl->p = p;
    // C: setclLvalue2s(L, ra, ncl);
    // C: for (i = 0; i < nup; i++) { ... }
    // TODO(port): pushclosure needs access to the enclosing closure's upvals and
    // the child proto from the current frame.  This stub forwards to a LuaState
    // method that has the required context.
    state.push_closure(proto_idx, ci, base, ra)
}

// ─── Yield recovery ──────────────────────────────────────────────────────────

/// C: `void luaV_finishOp(lua_State *L)`
/// Resume the opcode that was interrupted by a yield.
/// Called when a coroutine is resumed after yielding mid-instruction.
pub(crate) fn finish_op(state: &mut LuaState) -> Result<(), LuaError> {
    // C: CallInfo *ci = L->ci;
    //    StkId base = ci->func.p + 1;
    //    Instruction inst = *(ci->u.l.savedpc - 1);
    //    OpCode op = GET_OPCODE(inst);
    let ci = state.current_ci_idx();
    let base = state.ci_base(ci);
    let inst = state.ci_prev_instruction(ci);
    let op = inst.opcode();

    match op {
        // C: case OP_MMBIN: case OP_MMBINI: case OP_MMBINK:
        //    setobjs2s(L, base + GETARG_A(*(ci->u.l.savedpc - 2)), --L->top.p);
        OpCode::MmBin | OpCode::MmBinI | OpCode::MmBinK => {
            let prev_inst = state.ci_prev2_instruction(ci);
            let a = prev_inst.arg_a();
            state.dec_top();
            let top = state.top_idx();
            let v = state.get_at(top).clone();
            state.set_at(base + a, v);
        }
        // C: case OP_UNM: ... case OP_SELF:
        //    setobjs2s(L, base + GETARG_A(inst), --L->top.p);
        OpCode::Unm | OpCode::BNot | OpCode::Len
        | OpCode::GetTabUp | OpCode::GetTable | OpCode::GetI
        | OpCode::GetField | OpCode::Self_ => {
            let a = inst.arg_a();
            state.dec_top();
            let top = state.top_idx();
            let v = state.get_at(top).clone();
            state.set_at(base + a, v);
        }
        // C: case OP_LT: case OP_LE: case OP_LTI: case OP_LEI:
        //    case OP_GTI: case OP_GEI: case OP_EQ:
        //    int res = !l_isfalse(s2v(L->top.p - 1)); L->top.p--;
        //    if (res != GETARG_k(inst)) ci->u.l.savedpc++;
        OpCode::Lt | OpCode::Le | OpCode::LtI | OpCode::LeI
        | OpCode::GtI | OpCode::GeI | OpCode::Eq => {
            let top_minus1 = state.top_idx() - 1;
            let v = state.get_at(top_minus1).clone();
            let res = !matches!(v, LuaValue::Nil | LuaValue::Bool(false));
            state.dec_top();
            // C: if (res != GETARG_k(inst)) ci->u.l.savedpc++;
            if (res as i32) != inst.arg_k() {
                state.ci_skip_next_instruction(ci);
            }
            // Note: CIST_LEQ compatibility not supported (LUA_COMPAT_LT_LE dropped)
        }
        // C: case OP_CONCAT:
        //    StkId top = L->top.p - 1;
        //    int a = GETARG_A(inst);
        //    int total = cast_int(top - 1 - (base + a));
        //    setobjs2s(L, top - 2, top);  L->top.p = top - 1;
        //    luaV_concat(L, total);
        OpCode::Concat => {
            let top = state.top_idx() - 1; // top when luaT_tryconcatTM was called
            let a = inst.arg_a();
            let total_concat = (top - 1 - (base + a)) as i32;
            // C: setobjs2s(L, top - 2, top) — put TM result in proper position
            let v = state.get_at(top).clone();
            state.set_at(top - 2, v);
            // C: L->top.p = top - 1
            state.set_top(top - 1);
            concat(state, total_concat)?;
        }
        // C: case OP_CLOSE: ci->u.l.savedpc--;  (repeat to close other vars)
        OpCode::Close => {
            state.ci_step_pc_back(ci);
        }
        // C: case OP_RETURN:
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
            // C: only those other opcodes can yield
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

/// C: `void luaV_execute(lua_State *L, CallInfo *ci)`
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
        // C: startfunc: trap = L->hookmask;
        trap = state.hook_mask() != 0;

        // PORT NOTE: `returning:` is the re-entry after a Lua call returns.
        // Re-enters 'returning without resetting trap.
        'returning: loop {
            // C: cl = ci_func(ci); k = cl->p->k; pc = ci->u.l.savedpc;
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

            // C: if (l_unlikely(trap)) trap = luaG_tracecall(L);
            if trap {
                trap = state.trace_call(ci)?;
            }
            // C: base = ci->func.p + 1;
            let mut base: StackIdx = state.ci_base(ci);

            // ── Main dispatch loop ──────────────────────────────────────────
            // C: for (;;) { Instruction i; vmfetch(); vmdispatch(GET_OPCODE(i)) {...} }
            'dispatch: loop {
                // C: vmfetch() — handle hooks, then fetch+advance pc
                if trap {
                    // C: trap = luaG_traceexec(L, pc); updatebase(ci);
                    trap = state.trace_exec(ci, pc)?;
                    base = state.ci_base(ci); // updatebase
                }
                let i: Instruction = state.proto_code(&cl, pc);
                pc += 1;

                debug_assert!(base == state.ci_base(ci));

                // C: vmdispatch(GET_OPCODE(i))
                match i.opcode() {
                    // ── OP_MOVE ──────────────────────────────────────────────
                    // C: StkId ra = RA(i); setobjs2s(L, ra, RB(i));
                    OpCode::Move => {
                        let ra = base + i.arg_a();
                        let rb = base + i.arg_b();
                        let v = state.get_at(rb).clone();
                        state.set_at(ra, v);
                    }
                    // ── OP_LOADI ─────────────────────────────────────────────
                    // C: lua_Integer b = GETARG_sBx(i); setivalue(s2v(ra), b);
                    OpCode::LoadI => {
                        let ra = base + i.arg_a();
                        let b = i.arg_s_bx() as i64;
                        state.set_at(ra, LuaValue::Int(b));
                    }
                    // ── OP_LOADF ─────────────────────────────────────────────
                    // C: int b = GETARG_sBx(i); setfltvalue(s2v(ra), cast_num(b));
                    OpCode::LoadF => {
                        let ra = base + i.arg_a();
                        let b = i.arg_s_bx() as f64;
                        state.set_at(ra, LuaValue::Float(b));
                    }
                    // ── OP_LOADK ─────────────────────────────────────────────
                    // C: TValue *rb = k + GETARG_Bx(i); setobj2s(L, ra, rb);
                    OpCode::LoadK => {
                        let ra = base + i.arg_a();
                        let k_idx = i.arg_bx() as usize;
                        let v = state.proto_const(&cl, k_idx).clone();
                        state.set_at(ra, v);
                    }
                    // ── OP_LOADKX ────────────────────────────────────────────
                    // C: rb = k + GETARG_Ax(*pc); pc++;
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
                    // C: setbfvalue(s2v(ra)); pc++; (skip next instruction)
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
                    // C: int b = GETARG_B(i); do { setnilvalue(s2v(ra++)); } while (b--);
                    OpCode::LoadNil => {
                        let ra = base + i.arg_a();
                        let b = i.arg_b();
                        for k in 0..=b {
                            state.set_at(ra + k, LuaValue::Nil);
                        }
                    }
                    // ── OP_GETUPVAL ──────────────────────────────────────────
                    // C: setobj2s(L, ra, cl->upvals[b]->v.p);
                    OpCode::GetUpVal => {
                        let ra = base + i.arg_a();
                        let b = i.arg_b() as usize;
                        let v = state.upvalue_get(&cl, b);
                        state.set_at(ra, v);
                    }
                    // ── OP_SETUPVAL ──────────────────────────────────────────
                    // C: UpVal *uv = cl->upvals[GETARG_B(i)];
                    //    setobj(L, uv->v.p, s2v(ra)); luaC_barrier(L, uv, s2v(ra));
                    OpCode::SetUpVal => {
                        let ra = base + i.arg_a();
                        let b = i.arg_b() as usize;
                        let v = state.get_at(ra).clone();
                        state.upvalue_set(&cl, b, v.clone())?;
                        state.gc_barrier_upval(&cl, b, &v);
                    }
                    // ── OP_GETTABUP ──────────────────────────────────────────
                    // C: upval = cl->upvals[B]->v.p; rc = KC(i) (short string key)
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
                                // C: Protect(luaV_finishget(...))
                                state.set_ci_savedpc(ci, pc);
                                state.set_top(state.ci_top(ci));
                                finish_get(state, upval, key, ra, true)?;
                                trap = state.ci_trap(ci);
                            }
                        }
                    }
                    // ── OP_GETTABLE ──────────────────────────────────────────
                    // C: rb = vRB(i); rc = vRC(i);
                    //    if (integer key) fastgeti else fastget
                    OpCode::GetTable => {
                        let ra = base + i.arg_a();
                        let rb_v = state.get_at(base + i.arg_b()).clone();
                        let rc_v = state.get_at(base + i.arg_c()).clone();
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
                                finish_get(state, rb_v, rc_v, ra, true)?;
                                trap = state.ci_trap(ci);
                            }
                        }
                    }
                    // ── OP_GETI ──────────────────────────────────────────────
                    // C: rb = vRB(i); c = GETARG_C(i);
                    //    if (luaV_fastgeti(L, rb, c, slot)) setobj2s(L, ra, slot)
                    //    else { TValue key; setivalue(&key, c); Protect(finishget) }
                    OpCode::GetI => {
                        let ra = base + i.arg_a();
                        let rb_v = state.get_at(base + i.arg_b()).clone();
                        let c = i.arg_c() as i64;
                        match state.fast_get_int(&rb_v, c)? {
                            Some(v) => state.set_at(ra, v),
                            None => {
                                let key = LuaValue::Int(c);
                                state.set_ci_savedpc(ci, pc);
                                state.set_top(state.ci_top(ci));
                                finish_get(state, rb_v, key, ra, true)?;
                                trap = state.ci_trap(ci);
                            }
                        }
                    }
                    // ── OP_GETFIELD ──────────────────────────────────────────
                    // C: rb = vRB(i); rc = KC(i) (short string key)
                    OpCode::GetField => {
                        let ra = base + i.arg_a();
                        let rb_v = state.get_at(base + i.arg_b()).clone();
                        let k_idx = i.arg_c() as usize;
                        let key = state.proto_const(&cl, k_idx).clone();
                        match state.fast_get_short_str(&rb_v, &key)? {
                            Some(v) => state.set_at(ra, v),
                            None => {
                                state.set_ci_savedpc(ci, pc);
                                state.set_top(state.ci_top(ci));
                                finish_get(state, rb_v, key, ra, true)?;
                                trap = state.ci_trap(ci);
                            }
                        }
                    }
                    // ── OP_SETTABUP ──────────────────────────────────────────
                    // C: upval = cl->upvals[A]->v.p; rb = KB(i) key; rc = RKC(i) val
                    OpCode::SetTabUp => {
                        let a = i.arg_a() as usize;
                        let b_idx = i.arg_b() as usize; // key is KB(i)
                        let rc_v = if i.test_k() {
                            state.proto_const(&cl, i.arg_c() as usize).clone()
                        } else {
                            state.get_at(base + i.arg_c()).clone()
                        };
                        let upval = state.upvalue_get(&cl, a);
                        let key = state.proto_const(&cl, b_idx).clone();
                        match state.fast_get_short_str(&upval, &key)? {
                            Some(_slot) => {
                                // C: luaV_finishfastset(L, upval, slot, rc)
                                state.table_raw_set(&upval, key, rc_v.clone())?;
                                state.gc_barrier_back(&upval, &rc_v);
                            }
                            None => {
                                state.set_ci_savedpc(ci, pc);
                                state.set_top(state.ci_top(ci));
                                finish_set(state, upval, key, rc_v, false)?;
                                trap = state.ci_trap(ci);
                            }
                        }
                    }
                    // ── OP_SETTABLE ───────────────────────────────────────────
                    // C: ra = RA(i) (table); rb = vRB(i) key; rc = RKC(i) val
                    OpCode::SetTable => {
                        let ra_v = state.get_at(base + i.arg_a()).clone();
                        let rb_v = state.get_at(base + i.arg_b()).clone();
                        let rc_v = if i.test_k() {
                            state.proto_const(&cl, i.arg_c() as usize).clone()
                        } else {
                            state.get_at(base + i.arg_c()).clone()
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
                            finish_set(state, ra_v, rb_v, rc_v, false)?;
                            trap = state.ci_trap(ci);
                        }
                    }
                    // ── OP_SETI ───────────────────────────────────────────────
                    // C: ra = RA(i) (table); c = GETARG_B(i) (int key); rc = RKC(i)
                    OpCode::SetI => {
                        let ra_v = state.get_at(base + i.arg_a()).clone();
                        let c = i.arg_b() as i64;
                        let rc_v = if i.test_k() {
                            state.proto_const(&cl, i.arg_c() as usize).clone()
                        } else {
                            state.get_at(base + i.arg_c()).clone()
                        };
                        let fast = state.fast_get_int(&ra_v, c)?;
                        if fast.is_some() {
                            state.table_raw_set(&ra_v, LuaValue::Int(c), rc_v.clone())?;
                            state.gc_barrier_back(&ra_v, &rc_v);
                        } else {
                            state.set_ci_savedpc(ci, pc);
                            state.set_top(state.ci_top(ci));
                            finish_set(state, ra_v, LuaValue::Int(c), rc_v, false)?;
                            trap = state.ci_trap(ci);
                        }
                    }
                    // ── OP_SETFIELD ───────────────────────────────────────────
                    // C: ra = RA(i) table; rb = KB(i) key; rc = RKC(i) val
                    OpCode::SetField => {
                        let ra_v = state.get_at(base + i.arg_a()).clone();
                        let b_idx = i.arg_b() as usize;
                        let key = state.proto_const(&cl, b_idx).clone();
                        let rc_v = if i.test_k() {
                            state.proto_const(&cl, i.arg_c() as usize).clone()
                        } else {
                            state.get_at(base + i.arg_c()).clone()
                        };
                        match state.fast_get_short_str(&ra_v, &key)? {
                            Some(_) => {
                                state.table_raw_set(&ra_v, key, rc_v.clone())?;
                                state.gc_barrier_back(&ra_v, &rc_v);
                            }
                            None => {
                                state.set_ci_savedpc(ci, pc);
                                state.set_top(state.ci_top(ci));
                                finish_set(state, ra_v, key, rc_v, false)?;
                                trap = state.ci_trap(ci);
                            }
                        }
                    }
                    // ── OP_NEWTABLE ───────────────────────────────────────────
                    // C: b = log2(hash size)+1; c = array size
                    //    if (TESTARG_k(i)) c += GETARG_Ax(*pc) * (MAXARG_C + 1); pc++;
                    OpCode::NewTable => {
                        let ra = base + i.arg_a();
                        let mut b = i.arg_b();
                        let mut c = i.arg_c();
                        if b > 0 {
                            b = 1 << (b - 1); // C: b = 1 << (b - 1)
                        }
                        if i.test_k() {
                            let extra = state.proto_code(&cl, pc);
                            pc += 1;
                            // C: c += GETARG_Ax(*pc) * (MAXARG_C + 1)
                            const MAXARG_C: i32 = (1 << 8) - 1;
                            c += extra.arg_ax() * (MAXARG_C + 1);
                        } else {
                            pc += 1; // skip extra argument even if zero
                        }
                        // C: L->top.p = ra + 1; (for emergency GC)
                        state.set_top(ra + 1);
                        let t = state.new_table();
                        state.set_at(ra, LuaValue::Table(t.clone()));
                        if b != 0 || c != 0 {
                            state.table_resize(&t, c as usize, b as usize)?;
                        }
                        // C: checkGC(L, ra + 1)
                        state.set_ci_savedpc(ci, pc);
                        state.set_top(ra + 1);
                        state.gc_cond_step();
                        trap = state.ci_trap(ci);
                    }
                    // ── OP_SELF ───────────────────────────────────────────────
                    // C: ra+1 = rb; if fastget(rb, key) ra=slot else finishget
                    OpCode::Self_ => {
                        let ra = base + i.arg_a();
                        let rb_v = state.get_at(base + i.arg_b()).clone();
                        let k_idx = i.arg_c() as usize; // RKC key (always a string)
                        let key = if i.test_k() {
                            state.proto_const(&cl, k_idx).clone()
                        } else {
                            state.get_at(base + i.arg_c()).clone()
                        };
                        // C: setobj2s(L, ra+1, rb)
                        state.set_at(ra + 1, rb_v.clone());
                        match state.fast_get_short_str(&rb_v, &key)? {
                            Some(v) => state.set_at(ra, v),
                            None => {
                                state.set_ci_savedpc(ci, pc);
                                state.set_top(state.ci_top(ci));
                                finish_get(state, rb_v, key, ra, true)?;
                                trap = state.ci_trap(ci);
                            }
                        }
                    }
                    // ── Arithmetic immediates ──────────────────────────────────
                    // C: op_arithI(L, l_addi, luai_numadd)
                    OpCode::AddI => {
                        let ra = base + i.arg_a();
                        let v1 = state.get_at(base + i.arg_b()).clone();
                        let imm = i.arg_s_c() as i64;
                        match &v1 {
                            LuaValue::Int(iv1) => {
                                pc += 1;
                                state.set_at(ra, LuaValue::Int(intop_add(*iv1, imm)));
                            }
                            LuaValue::Float(nb) => {
                                pc += 1;
                                state.set_at(ra, LuaValue::Float(*nb + imm as f64));
                            }
                            _ => { /* metamethod handled by OP_MMBIN* fallback */ }
                        }
                    }
                    // ── Arithmetic with K constant operand ─────────────────────
                    // C: op_arithK(L, l_addi, luai_numadd)
                    OpCode::AddK => {
                        let ra = base + i.arg_a();
                        let v1 = state.get_at(base + i.arg_b()).clone();
                        let v2 = state.proto_const(&cl, i.arg_c() as usize).clone();
                        arith_op_aux_rr(state, ra, &v1, &v2, &mut pc,
                            intop_add, |a, b| a + b);
                    }
                    OpCode::SubK => {
                        let ra = base + i.arg_a();
                        let v1 = state.get_at(base + i.arg_b()).clone();
                        let v2 = state.proto_const(&cl, i.arg_c() as usize).clone();
                        arith_op_aux_rr(state, ra, &v1, &v2, &mut pc,
                            intop_sub, |a, b| a - b);
                    }
                    OpCode::MulK => {
                        let ra = base + i.arg_a();
                        let v1 = state.get_at(base + i.arg_b()).clone();
                        let v2 = state.proto_const(&cl, i.arg_c() as usize).clone();
                        arith_op_aux_rr(state, ra, &v1, &v2, &mut pc,
                            intop_mul, |a, b| a * b);
                    }
                    // C: op_arithK(L, luaV_mod, luaV_modf) — division by zero possible
                    OpCode::ModK => {
                        let ra = base + i.arg_a();
                        let v1 = state.get_at(base + i.arg_b()).clone();
                        let v2 = state.proto_const(&cl, i.arg_c() as usize).clone();
                        state.set_ci_savedpc(ci, pc); // savestate for div-by-zero
                        state.set_top(state.ci_top(ci));
                        arith_op_checked(state, ra, &v1, &v2, &mut pc,
                            |a, b| imod(a, b), fmodf)?;
                    }
                    // C: op_arithfK(L, luai_numpow) — float only
                    OpCode::PowK => {
                        let ra = base + i.arg_a();
                        let v1 = state.get_at(base + i.arg_b()).clone();
                        let v2 = state.proto_const(&cl, i.arg_c() as usize).clone();
                        arith_float_aux(state, ra, &v1, &v2, &mut pc,
                            |a, b| if b == 2.0 { a * a } else { a.powf(b) });
                    }
                    // C: op_arithfK(L, luai_numdiv) — float division
                    OpCode::DivK => {
                        let ra = base + i.arg_a();
                        let v1 = state.get_at(base + i.arg_b()).clone();
                        let v2 = state.proto_const(&cl, i.arg_c() as usize).clone();
                        arith_float_aux(state, ra, &v1, &v2, &mut pc, |a, b| a / b);
                    }
                    // C: op_arithK(L, luaV_idiv, luai_numidiv)
                    OpCode::IDivK => {
                        let ra = base + i.arg_a();
                        let v1 = state.get_at(base + i.arg_b()).clone();
                        let v2 = state.proto_const(&cl, i.arg_c() as usize).clone();
                        state.set_ci_savedpc(ci, pc);
                        state.set_top(state.ci_top(ci));
                        arith_op_checked(state, ra, &v1, &v2, &mut pc,
                            |a, b| idiv(a, b), |a, b| (a / b).floor())?;
                    }
                    // C: op_bitwiseK(L, l_band/l_bor/l_bxor)
                    OpCode::BAndK => {
                        let ra = base + i.arg_a();
                        let v1 = state.get_at(base + i.arg_b()).clone();
                        let v2 = state.proto_const(&cl, i.arg_c() as usize).clone();
                        bitwise_op_k(state, ra, &v1, &v2, &mut pc, intop_band);
                    }
                    OpCode::BOrK => {
                        let ra = base + i.arg_a();
                        let v1 = state.get_at(base + i.arg_b()).clone();
                        let v2 = state.proto_const(&cl, i.arg_c() as usize).clone();
                        bitwise_op_k(state, ra, &v1, &v2, &mut pc, intop_bor);
                    }
                    OpCode::BXOrK => {
                        let ra = base + i.arg_a();
                        let v1 = state.get_at(base + i.arg_b()).clone();
                        let v2 = state.proto_const(&cl, i.arg_c() as usize).clone();
                        bitwise_op_k(state, ra, &v1, &v2, &mut pc, intop_bxor);
                    }
                    // C: OP_SHRI — rb >> sC (shift right by immediate)
                    // C: if (tointegerns(rb, &ib)) { pc++; setivalue(s2v(ra), luaV_shiftl(ib, -ic)); }
                    OpCode::ShrI => {
                        let ra = base + i.arg_a();
                        let v = state.get_at(base + i.arg_b()).clone();
                        let ic = i.arg_s_c() as i64;
                        if let Some(ib) = to_integer_ns(&v, F2Imod::Eq) {
                            pc += 1;
                            state.set_at(ra, LuaValue::Int(shiftl(ib, -ic)));
                        }
                    }
                    // C: OP_SHLI — sC << rb
                    // C: if (tointegerns(rb, &ib)) { pc++; setivalue(s2v(ra), luaV_shiftl(ic, ib)); }
                    OpCode::ShlI => {
                        let ra = base + i.arg_a();
                        let v = state.get_at(base + i.arg_b()).clone();
                        let ic = i.arg_s_c() as i64;
                        if let Some(ib) = to_integer_ns(&v, F2Imod::Eq) {
                            pc += 1;
                            state.set_at(ra, LuaValue::Int(shiftl(ic, ib)));
                        }
                    }
                    // ── Arithmetic with register operands ──────────────────────
                    OpCode::Add => {
                        let ra = base + i.arg_a();
                        let v1 = state.get_at(base + i.arg_b()).clone();
                        let v2 = state.get_at(base + i.arg_c()).clone();
                        arith_op_aux_rr(state, ra, &v1, &v2, &mut pc,
                            intop_add, |a, b| a + b);
                    }
                    OpCode::Sub => {
                        let ra = base + i.arg_a();
                        let v1 = state.get_at(base + i.arg_b()).clone();
                        let v2 = state.get_at(base + i.arg_c()).clone();
                        arith_op_aux_rr(state, ra, &v1, &v2, &mut pc,
                            intop_sub, |a, b| a - b);
                    }
                    OpCode::Mul => {
                        let ra = base + i.arg_a();
                        let v1 = state.get_at(base + i.arg_b()).clone();
                        let v2 = state.get_at(base + i.arg_c()).clone();
                        arith_op_aux_rr(state, ra, &v1, &v2, &mut pc,
                            intop_mul, |a, b| a * b);
                    }
                    OpCode::Mod => {
                        let ra = base + i.arg_a();
                        let v1 = state.get_at(base + i.arg_b()).clone();
                        let v2 = state.get_at(base + i.arg_c()).clone();
                        state.set_ci_savedpc(ci, pc);
                        state.set_top(state.ci_top(ci));
                        arith_op_checked(state, ra, &v1, &v2, &mut pc,
                            |a, b| imod(a, b), fmodf)?;
                    }
                    OpCode::Pow => {
                        let ra = base + i.arg_a();
                        let v1 = state.get_at(base + i.arg_b()).clone();
                        let v2 = state.get_at(base + i.arg_c()).clone();
                        arith_float_aux(state, ra, &v1, &v2, &mut pc,
                            |a, b| if b == 2.0 { a * a } else { a.powf(b) });
                    }
                    OpCode::Div => {
                        let ra = base + i.arg_a();
                        let v1 = state.get_at(base + i.arg_b()).clone();
                        let v2 = state.get_at(base + i.arg_c()).clone();
                        arith_float_aux(state, ra, &v1, &v2, &mut pc, |a, b| a / b);
                    }
                    OpCode::IDiv => {
                        let ra = base + i.arg_a();
                        let v1 = state.get_at(base + i.arg_b()).clone();
                        let v2 = state.get_at(base + i.arg_c()).clone();
                        state.set_ci_savedpc(ci, pc);
                        state.set_top(state.ci_top(ci));
                        arith_op_checked(state, ra, &v1, &v2, &mut pc,
                            |a, b| idiv(a, b), |a, b| (a / b).floor())?;
                    }
                    // ── Bitwise with register operands ─────────────────────────
                    // C: op_bitwise(L, l_band/l_bor/l_bxor)
                    // if (tointegerns(v1, &i1) && tointegerns(v2, &i2)) { pc++; setivalue... }
                    OpCode::BAnd => {
                        let ra = base + i.arg_a();
                        let v1 = state.get_at(base + i.arg_b()).clone();
                        let v2 = state.get_at(base + i.arg_c()).clone();
                        bitwise_op_rr(state, ra, &v1, &v2, &mut pc, intop_band);
                    }
                    OpCode::BOr => {
                        let ra = base + i.arg_a();
                        let v1 = state.get_at(base + i.arg_b()).clone();
                        let v2 = state.get_at(base + i.arg_c()).clone();
                        bitwise_op_rr(state, ra, &v1, &v2, &mut pc, intop_bor);
                    }
                    OpCode::BXOr => {
                        let ra = base + i.arg_a();
                        let v1 = state.get_at(base + i.arg_b()).clone();
                        let v2 = state.get_at(base + i.arg_c()).clone();
                        bitwise_op_rr(state, ra, &v1, &v2, &mut pc, intop_bxor);
                    }
                    // C: op_bitwise(L, luaV_shiftr) — shift right via shiftl(-y)
                    OpCode::Shr => {
                        let ra = base + i.arg_a();
                        let v1 = state.get_at(base + i.arg_b()).clone();
                        let v2 = state.get_at(base + i.arg_c()).clone();
                        bitwise_shift_rr(state, ra, &v1, &v2, &mut pc, true);
                    }
                    // C: op_bitwise(L, luaV_shiftl)
                    OpCode::Shl => {
                        let ra = base + i.arg_a();
                        let v1 = state.get_at(base + i.arg_b()).clone();
                        let v2 = state.get_at(base + i.arg_c()).clone();
                        bitwise_shift_rr(state, ra, &v1, &v2, &mut pc, false);
                    }
                    // ── OP_MMBIN ─────────────────────────────────────────────
                    // C: fallback metamethod for binary arith ops
                    // Instruction pi = *(pc - 2); TMS tm = (TMS)GETARG_C(i);
                    // StkId result = RA(pi);
                    // Protect(luaT_trybinTM(L, s2v(ra), rb, result, tm));
                    OpCode::MmBin => {
                        let ra_v = state.get_at(base + i.arg_a()).clone();
                        let rb_v = state.get_at(base + i.arg_b()).clone();
                        let tm = tagmethod_from_index(i.arg_c() as usize);
                        let prev_inst = state.proto_code(&cl, pc - 2);
                        let result_idx = base + prev_inst.arg_a();
                        state.set_ci_savedpc(ci, pc);
                        state.set_top(state.ci_top(ci));
                        state.try_bin_tm(&ra_v, &rb_v, result_idx, tm)?;
                        trap = state.ci_trap(ci);
                    }
                    // C: OP_MMBINI — metamethod for arith-with-immediate
                    OpCode::MmBinI => {
                        let ra_v = state.get_at(base + i.arg_a()).clone();
                        let imm = i.arg_s_b() as i64;
                        let tm = tagmethod_from_index(i.arg_c() as usize);
                        let flip = i.arg_k() != 0;
                        let prev_inst = state.proto_code(&cl, pc - 2);
                        let result_idx = base + prev_inst.arg_a();
                        state.set_ci_savedpc(ci, pc);
                        state.set_top(state.ci_top(ci));
                        state.try_bin_i_tm(&ra_v, imm, flip, result_idx, tm)?;
                        trap = state.ci_trap(ci);
                    }
                    // C: OP_MMBINK — metamethod for arith-with-K
                    OpCode::MmBinK => {
                        let ra_v = state.get_at(base + i.arg_a()).clone();
                        let imm = state.proto_const(&cl, i.arg_b() as usize).clone();
                        let tm = tagmethod_from_index(i.arg_c() as usize);
                        let flip = i.arg_k() != 0;
                        let prev_inst = state.proto_code(&cl, pc - 2);
                        let result_idx = base + prev_inst.arg_a();
                        state.set_ci_savedpc(ci, pc);
                        state.set_top(state.ci_top(ci));
                        state.try_bin_assoc_tm(&ra_v, &imm, flip, result_idx, tm)?;
                        trap = state.ci_trap(ci);
                    }
                    // ── OP_UNM ───────────────────────────────────────────────
                    // C: if (ttisinteger(rb)) setivalue(s2v(ra), intop(-,0,ib))
                    //    else if (tonumberns(rb, nb)) setfltvalue(s2v(ra), -nb)
                    //    else Protect(luaT_trybinTM(L, rb, rb, ra, TM_UNM))
                    OpCode::Unm => {
                        let ra = base + i.arg_a();
                        let rb_v = state.get_at(base + i.arg_b()).clone();
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
                                state.try_bin_tm(&rb_v, &rb_v, ra, TagMethod::Unm)?;
                                trap = state.ci_trap(ci);
                            }
                        }
                    }
                    // ── OP_BNOT ──────────────────────────────────────────────
                    // C: if (tointegerns(rb, &ib)) setivalue(s2v(ra), intop(^, ~0u64, ib))
                    OpCode::BNot => {
                        let ra = base + i.arg_a();
                        let rb_v = state.get_at(base + i.arg_b()).clone();
                        if let Some(ib) = to_integer_ns(&rb_v, F2Imod::Eq) {
                            // C: intop(^, ~l_castS2U(0), ib) == bitwise NOT of ib
                            state.set_at(ra, LuaValue::Int(!ib));
                        } else {
                            state.set_ci_savedpc(ci, pc);
                            state.set_top(state.ci_top(ci));
                            state.try_bin_tm(&rb_v, &rb_v, ra, TagMethod::Bnot)?;
                            trap = state.ci_trap(ci);
                        }
                    }
                    // ── OP_NOT ───────────────────────────────────────────────
                    // C: if (l_isfalse(rb)) setbtvalue else setbfvalue
                    OpCode::Not => {
                        let ra = base + i.arg_a();
                        let rb_v = state.get_at(base + i.arg_b()).clone();
                        let falsy = matches!(rb_v, LuaValue::Nil | LuaValue::Bool(false));
                        state.set_at(ra, LuaValue::Bool(falsy));
                    }
                    // ── OP_LEN ───────────────────────────────────────────────
                    // C: Protect(luaV_objlen(L, ra, vRB(i)));
                    OpCode::Len => {
                        let ra = base + i.arg_a();
                        let rb_v = state.get_at(base + i.arg_b()).clone();
                        state.set_ci_savedpc(ci, pc);
                        state.set_top(state.ci_top(ci));
                        obj_len(state, ra, rb_v)?;
                        trap = state.ci_trap(ci);
                    }
                    // ── OP_CONCAT ─────────────────────────────────────────────
                    // C: n = GETARG_B(i); L->top.p = ra+n; ProtectNT(luaV_concat(L,n));
                    OpCode::Concat => {
                        let ra = base + i.arg_a();
                        let n = i.arg_b() as i32;
                        state.set_top(ra + n as i32);
                        state.set_ci_savedpc(ci, pc); // ProtectNT: save pc only
                        concat(state, n)?;
                        trap = state.ci_trap(ci);
                        // C: checkGC
                        let top = state.top_idx();
                        state.set_ci_savedpc(ci, pc);
                        state.set_top(top);
                        state.gc_cond_step();
                        trap = state.ci_trap(ci);
                    }
                    // ── OP_CLOSE ──────────────────────────────────────────────
                    // C: Protect(luaF_close(L, ra, LUA_OK, 1));
                    OpCode::Close => {
                        let ra = base + i.arg_a();
                        state.set_ci_savedpc(ci, pc);
                        state.set_top(state.ci_top(ci));
                        state.close_upvals(ra)?; // LUA_OK = 0
                        trap = state.ci_trap(ci);
                    }
                    // ── OP_TBC ────────────────────────────────────────────────
                    // C: halfProtect(luaF_newtbcupval(L, ra));
                    OpCode::Tbc => {
                        let ra = base + i.arg_a();
                        state.set_ci_savedpc(ci, pc);
                        state.set_top(state.ci_top(ci));
                        state.new_tbc_upval(ra)?;
                    }
                    // ── OP_JMP ────────────────────────────────────────────────
                    // C: dojump(ci, i, 0) → pc += GETARG_sJ(i) + 0; updatetrap(ci);
                    OpCode::Jmp => {
                        pc = (pc as i64 + i.arg_s_j() as i64) as u32;
                        trap = state.ci_trap(ci);
                    }
                    // ── OP_EQ ─────────────────────────────────────────────────
                    // C: Protect(cond = luaV_equalobj(L, s2v(ra), rb)); docondjump()
                    OpCode::Eq => {
                        let ra_v = state.get_at(base + i.arg_a()).clone();
                        let rb_v = state.get_at(base + i.arg_b()).clone();
                        state.set_ci_savedpc(ci, pc);
                        state.set_top(state.ci_top(ci));
                        let cond = equal_obj(Some(state), &ra_v, &rb_v)? as u32;
                        trap = state.ci_trap(ci);
                        // C: docondjump() — if cond != GETARG_k(i): pc++; else: dojump
                        if (cond as i32) != i.arg_k() {
                            pc += 1;
                        } else {
                            let next = state.proto_code(&cl, pc);
                            pc = (pc as i64 + next.arg_s_j() as i64 + 1) as u32;
                            trap = state.ci_trap(ci);
                        }
                    }
                    // ── OP_LT ─────────────────────────────────────────────────
                    // C: op_order(L, l_lti, LTnum, lessthanothers)
                    OpCode::Lt => {
                        let ra_v = state.get_at(base + i.arg_a()).clone();
                        let rb_v = state.get_at(base + i.arg_b()).clone();
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
                    // C: op_order(L, l_lei, LEnum, lessequalothers)
                    OpCode::Le => {
                        let ra_v = state.get_at(base + i.arg_a()).clone();
                        let rb_v = state.get_at(base + i.arg_b()).clone();
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
                    // C: int cond = luaV_rawequalobj(s2v(ra), rb); docondjump()
                    OpCode::EqK => {
                        let ra_v = state.get_at(base + i.arg_a()).clone();
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
                    // C: int im = GETARG_sB(i)
                    //    if (ttisinteger) cond = ivalue == im
                    //    elif (ttisfloat) cond = numeq(fltvalue, cast_num(im))
                    //    else cond = 0
                    OpCode::EqI => {
                        let ra_v = state.get_at(base + i.arg_a()).clone();
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
                    // C: op_orderI(L, l_lti/l_lei/l_gti/l_gei, luai_numlt/le/gt/ge,
                    //              inv=0/0/1/1, tm=TM_LT/TM_LE/TM_LT/TM_LE)
                    OpCode::LtI => {
                        order_imm_op(state, &cl, &mut pc, &mut trap, ci, base, i,
                            |a: i64, b: i64| a < b, |a: f64, b: f64| a < b,
                            false, TagMethod::Lt)?;
                    }
                    OpCode::LeI => {
                        order_imm_op(state, &cl, &mut pc, &mut trap, ci, base, i,
                            |a: i64, b: i64| a <= b, |a: f64, b: f64| a <= b,
                            false, TagMethod::Le)?;
                    }
                    OpCode::GtI => {
                        order_imm_op(state, &cl, &mut pc, &mut trap, ci, base, i,
                            |a: i64, b: i64| a > b, |a: f64, b: f64| a > b,
                            true, TagMethod::Lt)?;
                    }
                    OpCode::GeI => {
                        order_imm_op(state, &cl, &mut pc, &mut trap, ci, base, i,
                            |a: i64, b: i64| a >= b, |a: f64, b: f64| a >= b,
                            true, TagMethod::Le)?;
                    }
                    // ── OP_TEST ────────────────────────────────────────────────
                    // C: int cond = !l_isfalse(s2v(ra)); docondjump()
                    OpCode::Test => {
                        let ra_v = state.get_at(base + i.arg_a()).clone();
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
                    // C: if (l_isfalse(rb) == GETARG_k(i)) pc++;
                    //    else { setobj2s(L, ra, rb); donextjump(ci); }
                    OpCode::TestSet => {
                        let ra = base + i.arg_a();
                        let rb_v = state.get_at(base + i.arg_b()).clone();
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
                    // C: if ((newci = luaD_precall(L, ra, nresults)) == NULL)
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
                        match state.precall(ra, nresults)? {
                            None => {
                                // C function — nothing else to do
                                trap = state.ci_trap(ci); // updatetrap
                            }
                            Some(new_ci) => {
                                // Lua function — goto startfunc
                                ci = new_ci;
                                continue 'startfunc;
                            }
                        }
                    }
                    // ── OP_TAILCALL ────────────────────────────────────────────
                    // C: if ((n = luaD_pretailcall(L, ci, ra, b, delta)) < 0)
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
                            // C: luaF_closeupval(L, base); assert(L->tbclist.p < base);
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
                            trap = state.ci_trap(ci);
                            break 'dispatch; // goto ret
                        }
                    }
                    // ── OP_RETURN ──────────────────────────────────────────────
                    // C: n = GETARG_B(i)-1; if (n<0) n = cast_int(L->top.p - ra);
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
                            state.close_upvals(base)?;
                            trap = state.ci_trap(ci);
                            base = state.ci_base(ci); // updatestack
                        }
                        if nparams1 != 0 {
                            let nextraargs = state.ci_nextraargs(ci) as u32;
                            state.ci_adjust_func(ci, (nextraargs as i32 + nparams1 as i32));
                        }
                        state.set_top(ra + n as i32);
                        state.poscall(ci, n)?;
                        trap = state.ci_trap(ci);
                        break 'dispatch; // goto ret
                    }
                    // ── OP_RETURN0 ─────────────────────────────────────────────
                    // C: if (L->hookmask) { ra = RA; L->top = ra; savepc; poscall(0); trap=1; }
                    //    else { L->ci = ci->previous; L->top = base-1;
                    //           for (nres = ci->nresults; nres > 0; nres--)
                    //             setnilvalue(L->top++) }
                    //    goto ret;
                    OpCode::Return0 => {
                        if state.hook_mask() != 0 {
                            let ra = base + i.arg_a();
                            state.set_top(ra);
                            state.set_ci_savedpc(ci, pc);
                            state.poscall(ci, 0)?;
                            trap = true;
                        } else {
                            let nres = state.ci_nresults(ci);
                            state.set_ci_previous(ci);
                            state.set_top(base - 1);
                            for _ in 0..nres.max(0) {
                                state.push(LuaValue::Nil);
                            }
                        }
                        break 'dispatch; // goto ret
                    }
                    // ── OP_RETURN1 ─────────────────────────────────────────────
                    // C: if (L->hookmask) { L->top = ra+1; savepc; poscall(1); trap=1; }
                    //    else { nres = ci->nresults; ci = ci->previous; ...handle results... }
                    //    goto ret;
                    OpCode::Return1 => {
                        if state.hook_mask() != 0 {
                            let ra = base + i.arg_a();
                            state.set_top(ra + 1);
                            state.set_ci_savedpc(ci, pc);
                            state.poscall(ci, 1)?;
                            trap = true;
                        } else {
                            let nres = state.ci_nresults(ci);
                            state.set_ci_previous(ci);
                            if nres == 0 {
                                state.set_top(base - 1);
                            } else {
                                let ra = base + i.arg_a();
                                let v = state.get_at(ra).clone();
                                state.set_at(base - 1, v); // at least this result
                                state.set_top(base);
                                for _ in 1..nres.max(0) {
                                    state.push(LuaValue::Nil);
                                }
                            }
                        }
                        break 'dispatch; // goto ret
                    }
                    // ── OP_FORLOOP ─────────────────────────────────────────────
                    // C: if (ttisinteger(s2v(ra+2))) { integer loop }
                    //    else if (floatforloop(ra)) pc -= GETARG_Bx(i)
                    //    updatetrap(ci);
                    OpCode::ForLoop => {
                        let ra = base + i.arg_a();
                        let is_int_loop = matches!(state.get_at(ra + 2), LuaValue::Int(_));
                        if is_int_loop {
                            // C: count = l_castS2U(ivalue(s2v(ra+1)));
                            let count = match state.get_at(ra + 1) {
                                LuaValue::Int(c) => c as u64,
                                _ => 0,
                            };
                            if count > 0 {
                                let step = match state.get_at(ra + 2) {
                                    LuaValue::Int(s) => s,
                                    _ => 0,
                                };
                                let idx = match state.get_at(ra) {
                                    LuaValue::Int(x) => x,
                                    _ => 0,
                                };
                                // C: chgivalue(s2v(ra+1), count-1)
                                state.set_at(ra + 1, LuaValue::Int((count - 1) as i64));
                                // C: idx = intop(+, idx, step)
                                let new_idx = intop_add(idx, step);
                                state.set_at(ra, LuaValue::Int(new_idx));
                                state.set_at(ra + 3, LuaValue::Int(new_idx));
                                // C: pc -= GETARG_Bx(i)
                                pc = (pc as i64 - i.arg_bx() as i64) as u32;
                            }
                        } else if float_for_loop(state, ra) {
                            pc = (pc as i64 - i.arg_bx() as i64) as u32;
                        }
                        trap = state.ci_trap(ci);
                    }
                    // ── OP_FORPREP ─────────────────────────────────────────────
                    // C: savestate; if (forprep(L, ra)) pc += Bx + 1; (skip loop)
                    OpCode::ForPrep => {
                        let ra = base + i.arg_a();
                        state.set_ci_savedpc(ci, pc);
                        state.set_top(state.ci_top(ci));
                        if forprep(state, ra)? {
                            pc = (pc as i64 + i.arg_bx() as i64 + 1) as u32;
                        }
                    }
                    // ── OP_TFORPREP ────────────────────────────────────────────
                    // C: halfProtect(luaF_newtbcupval(L, ra+3));
                    //    pc += GETARG_Bx(i); i = *pc++; assert(OP_TFORCALL && ra==RA(i));
                    //    goto l_tforcall;
                    OpCode::TForPrep => {
                        let ra = base + i.arg_a();
                        state.set_ci_savedpc(ci, pc);
                        state.set_top(state.ci_top(ci));
                        state.new_tbc_upval(ra + 3)?;
                        // C: pc += GETARG_Bx(i); advance to TFORCALL
                        pc = (pc as i64 + i.arg_bx() as i64) as u32;
                        // C: i = *pc++; goto l_tforcall
                        let tfc_i = state.proto_code(&cl, pc);
                        pc += 1;
                        debug_assert!(tfc_i.opcode() == OpCode::TForCall);
                        // inline l_tforcall:
                        let tfc_ra = base + tfc_i.arg_a();
                        // C: memcpy(ra+4, ra, 3*sizeof(*ra)) — copy func, state, ctrl
                        for k in 0..3u32 {
                            let v = state.get_at(tfc_ra + k as i32).clone();
                            state.set_at(tfc_ra + 4 + k as i32, v);
                        }
                        state.set_top(tfc_ra + 4 + 3);
                        // C: ProtectNT(luaD_call(L, ra+4, GETARG_C(i)));
                        state.set_ci_savedpc(ci, pc);
                        state.call_at(tfc_ra + 4, tfc_i.arg_c() as i32)?;
                        trap = state.ci_trap(ci);
                        base = state.ci_base(ci); // updatestack
                        // C: i = *pc++; goto l_tforloop
                        let tfl_i = state.proto_code(&cl, pc);
                        pc += 1;
                        debug_assert!(tfl_i.opcode() == OpCode::TForLoop);
                        let tfl_ra = base + tfl_i.arg_a();
                        // inline l_tforloop:
                        if !matches!(state.get_at(tfl_ra + 4), LuaValue::Nil) {
                            // C: setobjs2s(L, ra+2, ra+4)
                            let v = state.get_at(tfl_ra + 4).clone();
                            state.set_at(tfl_ra + 2, v);
                            // C: pc -= GETARG_Bx(i)
                            pc = (pc as i64 - tfl_i.arg_bx() as i64) as u32;
                        }
                    }
                    // ── OP_TFORCALL ────────────────────────────────────────────
                    // C: l_tforcall: { push func/state/ctrl; call; goto l_tforloop }
                    OpCode::TForCall => {
                        let ra = base + i.arg_a();
                        for k in 0..3u32 {
                            let v = state.get_at(ra + k as i32).clone();
                            state.set_at(ra + 4 + k as i32, v);
                        }
                        state.set_top(ra + 4 + 3);
                        state.set_ci_savedpc(ci, pc);
                        state.call_at(ra + 4, i.arg_c() as i32)?;
                        trap = state.ci_trap(ci);
                        base = state.ci_base(ci); // updatestack
                        // C: i = *pc++; goto l_tforloop
                        let tfl_i = state.proto_code(&cl, pc);
                        pc += 1;
                        debug_assert!(tfl_i.opcode() == OpCode::TForLoop);
                        let tfl_ra = base + tfl_i.arg_a();
                        if !matches!(state.get_at(tfl_ra + 4), LuaValue::Nil) {
                            let v = state.get_at(tfl_ra + 4).clone();
                            state.set_at(tfl_ra + 2, v);
                            pc = (pc as i64 - tfl_i.arg_bx() as i64) as u32;
                        }
                    }
                    // ── OP_TFORLOOP ────────────────────────────────────────────
                    // C: l_tforloop: if (!ttisnil(s2v(ra+4))) { save ctrl; jump back }
                    OpCode::TForLoop => {
                        let ra = base + i.arg_a();
                        if !matches!(state.get_at(ra + 4), LuaValue::Nil) {
                            let v = state.get_at(ra + 4).clone();
                            state.set_at(ra + 2, v);
                            pc = (pc as i64 - i.arg_bx() as i64) as u32;
                        }
                    }
                    // ── OP_SETLIST ─────────────────────────────────────────────
                    // C: n = GETARG_B; if n==0: n = top - ra - 1; last = C;
                    //    if TESTARG_k: last += Ax * (MAXARG_C+1); pc++;
                    //    for (; n > 0; n--) h->array[last-1] = val; luaC_barrierback
                    OpCode::SetList => {
                        let ra = base + i.arg_a();
                        let n_raw = i.arg_b();
                        let mut last = i.arg_c();
                        let t_val = state.get_at(ra).clone();
                        let n: i32 = if n_raw == 0 {
                            state.top_idx() - ra - 1
                        } else {
                            // C: L->top.p = ci->top.p — correct top
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
                        // C: if (last > luaH_realasize(h)) luaH_resizearray(L, h, last)
                        state.table_ensure_array(&t_val, last as usize)?;
                        // C: for (; n > 0; n--) { val = s2v(ra + n as i32); h->array[last-1] = *val; last--; }
                        for k in (1..=n).rev() {
                            let val = state.get_at(ra + k as i32).clone();
                            state.table_array_set(&t_val, (last - 1) as usize, val.clone())?;
                            last -= 1;
                            state.gc_barrier_back(&t_val, &val);
                        }
                    }
                    // ── OP_CLOSURE ─────────────────────────────────────────────
                    // C: Proto *p = cl->p->p[GETARG_Bx(i)];
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
                    // C: n = GETARG_C(i)-1; Protect(luaT_getvarargs(L, ci, ra, n));
                    OpCode::VarArg => {
                        let ra = base + i.arg_a();
                        let n = i.arg_c() as i32 - 1;
                        state.set_ci_savedpc(ci, pc);
                        state.set_top(state.ci_top(ci));
                        state.get_varargs(ci, ra, n)?;
                        trap = state.ci_trap(ci);
                    }
                    // ── OP_VARARGPREP ──────────────────────────────────────────
                    // C: ProtectNT(luaT_adjustvarargs(L, GETARG_A(i), ci, cl->p));
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
                    // C: lua_assert(0) — should never be executed directly
                    OpCode::ExtraArg => {
                        debug_assert!(false, "OP_EXTRAARG executed directly");
                    }
                    // Unknown opcode
                    #[allow(unreachable_patterns)]
                    _ => {
                        // TODO(port): unrecognised opcode {:?} — add to match
                        todo!("unrecognised opcode");
                    }
                } // end match opcode
            } // end 'dispatch loop

            // ── ret: label ──────────────────────────────────────────────────
            // C: ret: if (ci->callstatus & CIST_FRESH) return; else { ci = ci->previous; goto returning; }
            if state.ci_is_fresh(ci) {
                return Ok(()); // C: return; (end this fresh frame)
            } else {
                ci = state.ci_previous(ci).expect("ci_previous: not fresh frame must have previous");
                // C: goto returning — re-enter 'returning without resetting trap
                continue 'returning;
            }
        } // end 'returning loop
    } // end 'startfunc loop
}

// ─── Local opcode dispatch helpers ───────────────────────────────────────────

/// C: `op_arith_aux` — try both-int fast path then float fallback.
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

/// C: `op_arithf_aux` — float-only arithmetic (no integer path).
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
    // C: tonumberns(v1, n1) && tonumberns(v2, n2)
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

/// C: `op_arith_aux` with fallible integer op (mod / idiv).
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
        state.set_at(ra, LuaValue::Int(iop(*i1, *i2)?));
    } else {
        arith_float_aux(state, ra, v1, v2, pc, fop);
    }
    Ok(())
}

/// C: `op_bitwiseK` — bitwise op with one integer constant operand.
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

/// C: `op_bitwise` — bitwise op with two register operands.
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

/// C: `op_bitwise(L, luaV_shiftl)` / `op_bitwise(L, luaV_shiftr)`.
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

/// C: `op_orderI` — comparison with an immediate integer operand.
/// `inv = true` inverts the condition (for GTI/GEI which use the flipped TM).
/// `cl` is the current closure handle (opaque to this helper; only used for proto_code).
/// TODO(port): replace `LuaValue` stand-in for `cl` with proper `GcRef<LuaClosure>` once
///             the closure GC type lands in Phase B.
#[allow(dead_code)]
#[allow(clippy::too_many_arguments)]
fn order_imm_op(
    state: &mut LuaState,
    cl: &lua_types::GcRef<lua_types::LuaLClosure>,
    pc: &mut u32,
    trap: &mut bool,
    ci: CallInfoIdx,
    base: StackIdx,
    i: Instruction,
    opi: fn(i64, i64) -> bool,
    opf: fn(f64, f64) -> bool,
    inv: bool,
    tm: TagMethod,
) -> Result<(), LuaError> {
    let ra_v = state.get_at(base + i.arg_a()).clone();
    let im = i.arg_s_b() as i64;
    let cond: bool = match &ra_v {
        LuaValue::Int(ia) => opi(*ia, im),
        LuaValue::Float(fa) => opf(*fa, im as f64),
        _ => {
            // C: Protect(cond = luaT_callorderiTM(L, s2v(ra), im, inv, isf, tm))
            let isf = i.arg_c() != 0;
            state.set_ci_savedpc(ci, *pc);
            state.set_top(state.ci_top(ci));
            let r = state.call_order_i_tm(&ra_v, im, inv, isf, tm)?;
            *trap = state.ci_trap(ci);
            r
        }
    };
    // C: docondjump()
    if (cond as i32) != i.arg_k() {
        *pc += 1;
    } else {
        let next = state.proto_code(&cl, *pc);
        *pc = (*pc as i64 + next.arg_s_j() as i64 + 1) as u32;
        *trap = state.ci_trap(ci);
    }
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        src/lvm.c  (1899 lines, 32 functions)
//   target_crate:  lua-vm
//   confidence:    medium
//   todos:         6
//   port_notes:    4
//   unsafe_blocks: 0   (must be 0 outside lua-gc/lua-coro)
//   notes:         All opcode handlers and helpers translated; LuaState methods
//                  referenced (fast_get, precall, poscall, etc.) are stubs that
//                  Phase B will land.  The execute() goto flow is modelled with
//                  labelled Rust loops ('startfunc/'returning/'dispatch).
//                  str_to_number is a stub pending luaO_str2num port (TODO #1).
//                  strcoll replaced with byte-lexicographic order (TODO #2).
//                  order_imm_op uses LuaValue as a stand-in for GcRef<LuaClosure>
//                  (TODO #3).  ClosureRef type alias not yet defined (TODO #4-6).
// ──────────────────────────────────────────────────────────────────────────
