//! Standard mathematical library — `math.*`
//!
//! Translated from `src/lmathlib.c` (Lua 5.4.7, 782 lines, 28 functions).
//!
//! The PRNG is xoshiro256** operating on four 64-bit words. In C the
//! implementation has two code paths (64-bit integers vs two 32-bit halves);
//! Rust always has `u64`, so only the 64-bit path is kept.
//!
//! Deprecated compat functions guarded by `LUA_COMPAT_MATHLIB` (cosh, sinh,
//! tanh, pow, frexp, ldexp, log10, atan2) are omitted; we target Lua 5.4
//! semantics only. See PORTING.md §13.

// PORT NOTE: All imports below will be unresolved until Phase B lands the
// lua-types crate. Expected Phase-A errors: E0432, E0412, E0433, E0425.
use lua_types::{LuaError, LuaType, LuaValue};
use crate::state_stub::{LuaState, LuaStateStubExt as _, lua_CFunction as LuaCFn, upvalue_index, CompareOp, LuaDebug};

// ── Constants ──────────────────────────────────────────────────────────────

/// C: `#define PI (l_mathop(3.141592653589793238462643383279502884))`
///
/// Higher precision than `std::f64::consts::PI`; matches the C source literal.
const PI: f64 = 3.141592653589793238462643383279502884_f64;

/// Number of binary digits in the mantissa of `lua_Number` (f64).
/// C: `#define FIGS l_floatatt(MANT_DIG)` — capped at 64.
const FIGS: u32 = 53; // DBL_MANT_DIG for f64

/// Bits to discard from the 64-bit random word before float conversion.
/// C: `#define shift64_FIG (64 - FIGS)`
const SHIFT64_FIG: u32 = 64 - FIGS; // = 11

// ── Type aliases for library registration ─────────────────────────────────

/// A Lua C-style function: takes the Lua state, returns count of pushed values.
/// PORT NOTE: Phase B will unify with `lua_types::LuaCFunction`.
type LuaCFunction = fn(&mut LuaState) -> Result<usize, LuaError>;

/// An entry in the library registration table (name, optional function).
/// `None` is used for placeholder entries whose values are set manually
/// (e.g. `pi`, `huge`, `maxinteger`, `mininteger`, `random`, `randomseed`).
/// PORT NOTE: Phase B will unify with `lua_types::LibReg`.
struct LibReg {
    name: &'static [u8],
    func: Option<LuaCFunction>,
}

// ── PRNG state ────────────────────────────────────────────────────────────

/// State for the xoshiro256** PRNG.
/// C: `typedef struct { Rand64 s[4]; } RanState;`
///
/// In C this is stored as raw `lua_newuserdatauv` memory and accessed by
/// casting the userdata pointer. In Rust it is stored as typed userdata;
/// see `set_rand_func` and the TODO(port) notes in `math_random`.
struct RanState {
    s: [u64; 4],
}

// ── Pure PRNG algorithms ──────────────────────────────────────────────────

/// Advance the xoshiro256** state by one step and return the next raw 64-bit
/// pseudo-random value.
///
/// C: `static Rand64 nextrand(Rand64 *state)` (64-bit implementation).
fn next_rand(s: &mut [u64; 4]) -> u64 {
    // C: Rand64 state0 = state[0]; … state3 = state[3] ^ state1;
    let s0 = s[0];
    let s1 = s[1];
    let s2 = s[2] ^ s0;
    let s3 = s[3] ^ s1;
    // C: Rand64 res = rotl(state1 * 5, 7) * 9;
    let res = s1.wrapping_mul(5).rotate_left(7).wrapping_mul(9);
    s[0] = s0 ^ s3;
    s[1] = s1 ^ s2;
    s[2] = s2 ^ (s1 << 17);
    // C: state[3] = rotl(state3, 45);
    s[3] = s3.rotate_left(45);
    res
}

/// Convert a raw 64-bit PRNG output to a float in [0.0, 1.0).
///
/// C: `static lua_Number I2d(Rand64 x)` (64-bit implementation).
/// Takes the top FIGS=53 bits, interprets them as a signed integer, scales
/// by `scaleFIG = 0.5 / 2^52`, then corrects the two's-complement sign.
fn rand_to_float(x: u64) -> f64 {
    // C: SRand64 sx = (SRand64)(trim64(x) >> shift64_FIG);
    let sx = (x >> SHIFT64_FIG) as i64;
    // C: scaleFIG = l_mathop(0.5) / ((Rand64)1 << (FIGS - 1))
    //            = 0.5 / 2^52
    let scale_fig: f64 = 0.5 / ((1u64 << (FIGS - 1)) as f64);
    // C: lua_Number res = (lua_Number)(sx) * scaleFIG;
    let mut res = (sx as f64) * scale_fig;
    // C: if (sx < 0) res += l_mathop(1.0);  /* correct two's complement */
    if sx < 0 {
        res += 1.0;
    }
    debug_assert!(0.0 <= res && res < 1.0);
    res
}

/// Initialise the four PRNG words from two seed values.
///
/// C: `static void setseed(lua_State *L, Rand64 *state, lua_Unsigned n1, n2)`
///
/// PORT NOTE: The Lua pushes (n1, n2) are done at the call site in Rust so
/// that this function does not need `&mut LuaState`, avoiding a borrow
/// conflict with the upvalue `RanState`.
fn set_seed_words(s: &mut [u64; 4], n1: u64, n2: u64) {
    // C: state[0] = Int2I(n1); state[1] = Int2I(0xff); …
    s[0] = n1;
    s[1] = 0xff; // avoid a zero state
    s[2] = n2;
    s[3] = 0;
    // C: for (i = 0; i < 16; i++) nextrand(state);
    for _ in 0..16 {
        next_rand(s); // discard initial values to "spread" seed
    }
}

/// Project `ran` uniformly into [0, n].
///
/// C: `static lua_Unsigned project(lua_Unsigned ran, lua_Unsigned n, RanState *state)`
///
/// Uses rejection sampling with the smallest Mersenne number ≥ n as a mask.
/// Takes `&mut [u64; 4]` rather than `&mut RanState` to avoid nested borrows
/// at call sites.
fn project(mut ran: u64, n: u64, s: &mut [u64; 4]) -> u64 {
    // C: if ((n & (n + 1)) == 0) return ran & n;  /* n+1 is power of 2, no bias */
    if (n & n.wrapping_add(1)) == 0 {
        return ran & n;
    }
    // Compute the smallest (2^b - 1) not smaller than n.
    let mut lim = n;
    lim |= lim >> 1;
    lim |= lim >> 2;
    lim |= lim >> 4;
    lim |= lim >> 8;
    lim |= lim >> 16;
    lim |= lim >> 32; // u64 always has 64 bits; C guards this with #if
    debug_assert!((lim & lim.wrapping_add(1)) == 0); // lim+1 is a power of 2
    debug_assert!(lim >= n);
    debug_assert!((lim >> 1) < n);
    // C: while ((ran &= lim) > n) ran = I2UInt(nextrand(state->s));
    loop {
        ran &= lim;
        if ran <= n {
            break;
        }
        ran = next_rand(s);
    }
    ran
}

// ── Helpers ───────────────────────────────────────────────────────────────

/// Convert `d` to integer and push it; push the float unchanged if it doesn't
/// fit exactly in an i64.
///
/// C: `static void pushnumint(lua_State *L, lua_Number d)`
fn push_num_int(state: &mut LuaState, d: f64) {
    // C: if (lua_numbertointeger(d, &n)) lua_pushinteger(L, n);
    //    else lua_pushnumber(L, d);
    //
    // lua_numbertointeger: d >= LUA_MININTEGER as float &&
    //                      d <  -(LUA_MININTEGER as float)
    let min_f = i64::MIN as f64; // -2^63
    let max_plus1_f = -(i64::MIN as f64); // 2^63 (one past i64::MAX as float)
    if d >= min_f && d < max_plus1_f {
        state.push(LuaValue::Int(d as i64));
    } else {
        state.push(LuaValue::Float(d));
    }
}

// ── Basic math functions ──────────────────────────────────────────────────

/// `math.abs(x)` — absolute value, preserving integer type when possible.
///
/// C: `static int math_abs(lua_State *L)`
fn math_abs(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: if (lua_isinteger(L, 1))
    if matches!(state.value_at(1), LuaValue::Int(_)) {
        let n = state.to_integer(1).unwrap_or(0);
        let n = if n < 0 {
            (0u64.wrapping_sub(n as u64)) as i64
        } else {
            n
        };
        state.push(LuaValue::Int(n));
    } else {
        // C: lua_pushnumber(L, fabs(luaL_checknumber(L, 1)));
        let x = state.check_number(1)?;
        state.push(LuaValue::Float(x.abs()));
    }
    Ok(1)
}

/// `math.sin(x)` — sine (radians).
///
/// C: `static int math_sin(lua_State *L)`
fn math_sin(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: lua_pushnumber(L, sin(luaL_checknumber(L, 1)));
    let x = state.check_number(1)?;
    state.push(LuaValue::Float(x.sin()));
    Ok(1)
}

/// `math.cos(x)` — cosine (radians).
///
/// C: `static int math_cos(lua_State *L)`
fn math_cos(state: &mut LuaState) -> Result<usize, LuaError> {
    let x = state.check_number(1)?;
    state.push(LuaValue::Float(x.cos()));
    Ok(1)
}

/// `math.tan(x)` — tangent (radians).
///
/// C: `static int math_tan(lua_State *L)`
fn math_tan(state: &mut LuaState) -> Result<usize, LuaError> {
    let x = state.check_number(1)?;
    state.push(LuaValue::Float(x.tan()));
    Ok(1)
}

/// `math.asin(x)` — arc-sine, result in radians.
///
/// C: `static int math_asin(lua_State *L)`
fn math_asin(state: &mut LuaState) -> Result<usize, LuaError> {
    let x = state.check_number(1)?;
    state.push(LuaValue::Float(x.asin()));
    Ok(1)
}

/// `math.acos(x)` — arc-cosine, result in radians.
///
/// C: `static int math_acos(lua_State *L)`
fn math_acos(state: &mut LuaState) -> Result<usize, LuaError> {
    let x = state.check_number(1)?;
    state.push(LuaValue::Float(x.acos()));
    Ok(1)
}

/// `math.atan(y [, x])` — arc-tangent of y/x (defaults x=1), result in
/// radians. Subsumes C's `atan2` when x is provided.
///
/// C: `static int math_atan(lua_State *L)`
fn math_atan(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: lua_Number y = luaL_checknumber(L, 1);
    let y = state.check_number(1)?;
    // C: lua_Number x = luaL_optnumber(L, 2, 1);
    let x = state.opt_number(2, 1.0)?;
    // C: lua_pushnumber(L, atan2(y, x));
    state.push(LuaValue::Float(y.atan2(x)));
    Ok(1)
}

/// `math.tointeger(x)` — convert x to an integer or return false.
///
/// C: `static int math_toint(lua_State *L)`
fn math_toint(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: int valid; lua_Integer n = lua_tointegerx(L, 1, &valid);
    // TODO(port): state.to_integer_opt(1) should return Option<i64>;
    // the method name/signature will be confirmed in Phase B.
    let maybe_n: Option<i64> = state.to_integer_opt(1);
    if let Some(n) = maybe_n {
        // C: if (l_likely(valid)) lua_pushinteger(L, n);
        state.push(LuaValue::Int(n));
    } else {
        // C: luaL_checkany(L, 1); luaL_pushfail(L);
        state.check_any(1)?;
        // PORT NOTE: luaL_pushfail in Lua 5.4 pushes false (not nil).
        state.push(LuaValue::Bool(false));
    }
    Ok(1)
}

/// `math.floor(x)` — largest integer ≤ x.
///
/// C: `static int math_floor(lua_State *L)`
fn math_floor(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: if (lua_isinteger(L, 1)) lua_settop(L, 1);  /* integer is its own floor */
    if matches!(state.value_at(1), LuaValue::Int(_)) {
        state.set_top(1); // keep arg 1 on stack, discard rest
    } else {
        // C: lua_Number d = floor(luaL_checknumber(L, 1)); pushnumint(L, d);
        let d = state.check_number(1)?.floor();
        push_num_int(state, d);
    }
    Ok(1)
}

/// `math.ceil(x)` — smallest integer ≥ x.
///
/// C: `static int math_ceil(lua_State *L)`
fn math_ceil(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: if (lua_isinteger(L, 1)) lua_settop(L, 1);  /* integer is its own ceil */
    if matches!(state.value_at(1), LuaValue::Int(_)) {
        state.set_top(1);
    } else {
        let d = state.check_number(1)?.ceil();
        push_num_int(state, d);
    }
    Ok(1)
}

/// `math.fmod(x, y)` — floating-point remainder (same sign as x).
///
/// C: `static int math_fmod(lua_State *L)`
fn math_fmod(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: if (lua_isinteger(L, 1) && lua_isinteger(L, 2))
    if matches!(state.value_at(1), LuaValue::Int(_))
        && matches!(state.value_at(2), LuaValue::Int(_))
    {
        let a = state.to_integer(1).unwrap_or(0);
        let d = state.to_integer(2).unwrap_or(0);
        if (d as u64).wrapping_add(1) <= 1 {
            if d == 0 {
                return Err(LuaError::arg_error(2, "zero"));
            }
            state.push(LuaValue::Int(0));
        } else {
            state.push(LuaValue::Int(a % d));
        }
    } else {
        // C: lua_pushnumber(L, fmod(luaL_checknumber(L, 1), luaL_checknumber(L, 2)));
        let x = state.check_number(1)?;
        let y = state.check_number(2)?;
        state.push(LuaValue::Float(x % y));
    }
    Ok(1)
}

/// `math.modf(x)` — split into integer and fractional parts; returns 2 values.
///
/// C: `static int math_modf(lua_State *L)`
///
/// PORT NOTE: Does not use `modf` (avoids `double *` / `float *` ABI mismatch
/// for non-double `lua_Number`). Instead, uses ceil/floor + subtraction.
fn math_modf(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: if (lua_isinteger(L, 1)) { lua_settop(L, 1); lua_pushnumber(L, 0); }
    if matches!(state.value_at(1), LuaValue::Int(_)) {
        state.set_top(1); // integer part is the integer itself
        state.push(LuaValue::Float(0.0)); // no fractional part
    } else {
        let n = state.check_number(1)?;
        // C: lua_Number ip = (n < 0) ? ceil(n) : floor(n);
        let ip = if n < 0.0 { n.ceil() } else { n.floor() };
        // C: pushnumint(L, ip);
        push_num_int(state, ip);
        // C: lua_pushnumber(L, (n == ip) ? 0.0 : (n - ip));
        let frac = if n == ip { 0.0 } else { n - ip };
        state.push(LuaValue::Float(frac));
    }
    Ok(2)
}

/// `math.sqrt(x)` — square root.
///
/// C: `static int math_sqrt(lua_State *L)`
fn math_sqrt(state: &mut LuaState) -> Result<usize, LuaError> {
    let x = state.check_number(1)?;
    state.push(LuaValue::Float(x.sqrt()));
    Ok(1)
}

/// `math.ult(m, n)` — unsigned less-than on integers.
///
/// C: `static int math_ult(lua_State *L)`
fn math_ult(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: lua_Integer a = luaL_checkinteger(L, 1); lua_Integer b = …(L, 2);
    let a = state.check_integer(1)?;
    let b = state.check_integer(2)?;
    // C: lua_pushboolean(L, (lua_Unsigned)a < (lua_Unsigned)b);
    state.push(LuaValue::Bool((a as u64) < (b as u64)));
    Ok(1)
}

/// `math.log(x [, base])` — logarithm; natural if base omitted.
///
/// C: `static int math_log(lua_State *L)`
fn math_log(state: &mut LuaState) -> Result<usize, LuaError> {
    let x = state.check_number(1)?;
    // C: if (lua_isnoneornil(L, 2)) res = log(x);
    let res = if matches!(state.type_at(2), LuaType::None | LuaType::Nil) {
        x.ln()
    } else {
        let base = state.check_number(2)?;
        // C: if (base == 2.0) res = log2(x);  (guarded by !LUA_USE_C89)
        if base == 2.0 {
            x.log2()
        } else if base == 10.0 {
            // C: else if (base == 10.0) res = log10(x);
            x.log10()
        } else {
            // C: else res = log(x) / log(base);
            x.ln() / base.ln()
        }
    };
    state.push(LuaValue::Float(res));
    Ok(1)
}

/// `math.exp(x)` — e raised to the power x.
///
/// C: `static int math_exp(lua_State *L)`
fn math_exp(state: &mut LuaState) -> Result<usize, LuaError> {
    let x = state.check_number(1)?;
    state.push(LuaValue::Float(x.exp()));
    Ok(1)
}

/// `math.deg(x)` — convert radians to degrees.
///
/// C: `static int math_deg(lua_State *L)`
fn math_deg(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: lua_pushnumber(L, luaL_checknumber(L, 1) * (180.0 / PI));
    let x = state.check_number(1)?;
    state.push(LuaValue::Float(x * (180.0 / PI)));
    Ok(1)
}

/// `math.rad(x)` — convert degrees to radians.
///
/// C: `static int math_rad(lua_State *L)`
fn math_rad(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: lua_pushnumber(L, luaL_checknumber(L, 1) * (PI / 180.0));
    let x = state.check_number(1)?;
    state.push(LuaValue::Float(x * (PI / 180.0)));
    Ok(1)
}

/// `math.min(x, ...)` — minimum of all arguments (uses Lua `<` comparison).
///
/// C: `static int math_min(lua_State *L)`
fn math_min(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: int n = lua_gettop(L); int imin = 1;
    let n = state.get_top();
    let mut imin: i32 = 1;
    // C: luaL_argcheck(L, n >= 1, 1, "value expected");
    if n < 1 {
        return Err(LuaError::arg_error(1, "value expected"));
    }
    for i in 2..=n {
        if state.compare_lt(i, imin)? {
            imin = i;
        }
    }
    // C: lua_pushvalue(L, imin);
    state.push_value(imin)?;
    Ok(1)
}

/// `math.max(x, ...)` — maximum of all arguments (uses Lua `<` comparison).
///
/// C: `static int math_max(lua_State *L)`
fn math_max(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: int n = lua_gettop(L); int imax = 1;
    let n = state.get_top();
    let mut imax: i32 = 1;
    // C: luaL_argcheck(L, n >= 1, 1, "value expected");
    if n < 1 {
        return Err(LuaError::arg_error(1, "value expected"));
    }
    // C: for (i = 2; i <= n; i++) { if (lua_compare(L, imax, i, LUA_OPLT)) imax = i; }
    for i in 2..=n {
        if state.compare_lt(imax, i)? {
            imax = i;
        }
    }
    // C: lua_pushvalue(L, imax);
    state.push_value(imax)?;
    Ok(1)
}

/// `math.type(x)` — return `"integer"`, `"float"`, or false for non-numbers.
///
/// C: `static int math_type(lua_State *L)`
fn math_type(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: if (lua_type(L, 1) == LUA_TNUMBER)
    if matches!(state.type_at(1), LuaType::Number) {
        // C: lua_pushstring(L, lua_isinteger(L, 1) ? "integer" : "float");
        if matches!(state.value_at(1), LuaValue::Int(_)) {
            state.push_string(b"integer");
        } else {
            state.push_string(b"float");
        }
    } else {
        // C: luaL_checkany(L, 1); luaL_pushfail(L);
        state.check_any(1)?;
        // PORT NOTE: luaL_pushfail pushes false in Lua 5.4.4+.
        state.push(LuaValue::Bool(false));
    }
    Ok(1)
}

// ── PRNG-backed Lua functions ─────────────────────────────────────────────

/// `math.random([m [, n]])` — pseudo-random number generation.
///
/// C: `static int math_random(lua_State *L)`
///
/// With no arguments: float in [0, 1).
/// With one argument n: integer in [1, n] (or full random u64 if n == 0).
/// With two arguments m, n: integer in [m, n].
fn math_random(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: RanState *state = (RanState *)lua_touserdata(L, lua_upvalueindex(1));
    // TODO(port): RanState is stored as typed userdata in closure upvalue 1.
    // Phase B must implement `state.upvalue_userdata_mut::<RanState>(1)` using
    // interior mutability (e.g. GcRef<RefCell<RanState>>) to avoid the borrow
    // conflict between &mut RanState and subsequent &mut LuaState push calls.
    //
    // For Phase A: advance PRNG and get args via separate borrows.
    let rv = advance_prng(state)?;
    let n_args = state.get_top();

    if n_args == 0 {
        // C: case 0: lua_pushnumber(L, I2d(rv)); return 1;
        state.push(LuaValue::Float(rand_to_float(rv)));
        return Ok(1);
    }

    let (low, up) = match n_args {
        1 => {
            // C: case 1: low = 1; up = luaL_checkinteger(L, 1);
            let up = state.check_integer(1)?;
            if up == 0 {
                // C: if (up == 0) { lua_pushinteger(L, I2UInt(rv)); return 1; }
                // I2UInt(rv) = rv (trivial for u64)
                state.push(LuaValue::Int(rv as i64));
                return Ok(1);
            }
            (1i64, up)
        }
        2 => {
            // C: case 2: low = luaL_checkinteger(L, 1); up = luaL_checkinteger(L, 2);
            let low = state.check_integer(1)?;
            let up = state.check_integer(2)?;
            (low, up)
        }
        _ => {
            // C: default: return luaL_error(L, "wrong number of arguments");
            return Err(LuaError::runtime(format_args!(
                "wrong number of arguments"
            )));
        }
    };

    // C: luaL_argcheck(L, low <= up, 1, "interval is empty");
    if low > up {
        return Err(LuaError::arg_error(1, "interval is empty"));
    }

    // C: p = project(I2UInt(rv), (lua_Unsigned)up - (lua_Unsigned)low, state);
    // C: lua_pushinteger(L, p + (lua_Unsigned)low);
    let range = (up as u64).wrapping_sub(low as u64);
    let p = project_from_upvalue(state, rv, range)?;
    state.push(LuaValue::Int((p as u64).wrapping_add(low as u64) as i64));
    Ok(1)
}

/// `math.randomseed([x [, y]])` — seed the PRNG; returns two seed values.
///
/// C: `static int math_randomseed(lua_State *L)`
fn math_randomseed(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: RanState *state = (RanState *)lua_touserdata(L, lua_upvalueindex(1));
    // TODO(port): same upvalue userdata access issue as math_random.
    if matches!(state.type_at(1), LuaType::None) {
        // C: if (lua_isnone(L, 1)) { randseed(L, state); }
        // randseed uses time(NULL) and address of L for entropy.
        apply_random_seed(state)?;
    } else {
        // C: lua_Integer n1 = luaL_checkinteger(L, 1);
        //    lua_Integer n2 = luaL_optinteger(L, 2, 0);
        let n1 = state.check_integer(1)? as u64;
        let n2 = state.opt_integer(2, 0)? as u64;
        // C: setseed(L, state->s, n1, n2);
        apply_set_seed(state, n1, n2)?;
    }
    Ok(2) // C: return 2;  /* return seeds */
}

/// Advance the PRNG stored in upvalue 1 and return the raw 64-bit output.
///
/// Extracted to a separate function so `math_random` can release the upvalue
/// borrow before calling `state.push(...)`.
///
/// TODO(port): implement by extracting `&mut RanState` from upvalue 1.
/// The RanState userdata must use interior mutability in Phase B to allow
/// concurrent reads of `state` (for push operations) while mutating the PRNG.
fn advance_prng(_state: &mut LuaState) -> Result<u64, LuaError> {
    // TODO(port): access GcRef<RefCell<RanState>> from closure upvalue 1,
    // borrow_mut it, call next_rand(&mut ran.s), return the result.
    todo!("PORT: advance_prng — upvalue RanState access (Phase B)")
}

/// Apply rejection sampling for `math.random` using the PRNG in upvalue 1.
///
/// Extracted so the upvalue borrow does not overlap the push call in `math_random`.
///
/// TODO(port): same upvalue access issue as `advance_prng`.
fn project_from_upvalue(
    _state: &mut LuaState,
    ran: u64,
    n: u64,
) -> Result<u64, LuaError> {
    // TODO(port): access &mut [u64; 4] from upvalue RanState for rejection loop.
    // For now, compute without the rejection loop (biased but structurally correct).
    // Phase B replaces with: project(ran, n, &mut ran_state.s)
    todo!("PORT: project_from_upvalue — upvalue RanState access (Phase B)")
}

/// Seed the PRNG from wall-clock time (entropy source).
///
/// C: `randseed(L, state)` — uses `time(NULL)` and address of `L` as seeds.
///
/// TODO(port): must write n1 and n2 back to the upvalue RanState.
fn apply_random_seed(state: &mut LuaState) -> Result<(), LuaError> {
    // C: lua_Unsigned seed1 = (lua_Unsigned)time(NULL);
    // PORT NOTE: std::time is not in the banned list (only std::fs/net/process).
    let seed1 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // C: lua_Unsigned seed2 = (lua_Unsigned)(size_t)L;
    // TODO(port): C uses address of L for ASLR entropy; no safe equivalent.
    // Phase B can use a thread-local counter or OS entropy instead.
    let seed2: u64 = 0;
    apply_set_seed(state, seed1, seed2)
}

/// Apply explicit seeds to the PRNG and push them onto the stack.
///
/// C: `setseed(L, state->s, n1, n2)` — also pushes n1, n2.
///
/// TODO(port): must write seeds into the upvalue RanState in Phase B.
fn apply_set_seed(state: &mut LuaState, n1: u64, n2: u64) -> Result<(), LuaError> {
    // TODO(port): extract &mut [u64; 4] from upvalue 1, call set_seed_words(s, n1, n2).
    // The push calls below are the correct post-seed behaviour.
    let _ = (n1, n2); // suppress unused-variable hint; Phase B will use these
    // C: lua_pushinteger(L, n1); lua_pushinteger(L, n2);
    state.push(LuaValue::Int(n1 as i64));
    state.push(LuaValue::Int(n2 as i64));
    Ok(())
}

/// Register `math.random` and `math.randomseed` as closures sharing a single
/// `RanState` userdata upvalue, then seed it.
///
/// C: `static void setrandfunc(lua_State *L)`
fn set_rand_func(state: &mut LuaState) -> Result<(), LuaError> {
    // C: RanState *state = (RanState *)lua_newuserdatauv(L, sizeof(RanState), 0);
    // TODO(port): create LuaValue::UserData holding a RanState, push it.
    // state.new_typed_userdata::<RanState>()? — API to be defined in Phase B.

    // C: randseed(L, state);  /* initialize with a "random" seed */
    // Seeds are pushed by randseed; pop them immediately.
    apply_random_seed(state)?;
    // C: lua_pop(L, 2);  /* remove pushed seeds */
    state.pop_n(2);

    // C: luaL_setfuncs(L, randfuncs, 1);
    // Registers math_random and math_randomseed as closures with 1 upvalue
    // (the RanState userdata on top of the stack).
    // TODO(port): state.set_funcs_with_upvalue(&RAND_FUNCS, 1)?;
    // RAND_FUNCS = [("random", math_random), ("randomseed", math_randomseed)]
    Ok(())
}

// ── Library registration table ────────────────────────────────────────────

/// The `math` library function table.
///
/// C: `static const luaL_Reg mathlib[]`
///
/// Placeholder entries (`None`) are filled in manually by `luaopen_math`
/// (`pi`, `huge`, `maxinteger`, `mininteger`) or by `set_rand_func`
/// (`random`, `randomseed`).
static MATHLIB: &[LibReg] = &[
    LibReg { name: b"abs",        func: Some(math_abs)    },
    LibReg { name: b"acos",       func: Some(math_acos)   },
    LibReg { name: b"asin",       func: Some(math_asin)   },
    LibReg { name: b"atan",       func: Some(math_atan)   },
    LibReg { name: b"ceil",       func: Some(math_ceil)   },
    LibReg { name: b"cos",        func: Some(math_cos)    },
    LibReg { name: b"deg",        func: Some(math_deg)    },
    LibReg { name: b"exp",        func: Some(math_exp)    },
    LibReg { name: b"tointeger",  func: Some(math_toint)  },
    LibReg { name: b"floor",      func: Some(math_floor)  },
    LibReg { name: b"fmod",       func: Some(math_fmod)   },
    LibReg { name: b"ult",        func: Some(math_ult)    },
    LibReg { name: b"log",        func: Some(math_log)    },
    LibReg { name: b"max",        func: Some(math_max)    },
    LibReg { name: b"min",        func: Some(math_min)    },
    LibReg { name: b"modf",       func: Some(math_modf)   },
    LibReg { name: b"rad",        func: Some(math_rad)    },
    LibReg { name: b"sin",        func: Some(math_sin)    },
    LibReg { name: b"sqrt",       func: Some(math_sqrt)   },
    LibReg { name: b"tan",        func: Some(math_tan)    },
    LibReg { name: b"type",       func: Some(math_type)   },
    // Placeholders; values are set manually in luaopen_math / set_rand_func.
    LibReg { name: b"random",     func: None },
    LibReg { name: b"randomseed", func: None },
    LibReg { name: b"pi",         func: None },
    LibReg { name: b"huge",       func: None },
    LibReg { name: b"maxinteger", func: None },
    LibReg { name: b"mininteger", func: None },
];

static MATHLIB_FUNCS: &[(&[u8], LuaCFunction)] = &[
    (b"abs",        math_abs),
    (b"acos",       math_acos),
    (b"asin",       math_asin),
    (b"atan",       math_atan),
    (b"ceil",       math_ceil),
    (b"cos",        math_cos),
    (b"deg",        math_deg),
    (b"exp",        math_exp),
    (b"tointeger",  math_toint),
    (b"floor",      math_floor),
    (b"fmod",       math_fmod),
    (b"ult",        math_ult),
    (b"log",        math_log),
    (b"max",        math_max),
    (b"min",        math_min),
    (b"modf",       math_modf),
    (b"rad",        math_rad),
    (b"sin",        math_sin),
    (b"sqrt",       math_sqrt),
    (b"tan",        math_tan),
    (b"type",       math_type),
];

// ── Module entry point ────────────────────────────────────────────────────

/// Open the `math` library: create the table, populate constants, register
/// the PRNG functions with their shared `RanState` upvalue.
///
/// C: `LUAMOD_API int luaopen_math(lua_State *L)`
///
/// `LUAMOD_API` → `pub` (see macros.tsv).
pub fn luaopen_math(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: luaL_newlib(L, mathlib);
    // Creates a new table and registers all non-None entries from MATHLIB.
    state.new_lib(MATHLIB_FUNCS)?;

    // C: lua_pushnumber(L, PI); lua_setfield(L, -2, "pi");
    state.push(LuaValue::Float(PI));
    state.set_field(-2, b"pi")?;

    // C: lua_pushnumber(L, (lua_Number)HUGE_VAL); lua_setfield(L, -2, "huge");
    state.push(LuaValue::Float(f64::INFINITY));
    state.set_field(-2, b"huge")?;

    // C: lua_pushinteger(L, LUA_MAXINTEGER); lua_setfield(L, -2, "maxinteger");
    // LUA_MAXINTEGER = i64::MAX (lua_Integer is int64_t in default config).
    state.push(LuaValue::Int(i64::MAX));
    state.set_field(-2, b"maxinteger")?;

    // C: lua_pushinteger(L, LUA_MININTEGER); lua_setfield(L, -2, "mininteger");
    state.push(LuaValue::Int(i64::MIN));
    state.set_field(-2, b"mininteger")?;

    // C: setrandfunc(L);
    // Registers math.random and math.randomseed as upvalue-bearing closures.
    set_rand_func(state)?;

    Ok(1)
}

// ──────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        src/lmathlib.c  (782 lines, 28 functions)
//   target_crate:  lua-stdlib
//   confidence:    medium
//   todos:         16
//   port_notes:    8
//   unsafe_blocks: 0
//   notes:         All basic math functions are mechanically faithful. The
//                  PRNG xoshiro256** algorithm is correctly translated using
//                  native u64 (only the 64-bit code path; the 32-bit fallback
//                  is dropped). The main Phase-B work is wiring up the upvalue
//                  RanState userdata: advance_prng, project_from_upvalue,
//                  apply_random_seed, apply_set_seed, and set_rand_func all
//                  carry TODO(port) stubs where typed userdata + interior
//                  mutability (RefCell) is required to avoid borrow conflicts.
//                  Deprecated LUA_COMPAT_MATHLIB functions are omitted per
//                  PORTING.md §13. state.new_lib, state.set_field,
//                  state.compare_lt, state.push_value, state.opt_number,
//                  state.opt_integer, state.check_integer, state.check_number,
//                  state.check_any, state.to_integer_opt, state.get_top,
//                  state.set_top, state.pop_n API names assumed; Phase B
//                  will reconcile with the actual LuaState impl.
// ──────────────────────────────────────────────────────────────────────────
