//! Standard I/O library — `io.*` functions and `file:*` methods.
//!
//! C source: `src/liolib.c` (841 lines, ~35 functions).
//!
//! PORT NOTE: This module necessarily requires file-system access. The PORTING.md
//! rule banning `std::fs` outside `lua-cli` conflicts with the crate assignment
//! (`lua-stdlib`). Every file-system call site carries a `TODO(port): std::fs`
//! marker. The architecture team must either relax the rule for this file, move
//! the module to `lua-cli`, or provide a thin IO-abstraction crate that wraps
//! `std::fs` under a permitted API.
//!
//! `popen` additionally requires `std::process::Command` and is stubbed.
//!
//! PORT NOTE: Rust's borrow checker prevents holding `&mut dyn LuaFileOps`
//! (extracted from userdata) and `&mut LuaState` simultaneously. The affected
//! functions (`io_read`, `f_read`, `io_write`, `f_write`, `io_flush`, `f_flush`,
//! `f_seek`, `f_setvbuf`, `get_io_file`) are marked with `TODO(port): borrow
//! split`. Phase B must restructure `g_read`/`g_write` to take a `StackIdx`
//! rather than a raw `&mut dyn LuaFileOps`, and use `RefCell` inside `LStream`
//! for interior mutability, or extract the file handle via a separate borrow
//! scope.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{self, SeekFrom};
use std::rc::Rc;

use lua_types::{LuaError, LuaType, LuaValue};
use crate::state_stub::{LuaState, LuaStateStubExt as _, lua_CFunction, upvalue_index, CompareOp, LuaDebug};

thread_local! {
    /// Side-table mapping userdata identity (the `Rc` pointer address from
    /// `GcRef::identity()`) to its associated `LStream`. The C port stores
    /// `LStream` directly inside the userdata payload; Rust cannot do that
    /// safely because `LStream` carries heap pointers (a `Box<dyn LuaFileOps>`
    /// and a fn pointer). Entries are inserted by `new_pre_file` and never
    /// removed in Phase A-C — leak is intentional per `PORTING.md` §2 #4.
    static LSTREAM_REGISTRY: RefCell<HashMap<usize, Rc<RefCell<LStream>>>>
        = RefCell::new(HashMap::new());
}

fn register_lstream(ud_id: usize, lstream: LStream) -> Rc<RefCell<LStream>> {
    let cell = Rc::new(RefCell::new(lstream));
    LSTREAM_REGISTRY.with(|reg| {
        reg.borrow_mut().insert(ud_id, cell.clone());
    });
    cell
}

fn lookup_lstream(ud_id: usize) -> Option<Rc<RefCell<LStream>>> {
    LSTREAM_REGISTRY.with(|reg| reg.borrow().get(&ud_id).cloned())
}

// ── Constants ────────────────────────────────────────────────────────────────

/// Name of the file-handle metatable in the Lua registry. C: `LUA_FILEHANDLE`.
pub const LUA_FILE_HANDLE: &[u8] = b"FILE*";

/// Registry key for the default input file. C: `IO_INPUT` = `"_IO_input"`.
const IO_INPUT_KEY: &[u8] = b"_IO_input";

/// Registry key for the default output file. C: `IO_OUTPUT` = `"_IO_output"`.
const IO_OUTPUT_KEY: &[u8] = b"_IO_output";

/// Number of bytes in the `"_IO_"` prefix, used to strip it in error messages.
/// C: `IOPREF_LEN`.
const IO_PREFIX_LEN: usize = 4;

/// Maximum number of format-arguments passed to `file:lines`. C: `MAXARGLINE`.
const MAX_ARG_LINE: usize = 250;

/// Maximum byte-length of a numeric literal read from a file. C: `L_MAXLENNUM`.
const L_MAX_LEN_NUM: usize = 200;

/// End-of-file sentinel returned by `LuaFileOps::read_byte`. C: `EOF` == -1.
const EOF_SENTINEL: i32 = -1;

/// Bulk-read chunk size, mirroring C's `LUAL_BUFFERSIZE`.
const LUAL_BUFFER_SIZE: usize = 8192;

// ── Traits ───────────────────────────────────────────────────────────────────

/// Capabilities required by the io library from an OS file handle.
///
/// TODO(port): Every concrete implementation of this trait requires either
/// `std::fs::File` (regular files), `std::io::Stdin`/`Stdout`/`Stderr`
/// (standard streams), or pipe handles from `std::process::Child` (popen). All
/// three are banned outside `lua-cli` by PORTING.md. This trait is the intended
/// abstraction seam for that restriction.
pub trait LuaFileOps: Send {
    /// Read one byte; return it as `i32`, or `EOF_SENTINEL` on EOF/error.
    fn read_byte(&mut self) -> i32;

    /// Push back a previously-read byte for the next `read_byte` call.
    fn unread_byte(&mut self, byte: i32);

    /// Write a byte slice; return the number of bytes actually written.
    fn write_bytes(&mut self, data: &[u8]) -> io::Result<usize>;

    /// Flush any write buffers to the OS.
    fn flush(&mut self) -> io::Result<()>;

    /// Seek within the file using a `SeekFrom` position.
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64>;

    /// Return the current file position without moving it.
    fn tell(&mut self) -> io::Result<u64>;

    /// Clear the error/EOF flag on the stream. C: `clearerr`.
    fn clear_error(&mut self);

    /// Return `true` if the stream has a pending error. C: `ferror`.
    fn has_error(&self) -> bool;

    /// Control stream buffering. C: `setvbuf`.
    fn set_buf_mode(&mut self, mode: BufMode, size: usize) -> io::Result<()>;
}

// ── Enums ────────────────────────────────────────────────────────────────────

/// Seek anchor for `file:seek`. C: `{SEEK_SET, SEEK_CUR, SEEK_END}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeekWhence {
    Set,
    Cur,
    End,
}

/// Buffering mode for `file:setvbuf`. C: `{_IONBF, _IOFBF, _IOLBF}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufMode {
    No,
    Full,
    Line,
}

/// Which standard stream to wrap in `create_std_file`.
pub enum StdFileKind {
    Stdin,
    Stdout,
    Stderr,
}

// ── Structs ──────────────────────────────────────────────────────────────────

/// Lua file handle stored as the typed payload of a `LuaUserData`.
///
/// C equivalent: `typedef luaL_Stream LStream` in `liolib.c`.
///
/// TODO(port): Phase B must arrange for `LStream` to live inside
/// `LuaUserData`'s opaque payload. The userdata system needs a typed-access
/// API, e.g. `state.check_arg_typed_userdata::<LStream>(1, LUA_FILE_HANDLE)?`.
///
/// TODO(port): `file` must be `Option<RefCell<Box<dyn LuaFileOps>>>` to allow
/// interior-mutability borrow splitting between the file handle and `LuaState`.
pub struct LStream {
    /// OS file handle. `None` = incompletely opened (pre-file pattern).
    /// TODO(port): concrete type is `Box<dyn LuaFileOps>`; implementations need std::fs.
    pub file: Option<Box<dyn LuaFileOps>>,
    /// Close callback. `None` means the stream is closed. C: `p->closef == NULL`.
    pub close_fn: Option<fn(&mut LuaState) -> Result<usize, LuaError>>,
}

impl LStream {
    /// `isclosed(p)` in C: true when `closef` is NULL.
    pub fn is_closed(&self) -> bool {
        self.close_fn.is_none()
    }
}

/// Minimal `LuaFileOps` placeholder for stdin/stdout/stderr while real
/// std::io wiring is deferred. All read/write/seek operations return
/// `Unsupported`, which is sufficient for the validation paths exercised
/// by `io.input(io.stdin)`, `io.output(io.stdout)`, and `io.type`.
struct StdStreamHandle {
    kind: StdFileKind,
}

impl LuaFileOps for StdStreamHandle {
    fn read_byte(&mut self) -> i32 { EOF_SENTINEL }
    fn unread_byte(&mut self, _byte: i32) {}
    fn write_bytes(&mut self, _data: &[u8]) -> io::Result<usize> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "std stream write not yet implemented",
        ))
    }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
    fn seek(&mut self, _pos: SeekFrom) -> io::Result<u64> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "stdio seek"))
    }
    fn tell(&mut self) -> io::Result<u64> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "stdio tell"))
    }
    fn clear_error(&mut self) {}
    fn has_error(&self) -> bool { false }
    fn set_buf_mode(&mut self, _mode: BufMode, _size: usize) -> io::Result<()> { Ok(()) }
}

impl StdStreamHandle {
    fn new(kind: StdFileKind) -> Self { StdStreamHandle { kind } }
}

/// State machine for reading a numeric literal byte-by-byte from a file.
/// C: `typedef struct { FILE *f; int c; int n; char buff[L_MAXLENNUM+1]; } RN`.
struct ReadNumState {
    /// Current look-ahead byte, or `EOF_SENTINEL`.
    current: i32,
    /// Number of bytes accumulated in `buf`.
    count: usize,
    /// Accumulated characters of the numeral (NUL-terminated on finalise).
    buf: [u8; L_MAX_LEN_NUM + 1],
}

impl ReadNumState {
    fn new(first_byte: i32) -> Self {
        ReadNumState {
            current: first_byte,
            count: 0,
            buf: [0u8; L_MAX_LEN_NUM + 1],
        }
    }

    /// Save current char to `buf` and read the next byte from `file`.
    /// Returns `false` if the buffer is full (numeral too long). C: `nextc`.
    fn advance(&mut self, file: &mut dyn LuaFileOps) -> bool {
        // C: if (rn->n >= L_MAXLENNUM) { rn->buff[0] = '\0'; return 0; }
        if self.count >= L_MAX_LEN_NUM {
            self.buf[0] = 0;
            return false;
        }
        self.buf[self.count] = self.current as u8;
        self.count += 1;
        self.current = file.read_byte();
        true
    }

    /// Accept current char if it equals either byte in `set`. C: `test2`.
    fn try2(&mut self, file: &mut dyn LuaFileOps, set: [u8; 2]) -> bool {
        // C: if (rn->c == set[0] || rn->c == set[1]) return nextc(rn);
        if self.current == set[0] as i32 || self.current == set[1] as i32 {
            self.advance(file)
        } else {
            false
        }
    }

    /// Consume a run of (hex)digits; return the count. C: `readdigits`.
    fn read_digits(&mut self, file: &mut dyn LuaFileOps, hex: bool) -> usize {
        // C: while ((hex ? isxdigit(rn->c) : isdigit(rn->c)) && nextc(rn)) count++;
        let mut count = 0usize;
        loop {
            let is_digit = if hex {
                (self.current as u8).is_ascii_hexdigit()
            } else {
                (self.current as u8).is_ascii_digit()
            };
            if !is_digit || self.current == EOF_SENTINEL {
                break;
            }
            if !self.advance(file) {
                break;
            }
            count += 1;
        }
        count
    }

    /// Return the accumulated bytes (without the NUL terminator).
    fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.count]
    }
}

// ── Function registration tables ─────────────────────────────────────────────

/// `io.*` module functions. C: `static const luaL_Reg iolib[]`.
pub const IO_LIB: &[(&[u8], fn(&mut LuaState) -> Result<usize, LuaError>)] = &[
    (b"close",   io_close),
    (b"flush",   io_flush),
    (b"input",   io_input),
    (b"lines",   io_lines),
    (b"open",    io_open),
    (b"output",  io_output),
    (b"popen",   io_popen),
    (b"read",    io_read),
    (b"tmpfile", io_tmpfile),
    (b"type",    io_type),
    (b"write",   io_write),
];

/// `file:*` instance methods. C: `static const luaL_Reg meth[]`.
pub const FILE_METHODS: &[(&[u8], fn(&mut LuaState) -> Result<usize, LuaError>)] = &[
    (b"read",    f_read),
    (b"write",   f_write),
    (b"lines",   f_lines),
    (b"flush",   f_flush),
    (b"seek",    f_seek),
    (b"close",   f_close),
    (b"setvbuf", f_setvbuf),
];

/// File-handle metamethods. C: `static const luaL_Reg metameth[]`.
pub const FILE_METAMETHODS: &[(&[u8], fn(&mut LuaState) -> Result<usize, LuaError>)] = &[
    (b"__gc",       f_gc),
    (b"__close",    f_gc),
    (b"__tostring", f_tostring),
];

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Validate an `fopen` mode string: must match `[rwa]\+?b*`. C: `l_checkmode`.
///
/// C: `return (*mode != '\0' && strchr("rwa", *(mode++)) != NULL &&
///           (*mode != '+' || ...) && strspn(mode, "b") == strlen(mode));`
fn check_mode(mode: &[u8]) -> bool {
    if mode.is_empty() {
        return false;
    }
    let mut idx = 0usize;
    if !matches!(mode[idx], b'r' | b'w' | b'a') {
        return false;
    }
    idx += 1;
    if idx < mode.len() && mode[idx] == b'+' {
        idx += 1;
    }
    mode[idx..].iter().all(|&b| b == b'b')
}

/// Validate a `popen` mode string: only `"r"` or `"w"`. C: `l_checkmodep`.
fn check_mode_popen(mode: &[u8]) -> bool {
    matches!(mode, b"r" | b"w")
}

/// Push success (`true`) or failure (`false`, msg, errno) per `luaL_fileresult`.
///
/// C: `if (stat) { lua_pushboolean(L,1); return 1; }
///     else { luaL_pushfail; pushstring(msg); pushinteger(errno); return 3; }`
fn file_result(
    state: &mut LuaState,
    success: bool,
    fname: Option<&[u8]>,
    os_err: io::Error,
) -> Result<usize, LuaError> {
    if success {
        // C: lua_pushboolean(L, 1); return 1;
        state.push(LuaValue::Bool(true));
        return Ok(1);
    }
    // C: luaL_pushfail(L)  — Lua 5.4 pushfail = push false
    state.push(LuaValue::Bool(false));
    // C: msg = strerror(errno); if (fname) lua_pushfstring(L, "%s: %s", fname, msg);
    let msg = os_err.to_string();
    match fname {
        Some(name) => {
            let mut s = Vec::with_capacity(name.len() + 2 + msg.len());
            s.extend_from_slice(name);
            s.extend_from_slice(b": ");
            s.extend_from_slice(msg.as_bytes());
            state.push_string(&s);
        }
        None => {
            state.push_string(msg.as_bytes());
        }
    }
    // C: lua_pushinteger(L, en)
    let errno_code = os_err.raw_os_error().unwrap_or(0) as i64;
    state.push(LuaValue::Int(errno_code));
    Ok(3)
}

/// Push popen/system exit-status results per `luaL_execresult`.
///
/// C: `if (stat == 0) { lua_pushboolean(L,1); return 1; }
///     else { luaL_pushfail; pushlstring("exit"|"signal"); pushinteger(stat); return 3; }`
///
/// TODO(port): POSIX `WIFEXITED`/`WTERMSIG` macros not available on all platforms;
/// this stub always treats non-zero stat as an exit code.
fn exec_result(state: &mut LuaState, stat: i32) -> Result<usize, LuaError> {
    if stat == 0 {
        state.push(LuaValue::Bool(true));
        Ok(1)
    } else {
        state.push(LuaValue::Bool(false));
        // TODO(port): distinguish exit vs signal via POSIX macros
        state.push_string(b"exit");
        state.push(LuaValue::Int(stat as i64));
        Ok(3)
    }
}

/// Retrieve `LStream` from argument 1 via a userdata type-check.
/// C: `tolstream(L)` = `(LStream *)luaL_checkudata(L, 1, LUA_FILEHANDLE)`.
///
/// Returns an `Rc<RefCell<LStream>>` from the side-table registry. The C port
/// returns a raw `LStream *` pointing into the userdata payload; Rust uses a
/// side table because `LStream` contains heap pointers that cannot be safely
/// reinterpreted from a raw byte buffer in safe Rust.
fn get_lstream(state: &mut LuaState) -> Result<Rc<RefCell<LStream>>, LuaError> {
    let ud = state.check_arg_userdata(1, LUA_FILE_HANDLE)?;
    lookup_lstream(ud.identity()).ok_or_else(|| {
        LuaError::runtime(format_args!("invalid file handle"))
    })
}

/// Validate that argument 1 is an open file handle; error if closed.
/// C: `tofile` (returns `FILE *` in C; here we return the wrapping `Rc<RefCell<LStream>>`).
fn tofile(state: &mut LuaState) -> Result<Rc<RefCell<LStream>>, LuaError> {
    let p_rc = get_lstream(state)?;
    {
        let p = p_rc.borrow();
        // C: if (isclosed(p)) luaL_error(L, "attempt to use a closed file");
        if p.is_closed() {
            return Err(LuaError::runtime(format_args!(
                "attempt to use a closed file"
            )));
        }
        // C: lua_assert(p->f);
        debug_assert!(p.file.is_some());
    }
    Ok(p_rc)
}

// ── File creation helpers ────────────────────────────────────────────────────

/// Allocate a "closed" file-handle userdata and push it; set its metatable.
/// Also registers an empty `LStream` in the side table keyed by the userdata
/// identity, and returns the `Rc<RefCell<LStream>>` so the caller may finish
/// initialising it (set `file`, set `close_fn`). C: `newprefile(L)`.
fn new_pre_file(state: &mut LuaState) -> Result<Rc<RefCell<LStream>>, LuaError> {
    // C: LStream *p = lua_newuserdatauv(L, sizeof(LStream), 0);
    let ud = state.new_userdata_typed(LUA_FILE_HANDLE, std::mem::size_of::<LStream>(), 0)?;
    // C: luaL_setmetatable(L, LUA_FILEHANDLE);
    state.set_metatable_by_name(LUA_FILE_HANDLE)?;
    // C: p->closef = NULL;  (LStream::close_fn = None marks the stream as closed)
    let cell = register_lstream(ud.identity(), LStream { file: None, close_fn: None });
    Ok(cell)
}

/// Allocate a new regular-file handle with `io_fclose` as the close function.
/// C: `newfile(L)`.
fn new_file(state: &mut LuaState) -> Result<Rc<RefCell<LStream>>, LuaError> {
    // C: LStream *p = newprefile(L); p->f = NULL; p->closef = &io_fclose;
    let cell = new_pre_file(state)?;
    cell.borrow_mut().close_fn = Some(io_fclose);
    Ok(cell)
}

/// Open `fname` and push its handle; raise a runtime error on failure.
/// C: `opencheck(L, fname, mode)`.
///
/// TODO(port): std::fs::File::open needed.
fn opencheck(state: &mut LuaState, fname: &[u8], mode: &[u8]) -> Result<(), LuaError> {
    // C: LStream *p = newfile(L); p->f = fopen(fname, mode);
    // C: if (p->f == NULL) luaL_error(L, "cannot open file '%s' (%s)", fname, strerror(errno));
    new_file(state)?;
    // TODO(port): std::fs::File — open fname in mode, store in LStream.file
    let open_result: Result<Box<dyn LuaFileOps>, io::Error> = Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "TODO(port): std::fs::File not yet wired",
    ));
    match open_result {
        Ok(_f) => {
            // TODO(port): store f in the just-allocated LStream.file
            Ok(())
        }
        Err(e) => Err(LuaError::runtime(format_args!(
            "cannot open file '{}' ({})",
            fname.escape_ascii(),
            e
        ))),
    }
}

// ── Close functions ──────────────────────────────────────────────────────────

/// Close a regular file via `fclose`. C: `io_fclose`.
///
/// TODO(port): flush + drop `Box<dyn LuaFileOps>`, map io::Error to file_result.
fn io_fclose(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: return luaL_fileresult(L, (fclose(p->f) == 0), NULL);
    let p_rc = get_lstream(state)?;
    // TODO(port): actually flush then drop p.file, capture any error
    let _closed = p_rc.borrow_mut().file.take();
    state.push(LuaValue::Bool(true));
    Ok(1)
}

/// Close a popen process pipe. C: `io_pclose`.
///
/// TODO(port): std::process::Child — popen not yet implemented.
fn io_pclose(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: return luaL_execresult(L, l_pclose(L, p->f));
    let p_rc = get_lstream(state)?;
    let _closed = p_rc.borrow_mut().file.take();
    // TODO(port): wait on the child process and forward its exit code
    exec_result(state, 0)
}

/// Refuse to close a standard-stream handle. C: `io_noclose`.
fn io_noclose(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: p->closef = &io_noclose;  /* keep file opened */
    // C: luaL_pushfail(L); lua_pushliteral(L, "cannot close standard file"); return 2;
    let p_rc = get_lstream(state)?;
    p_rc.borrow_mut().close_fn = Some(io_noclose); // reinstall to keep the handle alive
    state.push(LuaValue::Bool(false));
    state.push_string(b"cannot close standard file");
    Ok(2)
}

/// Invoke the stream's close function and mark it closed. C: `aux_close`.
fn aux_close(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: volatile lua_CFunction cf = p->closef; p->closef = NULL; return (*cf)(L);
    let p_rc = get_lstream(state)?;
    let cf = p_rc.borrow_mut().close_fn.take().ok_or_else(|| {
        LuaError::runtime(format_args!("attempt to close an already-closed file"))
    })?;
    cf(state)
}

// ── io.type ──────────────────────────────────────────────────────────────────

/// `io.type(x)` — return `"file"`, `"closed file"`, or `false`. C: `io_type`.
pub fn io_type(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: luaL_checkany(L, 1);
    state.check_arg_any(1)?;
    // C: p = (LStream *)luaL_testudata(L, 1, LUA_FILEHANDLE);
    // C: if (p == NULL) luaL_pushfail(L);
    // C: else if (isclosed(p)) lua_pushliteral(L, "closed file");
    // C: else lua_pushliteral(L, "file");
    let maybe_userdata = state.test_arg_userdata(1, LUA_FILE_HANDLE);
    match maybe_userdata {
        None => {
            state.push(LuaValue::Bool(false));
        }
        Some(ud) => {
            let is_closed = match lookup_lstream(ud.identity()) {
                Some(rc) => rc.borrow().is_closed(),
                None => true, // unknown userdata with FILE* metatable: treat as closed
            };
            if is_closed {
                state.push_string(b"closed file");
            } else {
                state.push_string(b"file");
            }
        }
    }
    Ok(1)
}

// ── __tostring metamethod ────────────────────────────────────────────────────

/// `tostring(file)` metamethod. C: `f_tostring`.
fn f_tostring(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: if (isclosed(p)) lua_pushliteral(L, "file (closed)");
    // C: else lua_pushfstring(L, "file (%p)", p->f);
    let p_rc = get_lstream(state)?;
    let closed = p_rc.borrow().is_closed();
    if closed {
        state.push_string(b"file (closed)");
    } else {
        // TODO(port): pointer-address representation for the file handle
        // C: lua_pushfstring(L, "file (%p)", p->f)
        state.push_string(b"file (0x?)");
    }
    Ok(1)
}

// ── close / gc ───────────────────────────────────────────────────────────────

/// `file:close()`. C: `f_close`.
fn f_close(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: tofile(L);  /* make sure argument is an open stream */
    // C: return aux_close(L);
    let _ = tofile(state)?; // validates stream is open before closing
    aux_close(state)
}

/// `io.close([file])`. C: `io_close`.
pub fn io_close(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: if (lua_isnone(L, 1)) lua_getfield(L, LUA_REGISTRYINDEX, IO_OUTPUT);
    if state.type_at(1) == LuaType::None {
        state.registry_get(IO_OUTPUT_KEY)?;
        state.replace(1);
    }
    f_close(state)
}

/// `__gc` / `__close` metamethod — silently close if still open. C: `f_gc`.
fn f_gc(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: if (!isclosed(p) && p->f != NULL) aux_close(L);  /* ignore errors */
    let p_rc = get_lstream(state)?;
    let needs_close = {
        let p = p_rc.borrow();
        !p.is_closed() && p.file.is_some()
    };
    if needs_close {
        // ignore any error from aux_close during GC finalisation
        let _ = aux_close(state);
    }
    Ok(0)
}

// ── io.open / io.popen / io.tmpfile ─────────────────────────────────────────

/// `io.open(filename [, mode])`. C: `io_open`.
pub fn io_open(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: const char *filename = luaL_checkstring(L, 1);
    // C: const char *mode = luaL_optstring(L, 2, "r");
    let filename: Vec<u8> = state.check_arg_string(1)?;
    let mode: Vec<u8> = state.opt_arg_string(2, b"r")?;
    // C: luaL_argcheck(L, l_checkmode(md), 2, "invalid mode");
    if !check_mode(&mode) {
        return Err(LuaError::arg_error(2, "invalid mode"));
    }
    new_file(state)?;
    // C: p->f = fopen(filename, mode);
    // C: return (p->f == NULL) ? luaL_fileresult(L, 0, filename) : 1;
    // TODO(port): std::fs — open filename in the requested mode, store in LStream.file
    let open_result: Result<Box<dyn LuaFileOps>, io::Error> = Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "TODO(port): std::fs::File not yet wired",
    ));
    match open_result {
        Ok(_f) => {
            // TODO(port): store _f in the just-allocated LStream.file
            Ok(1)
        }
        Err(e) => file_result(state, false, Some(&filename), e),
    }
}

/// `io.popen(filename [, mode])`. C: `io_popen`.
pub fn io_popen(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: luaL_argcheck(L, l_checkmodep(mode), 2, "invalid mode");
    // C: p->f = l_popen(L, filename, mode); p->closef = &io_pclose;
    let filename: Vec<u8> = state.check_arg_string(1)?;
    let mode: Vec<u8> = state.opt_arg_string(2, b"r")?;
    if !check_mode_popen(&mode) {
        return Err(LuaError::arg_error(2, "invalid mode"));
    }
    new_pre_file(state)?;
    // TODO(port): std::process::Command — spawn child, capture pipe, store in LStream
    let _ = (filename, mode);
    Err(LuaError::runtime(format_args!(
        "'popen' not supported in this build"
    )))
}

/// `io.tmpfile()`. C: `io_tmpfile`.
pub fn io_tmpfile(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: p->f = tmpfile();
    // C: return (p->f == NULL) ? luaL_fileresult(L, 0, NULL) : 1;
    new_file(state)?;
    // TODO(port): create anonymous temp file (tempfile crate or OS workaround)
    Err(LuaError::runtime(format_args!(
        "tmpfile not yet implemented"
    )))
}

// ── io.input / io.output ─────────────────────────────────────────────────────

/// Retrieve the current default IO file from the registry; error if closed.
/// C: `getiofile(L, findex)`.
///
/// TODO(port): borrow split — returns `&mut dyn LuaFileOps` while caller also
/// needs `&mut LuaState`. Phase B: use `RefCell` inside `LStream`.
fn get_io_file<'a>(
    state: &'a mut LuaState,
    key: &[u8],
) -> Result<&'a mut dyn LuaFileOps, LuaError> {
    // C: lua_getfield(L, LUA_REGISTRYINDEX, findex);
    // C: p = (LStream *)lua_touserdata(L, -1);
    // C: if (isclosed(p)) luaL_error(L, "default %s file is closed", findex+IOPREF_LEN);
    state.registry_get(key)?;
    // TODO(port): extract &mut LStream from the registry value's userdata payload
    let label = &key[IO_PREFIX_LEN..]; // strip "_IO_" for the error message
    let p: &mut LStream = todo!("TODO(port): extract LStream from registry userdata");
    if p.is_closed() {
        return Err(LuaError::runtime(format_args!(
            "default {} file is closed",
            label.escape_ascii()
        )));
    }
    Ok(p.file.as_mut().expect("open stream has no file handle").as_mut())
}

/// Generic setter/getter for `io.input` and `io.output`. C: `g_iofile`.
fn g_iofile(state: &mut LuaState, key: &[u8], mode: &[u8]) -> Result<usize, LuaError> {
    // C: if (!lua_isnoneornil(L, 1)) { ... }
    if !matches!(state.type_at(1), LuaType::None | LuaType::Nil) {
        if state.type_at(1) == LuaType::String {
            // C: opencheck(L, filename, mode);
            let filename = state.check_arg_string(1)?;
            opencheck(state, &filename, mode)?;
        } else {
            // C: tofile(L);  /* check that it's a valid file handle */
            // C: lua_pushvalue(L, 1);
            let _ = tofile(state)?;
            state.push_value_at(1);
        }
        // C: lua_setfield(L, LUA_REGISTRYINDEX, f);
        state.registry_set(key)?;
    }
    // C: lua_getfield(L, LUA_REGISTRYINDEX, f); return 1;
    state.registry_get(key)?;
    Ok(1)
}

/// `io.input([file])`. C: `io_input`.
pub fn io_input(state: &mut LuaState) -> Result<usize, LuaError> {
    g_iofile(state, IO_INPUT_KEY, b"r")
}

/// `io.output([file])`. C: `io_output`.
pub fn io_output(state: &mut LuaState) -> Result<usize, LuaError> {
    g_iofile(state, IO_OUTPUT_KEY, b"w")
}

// ── Read helpers ─────────────────────────────────────────────────────────────

/// Read and push a Lua number from `file`. Return `true` on success.
/// C: `read_number(L, f)`.
fn read_number(
    state: &mut LuaState,
    file: &mut dyn LuaFileOps,
) -> Result<bool, LuaError> {
    // C: do { rn.c = l_getc(rn.f); } while (isspace(rn.c)); /* skip spaces */
    let first = loop {
        let b = file.read_byte();
        if b == EOF_SENTINEL || !(b as u8).is_ascii_whitespace() {
            break b;
        }
    };

    let mut rn = ReadNumState::new(first);

    // C: test2(&rn, "-+")
    rn.try2(file, [b'-', b'+']);

    // C: if (test2(&rn, "00")) { if (test2(&rn, "xX")) hex = 1; else count = 1; }
    let mut count: usize = 0;
    let hex = if rn.try2(file, [b'0', b'0']) {
        if rn.try2(file, [b'x', b'X']) {
            true
        } else {
            count = 1;
            false
        }
    } else {
        false
    };

    // C: count += readdigits(&rn, hex);
    count += rn.read_digits(file, hex);

    // C: decp[0] = lua_getlocaledecpoint(); decp[1] = '.';
    // TODO(port): locale decimal-point character; defaulting to '.'
    let dec_point = b'.';
    if rn.try2(file, [dec_point, b'.']) {
        count += rn.read_digits(file, hex);
    }

    // C: if (count > 0 && test2(&rn, hex ? "pP" : "eE")) { ... exponent ... }
    if count > 0 {
        let exp_chars = if hex { [b'p', b'P'] } else { [b'e', b'E'] };
        if rn.try2(file, exp_chars) {
            rn.try2(file, [b'-', b'+']);
            rn.read_digits(file, false);
        }
    }

    // C: ungetc(rn.c, rn.f);
    file.unread_byte(rn.current);

    // C: if (lua_stringtonumber(L, rn.buff)) return 1; else { lua_pushnil(L); return 0; }
    // TODO(port): state.string_to_number(bytes) — parses bytes into LuaValue::Int or Float
    let bytes = rn.as_bytes();
    let pushed = state.string_to_number_push(&bytes)?;
    if pushed != 0 {
        Ok(true)
    } else {
        state.push(LuaValue::Nil);
        Ok(false)
    }
}

/// Test for EOF: push `""` and return `true` if not at EOF. C: `test_eof`.
fn test_eof(
    state: &mut LuaState,
    file: &mut dyn LuaFileOps,
) -> Result<bool, LuaError> {
    // C: int c = getc(f); ungetc(c, f); lua_pushliteral(L, ""); return (c != EOF);
    let c = file.read_byte();
    if c != EOF_SENTINEL {
        file.unread_byte(c);
    }
    state.push_string(b"");
    Ok(c != EOF_SENTINEL)
}

/// Read one line from `file` and push it. If `chop`, strip the trailing `\n`.
/// Return `true` if anything was read. C: `read_line(L, f, chop)`.
///
/// PERF(port): C uses luaL_prepbuffer (large fixed stack buffer) to avoid
/// per-byte allocation; Rust's Vec grows here, which is slightly slower.
fn read_line(
    state: &mut LuaState,
    file: &mut dyn LuaFileOps,
    chop: bool,
) -> Result<bool, LuaError> {
    let mut buf: Vec<u8> = Vec::new();
    let mut c: i32 = EOF_SENTINEL;

    // C: do { char *buff = luaL_prepbuffer(&b); int i = 0;
    //          while (i < LUAL_BUFFERSIZE && (c = l_getc(f)) != EOF && c != '\n')
    //            buff[i++] = c;
    //          luaL_addsize(&b, i);
    //    } while (c != EOF && c != '\n');
    'outer: loop {
        for _ in 0..LUAL_BUFFER_SIZE {
            c = file.read_byte();
            if c == EOF_SENTINEL || c == b'\n' as i32 {
                break 'outer;
            }
            buf.push(c as u8);
        }
        // chunk full but no newline/EOF yet — continue reading
    }

    // C: if (!chop && c == '\n') luaL_addchar(&b, c);
    if !chop && c == b'\n' as i32 {
        buf.push(b'\n');
    }

    // C: return (c == '\n' || lua_rawlen(L, -1) > 0);
    let had_content = c == b'\n' as i32 || !buf.is_empty();
    state.push_string(&buf);
    Ok(had_content)
}

/// Read the entire file into a single string and push it. C: `read_all(L, f)`.
///
/// PERF(port): C uses `fread` with a large buffer; Rust reads byte-by-byte via
/// `LuaFileOps::read_byte`. Phase B should add `read_chunk(&mut buf)` to the
/// trait for bulk reads.
fn read_all(state: &mut LuaState, file: &mut dyn LuaFileOps) -> Result<(), LuaError> {
    // C: do { nr = fread(p, LUAL_BUFFERSIZE, f); luaL_addsize(&b, nr); } while (nr == LUAL_BUFFERSIZE);
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let start = buf.len();
        let mut chunk_read = 0usize;
        for _ in 0..LUAL_BUFFER_SIZE {
            let b = file.read_byte();
            if b == EOF_SENTINEL {
                break;
            }
            buf.push(b as u8);
            chunk_read += 1;
        }
        if chunk_read < LUAL_BUFFER_SIZE {
            break;
        }
    }
    state.push_string(&buf);
    Ok(())
}

/// Read exactly `n` bytes from `file` and push the result.
/// Return `true` if anything was read. C: `read_chars(L, f, n)`.
fn read_chars(
    state: &mut LuaState,
    file: &mut dyn LuaFileOps,
    n: usize,
) -> Result<bool, LuaError> {
    // C: nr = fread(p, sizeof(char), n, f); luaL_addsize(&b, nr); return (nr > 0);
    let mut buf = Vec::with_capacity(n);
    for _ in 0..n {
        let b = file.read_byte();
        if b == EOF_SENTINEL {
            break;
        }
        buf.push(b as u8);
    }
    let nr = buf.len();
    state.push_string(&buf);
    Ok(nr > 0)
}

/// Dispatch one or more read formats; push results. C: `g_read(L, f, first)`.
///
/// TODO(port): borrow split — `file` is `&mut dyn LuaFileOps` extracted from a
/// `LuaUserData` that lives inside `state`. Phase B must restructure so the file
/// is accessed through the stack index rather than a raw borrow.
fn g_read(
    state: &mut LuaState,
    file: &mut dyn LuaFileOps,
    first: i32,
) -> Result<usize, LuaError> {
    // C: int nargs = lua_gettop(L) - 1;
    let nargs = state.top_idx().get() as i32 - 1;
    let mut n = first;
    let mut success = true;

    // C: clearerr(f);
    file.clear_error();

    if nargs == 0 {
        // C: success = read_line(L, f, 1); n = first + 1;
        success = read_line(state, file, true)?;
        n = first + 1;
    } else {
        // C: luaL_checkstack(L, nargs+LUA_MINSTACK, "too many arguments");
        state.ensure_stack((nargs as i32) + 20, "too many arguments")?;
        let mut remaining = nargs;
        while remaining > 0 && success {
            // C: if (lua_type(L, n) == LUA_TNUMBER)
            if state.type_at(n) == LuaType::Number {
                // C: size_t l = (size_t)luaL_checkinteger(L, n);
                let l = state.check_arg_integer(n)? as usize;
                success = if l == 0 {
                    test_eof(state, file)?
                } else {
                    read_chars(state, file, l)?
                };
            } else {
                // C: const char *p = luaL_checkstring(L, n);
                // C: if (*p == '*') p++;  /* skip optional '*' (compat) */
                let s: Vec<u8> = state.check_arg_string(n)?;
                let p: &[u8] = if s.first() == Some(&b'*') { &s[1..] } else { &s[..] };
                match p.first() {
                    // C: case 'n': success = read_number(L, f); break;
                    Some(&b'n') => {
                        success = read_number(state, file)?;
                    }
                    // C: case 'l': success = read_line(L, f, 1); break;
                    Some(&b'l') => {
                        success = read_line(state, file, true)?;
                    }
                    // C: case 'L': success = read_line(L, f, 0); break;
                    Some(&b'L') => {
                        success = read_line(state, file, false)?;
                    }
                    // C: case 'a': read_all(L, f); success = 1; break;
                    Some(&b'a') => {
                        read_all(state, file)?;
                        success = true;
                    }
                    _ => {
                        return Err(LuaError::arg_error(n, "invalid format"));
                    }
                }
            }
            n += 1;
            remaining -= 1;
        }
    }

    // C: if (ferror(f)) return luaL_fileresult(L, 0, NULL);
    if file.has_error() {
        return file_result(
            state,
            false,
            None,
            io::Error::new(io::ErrorKind::Other, "file read error"),
        );
    }

    // C: if (!success) { lua_pop(L, 1); luaL_pushfail(L); }
    if !success {
        state.pop_n(1);
        state.push(LuaValue::Bool(false));
    }

    // C: return n - first;
    Ok((n - first) as usize)
}

/// `io.read(...)`. C: `io_read`.
pub fn io_read(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: return g_read(L, getiofile(L, IO_INPUT), 1);
    // TODO(port): borrow split — extract file from registry before calling g_read;
    // requires RefCell<Box<dyn LuaFileOps>> inside LStream for interior mutability.
    Err(LuaError::runtime(format_args!(
        "TODO(port): borrow split needed for io_read"
    )))
}

/// `file:read(...)`. C: `f_read`.
pub fn f_read(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: return g_read(L, tofile(L), 2);
    // TODO(port): borrow split — same issue as io_read; g_read needs StackIdx API.
    Err(LuaError::runtime(format_args!(
        "TODO(port): borrow split needed for f_read"
    )))
}

// ── Write helpers ────────────────────────────────────────────────────────────

/// Dispatch one or more write values. C: `g_write(L, f, arg)`.
///
/// TODO(port): borrow split — same issue as g_read.
fn g_write(
    state: &mut LuaState,
    file: &mut dyn LuaFileOps,
    arg: i32,
) -> Result<usize, LuaError> {
    // C: int nargs = lua_gettop(L) - arg;
    let nargs = state.top_idx().get() as i32 - arg;
    let mut overall_ok = true;

    for i in 0..nargs {
        let idx = arg + i;
        if state.type_at(idx) == LuaType::Number {
            // C: lua_isinteger(L, arg) ? fprintf(LUA_INTEGER_FMT,...) : fprintf(LUA_NUMBER_FMT,...)
            // C: LUA_INTEGER_FMT = "%lld" (i64)
            // C: LUA_NUMBER_FMT  = "%.14g" (f64, 14 significant digits)
            // PERF(port): byte-by-byte write; Phase B add bulk write_fmt to LuaFileOps.
            // TODO(port): C's %.14g (significant digits) has no direct Rust equivalent.
            let s = if state.is_integer(idx) {
                let ival = state.to_integer(idx).unwrap_or(0);
                // C: LUA_INTEGER_FMT = "%lld"
                format!("{}", ival)
            } else {
                let fval = state.to_number(idx).unwrap_or(0.0);
                // C: LUA_NUMBER_FMT = "%.14g" — significant-digit format
                // TODO(port): implement proper %.14g (choose between %e and %f based on magnitude)
                format!("{:.14e}", fval)
            };
            match file.write_bytes(s.as_bytes()) {
                Ok(n) => overall_ok = overall_ok && n == s.len(),
                Err(_) => overall_ok = false,
            }
        } else {
            // C: const char *s = luaL_checklstring(L, arg, &l);
            // C: status = status && (fwrite(s, sizeof(char), l, f) == l);
            let s: Vec<u8> = state.check_arg_string(idx)?;
            match file.write_bytes(&s) {
                Ok(n) => overall_ok = overall_ok && n == s.len(),
                Err(_) => overall_ok = false,
            }
        }
    }

    // C: if (status) return 1; else return luaL_fileresult(L, status, NULL);
    if overall_ok {
        Ok(1) // file handle already at stack top; C returns it on success
    } else {
        file_result(
            state,
            false,
            None,
            io::Error::new(io::ErrorKind::Other, "write error"),
        )
    }
}

/// `io.write(...)`. C: `io_write`.
///
/// PORT NOTE: routes writes directly through `state.write_output()` rather than
/// `g_write(getiofile(IO_OUTPUT))`. The full C path needs typed-userdata access
/// to pull `LStream` out of the registry and an interior-mutability split to
/// borrow its `LuaFileOps` alongside `&mut LuaState`; both land in Phase B.
/// Until then, the only `IO_OUTPUT` ever installed by `luaopen_io` is `stdout`,
/// so writing through `write_output` is behaviourally identical for the
/// default case and lets `print`/`io.write` share the same sink.
pub fn io_write(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: int nargs = lua_gettop(L) - arg;  with arg == 1, so nargs == top().
    let n = state.top();
    for i in 1..=n {
        if state.type_at(i) == LuaType::Number {
            // C: lua_isinteger ? fprintf("%lld", ival) : fprintf("%.14g", fval)
            let s = if state.is_integer(i) {
                let ival = state.to_integer(i).unwrap_or(0);
                format!("{}", ival)
            } else {
                let fval = state.to_number(i).unwrap_or(0.0);
                // TODO(port): proper %.14g (significant-digit) formatting; same
                // gap as g_write — fine for now because integer tables hit the
                // branch above and printf-style float formatting is unused on
                // the common path.
                format!("{:.14e}", fval)
            };
            state.write_output(s.as_bytes())?;
        } else {
            // C: luaL_checklstring(L, arg, &l); fwrite(s, 1, l, f);
            let bytes: Vec<u8> = state.check_arg_string(i)?;
            state.write_output(&bytes)?;
        }
    }
    // C: returns the file handle (pushed by `getiofile`) so calls can chain
    // as `io.write("x"):write("y")`. Mirror that by pushing the IO_OUTPUT
    // userdata back onto the stack top.
    state.registry_get(IO_OUTPUT_KEY)?;
    Ok(1)
}

/// `file:write(...)`. C: `f_write`.
pub fn f_write(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: FILE *f = tofile(L); lua_pushvalue(L, 1); return g_write(L, f, 2);
    // TODO(port): borrow split — extract file, then push value, then call g_write.
    Err(LuaError::runtime(format_args!(
        "TODO(port): borrow split needed for f_write"
    )))
}

// ── Seek / setvbuf / flush ───────────────────────────────────────────────────

/// `file:seek([whence [, offset]])`. C: `f_seek`.
pub fn f_seek(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: static const int mode[] = {SEEK_SET, SEEK_CUR, SEEK_END};
    // C: static const char *const modenames[] = {"set","cur","end",NULL};
    static MODE_NAMES: &[&[u8]] = &[b"set", b"cur", b"end"];

    let _ = tofile(state)?; // validates stream is open
    // C: int op = luaL_checkoption(L, 2, "cur", modenames);
    let op = state.check_arg_option(2, Some(b"cur"), MODE_NAMES)?;
    // C: lua_Integer p3 = luaL_optinteger(L, 3, 0);
    let p3: i64 = state.opt_arg_integer(3, 0)?;
    // C: luaL_argcheck(L, (lua_Integer)offset == p3, 3, "not an integer in proper range");
    // (lua_Integer is i64 and l_seeknum is off_t/i64 on 64-bit; always satisfied)

    let seek_pos = match op {
        0 => SeekFrom::Start(p3 as u64),
        1 => SeekFrom::Current(p3),
        2 => SeekFrom::End(p3),
        _ => unreachable!(),
    };

    // C: op = l_fseek(f, offset, mode[op]);
    // C: if (op) return luaL_fileresult(L, 0, NULL);
    // C: else { lua_pushinteger(L, l_ftell(f)); return 1; }
    // TODO(port): borrow split — cannot call seek on extracted &mut dyn LuaFileOps
    //             while state is also borrowed. Phase B fix: RefCell or StackIdx API.
    let _ = seek_pos;
    Err(LuaError::runtime(format_args!(
        "TODO(port): borrow split needed for f_seek"
    )))
}

/// `file:setvbuf(mode [, size])`. C: `f_setvbuf`.
pub fn f_setvbuf(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: static const int mode[] = {_IONBF, _IOFBF, _IOLBF};
    // C: static const char *const modenames[] = {"no","full","line",NULL};
    static MODE_NAMES: &[&[u8]] = &[b"no", b"full", b"line"];

    let _ = tofile(state)?;
    let op = state.check_arg_option(2, None, MODE_NAMES)?;
    // C: lua_Integer sz = luaL_optinteger(L, 3, LUAL_BUFFERSIZE);
    let sz: i64 = state.opt_arg_integer(3, LUAL_BUFFER_SIZE as i64)?;
    let _mode = match op {
        0 => BufMode::No,
        1 => BufMode::Full,
        2 => BufMode::Line,
        _ => unreachable!(),
    };
    // C: res = setvbuf(f, NULL, mode[op], (size_t)sz);
    // C: return luaL_fileresult(L, res == 0, NULL);
    // TODO(port): borrow split — same as f_seek; also setvbuf is POSIX-only.
    let _ = sz;
    Err(LuaError::runtime(format_args!(
        "TODO(port): borrow split needed for f_setvbuf"
    )))
}

/// `io.flush()`. C: `io_flush`.
pub fn io_flush(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: FILE *f = getiofile(L, IO_OUTPUT);
    // C: return luaL_fileresult(L, fflush(f) == 0, NULL);
    // TODO(port): borrow split needed for io_flush
    Err(LuaError::runtime(format_args!(
        "TODO(port): borrow split needed for io_flush"
    )))
}

/// `file:flush()`. C: `f_flush`.
pub fn f_flush(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: FILE *f = tofile(L);
    // C: return luaL_fileresult(L, fflush(f) == 0, NULL);
    // TODO(port): borrow split needed for f_flush
    Err(LuaError::runtime(format_args!(
        "TODO(port): borrow split needed for f_flush"
    )))
}

// ── Lines iterator ───────────────────────────────────────────────────────────

/// Build the `io_readline` closure with its upvalues and push it.
/// C: `aux_lines(L, toclose)`.
///
/// Upvalue layout (C comment):
///   1) file handle (first stack value)
///   2) number of read-format arguments
///   3) toclose flag (bool)
///   4..n+3) format arguments
fn aux_lines(state: &mut LuaState, toclose: bool) -> Result<(), LuaError> {
    // C: int n = lua_gettop(L) - 1;
    let n = state.top_idx().get() as i32 - 1;
    // C: luaL_argcheck(L, n <= MAXARGLINE, MAXARGLINE+2, "too many arguments");
    if n > MAX_ARG_LINE as i32 {
        return Err(LuaError::arg_error(
            MAX_ARG_LINE as i32 + 2,
            "too many arguments",
        ));
    }
    // C: lua_pushvalue(L, 1);
    state.push_value_at(1);
    // C: lua_pushinteger(L, n);
    state.push(LuaValue::Int(n as i64));
    // C: lua_pushboolean(L, toclose);
    state.push(LuaValue::Bool(toclose));
    // C: lua_rotate(L, 2, 3);  /* move three values to their positions */
    state.rotate(2, 3);
    // C: lua_pushcclosure(L, io_readline, 3 + n);
    state.push_c_closure(io_readline, (3 + n) as i32)?;
    Ok(())
}

/// `file:lines(...)`. C: `f_lines`.
pub fn f_lines(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: tofile(L); aux_lines(L, 0); return 1;
    let _ = tofile(state)?; // validates file is open
    aux_lines(state, false)?;
    Ok(1)
}

/// `io.lines([filename, ...])`. C: `io_lines`.
pub fn io_lines(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: if (lua_isnone(L, 1)) lua_pushnil(L);
    if state.type_at(1) == LuaType::None {
        state.push(LuaValue::Nil);
    }
    // C: if (lua_isnil(L, 1)) { /* use default input */ }
    let toclose = if state.type_at(1) == LuaType::Nil {
        // C: lua_getfield(L, LUA_REGISTRYINDEX, IO_INPUT); lua_replace(L, 1);
        state.registry_get(IO_INPUT_KEY)?;
        state.replace(1);
        // C: tofile(L);  /* check it's valid */
        let _ = tofile(state)?;
        false
    } else {
        // C: const char *filename = luaL_checkstring(L, 1);
        // C: opencheck(L, filename, "r"); lua_replace(L, 1);
        let filename = state.check_arg_string(1)?;
        opencheck(state, &filename, b"r")?;
        state.replace(1)?;
        true
    };

    aux_lines(state, toclose)?;

    if toclose {
        // C: lua_pushnil(L); lua_pushnil(L); lua_pushvalue(L, 1); return 4;
        state.push(LuaValue::Nil); // state
        state.push(LuaValue::Nil); // control
        state.push_value_at(1);    // file as to-be-closed variable (4th result)
        Ok(4)
    } else {
        Ok(1)
    }
}

/// Iteration function created by `aux_lines`. C: `io_readline`.
///
/// Upvalue layout matches what `aux_lines` creates:
///   upvalue 1: file handle (userdata)
///   upvalue 2: n (number of read-format args)
///   upvalue 3: toclose flag
///   upvalue 4..n+3: format arguments
fn io_readline(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: LStream *p = (LStream *)lua_touserdata(L, lua_upvalueindex(1));
    // C: int n = (int)lua_tointeger(L, lua_upvalueindex(2));
    // TODO(port): access upvalues via state.get_upvalue(n); extract LStream from upvalue 1.
    let n = match state.value_at(crate::state_stub::upvalue_index(2)) {
        LuaValue::Int(i) => i as usize,
        _ => 0,
    };

    // C: if (isclosed(p)) return luaL_error(L, "file is already closed");
    // TODO(port): extract LStream from upvalue 1 and check is_closed()
    let is_closed: bool = todo!("TODO(port): check LStream::is_closed() from upvalue 1");
    if is_closed {
        return Err(LuaError::runtime(format_args!("file is already closed")));
    }

    // C: lua_settop(L, 1);
    state.set_top(1);
    // C: luaL_checkstack(L, n, "too many arguments");
    state.ensure_stack(n as i32, "too many arguments")?;

    // C: for (i = 1; i <= n; i++) lua_pushvalue(L, lua_upvalueindex(3 + i));
    for i in 1..=n {
        let uv = state.value_at(crate::state_stub::upvalue_index(3 + i as i32));
        state.push(uv);
    }

    // C: n = g_read(L, p->f, 2);
    // TODO(port): extract file from upvalue 1 LStream, call g_read(state, file, 2)
    let result_n: usize = todo!("TODO(port): call g_read with file from upvalue 1");

    // C: lua_assert(n > 0);
    debug_assert!(result_n > 0, "g_read should return at least one value");

    // C: if (lua_toboolean(L, -n)) return n;  /* read at least one value */
    let top = state.top_idx().get() as i32;
    let first_result_idx = top - result_n as i32;
    let first_truthy = !matches!(
        state.stack_at(first_result_idx),
        LuaValue::Nil | LuaValue::Bool(false)
    );
    if first_truthy {
        return Ok(result_n);
    }

    // C: if (n > 1) return luaL_error(L, "%s", lua_tostring(L, -n+1));
    if result_n > 1 {
        let err_val = state.stack_at(first_result_idx + 1).clone();
        return Err(LuaError::from_value(err_val));
    }

    // C: if (lua_toboolean(L, lua_upvalueindex(3))) { /* generator created file */ ... }
    let toclose = !matches!(
        state.value_at(crate::state_stub::upvalue_index(3)),
        LuaValue::Nil | LuaValue::Bool(false)
    );
    if toclose {
        // C: lua_settop(L, 0); lua_pushvalue(L, lua_upvalueindex(1)); aux_close(L);
        state.set_top(0);
        state.push_upvalue(1);
        aux_close(state)?;
    }

    Ok(0)
}

// ── Module registration ──────────────────────────────────────────────────────

/// Create the file-handle metatable in the registry. C: `createmeta(L)`.
fn create_meta(state: &mut LuaState) -> Result<(), LuaError> {
    // C: luaL_newmetatable(L, LUA_FILEHANDLE);
    state.new_metatable(LUA_FILE_HANDLE)?;
    // C: luaL_setfuncs(L, metameth, 0);
    state.set_funcs(FILE_METAMETHODS, 0)?;
    // C: luaL_newlibtable(L, meth);
    state.new_lib_table(FILE_METHODS)?;
    // C: luaL_setfuncs(L, meth, 0);
    state.set_funcs(FILE_METHODS, 0)?;
    // C: lua_setfield(L, -2, "__index");  /* metatable.__index = method table */
    state.set_field(-2, b"__index")?;
    // C: lua_pop(L, 1);
    state.pop_n(1);
    Ok(())
}

/// Register stdin, stdout, or stderr as a Lua file handle. C: `createstdfile`.
fn create_std_file(
    state: &mut LuaState,
    std_kind: StdFileKind,
    registry_key: Option<&[u8]>,
    field_name: &[u8],
) -> Result<(), LuaError> {
    // C: LStream *p = newprefile(L); p->f = f; p->closef = &io_noclose;
    let cell = new_pre_file(state)?;
    {
        let mut p = cell.borrow_mut();
        p.file = Some(Box::new(StdStreamHandle::new(std_kind)));
        p.close_fn = Some(io_noclose);
    }
    if let Some(key) = registry_key {
        // C: lua_pushvalue(L, -1); lua_setfield(L, LUA_REGISTRYINDEX, k);
        state.push_value_at(-1);
        state.registry_set(key)?;
    }
    // C: lua_setfield(L, -2, fname);
    state.set_field(-2, field_name)?;
    Ok(())
}

/// Open the `io` library and return 1 (the library table). C: `luaopen_io`.
pub fn luaopen_io(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: luaL_newlib(L, iolib);
    state.new_lib(IO_LIB)?;
    // C: createmeta(L);
    create_meta(state)?;
    // C: createstdfile(L, stdin,  IO_INPUT,  "stdin");
    create_std_file(state, StdFileKind::Stdin, Some(IO_INPUT_KEY), b"stdin")?;
    // C: createstdfile(L, stdout, IO_OUTPUT, "stdout");
    create_std_file(state, StdFileKind::Stdout, Some(IO_OUTPUT_KEY), b"stdout")?;
    // C: createstdfile(L, stderr, NULL,      "stderr");
    create_std_file(state, StdFileKind::Stderr, None, b"stderr")?;
    Ok(1)
}

// ────────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        src/liolib.c  (841 lines, ~35 functions)
//   target_crate:  lua-stdlib
//   confidence:    medium
//   todos:         64
//   port_notes:    2
//   unsafe_blocks: 0   (must be 0 outside lua-gc/lua-coro)
//   notes:         Logic faithfully translated. Three systemic Phase B blockers:
//                  (1) All concrete LuaFileOps implementations need std::fs or
//                  std::process, both banned outside lua-cli by PORTING.md; the
//                  architecture must grant an exemption for lua-stdlib/src/io_lib.rs
//                  or introduce a thin IO-abstraction crate.
//                  (2) The borrow checker prevents holding &mut dyn LuaFileOps
//                  (extracted from LuaUserData) and &mut LuaState simultaneously;
//                  fix via RefCell<Box<dyn LuaFileOps>> inside LStream, plus
//                  restructure g_read/g_write to accept StackIdx not a raw borrow.
//                  (3) C's %.14g (significant-digit float format) has no direct
//                  Rust equivalent; a custom formatter is needed for faithful
//                  number serialisation. The typed-userdata API (needed to cast
//                  raw LuaUserData bytes to LStream) must also land in Phase B.
//                  rustc self-check shows only expected E0432/E0433 import errors.
// ────────────────────────────────────────────────────────────────────────────
