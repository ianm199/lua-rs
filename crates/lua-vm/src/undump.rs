//! Load precompiled Lua chunks.
//!
//! Direct port of `reference/lua-5.4.7/src/lundump.c` (335 lines, 20 items).
//! Declarations from `lundump.h` are merged here per PORTING.md §1.
//!
//! The public entry point is [`undump`], which reads a binary Lua chunk from
//! a [`ZIO`] stream and returns a Lua closure ready to call.

// TODO(port): resolve import paths once the crate module graph is settled
// in Phase B.  These are best-guess paths based on other translated files.
use crate::state::LuaState;
#[allow(unused_imports)] use crate::prelude::*;
use crate::zio::ZIO;
use lua_types::error::LuaError;
use lua_types::value::LuaValue;

// PORT NOTE: GcRef<T>, LuaProto, LuaClosure, LuaString, UpvalDesc, LocalVar,
// AbsLineInfo, and Instruction are expected to live in lua_types or lua_vm
// crates.  All paths below are provisional for Phase A.
use lua_types::proto::{LuaProto, UpvalDesc, LocalVar, AbsLineInfo};
use lua_types::closure::LuaLClosure;
use lua_types::string::LuaString;
use lua_types::gc::GcRef;
use lua_types::opcode::Instruction;

// ── Constants (from lundump.h) ─────────────────────────────────────────────

/// Six-byte data marker in the chunk header used to catch conversion errors.
const LUAC_DATA: &[u8] = b"\x19\x93\r\n\x1a\n";

/// Reference integer written in the header to detect integer endianness/size
/// mismatches.
const LUAC_INT: i64 = 0x5678;

// macros.tsv: cast_num → x as f64
/// Reference float written in the header to detect float format mismatches.
const LUAC_NUM: f64 = 370.5;

// LUA_VERSION_NUM = 504 → ((5 * 16) + 4) = 0x54 = 84
/// One-byte version tag: upper nibble = major, lower nibble = minor.
const LUAC_VERSION: u8 = 0x54;

const LUAC_FORMAT: u8 = 0;

const LUA_SIGNATURE: &[u8] = b"\x1bLua";

// macros.tsv: LUAI_MAXSHORTLEN → const MAX_SHORT_LEN: usize = 40
const MAX_SHORT_LEN: usize = 40;

// ── Constant-pool type tags (from lobject.h makevariant) ───────────────────
//
// These are the byte values written by ldump.c into the constants array.
// makevariant(t, v) = t | (v << 4).
//
// PORT NOTE: types.tsv maps LUA_VNIL → LuaValue::Nil etc. but the *byte
// values* used in the binary format are the raw tag integers from lobject.h.
// We define them here as u8 constants so the match in load_constants is
// self-documenting.

const TAG_NIL: u8 = 0x00;
const TAG_FALSE: u8 = 0x01;
const TAG_TRUE: u8 = 0x11;
const TAG_INT: u8 = 0x03;
const TAG_FLOAT: u8 = 0x13;
const TAG_SHORT_STR: u8 = 0x04;
const TAG_LONG_STR: u8 = 0x14;

// ── LoadState ──────────────────────────────────────────────────────────────

/// Loader state bundled for convenience: Lua state, input stream, and the
/// chunk name used in error messages.
///
/// # C mapping
/// ```c
///
/// ```
///
/// PORT NOTE: In C, `LoadState` holds raw pointers to `lua_State` and `ZIO`.
/// In Rust these become references with a shared lifetime `'a`.  The struct is
/// always stack-allocated inside [`undump`] and never escapes the call.
struct LoadState<'a> {
    state: &'a mut LuaState,
    z: &'a mut ZIO,
}

// ── Error helper ───────────────────────────────────────────────────────────

/// Build a syntax error for a malformed binary chunk.
///
/// # C source
/// ```c
///
/// //   luaO_pushfstring(S->L, "%s: bad binary format (%s)", S->name, why);
/// //   luaD_throw(S->L, LUA_ERRSYNTAX);
/// // }
/// ```
///
/// PORT NOTE: `l_noret` in C (diverges via `longjmp`).  In Rust we return
/// `LuaError` and the caller does `return Err(load_error(...))`.  The C
/// pattern `luaO_pushfstring + luaD_throw(LUA_ERRSYNTAX)` collapses to a
/// single `LuaError::syntax` per error_sites.tsv.
///
/// TODO(port): `s.name` is `Vec<u8>`; `LuaError::syntax` takes `format_args!`
/// which requires an `std::fmt::Display` implementor.  `Vec<u8>` does not
/// implement `Display`.  Phase B should add a byte-string formatting path to
/// `LuaError::syntax_bytes` or similar, so the chunk name is included verbatim
/// in the message.
fn load_error(_s: &LoadState<'_>, why: &'static str) -> LuaError {
    LuaError::syntax(format_args!("bad binary format ({})", why))
}

// ── Low-level I/O ──────────────────────────────────────────────────────────

/// Read exactly `buf.len()` bytes from the stream into `buf`.
///
/// # C source
/// ```c
///
/// //   if (luaZ_read(S->Z, b, size) != 0)
/// //     error(S, "truncated chunk");
/// // }
/// ```
///
/// PORT NOTE: C takes `void *b` + explicit `size`.  In Rust we use `&mut [u8]`
/// whose length encodes the byte count.  `luaZ_read` returns the number of
/// bytes NOT read (0 = success), matching `ZIO::read`'s contract.
fn load_block(s: &mut LoadState<'_>, buf: &mut [u8]) -> Result<(), LuaError> {
    // macros.tsv: luaZ_read → z.read(buf)  (returns usize unread)
    if s.z.read(buf) != 0 {
        return Err(load_error(s, "truncated chunk"));
    }
    Ok(())
}

/// Read a single byte from the stream.
///
/// # C source
/// ```c
///
/// //   int b = zgetc(S->Z);
/// //   if (b == EOZ)
/// //     error(S, "truncated chunk");
/// //   return cast_byte(b);
/// // }
/// ```
///
/// PORT NOTE: `cast_byte` → `as u8` per macros.tsv; `zgetc` → `z.getc()`.
fn load_byte(s: &mut LoadState<'_>) -> Result<u8, LuaError> {
    // macros.tsv: zgetc → z.getc()  returning i32
    let b = s.z.getc();
    if b == crate::zio::EOZ {
        return Err(load_error(s, "truncated chunk"));
    }
    // macros.tsv: cast_byte → x as u8
    Ok(b as u8)
}

/// Read a variable-length unsigned integer (7 bits per byte, big-endian,
/// MSB-first continuation flag).
///
/// # C source
/// ```c
///
/// //   size_t x = 0;
/// //   int b;
/// //   limit >>= 7;
/// //   do {
/// //     b = loadByte(S);
/// //     if (x >= limit)
/// //       error(S, "integer overflow");
/// //     x = (x << 7) | (b & 0x7f);
/// //   } while ((b & 0x80) == 0);
/// //   return x;
/// // }
/// ```
///
/// PORT NOTE: The encoding terminates when a byte with the high bit set is
/// seen (the *last* byte has bit 7 = 1).  That is the opposite of the more
/// common LEB128 where the continuation bit means "more follows".
fn load_unsigned(s: &mut LoadState<'_>, limit: usize) -> Result<usize, LuaError> {
    let mut x: usize = 0;
    let limit = limit >> 7;
    loop {
        let b = load_byte(s)? as usize;
        if x >= limit {
            return Err(load_error(s, "integer overflow"));
        }
        x = (x << 7) | (b & 0x7f);
        if (b & 0x80) != 0 {
            break;
        }
    }
    Ok(x)
}

/// Read a `size_t`-sized unsigned value.
///
/// # C source
/// ```c
///
/// //   return loadUnsigned(S, MAX_SIZET);
/// // }
/// ```
///
/// PORT NOTE: `MAX_SIZET` → `usize::MAX` per macros.tsv.
fn load_size(s: &mut LoadState<'_>) -> Result<usize, LuaError> {
    // macros.tsv: MAX_SIZET → usize::MAX
    load_unsigned(s, usize::MAX)
}

/// Read a signed `int`-sized value.
///
/// # C source
/// ```c
///
/// //   return cast_int(loadUnsigned(S, INT_MAX));
/// // }
/// ```
///
/// PORT NOTE: `cast_int` → `x as i32` per macros.tsv.  `INT_MAX` → `i32::MAX
/// as usize`.
fn load_int(s: &mut LoadState<'_>) -> Result<i32, LuaError> {
    // macros.tsv: cast_int → x as i32
    let v = load_unsigned(s, i32::MAX as usize)?;
    Ok(v as i32)
}

/// Read a `lua_Number` (f64) as eight raw native-endian bytes.
///
/// # C source
/// ```c
///
/// //   lua_Number x;
/// //   loadVar(S, x);   /* expands to loadBlock(S, &x, sizeof(x)) */
/// //   return x;
/// // }
/// ```
///
/// PORT NOTE: `loadVar` reads `sizeof(lua_Number) = 8` raw bytes directly
/// into the value.  In Rust we use `f64::from_ne_bytes` (native endian) to
/// reconstruct the value from the eight bytes.  The binary format is host-
/// endian for these fields; the header check verifies endianness compatibility
/// via `LUAC_INT` and `LUAC_NUM` sentinels.
fn load_number(s: &mut LoadState<'_>) -> Result<f64, LuaError> {
    let mut buf = [0u8; 8];
    load_block(s, &mut buf)?;
    // PERF(port): f64::from_ne_bytes is zero-cost — same as C's union cast
    Ok(f64::from_ne_bytes(buf))
}

/// Read a `lua_Integer` (i64) as eight raw native-endian bytes.
///
/// # C source
/// ```c
///
/// //   lua_Integer x;
/// //   loadVar(S, x);   /* expands to loadBlock(S, &x, sizeof(x)) */
/// //   return x;
/// // }
/// ```
///
/// PORT NOTE: Same reasoning as [`load_number`] — uses `i64::from_ne_bytes`.
fn load_integer(s: &mut LoadState<'_>) -> Result<i64, LuaError> {
    let mut buf = [0u8; 8];
    load_block(s, &mut buf)?;
    Ok(i64::from_ne_bytes(buf))
}

// ── String loading ─────────────────────────────────────────────────────────

/// Load a nullable string.  Returns `None` if the stored size is zero.
///
/// # C source
/// ```c
///
/// //   lua_State *L = S->L;
/// //   TString *ts;
/// //   size_t size = loadSize(S);
/// //   if (size == 0) return NULL;
/// //   else if (--size <= LUAI_MAXSHORTLEN) {  /* short string? */
/// //     char buff[LUAI_MAXSHORTLEN];
/// //     loadVector(S, buff, size);
/// //     ts = luaS_newlstr(L, buff, size);
/// //   } else {  /* long string */
/// //     ts = luaS_createlngstrobj(L, size);
/// //     setsvalue2s(L, L->top.p, ts);  /* anchor it (loadVector can GC) */
/// //     luaD_inctop(L);
/// //     loadVector(S, getlngstr(ts), size);
/// //     L->top.p--;
/// //   }
/// //   luaC_objbarrier(L, p, ts);
/// //   return ts;
/// // }
/// ```
///
/// PORT NOTE: The Lua binary format stores `actual_length + 1` so that size=0
/// is the null-string sentinel.  After reading `raw_size`, the actual byte
/// count is `raw_size - 1`.
///
/// PORT NOTE: In C, long strings are created first (to anchor them from GC)
/// and then filled in-place via `getlngstr`.  In Rust, GC anchoring is not
/// needed in Phase A–C (Rc keeps objects alive); we read into a buffer and
/// then create the string.
///
/// TODO(port): `luaS_newlstr` interns the string (short strings only);
/// `luaS_createlngstrobj` does NOT intern.  Phase A uses `state.intern_str()`
/// for both.  Phase B should add a `state.create_long_str()` path that skips
/// the intern table, matching C semantics.
///
/// PORT NOTE: The `_proto` parameter corresponds to C's `Proto *p` used only
/// for `luaC_objbarrier(L, p, ts)`.  The barrier is a no-op in Phase A–C
/// (macros.tsv: `luaC_objbarrier → state.gc().obj_barrier(p, o)` no-op).
fn load_string_n(
    s: &mut LoadState<'_>,
    _proto: &LuaProto,
) -> Result<Option<GcRef<LuaString>>, LuaError> {
    let raw_size = load_size(s)?;
    if raw_size == 0 {
        return Ok(None);
    }
    let size = raw_size - 1;

    // Read the raw bytes regardless of short/long distinction.
    let mut buf = vec![0u8; size];

    if size <= MAX_SHORT_LEN {
        load_block(s, &mut buf)?;
    } else {
        load_block(s, &mut buf)?;
    }

    // macros.tsv: luaS_newlstr → state.intern_str(&s[..n])
    // TODO(port): long strings should not be interned; see doc-comment above.
    let ts = s.state.intern_str(&buf)?;

    // macros.tsv: luaC_objbarrier → state.gc().obj_barrier(p, o)  no-op Phase A
    // (dropped — Phase A GC is Rc, no barrier needed)

    Ok(Some(ts))
}

/// Load a non-nullable string; error if the stream encodes a null string.
///
/// # C source
/// ```c
///
/// //   TString *st = loadStringN(S, p);
/// //   if (st == NULL)
/// //     error(S, "bad format for constant string");
/// //   return st;
/// // }
/// ```
fn load_string(
    s: &mut LoadState<'_>,
    proto: &LuaProto,
) -> Result<GcRef<LuaString>, LuaError> {
    match load_string_n(s, proto)? {
        Some(ts) => Ok(ts),
        None => Err(load_error(s, "bad format for constant string")),
    }
}

// ── Proto-field loaders ────────────────────────────────────────────────────

/// Load the bytecode instruction array into a prototype.
///
/// # C source
/// ```c
///
/// //   int n = loadInt(S);
/// //   f->code = luaM_newvectorchecked(S->L, n, Instruction);
/// //   f->sizecode = n;
/// //   loadVector(S, f->code, n);
/// // }
/// ```
///
/// PORT NOTE: `loadVector(S, f->code, n)` expands to
/// `loadBlock(S, f->code, n * sizeof(Instruction))` — `n` raw 4-byte words.
/// We read each `u32` in native-endian order, consistent with how
/// [`load_number`] and [`load_integer`] work.
///
/// PORT NOTE: `f->sizecode` is removed in Rust — `Vec::len()` covers it
/// (types.tsv: `Proto.sizecode → removed`).
fn load_code(s: &mut LoadState<'_>, f: &mut LuaProto) -> Result<(), LuaError> {
    let n = load_int(s)? as usize;
    // macros.tsv: luaM_newvectorchecked → vec_checked::<T>(n)?
    // PORT NOTE: Phase A uses Vec directly; overflow check omitted for brevity.
    // TODO(port): add overflow / OOM check matching luaM_newvectorchecked.
    let mut code = Vec::with_capacity(n);
    for _ in 0..n {
        let mut buf = [0u8; 4];
        load_block(s, &mut buf)?;
        // Instruction is a u32 newtype per types.tsv
        code.push(Instruction(u32::from_ne_bytes(buf)));
    }
    f.code = code;
    Ok(())
}

/// Load the constant pool into a prototype.
///
/// # C source
/// ```c
///
/// //   int i; int n = loadInt(S);
/// //   f->k = luaM_newvectorchecked(S->L, n, TValue);
/// //   f->sizek = n;
/// //   for (i = 0; i < n; i++) setnilvalue(&f->k[i]);
/// //   for (i = 0; i < n; i++) {
/// //     TValue *o = &f->k[i];
/// //     int t = loadByte(S);
/// //     switch (t) {
/// //       case LUA_VNIL:    setnilvalue(o); break;
/// //       case LUA_VFALSE:  setbfvalue(o); break;
/// //       case LUA_VTRUE:   setbtvalue(o); break;
/// //       case LUA_VNUMFLT: setfltvalue(o, loadNumber(S)); break;
/// //       case LUA_VNUMINT: setivalue(o, loadInteger(S)); break;
/// //       case LUA_VSHRSTR:
/// //       case LUA_VLNGSTR: setsvalue2n(S->L, o, loadString(S, f)); break;
/// //       default: lua_assert(0);
/// //     }
/// //   }
/// // }
/// ```
///
/// PORT NOTE: The initial `setnilvalue` loop initialises the vector for GC
/// safety in C.  In Rust, `Vec` is always in a valid state; we skip it.
fn load_constants(s: &mut LoadState<'_>, f: &mut LuaProto) -> Result<(), LuaError> {
    let n = load_int(s)? as usize;
    // TODO(port): add overflow / OOM check.
    let mut k = Vec::with_capacity(n);

    // Dropped — Rust Vec elements are never uninitialized.

    for _ in 0..n {
        let t = load_byte(s)?;
        let val = match t {
            // macros.tsv: setnilvalue → *o = LuaValue::Nil
            TAG_NIL => LuaValue::Nil,

            // macros.tsv: setbfvalue → *o = LuaValue::Bool(false)
            TAG_FALSE => LuaValue::Bool(false),

            // macros.tsv: setbtvalue → *o = LuaValue::Bool(true)
            TAG_TRUE => LuaValue::Bool(true),

            // macros.tsv: setfltvalue → *o = LuaValue::Float(x)
            TAG_FLOAT => LuaValue::Float(load_number(s)?),

            // macros.tsv: setivalue → *o = LuaValue::Int(x)
            TAG_INT => LuaValue::Int(load_integer(s)?),

            // macros.tsv: setsvalue2n → *dst = LuaValue::Str(s.clone())
            TAG_SHORT_STR | TAG_LONG_STR => {
                let ts = load_string(s, f)?;
                LuaValue::Str(ts)
            }

            // macros.tsv: lua_assert → debug_assert!
            _ => {
                debug_assert!(false, "unknown constant type tag {:#04x}", t);
                LuaValue::Nil
            }
        };
        k.push(val);
    }

    f.k = k;
    Ok(())
}

/// Load nested function prototypes into a prototype.
///
/// # C source
/// ```c
///
/// //   int i; int n = loadInt(S);
/// //   f->p = luaM_newvectorchecked(S->L, n, Proto *);
/// //   f->sizep = n;
/// //   for (i = 0; i < n; i++) f->p[i] = NULL;
/// //   for (i = 0; i < n; i++) {
/// //     f->p[i] = luaF_newproto(S->L);
/// //     luaC_objbarrier(S->L, f, f->p[i]);
/// //     loadFunction(S, f->p[i], f->source);
/// //   }
/// // }
/// ```
///
/// PORT NOTE: C creates the proto first (for GC anchor) then fills it.  In
/// Rust we create a default `LuaProto`, fill it, then wrap in `GcRef`.
/// `f->sizep` is removed per types.tsv (`Proto.sizep → removed`).
fn load_protos(s: &mut LoadState<'_>, f: &mut LuaProto) -> Result<(), LuaError> {
    let n = load_int(s)? as usize;
    // TODO(port): add overflow / OOM check.
    let mut protos = Vec::with_capacity(n);


    for _ in 0..n {
        let mut sub = LuaProto::placeholder();

        // macros.tsv: luaC_objbarrier → state.gc().obj_barrier(p, o)  no-op Phase A

        // Pass parent source as fallback.
        let parent_source = f.source.clone();
        load_function(s, &mut sub, parent_source)?;

        // Wrap in GcRef after loading.
        // PORT NOTE: In C f->p[i] is a Proto * held by the proto's GC roots.
        // In Rust Phase A it becomes Rc<LuaProto>.
        // TODO(D-1c-bridge): wraps fully-populated LuaProto value; state.new_proto produces a placeholder
        protos.push(GcRef::new(sub));
    }

    f.p = protos;
    Ok(())
}

/// Load upvalue descriptors into a prototype.
///
/// # C source
/// ```c
///
/// //   int i, n;
/// //   n = loadInt(S);
/// //   f->upvalues = luaM_newvectorchecked(S->L, n, Upvaldesc);
/// //   f->sizeupvalues = n;
/// //   for (i = 0; i < n; i++)
/// //     f->upvalues[i].name = NULL;  /* make array valid for GC */
/// //   for (i = 0; i < n; i++) {
/// //     f->upvalues[i].instack = loadByte(S);
/// //     f->upvalues[i].idx    = loadByte(S);
/// //     f->upvalues[i].kind   = loadByte(S);
/// //   }
/// // }
/// ```
///
/// PORT NOTE: The C comment says names must be filled first for GC safety.
/// In Rust we build `UpvalDesc` values with `name: None` and fill names later
/// in [`load_debug`].  This requires `UpvalDesc.name` to be
/// `Option<GcRef<LuaString>>` rather than `GcRef<LuaString>` as listed in
/// types.tsv.  Phase B should reconcile the types.tsv entry.
///
/// PORT NOTE: `f->sizeupvalues` is removed per types.tsv.
fn load_upvalues(s: &mut LoadState<'_>, f: &mut LuaProto) -> Result<(), LuaError> {
    let n = load_int(s)? as usize;
    // TODO(port): add overflow / OOM check.

    // In Rust: construct with name = None.

    let mut upvalues = Vec::with_capacity(n);
    for _ in 0..n {
        let instack_raw = load_byte(s)?;
        let idx = load_byte(s)?;
        let kind = load_byte(s)?;

        // types.tsv: Upvaldesc.instack → bool (stored as lu_byte in C)
        upvalues.push(UpvalDesc {
            name: None,           // filled by load_debug
            instack: instack_raw != 0,
            idx,
            kind,
        });
    }

    f.upvalues = upvalues;
    Ok(())
}

/// Load debug information into a prototype.
///
/// # C source
/// ```c
///
/// //   int i, n;
/// //   n = loadInt(S);
/// //   f->lineinfo = luaM_newvectorchecked(S->L, n, ls_byte);
/// //   f->sizelineinfo = n;
/// //   loadVector(S, f->lineinfo, n);
/// //   n = loadInt(S);
/// //   f->abslineinfo = luaM_newvectorchecked(S->L, n, AbsLineInfo);
/// //   f->sizeabslineinfo = n;
/// //   for (i = 0; i < n; i++) {
/// //     f->abslineinfo[i].pc   = loadInt(S);
/// //     f->abslineinfo[i].line = loadInt(S);
/// //   }
/// //   n = loadInt(S);
/// //   f->locvars = luaM_newvectorchecked(S->L, n, LocVar);
/// //   f->sizelocvars = n;
/// //   for (i = 0; i < n; i++) f->locvars[i].varname = NULL;
/// //   for (i = 0; i < n; i++) {
/// //     f->locvars[i].varname = loadStringN(S, f);
/// //     f->locvars[i].startpc = loadInt(S);
/// //     f->locvars[i].endpc   = loadInt(S);
/// //   }
/// //   n = loadInt(S);
/// //   if (n != 0)  /* does it have debug information? */
/// //     n = f->sizeupvalues;  /* must be this many */
/// //   for (i = 0; i < n; i++)
/// //     f->upvalues[i].name = loadStringN(S, f);
/// // }
/// ```
///
/// PORT NOTE: `ls_byte` (signed byte) maps to `i8` per types.tsv.
/// `loadVector(S, f->lineinfo, n)` reads `n * sizeof(ls_byte) = n` bytes.
/// We read them as `u8` then reinterpret as `i8` via cast.
///
/// PORT NOTE: Size companion fields (`sizelineinfo`, `sizeabslineinfo`,
/// `sizelocvars`) are all removed per types.tsv — `Vec::len()` covers them.
///
/// PORT NOTE: `LocalVar.varname` and `UpvalDesc.name` are both
/// `Option<GcRef<LuaString>>` here because `loadStringN` can return `None`.
/// See also the note on [`load_upvalues`].
fn load_debug(s: &mut LoadState<'_>, f: &mut LuaProto) -> Result<(), LuaError> {
    let n = load_int(s)? as usize;
    let mut lineinfo = vec![0i8; n];
    // Read as u8 slice then cast — safe because i8 and u8 have the same
    // in-memory representation and we're casting a byte from the binary stream.
    // SAFETY(port): this would need `unsafe` for the slice transmute in real
    // code; for Phase A we read byte-by-byte.
    // TODO(port): replace the loop with a single load_block into a u8 buffer
    //             followed by an i8 transmute in Phase B (or use bytemuck).
    for item in lineinfo.iter_mut() {
        *item = load_byte(s)? as i8;
    }
    f.lineinfo = lineinfo;

    let n = load_int(s)? as usize;
    let mut abslineinfo = Vec::with_capacity(n);
    for _ in 0..n {
        abslineinfo.push(AbsLineInfo {
            pc: load_int(s)?,
            line: load_int(s)?,
        });
    }
    f.abslineinfo = abslineinfo;

    let n = load_int(s)? as usize;

    let mut locvars = Vec::with_capacity(n);
    for _ in 0..n {
        let varname = load_string_n(s, f)?;
        let startpc = load_int(s)?;
        let endpc = load_int(s)?;
        let varname = match varname {
            Some(v) => v,
            None => s.state.new_string(b"")?,
        };
        locvars.push(LocalVar { varname, startpc, endpc });
    }
    f.locvars = locvars;

    // PORT NOTE: if n == 0 then there is no upvalue name info (stripped).
    let has_names = load_int(s)?;
    if has_names != 0 {
        let n_upvals = f.upvalues.len();
        for i in 0..n_upvals {
            let name = load_string_n(s, f)?;
            f.upvalues[i].name = name;
        }
    }

    Ok(())
}

// ── Function loader ────────────────────────────────────────────────────────

/// Load a complete function prototype from the stream.
///
/// # C source
/// ```c
///
/// //   f->source = loadStringN(S, f);
/// //   if (f->source == NULL) f->source = psource;
/// //   f->linedefined    = loadInt(S);
/// //   f->lastlinedefined = loadInt(S);
/// //   f->numparams   = loadByte(S);
/// //   f->is_vararg   = loadByte(S);
/// //   f->maxstacksize = loadByte(S);
/// //   loadCode(S, f);
/// //   loadConstants(S, f);
/// //   loadUpvalues(S, f);
/// //   loadProtos(S, f);
/// //   loadDebug(S, f);
/// // }
/// ```
///
/// PORT NOTE: `TString *psource` becomes `Option<GcRef<LuaString>>` because
/// the top-level call passes `NULL` (mapped to `None`).  `f->source` in `LuaProto`
/// is typed `GcRef<LuaString>` in types.tsv, but the undump path needs
/// `Option<GcRef<LuaString>>` to express "inherited from parent".  Phase B
/// should align types.tsv or add a dedicated `Option` wrapper there.
///
/// PORT NOTE: `f->is_vararg` is stored as `lu_byte` in C but `bool` in
/// types.tsv.  We read the raw byte and convert to `bool` via `!= 0`.
fn load_function(
    s: &mut LoadState<'_>,
    f: &mut LuaProto,
    psource: Option<GcRef<LuaString>>,
) -> Result<(), LuaError> {
    let source = load_string_n(s, f)?;
    f.source = source.or(psource);

    f.linedefined = load_int(s)?;
    f.lastlinedefined = load_int(s)?;
    f.numparams = load_byte(s)?;
    // types.tsv: Proto.is_vararg → bool (stored as lu_byte in C)
    f.is_vararg = load_byte(s)? != 0;
    f.maxstacksize = load_byte(s)?;
    load_code(s, f)?;
    load_constants(s, f)?;
    load_upvalues(s, f)?;
    load_protos(s, f)?;
    load_debug(s, f)?;

    Ok(())
}

// ── Header validation ──────────────────────────────────────────────────────

/// Verify that the next `expected.len()` bytes in the stream match `expected`.
///
/// # C source
/// ```c
///
/// //   char buff[sizeof(LUA_SIGNATURE) + sizeof(LUAC_DATA)];
/// //   size_t len = strlen(s);
/// //   loadVector(S, buff, len);
/// //   if (memcmp(s, buff, len) != 0)
/// //     error(S, msg);
/// // }
/// ```
///
/// PORT NOTE: `strlen` on a `const char *` becomes `.len()` on a `&[u8]`.
/// `memcmp` becomes slice equality.
fn check_literal(
    s: &mut LoadState<'_>,
    expected: &[u8],
    msg: &'static str,
) -> Result<(), LuaError> {
    let mut buf = vec![0u8; expected.len()];
    load_block(s, &mut buf)?;
    if buf != expected {
        return Err(load_error(s, msg));
    }
    Ok(())
}

/// Verify that the next byte in the stream equals `expected_size`.
///
/// # C source
/// ```c
///
/// //   if (loadByte(S) != size)
/// //     error(S, luaO_pushfstring(S->L, "%s size mismatch", tname));
/// // }
/// ```
///
/// PORT NOTE: `luaO_pushfstring` is used here as a message formatter, not as
/// a throw site.  We inline the message directly.  `tname` is always a Rust
/// type-name string literal (ASCII) from the call sites; using `&'static str`
/// is appropriate here (not Lua data).
fn fcheck_size(
    s: &mut LoadState<'_>,
    expected_size: usize,
    tname: &'static str,
) -> Result<(), LuaError> {
    let b = load_byte(s)? as usize;
    if b != expected_size {
        // PORT NOTE: We build the error message inline rather than using
        // luaO_pushfstring to avoid a stack push just for error formatting.
        // TODO(port): include `tname` in the error message once LuaError::syntax
        // supports composing byte-string and &str fragments.
        return Err(LuaError::syntax(format_args!(
            "{} size mismatch",
            tname
        )));
    }
    Ok(())
}

/// Validate the binary chunk header.
///
/// # C source
/// ```c
///
/// //   checkliteral(S, &LUA_SIGNATURE[1], "not a binary chunk");
/// //   if (loadByte(S) != LUAC_VERSION) error(S, "version mismatch");
/// //   if (loadByte(S) != LUAC_FORMAT)  error(S, "format mismatch");
/// //   checkliteral(S, LUAC_DATA, "corrupted chunk");
/// //   checksize(S, Instruction);
/// //   checksize(S, lua_Integer);
/// //   checksize(S, lua_Number);
/// //   if (loadInteger(S) != LUAC_INT) error(S, "integer format mismatch");
/// //   if (loadNumber(S)  != LUAC_NUM) error(S, "float format mismatch");
/// // }
/// ```
///
/// PORT NOTE: `checksize(S, T)` expands to `fchecksize(S, sizeof(T), #T)`.
/// We emit the three concrete sizes inline.
/// - `sizeof(Instruction)` = 4 (u32)
/// - `sizeof(lua_Integer)` = 8 (i64)
/// - `sizeof(lua_Number)` = 8 (f64)
///
/// PORT NOTE: The first byte of `LUA_SIGNATURE` (`\x1b`) is already consumed
/// by the caller before `checkHeader` is invoked, so we check only bytes 1..
/// of the signature (`"Lua"`).
fn check_header(s: &mut LoadState<'_>) -> Result<(), LuaError> {
    // Skip LUA_SIGNATURE[0] (\x1b) — already consumed by the caller.
    check_literal(s, &LUA_SIGNATURE[1..], "not a binary chunk")?;

    let ver = load_byte(s)?;
    if ver != LUAC_VERSION {
        return Err(load_error(s, "version mismatch"));
    }

    let fmt = load_byte(s)?;
    if fmt != LUAC_FORMAT {
        return Err(load_error(s, "format mismatch"));
    }

    check_literal(s, LUAC_DATA, "corrupted chunk")?;

    fcheck_size(s, 4, "Instruction")?;

    fcheck_size(s, 8, "lua_Integer")?;

    fcheck_size(s, 8, "lua_Number")?;

    let int_check = load_integer(s)?;
    if int_check != LUAC_INT {
        return Err(load_error(s, "integer format mismatch"));
    }

    let num_check = load_number(s)?;
    if num_check != LUAC_NUM {
        return Err(load_error(s, "float format mismatch"));
    }

    Ok(())
}

// ── Public entry point ─────────────────────────────────────────────────────

/// Load a precompiled Lua chunk and return the top-level Lua closure.
///
/// This is the Rust equivalent of `luaU_undump` — the single public function
/// exported by `lundump.c`.
///
/// # C source
/// ```c
///
/// //   LoadState S;
/// //   LClosure *cl;
/// //   if (*name == '@' || *name == '=')
/// //     S.name = name + 1;
/// //   else if (*name == LUA_SIGNATURE[0])
/// //     S.name = "binary string";
/// //   else
/// //     S.name = name;
/// //   S.L = L; S.Z = Z;
/// //   checkHeader(&S);
/// //   cl = luaF_newLclosure(L, loadByte(&S));
/// //   setclLvalue2s(L, L->top.p, cl);
/// //   luaD_inctop(L);
/// //   cl->p = luaF_newproto(L);
/// //   luaC_objbarrier(L, cl, cl->p);
/// //   loadFunction(&S, cl->p, NULL);
/// //   lua_assert(cl->nupvalues == cl->p->sizeupvalues);
/// //   luai_verifycode(L, cl->p);
/// //   return cl;
/// // }
/// ```
///
/// # Parameters
/// - `state` — the Lua thread state.
/// - `z` — input stream positioned at the start of the binary chunk
///   (the first byte `\x1b` of `LUA_SIGNATURE` must still be present).
/// - `name` — chunk name for error messages.  Stripped per Lua convention:
///   - `@…` → filename (strip `@`)
///   - `=…` → literal name (strip `=`)
///   - starts with `\x1b` → `"binary string"`
///   - otherwise used as-is.
///
/// PORT NOTE: The C function returns `LClosure *`.  In Rust we return
/// `GcRef<LuaLClosure>` (the Lua-closure variant of `LuaClosure`).  The
/// closure is also pushed onto the stack for GC anchoring, matching the C
/// behaviour (`setclLvalue2s + luaD_inctop`).  The caller is responsible for
/// popping it when done (consistent with C).
///
/// PORT NOTE: `luai_verifycode` is a no-op in the default build
/// (`#define luai_verifycode(L,f)  /* empty */`); dropped here.
///
/// PORT NOTE: `cl->nupvalues == cl->p->sizeupvalues` — in Rust the nupvalues
/// count is implicit in `cl.upvals.len()` and `f.upvalues.len()`; the
/// assertion becomes `debug_assert_eq!`.
pub(crate) fn undump(
    state: &mut LuaState,
    z: &mut ZIO,
    _name: &[u8],
) -> Result<GcRef<LuaLClosure>, LuaError> {
    let mut s = LoadState {
        state,
        z,
    };

    check_header(&mut s)?;

    // loadByte(&S) reads the number of upvalues for the top-level closure.
    let nupvalues = load_byte(&mut s)?;
    // PORT NOTE: `luaF_newLclosure` allocates a closure with `nupvalues`
    // upvalue slots.  In Rust Phase A we construct the struct directly; the
    // GcRef wrapping happens after the proto is loaded.
    // TODO(port): use the proper lfunc::new_lua_closure(state, nupvalues) API
    // once lfunc.rs is translated and the API is settled.
    let mut cl = LuaLClosure::placeholder();
    let mut upvals_vec = Vec::with_capacity(nupvalues as usize);
    for _ in 0..nupvalues as usize {
        upvals_vec.push(std::cell::Cell::new(s.state.new_upval_closed(LuaValue::Nil)));
    }
    cl.upvals = upvals_vec;

    // macros.tsv: setclLvalue2s → state.set_at(o, LuaValue::Function(LuaClosure::Lua(cl)))
    // macros.tsv: luaD_inctop → (state.push already increments; use state.push)
    // PORT NOTE: We push a placeholder Nil first; the real closure value is
    // set after the proto is loaded.  This mirrors the C "anchor for GC"
    // pattern.  In Phase A-C GC anchoring via the stack is not strictly
    // necessary (Rc keeps things alive) but we preserve the stack discipline
    // for behavioural parity.
    // TODO(port): once GcRef<LuaLClosure> is cloneable into LuaValue, push
    // the real value here instead of a placeholder.
    s.state.push(LuaValue::Nil); // placeholder; replaced below

    let mut proto = LuaProto::placeholder();

    // macros.tsv: luaC_objbarrier → state.gc().obj_barrier(p, o)  no-op Phase A

    load_function(&mut s, &mut proto, None)?;

    // Wrap the proto in a GcRef and attach it to the closure.
    // TODO(D-1c-bridge): wraps fully-populated LuaProto value; state.new_proto produces a placeholder
    let proto_ref = GcRef::new(proto);

    // macros.tsv: lua_assert → debug_assert!
    // nupvalues is the byte we read; sizeupvalues = proto_ref.upvalues.len()
    debug_assert_eq!(
        nupvalues as usize,
        proto_ref.upvalues.len(),
        "upvalue count mismatch between closure header and prototype"
    );

    // The macro is defined as `/* empty */` in the default build; dropped.

    // Attach the loaded proto to the closure.
    cl.proto = proto_ref;

    // Wrap the closure in GcRef.
    // TODO(D-1c-bridge): wraps fully-populated LuaLClosure value; state.new_lclosure makes Nil-filled upvals
    let cl_ref = GcRef::new(cl);

    // Replace the stack placeholder with the real closure value.
    // macros.tsv: setclLvalue2s → state.set_at(o, LuaValue::Function(LuaClosure::Lua(...)))
    // TODO(port): replace the placeholder at the correct stack slot.
    // For now the top slot holds Nil; Phase B must fix this once
    // GcRef<LuaLClosure> → LuaValue conversion is defined.
    // TODO(port): update the stack slot pushed above with the real cl_ref value.

    Ok(cl_ref)
}

// ──────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        src/lundump.c  (335 lines, 20 functions/items)
//                  src/lundump.h  (35 lines, merged)
//   target_crate:  lua-vm
//   confidence:    medium
//   todos:         15
//   port_notes:    39
//   unsafe_blocks: 0   (must be 0 outside explicit unsafe-budget crates)
//   notes:         Logic is faithful to the C.  The main open items for Phase B
//                  are: (1) import paths for GcRef/LuaProto/LuaClosure/etc.;
//                  (2) LuaError::syntax byte-string formatting for the chunk
//                  name in load_error; (3) long-string vs short-string intern
//                  distinction in load_string_n; (4) the stack placeholder in
//                  undump must be replaced with the real GcRef<LuaLClosure>
//                  value once LuaValue conversion is defined; (5) UpvalDesc.name
//                  and LocalVar.varname need Option<GcRef<LuaString>> in the
//                  proto type to match the two-pass load order here.
// ──────────────────────────────────────────────────────────────────────────
