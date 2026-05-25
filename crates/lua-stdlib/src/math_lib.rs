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
use crate::state_stub::{LuaState, LuaStateStubExt as _};

// ── Constants ──────────────────────────────────────────────────────────────

///
/// Higher precision than `std::f64::consts::PI`; matches the C source literal.
const PI: f64 = 3.141592653589793238462643383279502884_f64;

/// Number of binary digits in the mantissa of `lua_Number` (f64).
const FIGS: u32 = 53; // DBL_MANT_DIG for f64

/// Bits to discard from the 64-bit random word before float conversion.
const SHIFT64_FIG: u32 = 64 - FIGS; // = 11

// ── Type aliases for library registration ─────────────────────────────────

/// A Lua C-style function: takes the Lua state, returns count of pushed values.
/// PORT NOTE: Phase B will unify with `lua_types::LuaCFunction`.
type LuaCFunction = fn(&mut LuaState) -> Result<usize, LuaError>;

/// An entry in the library registration table (name, optional function).
/// `None` is used for placeholder entries whose values are set manually
/// (e.g. `pi`, `huge`, `maxinteger`, `mininteger`, `random`, `randomseed`).
/// PORT NOTE: Phase B will unify with `lua_types::LibReg`.
#[expect(dead_code, reason = "ported stdlib helper; not yet wired into the runtime")]
struct LibReg {
    name: &'static [u8],
    func: Option<LuaCFunction>,
}

// ── PRNG state ────────────────────────────────────────────────────────────

/// State for the xoshiro256** PRNG.
///
/// In C this is stored as raw `lua_newuserdatauv` memory and accessed by
/// casting the userdata pointer. Until typed-userdata closure upvalues land
/// in Phase B, we keep the PRNG state in a thread-local cell so that
/// `math.random` and `math.randomseed` are callable from Lua. This collapses
/// per-lua_State PRNG isolation to per-thread, which is sufficient for the
/// 5.4 test corpus.
struct RanState {
    s: [u64; 4],
}

thread_local! {
    static RAN_STATE: std::cell::RefCell<RanState> =
        std::cell::RefCell::new(RanState { s: [0xff, 0xff, 0xff, 0xff] });
}

// ── Pure PRNG algorithms ──────────────────────────────────────────────────

/// Advance the xoshiro256** state by one step and return the next raw 64-bit
/// pseudo-random value.
///
fn next_rand(s: &mut [u64; 4]) -> u64 {
    let s0 = s[0];
    let s1 = s[1];
    let s2 = s[2] ^ s0;
    let s3 = s[3] ^ s1;
    let res = s1.wrapping_mul(5).rotate_left(7).wrapping_mul(9);
    s[0] = s0 ^ s3;
    s[1] = s1 ^ s2;
    s[2] = s2 ^ (s1 << 17);
    s[3] = s3.rotate_left(45);
    res
}

/// Convert a raw 64-bit PRNG output to a float in [0.0, 1.0).
///
/// Takes the top FIGS=53 bits, interprets them as a signed integer, scales
/// by `scaleFIG = 0.5 / 2^52`, then corrects the two's-complement sign.
fn rand_to_float(x: u64) -> f64 {
    let sx = (x >> SHIFT64_FIG) as i64;
    //            = 0.5 / 2^52
    let scale_fig: f64 = 0.5 / ((1u64 << (FIGS - 1)) as f64);
    let mut res = (sx as f64) * scale_fig;
    if sx < 0 {
        res += 1.0;
    }
    debug_assert!(0.0 <= res && res < 1.0);
    res
}

/// Initialise the four PRNG words from two seed values.
///
///
/// PORT NOTE: The Lua pushes (n1, n2) are done at the call site in Rust so
/// that this function does not need `&mut LuaState`, avoiding a borrow
/// conflict with the upvalue `RanState`.
fn set_seed_words(s: &mut [u64; 4], n1: u64, n2: u64) {
    s[0] = n1;
    s[1] = 0xff; // avoid a zero state
    s[2] = n2;
    s[3] = 0;
    for _ in 0..16 {
        next_rand(s); // discard initial values to "spread" seed
    }
}

/// Project `ran` uniformly into [0, n].
///
///
/// Uses rejection sampling with the smallest Mersenne number ≥ n as a mask.
/// Takes `&mut [u64; 4]` rather than `&mut RanState` to avoid nested borrows
/// at call sites.
fn project(mut ran: u64, n: u64, s: &mut [u64; 4]) -> u64 {
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
fn push_num_int(state: &mut LuaState, d: f64) {
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
fn math_abs(state: &mut LuaState) -> Result<usize, LuaError> {
    if matches!(state.value_at(1), LuaValue::Int(_)) {
        let n = state.to_integer(1).unwrap_or(0);
        let n = if n < 0 {
            (0u64.wrapping_sub(n as u64)) as i64
        } else {
            n
        };
        state.push(LuaValue::Int(n));
    } else {
        let x = state.check_number(1)?;
        state.push(LuaValue::Float(x.abs()));
    }
    Ok(1)
}

/// `math.sin(x)` — sine (radians).
///
fn math_sin(state: &mut LuaState) -> Result<usize, LuaError> {
    let x = state.check_number(1)?;
    state.push(LuaValue::Float(x.sin()));
    Ok(1)
}

/// `math.cos(x)` — cosine (radians).
///
fn math_cos(state: &mut LuaState) -> Result<usize, LuaError> {
    let x = state.check_number(1)?;
    state.push(LuaValue::Float(x.cos()));
    Ok(1)
}

/// `math.tan(x)` — tangent (radians).
///
fn math_tan(state: &mut LuaState) -> Result<usize, LuaError> {
    let x = state.check_number(1)?;
    state.push(LuaValue::Float(x.tan()));
    Ok(1)
}

/// `math.asin(x)` — arc-sine, result in radians.
///
fn math_asin(state: &mut LuaState) -> Result<usize, LuaError> {
    let x = state.check_number(1)?;
    state.push(LuaValue::Float(x.asin()));
    Ok(1)
}

/// `math.acos(x)` — arc-cosine, result in radians.
///
fn math_acos(state: &mut LuaState) -> Result<usize, LuaError> {
    let x = state.check_number(1)?;
    state.push(LuaValue::Float(x.acos()));
    Ok(1)
}

/// `math.atan(y [, x])` — arc-tangent of y/x (defaults x=1), result in
/// radians. Subsumes C's `atan2` when x is provided.
///
fn math_atan(state: &mut LuaState) -> Result<usize, LuaError> {
    let y = state.check_number(1)?;
    let x = state.opt_number(2, 1.0)?;
    state.push(LuaValue::Float(y.atan2(x)));
    Ok(1)
}

/// `math.tointeger(x)` — convert x to an integer or return false.
///
fn math_toint(state: &mut LuaState) -> Result<usize, LuaError> {
    // TODO(port): state.to_integer_opt(1) should return Option<i64>;
    // the method name/signature will be confirmed in Phase B.
    let maybe_n: Option<i64> = state.to_integer_opt(1);
    if let Some(n) = maybe_n {
        state.push(LuaValue::Int(n));
    } else {
        state.check_any(1)?;
        // PORT NOTE: luaL_pushfail in Lua 5.4 pushes false (not nil).
        state.push(LuaValue::Bool(false));
    }
    Ok(1)
}

/// `math.floor(x)` — largest integer ≤ x.
///
fn math_floor(state: &mut LuaState) -> Result<usize, LuaError> {
    if matches!(state.value_at(1), LuaValue::Int(_)) {
        // Must go through the public C-API set_top (relative to the call
        // frame); the inherent LuaState::set_top treats its argument as an
        // absolute StackIdx.
        lua_vm::api::set_top(state, 1)?;
    } else {
        let d = state.check_number(1)?.floor();
        push_num_int(state, d);
    }
    Ok(1)
}

/// `math.ceil(x)` — smallest integer ≥ x.
///
fn math_ceil(state: &mut LuaState) -> Result<usize, LuaError> {
    if matches!(state.value_at(1), LuaValue::Int(_)) {
        // Public C-API set_top (relative); inherent LuaState::set_top is absolute.
        lua_vm::api::set_top(state, 1)?;
    } else {
        let d = state.check_number(1)?.ceil();
        push_num_int(state, d);
    }
    Ok(1)
}

/// `math.fmod(x, y)` — floating-point remainder (same sign as x).
///
fn math_fmod(state: &mut LuaState) -> Result<usize, LuaError> {
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
        let x = state.check_number(1)?;
        let y = state.check_number(2)?;
        state.push(LuaValue::Float(x % y));
    }
    Ok(1)
}

/// `math.modf(x)` — split into integer and fractional parts; returns 2 values.
///
///
/// PORT NOTE: Does not use `modf` (avoids `double *` / `float *` ABI mismatch
/// for non-double `lua_Number`). Instead, uses ceil/floor + subtraction.
fn math_modf(state: &mut LuaState) -> Result<usize, LuaError> {
    if matches!(state.value_at(1), LuaValue::Int(_)) {
        // Public C-API set_top (relative); inherent LuaState::set_top is absolute.
        lua_vm::api::set_top(state, 1)?; // integer part is the integer itself
        state.push(LuaValue::Float(0.0)); // no fractional part
    } else {
        let n = state.check_number(1)?;
        let ip = if n < 0.0 { n.ceil() } else { n.floor() };
        push_num_int(state, ip);
        let frac = if n == ip { 0.0 } else { n - ip };
        state.push(LuaValue::Float(frac));
    }
    Ok(2)
}

/// `math.sqrt(x)` — square root.
///
fn math_sqrt(state: &mut LuaState) -> Result<usize, LuaError> {
    let x = state.check_number(1)?;
    state.push(LuaValue::Float(x.sqrt()));
    Ok(1)
}

/// `math.ult(m, n)` — unsigned less-than on integers.
///
fn math_ult(state: &mut LuaState) -> Result<usize, LuaError> {
    let a = state.check_integer(1)?;
    let b = state.check_integer(2)?;
    state.push(LuaValue::Bool((a as u64) < (b as u64)));
    Ok(1)
}

/// `math.log(x [, base])` — logarithm; natural if base omitted.
///
fn math_log(state: &mut LuaState) -> Result<usize, LuaError> {
    let x = state.check_number(1)?;
    let res = if matches!(state.type_at(2), LuaType::None | LuaType::Nil) {
        x.ln()
    } else {
        let base = state.check_number(2)?;
        if base == 2.0 {
            x.log2()
        } else if base == 10.0 {
            x.log10()
        } else {
            x.ln() / base.ln()
        }
    };
    state.push(LuaValue::Float(res));
    Ok(1)
}

/// `math.exp(x)` — e raised to the power x.
///
fn math_exp(state: &mut LuaState) -> Result<usize, LuaError> {
    let x = state.check_number(1)?;
    state.push(LuaValue::Float(x.exp()));
    Ok(1)
}

/// `math.deg(x)` — convert radians to degrees.
///
fn math_deg(state: &mut LuaState) -> Result<usize, LuaError> {
    let x = state.check_number(1)?;
    state.push(LuaValue::Float(x * (180.0 / PI)));
    Ok(1)
}

/// `math.rad(x)` — convert degrees to radians.
///
fn math_rad(state: &mut LuaState) -> Result<usize, LuaError> {
    let x = state.check_number(1)?;
    state.push(LuaValue::Float(x * (PI / 180.0)));
    Ok(1)
}

/// `math.min(x, ...)` — minimum of all arguments (uses Lua `<` comparison).
///
fn math_min(state: &mut LuaState) -> Result<usize, LuaError> {
    let n = state.get_top();
    let mut imin: i32 = 1;
    if n < 1 {
        return Err(LuaError::arg_error(1, "value expected"));
    }
    for i in 2..=n {
        if state.compare_lt(i, imin)? {
            imin = i;
        }
    }
    state.push_value(imin)?;
    Ok(1)
}

/// `math.max(x, ...)` — maximum of all arguments (uses Lua `<` comparison).
///
fn math_max(state: &mut LuaState) -> Result<usize, LuaError> {
    let n = state.get_top();
    let mut imax: i32 = 1;
    if n < 1 {
        return Err(LuaError::arg_error(1, "value expected"));
    }
    for i in 2..=n {
        if state.compare_lt(imax, i)? {
            imax = i;
        }
    }
    state.push_value(imax)?;
    Ok(1)
}

/// `math.type(x)` — return `"integer"`, `"float"`, or false for non-numbers.
///
fn math_type(state: &mut LuaState) -> Result<usize, LuaError> {
    if matches!(state.type_at(1), LuaType::Number) {
        if matches!(state.value_at(1), LuaValue::Int(_)) {
            state.push_string(b"integer")?;
        } else {
            state.push_string(b"float")?;
        }
    } else {
        state.check_any(1)?;
        // PORT NOTE: luaL_pushfail pushes false in Lua 5.4.4+.
        state.push(LuaValue::Bool(false));
    }
    Ok(1)
}

// ── PRNG-backed Lua functions ─────────────────────────────────────────────

/// `math.random([m [, n]])` — pseudo-random number generation.
///
///
/// With no arguments: float in [0, 1).
/// With one argument n: integer in [1, n] (or full random u64 if n == 0).
/// With two arguments m, n: integer in [m, n].
fn math_random(state: &mut LuaState) -> Result<usize, LuaError> {
    // TODO(port): RanState is stored as typed userdata in closure upvalue 1.
    // Phase B must implement `state.upvalue_userdata_mut::<RanState>(1)` using
    // interior mutability (e.g. GcRef<RefCell<RanState>>) to avoid the borrow
    // conflict between &mut RanState and subsequent &mut LuaState push calls.
    //
    // For Phase A: advance PRNG and get args via separate borrows.
    let rv = advance_prng(state)?;
    let n_args = state.get_top();

    if n_args == 0 {
        state.push(LuaValue::Float(rand_to_float(rv)));
        return Ok(1);
    }

    let (low, up) = match n_args {
        1 => {
            let up = state.check_integer(1)?;
            if up == 0 {
                // I2UInt(rv) = rv (trivial for u64)
                state.push(LuaValue::Int(rv as i64));
                return Ok(1);
            }
            (1i64, up)
        }
        2 => {
            let low = state.check_integer(1)?;
            let up = state.check_integer(2)?;
            (low, up)
        }
        _ => {
            return Err(LuaError::runtime(format_args!(
                "wrong number of arguments"
            )));
        }
    };

    if low > up {
        return Err(LuaError::arg_error(1, "interval is empty"));
    }

    let range = (up as u64).wrapping_sub(low as u64);
    let p = project_from_upvalue(state, rv, range)?;
    state.push(LuaValue::Int((p as u64).wrapping_add(low as u64) as i64));
    Ok(1)
}

/// `math.randomseed([x [, y]])` — seed the PRNG; returns two seed values.
///
fn math_randomseed(state: &mut LuaState) -> Result<usize, LuaError> {
    // TODO(port): same upvalue userdata access issue as math_random.
    if matches!(state.type_at(1), LuaType::None) {
        // randseed uses time(NULL) and address of L for entropy.
        apply_random_seed(state)?;
    } else {
        //    lua_Integer n2 = luaL_optinteger(L, 2, 0);
        let n1 = state.check_integer(1)? as u64;
        let n2 = state.opt_integer(2, 0)? as u64;
        apply_set_seed(state, n1, n2)?;
    }
    Ok(2)
}

/// Advance the PRNG stored in the thread-local `RAN_STATE` and return the
/// raw 64-bit output.
///
/// PORT NOTE: In C this draws from the userdata in closure upvalue 1. The
/// Rust port stores the PRNG state in a thread-local until typed-userdata
/// closure upvalues are wired up. Storage location is the only difference;
/// the algorithm is unchanged.
fn advance_prng(_state: &mut LuaState) -> Result<u64, LuaError> {
    Ok(RAN_STATE.with(|r| next_rand(&mut r.borrow_mut().s)))
}

/// Apply rejection sampling for `math.random` using the thread-local PRNG.
///
/// PORT NOTE: see `advance_prng` for the thread-local rationale.
fn project_from_upvalue(
    _state: &mut LuaState,
    ran: u64,
    n: u64,
) -> Result<u64, LuaError> {
    Ok(RAN_STATE.with(|r| project(ran, n, &mut r.borrow_mut().s)))
}

/// Seed the PRNG from wall-clock time (entropy source).
///
///
/// TODO(port): must write n1 and n2 back to the upvalue RanState.
fn apply_random_seed(state: &mut LuaState) -> Result<(), LuaError> {
    let entropy = state.global().entropy_hook.map(|hook| hook()).unwrap_or(0);
    let seed1 = entropy;
    // TODO(port): C also mixes address entropy; keep the second seed derived
    // deterministically unless a richer host entropy API is added.
    let seed2: u64 = entropy.rotate_left(17) ^ 0x9e37_79b9_7f4a_7c15;
    apply_set_seed(state, seed1, seed2)
}

/// Apply explicit seeds to the PRNG and push them onto the stack.
///
///
/// PORT NOTE: writes seeds into the thread-local RanState (see `advance_prng`).
fn apply_set_seed(state: &mut LuaState, n1: u64, n2: u64) -> Result<(), LuaError> {
    RAN_STATE.with(|r| set_seed_words(&mut r.borrow_mut().s, n1, n2));
    state.push(LuaValue::Int(n1 as i64));
    state.push(LuaValue::Int(n2 as i64));
    Ok(())
}

/// Register `math.random` and `math.randomseed` on the math library table at
/// stack top, after seeding the thread-local PRNG.
///
///
/// PORT NOTE: C stores the PRNG inside a userdata bound as upvalue 1 of both
/// closures. Until typed userdata closure upvalues are available, the Rust
/// port keeps the PRNG in a thread-local (see `RAN_STATE`) and registers the
/// functions as plain non-closure entries on the library table.
fn set_rand_func(state: &mut LuaState) -> Result<(), LuaError> {
    apply_random_seed(state)?;
    state.pop_n(2);

    state.push_c_function(math_random)?;
    state.set_field(-2, b"random")?;
    state.push_c_function(math_randomseed)?;
    state.set_field(-2, b"randomseed")?;
    Ok(())
}

// ── Library registration table ────────────────────────────────────────────

/// The `math` library function table.
///
///
/// Placeholder entries (`None`) are filled in manually by `luaopen_math`
/// (`pi`, `huge`, `maxinteger`, `mininteger`) or by `set_rand_func`
/// (`random`, `randomseed`).
#[expect(dead_code, reason = "ported stdlib helper; not yet wired into the runtime")]
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
///
/// `LUAMOD_API` → `pub` (see macros.tsv).
pub fn luaopen_math(state: &mut LuaState) -> Result<usize, LuaError> {
    // Creates a new table and registers all non-None entries from MATHLIB.
    state.new_lib(MATHLIB_FUNCS)?;

    state.push(LuaValue::Float(PI));
    state.set_field(-2, b"pi")?;

    state.push(LuaValue::Float(f64::INFINITY));
    state.set_field(-2, b"huge")?;

    // LUA_MAXINTEGER = i64::MAX (lua_Integer is int64_t in default config).
    state.push(LuaValue::Int(i64::MAX));
    state.set_field(-2, b"maxinteger")?;

    state.push(LuaValue::Int(i64::MIN));
    state.set_field(-2, b"mininteger")?;

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
