//! Generic functions over Lua objects.
//!
//! Ported from `reference/lua-5.4.7/src/lobject.c` (602 lines, ~20 functions).

// TODO(port): resolve import paths — all `crate::*` paths below are speculative;
// Phase B will reconcile against the actual module tree.
use crate::state::LuaState;
#[allow(unused_imports)] use crate::prelude::*;
use lua_types::{LuaValue, GcRef, LuaString, StackIdx};
use lua_types::error::LuaError;
use lua_types::arith::ArithOp;
use lua_types::tagmethod::TagMethod;
use lua_types::value::F2Imod;

// ──────────────────────────────────────────────────────────────────────────
// Module-level constants
// ──────────────────────────────────────────────────────────────────────────

/// Maximum number of significant hex digits to read (avoids overflow even for
/// single-precision floats).
/// C: `#define MAXSIGDIG 30`
const MAX_SIG_DIG: usize = 30;

/// Maximum length of a numeral string accepted for conversion to a number.
/// C: `#define L_MAXLENNUM 200`
const L_MAX_LEN_NUM: usize = 200;

/// Maximum size of a number-to-string conversion buffer.
/// Accommodates both `%.14g` float formatting and `%lld` integer formatting.
/// C: `#define MAXNUMBER2STR 44`
pub const MAX_NUMBER_2_STR: usize = 44;

/// Buffer size (bytes) for UTF-8 encoding; encoded backwards into this buffer.
/// C: `#define UTF8BUFFSZ 8`
pub const UTF8_BUF_SZ: usize = 8;

/// Maximum length of a chunk source identifier in error messages.
/// C: `LUA_IDSIZE` (typically 60 in luaconf.h).
// TODO(port): verify against luaconf.h; defaulting to 60 here.
pub const LUA_ID_SIZE: usize = 60;

/// Internal buffer size for `push_vfstring`.
/// C: `#define BUFVFS (LUA_IDSIZE + MAXNUMBER2STR + 95)`
const BUF_VFS: usize = LUA_ID_SIZE + MAX_NUMBER_2_STR + 95;

/// Truncation marker for long chunk source strings.
/// C: `#define RETS "..."`
const RETS: &[u8] = b"...";

/// Prefix for [string "..."] chunk identifiers.
/// C: `#define PRE "[string \""`
const PRE: &[u8] = b"[string \"";

/// Suffix for [string "..."] chunk identifiers.
/// C: `#define POS "\"]"`
const POS: &[u8] = b"\"]";

// ──────────────────────────────────────────────────────────────────────────
// ceil_log2
// ──────────────────────────────────────────────────────────────────────────

/// Computes `ceil(log2(x))`; returns the minimum `k` such that `2^k >= x`.
///
/// C: `int luaO_ceillog2 (unsigned int x)`
pub fn ceil_log2(x: u32) -> i32 {
    // C: static const lu_byte log_2[256] = { /* log_2[i] = ceil(log2(i - 1)) */ ... }
    static LOG_2: [u8; 256] = [
        0,1,2,2,3,3,3,3,4,4,4,4,4,4,4,4,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,
        6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,
        7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,
        7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,
        8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,
        8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,
        8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,
        8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,
    ];
    // C: int l = 0; x--; while (x >= 256) { l += 8; x >>= 8; } return l + log_2[x];
    let mut l: i32 = 0;
    let mut x = x.wrapping_sub(1);
    while x >= 256 {
        l += 8;
        x >>= 8;
    }
    l + LOG_2[x as usize] as i32
}

// ──────────────────────────────────────────────────────────────────────────
// Integer arithmetic dispatcher
// ──────────────────────────────────────────────────────────────────────────

/// Performs integer arithmetic for opcode `op` on operands `v1`, `v2`.
/// Returns `Result` because floor-mod and floor-div can raise on zero divisor.
///
/// C: `static lua_Integer intarith (lua_State *L, int op, lua_Integer v1, lua_Integer v2)`
fn int_arith(state: &mut LuaState, op: ArithOp, v1: i64, v2: i64) -> Result<i64, LuaError> {
    match op {
        // C: case LUA_OPADD: return intop(+, v1, v2);
        ArithOp::Add => Ok((v1 as u64).wrapping_add(v2 as u64) as i64),
        // C: case LUA_OPSUB: return intop(-, v1, v2);
        ArithOp::Sub => Ok((v1 as u64).wrapping_sub(v2 as u64) as i64),
        // C: case LUA_OPMUL: return intop(*, v1, v2);
        ArithOp::Mul => Ok((v1 as u64).wrapping_mul(v2 as u64) as i64),
        // C: case LUA_OPMOD: return luaV_mod(L, v1, v2);
        // TODO(port): confirm function path for integer floor-mod in lvm.rs
        ArithOp::Mod => crate::vm::int_floor_mod(state, v1, v2),
        // C: case LUA_OPIDIV: return luaV_idiv(L, v1, v2);
        // TODO(port): confirm function path for integer floor-div in lvm.rs
        ArithOp::Idiv => crate::vm::int_floor_div(state, v1, v2),
        // C: case LUA_OPBAND: return intop(&, v1, v2);
        ArithOp::Band => Ok(v1 & v2),
        // C: case LUA_OPBOR:  return intop(|, v1, v2);
        ArithOp::Bor => Ok(v1 | v2),
        // C: case LUA_OPBXOR: return intop(^, v1, v2);
        ArithOp::Bxor => Ok(v1 ^ v2),
        // C: case LUA_OPSHL: return luaV_shiftl(v1, v2);
        // TODO(port): confirm function path for shift-left in lvm.rs
        ArithOp::Shl => Ok(crate::vm::shiftl(v1, v2)),
        // C: case LUA_OPSHR: return luaV_shiftr(v1, v2);  [which is shiftl(v1, -v2)]
        ArithOp::Shr => Ok(crate::vm::shiftl(v1, -v2)),
        // C: case LUA_OPUNM: return intop(-, 0, v1);
        ArithOp::Unm => Ok((0u64).wrapping_sub(v1 as u64) as i64),
        // C: case LUA_OPBNOT: return intop(^, ~l_castS2U(0), v1);
        //    l_castS2U(0) → 0u64, ~0u64 = 0xFFFFFFFFFFFFFFFF = !0u64
        ArithOp::Bnot => Ok((!0u64 ^ v1 as u64) as i64),
        // C: default: lua_assert(0); return 0;
        _ => {
            debug_assert!(false, "int_arith called with non-integer op");
            Ok(0)
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Float arithmetic dispatcher
// ──────────────────────────────────────────────────────────────────────────

/// Performs float arithmetic for opcode `op` on operands `v1`, `v2`.
/// Returns `Result` because float floor-mod can raise on zero divisor.
///
/// C: `static lua_Number numarith (lua_State *L, int op, lua_Number v1, lua_Number v2)`
fn float_arith(state: &mut LuaState, op: ArithOp, v1: f64, v2: f64) -> Result<f64, LuaError> {
    match op {
        // C: case LUA_OPADD: return luai_numadd(L, v1, v2);
        ArithOp::Add => Ok(v1 + v2),
        // C: case LUA_OPSUB: return luai_numsub(L, v1, v2);
        ArithOp::Sub => Ok(v1 - v2),
        // C: case LUA_OPMUL: return luai_nummul(L, v1, v2);
        ArithOp::Mul => Ok(v1 * v2),
        // C: case LUA_OPDIV: return luai_numdiv(L, v1, v2);
        ArithOp::Div => Ok(v1 / v2),
        // C: case LUA_OPPOW: return luai_numpow(L, v1, v2);
        ArithOp::Pow => Ok(if v2 == 2.0 { v1 * v1 } else { v1.powf(v2) }),
        // C: case LUA_OPIDIV: return luai_numidiv(L, v1, v2);
        ArithOp::Idiv => Ok((v1 / v2).floor()),
        // C: case LUA_OPUNM: return luai_numunm(L, v1);
        ArithOp::Unm => Ok(-v1),
        // C: case LUA_OPMOD: return luaV_modf(L, v1, v2);
        // TODO(port): confirm function path for float floor-mod in lvm.rs
        ArithOp::Mod => crate::vm::float_floor_mod(state, v1, v2),
        // C: default: lua_assert(0); return 0;
        _ => {
            debug_assert!(false, "float_arith called with non-float op");
            Ok(0.0)
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Raw arithmetic (no metamethods)
// ──────────────────────────────────────────────────────────────────────────

/// Attempts raw (no-metamethod) arithmetic on two Lua values.
/// Writes the result to `res` and returns `true` on success, `false` if the
/// operation cannot be performed with the given types (caller should invoke
/// a metamethod instead).
///
/// C: `int luaO_rawarith (lua_State *L, int op, const TValue *p1, const TValue *p2, TValue *res)`
pub fn raw_arith(
    state: &mut LuaState,
    op: ArithOp,
    p1: &LuaValue,
    p2: &LuaValue,
    res: &mut LuaValue,
) -> Result<bool, LuaError> {
    match op {
        // C: case LUA_OPBAND: case LUA_OPBOR: case LUA_OPBXOR:
        // case LUA_OPSHL: case LUA_OPSHR: case LUA_OPBNOT: — integer-only ops
        ArithOp::Band | ArithOp::Bor | ArithOp::Bxor
        | ArithOp::Shl | ArithOp::Shr | ArithOp::Bnot => {
            // C: if (tointegerns(p1, &i1) && tointegerns(p2, &i2)) {
            //        setivalue(res, intarith(L, op, i1, i2));  return 1; }
            //    else return 0;
            if let (Some(i1), Some(i2)) = (
                p1.to_integer_no_strconv(),
                p2.to_integer_no_strconv(),
            ) {
                *res = LuaValue::Int(int_arith(state, op, i1, i2)?);
                Ok(true)
            } else {
                Ok(false)
            }
        }

        // C: case LUA_OPDIV: case LUA_OPPOW: — float-only ops
        ArithOp::Div | ArithOp::Pow => {
            // C: if (tonumberns(p1, n1) && tonumberns(p2, n2)) {
            //        setfltvalue(res, numarith(L, op, n1, n2));  return 1; }
            //    else return 0;
            if let (Some(n1), Some(n2)) = (
                p1.to_number_no_strconv(),
                p2.to_number_no_strconv(),
            ) {
                *res = LuaValue::Float(float_arith(state, op, n1, n2)?);
                Ok(true)
            } else {
                Ok(false)
            }
        }

        // C: default: — prefer integer if both operands are integers; else try float.
        _ => {
            // C: if (ttisinteger(p1) && ttisinteger(p2)) {
            //        setivalue(res, intarith(L, op, ivalue(p1), ivalue(p2)));  return 1; }
            if let (LuaValue::Int(i1), LuaValue::Int(i2)) = (p1, p2) {
                *res = LuaValue::Int(int_arith(state, op, *i1, *i2)?);
                return Ok(true);
            }
            // C: else if (tonumberns(p1, n1) && tonumberns(p2, n2)) { ... }
            if let (Some(n1), Some(n2)) = (
                p1.to_number_no_strconv(),
                p2.to_number_no_strconv(),
            ) {
                *res = LuaValue::Float(float_arith(state, op, n1, n2)?);
                Ok(true)
            } else {
                // C: else return 0;
                Ok(false)
            }
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Arithmetic (with metamethod fallback)
// ──────────────────────────────────────────────────────────────────────────

/// Performs arithmetic for opcode `op`, writing the result to the stack slot
/// `res`.  Falls back to a binary tag-method if raw arithmetic is not possible.
///
/// C: `void luaO_arith (lua_State *L, int op, const TValue *p1, const TValue *p2, StkId res)`
pub fn arith(
    state: &mut LuaState,
    op: ArithOp,
    p1: &LuaValue,
    p2: &LuaValue,
    res: StackIdx,
) -> Result<(), LuaError> {
    // C: if (!luaO_rawarith(L, op, p1, p2, s2v(res))) {
    //        luaT_trybinTM(L, p1, p2, res, cast(TMS, (op - LUA_OPADD) + TM_ADD)); }
    //
    // PORT NOTE: raw_arith writes to a local `temp` first; we then set the stack
    // slot.  This avoids holding a &mut borrow into the stack across try_bin_tm,
    // which would violate the StackIdx rule (PORTING.md §2 #5).
    let mut temp = LuaValue::Nil;
    if raw_arith(state, op, p1, p2, &mut temp)? {
        state.set_at(res, temp);
    } else {
        // TODO(phase-b): `cast(TMS, (op - LUA_OPADD) + TM_ADD)` — needs a real
        // ArithOp→TagMethod conversion helper in lua-types::tagmethod.
        let _tm: TagMethod = todo!("phase-b: TagMethod::from_arith_op");
        let _ = state;
        let _ = (p1, p2);
    }
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────
// hex_value
// ──────────────────────────────────────────────────────────────────────────

/// Converts a hexadecimal digit byte to its numeric value (0–15).
/// Caller must ensure `c` is a valid hex digit.
///
/// C: `int luaO_hexavalue (int c)`
pub fn hex_value(c: u8) -> u8 {
    // C: if (lisdigit(c)) return c - '0'; else return (ltolower(c) - 'a') + 10;
    if c.is_ascii_digit() {
        c - b'0'
    } else {
        c.to_ascii_lowercase() - b'a' + 10
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Sign helper
// ──────────────────────────────────────────────────────────────────────────

/// Checks for and consumes a leading sign byte (`+` or `-`) in `s` starting
/// at `*idx`.  Returns `true` if a minus sign was consumed.
///
/// C: `static int isneg (const char **s)`
fn is_neg(s: &[u8], idx: &mut usize) -> bool {
    // C: if (**s == '-') { (*s)++; return 1; }
    //    else if (**s == '+') (*s)++;
    //    return 0;
    if *idx < s.len() && s[*idx] == b'-' {
        *idx += 1;
        return true;
    }
    if *idx < s.len() && s[*idx] == b'+' {
        *idx += 1;
    }
    false
}

// ──────────────────────────────────────────────────────────────────────────
// Hexadecimal float parser
// ──────────────────────────────────────────────────────────────────────────

/// Converts a hexadecimal float literal (C99 `0x…p…` form) in `s` to `f64`.
/// Returns `Some((value, end_index))` on success, `None` on failure.
///
/// C: `static lua_Number lua_strx2number (const char *s, char **endptr)`
/// (conditionally compiled when the platform doesn't provide it)
fn str_x2number(s: &[u8]) -> Option<(f64, usize)> {
    let mut idx = 0;
    // C: while (lisspace(cast_uchar(*s))) s++;  — skip leading spaces
    while idx < s.len() && s[idx].is_ascii_whitespace() {
        idx += 1;
    }
    // C: neg = isneg(&s);
    let neg = is_neg(s, &mut idx);
    // C: if (!(*s == '0' && (*(s + 1) == 'x' || *(s + 1) == 'X'))) return 0.0;
    if idx + 1 >= s.len() || s[idx] != b'0' || (s[idx + 1] != b'x' && s[idx + 1] != b'X') {
        return None;
    }
    // C: for (s += 2; ; s++) { ... }  — skip '0x' and read mantissa digits
    idx += 2;
    let mut r: f64 = 0.0;
    let mut sigdig: usize = 0;
    let mut nosigdig: usize = 0;
    let mut e: i32 = 0;
    let mut hasdot = false;

    // PORT NOTE: `lua_getlocaledecpoint()` returns the locale decimal separator.
    // Rust has no locale; we always treat '.' as the separator here.
    let dot = b'.';

    loop {
        if idx >= s.len() {
            break;
        }
        let ch = s[idx];
        if ch == dot {
            // C: if (hasdot) break; else hasdot = 1;
            if hasdot {
                break;
            }
            hasdot = true;
        } else if ch.is_ascii_hexdigit() {
            // C: if (sigdig == 0 && *s == '0') nosigdig++;
            //    else if (++sigdig <= MAXSIGDIG) r = (r * 16.0) + luaO_hexavalue(*s);
            //    else e++;
            //    if (hasdot) e--;
            if sigdig == 0 && ch == b'0' {
                nosigdig += 1;
            } else if {
                sigdig += 1;
                sigdig <= MAX_SIG_DIG
            } {
                r = r * 16.0 + hex_value(ch) as f64;
            } else {
                e += 1;
            }
            if hasdot {
                e -= 1;
            }
        } else {
            break;
        }
        idx += 1;
    }

    // C: if (nosigdig + sigdig == 0) return 0.0;  — no digits at all
    if nosigdig + sigdig == 0 {
        return None;
    }
    // `idx` is now the valid end so far
    let valid_end = idx;
    // C: e *= 4;  — each hex digit is 4 bits
    e *= 4;

    // C: if (*s == 'p' || *s == 'P') { ... read exponent ... }
    if idx < s.len() && (s[idx] == b'p' || s[idx] == b'P') {
        idx += 1; // skip 'p'/'P'
        let neg1 = is_neg(s, &mut idx);
        // C: if (!lisdigit(cast_uchar(*s))) return 0.0;
        if idx >= s.len() || !s[idx].is_ascii_digit() {
            return None;
        }
        let mut exp1: i32 = 0;
        // C: while (lisdigit(cast_uchar(*s))) exp1 = exp1 * 10 + *(s++) - '0';
        while idx < s.len() && s[idx].is_ascii_digit() {
            exp1 = exp1 * 10 + (s[idx] - b'0') as i32;
            idx += 1;
        }
        if neg1 {
            exp1 = -exp1;
        }
        e += exp1;
        // update valid end: the exponent consumed up to here
        // (valid_end is updated to idx below)
    }
    // C: if (neg) r = -r;
    // C: return l_mathop(ldexp)(r, e);
    let result = if neg { -r } else { r };
    Some((result * (2.0f64).powi(e), idx))
}

// ──────────────────────────────────────────────────────────────────────────
// String-to-float helpers
// ──────────────────────────────────────────────────────────────────────────

/// Inner conversion: tries to parse the bytes `s` as a float using the given
/// `mode` (`b'x'` for hex, anything else for decimal).
/// Returns `Some((value, end_index))` or `None`.
///
/// C: `static const char *l_str2dloc (const char *s, lua_Number *result, int mode)`
fn str2dloc(s: &[u8], mode: u8) -> Option<(f64, usize)> {
    // C: *result = (mode == 'x') ? lua_strx2number(s, &endptr) : lua_str2number(s, &endptr);
    let (result, end) = if mode == b'x' {
        str_x2number(s)?
    } else {
        // C: lua_str2number(s, &endptr)  — essentially strtod.
        // PORT NOTE: from_utf8 used here because numeric string literals are
        // guaranteed to be ASCII (a strict subset of UTF-8).
        // TODO(port): replace with a bytes-native float parser in Phase B
        // (e.g., the `fast-float` crate) to satisfy the from_utf8 ban fully.
        let text = core::str::from_utf8(s).ok()?;
        let trimmed = text.trim();
        // Reject "inf", "infinity", "nan" — Lua does not accept these.
        let lower = trimmed.to_ascii_lowercase();
        if lower.starts_with("inf") || lower.starts_with("nan") {
            return None;
        }
        let f: f64 = trimmed.parse().ok()?;
        (f, s.len()) // strtod parses as many chars as possible; we consumed all
    };
    // C: if (endptr == s) return NULL;  — nothing recognized
    if end == 0 {
        return None;
    }
    // C: while (lisspace(cast_uchar(*endptr))) endptr++;
    let mut end2 = end;
    while end2 < s.len() && s[end2].is_ascii_whitespace() {
        end2 += 1;
    }
    // C: return (*endptr == '\0') ? endptr : NULL;  — OK iff no trailing chars
    if end2 == s.len() {
        Some((result, end2))
    } else {
        None
    }
}

/// Converts bytes `s` to a Lua float value.
/// Returns `Some((value, end_index))` on success, `None` on failure.
///
/// C: `static const char *l_str2d (const char *s, lua_Number *result)`
fn str2d(s: &[u8]) -> Option<(f64, usize)> {
    // C: const char *pmode = strpbrk(s, ".xXnN");
    //    int mode = pmode ? ltolower(cast_uchar(*pmode)) : 0;
    let pmode = s.iter().position(|&b| {
        b == b'.' || b == b'x' || b == b'X' || b == b'n' || b == b'N'
    });
    let mode = pmode.map(|i| s[i].to_ascii_lowercase()).unwrap_or(0);

    // C: if (mode == 'n') return NULL;  — reject 'inf' and 'nan'
    if mode == b'n' {
        return None;
    }

    // C: endptr = l_str2dloc(s, result, mode);
    if let Some(result) = str2dloc(s, mode) {
        return Some(result);
    }

    // C: if (endptr == NULL) { ... try replacing '.' with locale decimal point ... }
    // PORT NOTE: Lua retries by replacing '.' with the locale decimal separator.
    // Rust has no locale support; we skip this retry path and always use '.'.
    // TODO(port): add locale retry if locale-aware float parsing is needed.

    None
}

// ──────────────────────────────────────────────────────────────────────────
// String-to-integer helper
// ──────────────────────────────────────────────────────────────────────────

/// Converts bytes `s` to a Lua integer value (decimal or `0x` hex).
/// Returns `Some(value)` on success (the entire byte slice was consumed),
/// `None` on failure or overflow.
///
/// C: `static const char *l_str2int (const char *s, lua_Integer *result)`
fn str2int(s: &[u8]) -> Option<i64> {
    let mut idx = 0;
    // C: while (lisspace(cast_uchar(*s))) s++;
    while idx < s.len() && s[idx].is_ascii_whitespace() {
        idx += 1;
    }
    // C: neg = isneg(&s);
    let neg = is_neg(s, &mut idx);

    let mut a: u64 = 0;
    let mut empty = true;

    if idx + 1 < s.len() && s[idx] == b'0' && (s[idx + 1] == b'x' || s[idx + 1] == b'X') {
        // C: s += 2; for (; lisxdigit(cast_uchar(*s)); s++) { a = a * 16 + ...; empty = 0; }
        idx += 2;
        while idx < s.len() && s[idx].is_ascii_hexdigit() {
            a = a.wrapping_mul(16).wrapping_add(hex_value(s[idx]) as u64);
            empty = false;
            idx += 1;
        }
    } else {
        // C: decimal loop with overflow check:
        //    MAXBY10 = cast(lua_Unsigned, LUA_MAXINTEGER / 10)
        //    MAXLASTD = cast_int(LUA_MAXINTEGER % 10)
        //    if (a >= MAXBY10 && (a > MAXBY10 || d > MAXLASTD + neg)) return NULL;
        const MAX_BY10: u64 = (i64::MAX / 10) as u64;
        const MAX_LAST_D: u64 = (i64::MAX % 10) as u64;
        while idx < s.len() && s[idx].is_ascii_digit() {
            let d = (s[idx] - b'0') as u64;
            if a >= MAX_BY10 && (a > MAX_BY10 || d > MAX_LAST_D + if neg { 1 } else { 0 }) {
                return None; // overflow
            }
            a = a.wrapping_mul(10).wrapping_add(d);
            empty = false;
            idx += 1;
        }
    }

    // C: while (lisspace(cast_uchar(*s))) s++;
    while idx < s.len() && s[idx].is_ascii_whitespace() {
        idx += 1;
    }
    // C: if (empty || *s != '\0') return NULL;
    if empty || idx != s.len() {
        return None;
    }
    // C: *result = l_castU2S((neg) ? 0u - a : a);
    let result = if neg { (0u64).wrapping_sub(a) as i64 } else { a as i64 };
    Some(result)
}

// ──────────────────────────────────────────────────────────────────────────
// str2num — main public string-to-number conversion
// ──────────────────────────────────────────────────────────────────────────

/// Tries to convert the byte string `s` to a Lua number (integer first, then
/// float).  Writes the result to `o` and returns `consumed_bytes + 1` on
/// success (matching the C convention of including the null terminator in the
/// count), or `0` on failure.
///
/// C: `size_t luaO_str2num (const char *s, TValue *o)`
pub fn str2num(s: &[u8], o: &mut LuaValue) -> usize {
    // C: if ((e = l_str2int(s, &i)) != NULL) { setivalue(o, i); }
    if let Some(i) = str2int(s) {
        *o = LuaValue::Int(i);
        return s.len() + 1; // entire string consumed; +1 for C null-terminator convention
    }
    // C: else if ((e = l_str2d(s, &n)) != NULL) { setfltvalue(o, n); }
    if let Some((n, end)) = str2d(s) {
        *o = LuaValue::Float(n);
        return end + 1;
    }
    // C: else return 0;
    0
}

// ──────────────────────────────────────────────────────────────────────────
// UTF-8 encoder
// ──────────────────────────────────────────────────────────────────────────

/// Encodes Unicode codepoint `x` as UTF-8 into `buff` (filled backwards from
/// index `UTF8_BUF_SZ - 1`).  Returns the number of bytes written.
/// The valid bytes occupy `buff[UTF8_BUF_SZ - n .. UTF8_BUF_SZ]`.
///
/// C: `int luaO_utf8esc (char *buff, unsigned long x)`
pub fn utf8_esc(buff: &mut [u8; UTF8_BUF_SZ], x: u32) -> usize {
    // C: lua_assert(x <= 0x7FFFFFFFu);
    debug_assert!(x <= 0x7FFF_FFFF, "codepoint out of range");
    let mut n: usize = 1;
    if x < 0x80 {
        // C: buff[UTF8BUFFSZ - 1] = cast_char(x);
        buff[UTF8_BUF_SZ - 1] = x as u8;
    } else {
        // C: unsigned int mfb = 0x3f;  — max-fits-in-first-byte mask
        let mut mfb: u32 = 0x3f;
        let mut x = x;
        loop {
            // C: buff[UTF8BUFFSZ - (n++)] = cast_char(0x80 | (x & 0x3f));
            buff[UTF8_BUF_SZ - n] = 0x80 | (x & 0x3f) as u8;
            n += 1;
            x >>= 6;
            mfb >>= 1;
            // C: while (x > mfb);
            if x <= mfb {
                break;
            }
        }
        // C: buff[UTF8BUFFSZ - n] = cast_char((~mfb << 1) | x);
        buff[UTF8_BUF_SZ - n] = ((!mfb << 1) | x) as u8;
    }
    n
}

// ──────────────────────────────────────────────────────────────────────────
// Number → string conversion
// ──────────────────────────────────────────────────────────────────────────

/// Formats `f` as C's `printf("%.14g", f)` would, returning the bytes.
///
/// PORT NOTE: Rust has no built-in `%g` format. This replicates the C99
/// `%g` algorithm with precision 14: pick scientific or fixed-point based
/// on the value's exponent, strip trailing zeros, normalize the exponent
/// to `e[+-]NN` with at least two digits (matching C's output).
fn fmt_g14(f: f64) -> Vec<u8> {
    if f.is_nan() {
        return b"nan".to_vec();
    }
    if f.is_infinite() {
        return if f > 0.0 { b"inf".to_vec() } else { b"-inf".to_vec() };
    }
    if f == 0.0 {
        return if f.is_sign_negative() { b"-0".to_vec() } else { b"0".to_vec() };
    }

    let precision: i32 = 14;
    let abs = f.abs();
    let exp = abs.log10().floor() as i32;

    let s = if exp < -4 || exp >= precision {
        let mantissa_decimals = (precision - 1) as usize;
        let raw = format!("{:.*e}", mantissa_decimals, f);
        let e_idx = raw.find('e').expect("Rust scientific format always contains 'e'");
        let mantissa = strip_fixed_trailing_zeros(&raw[..e_idx]);
        let exp_num: i32 = raw[e_idx + 1..].parse().expect("Rust formats integer exponents");
        let sign = if exp_num < 0 { '-' } else { '+' };
        let abs_exp = exp_num.abs();
        if abs_exp < 10 {
            format!("{}e{}0{}", mantissa, sign, abs_exp)
        } else {
            format!("{}e{}{}", mantissa, sign, abs_exp)
        }
    } else {
        let decimals = (precision - 1 - exp).max(0) as usize;
        let raw = format!("{:.*}", decimals, f);
        strip_fixed_trailing_zeros(&raw)
    };

    s.into_bytes()
}

fn strip_fixed_trailing_zeros(s: &str) -> String {
    if !s.contains('.') {
        return s.to_string();
    }
    let mut out = s.to_string();
    while out.ends_with('0') {
        out.pop();
    }
    if out.ends_with('.') {
        out.pop();
    }
    out
}

/// Formats the numeric `LuaValue` `val` (must be Int or Float) into a byte
/// buffer and returns it.
///
/// C: `static int tostringbuff (TValue *obj, char *buff)`
fn number_to_str_buf(val: &LuaValue) -> Vec<u8> {
    // C: lua_assert(ttisnumber(obj));
    debug_assert!(
        matches!(val, LuaValue::Int(_) | LuaValue::Float(_)),
        "number_to_str_buf: value is not a number"
    );

    match val {
        LuaValue::Int(i) => {
            // C: len = lua_integer2str(buff, MAXNUMBER2STR, ivalue(obj));
            // lua_integer2str → l_sprintf with LUA_INTEGER_FMT ("%lld")
            // PORT NOTE: using Rust's default i64 Display formatting, which
            // matches C's `%lld` for all values in [i64::MIN, i64::MAX].
            let s = format!("{}", i);
            s.into_bytes()
        }
        LuaValue::Float(f) => {
            let mut bytes = fmt_g14(*f);

            let looks_like_int = bytes.iter().all(|&b| b == b'-' || b.is_ascii_digit());
            if looks_like_int {
                bytes.push(b'.');
                bytes.push(b'0');
            }
            bytes
        }
        // Unreachable — guarded by debug_assert above.
        _ => Vec::new(),
    }
}

/// Converts a numeric `LuaValue` to an interned `LuaString`, returning a
/// `GcRef<LuaString>` handle.  Callers are responsible for updating the
/// `LuaValue` (or stack slot) with `LuaValue::Str(s)`.
///
/// C: `void luaO_tostring (lua_State *L, TValue *obj)` which modifies `obj`
/// in place; in Rust we return the string because holding `&mut LuaValue`
/// across a `state.intern_str` call would borrow `state` twice.
pub fn num_to_string(state: &mut LuaState, val: &LuaValue) -> Result<GcRef<LuaString>, LuaError> {
    // C: char buff[MAXNUMBER2STR];
    //    int len = tostringbuff(obj, buff);
    //    setsvalue(L, obj, luaS_newlstr(L, buff, len));
    let bytes = number_to_str_buf(val);
    // TODO(port): state.intern_str path needs to be confirmed in lua-vm
    state.intern_str(&bytes)
}

// ──────────────────────────────────────────────────────────────────────────
// push_vfstring infrastructure
// ──────────────────────────────────────────────────────────────────────────

/// Typed format argument for `push_vfstring`.
///
/// PORT NOTE: replaces the C `va_list` variadic interface.  C callers of
/// `luaO_pushfstring(L, fmt, ...)` must be updated to pass structured
/// `FmtArg` slices.  The format-string scanning logic is preserved in
/// `push_vfstring`; only the argument-list type changes.
pub enum FmtArg<'a> {
    /// `%s` — a byte string (replaces `const char *` from va_list).
    Str(&'a [u8]),
    /// `%c` — a single byte character.
    Char(u8),
    /// `%d` — a 32-bit integer.
    Int(i32),
    /// `%I` — a Lua integer (i64).
    LuaInt(i64),
    /// `%f` — a Lua float (f64).
    Float(f64),
    /// `%U` — a Unicode codepoint (u32), encoded as UTF-8.
    Utf8Codepoint(u32),
    // TODO(port): %p (pointer) omitted — raw pointer in safe Rust is not allowed
    // outside lua-gc/lua-coro.  Callers that need pointer formatting must handle
    // it separately and pass the pre-formatted bytes as FmtArg::Str.
}

/// Internal accumulator for `push_vfstring`.
///
/// C: `typedef struct BuffFS { lua_State *L; int pushed; int blen; char space[BUFVFS]; } BuffFS;`
///
/// PORT NOTE: `space` is a `Vec<u8>` rather than a fixed-size array; the
/// BUF_VFS threshold is still respected for flushing behaviour.
struct BufFs {
    /// Whether at least one partial result has been pushed onto the stack.
    pushed: bool,
    /// Accumulated bytes not yet pushed to the stack.
    space: Vec<u8>,
}

impl BufFs {
    fn new() -> Self {
        BufFs {
            pushed: false,
            space: Vec::with_capacity(BUF_VFS),
        }
    }
}

/// Pushes the byte string `str_bytes` to the Lua stack and concatenates with
/// any prior partial result.
///
/// C: `static void pushstr (BuffFS *buff, const char *str, size_t lstr)`
fn pushstr(buf: &mut BufFs, state: &mut LuaState, str_bytes: &[u8]) -> Result<(), LuaError> {
    // C: setsvalue2s(L, L->top.p, luaS_newlstr(L, str, lstr));
    //    L->top.p++;
    //    if (!buff->pushed) buff->pushed = 1;
    //    else luaV_concat(L, 2);
    let s = state.intern_str(str_bytes)?;
    state.push(LuaValue::Str(s));
    if !buf.pushed {
        buf.pushed = true;
    } else {
        // C: luaV_concat(L, 2);
        // TODO(port): confirm path to string concatenation helper in lvm.rs
        crate::vm::concat(state, 2)?;
    }
    Ok(())
}

/// Flushes the internal buffer to the Lua stack.
///
/// C: `static void clearbuff (BuffFS *buff)`
fn clearbuff(buf: &mut BufFs, state: &mut LuaState) -> Result<(), LuaError> {
    // C: pushstr(buff, buff->space, buff->blen); buff->blen = 0;
    let bytes: Vec<u8> = buf.space.drain(..).collect();
    pushstr(buf, state, &bytes)
}

/// Adds `str_bytes` to the internal buffer, flushing first if it won't fit.
///
/// C: `static void addstr2buff (BuffFS *buff, const char *str, size_t slen)`
fn addstr2buff(buf: &mut BufFs, state: &mut LuaState, str_bytes: &[u8]) -> Result<(), LuaError> {
    // C: if (slen <= BUFVFS) { ... memcpy ... addsize(buff, slen); }
    //    else { clearbuff; pushstr directly; }
    if str_bytes.len() <= BUF_VFS {
        // C: if (sz > BUFVFS - buff->blen) clearbuff(buff);
        if str_bytes.len() > BUF_VFS - buf.space.len() {
            clearbuff(buf, state)?;
        }
        buf.space.extend_from_slice(str_bytes);
    } else {
        clearbuff(buf, state)?;
        pushstr(buf, state, str_bytes)?;
    }
    Ok(())
}

/// Formats the numeric value `num` and appends it to the buffer.
///
/// C: `static void addnum2buff (BuffFS *buff, TValue *num)`
fn addnum2buff(buf: &mut BufFs, state: &mut LuaState, num: &LuaValue) -> Result<(), LuaError> {
    // C: char *numbuff = getbuff(buff, MAXNUMBER2STR);
    //    int len = tostringbuff(num, numbuff);
    //    addsize(buff, len);
    let bytes = number_to_str_buf(num);
    addstr2buff(buf, state, &bytes)
}

// ──────────────────────────────────────────────────────────────────────────
// push_vfstring / push_fstring
// ──────────────────────────────────────────────────────────────────────────

/// Builds a formatted Lua string from a format byte string and structured
/// arguments, pushes it onto the stack, and returns the top-of-stack value.
///
/// Supported format specifiers (same subset as C's `luaO_pushvfstring`):
/// `%s`, `%c`, `%d`, `%I`, `%f`, `%U`, `%%`.
/// `%p` is **not** supported; see [`FmtArg`] documentation.
///
/// C: `const char *luaO_pushvfstring (lua_State *L, const char *fmt, va_list argp)`
///
/// PORT NOTE: `va_list` replaced by `&[FmtArg]`.  Call sites that previously
/// passed variadic arguments must be updated to build a `&[FmtArg]` slice.
pub fn push_vfstring<'a>(
    state: &mut LuaState,
    fmt: &[u8],
    args: &[FmtArg<'a>],
) -> Result<GcRef<LuaString>, LuaError> {
    let mut buf = BufFs::new();
    let mut arg_idx = 0usize;
    let mut pos = 0usize;

    // C: while ((e = strchr(fmt, '%')) != NULL) { ... }
    while let Some(rel) = fmt[pos..].iter().position(|&b| b == b'%') {
        let e = pos + rel;
        // C: addstr2buff(&buff, fmt, e - fmt);
        addstr2buff(&mut buf, state, &fmt[pos..e])?;

        // C: switch (*(e + 1)) { ... }
        let spec = if e + 1 < fmt.len() { fmt[e + 1] } else { 0 };
        match spec {
            b's' => {
                // C: const char *s = va_arg(argp, char *); if (!s) s = "(null)";
                //    addstr2buff(&buff, s, strlen(s));
                let s = match args.get(arg_idx) {
                    Some(FmtArg::Str(b)) => *b,
                    None => b"(null)",
                    _ => b"(null)",
                };
                arg_idx += 1;
                addstr2buff(&mut buf, state, s)?;
            }
            b'c' => {
                // C: char c = cast_uchar(va_arg(argp, int));
                //    addstr2buff(&buff, &c, sizeof(char));
                let c = match args.get(arg_idx) {
                    Some(FmtArg::Char(b)) => *b,
                    _ => b'?',
                };
                arg_idx += 1;
                addstr2buff(&mut buf, state, &[c])?;
            }
            b'd' => {
                // C: TValue num; setivalue(&num, va_arg(argp, int)); addnum2buff(&buff, &num);
                let n = match args.get(arg_idx) {
                    Some(FmtArg::Int(i)) => *i as i64,
                    _ => 0,
                };
                arg_idx += 1;
                addnum2buff(&mut buf, state, &LuaValue::Int(n))?;
            }
            b'I' => {
                // C: TValue num; setivalue(&num, cast(lua_Integer, va_arg(argp, l_uacInt)));
                //    addnum2buff(&buff, &num);
                let n = match args.get(arg_idx) {
                    Some(FmtArg::LuaInt(i)) => *i,
                    _ => 0,
                };
                arg_idx += 1;
                addnum2buff(&mut buf, state, &LuaValue::Int(n))?;
            }
            b'f' => {
                // C: TValue num; setfltvalue(&num, cast_num(va_arg(argp, l_uacNumber)));
                //    addnum2buff(&buff, &num);
                let f = match args.get(arg_idx) {
                    Some(FmtArg::Float(f)) => *f,
                    _ => 0.0,
                };
                arg_idx += 1;
                addnum2buff(&mut buf, state, &LuaValue::Float(f))?;
            }
            b'p' => {
                // C: void *p = va_arg(argp, void *); int len = lua_pointer2str(bf, sz, p);
                // TODO(port): %p pointer formatting not implemented in safe Rust;
                // callers that need it should pre-format the pointer and pass FmtArg::Str.
                arg_idx += 1; // consume the argument slot
                addstr2buff(&mut buf, state, b"<ptr>")?;
            }
            b'U' => {
                // C: char bf[UTF8BUFFSZ]; int len = luaO_utf8esc(bf, va_arg(argp, long));
                //    addstr2buff(&buff, bf + UTF8BUFFSZ - len, len);
                let cp = match args.get(arg_idx) {
                    Some(FmtArg::Utf8Codepoint(u)) => *u,
                    _ => b'?' as u32,
                };
                arg_idx += 1;
                let mut bf = [0u8; UTF8_BUF_SZ];
                let n = utf8_esc(&mut bf, cp);
                addstr2buff(&mut buf, state, &bf[UTF8_BUF_SZ - n..])?;
            }
            b'%' => {
                // C: addstr2buff(&buff, "%", 1);
                addstr2buff(&mut buf, state, b"%")?;
            }
            other => {
                // C: luaG_runerror(L, "invalid option '%%%c' to 'lua_pushfstring'", *(e + 1));
                return Err(LuaError::runtime(format_args!(
                    "invalid option '%%{}' to 'lua_pushfstring'",
                    other as char
                )));
            }
        }
        // C: fmt = e + 2;  — skip '%' and the specifier
        pos = e + 2;
    }

    // C: addstr2buff(&buff, fmt, strlen(fmt));  — rest of format string
    addstr2buff(&mut buf, state, &fmt[pos..])?;
    // C: clearbuff(&buff);
    clearbuff(&mut buf, state)?;
    // C: lua_assert(buff.pushed == 1);
    debug_assert!(buf.pushed, "push_vfstring: no string was pushed");

    // C: return getstr(tsvalue(s2v(L->top.p - 1)));
    // Return the interned string at the top of the stack.
    // PORT NOTE: in C this returns a `const char *` into the TString; in Rust
    // we return the GcRef<LuaString> directly.
    // TODO(port): state.peek_string_at_top() path needs to be confirmed.
    Ok(state.peek_string_at_top())
}

/// Variadic entry point; delegates to `push_vfstring`.
///
/// C: `const char *luaO_pushfstring (lua_State *L, const char *fmt, ...)`
///
/// PORT NOTE: callers that previously used `luaO_pushfstring` for error
/// messages should collapse the call into `LuaError::runtime(format_args!(...))`;
/// see PORTING.md §4.2 and error_sites.tsv.
pub fn push_fstring<'a>(
    state: &mut LuaState,
    fmt: &[u8],
    args: &[FmtArg<'a>],
) -> Result<GcRef<LuaString>, LuaError> {
    // C: va_start(argp, fmt); msg = luaO_pushvfstring(L, fmt, argp); va_end(argp);
    push_vfstring(state, fmt, args)
}

// ──────────────────────────────────────────────────────────────────────────
// chunk_id — human-readable chunk identifier
// ──────────────────────────────────────────────────────────────────────────

/// Fills `out` with a human-readable identifier derived from `source` and
/// returns the number of bytes written (not including any null terminator).
///
/// Rules (matching C):
/// - `=...`  → literal text (everything after `=`), truncated to `LUA_ID_SIZE - 1`.
/// - `@...`  → file name (everything after `@`), prefixed with `...` if too long.
/// - anything else → `[string "..."]`, with the first line truncated.
///
/// C: `void luaO_chunkid (char *out, const char *source, size_t srclen)`
pub fn chunk_id(out: &mut [u8], source: &[u8]) -> usize {
    let bufflen = LUA_ID_SIZE;
    let mut written = 0usize;

    // Helper: copy bytes into `out` at `written`, advance `written`.
    let mut write_bytes = |out: &mut [u8], written: &mut usize, bytes: &[u8]| {
        let avail = out.len().saturating_sub(*written);
        let n = bytes.len().min(avail);
        out[*written..*written + n].copy_from_slice(&bytes[..n]);
        *written += n;
    };

    if source.is_empty() {
        write_bytes(out, &mut written, b"?");
        return written;
    }

    match source[0] {
        b'=' => {
            // C: if (srclen <= bufflen) memcpy(out, source + 1, srclen);
            //    else { addstr(out, source + 1, bufflen - 1); *out = '\0'; }
            let body = &source[1..];
            if body.len() <= bufflen {
                write_bytes(out, &mut written, body);
            } else {
                // truncate
                write_bytes(out, &mut written, &body[..bufflen - 1]);
                // C: *out = '\0' — null-terminate at the truncation point
                if written < out.len() {
                    out[written] = 0;
                }
            }
        }
        b'@' => {
            // C: if (srclen <= bufflen) memcpy(out, source + 1, srclen);
            //    else { addstr(out, RETS, LL(RETS)); bufflen -= ...; memcpy(out, ...) }
            let body = &source[1..];
            if body.len() <= bufflen {
                write_bytes(out, &mut written, body);
            } else {
                // add "..." prefix then the tail of the filename
                write_bytes(out, &mut written, RETS);
                let remaining = bufflen - RETS.len();
                let tail_start = body.len().saturating_sub(remaining);
                write_bytes(out, &mut written, &body[tail_start..]);
            }
        }
        _ => {
            // C: string source; format as [string "source"]
            // C: const char *nl = strchr(source, '\n');
            let nl_pos = source.iter().position(|&b| b == b'\n');
            // C: addstr(out, PRE, LL(PRE));
            write_bytes(out, &mut written, PRE);
            // C: bufflen -= LL(PRE RETS POS) + 1;
            let reserved = PRE.len() + RETS.len() + POS.len() + 1;
            let inner_limit = bufflen.saturating_sub(reserved);

            // C: if (srclen < bufflen && nl == NULL) addstr(out, source, srclen);
            let src_len = source.len();
            if src_len <= inner_limit && nl_pos.is_none() {
                write_bytes(out, &mut written, source);
            } else {
                // C: if (nl != NULL) srclen = nl - source;
                //    if (srclen > bufflen) srclen = bufflen;
                //    addstr(out, source, srclen); addstr(out, RETS, LL(RETS));
                let take = nl_pos.unwrap_or(src_len).min(inner_limit);
                write_bytes(out, &mut written, &source[..take]);
                write_bytes(out, &mut written, RETS);
            }
            // C: memcpy(out, POS, (LL(POS) + 1) * sizeof(char));
            write_bytes(out, &mut written, POS);
        }
    }

    written
}

// ──────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        src/lobject.c  (602 lines, ~20 functions)
//   target_crate:  lua-vm
//   confidence:    medium
//   todos:         15
//   port_notes:    12
//   unsafe_blocks: 0
//   notes:         All import paths are speculative (crate::state, lua_types::*);
//                  Phase B must reconcile.  va_list replaced by FmtArg enum —
//                  call sites of push_fstring/push_vfstring need updating.
//                  Float formatting (%.14g) is approximated with {:.14e}; needs
//                  proper %g in Phase B.  Locale decimal-point handling is
//                  stubbed (always '.').  str2dloc uses from_utf8 for ASCII
//                  number strings (flagged TODO).  int_floor_mod, int_floor_div,
//                  shiftl, float_floor_mod, concat are assumed to exist in
//                  crate::vm; Phase B must confirm or create them.
// ──────────────────────────────────────────────────────────────────────────
