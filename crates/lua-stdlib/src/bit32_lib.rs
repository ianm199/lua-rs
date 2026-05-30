//! `bit32` — the Lua 5.2/5.3 32-bit bitwise library.
//!
//! This library was present (default-on) in Lua 5.3 and removed in Lua 5.4
//! (`specs/research/5.3-upstream-delta.md` delta #11). Its operations mask
//! every operand and result to **32 bits**, which is distinct from 5.3's
//! native 64-bit `&`/`|`/`~`/`<<`/`>>` operators (`5.3-upstream-delta.md`
//! risk #5). We register it only under the 5.3 backend.
//!
//! PRELIMINARY: this is a minimal, exploratory subset proving the per-version
//! stdlib-roster seam — it implements the most common operations. The full
//! 5.2/5.3 surface (`btest`, `extract`, `replace`, `lrotate`, `rrotate`,
//! `arshift`) is left as a clear TODO below.

use crate::state_stub::{LuaState, LuaStateStubExt as _};
use lua_types::{LuaError, LuaValue};

type LuaCFunction = fn(&mut LuaState) -> Result<usize, LuaError>;

/// Mask a Lua integer argument down to an unsigned 32-bit value, matching
/// `bit32`'s `lua_Unsigned`-truncation semantics.
fn arg_u32(state: &mut LuaState, arg: i32) -> Result<u32, LuaError> {
    let n = state.check_integer(arg)?;
    Ok(n as u32)
}

/// Push an unsigned 32-bit result as a Lua integer.
fn push_u32(state: &mut LuaState, v: u32) {
    state.push(LuaValue::Int(v as i64));
}

/// Fold a variadic AND/OR/XOR over every argument, starting from `init`.
fn fold(
    state: &mut LuaState,
    init: u32,
    op: fn(u32, u32) -> u32,
) -> Result<usize, LuaError> {
    let top = state.get_top();
    let mut acc = init;
    for i in 1..=top {
        acc = op(acc, arg_u32(state, i)?);
    }
    push_u32(state, acc & 0xFFFF_FFFF);
    Ok(1)
}

fn bit_band(state: &mut LuaState) -> Result<usize, LuaError> {
    fold(state, 0xFFFF_FFFF, |a, b| a & b)
}

fn bit_bor(state: &mut LuaState) -> Result<usize, LuaError> {
    fold(state, 0, |a, b| a | b)
}

fn bit_bxor(state: &mut LuaState) -> Result<usize, LuaError> {
    fold(state, 0, |a, b| a ^ b)
}

fn bit_bnot(state: &mut LuaState) -> Result<usize, LuaError> {
    let a = arg_u32(state, 1)?;
    push_u32(state, !a);
    Ok(1)
}

fn bit_lshift(state: &mut LuaState) -> Result<usize, LuaError> {
    let a = arg_u32(state, 1)?;
    let disp = state.check_integer(2)?;
    push_u32(state, shift(a, disp));
    Ok(1)
}

fn bit_rshift(state: &mut LuaState) -> Result<usize, LuaError> {
    let a = arg_u32(state, 1)?;
    let disp = state.check_integer(2)?;
    push_u32(state, shift(a, -disp));
    Ok(1)
}

/// `bit32` logical shift: positive `disp` shifts left, negative shifts right;
/// a displacement of 32 or more (in magnitude) yields 0, matching 5.3.
fn shift(x: u32, disp: i64) -> u32 {
    if disp <= -32 || disp >= 32 {
        0
    } else if disp >= 0 {
        x << disp
    } else {
        x >> (-disp)
    }
}

/// `w` low bits set, matching `bit32`'s field mask (`width` in `1..=32`).
fn mask_w(w: u32) -> u32 {
    if w >= 32 {
        0xFFFF_FFFF
    } else {
        (1u32 << w) - 1
    }
}

/// Validate and return the `(field, width)` pair for `extract`/`replace`,
/// matching Lua 5.2's `fieldargs` bounds checks. `width_arg` defaults to 1.
fn field_args(
    state: &mut LuaState,
    field_arg: i32,
    width_arg: i32,
) -> Result<(u32, u32), LuaError> {
    let f = state.check_integer(field_arg)?;
    let w = if state.get_top() >= width_arg {
        state.check_integer(width_arg)?
    } else {
        1
    };
    if f < 0 {
        return Err(LuaError::arg_error(field_arg, "field cannot be negative"));
    }
    if w < 1 {
        return Err(LuaError::arg_error(width_arg, "width must be positive"));
    }
    if f + w > 32 {
        return Err(LuaError::arg_error(
            field_arg,
            "trying to access non-existent bits",
        ));
    }
    Ok((f as u32, w as u32))
}

/// `bit32.btest(...)` — true iff the AND of all arguments is non-zero.
fn bit_btest(state: &mut LuaState) -> Result<usize, LuaError> {
    let top = state.get_top();
    let mut acc: u32 = 0xFFFF_FFFF;
    for i in 1..=top {
        acc &= arg_u32(state, i)?;
    }
    state.push(LuaValue::Bool(acc != 0));
    Ok(1)
}

/// `bit32.extract(n, field [, width])` — the `width` bits of `n` at `field`.
fn bit_extract(state: &mut LuaState) -> Result<usize, LuaError> {
    let n = arg_u32(state, 1)?;
    let (f, w) = field_args(state, 2, 3)?;
    push_u32(state, (n >> f) & mask_w(w));
    Ok(1)
}

/// `bit32.replace(n, v, field [, width])` — `n` with its `width` bits at
/// `field` replaced by the low bits of `v`.
fn bit_replace(state: &mut LuaState) -> Result<usize, LuaError> {
    let n = arg_u32(state, 1)?;
    let v = arg_u32(state, 2)?;
    let (f, w) = field_args(state, 3, 4)?;
    let m = mask_w(w);
    push_u32(state, (n & !(m << f)) | ((v & m) << f));
    Ok(1)
}

/// `bit32.arshift(x, disp)` — arithmetic right shift (sign-propagating);
/// negative `disp` shifts left.
fn bit_arshift(state: &mut LuaState) -> Result<usize, LuaError> {
    let x = arg_u32(state, 1)?;
    let disp = state.check_integer(2)?;
    let r = if disp < 0 {
        shift(x, -disp)
    } else if disp >= 32 {
        if x & 0x8000_0000 != 0 {
            0xFFFF_FFFF
        } else {
            0
        }
    } else if x & 0x8000_0000 != 0 {
        (x >> disp) | !(0xFFFF_FFFFu32 >> disp)
    } else {
        x >> disp
    };
    push_u32(state, r);
    Ok(1)
}

/// 32-bit rotate left by `disp` (mod 32); negative rotates right.
fn rotate(x: u32, disp: i64) -> u32 {
    let d = (((disp % 32) + 32) % 32) as u32;
    if d == 0 {
        x
    } else {
        (x << d) | (x >> (32 - d))
    }
}

fn bit_lrotate(state: &mut LuaState) -> Result<usize, LuaError> {
    let x = arg_u32(state, 1)?;
    let disp = state.check_integer(2)?;
    push_u32(state, rotate(x, disp));
    Ok(1)
}

fn bit_rrotate(state: &mut LuaState) -> Result<usize, LuaError> {
    let x = arg_u32(state, 1)?;
    let disp = state.check_integer(2)?;
    push_u32(state, rotate(x, -disp));
    Ok(1)
}

/// The `bit32` function roster — the full Lua 5.2/5.3 surface.
const BIT32_FUNCS: &[(&[u8], LuaCFunction)] = &[
    (b"band", bit_band),
    (b"bor", bit_bor),
    (b"bxor", bit_bxor),
    (b"bnot", bit_bnot),
    (b"lshift", bit_lshift),
    (b"rshift", bit_rshift),
    (b"btest", bit_btest),
    (b"extract", bit_extract),
    (b"replace", bit_replace),
    (b"arshift", bit_arshift),
    (b"lrotate", bit_lrotate),
    (b"rrotate", bit_rrotate),
];

/// Open the `bit32` library, leaving the populated table on the stack.
pub fn open_bit32(state: &mut LuaState) -> Result<usize, LuaError> {
    state.new_lib(BIT32_FUNCS)?;
    Ok(1)
}

// ──────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        src/lbitlib.c (Lua 5.2/5.3)
//   target_crate:  lua-stdlib
//   confidence:    low (preliminary multiversion scaffold)
//   todos:         1
//   port_notes:    0
//   unsafe_blocks: 0
//   notes:         Minimal 5.3-only bit32 subset (band/bor/bxor/bnot/lshift/
//                  rshift) proving the per-version stdlib roster seam. The
//                  remaining functions and exact error/range checks are TODO.
// ──────────────────────────────────────────────────────────────────────────
