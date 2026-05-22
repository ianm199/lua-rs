//! Pre-compiled Lua chunk serializer.
//!
//! Translates `reference/lua-5.4.7/src/ldump.c` (230 lines, 9 functions + 1 public entry point).
//! Writes a `LuaProto` to a byte sink in the standard Lua 5.4 bytecode format.

// C: #define ldump_c / #define LUA_CORE — build guards; not needed in Rust.

// TODO(port): Adjust import paths once crate boundaries stabilise in Phase B.
// The types below are expected to resolve as follows:
//   GcRef        — lua_types (or lua-gc Phase D)
//   LuaError     — lua_types
//   LuaProto     — lua-vm (this crate) or lua-types
//   LuaString    — lua-vm / lua-types
//   LuaValue     — lua_types
//   LuaState     — lua-vm (this crate)
use std::mem::size_of;
#[allow(unused_imports)] use crate::prelude::*;

use crate::state::LuaState;
use lua_types::{GcRef, LuaError, LuaString, LuaValue};
use lua_types::proto::LuaProto;

// ── Constants from lundump.h ─────────────────────────────────────────────────

// C: LUA_SIGNATURE "\x1bLua"  (lua.h; also in macros.tsv)
// dumpLiteral expands to dumpBlock(D, s, sizeof(s) - sizeof(char)).
// sizeof("\x1bLua") = 5; minus 1 = 4 bytes, no NUL terminator.
// b"\x1bLua" is &[u8; 4] in Rust — no NUL — so direct use is correct.
const LUA_SIGNATURE: &[u8] = b"\x1bLua";

// C: #define LUAC_VERSION (((LUA_VERSION_NUM / 100) * 16) + LUA_VERSION_NUM % 100)
// With LUA_VERSION_NUM = 504 (macros.tsv):
//   (504 / 100) * 16 + 504 % 100 = 5 * 16 + 4 = 84 = 0x54
const LUA_VERSION_NUM_DUMP: i32 = 504;
const LUAC_VERSION: u8 =
    ((LUA_VERSION_NUM_DUMP / 100) * 16 + LUA_VERSION_NUM_DUMP % 100) as u8;

// C: #define LUAC_FORMAT 0  /* this is the official format */
const LUAC_FORMAT: u8 = 0;

// C: #define LUAC_DATA "\x19\x93\r\n\x1a\n"
// sizeof("\x19\x93\r\n\x1a\n") = 7; minus 1 = 6 bytes written.
// b"\x19\x93\r\n\x1a\n" is &[u8; 6].
const LUAC_DATA: &[u8] = b"\x19\x93\r\n\x1a\n";

// C: #define LUAC_INT 0x5678
const LUAC_INT: i64 = 0x5678;

// C: #define LUAC_NUM cast_num(370.5)   cast_num → `as f64` (macros.tsv)
const LUAC_NUM: f64 = 370.5;

// C: sizeof(Instruction); Instruction is a u32 newtype (types.tsv).
const INSTRUCTION_SIZE: u8 = size_of::<u32>() as u8;

// C: sizeof(lua_Integer) = 8; lua_Integer → i64 (types.tsv).
const LUA_INTEGER_SIZE: u8 = size_of::<i64>() as u8;

// C: sizeof(lua_Number) = 8; lua_Number → f64 (types.tsv).
const LUA_NUMBER_SIZE: u8 = size_of::<f64>() as u8;

// ── DumpState ────────────────────────────────────────────────────────────────

/// Internal state threaded through every dump operation.
///
/// C: `typedef struct { lua_State *L; lua_Writer writer; void *data; int strip; int status; } DumpState;`
///
/// PORT NOTE: `lua_State *L` removed — it was used only for `lua_lock`/`lua_unlock`, which are
/// no-ops in the default Lua build and dropped here (macros.tsv). `void *data` is folded into
/// the writer closure. `int status` is replaced by `Result<(), LuaError>` propagated with `?`.
struct DumpState {
    /// Byte-sink callback. C original: `lua_Writer writer` + `void *data` (combined).
    /// lua_Writer type is TBD in types.tsv; for dump we use a bare byte-slice callback.
    writer: Box<dyn FnMut(&[u8]) -> Result<(), LuaError>>,
    /// When true, strip all debug information from the output.
    strip: bool,
}

impl DumpState {
    // ── Low-level write primitives ────────────────────────────────────────────

    /// Write raw bytes to the output stream.
    ///
    /// C: `static void dumpBlock(DumpState *D, const void *b, size_t size)`
    ///
    /// PORT NOTE: C accumulates errors in `D->status` and skips subsequent writes once
    /// non-zero; Rust returns `Result<(), LuaError>` and short-circuits via `?`.
    /// `lua_lock`/`lua_unlock` are no-ops in the default build and are dropped (macros.tsv).
    fn dump_block(&mut self, data: &[u8]) -> Result<(), LuaError> {
        // C: if (D->status == 0 && size > 0)
        if !data.is_empty() {
            // C: lua_unlock(D->L);
            // C: D->status = (*D->writer)(D->L, b, size, D->data);
            // C: lua_lock(D->L);
            (self.writer)(data)?;
        }
        Ok(())
    }

    /// Write one byte.
    ///
    /// C: `static void dumpByte(DumpState *D, int y)`
    /// C body: `lu_byte x = (lu_byte)y; dumpVar(D, x);`
    /// (`dumpVar(D,x)` expands to `dumpVector(D,&x,1)` expands to `dumpBlock(D,&x,sizeof(x))`)
    fn dump_byte(&mut self, y: u8) -> Result<(), LuaError> {
        // C: lu_byte x = (lu_byte)y; dumpVar(D, x)
        self.dump_block(&[y])
    }

    /// Write a `size_t` using Lua's variable-length encoding.
    ///
    /// C: `static void dumpSize(DumpState *D, size_t x)`
    ///
    /// Encoding (big-endian 7-bit groups, **last** byte marked with MSB = 1):
    /// - Each byte holds 7 payload bits.
    /// - Bytes are written most-significant group first.
    /// - The final byte (least-significant group) has its MSB set as an end marker.
    ///
    /// This differs from standard LEB128, which marks the *continuation* bytes rather than
    /// the terminating byte.
    ///
    /// C: `#define DIBS ((sizeof(size_t) * CHAR_BIT + 6) / 7)` — 10 on 64-bit.
    fn dump_size(&mut self, mut x: usize) -> Result<(), LuaError> {
        // C: lu_byte buff[DIBS]; int n = 0;
        // DIBS = (usize::BITS + 6) / 7; on 64-bit = (64+6)/7 = 10.
        const DIBS: usize = (usize::BITS as usize + 6) / 7;
        let mut buff = [0u8; DIBS];
        let mut n: usize = 0;

        // C: do { buff[DIBS - (++n)] = x & 0x7f; x >>= 7; } while (x != 0);
        loop {
            n += 1;
            buff[DIBS - n] = (x & 0x7f) as u8; // fill buffer in reverse order
            x >>= 7;
            if x == 0 {
                break;
            }
        }

        // C: buff[DIBS - 1] |= 0x80; /* mark last byte */
        // The byte at buff[DIBS-1] is the first byte placed (least-significant group).
        // Setting its MSB marks it as the terminal byte of the encoding.
        buff[DIBS - 1] |= 0x80;

        // C: dumpVector(D, buff + DIBS - n, n);
        self.dump_block(&buff[DIBS - n..])
    }

    /// Write an `int` as a variable-length size.
    ///
    /// C: `static void dumpInt(DumpState *D, int x)` → `dumpSize(D, x);`
    ///
    /// PORT NOTE: C implicitly casts `int` → `size_t`. All call sites pass non-negative values
    /// (line numbers, instruction counts, vector lengths); a debug assertion guards this.
    fn dump_int(&mut self, x: i32) -> Result<(), LuaError> {
        // C: dumpSize(D, x)
        debug_assert!(
            x >= 0,
            "dump_int: negative value {} cast to usize would wrap",
            x
        );
        self.dump_size(x as usize)
    }

    /// Write a `lua_Number` (f64) in the platform's native byte order.
    ///
    /// C: `static void dumpNumber(DumpState *D, lua_Number x)` → `dumpVar(D, x);`
    ///
    /// `dumpVar(D,x)` expands to `dumpBlock(D, &x, sizeof(lua_Number))` — 8 bytes, native order.
    /// `to_ne_bytes()` replicates native-endian serialisation. The bytecode header's `LUAC_NUM`
    /// sentinel (370.5) lets `lundump` detect byte-order mismatches at load time.
    fn dump_number(&mut self, x: f64) -> Result<(), LuaError> {
        // C: dumpVar(D, x) → dumpBlock(D, &x, sizeof(lua_Number))
        self.dump_block(&x.to_ne_bytes())
    }

    /// Write a `lua_Integer` (i64) in the platform's native byte order.
    ///
    /// C: `static void dumpInteger(DumpState *D, lua_Integer x)` → `dumpVar(D, x);`
    fn dump_integer(&mut self, x: i64) -> Result<(), LuaError> {
        // C: dumpVar(D, x) → dumpBlock(D, &x, sizeof(lua_Integer))
        self.dump_block(&x.to_ne_bytes())
    }

    // ── Mid-level serialisers ─────────────────────────────────────────────────

    /// Write an interned or long string, or a null sentinel (encoded size = 0).
    ///
    /// C: `static void dumpString(DumpState *D, const TString *s)`
    ///
    /// Encoding: `dumpSize(len + 1)` followed by `len` raw bytes; size 0 means null/absent.
    /// `tsslen(s)` → `s.len()` and `getstr(s)` → `s.as_bytes()` (macros.tsv).
    fn dump_string(&mut self, s: Option<&GcRef<LuaString>>) -> Result<(), LuaError> {
        match s {
            // C: if (s == NULL) dumpSize(D, 0);
            None => self.dump_size(0),

            Some(s) => {
                // C: size_t size = tsslen(s); const char *str = getstr(s);
                let bytes = s.as_bytes(); // tsslen → .len(); getstr → .as_bytes()
                // C: dumpSize(D, size + 1); dumpVector(D, str, size);
                self.dump_size(bytes.len() + 1)?;
                self.dump_block(bytes)
            }
        }
    }

    /// Write the bytecode instruction array.
    ///
    /// C: `static void dumpCode(DumpState *D, const Proto *f)`
    ///
    /// PORT NOTE: `f->sizecode` is covered by `Vec::len()` (types.tsv).
    fn dump_code(&mut self, proto: &LuaProto) -> Result<(), LuaError> {
        // C: dumpInt(D, f->sizecode);
        self.dump_int(proto.code.len() as i32)?;

        // C: dumpVector(D, f->code, f->sizecode)
        // dumpVector writes n * sizeof(Instruction) = n * 4 bytes in native byte order.
        for instr in &proto.code {
            // TODO(port): `Instruction` is a u32 newtype (types.tsv). Accessing the inner u32
            // via `.0` assumes a tuple-struct layout. If the Instruction API differs (e.g.,
            // exposes `.raw()` or `u32::from(*instr)`), adjust accordingly in Phase B.
            self.dump_block(&instr.0.to_ne_bytes())?;
        }
        Ok(())
    }

    /// Write the constant pool.
    ///
    /// C: `static void dumpConstants(DumpState *D, const Proto *f)`
    ///
    /// Each constant is written as: one tag byte (`ttypetag`), followed by the payload
    /// (float: 8 bytes; integer: 8 bytes; string: variable-length; nil/bool: nothing).
    ///
    /// PORT NOTE: `f->sizek` is covered by `Vec::len()` (types.tsv).
    fn dump_constants(&mut self, proto: &LuaProto) -> Result<(), LuaError> {
        // C: int n = f->sizek; dumpInt(D, n);
        let n = proto.k.len();
        self.dump_int(n as i32)?;

        for constant in &proto.k {
            // C: int tt = ttypetag(o); dumpByte(D, tt);
            // ttypetag(o) → o.full_type_tag() (macros.tsv)
            // Returns the C-side tag byte: bits 0-3 base type, bits 4-5 variant, bit 6 collectable.
            let tag = constant.full_type_tag();
            self.dump_byte(tag)?;

            // C: switch (tt) { case LUA_VNUMFLT / LUA_VNUMINT / LUA_VSHRSTR / LUA_VLNGSTR / default }
            match constant {
                LuaValue::Float(f) => {
                    // C: case LUA_VNUMFLT: dumpNumber(D, fltvalue(o));
                    // fltvalue(o) → o.as_float().expect("not float") or `if let` (macros.tsv)
                    self.dump_number(*f)?;
                }
                LuaValue::Int(i) => {
                    // C: case LUA_VNUMINT: dumpInteger(D, ivalue(o));
                    self.dump_integer(*i)?;
                }
                LuaValue::Str(s) => {
                    // C: case LUA_VSHRSTR: case LUA_VLNGSTR: dumpString(D, tsvalue(o));
                    // tsvalue(o) → o.as_string().expect("not string") (macros.tsv)
                    self.dump_string(Some(s))?;
                }
                LuaValue::Nil | LuaValue::Bool(_) => {
                    // C: default: lua_assert(tt == LUA_VNIL || tt == LUA_VFALSE || tt == LUA_VTRUE)
                    // Only the tag byte is written; nil and booleans carry no additional payload.
                    // lua_assert → debug_assert! (macros.tsv)
                    debug_assert!(
                        matches!(constant, LuaValue::Nil | LuaValue::Bool(_)),
                        "dump_constants: default branch reached for unexpected variant"
                    );
                }
                _ => {
                    // TODO(port): LuaValue variant not valid as a constant-pool entry.
                    // In C the default branch asserts nil/false/true only. Any other variant
                    // here indicates a malformed proto; flag for Phase B investigation.
                    debug_assert!(false, "dump_constants: unexpected LuaValue variant in constant pool");
                }
            }
        }
        Ok(())
    }

    /// Write nested function prototypes (sub-functions defined inside `proto`).
    ///
    /// C: `static void dumpProtos(DumpState *D, const Proto *f)`
    ///
    /// PORT NOTE: `f->sizep` is covered by `Vec::len()` (types.tsv).
    /// The parent's source string is passed down so that children with identical source
    /// origins can omit the redundant source name (see `dump_function`).
    fn dump_protos(&mut self, proto: &LuaProto) -> Result<(), LuaError> {
        // C: int n = f->sizep; dumpInt(D, n);
        let n = proto.p.len();
        self.dump_int(n as i32)?;

        for sub in &proto.p {
            // C: dumpFunction(D, f->p[i], f->source);
            // sub: &GcRef<LuaProto>; deref coercion (&GcRef<LuaProto> → &LuaProto) expected
            // when GcRef<T>: Deref<Target=T> (true for Rc<T> in Phase A).
            self.dump_function(sub, proto.source.as_ref())?;
        }
        Ok(())
    }

    /// Write upvalue descriptors (instack / idx / kind for each upvalue slot).
    ///
    /// C: `static void dumpUpvalues(DumpState *D, const Proto *f)`
    ///
    /// PORT NOTE: `f->sizeupvalues` is covered by `Vec::len()` (types.tsv).
    /// `Upvaldesc.instack` is `bool` in Rust (types.tsv); cast to `u8` for the wire format.
    fn dump_upvalues(&mut self, proto: &LuaProto) -> Result<(), LuaError> {
        // C: int i, n = f->sizeupvalues; dumpInt(D, n);
        let n = proto.upvalues.len();
        self.dump_int(n as i32)?;

        for upval in &proto.upvalues {
            // C: dumpByte(D, f->upvalues[i].instack);
            // PORT NOTE: instack is bool in Rust (types.tsv); cast to u8: true→1, false→0.
            self.dump_byte(upval.instack as u8)?;
            // C: dumpByte(D, f->upvalues[i].idx);
            self.dump_byte(upval.idx)?;
            // C: dumpByte(D, f->upvalues[i].kind);
            self.dump_byte(upval.kind)?;
        }
        Ok(())
    }

    /// Write debug information: per-instruction line deltas, absolute line records,
    /// local-variable lifetimes, and upvalue names.
    ///
    /// All counts are written as zero when `self.strip` is true.
    ///
    /// C: `static void dumpDebug(DumpState *D, const Proto *f)`
    ///
    /// PORT NOTE: all `f->size*` fields are covered by `Vec::len()` (types.tsv).
    fn dump_debug(&mut self, proto: &LuaProto) -> Result<(), LuaError> {
        // C: n = (D->strip) ? 0 : f->sizelineinfo; dumpInt(D, n);
        let n_lineinfo = if self.strip { 0 } else { proto.lineinfo.len() };
        self.dump_int(n_lineinfo as i32)?;

        // C: dumpVector(D, f->lineinfo, n)
        // lineinfo is Vec<i8> (ls_byte per types.tsv). C writes them as raw bytes (sizeof(i8)=1).
        // Cast each i8 to u8 (same bit pattern) before writing.
        // PERF(port): iterating one byte at a time vs. bulk write — profile in Phase B.
        // (A bulk write would require bytemuck::cast_slice or similar to avoid unsafe.)
        let lineinfo_bytes: Vec<u8> = proto.lineinfo[..n_lineinfo]
            .iter()
            .map(|&b| b as u8)
            .collect();
        self.dump_block(&lineinfo_bytes)?;

        // C: n = (D->strip) ? 0 : f->sizeabslineinfo; dumpInt(D, n);
        let n_absline = if self.strip { 0 } else { proto.abslineinfo.len() };
        self.dump_int(n_absline as i32)?;

        for abs in proto.abslineinfo.iter().take(n_absline) {
            // C: dumpInt(D, f->abslineinfo[i].pc); dumpInt(D, f->abslineinfo[i].line);
            // AbsLineInfo.pc and .line are i32 (types.tsv); non-negative in valid bytecode.
            self.dump_int(abs.pc)?;
            self.dump_int(abs.line)?;
        }

        // C: n = (D->strip) ? 0 : f->sizelocvars; dumpInt(D, n);
        let n_locvars = if self.strip { 0 } else { proto.locvars.len() };
        self.dump_int(n_locvars as i32)?;

        for locvar in proto.locvars.iter().take(n_locvars) {
            // C: dumpString(D, f->locvars[i].varname);
            // LocVar.varname is GcRef<LuaString> (types.tsv).
            self.dump_string(Some(&locvar.varname))?;
            // C: dumpInt(D, f->locvars[i].startpc);
            self.dump_int(locvar.startpc)?;
            // C: dumpInt(D, f->locvars[i].endpc);
            self.dump_int(locvar.endpc)?;
        }

        // C: n = (D->strip) ? 0 : f->sizeupvalues; dumpInt(D, n);
        // (Re-uses upvalues.len() for the name-writing pass — separate from dumpUpvalues
        //  which wrote structural descriptors; here we write debug names.)
        let n_upval_names = if self.strip { 0 } else { proto.upvalues.len() };
        self.dump_int(n_upval_names as i32)?;

        for upval in proto.upvalues.iter().take(n_upval_names) {
            // C: dumpString(D, f->upvalues[i].name);
            // PORT NOTE: UpvalDesc.name is GcRef<LuaString> per types.tsv (non-optional).
            // TODO(port): In C, `TString *name` can be NULL when an upvalue is unnamed (e.g.,
            // in bytecode compiled without debug info). Verify whether UpvalDesc.name should be
            // `Option<GcRef<LuaString>>` in the Rust model; if so, change call to pass the Option
            // directly instead of wrapping in Some.
            self.dump_string(upval.name.as_ref())?;
        }
        Ok(())
    }

    /// Write a complete function prototype: source name, header bytes, code, constants,
    /// upvalue descriptors, nested prototypes, and debug information.
    ///
    /// `psource` is the parent function's source string. When `f->source == psource` (pointer
    /// equality — Lua interns short strings so identical source names share an object), the
    /// source is written as null (size 0) to avoid duplication. The top-level call passes
    /// `None` to force writing the source.
    ///
    /// C: `static void dumpFunction(DumpState *D, const Proto *f, TString *psource)`
    ///
    /// PORT NOTE: `f->source == psource` is a C pointer comparison exploiting string interning.
    /// In Rust we use `GcRef::ptr_eq` (equivalent to `Rc::ptr_eq` in Phase A) for identity.
    /// `is_vararg` is `bool` in Rust (types.tsv); cast to `u8` for the wire format.
    fn dump_function(
        &mut self,
        proto: &LuaProto,
        psource: Option<&GcRef<LuaString>>,
    ) -> Result<(), LuaError> {
        // C: if (D->strip || f->source == psource) dumpString(D, NULL); else dumpString(D, f->source);
        // Pointer-equality check: same interned string object means same source file.
        let same_source = match (psource, proto.source.as_ref()) {
            (Some(ps), Some(src)) => GcRef::ptr_eq(src, ps),
            _ => false,
        };

        if self.strip || same_source {
            self.dump_string(None)?;
        } else {
            self.dump_string(proto.source.as_ref())?;
        }

        // C: dumpInt(D, f->linedefined);
        self.dump_int(proto.linedefined)?;
        // C: dumpInt(D, f->lastlinedefined);
        self.dump_int(proto.lastlinedefined)?;
        // C: dumpByte(D, f->numparams);
        self.dump_byte(proto.numparams)?;
        // C: dumpByte(D, f->is_vararg);
        // PORT NOTE: is_vararg is bool in Rust (types.tsv); true → 1u8, false → 0u8.
        self.dump_byte(proto.is_vararg as u8)?;
        // C: dumpByte(D, f->maxstacksize);
        self.dump_byte(proto.maxstacksize)?;

        self.dump_code(proto)?;
        self.dump_constants(proto)?;
        self.dump_upvalues(proto)?;
        self.dump_protos(proto)?;
        self.dump_debug(proto)?;
        Ok(())
    }

    /// Write the binary chunk header.
    ///
    /// The header allows `lundump` (and external tools) to verify the bytecode format,
    /// platform word sizes, and byte order before attempting to load the chunk.
    ///
    /// C: `static void dumpHeader(DumpState *D)`
    fn dump_header(&mut self) -> Result<(), LuaError> {
        // C: dumpLiteral(D, LUA_SIGNATURE)
        // dumpLiteral(D,s) = dumpBlock(D, s, sizeof(s) - sizeof(char))
        // b"\x1bLua" is &[u8; 4] (no NUL terminator in Rust byte literals), matching the
        // C expansion of sizeof("\x1bLua")-1 = 4 bytes.
        self.dump_block(LUA_SIGNATURE)?;

        // C: dumpByte(D, LUAC_VERSION)
        self.dump_byte(LUAC_VERSION)?;

        // C: dumpByte(D, LUAC_FORMAT)
        self.dump_byte(LUAC_FORMAT)?;

        // C: dumpLiteral(D, LUAC_DATA)
        // b"\x19\x93\r\n\x1a\n" is &[u8; 6], matching sizeof(LUAC_DATA)-1 = 6 bytes.
        self.dump_block(LUAC_DATA)?;

        // C: dumpByte(D, sizeof(Instruction))
        self.dump_byte(INSTRUCTION_SIZE)?;

        // C: dumpByte(D, sizeof(lua_Integer))
        self.dump_byte(LUA_INTEGER_SIZE)?;

        // C: dumpByte(D, sizeof(lua_Number))
        self.dump_byte(LUA_NUMBER_SIZE)?;

        // C: dumpInteger(D, LUAC_INT)   — 0x5678 as i64, native byte order
        self.dump_integer(LUAC_INT)?;

        // C: dumpNumber(D, LUAC_NUM)    — 370.5 as f64, native byte order
        self.dump_number(LUAC_NUM)?;

        Ok(())
    }
}

// ── Public entry point ───────────────────────────────────────────────────────

/// Serialize a compiled Lua function prototype as a precompiled bytecode chunk.
///
/// The `writer` callback receives successive slices of the serialised bytes and returns
/// `Err(LuaError)` to abort. `strip` omits debug info (line numbers, local names, etc.)
/// from the output.
///
/// C: `int luaU_dump(lua_State *L, const Proto *f, lua_Writer w, void *data, int strip)`
///
/// PORT NOTE: `lua_Writer w` (fn pointer) + `void *data` (userdata) are collapsed into a
/// single `impl FnMut(&[u8]) -> Result<(), LuaError>` closure — the Rust idiom for the
/// callback + context pair. `_state` is retained in the signature for API parity but unused
/// in the body: the C code needed it only for `lua_lock`/`lua_unlock`, which are no-ops per
/// macros.tsv. Return type changes from `int` (0 = ok, non-zero = writer error) to
/// `Result<(), LuaError>`.
pub(crate) fn dump(
    _state: &mut LuaState,
    proto: &GcRef<LuaProto>,
    writer: impl FnMut(&[u8]) -> Result<(), LuaError> + 'static,
    strip: bool,
) -> Result<(), LuaError> {
    // C: DumpState D; D.L = L; D.writer = w; D.data = data; D.strip = strip; D.status = 0;
    let mut d = DumpState {
        writer: Box::new(writer),
        strip,
    };

    // C: dumpHeader(&D);
    d.dump_header()?;

    // C: dumpByte(&D, f->sizeupvalues);
    // PORT NOTE: f->sizeupvalues is covered by Vec::len(). Bounded by MAXUPVAL = 255
    // (macros.tsv), so truncation via `as u8` is safe for well-formed prototypes.
    d.dump_byte(proto.upvalues.len() as u8)?;

    // C: dumpFunction(&D, f, NULL);
    // psource = None forces the top-level function to always write its source name.
    // Deref coercion: &GcRef<LuaProto> → &LuaProto (via Deref<Target=LuaProto> on GcRef/Rc).
    d.dump_function(proto, None)?;

    // C: return D.status;
    Ok(())
}

// ────────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        src/ldump.c  (230 lines, 10 functions)
//   target_crate:  lua-vm
//   confidence:    medium
//   todos:         4
//   port_notes:    12
//   unsafe_blocks: 0
//   notes:         Types/imports need Phase B wiring; logic should be faithful.
//                  Key uncertainties: (1) Instruction newtype inner-field access (.0 vs
//                  method); (2) UpvalDesc.name optionality; (3) GcRef::ptr_eq method
//                  existence. Lineinfo bulk-write is done via collect()+dump_block to
//                  avoid unsafe transmute of &[i8] → &[u8]; revisit with bytemuck in
//                  Phase B for performance. Native-endian serialisation via to_ne_bytes()
//                  matches C's raw-memory dumpVector behaviour.
// ────────────────────────────────────────────────────────────────────────────
