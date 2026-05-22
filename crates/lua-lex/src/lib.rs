//! Lexical analyzer — port of `llex.c` + `llex.h`.
//!
//! Provides the Lua 5.4 lexer: character-by-character scanning of a [`ZIO`]
//! input stream into [`Token`] values, with one-token lookahead.  The
//! `llex.h` header is merged here per PORTING.md §1.
//!
//! # C source files
//! - `reference/lua-5.4.7/src/llex.c`  (581 lines, 24 functions)
//! - `reference/lua-5.4.7/src/llex.h`  (91 lines; merged here)
//!
//! # Design notes
//! - `LexState.L` (back-pointer to `lua_State`) is removed.  All functions
//!   that need `LuaState` receive it as `state: &mut LuaState`.
//! - `Token.token` is `i32` in Phase A (matching the C `int token` field).
//!   Single-byte tokens are their ASCII values; reserved-word tokens start at
//!   `FIRST_RESERVED` (257).  A proper `TokenKind` enum is deferred to Phase B.
//! - `save` / `save_and_next` are now fallible (`Result<(), LuaError>`); the
//!   `?` operator replaces the C noreturn `lexerror` call on buffer overflow.
//! - The `goto read_save / only_save / no_save` pattern in `read_string` is
//!   translated via the local `EscapeResult` enum.

// TODO(port): resolve remaining cross-crate calls (intern_str, table anchor,
// number parsing, utf8 encoding) in Phase B.  Canonical cross-crate type
// imports are now in place per harness/type-vocabulary.tsv (see below).

use std::rc::Rc;
use std::io::Write as IoWrite;

// PORT NOTE: GcRef<T> = Rc<T> in Phases A–C; replaced by real GC pointer in Phase D.
// TODO(port): move GcRef to lua-types once the GC crate is defined (Phase D).
type GcRef<T> = Rc<T>;

// Canonical cross-crate types: imported from owner crates per
// harness/type-vocabulary.tsv.  See PORTING.md §7.
pub use lua_types::LuaError;
pub use lua_types::LuaString;
pub use lua_vm::state::LuaState;
pub use lua_vm::table::LuaTable;

/// Placeholder for `LexBuffer` from `lua_vm::zio`.
/// TODO(port): replace with `use lua_vm::zio::LexBuffer` in Phase B.
/// C: `Mbuffer` — growable byte buffer for token text.
/// types.tsv: Mbuffer → LexBuffer
pub struct LexBuffer {
    buffer: Vec<u8>,
}

impl LexBuffer {
    /// C: `luaZ_initbuffer` — construct an empty buffer.
    pub fn new() -> Self {
        LexBuffer { buffer: Vec::new() }
    }

    /// C: `#define luaZ_bufflen(b) ((b)->n)` — live byte count.
    /// macros.tsv: luaZ_bufflen → buf.len()
    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    /// C: `#define luaZ_sizebuffer(b) ((b)->buffsize)` — allocated capacity.
    /// macros.tsv: luaZ_sizebuffer → buf.capacity()
    pub fn capacity(&self) -> usize {
        self.buffer.capacity()
    }

    /// C: `#define luaZ_buffer(b) ((b)->buffer)` — raw byte slice.
    /// macros.tsv: luaZ_buffer → buf.as_mut_slice()
    pub fn as_slice(&self) -> &[u8] {
        &self.buffer
    }

    /// C: `#define luaZ_resetbuffer(b) ((b)->n = 0)` — reset to zero length.
    /// macros.tsv: luaZ_resetbuffer → buf.clear()
    pub fn clear(&mut self) {
        self.buffer.clear();
    }

    /// C: `#define luaZ_buffremove(b, i) ((b)->n -= (i))`.
    /// macros.tsv: luaZ_buffremove → buf.truncate_by(i)
    pub fn truncate_by(&mut self, i: usize) {
        let new_len = self.buffer.len().saturating_sub(i);
        self.buffer.truncate(new_len);
    }

    /// C: `luaZ_resizebuffer(L, b, newsize)` — grow/shrink the buffer's
    /// allocated capacity. In C this changes `buffsize`, not the live byte
    /// count `n`. The Rust analogue therefore manipulates `Vec::capacity`,
    /// never `Vec::len` (otherwise `push_byte` would write past the live
    /// content and leave embedded zero padding inside the token text).
    pub fn resize(&mut self, _state: &mut LuaState, size: usize) -> Result<(), LuaError> {
        if size < self.buffer.len() {
            self.buffer.truncate(size);
        }
        if size > self.buffer.capacity() {
            let extra = size - self.buffer.capacity();
            self.buffer.reserve_exact(extra);
        }
        Ok(())
    }

    /// Append one byte to the live contents.  Panics if capacity exceeded
    /// (callers must pre-check via `save`).
    fn push_byte(&mut self, c: u8) {
        self.buffer.push(c);
    }
}

impl Default for LexBuffer {
    fn default() -> Self {
        Self::new()
    }
}

/// Placeholder for `ZIO` from `lua_vm::zio`.
/// TODO(port): replace with `use lua_vm::zio::ZIO` in Phase B.
/// C: `ZIO` — buffered input stream.
/// types.tsv: Zio → ZIO
pub struct ZIO {
    // TODO(port): full ZIO implementation lives in lua_vm::zio; this is a stub.
    reader: Box<dyn FnMut() -> Option<Vec<u8>>>,
    n: usize,
    p: usize,
    current_chunk: Vec<u8>,
}

impl ZIO {
    /// Construct a ZIO from a reader callback that yields successive chunks.
    pub fn new(reader: Box<dyn FnMut() -> Option<Vec<u8>>>) -> Self {
        ZIO { reader, n: 0, p: 0, current_chunk: Vec::new() }
    }

    /// Construct a ZIO that yields the supplied bytes once and then EOZ.
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        let mut once = Some(bytes);
        ZIO::new(Box::new(move || once.take()))
    }

    /// C: `#define zgetc(z) (((z)->n--)>0 ? cast_uchar(*(z)->p++) : luaZ_fill(z))`
    /// macros.tsv: zgetc → z.getc()
    pub fn getc(&mut self) -> i32 {
        if self.n > 0 {
            self.n -= 1;
            let b = self.current_chunk[self.p] as u8;
            self.p += 1;
            b as i32
        } else {
            self.fill()
        }
    }

    fn fill(&mut self) -> i32 {
        match (self.reader)() {
            None => EOZ,
            Some(chunk) if chunk.is_empty() => EOZ,
            Some(chunk) => {
                self.n = chunk.len() - 1;
                self.current_chunk = chunk;
                self.p = 0;
                let b = self.current_chunk[self.p] as u8;
                self.p += 1;
                b as i32
            }
        }
    }
}

// ── Constants ─────────────────────────────────────────────────────────────────

// C: #define FIRST_RESERVED  (UCHAR_MAX + 1)
// macros.tsv: FIRST_RESERVED → const FIRST_RESERVED: i32 = 257
/// First token kind value that is not a single-byte character.
/// Single-byte tokens are represented by their ASCII value (0-255).
pub const FIRST_RESERVED: i32 = 257;

// C: #define LUA_ENV  "_ENV"
// macros.tsv: LUA_ENV → const LUA_ENV: &[u8] = b"_ENV"
/// Name of the global environment upvalue.
pub const LUA_ENV: &[u8] = b"_ENV";

// C: #define NUM_RESERVED  (cast_int(TK_WHILE - FIRST_RESERVED + 1))
// macros.tsv: NUM_RESERVED → const NUM_RESERVED: usize = (TK_WHILE - FIRST_RESERVED + 1) as usize
/// Number of reserved words (keywords).
pub const NUM_RESERVED: usize = (TK_WHILE - FIRST_RESERVED + 1) as usize;

// C: #define EOZ  (-1)   (from lzio.h)
// macros.tsv: EOZ → const EOZ: i32 = -1
/// End-of-stream sentinel returned by ZIO::getc.
pub const EOZ: i32 = -1;

// C: MAX_SIZE (llimits.h)
// macros.tsv: MAX_SIZE → const MAX_SIZE: usize = ...
const MAX_SIZE: usize = if std::mem::size_of::<usize>() < std::mem::size_of::<i64>() {
    usize::MAX
} else {
    i64::MAX as usize
};

// C: #define LUA_MINBUFFER  32   (llimits.h)
// macros.tsv: LUA_MIN_BUFFER → const LUA_MIN_BUFFER: usize = 32
const LUA_MIN_BUFFER: usize = 32;

// ── Token kind constants (ORDER RESERVED — matches C enum RESERVED) ───────────
//
// In C these are enum values.  In Rust we use i32 constants for Phase A
// (faithful to `Token.token: int` in C) with a TODO for a proper enum in Phase B.
//
// C: enum RESERVED { TK_AND = FIRST_RESERVED, TK_BREAK, ... }

/// `and`
pub const TK_AND: i32 = 257;
/// `break`
pub const TK_BREAK: i32 = 258;
/// `do`
pub const TK_DO: i32 = 259;
/// `else`
pub const TK_ELSE: i32 = 260;
/// `elseif`
pub const TK_ELSEIF: i32 = 261;
/// `end`
pub const TK_END: i32 = 262;
/// `false`
pub const TK_FALSE: i32 = 263;
/// `for`
pub const TK_FOR: i32 = 264;
/// `function`
pub const TK_FUNCTION: i32 = 265;
/// `goto`
pub const TK_GOTO: i32 = 266;
/// `if`
pub const TK_IF: i32 = 267;
/// `in`
pub const TK_IN: i32 = 268;
/// `local`
pub const TK_LOCAL: i32 = 269;
/// `nil`
pub const TK_NIL: i32 = 270;
/// `not`
pub const TK_NOT: i32 = 271;
/// `or`
pub const TK_OR: i32 = 272;
/// `repeat`
pub const TK_REPEAT: i32 = 273;
/// `return`
pub const TK_RETURN: i32 = 274;
/// `then`
pub const TK_THEN: i32 = 275;
/// `true`
pub const TK_TRUE: i32 = 276;
/// `until`
pub const TK_UNTIL: i32 = 277;
/// `while`  (last keyword; NUM_RESERVED = TK_WHILE - FIRST_RESERVED + 1 = 22)
pub const TK_WHILE: i32 = 278;
/// `//`  (floor division)
pub const TK_IDIV: i32 = 279;
/// `..`  (concatenation)
pub const TK_CONCAT: i32 = 280;
/// `...` (vararg)
pub const TK_DOTS: i32 = 281;
/// `==`
pub const TK_EQ: i32 = 282;
/// `>=`
pub const TK_GE: i32 = 283;
/// `<=`
pub const TK_LE: i32 = 284;
/// `~=`
pub const TK_NE: i32 = 285;
/// `<<`
pub const TK_SHL: i32 = 286;
/// `>>`
pub const TK_SHR: i32 = 287;
/// `::`
pub const TK_DBCOLON: i32 = 288;
/// `<eof>`
pub const TK_EOS: i32 = 289;
/// `<number>`  (float literal)
pub const TK_FLT: i32 = 290;
/// `<integer>` (integer literal)
pub const TK_INT: i32 = 291;
/// `<name>`    (identifier)
pub const TK_NAME: i32 = 292;
/// `<string>`  (string literal)
pub const TK_STRING: i32 = 293;

// C: static const char *const luaX_tokens [] = { ... };
// ORDER RESERVED — index 0 = TK_AND - FIRST_RESERVED, etc.
/// Display strings for tokens, indexed by `token - FIRST_RESERVED`.
pub static LUAX_TOKENS: &[&[u8]] = &[
    // keywords (indices 0-21)
    b"and", b"break", b"do", b"else", b"elseif",
    b"end", b"false", b"for", b"function", b"goto", b"if",
    b"in", b"local", b"nil", b"not", b"or", b"repeat",
    b"return", b"then", b"true", b"until", b"while",
    // other terminal symbols (indices 22-35)
    b"//", b"..", b"...", b"==", b">=", b"<=", b"~=",
    b"<<", b">>", b"::", b"<eof>",
    b"<number>", b"<integer>", b"<name>", b"<string>",
];

// ── SemInfo / TokenValue ───────────────────────────────────────────────────────

// C: typedef union { lua_Number r; lua_Integer i; TString *ts; } SemInfo;
// types.tsv: SemInfo → TokenValue
/// Semantic payload carried by a token.
///
/// Corresponds to `SemInfo` (a C union) in `llex.h`.  In Rust this is a
/// discriminated union (enum).
///
/// # C mapping
/// ```text
/// SemInfo.r   → TokenValue::Float(f64)      (lua_Number)
/// SemInfo.i   → TokenValue::Int(i64)        (lua_Integer)
/// SemInfo.ts  → TokenValue::Str(GcRef<LuaString>)
/// (no C field) → TokenValue::None           (default / unset)
/// ```
#[derive(Clone)]
pub enum TokenValue {
    /// No semantic value (default; used for single-byte and most multi-char tokens).
    None,
    /// Float literal payload.  C: `seminfo.r` (`lua_Number`).
    Float(f64),
    /// Integer literal payload.  C: `seminfo.i` (`lua_Integer`).
    Int(i64),
    /// String/name payload.  C: `seminfo.ts` (`TString *`).
    Str(GcRef<LuaString>),
}

// ── Token ─────────────────────────────────────────────────────────────────────

// C: typedef struct Token { int token; SemInfo seminfo; } Token;
// types.tsv: Token → Token;  Token.token → i32 (Phase A; TODO: TokenKind enum Phase B)
/// A single lexed token with its semantic payload.
///
/// `kind` is an `i32` whose value is either an ASCII byte code (for single-byte
/// tokens like `+`, `-`, `[`) or one of the `TK_*` constants (for reserved
/// words, multi-char symbols, and literals).
///
/// TODO(port): Phase B — replace `kind: i32` with a proper `TokenKind` enum
/// covering both single-byte and named tokens (e.g. `TokenKind::Char(u8)` +
/// named variants).
#[derive(Clone)]
pub struct Token {
    // C: int token;
    pub kind: i32,
    // C: SemInfo seminfo;
    pub value: TokenValue,
}

impl Token {
    /// Construct a token with no semantic value.
    pub fn new(kind: i32) -> Self {
        Token { kind, value: TokenValue::None }
    }

    /// The end-of-stream sentinel token.
    pub fn eos() -> Self {
        Token::new(TK_EOS)
    }
}

// ── LexState ──────────────────────────────────────────────────────────────────

// C: typedef struct LexState { ... } LexState;
// types.tsv: LexState → LexState;  LexState.L removed (thread via &mut LuaState)
/// Per-chunk lexer (and shared parser) state.
///
/// Corresponds to `LexState` in `llex.h`.  Owns the input stream, token
/// buffer, and current/lookahead tokens.
///
/// # C mapping (types.tsv)
/// ```text
/// LexState.current    → current: i32        (charint; -1 = EOZ)
/// LexState.linenumber → linenumber: i32
/// LexState.lastline   → lastline: i32
/// LexState.t          → t: Token            (current token)
/// LexState.lookahead  → lookahead: Token    (one-token lookahead)
/// LexState.fs         → fs: Option<Box<FuncState>>   (parser state)
/// LexState.L          → (removed; callers pass &mut LuaState)
/// LexState.z          → z: ZIO              (owned input stream)
/// LexState.buff       → buff: LexBuffer     (owned token-text buffer)
/// LexState.h          → h: GcRef<LuaTable>  (string-anchor table)
/// LexState.dyd        → dyd: DynData        (parser dynamic data)
/// LexState.source     → source: GcRef<LuaString>
/// LexState.envn       → envn: GcRef<LuaString>
/// ```
pub struct LexState {
    // C: int current;  /* current character (charint) */
    pub current: i32,
    // C: int linenumber;  /* input line counter */
    pub linenumber: i32,
    // C: int lastline;  /* line of last token 'consumed' */
    pub lastline: i32,
    // C: Token t;  /* current token */
    pub t: Token,
    // C: Token lookahead;  /* look ahead token */
    pub lookahead: Token,
    // C: struct FuncState *fs;  /* current function (parser) */
    // TODO(port): Box<FuncState> once FuncState lands in lua-parse (Phase B)
    pub fs: Option<()>,
    // C: ZIO *z;  /* input stream */
    // PORT NOTE: C held a pointer; Rust owns the ZIO directly per types.tsv.
    pub z: ZIO,
    // C: Mbuffer *buff;  /* buffer for tokens */
    // PORT NOTE: C held a pointer; Rust owns the LexBuffer directly per types.tsv.
    pub buff: LexBuffer,
    // C: Table *h;  /* to avoid collection/reuse strings */
    // TODO(port): GcRef<LuaTable> once LuaTable is defined in Phase B
    pub h: Option<GcRef<LuaTable>>,
    // C: struct Dyndata *dyd;  /* dynamic structures used by the parser */
    // TODO(port): DynData once parser types land in Phase B
    pub dyd: Option<()>,
    // C: TString *source;  /* current source name */
    pub source: GcRef<LuaString>,
    // C: TString *envn;  /* environment variable name */
    pub envn: GcRef<LuaString>,
}

// ── Character-classification helpers ─────────────────────────────────────────
//
// C: `lctype.h` — Lua's own ctype table.
// These are simplified ASCII implementations for Phase A.
// TODO(port): import from lua_vm::ctype in Phase B; the full table handles
// the LUA_UCID (Unicode identifiers) flag and matches the C bit-table exactly.
//
// PORT NOTE: the C macros take `int` (not `char`) so they handle EOZ (-1) safely.
// These Rust fns match that contract: EOZ returns false for all predicates.

// C: #define lisdigit(c)   (testprop(c, DIGITBIT))
#[inline]
fn is_digit(c: i32) -> bool {
    c >= b'0' as i32 && c <= b'9' as i32
}

// C: #define lisxdigit(c)  (testprop(c, XDIGITBIT))
#[inline]
fn is_xdigit(c: i32) -> bool {
    (c >= b'0' as i32 && c <= b'9' as i32)
        || (c >= b'a' as i32 && c <= b'f' as i32)
        || (c >= b'A' as i32 && c <= b'F' as i32)
}

// C: #define lislalpha(c)  (testprop(c, ALPHABIT))
// ALPHABIT: ASCII letters + '_'
#[inline]
fn is_lalpha(c: i32) -> bool {
    (c >= b'a' as i32 && c <= b'z' as i32)
        || (c >= b'A' as i32 && c <= b'Z' as i32)
        || c == b'_' as i32
}

// C: #define lislalnum(c)  (testprop(c, ALPHABIT|DIGITBIT))
#[inline]
fn is_lalnum(c: i32) -> bool {
    is_lalpha(c) || is_digit(c)
}

// C: #define lisspace(c)   (testprop(c, SPACEBIT))
#[inline]
fn is_space(c: i32) -> bool {
    matches!(c, 9 | 10 | 11 | 12 | 13 | 32) // \t \n \v \f \r space
}

// C: #define lisprint(c)   (testprop(c, PRINTBIT))
// PRINTBIT: printable ASCII (graph + space), i.e. 0x20-0x7E
#[inline]
fn is_print(c: i32) -> bool {
    c >= 0x20 && c <= 0x7E
}

// C: #define currIsNewline(ls)  (ls->current == '\n' || ls->current == '\r')
#[inline]
fn curr_is_newline(ls: &LexState) -> bool {
    ls.current == b'\n' as i32 || ls.current == b'\r' as i32
}

// ── Low-level stream helpers ───────────────────────────────────────────────────

// C: #define next(ls)  (ls->current = zgetc(ls->z))
/// Advance the lexer by one character.
///
/// Corresponds to the `next(ls)` macro.  Named `advance` to avoid collision
/// with Rust's iterator method.
#[inline]
fn advance(ls: &mut LexState) {
    // C: ls->current = zgetc(ls->z)
    // macros.tsv: zgetc → z.getc()
    ls.current = ls.z.getc();
}

// C: static void save (LexState *ls, int c) { ... }
/// Append character `c` to the token buffer, growing it if necessary.
///
/// On overflow calls [`lex_error`] which becomes `Err(LuaError::Syntax(...))`.
///
/// # C source
/// ```c
/// // C: static void save (LexState *ls, int c) {
/// //   Mbuffer *b = ls->buff;
/// //   if (luaZ_bufflen(b) + 1 > luaZ_sizebuffer(b)) {
/// //     size_t newsize;
/// //     if (luaZ_sizebuffer(b) >= MAX_SIZE/2)
/// //       lexerror(ls, "lexical element too long", 0);
/// //     newsize = luaZ_sizebuffer(b) * 2;
/// //     luaZ_resizebuffer(ls->L, b, newsize);
/// //   }
/// //   b->buffer[luaZ_bufflen(b)++] = cast_char(c);
/// // }
/// ```
fn save(ls: &mut LexState, state: &mut LuaState, c: i32) -> Result<(), LuaError> {
    // C: if (luaZ_bufflen(b) + 1 > luaZ_sizebuffer(b))
    // macros.tsv: luaZ_bufflen → buf.len(); luaZ_sizebuffer → buf.capacity()
    if ls.buff.len() + 1 > ls.buff.capacity() {
        // C: if (luaZ_sizebuffer(b) >= MAX_SIZE/2) lexerror(...)
        if ls.buff.capacity() >= MAX_SIZE / 2 {
            return Err(lex_error(ls, b"lexical element too long", 0));
        }
        // C: newsize = luaZ_sizebuffer(b) * 2;
        //    luaZ_resizebuffer(ls->L, b, newsize);
        // macros.tsv: luaZ_resizebuffer → buf.resize(state, size)?
        let newsize = ls.buff.capacity() * 2;
        ls.buff.resize(state, newsize)?;
    }
    // C: b->buffer[luaZ_bufflen(b)++] = cast_char(c);
    // macros.tsv: cast_char → x as i8  (C char is signed; Lua bytes stored as-is)
    // PORT NOTE: we store the byte value directly; the i8 cast in C is for the
    // C char type but the data is read back as unsigned via cast_uchar everywhere.
    ls.buff.push_byte(c as u8);
    Ok(())
}

// C: #define save_and_next(ls) (save(ls, ls->current), next(ls))
/// Save the current character into the token buffer, then advance the stream.
///
/// Corresponds to the `save_and_next(ls)` macro.  Fallible because `save`
/// may need to grow the buffer.
#[inline]
fn save_and_next(ls: &mut LexState, state: &mut LuaState) -> Result<(), LuaError> {
    // C: save(ls, ls->current)
    let c = ls.current;
    save(ls, state, c)?;
    // C: next(ls)
    advance(ls);
    Ok(())
}

// ── Error helpers ─────────────────────────────────────────────────────────────

// C: static l_noret lexerror (LexState *ls, const char *msg, int token)
// l_noret → -> !  but in Rust we return LuaError (callers wrap in Err(...))
// error_sites.tsv: luaX_lexerror → return Err(LuaError::syntax_at(ls, "msg", token))
/// Build a syntax error, optionally annotated with the offending token text.
///
/// Corresponds to the static `lexerror` function in `llex.c`.  In C this is
/// `l_noret` (diverges via `luaD_throw`); in Rust it returns a `LuaError`
/// value that callers wrap in `Err(...)`.
///
/// # C source
/// ```c
/// // C: static l_noret lexerror (LexState *ls, const char *msg, int token) {
/// //   msg = luaG_addinfo(ls->L, msg, ls->source, ls->linenumber);
/// //   if (token)
/// //     luaO_pushfstring(ls->L, "%s near %s", msg, txtToken(ls, token));
/// //   luaD_throw(ls->L, LUA_ERRSYNTAX);
/// // }
/// ```
fn lex_error(ls: &mut LexState, msg: &[u8], token: i32) -> LuaError {
    // C: msg = luaG_addinfo(ls->L, msg, ls->source, ls->linenumber);
    // TODO(port): call lua_vm::debug::add_info(msg, &ls.source, ls.linenumber)
    // to prepend "<source>:<line>: " prefix.  For Phase A, build it manually:
    let mut full_msg: Vec<u8> = Vec::new();
    let _ = write!(full_msg, "{}:{}: ", "<source>", ls.linenumber);
    full_msg.extend_from_slice(msg);

    // C: if (token) luaO_pushfstring(ls->L, "%s near %s", msg, txtToken(ls, token));
    if token != 0 {
        let tok_text = txt_token(ls, token);
        full_msg.extend_from_slice(b" near ");
        full_msg.extend_from_slice(&tok_text);
    }

    // C: luaD_throw(ls->L, LUA_ERRSYNTAX);
    // error_sites.tsv: → LuaError::syntax(...)
    // TODO(port): LuaError::syntax(format_args!(...)) once LuaError is wired;
    // for Phase A return a placeholder via the stub LuaError type.
    LuaError::syntax_raw(&full_msg)
}

// C: l_noret luaX_syntaxerror (LexState *ls, const char *msg)
// LUAI_FUNC → pub(crate)
// error_sites.tsv: luaX_syntaxerror → return Err(LuaError::syntax(format_args!("msg")))
/// Report a syntax error at the current token.
///
/// # C source
/// ```c
/// // C: l_noret luaX_syntaxerror (LexState *ls, const char *msg) {
/// //   lexerror(ls, msg, ls->t.token);
/// // }
/// ```
pub(crate) fn syntax_error(ls: &mut LexState, msg: &[u8]) -> LuaError {
    // C: lexerror(ls, msg, ls->t.token);
    let token = ls.t.kind;
    lex_error(ls, msg, token)
}

// C: static const char *txtToken (LexState *ls, int token)
/// Produce a human-readable representation of `token` for error messages.
///
/// For `TK_NAME`, `TK_STRING`, `TK_FLT`, `TK_INT`: formats the current
/// token buffer contents as `'<text>'`.  For everything else, delegates to
/// [`token2str`].
///
/// # C source
/// ```c
/// // C: static const char *txtToken (LexState *ls, int token) {
/// //   switch (token) {
/// //     case TK_NAME: case TK_STRING:
/// //     case TK_FLT: case TK_INT:
/// //       save(ls, '\0');
/// //       return luaO_pushfstring(ls->L, "'%s'", luaZ_buffer(ls->buff));
/// //     default:
/// //       return luaX_token2str(ls, token);
/// //   }
/// // }
/// ```
///
/// PORT NOTE: C calls `luaO_pushfstring` which pushes the string onto the
/// Lua stack (stack-anchored temporary).  Rust returns `Vec<u8>` directly
/// since there is no stack-based string lifecycle for error formatting.
fn txt_token(ls: &mut LexState, token: i32) -> Vec<u8> {
    match token {
        // C: case TK_NAME: case TK_STRING: case TK_FLT: case TK_INT:
        t if t == TK_NAME || t == TK_STRING || t == TK_FLT || t == TK_INT => {
            // C: save(ls, '\0') — NUL-terminate the buffer for use as C string
            // In Rust we don't NUL-terminate; use the live bytes directly.
            // C: return luaO_pushfstring(ls->L, "'%s'", luaZ_buffer(ls->buff));
            // macros.tsv: luaZ_buffer → buf.as_mut_slice()
            let mut v: Vec<u8> = Vec::new();
            v.push(b'\'');
            v.extend_from_slice(ls.buff.as_slice());
            v.push(b'\'');
            v
        }
        // C: default: return luaX_token2str(ls, token);
        _ => token2str_raw(token),
    }
}

// C: const char *luaX_token2str (LexState *ls, int token)
// LUAI_FUNC → pub(crate)
/// Produce a human-readable token description (for error messages and the parser).
///
/// Single-byte printable tokens are formatted as `'X'`; non-printable as
/// `'<\N>'`.  Reserved words and multi-char symbols are formatted as `'kw'`.
/// Literal tokens (`<name>`, `<string>`, etc.) return the bare label.
///
/// # C source
/// ```c
/// // C: const char *luaX_token2str (LexState *ls, int token) {
/// //   if (token < FIRST_RESERVED) {
/// //     if (lisprint(token))
/// //       return luaO_pushfstring(ls->L, "'%c'", token);
/// //     else
/// //       return luaO_pushfstring(ls->L, "'<\\%d>'", token);
/// //   }
/// //   else {
/// //     const char *s = luaX_tokens[token - FIRST_RESERVED];
/// //     if (token < TK_EOS)
/// //       return luaO_pushfstring(ls->L, "'%s'", s);
/// //     else
/// //       return s;
/// //   }
/// // }
/// ```
///
/// PORT NOTE: The `LexState` parameter is retained in the signature for API
/// parity with the C export, but is unused in Rust because we don't push onto
/// the Lua stack.  The real formatting is in [`token2str_raw`].
pub(crate) fn token2str(_ls: &LexState, token: i32) -> Vec<u8> {
    token2str_raw(token)
}

/// Inner implementation of [`token2str`] that does not need `LexState`.
fn token2str_raw(token: i32) -> Vec<u8> {
    if token < FIRST_RESERVED {
        // C: if (lisprint(token)) return "'%c'"; else return "'<\\%d>'"
        if is_print(token) {
            // C: luaO_pushfstring(ls->L, "'%c'", token)
            vec![b'\'', token as u8, b'\'']
        } else {
            // C: luaO_pushfstring(ls->L, "'<\\%d>'", token)
            // PORT NOTE: uses write! to Vec<u8> to avoid String allocation for Lua data.
            let mut v: Vec<u8> = Vec::new();
            v.extend_from_slice(b"'<\\");
            let _ = write!(&mut v, "{}", token);
            v.extend_from_slice(b">'");
            v
        }
    } else {
        let idx = (token - FIRST_RESERVED) as usize;
        let s = LUAX_TOKENS[idx];
        if token < TK_EOS {
            // C: luaO_pushfstring(ls->L, "'%s'", s)  — wrap in single quotes
            let mut v: Vec<u8> = Vec::with_capacity(s.len() + 2);
            v.push(b'\'');
            v.extend_from_slice(s);
            v.push(b'\'');
            v
        } else {
            // C: return s  — bare label like "<name>", "<eof>"
            s.to_vec()
        }
    }
}

// ── Public init / setup ───────────────────────────────────────────────────────

// C: void luaX_init (lua_State *L)
// LUAI_FUNC → pub(crate)
/// Initialise the lexer subsystem: intern all reserved words and fix them
/// in the GC so they are never collected.
///
/// Must be called exactly once during VM startup via `luaX_init`.
///
/// # C source
/// ```c
/// // C: void luaX_init (lua_State *L) {
/// //   int i;
/// //   TString *e = luaS_newliteral(L, LUA_ENV);  /* create env name */
/// //   luaC_fix(L, obj2gco(e));  /* never collect this name */
/// //   for (i=0; i<NUM_RESERVED; i++) {
/// //     TString *ts = luaS_new(L, luaX_tokens[i]);
/// //     luaC_fix(L, obj2gco(ts));  /* reserved words are never collected */
/// //     ts->extra = cast_byte(i+1);  /* reserved word */
/// //   }
/// // }
/// ```
pub fn init(state: &mut LuaState) -> Result<(), LuaError> {
    // C: TString *e = luaS_newliteral(L, LUA_ENV);
    // macros.tsv: luaS_newliteral → state.intern_str(b"...")
    // TODO(port): call state.intern_str(LUA_ENV) once LuaState has that method (Phase B)
    let _e = intern_str_stub(state, LUA_ENV)?;

    // C: luaC_fix(L, obj2gco(e));  /* never collect this name */
    // macros.tsv: luaC_objbarrier / luaC_fix — GC fix; no-op in Phases A-C
    // TODO(port): state.gc().fix(e) in Phase D

    for i in 0..NUM_RESERVED {
        // C: TString *ts = luaS_new(L, luaX_tokens[i]);
        // macros.tsv: luaS_new → state.intern_str(...)
        // TODO(port): call state.intern_str(LUAX_TOKENS[i]) in Phase B
        let ts = intern_str_stub(state, LUAX_TOKENS[i])?;

        // C: luaC_fix(L, obj2gco(ts));  /* reserved words are never collected */
        // TODO(port): state.gc().fix(ts.clone()) in Phase D

        // C: ts->extra = cast_byte(i+1);  /* reserved word */
        // macros.tsv: cast_byte → x as u8
        // PORT NOTE: LuaString.extra uses Cell<u8> interior mutability.
        // TODO(port): ts.set_extra((i + 1) as u8) — needs pub accessor on LuaString
        let _ = ts; // suppress unused warning until Phase B
    }

    Ok(())
}

// C: void luaX_setinput (lua_State *L, LexState *ls, ZIO *z, TString *source, int firstchar)
// LUAI_FUNC → pub(crate)
/// Initialise `ls` for lexing a new chunk from stream `z`.
///
/// # C source
/// ```c
/// // C: void luaX_setinput (lua_State *L, LexState *ls, ZIO *z,
/// //                         TString *source, int firstchar) {
/// //   ls->t.token = 0;
/// //   ls->L = L;
/// //   ls->current = firstchar;
/// //   ls->lookahead.token = TK_EOS;  /* no look-ahead token */
/// //   ls->z = z;
/// //   ls->fs = NULL;
/// //   ls->linenumber = 1;
/// //   ls->lastline = 1;
/// //   ls->source = source;
/// //   ls->envn = luaS_newliteral(L, LUA_ENV);  /* get env name */
/// //   luaZ_resizebuffer(ls->L, ls->buff, LUA_MINBUFFER);
/// // }
/// ```
pub fn set_input(
    state: &mut LuaState,
    ls: &mut LexState,
    z: ZIO,
    source: GcRef<LuaString>,
    firstchar: i32,
) -> Result<(), LuaError> {
    // C: ls->t.token = 0;
    ls.t = Token::new(0);
    // C: ls->L = L;  — removed; state is threaded via fn params
    // C: ls->current = firstchar;
    ls.current = firstchar;
    // C: ls->lookahead.token = TK_EOS;
    ls.lookahead = Token::eos();
    // C: ls->z = z;
    ls.z = z;
    // C: ls->fs = NULL;
    ls.fs = None;
    // C: ls->linenumber = 1;
    ls.linenumber = 1;
    // C: ls->lastline = 1;
    ls.lastline = 1;
    // C: ls->source = source;
    ls.source = source;
    // C: ls->envn = luaS_newliteral(L, LUA_ENV);
    // macros.tsv: luaS_newliteral → state.intern_str(b"...")
    // TODO(port): state.intern_str(LUA_ENV) in Phase B
    ls.envn = intern_str_stub(state, LUA_ENV)?;
    // C: luaZ_resizebuffer(ls->L, ls->buff, LUA_MINBUFFER);
    // macros.tsv: luaZ_resizebuffer → buf.resize(state, size)?
    ls.buff.resize(state, LUA_MIN_BUFFER)?;
    Ok(())
}

// C: TString *luaX_newstring (LexState *ls, const char *str, size_t l)
// LUAI_FUNC → pub(crate)
/// Create (or retrieve) a Lua string and anchor it in the parser's GC-protection
/// table `ls.h` so it cannot be collected before the end of compilation.
///
/// Also internalises long strings so that each unique content has exactly one
/// copy in memory.  The table `ls.h` is used as a set: the string is both the
/// key and the value.
///
/// # C source
/// ```c
/// // C: TString *luaX_newstring (LexState *ls, const char *str, size_t l) {
/// //   lua_State *L = ls->L;
/// //   TString *ts = luaS_newlstr(L, str, l);
/// //   const TValue *o = luaH_getstr(ls->h, ts);
/// //   if (!ttisnil(o))  /* string already present? */
/// //     ts = keystrval(nodefromval(o));  /* get saved copy */
/// //   else {
/// //     TValue *stv = s2v(L->top.p++);  /* reserve stack space */
/// //     setsvalue(L, stv, ts);           /* anchor the string */
/// //     luaH_finishset(L, ls->h, stv, o, stv);  /* t[string] = string */
/// //     luaC_checkGC(L);
/// //     L->top.p--;                       /* remove string from stack */
/// //   }
/// //   return ts;
/// // }
/// ```
pub(crate) fn new_string(
    state: &mut LuaState,
    ls: &mut LexState,
    bytes: &[u8],
) -> Result<GcRef<LuaString>, LuaError> {
    // C: TString *ts = luaS_newlstr(L, str, l);
    // macros.tsv: luaS_newlstr → state.intern_str(&s[..n])
    // TODO(port): call state.intern_str(bytes) in Phase B
    let ts = intern_str_stub(state, bytes)?;

    // C: const TValue *o = luaH_getstr(ls->h, ts);
    // TODO(port): luaH_getstr — table string lookup; needs Phase B table ops.
    // For Phase A, skip the anchor-table logic and return the interned string directly.
    // The anchor table prevents GC of strings during parsing.  In Phase A-C,
    // Rc keeps strings alive as long as there is a reference.
    // PORT NOTE: the anchor-table set/get logic (luaH_getstr, luaH_finishset,
    // setsvalue, s2v stack push/pop, luaC_checkGC) is omitted for Phase A.
    // Phase B must wire ls.h as a GcRef<LuaTable> and call these operations.

    Ok(ts)
}

// ── Public advance / lookahead ─────────────────────────────────────────────────

// C: void luaX_next (LexState *ls)
// LUAI_FUNC → pub(crate)
/// Consume the current token; load the next one from the stream.
///
/// If a lookahead token was set, it becomes the current token without re-reading
/// from the stream.
///
/// # C source
/// ```c
/// // C: void luaX_next (LexState *ls) {
/// //   ls->lastline = ls->linenumber;
/// //   if (ls->lookahead.token != TK_EOS) {
/// //     ls->t = ls->lookahead;
/// //     ls->lookahead.token = TK_EOS;
/// //   }
/// //   else
/// //     ls->t.token = llex(ls, &ls->t.seminfo);
/// // }
/// ```
pub fn next(
    state: &mut LuaState,
    ls: &mut LexState,
) -> Result<(), LuaError> {
    // C: ls->lastline = ls->linenumber;
    ls.lastline = ls.linenumber;

    // C: if (ls->lookahead.token != TK_EOS)
    if ls.lookahead.kind != TK_EOS {
        // C: ls->t = ls->lookahead;
        // Clone to avoid borrow conflict; LuaString inside TokenValue is GcRef (Rc).
        ls.t = ls.lookahead.clone();
        // C: ls->lookahead.token = TK_EOS;
        ls.lookahead = Token::eos();
    } else {
        // C: ls->t.token = llex(ls, &ls->t.seminfo);
        let mut val = TokenValue::None;
        let kind = llex(state, ls, &mut val)?;
        ls.t = Token { kind, value: val };
    }
    Ok(())
}

// C: int luaX_lookahead (LexState *ls)
// LUAI_FUNC → pub(crate)
/// Peek at the next token without consuming the current one.
///
/// The lookahead token is cached in `ls.lookahead` and returned.  Only one
/// token of lookahead is supported; calling this twice without an intervening
/// [`next`] is a logic error (asserted in debug builds).
///
/// # C source
/// ```c
/// // C: int luaX_lookahead (LexState *ls) {
/// //   lua_assert(ls->lookahead.token == TK_EOS);
/// //   ls->lookahead.token = llex(ls, &ls->lookahead.seminfo);
/// //   return ls->lookahead.token;
/// // }
/// ```
pub fn lookahead(
    state: &mut LuaState,
    ls: &mut LexState,
) -> Result<i32, LuaError> {
    // C: lua_assert(ls->lookahead.token == TK_EOS);
    // macros.tsv: lua_assert → debug_assert!
    debug_assert!(
        ls.lookahead.kind == TK_EOS,
        "luaX_lookahead: lookahead already set"
    );

    // C: ls->lookahead.token = llex(ls, &ls->lookahead.seminfo);
    let mut val = TokenValue::None;
    let kind = llex(state, ls, &mut val)?;
    ls.lookahead = Token { kind, value: val };

    // C: return ls->lookahead.token;
    Ok(ls.lookahead.kind)
}

// ── Private lexer helpers ──────────────────────────────────────────────────────

// C: static int check_next1 (LexState *ls, int c)
/// If the current character equals `c`, advance and return `true`.
///
/// # C source
/// ```c
/// // C: static int check_next1 (LexState *ls, int c) {
/// //   if (ls->current == c) { next(ls); return 1; }
/// //   else return 0;
/// // }
/// ```
fn check_next1(ls: &mut LexState, c: i32) -> bool {
    if ls.current == c {
        // C: next(ls)
        advance(ls);
        true
    } else {
        false
    }
}

// C: static int check_next2 (LexState *ls, const char *set)
/// If the current character is either of the two bytes in `set`, save-and-advance
/// and return `true`.
///
/// # C source
/// ```c
/// // C: static int check_next2 (LexState *ls, const char *set) {
/// //   lua_assert(set[2] == '\0');
/// //   if (ls->current == set[0] || ls->current == set[1]) {
/// //     save_and_next(ls);
/// //     return 1;
/// //   }
/// //   else return 0;
/// // }
/// ```
fn check_next2(
    ls: &mut LexState,
    state: &mut LuaState,
    set: &[u8; 2],
) -> Result<bool, LuaError> {
    // C: lua_assert(set[2] == '\0');  — guaranteed by [u8;2] type
    if ls.current == set[0] as i32 || ls.current == set[1] as i32 {
        // C: save_and_next(ls)
        save_and_next(ls, state)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

// C: static void inclinenumber (LexState *ls)
/// Increment the line counter and consume the newline sequence.
///
/// Handles `\n`, `\r`, `\n\r`, and `\r\n`.
///
/// # C source
/// ```c
/// // C: static void inclinenumber (LexState *ls) {
/// //   int old = ls->current;
/// //   lua_assert(currIsNewline(ls));
/// //   next(ls);  /* skip '\n' or '\r' */
/// //   if (currIsNewline(ls) && ls->current != old)
/// //     next(ls);  /* skip '\n\r' or '\r\n' */
/// //   if (++ls->linenumber >= MAX_INT)
/// //     lexerror(ls, "chunk has too many lines", 0);
/// // }
/// ```
fn inc_line_number(ls: &mut LexState, _state: &mut LuaState) -> Result<(), LuaError> {
    // C: lua_assert(currIsNewline(ls))
    // macros.tsv: lua_assert → debug_assert!
    debug_assert!(curr_is_newline(ls), "inc_line_number: not at a newline");

    let old = ls.current;
    // C: next(ls)  — skip '\n' or '\r'
    advance(ls);

    // C: if (currIsNewline(ls) && ls->current != old) next(ls)
    if curr_is_newline(ls) && ls.current != old {
        advance(ls);
    }

    // C: if (++ls->linenumber >= MAX_INT) lexerror(...)
    // macros.tsv: MAX_INT → i32::MAX
    ls.linenumber += 1;
    if ls.linenumber >= i32::MAX {
        return Err(lex_error(ls, b"chunk has too many lines", 0));
    }
    Ok(())
}

// C: static int read_numeral (LexState *ls, SemInfo *seminfo)
/// Scan a numeric literal (integer or float, decimal or hex).
///
/// The caller may have already read an initial dot.  Accepts the pattern:
/// `%d(%x|%.|(Ee[+-]?))*` or `0[Xx](%x|%.|(Pp[+-]?))*`.
///
/// Returns `TK_INT` for integers, `TK_FLT` for floats.
///
/// # C source
/// ```c
/// // C: static int read_numeral (LexState *ls, SemInfo *seminfo) {
/// //   TValue obj;
/// //   const char *expo = "Ee";
/// //   int first = ls->current;
/// //   lua_assert(lisdigit(ls->current));
/// //   save_and_next(ls);
/// //   if (first == '0' && check_next2(ls, "xX"))  /* hexadecimal? */
/// //     expo = "Pp";
/// //   for (;;) {
/// //     if (check_next2(ls, expo))
/// //       check_next2(ls, "-+");
/// //     else if (lisxdigit(ls->current) || ls->current == '.')
/// //       save_and_next(ls);
/// //     else break;
/// //   }
/// //   if (lislalpha(ls->current))  /* numeral touching a letter? */
/// //     save_and_next(ls);         /* force an error */
/// //   save(ls, '\0');
/// //   if (luaO_str2num(luaZ_buffer(ls->buff), &obj) == 0)
/// //     lexerror(ls, "malformed number", TK_FLT);
/// //   if (ttisinteger(&obj)) { seminfo->i = ivalue(&obj); return TK_INT; }
/// //   else { seminfo->r = fltvalue(&obj); return TK_FLT; }
/// // }
/// ```
fn read_numeral(
    state: &mut LuaState,
    ls: &mut LexState,
    seminfo: &mut TokenValue,
) -> Result<i32, LuaError> {
    // C: const char *expo = "Ee";
    let mut expo: &[u8; 2] = b"Ee";

    // C: int first = ls->current;
    let first = ls.current;

    // C: lua_assert(lisdigit(ls->current))
    debug_assert!(is_digit(ls.current), "read_numeral: not at a digit");

    // C: save_and_next(ls);
    save_and_next(ls, state)?;

    // C: if (first == '0' && check_next2(ls, "xX"))
    if first == b'0' as i32 && check_next2(ls, state, b"xX")? {
        expo = b"Pp";
    }

    loop {
        // C: if (check_next2(ls, expo))
        if check_next2(ls, state, expo)? {
            // C: check_next2(ls, "-+")
            check_next2(ls, state, b"-+")?;
        } else if is_xdigit(ls.current) || ls.current == b'.' as i32 {
            // C: else if (lisxdigit(ls->current) || ls->current == '.')
            //      save_and_next(ls);
            save_and_next(ls, state)?;
        } else {
            break;
        }
    }

    // C: if (lislalpha(ls->current)) save_and_next(ls);  /* force an error */
    if is_lalpha(ls.current) {
        save_and_next(ls, state)?;
    }

    // C: save(ls, '\0') — NUL-terminate the buffer for C's str2num
    // In Rust, luaO_str2num will receive a byte slice; NUL is not needed.
    // We save 0 for parity with C, but our str2num stub ignores it.
    save(ls, state, 0)?;

    // C: if (luaO_str2num(luaZ_buffer(ls->buff), &obj) == 0)
    //        lexerror(ls, "malformed number", TK_FLT);
    // macros.tsv: luaZ_buffer → buf.as_mut_slice()
    let buf = ls.buff.as_slice();
    let num_bytes = if buf.last() == Some(&0) { &buf[..buf.len() - 1] } else { buf };
    let mut obj = lua_types::LuaValue::Nil;
    if lua_vm::object::str2num(num_bytes, &mut obj) == 0 {
        return Err(lex_error(ls, b"malformed number", TK_FLT));
    }
    match obj {
        lua_types::LuaValue::Int(i) => {
            *seminfo = TokenValue::Int(i);
            Ok(TK_INT)
        }
        lua_types::LuaValue::Float(f) => {
            *seminfo = TokenValue::Float(f);
            Ok(TK_FLT)
        }
        _ => unreachable!("str2num returned non-numeric LuaValue"),
    }
}

// C: static size_t skip_sep (LexState *ls)
/// Scan a `[=*[` or `]=*]` sequence; leave the last bracket as current char.
///
/// Returns:
/// - `count + 2` if well-formed (where `count` is the number of `=` signs),
/// - `1` if a single bracket with no `=`s and no second bracket,
/// - `0` if malformed (e.g. `[==` with no closing bracket).
///
/// # C source
/// ```c
/// // C: static size_t skip_sep (LexState *ls) {
/// //   size_t count = 0;
/// //   int s = ls->current;
/// //   lua_assert(s == '[' || s == ']');
/// //   save_and_next(ls);
/// //   while (ls->current == '=') {
/// //     save_and_next(ls);
/// //     count++;
/// //   }
/// //   return (ls->current == s) ? count + 2
/// //          : (count == 0) ? 1
/// //          : 0;
/// // }
/// ```
fn skip_sep(
    state: &mut LuaState,
    ls: &mut LexState,
) -> Result<usize, LuaError> {
    let mut count: usize = 0;
    let s = ls.current;
    // C: lua_assert(s == '[' || s == ']')
    debug_assert!(s == b'[' as i32 || s == b']' as i32, "skip_sep: not at bracket");

    // C: save_and_next(ls)
    save_and_next(ls, state)?;

    // C: while (ls->current == '=')
    while ls.current == b'=' as i32 {
        save_and_next(ls, state)?;
        count += 1;
    }

    // C: return (ls->current == s) ? count + 2 : (count == 0) ? 1 : 0;
    if ls.current == s {
        Ok(count + 2)
    } else if count == 0 {
        Ok(1)
    } else {
        Ok(0)
    }
}

// C: static void read_long_string (LexState *ls, SemInfo *seminfo, size_t sep)
/// Scan a long string or long comment delimited by `[=*[` … `]=*]`.
///
/// `seminfo` is `Some` when reading a string literal; `None` when skipping a
/// long comment.  When `None`, buffer contents are discarded on each newline
/// to avoid wasting memory.
///
/// # C source
/// ```c
/// // C: static void read_long_string (LexState *ls, SemInfo *seminfo, size_t sep) {
/// //   int line = ls->linenumber;
/// //   save_and_next(ls);  /* skip 2nd '[' */
/// //   if (currIsNewline(ls)) inclinenumber(ls);
/// //   for (;;) {
/// //     switch (ls->current) {
/// //       case EOZ: { /* error */
/// //         const char *what = (seminfo ? "string" : "comment");
/// //         const char *msg = luaO_pushfstring(..., what, line);
/// //         lexerror(ls, msg, TK_EOS);
/// //         break;
/// //       }
/// //       case ']': {
/// //         if (skip_sep(ls) == sep) {
/// //           save_and_next(ls);  /* skip 2nd ']' */
/// //           goto endloop;
/// //         }
/// //         break;
/// //       }
/// //       case '\n': case '\r': {
/// //         save(ls, '\n');
/// //         inclinenumber(ls);
/// //         if (!seminfo) luaZ_resetbuffer(ls->buff);
/// //         break;
/// //       }
/// //       default: {
/// //         if (seminfo) save_and_next(ls);
/// //         else next(ls);
/// //       }
/// //     }
/// //   } endloop:
/// //   if (seminfo)
/// //     seminfo->ts = luaX_newstring(ls, luaZ_buffer(ls->buff) + sep,
/// //                                      luaZ_bufflen(ls->buff) - 2 * sep);
/// // }
/// ```
fn read_long_string(
    state: &mut LuaState,
    ls: &mut LexState,
    seminfo: Option<&mut TokenValue>,
    sep: usize,
) -> Result<(), LuaError> {
    let line = ls.linenumber; // C: int line = ls->linenumber;

    // C: save_and_next(ls)  — skip 2nd '['
    save_and_next(ls, state)?;

    // C: if (currIsNewline(ls)) inclinenumber(ls);
    if curr_is_newline(ls) {
        inc_line_number(ls, state)?;
    }

    // is_string: whether we are reading a string (true) or a comment (false)
    let is_string = seminfo.is_some();

    loop {
        match ls.current {
            // C: case EOZ:
            c if c == EOZ => {
                // C: const char *what = (seminfo ? "string" : "comment");
                let what: &[u8] = if is_string { b"string" } else { b"comment" };
                // C: luaO_pushfstring(ls->L, "unfinished long %s (starting at line %d)", what, line)
                // PORT NOTE: build message as Vec<u8> to avoid String allocation.
                let mut msg: Vec<u8> = Vec::new();
                msg.extend_from_slice(b"unfinished long ");
                msg.extend_from_slice(what);
                msg.extend_from_slice(b" (starting at line ");
                let _ = write!(&mut msg, "{}", line);
                msg.push(b')');
                return Err(lex_error(ls, &msg, TK_EOS));
            }
            // C: case ']':
            c if c == b']' as i32 => {
                let s = skip_sep(state, ls)?;
                if s == sep {
                    // C: save_and_next(ls)  — skip 2nd ']'
                    save_and_next(ls, state)?;
                    break; // C: goto endloop
                }
                // else: the ']' sequence wasn't the closing delimiter; continue
            }
            // C: case '\n': case '\r':
            c if c == b'\n' as i32 || c == b'\r' as i32 => {
                // C: save(ls, '\n')
                save(ls, state, b'\n' as i32)?;
                // C: inclinenumber(ls)
                inc_line_number(ls, state)?;
                // C: if (!seminfo) luaZ_resetbuffer(ls->buff)
                // macros.tsv: luaZ_resetbuffer → buf.clear()
                if !is_string {
                    ls.buff.clear();
                }
            }
            // C: default:
            _ => {
                if is_string {
                    // C: if (seminfo) save_and_next(ls)
                    save_and_next(ls, state)?;
                } else {
                    // C: else next(ls)
                    advance(ls);
                }
            }
        }
    }

    // C: if (seminfo)
    //      seminfo->ts = luaX_newstring(ls, luaZ_buffer(ls->buff) + sep,
    //                                       luaZ_bufflen(ls->buff) - 2 * sep);
    if let Some(out) = seminfo {
        // The buffer contains: sep bytes of '[=' + content + sep bytes of '=]'
        // We want the content in between.
        // PORT NOTE: per PORTING.md §4.3, capture the slice into an owned
        // Vec so the immutable borrow of ls.buff is dropped before the
        // mutable borrow needed by new_string.
        let buf = ls.buff.as_slice();
        let content: Vec<u8> = buf[sep..buf.len() - sep].to_vec();
        let ts = new_string(state, ls, &content)?;
        *out = TokenValue::Str(ts);
    }
    Ok(())
}

// C: static void esccheck (LexState *ls, int c, const char *msg)
/// Check `c` is non-zero (truthy); if not, save the current char and raise a
/// string-escape error.
///
/// # C source
/// ```c
/// // C: static void esccheck (LexState *ls, int c, const char *msg) {
/// //   if (!c) {
/// //     if (ls->current != EOZ)
/// //       save_and_next(ls);  /* add current to buffer for error message */
/// //     lexerror(ls, msg, TK_STRING);
/// //   }
/// // }
/// ```
fn esc_check(
    state: &mut LuaState,
    ls: &mut LexState,
    ok: bool,
    msg: &[u8],
) -> Result<(), LuaError> {
    if !ok {
        if ls.current != EOZ {
            save_and_next(ls, state)?;
        }
        return Err(lex_error(ls, msg, TK_STRING));
    }
    Ok(())
}

// C: static int gethexa (LexState *ls)
/// Save-and-advance, then verify the new current char is a hex digit; return
/// its numeric value (0-15).
///
/// # C source
/// ```c
/// // C: static int gethexa (LexState *ls) {
/// //   save_and_next(ls);
/// //   esccheck (ls, lisxdigit(ls->current), "hexadecimal digit expected");
/// //   return luaO_hexavalue(ls->current);
/// // }
/// ```
fn get_hexa(
    state: &mut LuaState,
    ls: &mut LexState,
) -> Result<u32, LuaError> {
    // C: save_and_next(ls)
    save_and_next(ls, state)?;
    // C: esccheck(ls, lisxdigit(ls->current), "hexadecimal digit expected")
    esc_check(state, ls, is_xdigit(ls.current), b"hexadecimal digit expected")?;
    // C: return luaO_hexavalue(ls->current)
    // TODO(port): call lua_vm::object::hex_value in Phase B
    Ok(hex_value_stub(ls.current))
}

// C: static int readhexaesc (LexState *ls)
/// Scan a `\xNN` hex escape; return the decoded byte value.
///
/// # C source
/// ```c
/// // C: static int readhexaesc (LexState *ls) {
/// //   int r = gethexa(ls);
/// //   r = (r << 4) + gethexa(ls);
/// //   luaZ_buffremove(ls->buff, 2);  /* remove saved chars from buffer */
/// //   return r;
/// // }
/// ```
fn read_hex_esc(
    state: &mut LuaState,
    ls: &mut LexState,
) -> Result<u32, LuaError> {
    // C: int r = gethexa(ls);
    let r = get_hexa(state, ls)?;
    // C: r = (r << 4) + gethexa(ls);
    let r = (r << 4) + get_hexa(state, ls)?;
    // C: luaZ_buffremove(ls->buff, 2)
    // macros.tsv: luaZ_buffremove → buf.truncate_by(i)
    ls.buff.truncate_by(2);
    Ok(r)
}

// C: static unsigned long readutf8esc (LexState *ls)
/// Scan a `\u{XXXXXX}` UTF-8 escape; return the Unicode codepoint.
///
/// # C source
/// ```c
/// // C: static unsigned long readutf8esc (LexState *ls) {
/// //   unsigned long r;
/// //   int i = 4;  /* chars to remove: '\', 'u', '{', first digit */
/// //   save_and_next(ls);  /* skip 'u' */
/// //   esccheck(ls, ls->current == '{', "missing '{'");
/// //   r = gethexa(ls);  /* must have at least one digit */
/// //   while (cast_void(save_and_next(ls)), lisxdigit(ls->current)) {
/// //     i++;
/// //     esccheck(ls, r <= (0x7FFFFFFFu >> 4), "UTF-8 value too large");
/// //     r = (r << 4) + luaO_hexavalue(ls->current);
/// //   }
/// //   esccheck(ls, ls->current == '}', "missing '}'");
/// //   next(ls);  /* skip '}' */
/// //   luaZ_buffremove(ls->buff, i);
/// //   return r;
/// // }
/// ```
fn read_utf8_esc(
    state: &mut LuaState,
    ls: &mut LexState,
) -> Result<u32, LuaError> {
    // C: int i = 4;  /* chars to remove: '\', 'u', '{', first digit */
    let mut i: usize = 4;

    // C: save_and_next(ls)  — skip 'u'
    save_and_next(ls, state)?;

    // C: esccheck(ls, ls->current == '{', "missing '{'")
    esc_check(state, ls, ls.current == b'{' as i32, b"missing '{'")?;

    // C: r = gethexa(ls)
    let mut r = get_hexa(state, ls)?;

    // C: while (cast_void(save_and_next(ls)), lisxdigit(ls->current)) { ... }
    // cast_void: discard return value
    loop {
        save_and_next(ls, state)?;
        if !is_xdigit(ls.current) {
            break;
        }
        i += 1;
        // C: esccheck(ls, r <= (0x7FFFFFFFu >> 4), "UTF-8 value too large")
        esc_check(state, ls, r <= (0x7FFF_FFFFu32 >> 4), b"UTF-8 value too large")?;
        // C: r = (r << 4) + luaO_hexavalue(ls->current)
        // TODO(port): lua_vm::object::hex_value in Phase B
        r = (r << 4) + hex_value_stub(ls.current);
    }

    // C: esccheck(ls, ls->current == '}', "missing '}'")
    esc_check(state, ls, ls.current == b'}' as i32, b"missing '}'")?;

    // C: next(ls)  — skip '}'
    advance(ls);

    // C: luaZ_buffremove(ls->buff, i)
    ls.buff.truncate_by(i);

    Ok(r)
}

// C: static void utf8esc (LexState *ls)
/// Scan `\u{...}` and append the UTF-8 encoding of the codepoint to the buffer.
///
/// # C source
/// ```c
/// // C: static void utf8esc (LexState *ls) {
/// //   char buff[UTF8BUFFSZ];
/// //   int n = luaO_utf8esc(buff, readutf8esc(ls));
/// //   for (; n > 0; n--)
/// //     save(ls, buff[UTF8BUFFSZ - n]);
/// // }
/// ```
fn utf8_esc(
    state: &mut LuaState,
    ls: &mut LexState,
) -> Result<(), LuaError> {
    // C: unsigned long r = readutf8esc(ls)
    let codepoint = read_utf8_esc(state, ls)?;

    // C: char buff[UTF8BUFFSZ];  int n = luaO_utf8esc(buff, r);
    // macros.tsv: UTF8BUFFSZ → const UTF8_BUF_SZ: usize = 8
    // TODO(port): call lua_vm::object::utf8_esc_encode(codepoint) in Phase B.
    // For Phase A, encode directly here.
    let encoded = utf8_encode_stub(codepoint);

    // C: for (; n > 0; n--) save(ls, buff[UTF8BUFFSZ - n]);
    for &b in &encoded {
        save(ls, state, b as i32)?;
    }
    Ok(())
}

// C: static int readdecesc (LexState *ls)
/// Scan a decimal escape `\ddd` (up to 3 digits); return the byte value.
///
/// # C source
/// ```c
/// // C: static int readdecesc (LexState *ls) {
/// //   int i;
/// //   int r = 0;
/// //   for (i = 0; i < 3 && lisdigit(ls->current); i++) {
/// //     r = 10*r + ls->current - '0';
/// //     save_and_next(ls);
/// //   }
/// //   esccheck(ls, r <= UCHAR_MAX, "decimal escape too large");
/// //   luaZ_buffremove(ls->buff, i);  /* remove read digits from buffer */
/// //   return r;
/// // }
/// ```
fn read_dec_esc(
    state: &mut LuaState,
    ls: &mut LexState,
) -> Result<u32, LuaError> {
    let mut i: usize = 0;
    let mut r: u32 = 0;

    // C: for (i = 0; i < 3 && lisdigit(ls->current); i++)
    while i < 3 && is_digit(ls.current) {
        // C: r = 10*r + ls->current - '0';
        r = 10 * r + (ls.current as u32 - b'0' as u32);
        // C: save_and_next(ls)
        save_and_next(ls, state)?;
        i += 1;
    }

    // C: esccheck(ls, r <= UCHAR_MAX, "decimal escape too large")
    // UCHAR_MAX = 255 = u8::MAX
    esc_check(state, ls, r <= u8::MAX as u32, b"decimal escape too large")?;

    // C: luaZ_buffremove(ls->buff, i)
    ls.buff.truncate_by(i);
    Ok(r)
}

// C: static void read_string (LexState *ls, int del, SemInfo *seminfo)
/// Scan a short (single/double-quoted) string literal.
///
/// The C function uses `goto read_save / only_save / no_save` for escape
/// handling.  In Rust this is replaced by the `EscapeResult` enum.
///
/// # C source (see llex.c lines 382-442 for full listing)
fn read_string(
    state: &mut LuaState,
    ls: &mut LexState,
    del: i32,
    seminfo: &mut TokenValue,
) -> Result<(), LuaError> {
    // Encoding for what the escape sequence handler needs to do after decoding.
    //
    // read_save:  advance(ls), remove '\' from buffer, save decoded byte
    // only_save:  remove '\' from buffer, save decoded byte (no advance)
    // no_save:    nothing (just break from the escape case)
    enum EscapeResult {
        ReadSave(i32),
        OnlySave(i32),
        NoSave,
    }

    // C: save_and_next(ls)  — keep delimiter for error messages
    save_and_next(ls, state)?;

    // C: while (ls->current != del)
    while ls.current != del {
        match ls.current {
            // C: case EOZ: lexerror(ls, "unfinished string", TK_EOS); break;
            c if c == EOZ => {
                return Err(lex_error(ls, b"unfinished string", TK_EOS));
            }
            // C: case '\n': case '\r': lexerror(ls, "unfinished string", TK_STRING); break;
            c if c == b'\n' as i32 || c == b'\r' as i32 => {
                return Err(lex_error(ls, b"unfinished string", TK_STRING));
            }
            // C: case '\\': { ... escape sequence ... }
            c if c == b'\\' as i32 => {
                // C: save_and_next(ls)  — keep '\\' for error messages
                save_and_next(ls, state)?;

                // Inner switch on the escape character
                let esc = match ls.current {
                    // C: case 'a': c = '\a'; goto read_save;
                    c if c == b'a' as i32 => EscapeResult::ReadSave(b'\x07' as i32),
                    // C: case 'b': c = '\b'; goto read_save;
                    c if c == b'b' as i32 => EscapeResult::ReadSave(b'\x08' as i32),
                    // C: case 'f': c = '\f'; goto read_save;
                    c if c == b'f' as i32 => EscapeResult::ReadSave(b'\x0C' as i32),
                    // C: case 'n': c = '\n'; goto read_save;
                    c if c == b'n' as i32 => EscapeResult::ReadSave(b'\n' as i32),
                    // C: case 'r': c = '\r'; goto read_save;
                    c if c == b'r' as i32 => EscapeResult::ReadSave(b'\r' as i32),
                    // C: case 't': c = '\t'; goto read_save;
                    c if c == b't' as i32 => EscapeResult::ReadSave(b'\t' as i32),
                    // C: case 'v': c = '\v'; goto read_save;
                    c if c == b'v' as i32 => EscapeResult::ReadSave(b'\x0B' as i32),
                    // C: case 'x': c = readhexaesc(ls); goto read_save;
                    c if c == b'x' as i32 => {
                        let decoded = read_hex_esc(state, ls)?;
                        EscapeResult::ReadSave(decoded as i32)
                    }
                    // C: case 'u': utf8esc(ls); goto no_save;
                    c if c == b'u' as i32 => {
                        utf8_esc(state, ls)?;
                        EscapeResult::NoSave
                    }
                    // C: case '\n': case '\r': inclinenumber(ls); c = '\n'; goto only_save;
                    c if c == b'\n' as i32 || c == b'\r' as i32 => {
                        inc_line_number(ls, state)?;
                        EscapeResult::OnlySave(b'\n' as i32)
                    }
                    // C: case '\\': case '"': case '\'': c = ls->current; goto read_save;
                    c if c == b'\\' as i32 || c == b'"' as i32 || c == b'\'' as i32 => {
                        EscapeResult::ReadSave(c)
                    }
                    // C: case EOZ: goto no_save;  /* will raise an error next loop */
                    c if c == EOZ => EscapeResult::NoSave,
                    // C: case 'z': { luaZ_buffremove(1); next(ls); while (lisspace) ... }
                    c if c == b'z' as i32 => {
                        // C: luaZ_buffremove(ls->buff, 1)  — remove '\'
                        ls.buff.truncate_by(1);
                        // C: next(ls)  — skip 'z'
                        advance(ls);
                        // C: while (lisspace(ls->current)) { if newline: incline; else next; }
                        while is_space(ls.current) {
                            if curr_is_newline(ls) {
                                inc_line_number(ls, state)?;
                            } else {
                                advance(ls);
                            }
                        }
                        EscapeResult::NoSave
                    }
                    // C: default: esccheck(digit); c = readdecesc(ls); goto only_save;
                    _ => {
                        esc_check(
                            state, ls,
                            is_digit(ls.current),
                            b"invalid escape sequence",
                        )?;
                        let decoded = read_dec_esc(state, ls)?;
                        EscapeResult::OnlySave(decoded as i32)
                    }
                };

                // Dispatch the C goto targets as match arms.
                match esc {
                    // C: read_save: next(ls); /* fall through */ only_save: ...
                    EscapeResult::ReadSave(c) => {
                        advance(ls); // C: next(ls)
                        ls.buff.truncate_by(1); // C: luaZ_buffremove(ls->buff, 1) remove '\'
                        save(ls, state, c)?; // C: save(ls, c)
                    }
                    // C: only_save: luaZ_buffremove(ls->buff, 1); save(ls, c);
                    EscapeResult::OnlySave(c) => {
                        ls.buff.truncate_by(1); // C: luaZ_buffremove(ls->buff, 1) remove '\'
                        save(ls, state, c)?; // C: save(ls, c)
                    }
                    // C: no_save: break;
                    EscapeResult::NoSave => {}
                }
            }
            // C: default: save_and_next(ls);
            _ => {
                save_and_next(ls, state)?;
            }
        }
    }

    // C: save_and_next(ls)  — skip closing delimiter
    save_and_next(ls, state)?;

    // C: seminfo->ts = luaX_newstring(ls, luaZ_buffer(ls->buff) + 1,
    //                                     luaZ_bufflen(ls->buff) - 2);
    // Buffer contains: delimiter + content + delimiter; strip both delimiters.
    // PORT NOTE: capture into owned Vec to drop the borrow before new_string.
    let buf = ls.buff.as_slice();
    let content: Vec<u8> = if buf.len() >= 2 {
        buf[1..buf.len() - 1].to_vec()
    } else {
        Vec::new()
    };
    let ts = new_string(state, ls, &content)?;
    *seminfo = TokenValue::Str(ts);
    Ok(())
}

// C: static int llex (LexState *ls, SemInfo *seminfo)
/// Core lexer dispatch: consume and return the next raw token kind.
///
/// This is the heart of the lexer: a large `for`-`switch` loop that classifies
/// the current character and dispatches to the appropriate scanner.
///
/// # C source (see llex.c lines 445-562 for full listing)
fn llex(
    state: &mut LuaState,
    ls: &mut LexState,
    seminfo: &mut TokenValue,
) -> Result<i32, LuaError> {
    // C: luaZ_resetbuffer(ls->buff)
    // macros.tsv: luaZ_resetbuffer → buf.clear()
    ls.buff.clear();

    loop {
        match ls.current {
            // C: case '\n': case '\r': { inclinenumber(ls); break; }
            c if c == b'\n' as i32 || c == b'\r' as i32 => {
                inc_line_number(ls, state)?;
            }

            // C: case ' ': case '\f': case '\t': case '\v': { next(ls); break; }
            c if c == b' ' as i32
                || c == b'\x0C' as i32
                || c == b'\t' as i32
                || c == b'\x0B' as i32 =>
            {
                advance(ls);
            }

            // C: case '-': { '-' or '--' comment }
            c if c == b'-' as i32 => {
                advance(ls); // C: next(ls)
                if ls.current != b'-' as i32 {
                    return Ok(b'-' as i32);
                }
                // C: /* else is a comment */ next(ls)
                advance(ls);

                if ls.current == b'[' as i32 {
                    // C: long comment?
                    let sep = skip_sep(state, ls)?;
                    // C: luaZ_resetbuffer(ls->buff)
                    ls.buff.clear();
                    if sep >= 2 {
                        // C: read_long_string(ls, NULL, sep)
                        read_long_string(state, ls, None, sep)?;
                        ls.buff.clear(); // C: luaZ_resetbuffer after call
                        continue;
                    }
                }
                // C: short comment — skip until end of line
                while !curr_is_newline(ls) && ls.current != EOZ {
                    advance(ls);
                }
                // loop continues (no token emitted for comments)
            }

            // C: case '[': { long string or simply '[' }
            c if c == b'[' as i32 => {
                let sep = skip_sep(state, ls)?;
                if sep >= 2 {
                    read_long_string(state, ls, Some(seminfo), sep)?;
                    return Ok(TK_STRING);
                } else if sep == 0 {
                    // C: '[=...' missing second bracket
                    return Err(lex_error(ls, b"invalid long string delimiter", TK_STRING));
                }
                // sep == 1: plain '[', no long string
                return Ok(b'[' as i32);
            }

            // C: case '=':
            c if c == b'=' as i32 => {
                advance(ls);
                if check_next1(ls, b'=' as i32) {
                    return Ok(TK_EQ); // C: '=='
                }
                return Ok(b'=' as i32);
            }

            // C: case '<':
            c if c == b'<' as i32 => {
                advance(ls);
                if check_next1(ls, b'=' as i32) {
                    return Ok(TK_LE); // C: '<='
                } else if check_next1(ls, b'<' as i32) {
                    return Ok(TK_SHL); // C: '<<'
                }
                return Ok(b'<' as i32);
            }

            // C: case '>':
            c if c == b'>' as i32 => {
                advance(ls);
                if check_next1(ls, b'=' as i32) {
                    return Ok(TK_GE); // C: '>='
                } else if check_next1(ls, b'>' as i32) {
                    return Ok(TK_SHR); // C: '>>'
                }
                return Ok(b'>' as i32);
            }

            // C: case '/':
            c if c == b'/' as i32 => {
                advance(ls);
                if check_next1(ls, b'/' as i32) {
                    return Ok(TK_IDIV); // C: '//'
                }
                return Ok(b'/' as i32);
            }

            // C: case '~':
            c if c == b'~' as i32 => {
                advance(ls);
                if check_next1(ls, b'=' as i32) {
                    return Ok(TK_NE); // C: '~='
                }
                return Ok(b'~' as i32);
            }

            // C: case ':':
            c if c == b':' as i32 => {
                advance(ls);
                if check_next1(ls, b':' as i32) {
                    return Ok(TK_DBCOLON); // C: '::'
                }
                return Ok(b':' as i32);
            }

            // C: case '"': case '\'': { short literal strings }
            c if c == b'"' as i32 || c == b'\'' as i32 => {
                let del = ls.current;
                read_string(state, ls, del, seminfo)?;
                return Ok(TK_STRING);
            }

            // C: case '.': { '.', '..', '...', or number }
            c if c == b'.' as i32 => {
                save_and_next(ls, state)?;
                if check_next1(ls, b'.' as i32) {
                    if check_next1(ls, b'.' as i32) {
                        return Ok(TK_DOTS); // C: '...'
                    }
                    return Ok(TK_CONCAT); // C: '..'
                } else if !is_digit(ls.current) {
                    return Ok(b'.' as i32);
                } else {
                    return read_numeral(state, ls, seminfo); // C: numeric starting with '.'
                }
            }

            // C: case '0'..='9':
            c if is_digit(c) => {
                return read_numeral(state, ls, seminfo);
            }

            // C: case EOZ: return TK_EOS;
            c if c == EOZ => {
                return Ok(TK_EOS);
            }

            // C: default:
            c => {
                if is_lalpha(c) {
                    // C: identifier or reserved word
                    // C: do { save_and_next(ls); } while (lislalnum(ls->current));
                    loop {
                        save_and_next(ls, state)?;
                        if !is_lalnum(ls.current) {
                            break;
                        }
                    }

                    // C: ts = luaX_newstring(ls, luaZ_buffer(ls->buff), luaZ_bufflen(ls->buff))
                    // PORT NOTE: copy buffer bytes to drop borrow before new_string.
                    let content: Vec<u8> = ls.buff.as_slice().to_vec();
                    let ts = new_string(state, ls, &content)?;

                    // C: seminfo->ts = ts
                    // PORT NOTE: canonical `lua_types::LuaString` lacks the `extra`
                    // byte that C-Lua uses to mark reserved words. Recover the
                    // keyword index directly from the interned bytes via the
                    // `LUAX_TOKENS` table; the first `NUM_RESERVED` entries are
                    // the keywords in declaration order, so token id =
                    // `FIRST_RESERVED + index`.
                    let reserved_token: Option<i32> = LUAX_TOKENS[..NUM_RESERVED]
                        .iter()
                        .position(|kw| *kw == content.as_slice())
                        .map(|i| FIRST_RESERVED + i as i32);
                    *seminfo = TokenValue::Str(ts);

                    if let Some(tk) = reserved_token {
                        return Ok(tk);
                    } else {
                        return Ok(TK_NAME);
                    }
                } else {
                    // C: single-char token — next(ls); return c;
                    let tok = ls.current;
                    advance(ls);
                    return Ok(tok);
                }
            }
        }
    }
}

// ── Phase A stubs for cross-crate helpers ──────────────────────────────────────
//
// The functions below stand in for cross-crate calls that cannot resolve in
// Phase A.  They will be replaced by proper imports in Phase B.

// TODO(port): replace with state.intern_str(bytes) once LuaState gains that
// method (from lua_vm::string::new_lstr wired in Phase B).
// TODO_ARCH(phase-b-reconcile): canonical LuaString is constructed via
// from_bytes; once LuaState::intern_str is wired, route through there instead.
fn intern_str_stub(
    state: &mut LuaState,
    bytes: &[u8],
) -> Result<GcRef<LuaString>, LuaError> {
    Ok(state.intern_str(bytes)?.0)
}

/// Result of converting a byte string to a Lua number.
/// TODO(port): replace with the real `LuaValue` enum variants from lua-types (Phase B).
enum NumResult {
    Int(i64),
    Float(f64),
}

fn str2num_stub(bytes: &[u8]) -> Option<NumResult> {
    let s = bytes.iter().position(|&b| b == 0)
        .map(|n| &bytes[..n])
        .unwrap_or(bytes);
    let mut value = lua_types::LuaValue::Nil;
    if lua_vm::object::str2num(s, &mut value) == 0 {
        return None;
    }
    match value {
        lua_types::LuaValue::Int(i) => Some(NumResult::Int(i)),
        lua_types::LuaValue::Float(f) => Some(NumResult::Float(f)),
        _ => None,
    }
}

// TODO(port): replace with lua_vm::object::hex_value(c) in Phase B.
fn hex_value_stub(c: i32) -> u32 {
    match c {
        c if c >= b'0' as i32 && c <= b'9' as i32 => (c - b'0' as i32) as u32,
        c if c >= b'a' as i32 && c <= b'f' as i32 => (c - b'a' as i32 + 10) as u32,
        c if c >= b'A' as i32 && c <= b'F' as i32 => (c - b'A' as i32 + 10) as u32,
        _ => 0,
    }
}

// TODO(port): replace with lua_vm::object::utf8_esc_encode(codepoint) in Phase B.
/// Encode a Unicode codepoint as a Lua-extended UTF-8 byte sequence (1 to 6 bytes).
///
/// Faithful port of `luaO_utf8esc` from lobject.c.  Lua permits codepoints up
/// to `0x7FFFFFFF` (5- and 6-byte sequences are non-strict UTF-8 but accepted
/// by `\u{...}` escapes per literals.lua test cases).
fn utf8_encode_stub(codepoint: u32) -> Vec<u8> {
    debug_assert!(codepoint <= 0x7FFF_FFFF);
    if codepoint < 0x80 {
        return vec![codepoint as u8];
    }
    let mut x = codepoint;
    let mut mfb: u32 = 0x3f;
    let mut buf: Vec<u8> = Vec::with_capacity(8);
    loop {
        buf.push(0x80 | ((x & 0x3f) as u8));
        x >>= 6;
        mfb >>= 1;
        if x <= mfb {
            break;
        }
    }
    buf.push(((!mfb << 1) | x) as u8);
    buf.reverse();
    buf
}

// ──────────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        src/llex.c  (581 lines, 24 functions)
//                  src/llex.h  (91 lines; merged)
//   target_crate:  lua-lex
//   confidence:    medium
//   todos:         18
//   port_notes:    12
//   unsafe_blocks: 0   (must be 0 outside lua-gc/lua-coro)
//   notes:         Logic is faithful to the C.  The main structural differences:
//                  (1) LexState.L removed — state threaded via fn params;
//                  (2) save/save_and_next/inclinenumber/helpers are all fallible
//                  (Result<_, LuaError>) because lexerror is no longer noreturn;
//                  (3) goto read_save/only_save/no_save in read_string replaced
//                  by EscapeResult enum; (4) Cross-crate calls (intern_str,
//                  luaH_getstr/finishset, luaG_addinfo, luaO_str2num,
//                  luaO_hexavalue, luaO_utf8esc, luaC_fix, luaC_checkGC) are
//                  stubbed with TODO; (5) LuaError, LuaString, ZIO, LexBuffer,
//                  LuaState defined as local stubs — Phase B replaces with real
//                  imports once the crate graph is wired.  Key Phase B tasks:
//                  wire import paths; move LuaString.extra accessor to pub;
//                  implement luaX_newstring anchor-table logic.  Numeric
//                  literal parsing now delegates to lua_vm::object::str2num
//                  (handles hex integers with wrap-around and hex floats).
// ──────────────────────────────────────────────────────────────────────────────
