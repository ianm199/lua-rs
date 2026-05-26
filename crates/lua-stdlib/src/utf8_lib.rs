//! UTF-8 standard library for Lua 5.4.
//!
//! Port of `lutf8lib.c` (291 lines, 9 functions).
//!
//! Provides the `utf8` module with `char`, `codepoint`, `codes`, `len`,
//! `offset`, and `charpattern`. Supports both strict (Unicode-conformant)
//! and lax (extended UTF-8, up to `MAX_UTF = 0x7FFFFFFF`) decoding modes.
//!
//! Strict mode rejects surrogates (U+D800..U+DFFF) and values above U+10FFFF.
//! Lax mode accepts any well-formed byte sequence with a value ≤ MAX_UTF.

use lua_types::error::LuaError;
use lua_types::value::LuaValue;
use crate::state_stub::{LuaState, LuaStateStubExt as _};

const MAX_UNICODE: u32 = 0x10_FFFF;

const MAX_UTF: u32 = 0x7FFF_FFFF;

// 31 bits are needed for MAX_UTF; u32 is sufficient on all Rust targets.
type UtfInt = u32;

// sizeof(UTF8PATT)/sizeof(char) - 1 = 14 bytes (contains an embedded NUL).
const UTF8_PATT: &[u8] = b"[\x00-\x7F\xC2-\xFD][\x80-\xBF]*";

// ── Internal helpers ───────────────────────────────────────────────────────

/// Translate a relative string position: negative values count backward from end.
///
fn pos_relat(pos: i64, len: usize) -> i64 {
    if pos >= 0 {
        pos
    } else {
        // 0u - (size_t)pos is the magnitude of pos as an unsigned value.
        let abs_pos = pos.unsigned_abs() as u64;
        if abs_pos > len as u64 {
            0
        } else {
            len as i64 + pos + 1
        }
    }
}

/// Return `true` if byte `c` is a UTF-8 continuation byte (`10xxxxxx`).
///
#[inline]
fn is_cont(c: u8) -> bool {
    (c & 0xC0) == 0x80
}

/// Return `true` if the byte at 0-based index `pos` in `s` is a continuation
/// byte, treating out-of-bounds positions as non-continuation.
///
/// C strings carry a NUL terminator that is never a continuation byte;
/// the bounds-check here replaces that guarantee.
#[inline]
fn is_cont_at(s: &[u8], pos: i64) -> bool {
    if pos < 0 {
        return false;
    }
    s.get(pos as usize).map_or(false, |&b| is_cont(b))
}

/// Decode one UTF-8 sequence from the start of `s`.
///
/// Returns `None` if the byte sequence is invalid.
/// Returns `Some((remaining_slice, codepoint))` on success.
///
/// When `strict` is `true`, surrogates and values above `MAX_UNICODE` are
/// rejected. When `false`, any value ≤ `MAX_UTF` is accepted (extended UTF-8).
///
fn utf8_decode(s: &[u8], strict: bool) -> Option<(&[u8], UtfInt)> {
    // LIMITS[count] is the minimum value for a sequence with `count` continuation bytes.
    // LIMITS[0] = u32::MAX forces an error when a non-ASCII byte has no continuation bytes.
    const LIMITS: [UtfInt; 6] = [u32::MAX, 0x80, 0x800, 0x10000, 0x200000, 0x4000000];

    if s.is_empty() {
        return None;
    }

    let mut c = s[0] as u32;
    let res: UtfInt;
    let advance: usize;

    if c < 0x80 {
        // ASCII fast path — no continuation bytes needed.
        res = c;
        advance = 1;
    } else {
        let mut count: usize = 0;
        let mut r: UtfInt = 0;

        // The C for-loop runs the body first, then applies `c <<= 1` as the update.
        while c & 0x40 != 0 {
            count += 1;
            if count >= s.len() {
                return None; // string too short for the indicated sequence length
            }
            let cc = s[count] as u32;

            if (cc & 0xC0) != 0x80 {
                return None; // expected continuation byte, got something else
            }

            r = (r << 6) | (cc & 0x3F);

            // C for-loop update: c <<= 1
            c <<= 1;
        }

        r |= (c & 0x7F) << (count as u32 * 5);

        if count > 5 || r > MAX_UTF || r < LIMITS[count] {
            return None; // invalid (overlong, too large, or excess continuation bytes)
        }

        res = r;
        advance = count + 1;
        if advance > s.len() {
            return None;
        }
    }

    if strict && (res > MAX_UNICODE || (0xD800 <= res && res <= 0xDFFF)) {
        return None; // surrogate or out-of-Unicode-range value in strict mode
    }

    Some((&s[advance..], res))
}

/// Encode a codepoint (≤ `MAX_UTF`) as extended UTF-8 bytes.
///
/// Mirrors `luaO_utf8esc` from `lobject.c`, which fills a fixed buffer backwards.
/// This Rust version builds the bytes naturally and returns a `Vec<u8>`.
///
fn encode_utf8_codepoint(code: u32) -> Vec<u8> {
    debug_assert!(code <= MAX_UTF);

    if code < 0x80 {
        return vec![code as u8];
    }

    let mut x = code;
    let mut mfb: u32 = 0x3F;
    // Continuation bytes built in reverse, then reversed at the end.
    let mut bytes_rev: Vec<u8> = Vec::with_capacity(6);

    //    while (x > mfb);
    loop {
        bytes_rev.push(0x80 | (x & 0x3F) as u8);
        x >>= 6;
        mfb >>= 1;
        if x <= mfb {
            break;
        }
    }

    // wrapping_shl avoids a Rust debug-mode overflow panic on `!mfb << 1`
    // (e.g., !0x1Fu32 = 0xFFFF_FFE0; << 1 = 0xFFFF_FFC0; as u8 = 0xC0).
    let leading = ((!mfb).wrapping_shl(1) as u8) | (x as u8);

    let mut result = Vec::with_capacity(bytes_rev.len() + 1);
    result.push(leading);
    for &b in bytes_rev.iter().rev() {
        result.push(b);
    }
    result
}

// ── Library functions ──────────────────────────────────────────────────────

/// `utf8.len(s [, i [, j [, lax]]])` → integer | (nil, integer)
///
/// Returns the number of UTF-8 characters that start in the byte range `[i,j]`
/// of string `s` (1-based, defaulting to the whole string).
/// On a malformed sequence, returns `(nil, position)` where `position` is the
/// 1-based byte offset of the first bad byte.
///
fn utf_len(state: &mut LuaState) -> Result<usize, LuaError> {
    // Clone to avoid holding a borrow across subsequent mutable state calls.
    let s: Vec<u8> = state.check_arg_string(1)?.to_vec();
    let len = s.len();

    let raw_posi: i64 = state.opt_arg_integer(2, 1)?;
    let mut posi: i64 = pos_relat(raw_posi, len);

    let raw_posj: i64 = state.opt_arg_integer(3, -1)?;
    let mut posj: i64 = pos_relat(raw_posj, len);

    let lax: bool = state.to_boolean(4);

    // Note: C short-circuits, so --posi only executes when 1 <= posi.
    if posi < 1 {
        return Err(LuaError::arg_error(2, "initial position out of bounds"));
    }
    posi -= 1; // 1-based → 0-based
    if posi > len as i64 {
        return Err(LuaError::arg_error(2, "initial position out of bounds"));
    }

    posj -= 1; // 1-based → 0-based (always decremented, no short-circuit)
    if posj >= len as i64 {
        return Err(LuaError::arg_error(3, "final position out of bounds"));
    }

    let mut n: i64 = 0;

    while posi <= posj {
        match utf8_decode(&s[posi as usize..], !lax) {
            None => {
                state.push(LuaValue::Nil); // luaL_pushfail
                state.push(LuaValue::Int(posi + 1)); // 1-based position of failure
                return Ok(2);
            }
            Some((remaining, _)) => {
                posi = (len - remaining.len()) as i64;
                n += 1;
            }
        }
    }

    state.push(LuaValue::Int(n));
    Ok(1)
}

/// `utf8.codepoint(s [, i [, j [, lax]]])` → integer, ...
///
/// Returns the codepoints (as integers) for all characters starting in `s[i..j]`.
///
fn codepoint(state: &mut LuaState) -> Result<usize, LuaError> {
    let s: Vec<u8> = state.check_arg_string(1)?.to_vec();
    let len = s.len();

    let raw_posi: i64 = state.opt_arg_integer(2, 1)?;
    let posi: i64 = pos_relat(raw_posi, len);

    // Default for the end position is posi (1-based), giving a single character.
    let raw_pose: i64 = state.opt_arg_integer(3, posi)?;
    let pose: i64 = pos_relat(raw_pose, len);

    let lax: bool = state.to_boolean(4);

    if posi < 1 {
        return Err(LuaError::arg_error(2, "out of bounds"));
    }

    if pose > len as i64 {
        return Err(LuaError::arg_error(3, "out of bounds"));
    }

    if posi > pose {
        return Ok(0); // empty interval: no values
    }

    if pose - posi >= i32::MAX as i64 {
        return Err(LuaError::runtime(format_args!("string slice too long")));
    }

    let n_max = (pose - posi + 1) as i32;
    state.ensure_stack(n_max, "string slice too long")?;

    // 0-based: start at (posi - 1), stop before byte index `pose`.
    let mut pos: usize = (posi - 1) as usize; // 0-based start
    let end: usize = pose as usize; // 0-based exclusive end
    let mut count: usize = 0;

    while pos < end {
        match utf8_decode(&s[pos..], !lax) {
            None => return Err(LuaError::runtime(format_args!("invalid UTF-8 code"))),
            Some((remaining, code)) => {
                state.push(LuaValue::Int(code as i64));
                count += 1;
                pos = len - remaining.len(); // advance by decoded character width
            }
        }
    }

    Ok(count)
}

/// Encode the codepoint at stack argument `arg` and return the UTF-8 bytes.
///
/// `Vec<u8>` directly rather than pushing to the stack, avoiding the push/pop
/// dance that `luaL_Buffer` required.
///
/// PORT NOTE: C's `pushutfchar` called `lua_pushfstring(L, "%U", code)` to encode
/// and push in one step. Here the encoding is extracted so `utf_char` can build
/// the concatenated result without intermediate stack operations.
fn get_utf_char_bytes(state: &mut LuaState, arg: i32) -> Result<Vec<u8>, LuaError> {
    let code = state.check_arg_integer(arg)? as u64;

    if code > MAX_UTF as u64 {
        return Err(LuaError::arg_error(arg, "value out of range"));
    }

    Ok(encode_utf8_codepoint(code as u32))
}

/// `utf8.char(n1, n2, ...)` → string
///
/// Returns a string formed by the UTF-8 encoding of the given codepoints.
///
fn utf_char(state: &mut LuaState) -> Result<usize, LuaError> {
    let n: i32 = state.stack_top() as i32;

    if n == 1 {
        let bytes = get_utf_char_bytes(state, 1)?;
        let s = state.intern_str(&bytes)?;
        state.push(LuaValue::Str(s));
    } else {
        //    for (i = 1; i <= n; i++) { pushutfchar(L, i); luaL_addvalue(&b); }
        //    luaL_pushresult(&b);
        // PORT NOTE: luaL_Buffer replaced by Vec<u8>; codepoints are encoded
        // directly into the accumulator without intermediate stack push/pop.
        let mut buf: Vec<u8> = Vec::new();
        for i in 1..=n {
            buf.extend_from_slice(&get_utf_char_bytes(state, i)?);
        }
        let s = state.intern_str(&buf)?;
        state.push(LuaValue::Str(s));
    }

    Ok(1)
}

/// `utf8.offset(s, n [, i])` → integer | nil
///
/// Returns the byte offset where the n-th character (counting from position `i`)
/// starts. Negative `n` counts from the end. `n == 0` returns the start of the
/// character that contains position `i`.
/// Returns `nil` if the character cannot be found.
///
fn byte_offset(state: &mut LuaState) -> Result<usize, LuaError> {
    let s: Vec<u8> = state.check_arg_string(1)?.to_vec();
    let len = s.len();

    let n: i64 = state.check_arg_integer(2)?;

    let default_posi: i64 = if n >= 0 { 1 } else { len as i64 + 1 };

    let raw_posi: i64 = state.opt_arg_integer(3, default_posi)?;
    let posi_1based: i64 = pos_relat(raw_posi, len);

    if posi_1based < 1 {
        return Err(LuaError::arg_error(3, "position out of bounds"));
    }
    let mut posi: i64 = posi_1based - 1; // 1-based → 0-based
    if posi > len as i64 {
        return Err(LuaError::arg_error(3, "position out of bounds"));
    }

    // `count` is a mutable copy of `n`; driven to 0 when the target character is found.
    let mut count = n;

    if count == 0 {
        // Scan backward to find the start of the character containing `posi`.
        while posi > 0 && is_cont_at(&s, posi) {
            posi -= 1;
        }
        // count remains 0
    } else {
        if is_cont_at(&s, posi) {
            return Err(LuaError::runtime(format_args!(
                "initial position is a continuation byte"
            )));
        }

        if count < 0 {
            //      do { posi--; } while (posi > 0 && iscontp(s + posi));
            //      n++;
            //    }
            while count < 0 && posi > 0 {
                // do-while: always decrements at least once, then skips back over
                // any continuation bytes to land on a leading byte.
                loop {
                    posi -= 1;
                    if posi == 0 || !is_cont_at(&s, posi) {
                        break;
                    }
                }
                count += 1;
            }
        } else {
            //    while (n > 0 && posi < (lua_Integer)len) {
            //      do { posi++; } while (iscontp(s + posi));  /* cannot pass '\0' */
            //      n--;
            //    }
            count -= 1; // do not move for the 1st character
            while count > 0 && posi < len as i64 {
                // C relies on the NUL terminator to stop the inner do-while.
                // Rust uses an explicit bounds check instead.
                loop {
                    posi += 1;
                    if !is_cont_at(&s, posi) {
                        break;
                    }
                }
                count -= 1;
            }
        }
    }

    if count == 0 {
        state.push(LuaValue::Int(posi + 1)); // 0-based → 1-based
    } else {
        state.push(LuaValue::Nil); // luaL_pushfail: character not found
    }
    Ok(1)
}

/// Internal iterator body shared by `iter_aux_strict` and `iter_aux_lax`.
///
/// Stack on entry (from the generic for): (1) string, (2) current byte position
/// (0-based; initially pushed as 0 by `iter_codes`).
///
/// Advances past any leading continuation bytes, decodes the next character,
/// and returns `(next_1based_pos, codepoint)`.  Returns nothing (0) when the
/// string is exhausted.
///
fn iter_aux(state: &mut LuaState, strict: bool) -> Result<usize, LuaError> {
    let s: Vec<u8> = state.check_arg_string(1)?.to_vec();
    let len = s.len();

    let mut n: u64 = state.to_integer(2).unwrap_or(0) as u64;

    if (n as usize) < len {
        while (n as usize) < len && is_cont(s[n as usize]) {
            n += 1;
        }
    }

    if (n as usize) >= len {
        return Ok(0); // no more codepoints
    }

    //    if (next == NULL || iscontp(next)) return luaL_error(L, MSGInvalid);
    match utf8_decode(&s[n as usize..], strict) {
        None => Err(LuaError::runtime(format_args!("invalid UTF-8 code"))),
        Some((remaining, code)) => {
            let next_pos = len - remaining.len(); // 0-based index of the next character
            // valid sequence indicates a malformed input stream.
            if next_pos < len && is_cont(s[next_pos]) {
                return Err(LuaError::runtime(format_args!("invalid UTF-8 code")));
            }
            state.push(LuaValue::Int((n + 1) as i64)); // 1-based position for next iteration
            state.push(LuaValue::Int(code as i64));
            Ok(2)
        }
    }
}

/// Strict iterator body: rejects surrogates and values > MAX_UNICODE.
///
fn iter_aux_strict(state: &mut LuaState) -> Result<usize, LuaError> {
    iter_aux(state, true)
}

/// Lax iterator body: accepts extended UTF-8 up to MAX_UTF.
///
fn iter_aux_lax(state: &mut LuaState) -> Result<usize, LuaError> {
    iter_aux(state, false)
}

/// `utf8.codes(s [, lax])` → function, string, integer
///
/// Returns the iterator triple `(f, s, 0)` for use in a generic for loop.
/// Each call to `f(s, pos)` returns the next `(pos, codepoint)` pair.
///
fn iter_codes(state: &mut LuaState) -> Result<usize, LuaError> {
    let lax: bool = state.to_boolean(2);

    let s: Vec<u8> = state.check_arg_string(1)?.to_vec();

    if s.first().map_or(false, |&b| is_cont(b)) {
        return Err(LuaError::arg_error(1, "invalid UTF-8 code"));
    }

    let iter_fn: fn(&mut LuaState) -> Result<usize, LuaError> =
        if lax { iter_aux_lax } else { iter_aux_strict };
    state.push_c_function(iter_fn)?;

    state.push_value_at(1)?;

    state.push(LuaValue::Int(0));

    Ok(3)
}

// ── Library registration ───────────────────────────────────────────────────

/// Function registration table for the `utf8` library.
///
/// "charpattern" is intentionally absent here; it is a string value and is
/// registered separately inside `open_utf8` via `lua_setfield`.
pub const FUNCS: &[(&[u8], fn(&mut LuaState) -> Result<usize, LuaError>)] = &[
    (b"offset", byte_offset),
    (b"codepoint", codepoint),
    (b"char", utf_char),
    (b"len", utf_len),
    (b"codes", iter_codes),
];

/// Open the `utf8` library.
///
/// Registers all functions from `FUNCS` into a new table, then sets
/// `utf8.charpattern` to the byte-string pattern matching one UTF-8 sequence.
///
pub fn open_utf8(state: &mut LuaState) -> Result<usize, LuaError> {
    state.new_lib(FUNCS)?;

    let patt = state.intern_str(UTF8_PATT)?;
    state.push(LuaValue::Str(patt));

    state.set_field(-2, b"charpattern")?;

    Ok(1)
}

// ──────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        src/lutf8lib.c  (291 lines, 9 functions)
//   target_crate:  lua-stdlib
//   confidence:    high
//   todos:         0
//   unsafe_blocks: 0   (must be 0 outside explicit unsafe-budget crates)
//   notes:         Core UTF-8 logic (utf8_decode, encode_utf8_codepoint,
//                  pos_relat, is_cont_at) is a faithful translation. LuaState
//                  API names reconciled against state_stub overrides. No unsafe
//                  blocks; NUL-terminator reliance in C replaced by Rust bounds
//                  checks throughout.
// ──────────────────────────────────────────────────────────────────────────
