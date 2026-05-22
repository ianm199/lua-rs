//! Standard library for string operations and pattern-matching.
//!
//! Port of `lstrlib.c` (Lua 5.4.7, 1875 lines, 46 functions).
//!
//! Sections:
//!   1. Basic string operations (byte, char, find, format, gmatch, gsub, len,
//!      lower, match, rep, reverse, sub, upper)
//!   2. Pattern-matching engine (MatchState + recursive matcher)
//!   3. String format (`string.format`)
//!   4. Pack / unpack (`string.pack`, `string.packsize`, `string.unpack`)
//!   5. Module registration (`luaopen_string`)

use lua_types::error::LuaError;
use lua_types::value::LuaValue;
use lua_types::arith::ArithOp;
use lua_types::gc::GcRef;
use lua_types::string::LuaString;
use lua_types::{LuaType, LuaStatus};
use crate::state_stub::{LuaState, LuaStateStubExt as _, lua_CFunction, upvalue_index, CompareOp, LuaDebug};

// ────────────────────────────────────────────────────────────────────────────
// Constants
// ────────────────────────────────────────────────────────────────────────────

// C: #define LUA_MAXCAPTURES 32
const LUA_MAX_CAPTURES: usize = 32;

// C: #define MAXCCALLS 200
const MAX_CC_CALLS: i32 = 200;

// C: #define L_ESC '%'
const L_ESC: u8 = b'%';

// C: #define SPECIALS "^$*+?.([%-"
const SPECIALS: &[u8] = b"^$*+?.([%-";

// C: #define CAP_UNFINISHED (-1)
const CAP_UNFINISHED: isize = -1;

// C: #define CAP_POSITION (-2)
const CAP_POSITION: isize = -2;

// C: #define MAX_ITEM 120
const MAX_ITEM: usize = 120;

// C: #define MAX_ITEMF (110 + DBL_MAX_10_EXP) ≈ 110 + 308 = 418
const MAX_ITEM_F: usize = 418;

// C: #define MAX_FORMAT 32
const MAX_FORMAT: usize = 32;

// C: #define MAXINTSIZE 16
const MAX_INT_SIZE: usize = 16;

// C: #define NB CHAR_BIT  (8)
const NB: u32 = 8;

// C: #define MC ((1 << NB) - 1)
const MC: u8 = 0xFF;

// C: #define SZINT ((int)sizeof(lua_Integer))
const SZINT: usize = 8; // sizeof(i64) == 8

// C: #define LUAL_PACKPADBYTE 0x00
const PACK_PAD_BYTE: u8 = 0x00;

// ────────────────────────────────────────────────────────────────────────────
// Pattern-matching types
// ────────────────────────────────────────────────────────────────────────────

/// One capture record inside MatchState.
///
/// C: `struct { const char *init; ptrdiff_t len; }`
/// In Rust, `init` is an index into `MatchState::src`; `len` is either a
/// non-negative actual length, `CAP_UNFINISHED`, or `CAP_POSITION`.
#[derive(Copy, Clone)]
struct Capture {
    /// Index into the source slice where this capture started.
    init: usize,
    /// CAP_UNFINISHED, CAP_POSITION, or non-negative byte count.
    len: isize,
}

impl Default for Capture {
    fn default() -> Self {
        Capture { init: 0, len: CAP_UNFINISHED }
    }
}

/// State threaded through the recursive pattern-matcher.
///
/// C: `typedef struct MatchState { ... } MatchState;`
/// Raw C pointers replaced by indices into `src` / `pat` slices.
struct MatchState<'a> {
    /// Source string being searched.
    src: &'a [u8],
    /// Pattern string.
    pat: &'a [u8],
    /// Recursion depth counter; decremented on entry, incremented on return.
    matchdepth: i32,
    /// Number of capture records currently in use.
    level: u8,
    /// Capture records indexed `0..level`.
    captures: [Capture; LUA_MAX_CAPTURES],
}

impl<'a> MatchState<'a> {
    fn new(src: &'a [u8], pat: &'a [u8]) -> Self {
        MatchState {
            src,
            pat,
            matchdepth: MAX_CC_CALLS,
            level: 0,
            captures: [Capture::default(); LUA_MAX_CAPTURES],
        }
    }

    fn reset_level(&mut self) {
        self.level = 0;
        debug_assert!(self.matchdepth == MAX_CC_CALLS);
    }
}

/// Iterator state for `string.gmatch`.
///
/// C: `typedef struct GMatchState { ... } GMatchState;`
/// Stored as userdata on the Lua stack in the C implementation; in Phase A we
/// represent it as a plain Rust struct.
///
/// TODO(port): In the real port, this needs to live in a Lua userdata object
/// so that Lua GC can see it. For now it's a plain struct passed by
/// `state.to_userdata()`.
struct GMatchState {
    /// Current position in `src` (index into the source slice).
    src_pos: usize,
    /// The pattern string (owned copy so it survives the closure).
    pat: Vec<u8>,
    /// End of the last match (to avoid zero-length infinite loops).
    last_match: Option<usize>,
    /// Source string (owned copy).
    src: Vec<u8>,
}

// ────────────────────────────────────────────────────────────────────────────
// Pack/unpack types
// ────────────────────────────────────────────────────────────────────────────

/// Pack/unpack format option.
///
/// C: `typedef enum KOption { Kint, Kuint, ... } KOption;`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KOption {
    Int,        // signed integers
    Uint,       // unsigned integers
    Float,      // single-precision float (C float)
    Number,     // Lua native float (lua_Number = f64)
    Double,     // double-precision float (C double)
    Char,       // fixed-length string
    Kstring,    // string with length prefix
    Zstr,       // zero-terminated string
    Padding,    // padding byte (x)
    Paddalign,  // padding to alignment (X)
    Nop,        // no-op (space, <, >, =, !)
}

/// Header state for pack/unpack format parsing.
///
/// C: `typedef struct Header { lua_State *L; int islittle; int maxalign; } Header;`
struct Header {
    is_little: bool,
    max_align: usize,
}

impl Header {
    fn new() -> Self {
        Header {
            is_little: cfg!(target_endian = "little"),
            max_align: 1,
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// §1  Basic string helpers
// ────────────────────────────────────────────────────────────────────────────

/// Translate a relative initial string position: negative means back from end;
/// result is clipped to `[1, ∞)`.
///
/// C: `static size_t posrelatI(lua_Integer pos, size_t len)`
fn pos_relat_i(pos: i64, len: usize) -> usize {
    if pos > 0 {
        pos as usize
    } else if pos == 0 {
        1
    } else if pos < -(len as i64) {
        1
    } else {
        len.wrapping_add(pos as usize).wrapping_add(1)
    }
}

/// Get an optional ending string position from argument `arg`, default `def`.
/// Negative means back from end; clipped to `[0, len]`.
///
/// C: `static size_t getendpos(lua_State *L, int arg, lua_Integer def, size_t len)`
fn get_end_pos(pos: i64, len: usize) -> usize {
    if pos > len as i64 {
        len
    } else if pos >= 0 {
        pos as usize
    } else if pos < -(len as i64) {
        0
    } else {
        len.wrapping_add(pos as usize).wrapping_add(1)
    }
}

// ────────────────────────────────────────────────────────────────────────────
// §2  Exported string functions (registered in strlib[])
// ────────────────────────────────────────────────────────────────────────────

/// `string.len(s)` — return byte-length of `s`.
///
/// C: `static int str_len(lua_State *L)`
pub fn str_len(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: luaL_checklstring(L, 1, &l); lua_pushinteger(L, (lua_Integer)l);
    let s = state.check_arg_string(1)?;
    let l = s.len();
    state.push(LuaValue::Int(l as i64));
    Ok(1)
}

/// `string.sub(s, i [, j])` — return substring.
///
/// C: `static int str_sub(lua_State *L)`
pub fn str_sub(state: &mut LuaState) -> Result<usize, LuaError> {
    let s = state.check_arg_string(1)?;
    let l = s.len();
    let start = pos_relat_i(state.check_arg_integer(2)?, l);
    let end_pos_raw = state.opt_arg_integer(3, -1)?;
    let end = get_end_pos(end_pos_raw, l);
    if start <= end {
        // C: lua_pushlstring(L, s + start - 1, (end - start) + 1);
        let slice = &s[(start - 1)..end];
        state.push_string(slice)?;
    } else {
        // C: lua_pushliteral(L, "");
        state.push_string(b"")?;
    }
    Ok(1)
}

/// `string.reverse(s)` — return string with bytes reversed.
///
/// C: `static int str_reverse(lua_State *L)`
pub fn str_reverse(state: &mut LuaState) -> Result<usize, LuaError> {
    let s = state.check_arg_string(1)?;
    let mut buf: Vec<u8> = s.iter().copied().rev().collect();
    state.push_bytes(&buf)?;
    Ok(1)
}

/// `string.lower(s)` — return lowercase copy.
///
/// C: `static int str_lower(lua_State *L)`
pub fn str_lower(state: &mut LuaState) -> Result<usize, LuaError> {
    let s = state.check_arg_string(1)?;
    let buf: Vec<u8> = s.iter().map(|&c| c.to_ascii_lowercase()).collect();
    state.push_bytes(&buf)?;
    Ok(1)
}

/// `string.upper(s)` — return uppercase copy.
///
/// C: `static int str_upper(lua_State *L)`
pub fn str_upper(state: &mut LuaState) -> Result<usize, LuaError> {
    let s = state.check_arg_string(1)?;
    let buf: Vec<u8> = s.iter().map(|&c| c.to_ascii_uppercase()).collect();
    state.push_bytes(&buf)?;
    Ok(1)
}

/// `string.rep(s, n [, sep])` — return `n` copies of `s` separated by `sep`.
///
/// C: `static int str_rep(lua_State *L)`
pub fn str_rep(state: &mut LuaState) -> Result<usize, LuaError> {
    let s = state.check_arg_string(1)?;
    let l = s.len();
    let n = state.check_arg_integer(2)?;
    let sep = state.opt_arg_string(3, b"")?;
    let lsep = sep.len();

    if n <= 0 {
        state.push_string(b"")?;
    } else {
        const MAXSIZE: usize = i32::MAX as usize;
        let per = l.checked_add(lsep)
            .ok_or_else(|| LuaError::runtime(format_args!("resulting string too large")))?;
        if per > MAXSIZE / (n as usize) {
            return Err(LuaError::runtime(format_args!("resulting string too large")));
        }
        let total = per * (n as usize) - lsep;

        let mut buf: Vec<u8> = Vec::with_capacity(total);
        let s_data = s.to_vec();
        let sep_data = sep.to_vec();
        for i in 0..(n as usize) {
            buf.extend_from_slice(&s_data);
            if i < (n as usize - 1) && lsep > 0 {
                buf.extend_from_slice(&sep_data);
            }
        }
        state.push_bytes(&buf)?;
    }
    Ok(1)
}

/// `string.byte(s [, i [, j]])` — return numeric codes of characters.
///
/// C: `static int str_byte(lua_State *L)`
pub fn str_byte(state: &mut LuaState) -> Result<usize, LuaError> {
    let s = state.check_arg_string(1)?;
    let l = s.len();
    let pi = state.opt_arg_integer(2, 1)?;
    let posi = pos_relat_i(pi, l);
    let pose_raw = state.opt_arg_integer(3, pi)?;
    let pose = get_end_pos(pose_raw, l);

    if posi > pose {
        return Ok(0); // empty interval
    }
    // C: if (l_unlikely(pose - posi >= (size_t)INT_MAX))
    //        return luaL_error(L, "string slice too long");
    let count = pose.saturating_sub(posi - 1) + 1;
    if count > i32::MAX as usize {
        return Err(LuaError::runtime(format_args!("string slice too long")));
    }
    let n = (pose - posi + 1) as usize;
    state.ensure_stack(n as i32, "string slice too long")?;

    for i in 0..n {
        state.push(LuaValue::Int(s[posi - 1 + i] as i64));
    }
    Ok(n)
}

/// `string.char(...)` — return string built from character codes.
///
/// C: `static int str_char(lua_State *L)`
pub fn str_char(state: &mut LuaState) -> Result<usize, LuaError> {
    let n = state.get_top();
    let mut buf = Vec::with_capacity(n as usize);
    for i in 1..=n {
        let c = state.check_arg_integer(i)? as u64;
        if c > u8::MAX as u64 {
            return Err(LuaError::arg_error(i, "value out of range"));
        }
        buf.push(c as u8);
    }
    state.push_bytes(&buf)?;
    Ok(1)
}

/// `string.dump(function [, strip])` — serialize a function as binary chunk.
///
/// C: `static int str_dump(lua_State *L)`
/// Uses `lua_dump` internally; the writer callback builds a buffer.
pub fn str_dump(state: &mut LuaState) -> Result<usize, LuaError> {
    state.check_arg_type(1, LuaType::Function)?;
    let strip = state.arg_to_bool(2);
    // C: lua_settop(L, 1);
    state.set_top(1);
    // TODO(port): state.dump_function(strip) needs to produce &[u8].
    // In the C code, lua_dump writes to a writer callback that fills a luaL_Buffer.
    // In Rust, state.dump() should return Vec<u8> or write to a &mut Vec<u8>.
    let bytes = state.dump_function(strip)
        .map_err(|_| LuaError::runtime(format_args!("unable to dump given function")))?;
    state.push_bytes(&bytes)?;
    Ok(1)
}

// ────────────────────────────────────────────────────────────────────────────
// §3  String metamethods (arithmetic coercion)
// ────────────────────────────────────────────────────────────────────────────

/// Try to coerce the argument at `arg` to a number, pushing it on the stack.
/// Returns true on success.
///
/// C: `static int tonum(lua_State *L, int arg)`
fn tonum(state: &mut LuaState, arg: i32) -> Result<bool, LuaError> {
    if state.type_at(arg) == LuaType::Number {
        let v = state.get_at(arg).clone();
        state.push(v);
        Ok(true)
    } else {
        // check whether it is a numerical string
        // C: const char *s = lua_tolstring(L, arg, &len);
        //    return (s != NULL && lua_stringtonumber(L, s) == len + 1);
        if let Some(s) = state.to_lua_string_bytes(arg) {
            let len = s.len();
            // PORT NOTE: string_to_number pushes the number if successful
            let pushed = state.string_to_number_push(&s)?;
            Ok(pushed == len + 1)
        } else {
            Ok(false)
        }
    }
}

/// Try to invoke the metamethod `mtname` on the two operands.
///
/// C: `static void trymt(lua_State *L, const char *mtname)`
fn trymt(state: &mut LuaState, mtname: &[u8]) -> Result<(), LuaError> {
    // C: lua_settop(L, 2); /* back to original arguments */
    state.set_top(2);
    // C: if (lua_type(L, 2) == LUA_TSTRING || !luaL_getmetafield(L, 2, mtname))
    //        luaL_error(...)
    let t2_is_string = state.type_at(2) == LuaType::String;
    let has_mm = state.get_meta_field(2, mtname)?;
    if t2_is_string || !has_mm {
        // C: luaL_error(L, "attempt to %s a '%s' with a '%s'", mtname + 2, ...)
        let op = &mtname[2..]; // skip "__"
        return Err(LuaError::runtime(format_args!(
            "attempt to {} a '{}' with a '{}'",
            op.escape_ascii(),
            state.type_name_at(-2).escape_ascii(),
            state.type_name_at(-1).escape_ascii(),
        )));
    }
    // C: lua_insert(L, -3); lua_call(L, 2, 1);
    state.insert(-3)?;
    state.call(2, 1)?;
    Ok(())
}

/// Generic arithmetic helper: coerce both args and call `op`, else try metamethod.
///
/// C: `static int arith(lua_State *L, int op, const char *mtname)`
fn arith(state: &mut LuaState, op: ArithOp, mtname: &[u8]) -> Result<usize, LuaError> {
    if tonum(state, 1)? && tonum(state, 2)? {
        state.arith(op)?;
    } else {
        trymt(state, mtname)?;
    }
    Ok(1)
}

/// C: `static int arith_add(lua_State *L)`
pub fn arith_add(state: &mut LuaState) -> Result<usize, LuaError> {
    arith(state, ArithOp::Add, b"__add")
}
/// C: `static int arith_sub(lua_State *L)`
pub fn arith_sub(state: &mut LuaState) -> Result<usize, LuaError> {
    arith(state, ArithOp::Sub, b"__sub")
}
/// C: `static int arith_mul(lua_State *L)`
pub fn arith_mul(state: &mut LuaState) -> Result<usize, LuaError> {
    arith(state, ArithOp::Mul, b"__mul")
}
/// C: `static int arith_mod(lua_State *L)`
pub fn arith_mod(state: &mut LuaState) -> Result<usize, LuaError> {
    arith(state, ArithOp::Mod, b"__mod")
}
/// C: `static int arith_pow(lua_State *L)`
pub fn arith_pow(state: &mut LuaState) -> Result<usize, LuaError> {
    arith(state, ArithOp::Pow, b"__pow")
}
/// C: `static int arith_div(lua_State *L)`
pub fn arith_div(state: &mut LuaState) -> Result<usize, LuaError> {
    arith(state, ArithOp::Div, b"__div")
}
/// C: `static int arith_idiv(lua_State *L)`
pub fn arith_idiv(state: &mut LuaState) -> Result<usize, LuaError> {
    arith(state, ArithOp::Idiv, b"__idiv")
}
/// C: `static int arith_unm(lua_State *L)`
pub fn arith_unm(state: &mut LuaState) -> Result<usize, LuaError> {
    arith(state, ArithOp::Unm, b"__unm")
}

// ────────────────────────────────────────────────────────────────────────────
// §4  Pattern-matching engine
// ────────────────────────────────────────────────────────────────────────────

/// Return `true` if `c` belongs to the character class `cl` (a `%x` letter).
///
/// C: `static int match_class(int c, int cl)`
fn match_class(c: u8, cl: u8) -> bool {
    let res = match cl.to_ascii_lowercase() {
        b'a' => c.is_ascii_alphabetic(),
        b'c' => c.is_ascii_control(),
        b'd' => c.is_ascii_digit(),
        b'g' => c.is_ascii_graphic(),
        b'l' => c.is_ascii_lowercase(),
        b'p' => c.is_ascii_punctuation(),
        b's' => c.is_ascii_whitespace(),
        b'u' => c.is_ascii_uppercase(),
        b'w' => c.is_ascii_alphanumeric(),
        b'x' => c.is_ascii_hexdigit(),
        b'z' => c == 0,
        _    => return cl == c,
    };
    if cl.is_ascii_lowercase() { res } else { !res }
}

/// Match character `c` against a bracket class `[p .. ec-1]`.
///
/// C: `static int matchbracketclass(int c, const char *p, const char *ec)`
/// `p` and `ec` are indices into `pat`.
fn matchbracketclass(pat: &[u8], c: u8, mut p: usize, ec: usize) -> bool {
    let sig = if p + 1 < pat.len() && pat[p + 1] == b'^' {
        p += 1; // skip '^'
        false
    } else {
        true
    };
    p += 1; // advance past '[' or '^'
    while p < ec {
        if pat[p] == L_ESC {
            p += 1;
            if p < ec && match_class(c, pat[p]) {
                return sig;
            }
        } else if p + 1 < ec && pat[p + 1] == b'-' && p + 2 < ec {
            let lo = pat[p];
            p += 2;
            let hi = pat[p];
            if lo <= c && c <= hi {
                return sig;
            }
        } else if pat[p] == c {
            return sig;
        }
        p += 1;
    }
    !sig
}

/// Return `true` if the single character at `src[s]` matches the pattern
/// element starting at `pat[p]` with class end at `ep`.
///
/// C: `static int singlematch(MatchState *ms, const char *s, const char *p, const char *ep)`
fn singlematch(ms: &MatchState, s: usize, p: usize, ep: usize) -> bool {
    if s >= ms.src.len() {
        return false;
    }
    let c = ms.src[s];
    match ms.pat[p] {
        b'.' => true,
        L_ESC => match_class(c, ms.pat[p + 1]),
        b'[' => matchbracketclass(ms.pat, c, p, ep - 1),
        pc   => pc == c,
    }
}

/// Find the end of the pattern element starting at `pat[p]`.
/// Returns the index one past the element, or an error for malformed patterns.
///
/// C: `static const char *classend(MatchState *ms, const char *p)`
fn classend(ms: &MatchState, p: usize) -> Result<usize, LuaError> {
    let pat = ms.pat;
    match pat.get(p).copied() {
        Some(L_ESC) => {
            // C: if (p == ms->p_end) luaL_error(...);
            if p + 1 >= pat.len() {
                return Err(LuaError::runtime(format_args!(
                    "malformed pattern (ends with '%')"
                )));
            }
            Ok(p + 2)
        }
        Some(b'[') => {
            let mut q = p + 1;
            if q < pat.len() && pat[q] == b'^' {
                q += 1;
            }
            loop {
                if q >= pat.len() {
                    return Err(LuaError::runtime(format_args!(
                        "malformed pattern (missing ']')"
                    )));
                }
                let ch = pat[q];
                q += 1;
                if ch == L_ESC && q < pat.len() {
                    q += 1; // skip escaped char
                } else if ch == b']' {
                    return Ok(q);
                }
            }
        }
        Some(_) => Ok(p + 1),
        None => Ok(p),
    }
}

/// Check that capture `l` (1-based char digit from pattern) is valid.
/// Returns the 0-based capture index.
///
/// C: `static int check_capture(MatchState *ms, int l)`
fn check_capture(ms: &MatchState, l: u8) -> Result<usize, LuaError> {
    let signed = (l as i32) - (b'1' as i32);
    if signed < 0
        || signed >= ms.level as i32
        || ms.captures[signed as usize].len == CAP_UNFINISHED
    {
        return Err(LuaError::runtime(format_args!(
            "invalid capture index %{}",
            signed + 1
        )));
    }
    Ok(signed as usize)
}

/// Find the most recent unfinished capture to close.
///
/// C: `static int capture_to_close(MatchState *ms)`
fn capture_to_close(ms: &MatchState) -> Result<usize, LuaError> {
    let mut level = ms.level as usize;
    while level > 0 {
        level -= 1;
        if ms.captures[level].len == CAP_UNFINISHED {
            return Ok(level);
        }
    }
    Err(LuaError::runtime(format_args!("invalid pattern capture")))
}

/// Match a balanced string `%bxy` starting at `src[s]`.
///
/// C: `static const char *matchbalance(MatchState *ms, const char *s, const char *p)`
/// Returns the new `s` position after the match, or `None`.
fn matchbalance(ms: &MatchState, s: usize, p: usize) -> Result<Option<usize>, LuaError> {
    // C: if (p >= ms->p_end - 1) luaL_error(...)
    if p + 1 >= ms.pat.len() {
        return Err(LuaError::runtime(format_args!(
            "malformed pattern (missing arguments to '%b')"
        )));
    }
    let b = ms.pat[p];
    let e = ms.pat[p + 1];
    if s >= ms.src.len() || ms.src[s] != b {
        return Ok(None);
    }
    let mut cont = 1i32;
    let mut s = s + 1;
    while s < ms.src.len() {
        if ms.src[s] == e {
            cont -= 1;
            if cont == 0 {
                return Ok(Some(s + 1));
            }
        } else if ms.src[s] == b {
            cont += 1;
        }
        s += 1;
    }
    Ok(None)
}

/// Greedy match: match as many as possible, then try the rest of the pattern.
///
/// C: `static const char *max_expand(MatchState *ms, const char *s, const char *p, const char *ep)`
fn max_expand(
    ms: &mut MatchState,
    s: usize,
    p: usize,
    ep: usize,
) -> Result<Option<usize>, LuaError> {
    let mut count: isize = 0;
    while singlematch(ms, s + count as usize, p, ep) {
        count += 1;
    }
    while count >= 0 {
        let res = match_pat(ms, s + count as usize, ep + 1)?;
        if res.is_some() {
            return Ok(res);
        }
        count -= 1;
    }
    Ok(None)
}

/// Lazy match: try the rest of the pattern first, then expand by one.
///
/// C: `static const char *min_expand(MatchState *ms, const char *s, const char *p, const char *ep)`
fn min_expand(
    ms: &mut MatchState,
    mut s: usize,
    p: usize,
    ep: usize,
) -> Result<Option<usize>, LuaError> {
    loop {
        let res = match_pat(ms, s, ep + 1)?;
        if res.is_some() {
            return Ok(res);
        } else if singlematch(ms, s, p, ep) {
            s += 1;
        } else {
            return Ok(None);
        }
    }
}

/// Open a new capture at `src[s]`.
///
/// C: `static const char *start_capture(MatchState *ms, const char *s, const char *p, int what)`
fn start_capture(
    ms: &mut MatchState,
    s: usize,
    p: usize,
    what: isize,
) -> Result<Option<usize>, LuaError> {
    let level = ms.level as usize;
    if level >= LUA_MAX_CAPTURES {
        return Err(LuaError::runtime(format_args!("too many captures")));
    }
    ms.captures[level].init = s;
    ms.captures[level].len = what;
    ms.level += 1;
    let res = match_pat(ms, s, p)?;
    if res.is_none() {
        ms.level -= 1; // undo capture
    }
    Ok(res)
}

/// Close the most recent open capture at `src[s]`.
///
/// C: `static const char *end_capture(MatchState *ms, const char *s, const char *p)`
fn end_capture(ms: &mut MatchState, s: usize, p: usize) -> Result<Option<usize>, LuaError> {
    let l = capture_to_close(ms)?;
    ms.captures[l].len = (s - ms.captures[l].init) as isize;
    let res = match_pat(ms, s, p)?;
    if res.is_none() {
        ms.captures[l].len = CAP_UNFINISHED; // undo
    }
    Ok(res)
}

/// Match a back-reference `%n` against `src[s]`.
///
/// C: `static const char *match_capture(MatchState *ms, const char *s, int l)`
fn match_capture(ms: &MatchState, s: usize, l: u8) -> Result<Option<usize>, LuaError> {
    let idx = check_capture(ms, l)?;
    let cap_len = ms.captures[idx].len as usize;
    let cap_init = ms.captures[idx].init;
    if ms.src.len() - s >= cap_len
        && &ms.src[s..s + cap_len] == &ms.src[cap_init..cap_init + cap_len]
    {
        Ok(Some(s + cap_len))
    } else {
        Ok(None)
    }
}

/// Core recursive pattern matcher.
/// Returns `Ok(Some(new_s))` on match, `Ok(None)` on failure, `Err` on error.
///
/// C: `static const char *match(MatchState *ms, const char *s, const char *p)`
/// The C code uses `goto init` for tail calls; here we use a loop.
fn match_pat(ms: &mut MatchState, mut s: usize, mut p: usize) -> Result<Option<usize>, LuaError> {
    // C: if (l_unlikely(ms->matchdepth-- == 0)) luaL_error(ms->L, "pattern too complex");
    ms.matchdepth -= 1;
    if ms.matchdepth < 0 {
        ms.matchdepth = 0;
        return Err(LuaError::runtime(format_args!("pattern too complex")));
    }

    // Use a loop to simulate `goto init` (tail-call optimization).
    let result = 'outer: loop {
        if p >= ms.pat.len() {
            // end of pattern — full match up to current s
            break 'outer Ok(Some(s));
        }

        match ms.pat[p] {
            b'(' => {
                // C: start capture
                let s2 = if p + 1 < ms.pat.len() && ms.pat[p + 1] == b')' {
                    // position capture
                    start_capture(ms, s, p + 2, CAP_POSITION)?
                } else {
                    start_capture(ms, s, p + 1, CAP_UNFINISHED)?
                };
                break 'outer Ok(s2);
            }
            b')' => {
                let s2 = end_capture(ms, s, p + 1)?;
                break 'outer Ok(s2);
            }
            b'$' => {
                // C: if ((p + 1) != ms->p_end) goto dflt;
                if p + 1 != ms.pat.len() {
                    // fall through to default
                    let ep = classend(ms, p)?;
                    let s2 = handle_class_with_suffix(ms, s, p, ep)?;
                    break 'outer Ok(s2);
                }
                // C: s = (s == ms->src_end) ? s : NULL;
                break 'outer Ok(if s == ms.src.len() { Some(s) } else { None });
            }
            L_ESC => {
                match ms.pat.get(p + 1).copied().unwrap_or(0) {
                    b'b' => {
                        // C: matchbalance
                        let s2 = matchbalance(ms, s, p + 2)?;
                        if let Some(ns) = s2 {
                            s = ns;
                            p += 4;
                            continue 'outer; // tail call: match(ms, s, p+4)
                        }
                        break 'outer Ok(None);
                    }
                    b'f' => {
                        // C: frontier pattern
                        p += 2;
                        if ms.pat.get(p).copied() != Some(b'[') {
                            return Err(LuaError::runtime(format_args!(
                                "missing '[' after '%f' in pattern"
                            )));
                        }
                        let ep = classend(ms, p)?;
                        let previous = if s == 0 { 0u8 } else { ms.src[s - 1] };
                        let current = ms.src.get(s).copied().unwrap_or(0);
                        if !matchbracketclass(ms.pat, previous, p, ep - 1)
                            && matchbracketclass(ms.pat, current, p, ep - 1)
                        {
                            p = ep;
                            continue 'outer; // tail call: match(ms, s, ep)
                        }
                        break 'outer Ok(None);
                    }
                    c @ b'0'..=b'9' => {
                        // C: back-reference %0-%9
                        let s2 = match_capture(ms, s, c)?;
                        if let Some(ns) = s2 {
                            s = ns;
                            p += 2;
                            continue 'outer; // tail call: match(ms, s, p+2)
                        }
                        break 'outer Ok(None);
                    }
                    _ => {
                        // fall through to default class handling
                        let ep = classend(ms, p)?;
                        let s2 = handle_class_with_suffix(ms, s, p, ep)?;
                        break 'outer Ok(s2);
                    }
                }
            }
            _ => {
                // default: pattern class plus optional suffix
                let ep = classend(ms, p)?;
                let s2 = handle_class_with_suffix(ms, s, p, ep)?;
                break 'outer Ok(s2);
            }
        }
    };

    ms.matchdepth += 1;
    result
}

/// Handle a pattern class element with an optional repetition suffix (`*`, `+`, `?`, `-`).
///
/// PORT NOTE: Factored out from `match_pat`'s `default/dflt` label to share
/// code between the ESC-default and plain-default paths.
fn handle_class_with_suffix(
    ms: &mut MatchState,
    s: usize,
    p: usize,
    ep: usize,
) -> Result<Option<usize>, LuaError> {
    let matched_once = singlematch(ms, s, p, ep);
    if !matched_once {
        // C: if (*ep == '*' || *ep == '?' || *ep == '-') { p = ep+1; goto init; }
        //    else s = NULL;
        match ms.pat.get(ep).copied() {
            Some(b'*') | Some(b'?') | Some(b'-') => {
                // Accept zero occurrences: tail-call match(ms, s, ep+1)
                // We can't do a tail call into match_pat because we're returning
                // from handle_class_with_suffix, but we can call it directly.
                return match_pat(ms, s, ep + 1);
            }
            _ => return Ok(None),
        }
    }

    // Matched at least once
    match ms.pat.get(ep).copied() {
        Some(b'?') => {
            // Optional: try matching with s+1, fall back to ep+1
            let res = match_pat(ms, s + 1, ep + 1)?;
            if res.is_some() {
                Ok(res)
            } else {
                match_pat(ms, s, ep + 1)
            }
        }
        Some(b'+') => {
            // 1 or more: greedy from s+1
            max_expand(ms, s + 1, p, ep)
        }
        Some(b'*') => {
            // 0 or more: greedy from s
            max_expand(ms, s, p, ep)
        }
        Some(b'-') => {
            // 0 or more: lazy from s
            min_expand(ms, s, p, ep)
        }
        _ => {
            // No suffix: match one, advance both s and p
            match_pat(ms, s + 1, ep)
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// §5  Pattern-matching public API helpers
// ────────────────────────────────────────────────────────────────────────────

/// Find `needle` in `haystack` using a plain memmem-style search.
///
/// C: `static const char *lmemfind(const char *s1, size_t l1, const char *s2, size_t l2)`
/// Returns the byte-offset of the first occurrence, or `None`.
fn lmemfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if needle.len() > haystack.len() {
        return None;
    }
    let first = needle[0];
    let rest = &needle[1..];
    let limit = haystack.len() - rest.len();
    let mut s = 0;
    while s <= limit {
        if let Some(pos) = haystack[s..].iter().position(|&b| b == first) {
            let pos = s + pos;
            if pos + 1 + rest.len() <= haystack.len()
                && &haystack[pos + 1..pos + 1 + rest.len()] == rest
            {
                return Some(pos);
            }
            s = pos + 1;
        } else {
            break;
        }
    }
    None
}

/// Check whether the pattern `pat` has no special characters (for plain search).
///
/// C: `static int nospecials(const char *p, size_t l)`
fn nospecials(pat: &[u8]) -> bool {
    !pat.iter().any(|b| SPECIALS.contains(b))
}

/// Information about one capture result.
enum CaptureInfo<'a> {
    /// A position capture; value is 1-based index.
    Position(i64),
    /// A string capture (slice of source).
    Bytes(&'a [u8]),
}

/// Get information about the `i`-th capture.
/// If there are no captures and `i == 0`, returns the whole match `s..e`.
///
/// C: `static size_t get_onecapture(MatchState *ms, int i, const char *s, const char *e, const char **cap)`
fn get_one_capture<'a>(
    ms: &'a MatchState,
    i: usize,
    s: usize,
    e: usize,
) -> Result<CaptureInfo<'a>, LuaError> {
    if i >= ms.level as usize {
        // C: if (i != 0) luaL_error(...)
        if i != 0 {
            return Err(LuaError::runtime(format_args!(
                "invalid capture index %{}",
                i + 1
            )));
        }
        // Return whole match
        return Ok(CaptureInfo::Bytes(&ms.src[s..e]));
    }
    let cap = &ms.captures[i];
    if cap.len == CAP_UNFINISHED {
        return Err(LuaError::runtime(format_args!("unfinished capture")));
    }
    if cap.len == CAP_POSITION {
        // C: lua_pushinteger(ms->L, (ms->capture[i].init - ms->src_init) + 1);
        return Ok(CaptureInfo::Position((cap.init + 1) as i64));
    }
    let len = cap.len as usize;
    Ok(CaptureInfo::Bytes(&ms.src[cap.init..cap.init + len]))
}

/// Push all captures (or whole match if none) onto the stack.
/// Returns the number of values pushed.
///
/// C: `static int push_captures(MatchState *ms, const char *s, const char *e)`
fn push_captures(
    state: &mut LuaState,
    ms: &MatchState,
    s: usize,
    e: usize,
) -> Result<usize, LuaError> {
    let nlevels = if ms.level == 0 { 1 } else { ms.level as usize };
    state.ensure_stack(nlevels as i32, "too many captures")?;
    for i in 0..nlevels {
        match get_one_capture(ms, i, s, e)? {
            CaptureInfo::Position(n) => state.push(LuaValue::Int(n)),
            CaptureInfo::Bytes(b) => state.push_bytes(b)?,
        }
    }
    Ok(nlevels)
}

// ────────────────────────────────────────────────────────────────────────────
// §6  str_find / str_match / gmatch / gsub
// ────────────────────────────────────────────────────────────────────────────

/// Shared implementation of `string.find` and `string.match`.
///
/// C: `static int str_find_aux(lua_State *L, int find)`
fn str_find_aux(state: &mut LuaState, find: bool) -> Result<usize, LuaError> {
    let s_bytes = state.check_arg_string(1)?;
    let p_bytes = state.check_arg_string(2)?;
    let ls = s_bytes.len();
    let lp = p_bytes.len();
    let init_raw = state.opt_arg_integer(3, 1)?;
    let init = pos_relat_i(init_raw, ls).saturating_sub(1);

    if init > ls {
        // C: luaL_pushfail(L); return 1;
        state.push(LuaValue::Nil);
        return Ok(1);
    }

    // Clone to avoid borrow-across-push issues
    let s_owned: Vec<u8> = s_bytes.to_vec();
    let p_owned: Vec<u8> = p_bytes.to_vec();
    let s = &s_owned[..];
    let p = &p_owned[..];

    // C: if (find && (lua_toboolean(L, 4) || nospecials(p, lp)))
    if find && (state.arg_to_bool(4) || nospecials(p)) {
        // plain search
        if let Some(pos) = lmemfind(&s[init..], p) {
            let abs = init + pos;
            state.push(LuaValue::Int((abs + 1) as i64));
            state.push(LuaValue::Int((abs + lp) as i64));
            return Ok(2);
        }
    } else {
        let mut ms = MatchState::new(s, p);
        let anchor = p.first() == Some(&b'^');
        let (p_start, p_slice) = if anchor {
            (0, &p[1..])
        } else {
            (0, p)
        };
        ms.pat = p_slice;

        let mut s1 = init;
        loop {
            ms.reset_level();
            if let Some(res) = match_pat(&mut ms, s1, 0)? {
                if find {
                    state.push(LuaValue::Int((s1 + 1) as i64));
                    state.push(LuaValue::Int(res as i64));
                    let nc = push_captures(state, &ms, 0, 0)?;
                    return Ok(nc + 2);
                } else {
                    return push_captures(state, &ms, s1, res);
                }
            }
            if s1 >= ms.src.len() || anchor {
                break;
            }
            s1 += 1;
        }
    }

    // C: luaL_pushfail(L); return 1;
    state.push(LuaValue::Nil);
    Ok(1)
}

/// `string.find(s, pattern [, init [, plain]])` — find pattern in `s`.
///
/// C: `static int str_find(lua_State *L)`
pub fn str_find(state: &mut LuaState) -> Result<usize, LuaError> {
    str_find_aux(state, true)
}

/// `string.match(s, pattern [, init])` — match pattern against `s`.
///
/// C: `static int str_match(lua_State *L)`
pub fn str_match(state: &mut LuaState) -> Result<usize, LuaError> {
    str_find_aux(state, false)
}

/// Continuation function for `string.gmatch` iterator closure.
///
/// C: `static int gmatch_aux(lua_State *L)`
///
/// PORT NOTE: The C version stores `GMatchState` inside a heap-allocated
/// userdata referenced by upvalue 3, then mutates fields via the raw pointer
/// each iteration. Our Phase-A `LuaCClosure.upvalues` is immutable, so the
/// iterator state lives in a Lua table referenced by upvalue 1 with
/// integer-keyed slots:
///   t[1] = source bytes (string), t[2] = pattern bytes (string),
///   t[3] = current source position (1-based; equals `lastmatch` after a
///   successful match), t[4] = end of last match (`0` ≡ NULL in C, meaning
///   "no match yet").
pub fn gmatch_aux(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: GMatchState *gm = (GMatchState *)lua_touserdata(L, lua_upvalueindex(3));
    // Pull the state table from upvalue 1 onto the top of the stack.
    state.push_value(upvalue_index(1))?;
    let tbl_idx = state.top();

    // Read t[1] = src, t[2] = pat, t[3] = pos, t[4] = lastmatch.
    state.raw_geti(tbl_idx, 1)?;
    let s: Vec<u8> = state.to_lua_string_bytes(-1).unwrap_or_default();
    state.pop_n(1);
    state.raw_geti(tbl_idx, 2)?;
    let p: Vec<u8> = state.to_lua_string_bytes(-1).unwrap_or_default();
    state.pop_n(1);
    state.raw_geti(tbl_idx, 3)?;
    let pos = state.to_integer_x(-1).unwrap_or(1);
    state.pop_n(1);
    state.raw_geti(tbl_idx, 4)?;
    let lastmatch_raw = state.to_integer_x(-1).unwrap_or(0);
    state.pop_n(1);
    let last_match: Option<usize> = if lastmatch_raw <= 0 {
        None
    } else {
        Some((lastmatch_raw - 1) as usize)
    };

    let ls = s.len();
    let start_pos = if pos < 1 { 0usize } else { (pos - 1) as usize };

    let mut ms = MatchState::new(&s, &p);

    // C: for (src = gm->src; src <= gm->ms.src_end; src++)
    let mut src = start_pos;
    while src <= ls {
        // C: reprepstate(&gm->ms);
        ms.reset_level();
        // C: if ((e = match(&gm->ms, src, gm->p)) != NULL && e != gm->lastmatch)
        if let Some(e) = match_pat(&mut ms, src, 0)? {
            if Some(e) != last_match {
                // C: gm->src = gm->lastmatch = e;
                // Write back into the state table. The table is still on top of
                // the stack at tbl_idx.
                state.push(LuaValue::Int((e + 1) as i64));
                state.raw_seti(tbl_idx, 3)?;
                state.push(LuaValue::Int((e + 1) as i64));
                state.raw_seti(tbl_idx, 4)?;
                // Pop the state table before pushing captures.
                state.pop_n(1);
                // C: return push_captures(&gm->ms, src, e);
                return push_captures(state, &ms, src, e);
            }
        }
        src += 1;
    }

    // C: return 0;  /* not found */
    state.pop_n(1);
    Ok(0)
}

/// `string.gmatch(s, pattern [, init])` — return an iterator for all matches.
///
/// C: `static int gmatch(lua_State *L)`
///
/// PORT NOTE: C uses `lua_newuserdatauv` for the GMatchState plus a 3-upvalue
/// C closure. Phase-A LuaCClosure upvalues are immutable, so we collapse the
/// state into a 4-element Lua table held in a single upvalue (see
/// `gmatch_aux`).
pub fn gmatch(state: &mut LuaState) -> Result<usize, LuaError> {
    let s: Vec<u8> = state.check_arg_string(1)?.to_vec();
    let p: Vec<u8> = state.check_arg_string(2)?.to_vec();
    let ls = s.len();
    // C: size_t init = posrelatI(luaL_optinteger(L, 3, 1), ls) - 1;
    let init_raw = state.opt_arg_integer(3, 1)?;
    let mut init = pos_relat_i(init_raw, ls).saturating_sub(1);
    // C: if (init > ls) init = ls + 1;
    if init > ls {
        init = ls + 1;
    }

    // C: lua_settop(L, 2);
    state.set_top(2);

    // Build the iterator-state table: t = {src, pat, pos, lastmatch}.
    state.create_table(4, 0)?;
    let tbl_idx = state.top();
    state.push_bytes(&s)?;
    state.raw_seti(tbl_idx, 1)?;
    state.push_bytes(&p)?;
    state.raw_seti(tbl_idx, 2)?;
    state.push(LuaValue::Int((init + 1) as i64));
    state.raw_seti(tbl_idx, 3)?;
    state.push(LuaValue::Int(0));
    state.raw_seti(tbl_idx, 4)?;

    // C: lua_pushcclosure(L, gmatch_aux, 3);
    // Our version uses 1 upvalue (the state table) instead of 3.
    state.push_c_closure(gmatch_aux, 1)?;
    Ok(1)
}

/// Add a replacement string with `%n` capture references to `buf`.
///
/// C: `static void add_s(MatchState *ms, luaL_Buffer *b, const char *s, const char *e)`
fn add_s(
    state: &mut LuaState,
    ms: &MatchState,
    buf: &mut Vec<u8>,
    s: usize,
    e: usize,
) -> Result<(), LuaError> {
    // C: const char *news = lua_tolstring(L, 3, &l);
    let news_bytes = state.to_lua_string_bytes(3)
        .map(|b| b.to_vec())
        .unwrap_or_default();
    let mut i = 0usize;
    while i < news_bytes.len() {
        if news_bytes[i] != L_ESC {
            buf.push(news_bytes[i]);
            i += 1;
        } else {
            i += 1; // skip ESC
            if i >= news_bytes.len() {
                break;
            }
            let c = news_bytes[i];
            if c == L_ESC {
                buf.push(L_ESC);
            } else if c == b'0' {
                buf.extend_from_slice(&ms.src[s..e]);
            } else if c.is_ascii_digit() {
                match get_one_capture(ms, (c - b'1') as usize, s, e)? {
                    CaptureInfo::Position(n) => {
                        // push position then pop into buf
                        // C: luaL_addvalue(b);  -- adds the top of stack
                        let formatted = format!("{}", n).into_bytes();
                        buf.extend_from_slice(&formatted);
                    }
                    CaptureInfo::Bytes(b) => {
                        buf.extend_from_slice(b);
                    }
                }
            } else {
                return Err(LuaError::runtime(format_args!(
                    "invalid use of '{}' in replacement string",
                    L_ESC as char
                )));
            }
            i += 1;
        }
    }
    Ok(())
}

/// Add the replacement value (string, table lookup, or function call) to `buf`.
/// Returns `true` if the original text was changed.
///
/// C: `static int add_value(MatchState *ms, luaL_Buffer *b, const char *s, const char *e, int tr)`
fn add_value(
    state: &mut LuaState,
    ms: &MatchState,
    buf: &mut Vec<u8>,
    s: usize,
    e: usize,
    tr: LuaType,
) -> Result<bool, LuaError> {
    match tr {
        LuaType::Function => {
            // C: lua_pushvalue(L, 3); n = push_captures(...); lua_call(L, n, 1);
            state.push_value_at(3)?;
            let n = push_captures(state, ms, s, e)?;
            state.call(n as i32, 1)?;
        }
        LuaType::Table => {
            // C: push_onecapture(ms, 0, s, e); lua_gettable(L, 3);
            match get_one_capture(ms, 0, s, e)? {
                CaptureInfo::Position(n) => state.push(LuaValue::Int(n)),
                CaptureInfo::Bytes(b) => state.push_bytes(b)?,
            }
            state.get_table(3)?;
        }
        _ => {
            // LUA_TNUMBER or LUA_TSTRING: add replacement string directly
            add_s(state, ms, buf, s, e)?;
            return Ok(true);
        }
    }

    // C: if (!lua_toboolean(L, -1)) { lua_pop(L, 1); addlstring(b, s, e-s); return 0; }
    let top_bool = state.arg_to_bool(-1);
    if !top_bool {
        state.pop_n(1);
        buf.extend_from_slice(&ms.src[s..e]);
        return Ok(false);
    }
    if state.type_at(-1) != LuaType::String {
        let tname = state.type_name_at(-1).to_owned();
        return Err(LuaError::runtime(format_args!(
            "invalid replacement value (a {})", tname.escape_ascii()
        )));
    }
    // C: luaL_addvalue(b);
    let v = state.to_bytes(-1).unwrap_or_default();
    state.pop();
    buf.extend_from_slice(&v);
    Ok(true)
}

/// `string.gsub(s, pattern, repl [, n])` — global substitution.
///
/// C: `static int str_gsub(lua_State *L)`
pub fn str_gsub(state: &mut LuaState) -> Result<usize, LuaError> {
    let src_bytes = state.check_arg_string(1)?;
    let pat_bytes = state.check_arg_string(2)?;
    let src_len = src_bytes.len();
    let max_s = state.opt_arg_integer(4, (src_len + 1) as i64)?;
    let tr = state.type_at(3);

    // C: luaL_argexpected(..., tr == TNUMBER || tr == TSTRING || tr == TFUNCTION || tr == TTABLE, ...)
    if !matches!(tr, LuaType::Number | LuaType::String | LuaType::Function | LuaType::Table) {
        let v = state.arg(3);
        return Err(LuaError::type_arg_error(3, "string/function/table", &v));
    }

    let src_owned = src_bytes.to_vec();
    let pat_owned = pat_bytes.to_vec();

    let anchor = pat_owned.first() == Some(&b'^');
    let pat_slice = if anchor { &pat_owned[1..] } else { &pat_owned[..] };

    let mut ms = MatchState::new(&src_owned, pat_slice);
    let mut buf: Vec<u8> = Vec::new();
    let mut src_pos = 0usize;
    let mut last_match: Option<usize> = None;
    let mut n: i64 = 0;
    let mut changed = false;

    while n < max_s {
        ms.reset_level();
        let maybe_e = match_pat(&mut ms, src_pos, 0)?;
        if let Some(e) = maybe_e {
            if last_match != Some(e) {
                n += 1;
                let delta = add_value(state, &ms, &mut buf, src_pos, e, tr)?;
                changed |= delta;
                src_pos = e;
                last_match = Some(e);
            } else if src_pos < ms.src.len() {
                buf.push(ms.src[src_pos]);
                src_pos += 1;
            } else {
                break;
            }
        } else if src_pos < ms.src.len() {
            buf.push(ms.src[src_pos]);
            src_pos += 1;
        } else {
            break;
        }
        if anchor {
            break;
        }
    }

    if !changed {
        // C: lua_pushvalue(L, 1); /* return original string */
        state.push_value_at(1)?;
    } else {
        buf.extend_from_slice(&ms.src[src_pos..]);
        state.push_bytes(&buf)?;
    }
    state.push(LuaValue::Int(n));
    Ok(2)
}

// ────────────────────────────────────────────────────────────────────────────
// §7  String format (`string.format`)
// ────────────────────────────────────────────────────────────────────────────

/// Add a hex-float digit to buffer and return the fractional remainder.
///
/// C: `static lua_Number adddigit(char *buff, int n, lua_Number x)`
fn adddigit(buf: &mut Vec<u8>, x: f64) -> f64 {
    let dd = x.floor();
    let d = dd as i32;
    let c = if d < 10 { b'0' + d as u8 } else { b'a' + (d - 10) as u8 };
    buf.push(c);
    x - dd
}

/// Convert a float to a hex-float string (like `%a`).
///
/// C: `static int num2straux(char *buff, int sz, lua_Number x)`
fn num2straux(x: f64) -> Vec<u8> {
    if x.is_nan() || x.is_infinite() {
        // C: return l_sprintf(buff, sz, LUA_NUMBER_FMT, x);
        // Use %g-like representation
        return format!("{}", x).into_bytes();
    }
    if x == 0.0 {
        // C: return l_sprintf(buff, sz, LUA_NUMBER_FMT "x0p+0", x);
        if x.is_sign_negative() {
            return b"-0x0p+0".to_vec();
        }
        return b"0x0p+0".to_vec();
    }

    let (m_raw, exp) = frexp(x);
    let mut buf: Vec<u8> = Vec::new();
    let mut m = m_raw;
    if m < 0.0 {
        buf.push(b'-');
        m = -m;
    }
    buf.extend_from_slice(b"0x");

    // C: L_NBFD = (MANT_DIG - 1) % 4 + 1  where MANT_DIG=53 → (52%4)+1 = 1
    let nbfd = 1;
    m = adddigit(&mut buf, m * (1 << nbfd) as f64);
    let e = exp - nbfd;

    if m > 0.0 {
        buf.push(b'.');
        while m > 0.0 {
            m = adddigit(&mut buf, m * 16.0);
        }
    }

    let exp_str = format!("p{:+}", e);
    buf.extend_from_slice(exp_str.as_bytes());
    buf
}

/// Decompose `x` into mantissa in `[0.5, 1.0)` and exponent.
/// Equivalent to C's `frexp`.
fn frexp(x: f64) -> (f64, i32) {
    if x == 0.0 || x.is_nan() || x.is_infinite() {
        return (x, 0);
    }
    let bits = x.to_bits();
    let exp_bits = ((bits >> 52) & 0x7FF) as i32;
    if exp_bits == 0 {
        // subnormal
        let (m, e) = frexp(x * (1u64 << 52) as f64);
        return (m, e - 52);
    }
    let exp = exp_bits - 1022;
    let mantissa_bits = (bits & 0x000F_FFFF_FFFF_FFFF) | 0x3FE0_0000_0000_0000;
    (f64::from_bits(mantissa_bits), exp)
}

/// Convert float `n` to a Lua-readable literal (hex or special representation).
///
/// C: `static int quotefloat(lua_State *L, char *buff, lua_Number n)`
fn quotefloat(n: f64) -> Vec<u8> {
    if n == f64::INFINITY {
        return b"1e9999".to_vec();
    } else if n == f64::NEG_INFINITY {
        return b"-1e9999".to_vec();
    } else if n.is_nan() {
        return b"(0/0)".to_vec();
    }
    // hex float, ensuring dot separator
    let mut buf = num2straux(n);
    if !buf.contains(&b'.') && !buf.contains(&b'p') {
        // try to find locale decimal point and replace with '.'
        // PORT NOTE: We always produce '.' so this branch is not taken.
    }
    buf
}

/// Add a quoted Lua string literal to `buf`.
///
/// C: `static void addquoted(luaL_Buffer *b, const char *s, size_t len)`
fn addquoted(buf: &mut Vec<u8>, s: &[u8]) {
    buf.push(b'"');
    for &c in s {
        if c == b'"' || c == b'\\' || c == b'\n' {
            buf.push(b'\\');
            buf.push(c);
        } else if c.is_ascii_control() {
            let next = 0u8; // TODO(port): we don't know the next char at this point
            // C: if (!isdigit(uchar(*(s+1)))) l_sprintf(buff, ..., "\\%d", ...)
            // else l_sprintf(buff, ..., "\\%03d", ...)
            // For safety, always use 3-digit form.
            let s = format!("\\{:03}", c);
            buf.extend_from_slice(s.as_bytes());
        } else {
            buf.push(c);
        }
    }
    buf.push(b'"');
}

/// Add a Lua literal representation of arg `n` to `buf`.
///
/// C: `static void addliteral(lua_State *L, luaL_Buffer *b, int arg)`
fn addliteral(state: &mut LuaState, buf: &mut Vec<u8>, arg: i32) -> Result<(), LuaError> {
    match state.type_at(arg) {
        LuaType::String => {
            let s = state.check_arg_string(arg)?.to_vec();
            addquoted(buf, &s);
        }
        LuaType::Number => {
            if matches!(state.get_at(arg), LuaValue::Int(_)) {
                let n = state.to_integer(arg).unwrap_or(0);
                let formatted = if n == i64::MIN {
                    format!("0x{:016x}", n as u64)
                } else {
                    format!("{}", n)
                };
                buf.extend_from_slice(formatted.as_bytes());
            } else {
                let n = state.to_number(arg).unwrap_or(0.0);
                let hex = quotefloat(n);
                buf.extend_from_slice(&hex);
            }
        }
        LuaType::Nil | LuaType::Boolean => {
            // C: luaL_tolstring(L, arg, NULL); luaL_addvalue(b);
            let s = state.to_string_coerced(arg).unwrap_or_default();
            buf.extend_from_slice(&s);
        }
        _ => {
            return Err(LuaError::arg_error(arg, "value has no literal form"));
        }
    }
    Ok(())
}

/// `string.format(fmt, ...)` — C-style string formatting.
///
/// C: `static int str_format(lua_State *L)`
///
/// TODO(port): This function uses dynamic sprintf with format strings built
/// at runtime. The C implementation delegates to snprintf with platform
/// format specifiers. The Rust port translates each format case manually.
/// The `%a`/`%A` hex float case uses our `num2straux`; other cases use
/// Rust formatting. Edge cases involving locale-dependent behavior (e.g.
/// `lua_getlocaledecpoint`) are approximated.
pub fn str_format(state: &mut LuaState) -> Result<usize, LuaError> {
    let top = state.get_top();
    let mut arg = 1i32;
    let fmt_bytes = state.check_arg_string(1)?.to_vec();
    let mut buf: Vec<u8> = Vec::new();
    let mut i = 0usize;

    while i < fmt_bytes.len() {
        let c = fmt_bytes[i];
        if c != L_ESC {
            buf.push(c);
            i += 1;
            continue;
        }
        i += 1;
        if i >= fmt_bytes.len() {
            break;
        }
        if fmt_bytes[i] == L_ESC {
            buf.push(L_ESC);
            i += 1;
            continue;
        }

        // Parse a format specifier
        arg += 1;
        if arg > top {
            return Err(LuaError::arg_error(arg, "no value"));
        }

        // Collect flags, width, precision
        let spec_start = i - 1; // includes the initial '%'
        // Skip flags: -, +, #, 0, space
        while i < fmt_bytes.len() && b"-+#0 ".contains(&fmt_bytes[i]) {
            i += 1;
        }
        // Skip width digits
        if i < fmt_bytes.len() && fmt_bytes[i] != b'0' {
            while i < fmt_bytes.len() && fmt_bytes[i].is_ascii_digit() {
                i += 1;
            }
        }
        // Skip precision
        if i < fmt_bytes.len() && fmt_bytes[i] == b'.' {
            i += 1;
            while i < fmt_bytes.len() && fmt_bytes[i].is_ascii_digit() {
                i += 1;
            }
        }

        if i >= fmt_bytes.len() {
            return Err(LuaError::runtime(format_args!("invalid conversion specification")));
        }

        let conv = fmt_bytes[i];
        i += 1;

        match conv {
            b'c' => {
                let n = state.check_arg_integer(arg)?;
                buf.push(n as u8);
            }
            b'd' | b'i' => {
                let n = state.check_arg_integer(arg)?;
                // TODO(port): width/precision/flags from spec not applied
                // PERF(port): str_format %d/%i ignores width/precision/flags; use sprintf crate in Phase B
                let s = format!("{}", n);
                buf.extend_from_slice(s.as_bytes());
            }
            b'u' => {
                let n = state.check_arg_integer(arg)? as u64;
                let s = format!("{}", n);
                buf.extend_from_slice(s.as_bytes());
            }
            b'o' => {
                let n = state.check_arg_integer(arg)? as u64;
                let s = format!("{:o}", n);
                buf.extend_from_slice(s.as_bytes());
            }
            b'x' => {
                let n = state.check_arg_integer(arg)? as u64;
                let s = format!("{:x}", n);
                buf.extend_from_slice(s.as_bytes());
            }
            b'X' => {
                let n = state.check_arg_integer(arg)? as u64;
                let s = format!("{:X}", n);
                buf.extend_from_slice(s.as_bytes());
            }
            b'a' => {
                let n = state.check_arg_number(arg)?;
                let hex = num2straux(n);
                buf.extend_from_slice(&hex);
            }
            b'A' => {
                let n = state.check_arg_number(arg)?;
                let hex: Vec<u8> = num2straux(n).into_iter().map(|b| b.to_ascii_uppercase()).collect();
                buf.extend_from_slice(&hex);
            }
            b'f' => {
                let n = state.check_arg_number(arg)?;
                // TODO(port): width/precision not applied
                let s = format!("{}", n);
                buf.extend_from_slice(s.as_bytes());
            }
            b'e' => {
                let n = state.check_arg_number(arg)?;
                let s = format!("{:e}", n);
                buf.extend_from_slice(s.as_bytes());
            }
            b'E' => {
                let n = state.check_arg_number(arg)?;
                let s = format!("{:E}", n);
                buf.extend_from_slice(s.as_bytes());
            }
            b'g' | b'G' => {
                let n = state.check_arg_number(arg)?;
                // TODO(port): %g format — use shortest decimal representation
                // Rust doesn't have a direct %g equivalent; approximate with {:?}
                let s = format!("{}", n);
                buf.extend_from_slice(s.as_bytes());
            }
            b'p' => {
                // C: pointer as hex; not meaningful in safe Rust
                // TODO(port): %p format not directly portable; push placeholder
                buf.extend_from_slice(b"(ptr)");
                arg -= 1; // no argument consumed for %p? Actually yes: lua_topointer
                // PORT NOTE: %p takes an argument in Lua's string.format (the value);
                // we just push a placeholder.
                arg += 1;
            }
            b'q' => {
                addliteral(state, &mut buf, arg)?;
            }
            b's' => {
                // C: luaL_tolstring(L, arg, &l);
                let s = state.to_string_coerced(arg).unwrap_or_default();
                // TODO(port): width/precision/flags not applied for %s
                buf.extend_from_slice(&s);
                state.pop_n(1); // luaL_tolstring pushes a value
            }
            _ => {
                return Err(LuaError::runtime(format_args!(
                    "invalid conversion '%{}' to 'format'", conv as char
                )));
            }
        }
    }

    state.push_bytes(&buf)?;
    Ok(1)
}

// ────────────────────────────────────────────────────────────────────────────
// §8  Pack / unpack
// ────────────────────────────────────────────────────────────────────────────

/// Return `true` if `c` is an ASCII digit.
fn is_digit(c: u8) -> bool {
    c.is_ascii_digit()
}

/// Read an optional integer from the format string, returning `df` if absent.
///
/// C: `static int getnum(const char **fmt, int df)`
fn getnum(fmt: &[u8], pos: &mut usize, df: i32) -> i32 {
    if *pos >= fmt.len() || !is_digit(fmt[*pos]) {
        return df;
    }
    let mut a = 0i32;
    while *pos < fmt.len() && is_digit(fmt[*pos]) {
        a = a * 10 + (fmt[*pos] - b'0') as i32;
        *pos += 1;
        if a > (i32::MAX - 9) / 10 {
            break;
        }
    }
    a
}

/// Read an integer from the format string, error if out of `[1, MAXINTSIZE]`.
///
/// C: `static int getnumlimit(Header *h, const char **fmt, int df)`
fn getnumlimit(fmt: &[u8], pos: &mut usize, df: i32) -> Result<usize, LuaError> {
    let sz = getnum(fmt, pos, df);
    if sz > MAX_INT_SIZE as i32 || sz <= 0 {
        return Err(LuaError::runtime(format_args!(
            "integral size ({}) out of limits [1,{}]",
            sz, MAX_INT_SIZE
        )));
    }
    Ok(sz as usize)
}

/// Read and classify the next pack format option, filling `size`.
///
/// C: `static KOption getoption(Header *h, const char **fmt, int *size)`
fn getoption(h: &mut Header, fmt: &[u8], pos: &mut usize, size: &mut usize) -> Result<KOption, LuaError> {
    // C: struct cD { char c; union { LUAI_MAXALIGN; } u; };
    // In Rust, the native max-align of a union of f64/void*/size_t is 8 on 64-bit.
    const NATIVE_MAX_ALIGN: usize = std::mem::align_of::<f64>();

    if *pos >= fmt.len() {
        return Ok(KOption::Nop);
    }
    let opt = fmt[*pos];
    *pos += 1;
    *size = 0;

    match opt {
        b'b' => { *size = 1; Ok(KOption::Int) }
        b'B' => { *size = 1; Ok(KOption::Uint) }
        b'h' => { *size = 2; Ok(KOption::Int) }
        b'H' => { *size = 2; Ok(KOption::Uint) }
        b'l' => { *size = 8; Ok(KOption::Int) }  // sizeof(long) on 64-bit
        b'L' => { *size = 8; Ok(KOption::Uint) }
        b'j' => { *size = SZINT; Ok(KOption::Int) }
        b'J' => { *size = SZINT; Ok(KOption::Uint) }
        b'T' => { *size = std::mem::size_of::<usize>(); Ok(KOption::Uint) }
        b'f' => { *size = 4; Ok(KOption::Float) }
        b'n' => { *size = 8; Ok(KOption::Number) }  // sizeof(lua_Number) = sizeof(f64) = 8
        b'd' => { *size = 8; Ok(KOption::Double) }  // sizeof(double) = 8
        b'i' => { *size = getnumlimit(fmt, pos, 4)?; Ok(KOption::Int) }
        b'I' => { *size = getnumlimit(fmt, pos, 4)?; Ok(KOption::Uint) }
        b's' => { *size = getnumlimit(fmt, pos, std::mem::size_of::<usize>()  as i32)?; Ok(KOption::Kstring) }
        b'c' => {
            let n = getnum(fmt, pos, -1);
            if n == -1 {
                return Err(LuaError::runtime(format_args!("missing size for format option 'c'")));
            }
            *size = n as usize;
            Ok(KOption::Char)
        }
        b'z' => Ok(KOption::Zstr),
        b'x' => { *size = 1; Ok(KOption::Padding) }
        b'X' => Ok(KOption::Paddalign),
        b' ' => Ok(KOption::Nop),
        b'<' => { h.is_little = true; Ok(KOption::Nop) }
        b'>' => { h.is_little = false; Ok(KOption::Nop) }
        b'=' => { h.is_little = cfg!(target_endian = "little"); Ok(KOption::Nop) }
        b'!' => {
            let n = getnum(fmt, pos, NATIVE_MAX_ALIGN as i32);
            h.max_align = getnumlimit(fmt, pos, n)?;
            Ok(KOption::Nop)
        }
        _ => Err(LuaError::runtime(format_args!("invalid format option '{}'", opt as char)))
    }
}

/// Get full details about the next format option, including alignment padding.
///
/// C: `static KOption getdetails(Header *h, size_t totalsize, const char **fmt, int *psize, int *ntoalign)`
fn getdetails(
    h: &mut Header,
    total_size: usize,
    fmt: &[u8],
    pos: &mut usize,
    psize: &mut usize,
    ntoalign: &mut usize,
) -> Result<KOption, LuaError> {
    let opt = getoption(h, fmt, pos, psize)?;
    let mut align = *psize;

    if opt == KOption::Paddalign {
        // C: if (**fmt == '\0' || getoption(h, fmt, &align) == Kchar || align == 0) argerror
        if *pos >= fmt.len() {
            return Err(LuaError::arg_error(1, "invalid next option for option 'X'"));
        }
        let mut dummy_size = 0usize;
        let next_opt = getoption(h, fmt, pos, &mut dummy_size)?;
        align = dummy_size;
        if next_opt == KOption::Char || align == 0 {
            return Err(LuaError::arg_error(1, "invalid next option for option 'X'"));
        }
    }

    if align <= 1 || opt == KOption::Char {
        *ntoalign = 0;
    } else {
        if align > h.max_align {
            align = h.max_align;
        }
        if (align & (align - 1)) != 0 {
            return Err(LuaError::arg_error(1, "format asks for alignment not power of 2"));
        }
        *ntoalign = (align - (total_size & (align - 1))) & (align - 1);
    }
    Ok(opt)
}

/// Pack integer `n` with `size` bytes into `buf` with given endianness.
///
/// C: `static void packint(luaL_Buffer *b, lua_Unsigned n, int islittle, int size, int neg)`
fn packint(buf: &mut Vec<u8>, mut n: u64, is_little: bool, size: usize, neg: bool) {
    let start = buf.len();
    buf.resize(start + size, 0);
    let slice = &mut buf[start..start + size];
    // Write LSB first (little-endian), then swap if big-endian
    for i in 0..size {
        slice[if is_little { i } else { size - 1 - i }] = (n & MC as u64) as u8;
        n >>= NB;
    }
    // Sign extension for negative numbers larger than lua_Integer
    if neg && size > SZINT {
        for i in SZINT..size {
            slice[if is_little { i } else { size - 1 - i }] = MC;
        }
    }
}

/// Copy bytes with endianness correction.
///
/// C: `static void copywithendian(char *dest, const char *src, int size, int islittle)`
fn copywithendian(dest: &mut [u8], src: &[u8], is_little: bool) {
    debug_assert_eq!(dest.len(), src.len());
    if is_little == cfg!(target_endian = "little") {
        dest.copy_from_slice(src);
    } else {
        for (d, s) in dest.iter_mut().zip(src.iter().rev()) {
            *d = *s;
        }
    }
}

/// Unpack a (possibly signed) integer from `data[0..size]`.
///
/// C: `static lua_Integer unpackint(lua_State *L, const char *str, int islittle, int size, int issigned)`
fn unpackint(state: &LuaState, data: &[u8], is_little: bool, size: usize, is_signed: bool) -> Result<i64, LuaError> {
    let limit = size.min(SZINT);
    let mut res: u64 = 0;
    for i in (0..limit).rev() {
        res <<= NB;
        let byte_idx = if is_little { i } else { size - 1 - i };
        res |= data[byte_idx] as u64;
    }

    if size < SZINT {
        if is_signed {
            let mask: u64 = 1u64 << (size * NB as usize - 1);
            res = (res ^ mask).wrapping_sub(mask);
        }
    } else if size > SZINT {
        let mask = if !is_signed || (res as i64) >= 0 { 0u8 } else { MC };
        for i in limit..size {
            let byte_idx = if is_little { i } else { size - 1 - i };
            if data[byte_idx] != mask {
                return Err(LuaError::runtime(format_args!(
                    "{}-byte integer does not fit into Lua Integer", size
                )));
            }
        }
    }
    Ok(res as i64)
}

/// `string.pack(fmt, ...)` — pack values into a binary string.
///
/// C: `static int str_pack(lua_State *L)`
pub fn str_pack(state: &mut LuaState) -> Result<usize, LuaError> {
    let fmt_bytes = state.check_arg_string(1)?.to_vec();
    let fmt = &fmt_bytes[..];
    let mut h = Header::new();
    let mut arg = 1i32;
    let mut total_size = 0usize;
    let mut buf: Vec<u8> = Vec::new();
    let mut pos = 0usize;

    while pos < fmt.len() {
        let mut size = 0usize;
        let mut ntoalign = 0usize;
        let opt = getdetails(&mut h, total_size, fmt, &mut pos, &mut size, &mut ntoalign)?;
        total_size += ntoalign + size;
        for _ in 0..ntoalign {
            buf.push(PACK_PAD_BYTE);
        }
        arg += 1;

        match opt {
            KOption::Int => {
                let n = state.check_arg_integer(arg)?;
                if size < SZINT {
                    let lim: i64 = 1i64 << (size * NB as usize - 1);
                    if !(-lim <= n && n < lim) {
                        return Err(LuaError::arg_error(arg, "integer overflow"));
                    }
                }
                packint(&mut buf, n as u64, h.is_little, size, n < 0);
            }
            KOption::Uint => {
                let n = state.check_arg_integer(arg)?;
                if size < SZINT {
                    let lim: u64 = 1u64 << (size * NB as usize);
                    if (n as u64) >= lim {
                        return Err(LuaError::arg_error(arg, "unsigned overflow"));
                    }
                }
                packint(&mut buf, n as u64, h.is_little, size, false);
            }
            KOption::Float => {
                let f = state.check_arg_number(arg)? as f32;
                let start = buf.len();
                buf.resize(start + 4, 0);
                copywithendian(&mut buf[start..start + 4], &f.to_bits().to_ne_bytes(), h.is_little);
            }
            KOption::Number => {
                let f = state.check_arg_number(arg)?;
                let start = buf.len();
                buf.resize(start + 8, 0);
                copywithendian(&mut buf[start..start + 8], &f.to_bits().to_ne_bytes(), h.is_little);
            }
            KOption::Double => {
                let f = state.check_arg_number(arg)? as f64;
                let start = buf.len();
                buf.resize(start + 8, 0);
                copywithendian(&mut buf[start..start + 8], &f.to_bits().to_ne_bytes(), h.is_little);
            }
            KOption::Char => {
                let s = state.check_arg_string(arg)?.to_vec();
                if s.len() > size {
                    return Err(LuaError::arg_error(arg, "string longer than given size"));
                }
                buf.extend_from_slice(&s);
                let pad = size - s.len();
                for _ in 0..pad {
                    buf.push(PACK_PAD_BYTE);
                }
            }
            KOption::Kstring => {
                let s = state.check_arg_string(arg)?.to_vec();
                let len = s.len();
                if size < SZINT && len >= (1usize << (size * 8)) {
                    return Err(LuaError::arg_error(arg, "string length does not fit in given size"));
                }
                packint(&mut buf, len as u64, h.is_little, size, false);
                buf.extend_from_slice(&s);
                total_size += len;
            }
            KOption::Zstr => {
                let s = state.check_arg_string(arg)?.to_vec();
                if s.contains(&0) {
                    return Err(LuaError::arg_error(arg, "string contains zeros"));
                }
                buf.extend_from_slice(&s);
                buf.push(0);
                total_size += s.len() + 1;
            }
            KOption::Padding => {
                buf.push(PACK_PAD_BYTE);
                arg -= 1; // undo increment
            }
            KOption::Paddalign | KOption::Nop => {
                arg -= 1; // undo increment
            }
        }
    }

    state.push_bytes(&buf)?;
    Ok(1)
}

/// `string.packsize(fmt)` — return the byte-size the format would produce.
///
/// C: `static int str_packsize(lua_State *L)`
pub fn str_packsize(state: &mut LuaState) -> Result<usize, LuaError> {
    let fmt_bytes = state.check_arg_string(1)?.to_vec();
    let fmt = &fmt_bytes[..];
    let mut h = Header::new();
    let mut total_size = 0usize;
    let mut pos = 0usize;

    while pos < fmt.len() {
        let mut size = 0usize;
        let mut ntoalign = 0usize;
        let opt = getdetails(&mut h, total_size, fmt, &mut pos, &mut size, &mut ntoalign)?;
        if opt == KOption::Kstring || opt == KOption::Zstr {
            return Err(LuaError::arg_error(1, "variable-length format"));
        }
        let space = ntoalign + size;
        if total_size > usize::MAX - space {
            return Err(LuaError::arg_error(1, "format result too large"));
        }
        total_size += space;
    }
    state.push(LuaValue::Int(total_size as i64));
    Ok(1)
}

/// `string.unpack(fmt, s [, pos])` — unpack binary data from string.
///
/// C: `static int str_unpack(lua_State *L)`
pub fn str_unpack(state: &mut LuaState) -> Result<usize, LuaError> {
    let fmt_bytes = state.check_arg_string(1)?.to_vec();
    let data_bytes = state.check_arg_string(2)?.to_vec();
    let ld = data_bytes.len();
    let pos_raw = state.opt_arg_integer(3, 1)?;
    let mut pos = pos_relat_i(pos_raw, ld).saturating_sub(1);

    if pos > ld {
        return Err(LuaError::arg_error(3, "initial position out of string"));
    }

    let fmt = &fmt_bytes[..];
    let data = &data_bytes[..];
    let mut h = Header::new();
    let mut fmt_pos = 0usize;
    let mut n = 0usize;

    while fmt_pos < fmt.len() {
        let mut size = 0usize;
        let mut ntoalign = 0usize;
        let opt = getdetails(&mut h, pos, fmt, &mut fmt_pos, &mut size, &mut ntoalign)?;

        if ntoalign + size > ld - pos {
            return Err(LuaError::arg_error(2, "data string too short"));
        }
        pos += ntoalign;
        state.ensure_stack(2, "too many results")?;
        n += 1;

        match opt {
            KOption::Int => {
                let v = unpackint(state, &data[pos..pos + size], h.is_little, size, true)?;
                state.push(LuaValue::Int(v));
            }
            KOption::Uint => {
                let v = unpackint(state, &data[pos..pos + size], h.is_little, size, false)?;
                state.push(LuaValue::Int(v));
            }
            KOption::Float => {
                let mut bytes = [0u8; 4];
                copywithendian(&mut bytes, &data[pos..pos + 4], h.is_little);
                let f = f32::from_bits(u32::from_ne_bytes(bytes));
                state.push(LuaValue::Float(f as f64));
            }
            KOption::Number => {
                let mut bytes = [0u8; 8];
                copywithendian(&mut bytes, &data[pos..pos + 8], h.is_little);
                let f = f64::from_bits(u64::from_ne_bytes(bytes));
                state.push(LuaValue::Float(f));
            }
            KOption::Double => {
                let mut bytes = [0u8; 8];
                copywithendian(&mut bytes, &data[pos..pos + 8], h.is_little);
                let f = f64::from_bits(u64::from_ne_bytes(bytes));
                state.push(LuaValue::Float(f));
            }
            KOption::Char => {
                state.push_bytes(&data[pos..pos + size])?;
            }
            KOption::Kstring => {
                let len = unpackint(state, &data[pos..pos + size], h.is_little, size, false)? as usize;
                if len > ld - pos - size {
                    return Err(LuaError::arg_error(2, "data string too short"));
                }
                state.push_bytes(&data[pos + size..pos + size + len])?;
                pos += len;
            }
            KOption::Zstr => {
                let end = data[pos..].iter().position(|&b| b == 0)
                    .ok_or_else(|| LuaError::arg_error(2, "unfinished string for format 'z'"))?;
                if pos + end >= ld {
                    return Err(LuaError::arg_error(2, "unfinished string for format 'z'"));
                }
                state.push_bytes(&data[pos..pos + end])?;
                pos += end + 1;
            }
            KOption::Paddalign | KOption::Padding | KOption::Nop => {
                n -= 1; // undo increment
            }
        }
        pos += size;
    }

    state.push(LuaValue::Int((pos + 1) as i64));
    Ok(n + 1)
}

// ────────────────────────────────────────────────────────────────────────────
// §9  Module registration
// ────────────────────────────────────────────────────────────────────────────

/// Function table for `string` library.
///
/// C: `static const luaL_Reg strlib[]`
pub const STRING_LIB: &[(&[u8], lua_CFunction)] = &[
    (b"byte",     str_byte),
    (b"char",     str_char),
    (b"dump",     str_dump),
    (b"find",     str_find),
    (b"format",   str_format),
    (b"gmatch",   gmatch),
    (b"gsub",     str_gsub),
    (b"len",      str_len),
    (b"lower",    str_lower),
    (b"match",    str_match),
    (b"rep",      str_rep),
    (b"reverse",  str_reverse),
    (b"sub",      str_sub),
    (b"upper",    str_upper),
    (b"pack",     str_pack),
    (b"packsize", str_packsize),
    (b"unpack",   str_unpack),
];

/// Metamethods to install on the string metatable.
///
/// C: `static const luaL_Reg stringmetamethods[]`
pub const STRING_META_METHODS: &[(&[u8], lua_CFunction)] = &[
    (b"__add",  arith_add),
    (b"__sub",  arith_sub),
    (b"__mul",  arith_mul),
    (b"__mod",  arith_mod),
    (b"__pow",  arith_pow),
    (b"__div",  arith_div),
    (b"__idiv", arith_idiv),
    (b"__unm",  arith_unm),
];

/// Create the string metatable and set it as the metatable for all strings.
///
/// C: `static void createmetatable(lua_State *L)`
pub fn createmetatable(state: &mut LuaState) -> Result<(), LuaError> {
    // C: luaL_newlibtable(L, stringmetamethods);
    state.new_lib_table(STRING_META_METHODS)?;
    // C: luaL_setfuncs(L, stringmetamethods, 0);
    state.set_funcs(STRING_META_METHODS, 0)?;
    // C: lua_pushliteral(L, ""); lua_pushvalue(L, -2); lua_setmetatable(L, -2);
    state.push_string(b"")?;
    let mt_idx = state.top_idx() - 2;
    let mt = state.get_at(mt_idx);
    state.push(mt);
    state.set_metatable(-2)?;
    // C: lua_pop(L, 1);
    state.pop_n(1);
    // C: lua_pushvalue(L, -2); lua_setfield(L, -2, "__index");
    let strlib_idx = state.top_idx() - 2;
    let strlib = state.get_at(strlib_idx);
    state.push(strlib);
    state.set_field(-2, b"__index")?;
    // C: lua_pop(L, 1);
    state.pop_n(1);
    Ok(())
}

/// `luaopen_string` — open the string library.
///
/// C: `LUAMOD_API int luaopen_string(lua_State *L)`
pub fn luaopen_string(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: luaL_newlib(L, strlib);
    state.new_lib(STRING_LIB)?;
    createmetatable(state)?;
    Ok(1)
}

// ────────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        src/lstrlib.c  (1875 lines, 46 functions)
//   target_crate:  lua-stdlib
//   confidence:    medium
//   todos:         13
//   port_notes:    4
//   unsafe_blocks: 0
//   notes:         Pattern engine uses index-based MatchState (not raw ptrs).
//                  string.format delegates numeric widths/precision/flags to
//                  Phase B (a sprintf-compatible crate or manual impl).
//                  gmatch iterator state (GMatchState userdata + upvalue closure)
//                  is stubbed — requires userdata + CClosure support in LuaState.
//                  copywithendian uses safe byte-level swapping (no transmute).
//                  unpackint sign-extension uses two's-complement bit tricks;
//                  logic review needed in Phase B.
//                  str_dump requires state.dump_function() which is not yet
//                  defined; Phase B wires up the ldump.c port.
//                  addquoted uses 3-digit escape for all control chars (slight
//                  deviation from C which uses 1-digit when safe); benign.
// ────────────────────────────────────────────────────────────────────────────
