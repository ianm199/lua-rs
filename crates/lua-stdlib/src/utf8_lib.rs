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
use lua_types::value::{LuaValue, LuaClosure};
use lua_vm::state::LuaState;

// C: #define MAXUNICODE 0x10FFFFu
const MAX_UNICODE: u32 = 0x10_FFFF;

// C: #define MAXUTF 0x7FFFFFFFu
const MAX_UTF: u32 = 0x7FFF_FFFF;

// C: typedef unsigned int utfint;
// 31 bits are needed for MAX_UTF; u32 is sufficient on all Rust targets.
type UtfInt = u32;

// C: #define UTF8PATT "[\0-\x7F\xC2-\xFD][\x80-\xBF]*"
// sizeof(UTF8PATT)/sizeof(char) - 1 = 14 bytes (contains an embedded NUL).
const UTF8_PATT: &[u8] = b"[\x00-\x7F\xC2-\xFD][\x80-\xBF]*";

// ── Internal helpers ───────────────────────────────────────────────────────

/// Translate a relative string position: negative values count backward from end.
///
/// C: `static lua_Integer u_posrelat(lua_Integer pos, size_t len)` (strlib copy)
fn pos_relat(pos: i64, len: usize) -> i64 {
    // C: if (pos >= 0) return pos;
    if pos >= 0 {
        pos
    } else {
        // C: else if (0u - (size_t)pos > len) return 0;
        // 0u - (size_t)pos is the magnitude of pos as an unsigned value.
        let abs_pos = pos.unsigned_abs() as u64;
        if abs_pos > len as u64 {
            0
        } else {
            // C: else return (lua_Integer)len + pos + 1;
            len as i64 + pos + 1
        }
    }
}

/// Return `true` if byte `c` is a UTF-8 continuation byte (`10xxxxxx`).
///
/// C: `#define iscont(c)  (((c) & 0xC0) == 0x80)`
#[inline]
fn is_cont(c: u8) -> bool {
    (c & 0xC0) == 0x80
}

/// Return `true` if the byte at 0-based index `pos` in `s` is a continuation
/// byte, treating out-of-bounds positions as non-continuation.
///
/// C: `#define iscontp(p)  iscont(*(p))` — where `p = s + pos`.
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
/// C: `static const char *utf8_decode(const char *s, utfint *val, int strict)`
fn utf8_decode(s: &[u8], strict: bool) -> Option<(&[u8], UtfInt)> {
    // C: static const utfint limits[] = {~(utfint)0, 0x80, 0x800, 0x10000u, 0x200000u, 0x4000000u};
    // LIMITS[count] is the minimum value for a sequence with `count` continuation bytes.
    // LIMITS[0] = u32::MAX forces an error when a non-ASCII byte has no continuation bytes.
    const LIMITS: [UtfInt; 6] = [u32::MAX, 0x80, 0x800, 0x10000, 0x200000, 0x4000000];

    if s.is_empty() {
        return None;
    }

    // C: unsigned int c = (unsigned char)s[0];
    let mut c = s[0] as u32;
    let res: UtfInt;
    let advance: usize;

    if c < 0x80 {
        // ASCII fast path — no continuation bytes needed.
        res = c;
        advance = 1;
    } else {
        // C: int count = 0; utfint res = 0;
        let mut count: usize = 0;
        let mut r: UtfInt = 0;

        // C: for (; c & 0x40; c <<= 1) { unsigned int cc = (unsigned char)s[++count]; ... }
        // The C for-loop runs the body first, then applies `c <<= 1` as the update.
        while c & 0x40 != 0 {
            // C: unsigned int cc = (unsigned char)s[++count];
            count += 1;
            if count >= s.len() {
                return None; // string too short for the indicated sequence length
            }
            let cc = s[count] as u32;

            // C: if (!iscont(cc)) return NULL;
            if (cc & 0xC0) != 0x80 {
                return None; // expected continuation byte, got something else
            }

            // C: res = (res << 6) | (cc & 0x3F);
            r = (r << 6) | (cc & 0x3F);

            // C for-loop update: c <<= 1
            c <<= 1;
        }

        // C: res |= ((utfint)(c & 0x7F) << (count * 5));
        r |= (c & 0x7F) << (count as u32 * 5);

        // C: if (count > 5 || res > MAXUTF || res < limits[count]) return NULL;
        if count > 5 || r > MAX_UTF || r < LIMITS[count] {
            return None; // invalid (overlong, too large, or excess continuation bytes)
        }

        res = r;
        // C: s += count; return s + 1; → total bytes consumed = count + 1
        advance = count + 1;
        if advance > s.len() {
            return None;
        }
    }

    // C: if (strict) { if (res > MAXUNICODE || (0xD800u <= res && res <= 0xDFFFu)) return NULL; }
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
/// C: `int luaO_utf8esc(char *buff, unsigned long x)` (lobject.c)
fn encode_utf8_codepoint(code: u32) -> Vec<u8> {
    debug_assert!(code <= MAX_UTF);

    // C: if (x < 0x80) buff[UTF8BUFFSZ - 1] = cast_char(x);
    if code < 0x80 {
        return vec![code as u8];
    }

    let mut x = code;
    // C: unsigned int mfb = 0x3f;  — maximum value that fits in the first byte
    let mut mfb: u32 = 0x3F;
    // Continuation bytes built in reverse, then reversed at the end.
    let mut bytes_rev: Vec<u8> = Vec::with_capacity(6);

    // C: do { buff[UTF8BUFFSZ - (n++)] = cast_char(0x80 | (x & 0x3f)); x >>= 6; mfb >>= 1; }
    //    while (x > mfb);
    loop {
        bytes_rev.push(0x80 | (x & 0x3F) as u8);
        x >>= 6;
        mfb >>= 1;
        if x <= mfb {
            break;
        }
    }

    // C: buff[UTF8BUFFSZ - n] = cast_char((~mfb << 1) | x);
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
/// C: `static int utflen(lua_State *L)`
fn utf_len(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: const char *s = luaL_checklstring(L, 1, &len);
    // Clone to avoid holding a borrow across subsequent mutable state calls.
    let s: Vec<u8> = state.check_arg_string(1)?.to_vec();
    let len = s.len();

    // C: lua_Integer posi = u_posrelat(luaL_optinteger(L, 2, 1), len);
    // TODO(port): opt_arg_integer(narg, default) not yet in LuaState API; adjust in Phase B.
    let raw_posi: i64 = state.opt_arg_integer(2, 1)?;
    let mut posi: i64 = pos_relat(raw_posi, len);

    // C: lua_Integer posj = u_posrelat(luaL_optinteger(L, 3, -1), len);
    // TODO(port): opt_arg_integer API (second call site).
    let raw_posj: i64 = state.opt_arg_integer(3, -1)?;
    let mut posj: i64 = pos_relat(raw_posj, len);

    // C: int lax = lua_toboolean(L, 4);
    // TODO(port): to_boolean(n) method not yet confirmed in LuaState API.
    let lax: bool = state.to_boolean(4);

    // C: luaL_argcheck(L, 1 <= posi && --posi <= (lua_Integer)len, 2, ...);
    // Note: C short-circuits, so --posi only executes when 1 <= posi.
    if posi < 1 {
        return Err(LuaError::arg_error(2, "initial position out of bounds"));
    }
    posi -= 1; // 1-based → 0-based
    if posi > len as i64 {
        return Err(LuaError::arg_error(2, "initial position out of bounds"));
    }

    // C: luaL_argcheck(L, --posj < (lua_Integer)len, 3, ...);
    posj -= 1; // 1-based → 0-based (always decremented, no short-circuit)
    if posj >= len as i64 {
        return Err(LuaError::arg_error(3, "final position out of bounds"));
    }

    let mut n: i64 = 0;

    // C: while (posi <= posj) { const char *s1 = utf8_decode(s + posi, NULL, !lax); ... }
    while posi <= posj {
        match utf8_decode(&s[posi as usize..], !lax) {
            None => {
                // C: luaL_pushfail(L); lua_pushinteger(L, posi + 1); return 2;
                state.push(LuaValue::Nil); // luaL_pushfail
                state.push(LuaValue::Int(posi + 1)); // 1-based position of failure
                return Ok(2);
            }
            Some((remaining, _)) => {
                // C: posi = s1 - s;  (s1 points past the decoded bytes)
                posi = (len - remaining.len()) as i64;
                n += 1;
            }
        }
    }

    // C: lua_pushinteger(L, n); return 1;
    state.push(LuaValue::Int(n));
    Ok(1)
}

/// `utf8.codepoint(s [, i [, j [, lax]]])` → integer, ...
///
/// Returns the codepoints (as integers) for all characters starting in `s[i..j]`.
///
/// C: `static int codepoint(lua_State *L)`
fn codepoint(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: const char *s = luaL_checklstring(L, 1, &len);
    let s: Vec<u8> = state.check_arg_string(1)?.to_vec();
    let len = s.len();

    // C: lua_Integer posi = u_posrelat(luaL_optinteger(L, 2, 1), len);
    // TODO(port): opt_arg_integer API (codepoint start position).
    let raw_posi: i64 = state.opt_arg_integer(2, 1)?;
    let posi: i64 = pos_relat(raw_posi, len);

    // C: lua_Integer pose = u_posrelat(luaL_optinteger(L, 3, posi), len);
    // Default for the end position is posi (1-based), giving a single character.
    // TODO(port): opt_arg_integer API (codepoint end position).
    let raw_pose: i64 = state.opt_arg_integer(3, posi)?;
    let pose: i64 = pos_relat(raw_pose, len);

    // C: int lax = lua_toboolean(L, 4);
    // TODO(port): to_boolean API (codepoint lax mode).
    let lax: bool = state.to_boolean(4);

    // C: luaL_argcheck(L, posi >= 1, 2, "out of bounds");
    if posi < 1 {
        return Err(LuaError::arg_error(2, "out of bounds"));
    }

    // C: luaL_argcheck(L, pose <= (lua_Integer)len, 3, "out of bounds");
    if pose > len as i64 {
        return Err(LuaError::arg_error(3, "out of bounds"));
    }

    // C: if (posi > pose) return 0;
    if posi > pose {
        return Ok(0); // empty interval: no values
    }

    // C: if (pose - posi >= INT_MAX) return luaL_error(L, "string slice too long");
    if pose - posi >= i32::MAX as i64 {
        return Err(LuaError::runtime(format_args!("string slice too long")));
    }

    // C: n = (int)(pose - posi) + 1; luaL_checkstack(L, n, "string slice too long");
    let n_max = (pose - posi + 1) as usize;
    state.ensure_stack(n_max, "string slice too long")?;

    // C: se = s + pose; for (s += posi - 1; s < se;) { ... }
    // 0-based: start at (posi - 1), stop before byte index `pose`.
    let mut pos: usize = (posi - 1) as usize; // 0-based start
    let end: usize = pose as usize; // 0-based exclusive end
    let mut count: usize = 0;

    while pos < end {
        // C: s = utf8_decode(s, &code, !lax); if (s == NULL) return luaL_error(L, MSGInvalid);
        match utf8_decode(&s[pos..], !lax) {
            None => return Err(LuaError::runtime(format_args!("invalid UTF-8 code"))),
            Some((remaining, code)) => {
                // C: lua_pushinteger(L, code); n++;
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
/// C: `static void pushutfchar(lua_State *L, int arg)` — restructured to return
/// `Vec<u8>` directly rather than pushing to the stack, avoiding the push/pop
/// dance that `luaL_Buffer` required.
///
/// PORT NOTE: C's `pushutfchar` called `lua_pushfstring(L, "%U", code)` to encode
/// and push in one step. Here the encoding is extracted so `utf_char` can build
/// the concatenated result without intermediate stack operations.
fn get_utf_char_bytes(state: &mut LuaState, arg: i32) -> Result<Vec<u8>, LuaError> {
    // C: lua_Unsigned code = (lua_Unsigned)luaL_checkinteger(L, arg);
    let code = state.check_arg_integer(arg)? as u64;

    // C: luaL_argcheck(L, code <= MAXUTF, arg, "value out of range");
    if code > MAX_UTF as u64 {
        return Err(LuaError::arg_error(arg, "value out of range"));
    }

    Ok(encode_utf8_codepoint(code as u32))
}

/// `utf8.char(n1, n2, ...)` → string
///
/// Returns a string formed by the UTF-8 encoding of the given codepoints.
///
/// C: `static int utfchar(lua_State *L)`
fn utf_char(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: int n = lua_gettop(L);
    // TODO(port): stack_top() / arg_count() API on LuaState not yet confirmed.
    let n: i32 = state.stack_top() as i32;

    if n == 1 {
        // C: pushutfchar(L, 1);  — optimized single-character path
        let bytes = get_utf_char_bytes(state, 1)?;
        let s = state.intern_str(&bytes);
        state.push(LuaValue::Str(s));
    } else {
        // C: luaL_Buffer b; luaL_buffinit(L, &b);
        //    for (i = 1; i <= n; i++) { pushutfchar(L, i); luaL_addvalue(&b); }
        //    luaL_pushresult(&b);
        // PORT NOTE: luaL_Buffer replaced by Vec<u8>; codepoints are encoded
        // directly into the accumulator without intermediate stack push/pop.
        let mut buf: Vec<u8> = Vec::new();
        for i in 1..=n {
            buf.extend_from_slice(&get_utf_char_bytes(state, i)?);
        }
        let s = state.intern_str(&buf);
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
/// C: `static int byteoffset(lua_State *L)`
fn byte_offset(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: const char *s = luaL_checklstring(L, 1, &len);
    let s: Vec<u8> = state.check_arg_string(1)?.to_vec();
    let len = s.len();

    // C: lua_Integer n = luaL_checkinteger(L, 2);
    let n: i64 = state.check_arg_integer(2)?;

    // C: lua_Integer posi = (n >= 0) ? 1 : len + 1;
    let default_posi: i64 = if n >= 0 { 1 } else { len as i64 + 1 };

    // C: posi = u_posrelat(luaL_optinteger(L, 3, posi), len);
    // TODO(port): opt_arg_integer API (byte_offset position argument).
    let raw_posi: i64 = state.opt_arg_integer(3, default_posi)?;
    let posi_1based: i64 = pos_relat(raw_posi, len);

    // C: luaL_argcheck(L, 1 <= posi && --posi <= (lua_Integer)len, 3, "position out of bounds");
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
        // C: while (posi > 0 && iscontp(s + posi)) posi--;
        // Scan backward to find the start of the character containing `posi`.
        while posi > 0 && is_cont_at(&s, posi) {
            posi -= 1;
        }
        // count remains 0
    } else {
        // C: if (iscontp(s + posi)) return luaL_error(L, "initial position is a continuation byte");
        if is_cont_at(&s, posi) {
            return Err(LuaError::runtime(format_args!(
                "initial position is a continuation byte"
            )));
        }

        if count < 0 {
            // C: while (n < 0 && posi > 0) {
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
            // C: n--;
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

    // C: if (n == 0) lua_pushinteger(L, posi + 1); else luaL_pushfail(L);
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
/// C: `static int iter_aux(lua_State *L, int strict)`
fn iter_aux(state: &mut LuaState, strict: bool) -> Result<usize, LuaError> {
    // C: const char *s = luaL_checklstring(L, 1, &len);
    let s: Vec<u8> = state.check_arg_string(1)?.to_vec();
    let len = s.len();

    // C: lua_Unsigned n = (lua_Unsigned)lua_tointeger(L, 2);
    // TODO(port): to_integer(n) exact return type (i64/Option<i64>) not yet confirmed;
    // treating as i64 cast to u64 for unsigned byte-index arithmetic.
    let mut n: u64 = state.to_integer(2) as u64;

    // C: if (n < len) { while (iscontp(s + n)) n++; }
    if (n as usize) < len {
        while (n as usize) < len && is_cont(s[n as usize]) {
            n += 1;
        }
    }

    // C: if (n >= len) return 0;
    if (n as usize) >= len {
        return Ok(0); // no more codepoints
    }

    // C: const char *next = utf8_decode(s + n, &code, strict);
    //    if (next == NULL || iscontp(next)) return luaL_error(L, MSGInvalid);
    match utf8_decode(&s[n as usize..], strict) {
        None => Err(LuaError::runtime(format_args!("invalid UTF-8 code"))),
        Some((remaining, code)) => {
            let next_pos = len - remaining.len(); // 0-based index of the next character
            // C: iscontp(next) — an unexpected continuation byte immediately after a
            // valid sequence indicates a malformed input stream.
            if next_pos < len && is_cont(s[next_pos]) {
                return Err(LuaError::runtime(format_args!("invalid UTF-8 code")));
            }
            // C: lua_pushinteger(L, n + 1); lua_pushinteger(L, code); return 2;
            state.push(LuaValue::Int((n + 1) as i64)); // 1-based position for next iteration
            state.push(LuaValue::Int(code as i64));
            Ok(2)
        }
    }
}

/// Strict iterator body: rejects surrogates and values > MAX_UNICODE.
///
/// C: `static int iter_auxstrict(lua_State *L)`
fn iter_aux_strict(state: &mut LuaState) -> Result<usize, LuaError> {
    iter_aux(state, true)
}

/// Lax iterator body: accepts extended UTF-8 up to MAX_UTF.
///
/// C: `static int iter_auxlax(lua_State *L)`
fn iter_aux_lax(state: &mut LuaState) -> Result<usize, LuaError> {
    iter_aux(state, false)
}

/// `utf8.codes(s [, lax])` → function, string, integer
///
/// Returns the iterator triple `(f, s, 0)` for use in a generic for loop.
/// Each call to `f(s, pos)` returns the next `(pos, codepoint)` pair.
///
/// C: `static int iter_codes(lua_State *L)`
fn iter_codes(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: int lax = lua_toboolean(L, 2);
    // TODO(port): to_boolean API (iter_codes lax mode).
    let lax: bool = state.to_boolean(2);

    // C: const char *s = luaL_checkstring(L, 1);
    let s: Vec<u8> = state.check_arg_string(1)?.to_vec();

    // C: luaL_argcheck(L, !iscontp(s), 1, MSGInvalid);
    // The very first byte of the string must not be a continuation byte.
    if s.first().map_or(false, |&b| is_cont(b)) {
        return Err(LuaError::arg_error(1, "invalid UTF-8 code"));
    }

    // C: lua_pushcfunction(L, lax ? iter_auxlax : iter_auxstrict);
    // TODO(port): verify LuaClosure::LightC wraps fn(&mut LuaState)->Result<usize,LuaError>.
    let iter_fn: fn(&mut LuaState) -> Result<usize, LuaError> =
        if lax { iter_aux_lax } else { iter_aux_strict };
    state.push(LuaValue::Function(LuaClosure::LightC(iter_fn)));

    // C: lua_pushvalue(L, 1);  — push the string argument as the loop invariant
    // TODO(port): push_value_at(idx) not yet confirmed in LuaState API.
    state.push_value_at(1)?;

    // C: lua_pushinteger(L, 0);  — initial control variable (byte position 0)
    state.push(LuaValue::Int(0));

    Ok(3)
}

// ── Library registration ───────────────────────────────────────────────────

/// Function registration table for the `utf8` library.
///
/// C: `static const luaL_Reg funcs[]`
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
/// C: `LUAMOD_API int luaopen_utf8(lua_State *L)`
pub fn open_utf8(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: luaL_newlib(L, funcs);
    // TODO(port): new_lib(funcs) API on LuaState not yet confirmed; expected to
    // create a new table and register all (name, fn) pairs from FUNCS.
    state.new_lib(FUNCS)?;

    // C: lua_pushlstring(L, UTF8PATT, sizeof(UTF8PATT)/sizeof(char) - 1);
    let patt = state.intern_str(UTF8_PATT);
    state.push(LuaValue::Str(patt));

    // C: lua_setfield(L, -2, "charpattern");
    // TODO(port): set_field(table_idx, field_name) API on LuaState not yet confirmed.
    state.set_field(-2, b"charpattern")?;

    Ok(1)
}

// ──────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        src/lutf8lib.c  (291 lines, 9 functions)
//   target_crate:  lua-stdlib
//   confidence:    medium
//   todos:         13
//   port_notes:    2
//   unsafe_blocks: 0   (must be 0 outside lua-gc/lua-coro)
//   notes:         Core UTF-8 logic (utf8_decode, encode_utf8_codepoint,
//                  pos_relat, is_cont_at) is a faithful translation and should
//                  be correct. All 13 TODOs are unresolved LuaState API names:
//                  opt_arg_integer, to_boolean, stack_top, push_value_at,
//                  new_lib, set_field, and to_integer — Phase B reconciles
//                  these against the actual method signatures. No unsafe
//                  blocks; NUL-terminator reliance in C replaced by Rust
//                  bounds checks throughout.
// ──────────────────────────────────────────────────────────────────────────
