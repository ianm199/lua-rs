//! Standard I/O library — `io.*` functions and `file:*` methods.
//!
//! C source: `reference/lua-5.4.7/src/liolib.c`.
//!
//! **Impurity is host-provided and load-bearing.** Filesystem and process
//! access reach the host only through hooks: regular files via
//! `GlobalState::file_open_hook`, `io.popen` via `GlobalState::popen_hook`,
//! stdout/stderr via output hooks. The native CLI installs hooks backed by
//! `std::fs`/`std::process`/`std::io`; sandboxed and `wasm32` hosts leave those
//! capabilities absent, and the functions then return a clean `(nil, msg,
//! errno)` failure tuple (or, for the standard streams, an `Unsupported` error)
//! instead of touching ambient OS state. That hook plumbing and the `wasm32`
//! cfg gates are deliberately kept intact.
//!
//! **The side-table indirection** is the one structural divergence from C: C
//! stores the `LStream` inside the userdata payload, but `LStream` carries heap
//! pointers (a `Box<dyn LuaFileHandle>` and a fn pointer) that cannot be safely
//! reinterpreted from a raw byte buffer in safe Rust, so the stream lives in a
//! per-VM side table (`GlobalState::io_streams`) keyed by userdata identity,
//! dropped with the VM (issue #317). Each I/O step
//! borrows the file through its `Rc<RefCell<LStream>>` briefly, releases the
//! borrow, then touches `LuaState` — resolving C's single `LStream *` aliasing
//! into two scoped borrows.
//!
//! Graduation: the deterministic format-parsing/validation and error-shaping
//! surface (read formats, the `*`-prefix version seam, the closed-file error,
//! `io.type`) is pinned by `tests/io_strengthen.rs` against the reference
//! binaries; host-specific I/O *results* are not reproducible and stay
//! oracle-checked only through the official `files.lua` suite. See
//! `crates/lua-stdlib/GRADUATED.md`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{self, SeekFrom};
use std::rc::Rc;

use crate::state_stub::{LuaState, LuaStateStubExt as _};
use lua_types::{LuaError, LuaFileHandle, LuaType, LuaValue};
use lua_vm::state::{InputHook, OutputHook};

/// Side-table mapping userdata identity (the `Rc` pointer address from
/// `GcRef::identity()`) to its associated `LStream` (see the module header
/// for why the stream lives here rather than inside the userdata payload).
/// Entries are inserted by `new_pre_file` and never removed individually —
/// a leak bounded by the owning VM's lifetime per `PORTING.md` §2 #4.
type LStreamMap = RefCell<HashMap<usize, Rc<RefCell<LStream>>>>;

/// Fetch this VM's `LStream` side table from [`GlobalState::io_streams`],
/// lazily installing an empty map on first use. The table is per-VM state
/// (issue #317): a process-wide `thread_local!` outlived the VMs that filled
/// it, retaining one dead `LStream` per file userdata whose identity address
/// was never reused — the leak canary's per-iteration VM-churn growth.
///
/// [`GlobalState::io_streams`]: lua_vm::state::GlobalState::io_streams
fn lstream_registry(state: &LuaState) -> Rc<LStreamMap> {
    let installed = state.global().io_streams.clone();
    match installed {
        Some(any) => any
            .downcast::<LStreamMap>()
            .expect("GlobalState::io_streams holds the io LStream side table"),
        None => {
            let fresh: Rc<LStreamMap> = Rc::new(RefCell::new(HashMap::new()));
            state.global_mut().io_streams = Some(fresh.clone());
            fresh
        }
    }
}

fn register_lstream(state: &LuaState, ud_id: usize, lstream: LStream) -> Rc<RefCell<LStream>> {
    let cell = Rc::new(RefCell::new(lstream));
    lstream_registry(state)
        .borrow_mut()
        .insert(ud_id, cell.clone());
    cell
}

fn lookup_lstream(state: &LuaState, ud_id: usize) -> Option<Rc<RefCell<LStream>>> {
    lstream_registry(state).borrow().get(&ud_id).cloned()
}

// ── Constants ────────────────────────────────────────────────────────────────

/// Name of the file-handle metatable in the Lua registry. C: `LUA_FILEHANDLE`.
pub const LUA_FILE_HANDLE: &[u8] = b"FILE*";

/// Registry key for the default input file. C: `IO_INPUT` = `"_IO_input"`.
const IO_INPUT_KEY: &[u8] = b"_IO_input";

/// Registry key for the default output file. C: `IO_OUTPUT` = `"_IO_output"`.
const IO_OUTPUT_KEY: &[u8] = b"_IO_output";

/// Number of bytes in the `"_IO_"` prefix, used to strip it in error messages.
const IO_PREFIX_LEN: usize = 4;

/// Maximum number of format-arguments passed to `file:lines`. C: `MAXARGLINE`.
const MAX_ARG_LINE: usize = 250;

/// Maximum byte-length of a numeric literal read from a file. C: `L_MAXLENNUM`.
const L_MAX_LEN_NUM: usize = 200;

/// End-of-file sentinel returned by `LuaFileHandle::read_byte`. C: `EOF` == -1.
const EOF_SENTINEL: i32 = -1;

/// Bulk-read chunk size, mirroring C's `LUAL_BUFFERSIZE`.
const LUAL_BUFFER_SIZE: usize = 8192;

// ── Traits ───────────────────────────────────────────────────────────────────

/// Capabilities required by the io library from an OS file handle.
///
/// This trait extends [`LuaFileHandle`] (defined in `lua-types`) with the
/// additional `set_buf_mode` operation. Concrete implementations backed by
/// `std::fs::File` live in `lua-cli`; standard-stream implementations live in
/// this module. The split keeps `std::fs` out of `lua-stdlib` per PORTING.md §1.
pub trait LuaFileOps: LuaFileHandle {
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

/// A Lua file handle. C equivalent: `typedef luaL_Stream LStream`.
///
/// The instance lives in `LSTREAM_REGISTRY` (keyed by its userdata's identity),
/// wrapped in `Rc<RefCell<…>>` so a brief file borrow and a `LuaState` borrow
/// never overlap — the safe-Rust resolution of C's single `LStream *` pointer.
pub struct LStream {
    /// OS file handle. `None` = incompletely opened (the pre-file pattern).
    /// Concrete implementations are installed via `GlobalState::file_open_hook`
    /// (registered by `lua-cli`) to keep `std::fs` out of `lua-stdlib`.
    pub file: Option<Box<dyn LuaFileHandle>>,
    /// Close callback. `None` means the stream is closed. C: `p->closef == NULL`.
    pub close_fn: Option<fn(&mut LuaState) -> Result<usize, LuaError>>,
}

impl LStream {
    /// `isclosed(p)` in C: true when `closef` is NULL.
    pub fn is_closed(&self) -> bool {
        self.close_fn.is_none()
    }
}

/// Standard stream handle for stdin/stdout/stderr.
///
/// Output goes through host hooks when installed. Native builds keep a direct
/// stdio fallback for compatibility; bare `wasm32-unknown-unknown` reports
/// unsupported instead of touching stubbed stdio.
struct StdStreamHandle {
    kind: StdFileKind,
    input_hook: Option<InputHook>,
    output_hook: Option<OutputHook>,
    unread: Option<u8>,
}

impl LuaFileHandle for StdStreamHandle {
    fn read_byte(&mut self) -> i32 {
        if let Some(byte) = self.unread.take() {
            return byte as i32;
        }
        match self.kind {
            StdFileKind::Stdin => {
                if let Some(read_fn) = self.input_hook {
                    let mut buf = [0u8; 1];
                    return match read_fn(&mut buf) {
                        Ok(1) => buf[0] as i32,
                        _ => EOF_SENTINEL,
                    };
                }

                #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
                {
                    EOF_SENTINEL
                }

                #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
                {
                    use std::io::Read;
                    let mut buf = [0u8; 1];
                    match std::io::stdin().read(&mut buf) {
                        Ok(1) => buf[0] as i32,
                        _ => EOF_SENTINEL,
                    }
                }
            }
            _ => EOF_SENTINEL,
        }
    }
    fn unread_byte(&mut self, byte: i32) {
        if (0..=u8::MAX as i32).contains(&byte) {
            self.unread = Some(byte as u8);
        }
    }
    fn write_bytes(&mut self, data: &[u8]) -> io::Result<usize> {
        if let Some(write_fn) = self.output_hook {
            write_fn(data)?;
            return Ok(data.len());
        }

        #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
        {
            let _ = data;
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "standard output not available in this host",
            ));
        }

        #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
        {
            use std::io::Write;
            match self.kind {
                StdFileKind::Stderr => {
                    std::io::stderr().write_all(data)?;
                    Ok(data.len())
                }
                _ => {
                    std::io::stdout().write_all(data)?;
                    Ok(data.len())
                }
            }
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        if self.output_hook.is_some() {
            return Ok(());
        }

        #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "standard output not available in this host",
            ));
        }

        #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
        {
            use std::io::Write;
            match self.kind {
                StdFileKind::Stderr => std::io::stderr().flush(),
                _ => std::io::stdout().flush(),
            }
        }
    }
    fn seek(&mut self, _pos: SeekFrom) -> io::Result<u64> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "stdio seek"))
    }
    fn tell(&mut self) -> io::Result<u64> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "stdio tell"))
    }
    fn clear_error(&mut self) {}
    fn has_error(&self) -> bool {
        false
    }
}

impl LuaFileOps for StdStreamHandle {
    fn set_buf_mode(&mut self, _mode: BufMode, _size: usize) -> io::Result<()> {
        Ok(())
    }
}

impl StdStreamHandle {
    fn new(
        kind: StdFileKind,
        input_hook: Option<InputHook>,
        output_hook: Option<OutputHook>,
    ) -> Self {
        StdStreamHandle {
            kind,
            input_hook,
            output_hook,
            unread: None,
        }
    }
}

/// State machine for reading a numeric literal byte-by-byte from a file.
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
    fn advance(&mut self, file: &mut dyn LuaFileHandle) -> bool {
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
    fn try2(&mut self, file: &mut dyn LuaFileHandle, set: [u8; 2]) -> bool {
        if self.current == set[0] as i32 || self.current == set[1] as i32 {
            self.advance(file)
        } else {
            false
        }
    }

    /// Consume a run of (hex)digits; return the count. C: `readdigits`.
    fn read_digits(&mut self, file: &mut dyn LuaFileHandle, hex: bool) -> usize {
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
    (b"close", io_close),
    (b"flush", io_flush),
    (b"input", io_input),
    (b"lines", io_lines),
    (b"open", io_open),
    (b"output", io_output),
    (b"popen", io_popen),
    (b"read", io_read),
    (b"tmpfile", io_tmpfile),
    (b"type", io_type),
    (b"write", io_write),
];

/// `file:*` instance methods. C: `static const luaL_Reg meth[]`.
pub const FILE_METHODS: &[(&[u8], fn(&mut LuaState) -> Result<usize, LuaError>)] = &[
    (b"read", f_read),
    (b"write", f_write),
    (b"lines", f_lines),
    (b"flush", f_flush),
    (b"seek", f_seek),
    (b"close", f_close),
    (b"setvbuf", f_setvbuf),
];

/// File-handle metamethods. C: `static const luaL_Reg metameth[]`.
pub const FILE_METAMETHODS: &[(&[u8], fn(&mut LuaState) -> Result<usize, LuaError>)] = &[
    (b"__gc", f_gc),
    (b"__close", f_gc),
    (b"__tostring", f_tostring),
];

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Validate an `fopen` mode string: must match `[rwa]\+?b*`. C: `l_checkmode`.
///
/// (*mode != '+' || ...) && strspn(mode, "b") == strlen(mode));`
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

/// Canonical POSIX `strerror` text for common errnos, target-independent.
///
/// `wasm32-unknown-unknown`'s `std` ships no libc `strerror` table, so
/// `io::Error::from_raw_os_error(n).to_string()` renders a useless
/// "operation successful" there (#301). Resolving the message from this static
/// map instead makes the wasm build report the SAME text a native build does
/// for the same errno. The strings are the standard POSIX texts that Darwin and
/// glibc `strerror` both emit — verified against the reference binary for the
/// file errnos the oracle exercises (2, 9, 13, 20, 28, 1) — so using the table
/// on every target is deterministic and keeps wasm and native identical.
/// Errnos absent here (rare, platform-specific values whose text diverges
/// between Darwin and glibc, e.g. macOS `ENOTEMPTY` = 66) return `None` and are
/// resolved from the OS `Display`, which is correct on a native host and does
/// not arise on wasm.
#[cfg(any(unix, target_arch = "wasm32"))]
fn posix_strerror(code: i32) -> Option<&'static str> {
    Some(match code {
        1 => "Operation not permitted",
        2 => "No such file or directory",
        3 => "No such process",
        4 => "Interrupted system call",
        5 => "Input/output error",
        7 => "Argument list too long",
        8 => "Exec format error",
        9 => "Bad file descriptor",
        10 => "No child processes",
        12 => "Cannot allocate memory",
        13 => "Permission denied",
        14 => "Bad address",
        17 => "File exists",
        20 => "Not a directory",
        21 => "Is a directory",
        22 => "Invalid argument",
        24 => "Too many open files",
        27 => "File too large",
        28 => "No space left on device",
        29 => "Illegal seek",
        30 => "Read-only file system",
        32 => "Broken pipe",
        _ => return None,
    })
}

/// The [`posix_strerror`] table only applies where `raw_os_error()` IS a POSIX
/// errno — Unix and wasm. On Windows `raw_os_error()` is a Win32 code with
/// different numbering (code 5 is `ERROR_ACCESS_DENIED`, not `EIO`), so the
/// table would mistranslate it; there we return `None` and fall back to the OS
/// `Display` (Windows's own message + code), which is the port's native
/// behavior. Faithful Win32→CRT-errno normalization is a separate follow-up.
#[cfg(any(unix, target_arch = "wasm32"))]
fn target_posix_strerror(code: i32) -> Option<&'static str> {
    posix_strerror(code)
}

#[cfg(not(any(unix, target_arch = "wasm32")))]
fn target_posix_strerror(_code: i32) -> Option<&'static str> {
    None
}

/// Render an `io::Error` the way C's `strerror(errno)` would: the plain
/// platform message, with no trailing `" (os error N)"`.
///
/// For an OS-backed error the message comes from [`posix_strerror`] (a
/// target-independent table, so the wasm build matches native — #301), falling
/// back to the OS `Display` minus its `" (os error N)"` suffix for errnos the
/// table does not cover. `std::io::Error`'s `Display` appends that suffix for
/// the `Repr::Os` case in a stable format, so stripping it is a deterministic
/// un-format. Errors with no raw OS code (e.g. "no filesystem hook registered")
/// carry their own message and pass through unchanged.
fn strerror_text(err: &io::Error) -> String {
    match err.raw_os_error() {
        Some(code) => match target_posix_strerror(code) {
            Some(msg) => msg.to_string(),
            None => {
                let full = err.to_string();
                let suffix = format!(" (os error {code})");
                full.strip_suffix(&suffix).unwrap_or(&full).to_string()
            }
        },
        None => err.to_string(),
    }
}

/// Push success (`true`) or failure (`fail`, msg[, errno]) per `luaL_fileresult`.
///
/// On success: `lua_pushboolean(L, 1); return 1`. On failure C runs
/// `luaL_pushfail(L); lua_pushfstring(...); lua_pushinteger(errno); return 3`,
/// and `luaL_pushfail` resolves to `lua_pushnil` on every supported version
/// (5.1-5.5), so the first failure value is `nil`, never `false`. Tests that
/// compare the failure handle to `nil` (e.g. `io.open(missing) == nil`) rely on
/// this exact value.
///
/// The message and errno come straight off `os_err` via [`strerror_text`] and
/// `raw_os_error()` — callers must pass through the original `io::Error` from
/// the host hook unmodified, never re-wrap it as `ErrorKind::Other` (#301:
/// re-wrapping drops `raw_os_error()`, surfacing as errno 0 downstream).
///
/// The errno is pushed **only when `os_err` actually carries one**
/// (`raw_os_error().is_some()` — every real OS failure does). C always has a
/// live `errno`, so real filesystem failures produce the faithful 3-value
/// triple. When the error is a non-OS failure with no errno — an omniLua-only
/// case C cannot reach, e.g. a sandbox capability failure ("no filesystem hook
/// registered") or a host hook that returned a `raw_os_error`-less
/// `io::Error` — there is genuinely no errno to report, so the result is the
/// honest 2-value `(nil, msg)`. Coercing that to `0` (the removed
/// `unwrap_or(0)`) was the #301 fallback: a fabricated errno that no `errno`
/// value ever legitimately equals here.
pub(crate) fn file_result(
    state: &mut LuaState,
    success: bool,
    fname: Option<&[u8]>,
    os_err: io::Error,
) -> Result<usize, LuaError> {
    if success {
        state.push(LuaValue::Bool(true));
        return Ok(1);
    }
    state.push(LuaValue::Nil);
    let msg = strerror_text(&os_err);
    match fname {
        Some(name) => {
            let mut s = Vec::with_capacity(name.len() + 2 + msg.len());
            s.extend_from_slice(name);
            s.extend_from_slice(b": ");
            s.extend_from_slice(msg.as_bytes());
            state.push_string(&s)?;
        }
        None => {
            state.push_string(msg.as_bytes())?;
        }
    }
    match os_err.raw_os_error() {
        Some(code) => {
            state.push(LuaValue::Int(code as i64));
            Ok(3)
        }
        None => Ok(2),
    }
}

/// Push popen/system exit-status results per `luaL_execresult`: `true` on a
/// zero status, else `(nil, "exit"|"signal", stat)`.
///
/// Deferred: `WIFEXITED`/`WTERMSIG` are not portable across all hosts, so this
/// always reports a non-zero status as an `"exit"` code and never distinguishes
/// a signal — faithful enough for the clients that probe an exit status.
fn exec_result(state: &mut LuaState, stat: i32) -> Result<usize, LuaError> {
    if stat == 0 {
        state.push(LuaValue::Bool(true));
        Ok(1)
    } else {
        state.push(LuaValue::Bool(false));
        state.push_string(b"exit")?;
        state.push(LuaValue::Int(stat as i64));
        Ok(3)
    }
}

/// Retrieve `LStream` from argument 1 via a userdata type-check.
///
/// Returns an `Rc<RefCell<LStream>>` from the side-table registry. The C port
/// returns a raw `LStream *` pointing into the userdata payload; Rust uses a
/// side table because `LStream` contains heap pointers that cannot be safely
/// reinterpreted from a raw byte buffer in safe Rust.
fn get_lstream(state: &mut LuaState) -> Result<Rc<RefCell<LStream>>, LuaError> {
    let ud = state.check_arg_userdata(1, LUA_FILE_HANDLE)?;
    lookup_lstream(state, ud.identity())
        .ok_or_else(|| LuaError::runtime(format_args!("invalid file handle")))
}

/// Look up the `LStream` registered for the userdata sitting at upvalue `idx`.
///
/// `aux_lines` stores the file-handle userdata as upvalue 1 of `io_readline`;
/// this helper performs the same registry round-trip that `get_lstream` does
/// for argument 1, but reads the value from the closure's upvalue slot instead
/// of the call stack.
fn lstream_from_upvalue(state: &mut LuaState, idx: i32) -> Result<Rc<RefCell<LStream>>, LuaError> {
    let v = state.value_at(crate::state_stub::upvalue_index(idx));
    let ud_id = match v {
        LuaValue::UserData(ud) => ud.identity(),
        _ => {
            return Err(LuaError::runtime(format_args!(
                "invalid file handle in upvalue {}",
                idx
            )));
        }
    };
    lookup_lstream(state, ud_id)
        .ok_or_else(|| LuaError::runtime(format_args!("invalid file handle in upvalue {}", idx)))
}

/// Validate that argument 1 is an open file handle; error if closed.
///
/// The closed-file error is raised through `c_api_runtime` (the `luaL_error`
/// analogue) so it carries the calling source-location prefix
/// (`<source>:<line>:`), matching the reference `tofile` in `liolib.c`.
fn tofile(state: &mut LuaState) -> Result<Rc<RefCell<LStream>>, LuaError> {
    let p_rc = get_lstream(state)?;
    let closed = {
        let p = p_rc.borrow();
        debug_assert!(p.is_closed() || p.file.is_some());
        p.is_closed()
    };
    if closed {
        return Err(lua_vm::debug::c_api_runtime(
            state,
            b"attempt to use a closed file".to_vec(),
        ));
    }
    Ok(p_rc)
}

// ── File creation helpers ────────────────────────────────────────────────────

/// Allocate a "closed" file-handle userdata and push it; set its metatable.
/// Also registers an empty `LStream` in the side table keyed by the userdata
/// identity, and returns the `Rc<RefCell<LStream>>` so the caller may finish
/// initialising it (set `file`, set `close_fn`). C: `newprefile(L)`.
fn new_pre_file(state: &mut LuaState) -> Result<Rc<RefCell<LStream>>, LuaError> {
    let ud = state.new_userdata_typed(LUA_FILE_HANDLE, std::mem::size_of::<LStream>(), 0)?;
    state.set_metatable_by_name(LUA_FILE_HANDLE)?;
    let cell = register_lstream(
        state,
        ud.identity(),
        LStream {
            file: None,
            close_fn: None,
        },
    );
    Ok(cell)
}

/// Allocate a new regular-file handle with `io_fclose` as the close function.
fn new_file(state: &mut LuaState) -> Result<Rc<RefCell<LStream>>, LuaError> {
    let cell = new_pre_file(state)?;
    cell.borrow_mut().close_fn = Some(io_fclose);
    Ok(cell)
}

/// Build the `io.input`/`io.output`/`io.lines` open-failure error, version-gated.
///
/// C splits here by version. 5.2+ `opencheck` raises
/// `luaL_error(L, "cannot open file '%s' (%s)", fname, strerror(errno))`. 5.1's
/// `g_iofile`/`io_lines` instead call `fileerror`, which does
/// `luaL_argerror(L, 1, "<fname>: <strerror>")` — rendered as
/// `bad argument #1 to '<caller>' (<fname>: <strerror>)`, with `<caller>`
/// resolved from the call stack ('?' under `pcall(io.input, x)`, `'lines'` when
/// called from a Lua frame). [`lua_vm::debug::arg_error_impl`] reproduces that
/// exact stack walk, so routing the 5.1 case through it matches the reference
/// funcname and location prefix without special-casing either.
fn open_failure_error(state: &mut LuaState, fname: &[u8], detail: &[u8]) -> LuaError {
    if matches!(state.global().lua_version, lua_types::LuaVersion::V51) {
        let mut extramsg = Vec::with_capacity(fname.len() + 2 + detail.len());
        extramsg.extend_from_slice(fname);
        extramsg.extend_from_slice(b": ");
        extramsg.extend_from_slice(detail);
        lua_vm::debug::arg_error_impl(state, 1, &extramsg)
    } else {
        let mut msg = Vec::with_capacity(fname.len() + detail.len() + 20);
        msg.extend_from_slice(b"cannot open file '");
        msg.extend_from_slice(fname);
        msg.extend_from_slice(b"' (");
        msg.extend_from_slice(detail);
        msg.push(b')');
        lua_vm::debug::c_api_runtime(state, msg)
    }
}

/// Open `fname` and push its handle; raise a runtime error on failure.
///
/// The file system is reached via `GlobalState::file_open_hook` (registered by
/// `lua-cli`) since `std::fs` is banned in `lua-stdlib` per PORTING.md §1.
fn opencheck(state: &mut LuaState, fname: &[u8], mode: &[u8]) -> Result<(), LuaError> {
    let hook = state.global().file_open_hook;
    let fh = match hook {
        Some(open_fn) => match open_fn(fname, mode) {
            Ok(fh) => fh,
            Err(e) => {
                let detail = strerror_text(&e);
                return Err(open_failure_error(state, fname, detail.as_bytes()));
            }
        },
        None => {
            return Err(open_failure_error(
                state,
                fname,
                b"no filesystem hook registered",
            ));
        }
    };
    let cell = new_file(state)?;
    cell.borrow_mut().file = Some(fh);
    Ok(())
}

// ── Close functions ──────────────────────────────────────────────────────────

/// Close a regular file via `fclose`. C: `io_fclose`.
///
/// Dropping the `Box<dyn LuaFileHandle>` flushes through the host handle's own
/// `Drop` (the CLI's writer flushes on drop). Deferred: surfacing a close-time
/// I/O error as a `file_result` failure tuple — close currently always reports
/// success.
fn io_fclose(state: &mut LuaState) -> Result<usize, LuaError> {
    let p_rc = get_lstream(state)?;
    let _closed = p_rc.borrow_mut().file.take();
    state.push(LuaValue::Bool(true));
    Ok(1)
}

/// Close a popen process pipe. C: `io_pclose`.
///
/// Deferred: waiting on the child and forwarding its real exit status. Dropping
/// the handle closes the pipe; the status reported here is a fixed success.
fn io_pclose(state: &mut LuaState) -> Result<usize, LuaError> {
    let p_rc = get_lstream(state)?;
    let _closed = p_rc.borrow_mut().file.take();
    exec_result(state, 0)
}

/// Refuse to close a standard-stream handle. C: `io_noclose`.
///
/// The close function is reinstalled before returning so the handle stays alive
/// and remains closeable-but-inert on a later attempt, matching C's `io_noclose`.
fn io_noclose(state: &mut LuaState) -> Result<usize, LuaError> {
    let p_rc = get_lstream(state)?;
    p_rc.borrow_mut().close_fn = Some(io_noclose);
    state.push(LuaValue::Bool(false));
    state.push_string(b"cannot close standard file")?;
    Ok(2)
}

/// Invoke the stream's close function and mark it closed. C: `aux_close`.
fn aux_close(state: &mut LuaState) -> Result<usize, LuaError> {
    let p_rc = get_lstream(state)?;
    let cf = p_rc.borrow_mut().close_fn.take().ok_or_else(|| {
        LuaError::runtime(format_args!("attempt to close an already-closed file"))
    })?;
    cf(state)
}

// ── io.type ──────────────────────────────────────────────────────────────────

/// `io.type(x)` — return `"file"`, `"closed file"`, or the fail value for a
/// non-handle. C: `io_type`.
///
/// A non-handle pushes the `fail` value (`nil`) via the reference's
/// `luaL_pushfail`, NOT `false`; `fail` is `nil` on every supported version.
/// An unknown userdata still carrying the `FILE*` metatable but absent from the
/// `LStream` side table is treated as closed (it cannot be an open stream).
pub fn io_type(state: &mut LuaState) -> Result<usize, LuaError> {
    state.check_arg_any(1)?;
    let maybe_userdata = state.test_arg_userdata(1, LUA_FILE_HANDLE);
    match maybe_userdata {
        None => {
            state.push(LuaValue::Nil);
        }
        Some(ud) => {
            let is_closed = match lookup_lstream(state, ud.identity()) {
                Some(rc) => rc.borrow().is_closed(),
                None => true,
            };
            if is_closed {
                state.push_string(b"closed file")?;
            } else {
                state.push_string(b"file")?;
            }
        }
    }
    Ok(1)
}

// ── __tostring metamethod ────────────────────────────────────────────────────

/// `tostring(file)` metamethod. C: `f_tostring`.
///
/// An open handle renders `file (0x?)`. The reference prints the handle's real
/// pointer address (`file (0x<addr>)`); that address is non-deterministic, so
/// reproducing it is deferred and intentionally not pinned by the behavioral
/// net. A closed handle renders `file (closed)`, matching the reference exactly.
fn f_tostring(state: &mut LuaState) -> Result<usize, LuaError> {
    let p_rc = get_lstream(state)?;
    let closed = p_rc.borrow().is_closed();
    if closed {
        state.push_string(b"file (closed)")?;
    } else {
        state.push_string(b"file (0x?)")?;
    }
    Ok(1)
}

// ── close / gc ───────────────────────────────────────────────────────────────

/// `file:close()`. C: `f_close`.
fn f_close(state: &mut LuaState) -> Result<usize, LuaError> {
    let _ = tofile(state)?; // validates stream is open before closing
    aux_close(state)
}

/// `io.close([file])`. C: `io_close`.
pub fn io_close(state: &mut LuaState) -> Result<usize, LuaError> {
    // The pushed value naturally lands at position 1 (top advances by one from
    // func+1 to func+2). The C source does NOT call lua_replace here; adding one
    // would pop the value back out, since position 1 equals top-1 in this case.
    if state.type_at(1) == LuaType::None {
        state.registry_get(IO_OUTPUT_KEY)?;
    }
    f_close(state)
}

/// `__gc` / `__close` metamethod — silently close if still open. C: `f_gc`.
fn f_gc(state: &mut LuaState) -> Result<usize, LuaError> {
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
///
/// The file system is reached via `GlobalState::file_open_hook` (registered by
/// `lua-cli`) since `std::fs` is banned in `lua-stdlib` per PORTING.md §1.
pub fn io_open(state: &mut LuaState) -> Result<usize, LuaError> {
    let filename: Vec<u8> = state.check_arg_string(1)?;
    let mode: Vec<u8> = state.opt_arg_string(2, b"r")?;
    if !check_mode(&mode) {
        return Err(lua_vm::debug::arg_error_impl(state, 2, b"invalid mode"));
    }
    let hook = state.global().file_open_hook;
    match hook {
        Some(open_fn) => match open_fn(&filename, &mode) {
            Ok(fh) => {
                let cell = new_file(state)?;
                cell.borrow_mut().file = Some(fh);
                Ok(1)
            }
            Err(os_err) => file_result(state, false, Some(&filename), os_err),
        },
        None => {
            let os_err =
                io::Error::new(io::ErrorKind::Unsupported, "no filesystem hook registered");
            file_result(state, false, Some(&filename), os_err)
        }
    }
}

/// `io.popen(filename [, mode])`. C: `io_popen`.
///
/// `std::process::Command` is banned in `lua-stdlib`; the child process is
/// spawned via `GlobalState::popen_hook`, which `lua-cli` installs. When the
/// hook is absent (sandboxed embeddings), this returns a clean Lua failure
/// shape (`nil, errmsg, errno`) rather than panicking, so clients such as
/// LuaRocks that probe `io.popen` fall back gracefully instead of crashing
/// the host.
pub fn io_popen(state: &mut LuaState) -> Result<usize, LuaError> {
    let filename: Vec<u8> = state.check_arg_string(1)?;
    let mode: Vec<u8> = state.opt_arg_string(2, b"r")?;
    if !check_mode_popen(&mode) {
        return Err(lua_vm::debug::arg_error_impl(state, 2, b"invalid mode"));
    }
    let hook = state.global().popen_hook;
    match hook {
        Some(spawn_fn) => match spawn_fn(&filename, &mode) {
            Ok(fh) => {
                let cell = new_pre_file(state)?;
                let mut p = cell.borrow_mut();
                p.file = Some(fh);
                p.close_fn = Some(io_pclose);
                drop(p);
                Ok(1)
            }
            Err(e) => {
                let os_err = io::Error::new(
                    io::ErrorKind::Other,
                    match e.message_bytes() {
                        Some(b) => String::from_utf8_lossy(b).into_owned(),
                        None => format!("{:?}", &e),
                    },
                );
                file_result(state, false, Some(&filename), os_err)
            }
        },
        None => {
            let os_err = io::Error::new(
                io::ErrorKind::Unsupported,
                "popen not enabled in this build",
            );
            file_result(state, false, Some(&filename), os_err)
        }
    }
}

fn native_temp_name() -> io::Result<Vec<u8>> {
    #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "temporary files not available in this host",
        ));
    }

    #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
    {
        let mut path = std::env::temp_dir().to_string_lossy().as_bytes().to_vec();
        if path.last().copied() != Some(b'/') && path.last().copied() != Some(b'\\') {
            path.push(b'/');
        }
        let unique = format!(
            "lua_tmpfile_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        path.extend_from_slice(unique.as_bytes());
        Ok(path)
    }
}

/// `io.tmpfile()`. C: `io_tmpfile`.
pub fn io_tmpfile(state: &mut LuaState) -> Result<usize, LuaError> {
    let hook = state.global().file_open_hook;
    let Some(open_fn) = hook else {
        let os_err = io::Error::new(io::ErrorKind::Unsupported, "no filesystem hook registered");
        return file_result(state, false, None, os_err);
    };

    let temp_name_hook = state.global().temp_name_hook;
    let path = match temp_name_hook {
        Some(temp_fn) => match temp_fn() {
            Ok(path) => path,
            Err(e) => {
                let msg = match e.message_bytes() {
                    Some(b) => String::from_utf8_lossy(b).into_owned(),
                    None => format!("{:?}", &e),
                };
                return file_result(
                    state,
                    false,
                    None,
                    io::Error::new(io::ErrorKind::Unsupported, msg),
                );
            }
        },
        None => match native_temp_name() {
            Ok(path) => path,
            Err(e) => return file_result(state, false, None, e),
        },
    };

    match open_fn(&path, b"w+b") {
        Ok(fh) => {
            let cell = new_file(state)?;
            cell.borrow_mut().file = Some(fh);
            Ok(1)
        }
        Err(os_err) => file_result(state, false, None, os_err),
    }
}

// ── io.input / io.output ─────────────────────────────────────────────────────

/// Generic setter/getter for `io.input` and `io.output`. C: `g_iofile`.
fn g_iofile(state: &mut LuaState, key: &[u8], mode: &[u8]) -> Result<usize, LuaError> {
    if !matches!(state.type_at(1), LuaType::None | LuaType::Nil) {
        if state.type_at(1) == LuaType::String {
            let filename = state.check_arg_string(1)?;
            opencheck(state, &filename, mode)?;
        } else {
            let _ = tofile(state)?;
            state.push_value_at(1)?;
        }
        state.registry_set(key)?;
    }
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

/// Read a numeric literal from `file` into an owned byte buffer.
///
/// The decimal point is always `.`: the reference reads the locale's
/// `decimal_point`, but omnilua is locale-independent, so `.` is the single
/// source of truth (the same simplification the rest of the number path makes).
fn read_number_bytes(file: &mut dyn LuaFileHandle) -> Vec<u8> {
    let first = loop {
        let b = file.read_byte();
        if b == EOF_SENTINEL || !(b as u8).is_ascii_whitespace() {
            break b;
        }
    };

    let mut rn = ReadNumState::new(first);

    rn.try2(file, [b'-', b'+']);

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

    count += rn.read_digits(file, hex);

    let dec_point = b'.';
    if rn.try2(file, [dec_point, b'.']) {
        count += rn.read_digits(file, hex);
    }

    if count > 0 {
        let exp_chars = if hex { [b'p', b'P'] } else { [b'e', b'E'] };
        if rn.try2(file, exp_chars) {
            rn.try2(file, [b'-', b'+']);
            rn.read_digits(file, false);
        }
    }

    file.unread_byte(rn.current);
    rn.as_bytes().to_vec()
}

/// Peek for EOF: returns `true` if more input is available. C: `test_eof`
/// (the file-only half — caller still pushes `""` regardless).
fn test_eof(file: &mut dyn LuaFileHandle) -> bool {
    let c = file.read_byte();
    if c != EOF_SENTINEL {
        file.unread_byte(c);
    }
    c != EOF_SENTINEL
}

/// Read one line from `file` into an owned buffer. Returns `(bytes, had_content)`.
/// If `chop` is true the trailing `\n` is stripped. C: `read_line(L, f, chop)`.
///
/// The bytes are accumulated in `LUAL_BUFFER_SIZE`-sized passes and the outer
/// loop continues while a pass fills without hitting a newline or EOF — the
/// chunked structure mirrors C's `luaL_prepbuffer` loop, though a growable `Vec`
/// stands in for the fixed stack buffer.
fn read_line(file: &mut dyn LuaFileHandle, chop: bool) -> (Vec<u8>, bool) {
    let mut buf: Vec<u8> = Vec::new();
    let mut c: i32;

    'outer: loop {
        for _ in 0..LUAL_BUFFER_SIZE {
            c = file.read_byte();
            if c == EOF_SENTINEL || c == b'\n' as i32 {
                break 'outer;
            }
            buf.push(c as u8);
        }
    }

    if !chop && c == b'\n' as i32 {
        buf.push(b'\n');
    }

    let had_content = c == b'\n' as i32 || !buf.is_empty();
    (buf, had_content)
}

/// Read the entire file into an owned buffer. C: `read_all(L, f)` (file-only half).
///
/// Perf: C `fread`s in bulk; this reads one byte at a time via
/// `LuaFileHandle::read_byte`. A future `read_chunk(&mut [u8])` on the trait
/// would let the host fill a buffer directly.
fn read_all(file: &mut dyn LuaFileHandle) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::new();
    loop {
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
    buf
}

/// Read at most `n` bytes from `file`. Returns `(bytes, had_content)`.
fn read_chars(file: &mut dyn LuaFileHandle, n: usize) -> (Vec<u8>, bool) {
    let mut buf = Vec::with_capacity(n);
    for _ in 0..n {
        let b = file.read_byte();
        if b == EOF_SENTINEL {
            break;
        }
        buf.push(b as u8);
    }
    let nr = buf.len();
    (buf, nr > 0)
}

/// A validated `file:read`/`io.read` string format: the canonical option byte
/// (`b'n'`/`b'l'`/`b'L'`/`b'a'`) after the leading `*` has been resolved.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ReadFormat {
    Number,
    Line,
    LineWithEol,
    All,
}

/// Whether the leading `*` on a read format is REQUIRED (5.1/5.2) or OPTIONAL
/// (5.3+). The `*` was a mandatory marker in 5.1/5.2; 5.3 kept it accepted for
/// compatibility but made it optional (`if (*p == '*') p++;` in `liolib.c`'s
/// `g_read`). Single source of truth for that seam — verified empirically
/// against the 5.1.5/5.2.4 vs 5.3.6/5.4.7/5.5.0 reference binaries.
fn read_format_requires_star(version: lua_types::LuaVersion) -> bool {
    matches!(
        version,
        lua_types::LuaVersion::V51 | lua_types::LuaVersion::V52
    )
}

/// Whether the `L` (line-with-end-of-line) format exists. `L` was added in 5.2;
/// 5.1 has only `n`/`l`/`a`, so `*L` there is an invalid format. Single source of
/// truth — verified against the 5.1.5 reference (`*L` → "invalid format") vs
/// 5.2.4+ (`*L` reads the line including its newline).
fn read_format_has_line_with_eol(version: lua_types::LuaVersion) -> bool {
    version != lua_types::LuaVersion::V51
}

/// Resolve a read-format string against the version seam, returning the
/// canonical [`ReadFormat`] or the exact `extramsg` the reference passes to
/// `luaL_argerror`.
///
/// The two reference wordings encode the seam: a leading char that is not a
/// valid format marker yields `"invalid option"`, while a recognised marker
/// followed by an unknown option yields `"invalid format"`.
///   * 5.1/5.2: the `*` is the required marker. No `*` ⇒ `"invalid option"`.
///     After `*`, an unknown/absent option char ⇒ `"invalid format"`; on 5.1
///     the `L` option does not exist, so `*L` ⇒ `"invalid format"`.
///   * 5.3+: the `*` is optional. The option char is read directly (with or
///     without a leading `*`); an unknown one ⇒ `"invalid format"`.
fn resolve_read_format(
    version: lua_types::LuaVersion,
    fmt: &[u8],
) -> Result<ReadFormat, &'static [u8]> {
    let option = if read_format_requires_star(version) {
        if fmt.first() != Some(&b'*') {
            return Err(b"invalid option");
        }
        fmt.get(1).copied()
    } else if fmt.first() == Some(&b'*') {
        fmt.get(1).copied()
    } else {
        fmt.first().copied()
    };
    match option {
        Some(b'n') => Ok(ReadFormat::Number),
        Some(b'l') => Ok(ReadFormat::Line),
        Some(b'L') if read_format_has_line_with_eol(version) => Ok(ReadFormat::LineWithEol),
        Some(b'a') => Ok(ReadFormat::All),
        _ => Err(b"invalid format"),
    }
}

/// Dispatch one or more read formats; push results. C: `g_read(L, f, first)`.
///
/// Takes an `Rc<RefCell<LStream>>` so each I/O step can borrow the file briefly,
/// release the borrow, then push the result to `state`. This is the "collect
/// then borrow" pattern that resolves the `&mut state` vs `&mut file` conflict.
fn g_read(
    state: &mut LuaState,
    p_rc: &Rc<RefCell<LStream>>,
    first: i32,
) -> Result<usize, LuaError> {
    //
    // In C, `getiofile` leaves the default stream on the stack, so subtracting
    // one skips that extra value. This Rust port resolves registry streams into
    // an Rc and pops the registry value before reaching `g_read`, so count the
    // read formats directly from `first`.
    let nargs = (state.top() - first + 1).max(0);
    let mut n = first;
    let mut success = true;

    {
        let mut p = p_rc.borrow_mut();
        let fh = p.file.as_mut().expect("open stream has no file handle");
        fh.clear_error();
    }

    if nargs == 0 {
        let (bytes, had) = {
            let mut p = p_rc.borrow_mut();
            let fh = p
                .file
                .as_deref_mut()
                .expect("open stream has no file handle");
            read_line(fh, true)
        };
        state.push_string(&bytes)?;
        success = had;
        n = first + 1;
    } else {
        state.ensure_stack((nargs as i32) + 20, "too many arguments")?;
        let mut remaining = nargs;
        while remaining > 0 && success {
            if state.type_at(n) == LuaType::Number {
                let l = state.check_arg_integer(n)? as usize;
                if l == 0 {
                    let not_eof = {
                        let mut p = p_rc.borrow_mut();
                        let fh = p
                            .file
                            .as_deref_mut()
                            .expect("open stream has no file handle");
                        test_eof(fh)
                    };
                    state.push_string(b"")?;
                    success = not_eof;
                } else {
                    let (bytes, had) = {
                        let mut p = p_rc.borrow_mut();
                        let fh = p
                            .file
                            .as_deref_mut()
                            .expect("open stream has no file handle");
                        read_chars(fh, l)
                    };
                    state.push_string(&bytes)?;
                    success = had;
                }
            } else {
                let s: Vec<u8> = state.check_arg_string(n)?;
                let version = state.global().lua_version;
                let format = match resolve_read_format(version, &s) {
                    Ok(format) => format,
                    Err(extramsg) => {
                        return Err(lua_vm::debug::arg_error_impl(state, n, extramsg));
                    }
                };
                match format {
                    ReadFormat::Number => {
                        let bytes = {
                            let mut p = p_rc.borrow_mut();
                            let fh = p
                                .file
                                .as_deref_mut()
                                .expect("open stream has no file handle");
                            read_number_bytes(fh)
                        };
                        let pushed = state.string_to_number_push(&bytes)?;
                        if pushed != 0 {
                            success = true;
                        } else {
                            state.push(LuaValue::Nil);
                            success = false;
                        }
                    }
                    ReadFormat::Line => {
                        let (bytes, had) = {
                            let mut p = p_rc.borrow_mut();
                            let fh = p
                                .file
                                .as_deref_mut()
                                .expect("open stream has no file handle");
                            read_line(fh, true)
                        };
                        state.push_string(&bytes)?;
                        success = had;
                    }
                    ReadFormat::LineWithEol => {
                        let (bytes, had) = {
                            let mut p = p_rc.borrow_mut();
                            let fh = p
                                .file
                                .as_deref_mut()
                                .expect("open stream has no file handle");
                            read_line(fh, false)
                        };
                        state.push_string(&bytes)?;
                        success = had;
                    }
                    ReadFormat::All => {
                        let bytes = {
                            let mut p = p_rc.borrow_mut();
                            let fh = p
                                .file
                                .as_deref_mut()
                                .expect("open stream has no file handle");
                            read_all(fh)
                        };
                        state.push_string(&bytes)?;
                        success = true;
                    }
                }
            }
            n += 1;
            remaining -= 1;
        }
    }

    let has_err = {
        let p = p_rc.borrow();
        match p.file.as_deref() {
            Some(fh) => fh.has_error(),
            None => false,
        }
    };
    if has_err {
        let err = {
            let p = p_rc.borrow();
            match p.file.as_deref().and_then(|fh| fh.last_error_info()) {
                Some((code, _msg)) if code != 0 => io::Error::from_raw_os_error(code),
                Some((_code, msg)) => io::Error::new(io::ErrorKind::Other, msg),
                None => io::Error::new(io::ErrorKind::Other, "file read error"),
            }
        };
        return file_result(state, false, None, err);
    }

    if !success {
        state.pop_n(1);
        state.push(LuaValue::Nil);
    }

    Ok((n - first) as usize)
}

/// Resolve the registry-default I/O file (IO_INPUT / IO_OUTPUT) into its
/// backing `Rc<RefCell<LStream>>`. Errors if the slot holds a closed handle
/// or a value that is not a registered file userdata.
///
fn get_io_file_rc(state: &mut LuaState, key: &[u8]) -> Result<Rc<RefCell<LStream>>, LuaError> {
    state.registry_get(key)?;
    let ud_id = state
        .test_arg_userdata(-1, LUA_FILE_HANDLE)
        .map(|ud| ud.identity());
    state.pop_n(1);
    let label = &key[IO_PREFIX_LEN..];
    let id = ud_id.ok_or_else(|| {
        LuaError::runtime(format_args!(
            "default {} file is invalid",
            label.escape_ascii()
        ))
    })?;
    let rc = lookup_lstream(state, id).ok_or_else(|| {
        LuaError::runtime(format_args!(
            "default {} file is invalid",
            label.escape_ascii()
        ))
    })?;
    if rc.borrow().is_closed() {
        return Err(LuaError::runtime(format_args!(
            "default {} file is closed",
            label.escape_ascii()
        )));
    }
    Ok(rc)
}

/// `io.read(...)`. C: `io_read`.
pub fn io_read(state: &mut LuaState) -> Result<usize, LuaError> {
    let p_rc = get_io_file_rc(state, IO_INPUT_KEY)?;
    g_read(state, &p_rc, 1)
}

/// `file:read(...)`. C: `f_read`.
pub fn f_read(state: &mut LuaState) -> Result<usize, LuaError> {
    let p_rc = tofile(state)?;
    g_read(state, &p_rc, 2)
}

// ── Write helpers ────────────────────────────────────────────────────────────

/// Render a numeric `LuaValue` to its `io.write` byte form.
///
/// Reference `g_write` writes numbers with `lua_tostring` — the same
/// `tostringbuff` path as `print`/`tostring` — so this routes through the
/// shared, version-aware [`lua_vm::object::num_to_string`] to keep
/// `io.write(1.0)` byte-identical to `print(1.0)` on every version: `%.14g` on
/// 5.1-5.4 (no `.0` suffix under the float-only 5.1/5.2), shortest-round-trip
/// on 5.5.
fn num_to_write_bytes(state: &mut LuaState, val: &LuaValue) -> Result<Vec<u8>, LuaError> {
    let s = lua_vm::object::num_to_string(state, val)?;
    Ok(s.as_bytes().to_vec())
}

/// Collect the write arguments `arg..=top` as owned byte chunks. Numbers are
/// stringified with `num_to_write_bytes`; everything else is arg-checked to a
/// string. Done before borrowing the file handle to keep `&mut LuaState`
/// available for the coercions.
fn collect_write_chunks(state: &mut LuaState, first_arg: i32) -> Result<Vec<Vec<u8>>, LuaError> {
    let n = state.top() as i32;
    let mut chunks: Vec<Vec<u8>> = Vec::with_capacity((n - first_arg + 1).max(0) as usize);
    for i in first_arg..=n {
        if state.type_at(i) == LuaType::Number {
            let val = state.value_at(i);
            chunks.push(num_to_write_bytes(state, &val)?);
        } else {
            let bytes: Vec<u8> = state.check_arg_string(i)?;
            chunks.push(bytes);
        }
    }
    Ok(chunks)
}

/// Write every chunk to `p_rc`'s handle, accumulating total bytes written, and
/// shape the result exactly like C's `g_write` (shared by `io.write` and
/// `file:write`).
///
/// On full success returns `Ok(None)`; the caller pushes its own success value
/// (the output file for `io.write`, the handle for `file:write`). On the first
/// short/failed write returns `Ok(Some(nres))` with the failure result already
/// on the stack: the `luaL_fileresult` tuple `(nil, msg[, errno])` and, **on
/// Lua 5.5 only**, an appended total-bytes-written counter (5.5's `g_write`
/// pushes `cast_st2S(totalbytes)` and returns `n + 1`; 5.1-5.4's `g_write` does
/// not).
fn g_write(
    state: &mut LuaState,
    p_rc: &Rc<RefCell<LStream>>,
    chunks: &[Vec<u8>],
) -> Result<Option<usize>, LuaError> {
    let mut total: usize = 0;
    let write_err: Option<io::Error> = {
        let mut p = p_rc.borrow_mut();
        let fh = p.file.as_mut().expect("open stream has no file handle");
        let mut err = None;
        for chunk in chunks {
            match fh.write_bytes(chunk) {
                Ok(written) => {
                    total += written;
                    if written < chunk.len() {
                        err = Some(io::Error::new(io::ErrorKind::Other, "short write"));
                        break;
                    }
                }
                Err(e) => {
                    err = Some(e);
                    break;
                }
            }
        }
        err
    };

    match write_err {
        None => Ok(None),
        Some(e) => {
            let mut nres = file_result(state, false, None, e)?;
            if matches!(state.global().lua_version, lua_types::LuaVersion::V55) {
                state.push(LuaValue::Int(total as i64));
                nres += 1;
            }
            Ok(Some(nres))
        }
    }
}

/// `io.write(...)`. C: `io_write` → `g_write(L, getiofile(IO_OUTPUT), 1)`.
///
/// Writes all arguments to the current default output file (`IO_OUTPUT`). A
/// write failure returns the `luaL_fileresult` tuple (like the reference), not
/// a raised error.
pub fn io_write(state: &mut LuaState) -> Result<usize, LuaError> {
    let chunks = collect_write_chunks(state, 1)?;
    let p_rc = get_io_file_rc(state, IO_OUTPUT_KEY)?;
    match g_write(state, &p_rc, &chunks)? {
        None => {
            state.registry_get(IO_OUTPUT_KEY)?;
            Ok(1)
        }
        Some(nres) => Ok(nres),
    }
}

/// `file:write(...)`. C: `f_write` → `g_write(L, tofile(L), 2)`.
pub fn f_write(state: &mut LuaState) -> Result<usize, LuaError> {
    let p_rc = tofile(state)?;
    let chunks = collect_write_chunks(state, 2)?;
    match g_write(state, &p_rc, &chunks)? {
        None => {
            state.push_value_at(1)?;
            Ok(1)
        }
        Some(nres) => Ok(nres),
    }
}

// ── Seek / setvbuf / flush ───────────────────────────────────────────────────

/// `file:seek([whence [, offset]])`. C: `f_seek`.
pub fn f_seek(state: &mut LuaState) -> Result<usize, LuaError> {
    static MODE_NAMES: &[&[u8]] = &[b"set", b"cur", b"end"];

    let p_rc = tofile(state)?;
    let op = state.check_arg_option(2, Some(b"cur"), MODE_NAMES)?;
    let p3: i64 = state.opt_arg_integer(3, 0)?;

    let seek_pos = match op {
        0 => SeekFrom::Start(p3 as u64),
        1 => SeekFrom::Current(p3),
        2 => SeekFrom::End(p3),
        _ => unreachable!(),
    };

    let result = {
        let mut p = p_rc.borrow_mut();
        let fh = p.file.as_mut().expect("open stream has no file handle");
        fh.seek(seek_pos)
    };
    match result {
        Ok(pos) => {
            state.push(LuaValue::Int(pos as i64));
            Ok(1)
        }
        Err(e) => file_result(state, false, None, e),
    }
}

/// `file:setvbuf(mode [, size])`. C: `f_setvbuf`.
pub fn f_setvbuf(state: &mut LuaState) -> Result<usize, LuaError> {
    static MODE_NAMES: &[&[u8]] = &[b"no", b"full", b"line"];

    let p_rc = tofile(state)?;
    let op = state.check_arg_option(2, None, MODE_NAMES)?;
    let sz: i64 = state.opt_arg_integer(3, LUAL_BUFFER_SIZE as i64)?;
    let mode = match op {
        0 => BufMode::No,
        1 => BufMode::Full,
        2 => BufMode::Line,
        _ => unreachable!(),
    };
    let result = {
        let mut p = p_rc.borrow_mut();
        let fh = p.file.as_mut().expect("open stream has no file handle");
        let mode_index = match mode {
            BufMode::No => 0,
            BufMode::Full => 1,
            BufMode::Line => 2,
        };
        fh.set_buf_mode(mode_index, sz.max(0) as usize)
    };
    match result {
        Ok(()) => file_result(state, true, None, io::Error::last_os_error()),
        Err(e) => file_result(state, false, None, e),
    }
}

/// `io.flush()`. C: `io_flush`.
pub fn io_flush(state: &mut LuaState) -> Result<usize, LuaError> {
    let ud_id: Option<usize> = {
        state.registry_get(IO_OUTPUT_KEY)?;
        let id = state
            .test_arg_userdata(-1, LUA_FILE_HANDLE)
            .map(|ud| ud.identity());
        state.pop_n(1);
        id
    };
    if let Some(id) = ud_id {
        if let Some(rc) = lookup_lstream(state, id) {
            let result = {
                let mut p = rc.borrow_mut();
                if p.is_closed() {
                    return Err(LuaError::runtime(format_args!(
                        "default output file is closed"
                    )));
                }
                let fh = p
                    .file
                    .as_deref_mut()
                    .expect("open stream has no file handle");
                fh.flush()
            };
            return match result {
                Ok(()) => {
                    state.push(LuaValue::Bool(true));
                    Ok(1)
                }
                Err(e) => file_result(state, false, None, e),
            };
        }
    }
    // No live default output file: behave like a successful no-op flush of stdout.
    state.push(LuaValue::Bool(true));
    Ok(1)
}

/// `file:flush()`. C: `f_flush`.
pub fn f_flush(state: &mut LuaState) -> Result<usize, LuaError> {
    let p_rc = tofile(state)?;
    let result = {
        let mut p = p_rc.borrow_mut();
        let fh = p.file.as_mut().expect("open stream has no file handle");
        fh.flush()
    };
    match result {
        Ok(()) => {
            state.push(LuaValue::Bool(true));
            Ok(1)
        }
        Err(e) => file_result(state, false, None, e),
    }
}

// ── Lines iterator ───────────────────────────────────────────────────────────

/// Build the `io_readline` closure with its upvalues and push it.
///
/// Upvalue layout (C comment):
///   1) file handle (first stack value)
///   2) number of read-format arguments
///   3) toclose flag (bool)
///   4..n+3) format arguments
fn aux_lines(state: &mut LuaState, toclose: bool) -> Result<(), LuaError> {
    // `lua_gettop` is the stack count RELATIVE to the current frame, not the
    // absolute `top_idx`; using `state.top()` mirrors that.
    let n = state.top() - 1;
    if n > MAX_ARG_LINE as i32 {
        return Err(lua_vm::debug::arg_error_impl(
            state,
            MAX_ARG_LINE as i32 + 2,
            b"too many arguments",
        ));
    }
    state.push_value_at(1)?;
    state.push(LuaValue::Int(n as i64));
    state.push(LuaValue::Bool(toclose));
    state.rotate(2, 3)?;
    state.push_c_closure(io_readline, (3 + n) as i32)?;
    Ok(())
}

/// `file:lines(...)`. C: `f_lines`.
pub fn f_lines(state: &mut LuaState) -> Result<usize, LuaError> {
    let _ = tofile(state)?; // validates file is open
    aux_lines(state, false)?;
    Ok(1)
}

/// `io.lines([filename, ...])`. C: `io_lines`.
pub fn io_lines(state: &mut LuaState) -> Result<usize, LuaError> {
    if state.type_at(1) == LuaType::None {
        state.push(LuaValue::Nil);
    }
    let toclose = if state.type_at(1) == LuaType::Nil {
        state.registry_get(IO_INPUT_KEY)?;
        state.replace(1)?;
        let _ = tofile(state)?;
        false
    } else {
        let filename = state.check_arg_string(1)?;
        opencheck(state, &filename, b"r")?;
        state.replace(1)?;
        true
    };

    aux_lines(state, toclose)?;

    if toclose && state.global().lua_version.lines_returns_to_be_closed() {
        state.push(LuaValue::Nil); // state
        state.push(LuaValue::Nil); // control
        state.push_value_at(1)?; // file as to-be-closed variable (4th result)
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
    let n = match state.value_at(crate::state_stub::upvalue_index(2)) {
        LuaValue::Int(i) => i as usize,
        _ => 0,
    };

    let p_rc = lstream_from_upvalue(state, 1)?;

    if p_rc.borrow().is_closed() {
        return Err(LuaError::runtime(format_args!("file is already closed")));
    }

    lua_vm::api::set_top(state, 1)?;
    state.ensure_stack(n as i32, "too many arguments")?;

    for i in 1..=n {
        let uv = state.value_at(crate::state_stub::upvalue_index(3 + i as i32));
        state.push(uv);
    }

    let result_n: usize = g_read(state, &p_rc, 2)?;

    debug_assert!(result_n > 0, "g_read should return at least one value");

    let top = state.top_idx().get() as i32;
    let first_result_idx = top - result_n as i32;
    let first_truthy = !matches!(
        state.stack_at(first_result_idx),
        LuaValue::Nil | LuaValue::Bool(false)
    );
    if first_truthy {
        return Ok(result_n);
    }

    if result_n > 1 {
        let err_val = state.stack_at(first_result_idx + 1).clone();
        return Err(LuaError::from_value(err_val));
    }

    let toclose = !matches!(
        state.value_at(crate::state_stub::upvalue_index(3)),
        LuaValue::Nil | LuaValue::Bool(false)
    );
    if toclose {
        lua_vm::api::set_top(state, 0)?;
        state.push_upvalue(1)?;
        aux_close(state)?;
    }

    Ok(0)
}

// ── Module registration ──────────────────────────────────────────────────────

/// Create the file-handle metatable in the registry. C: `createmeta(L)`.
fn create_meta(state: &mut LuaState) -> Result<(), LuaError> {
    state.new_metatable(LUA_FILE_HANDLE)?;
    state.set_funcs(FILE_METAMETHODS, 0)?;
    state.new_lib_table(FILE_METHODS)?;
    state.set_funcs(FILE_METHODS, 0)?;
    state.set_field(-2, b"__index")?;
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
    let cell = new_pre_file(state)?;
    let output_hook = match std_kind {
        StdFileKind::Stdout => state.global().stdout_hook,
        StdFileKind::Stderr => state.global().stderr_hook,
        StdFileKind::Stdin => None,
    };
    let input_hook = match std_kind {
        StdFileKind::Stdin => state.global().stdin_hook,
        StdFileKind::Stdout | StdFileKind::Stderr => None,
    };
    {
        let mut p = cell.borrow_mut();
        p.file = Some(Box::new(StdStreamHandle::new(
            std_kind,
            input_hook,
            output_hook,
        )));
        p.close_fn = Some(io_noclose);
    }
    if let Some(key) = registry_key {
        state.push_value_at(-1)?;
        state.registry_set(key)?;
    }
    state.set_field(-2, field_name)?;
    Ok(())
}

/// Open the `io` library and return 1 (the library table). C: `luaopen_io`.
pub fn luaopen_io(state: &mut LuaState) -> Result<usize, LuaError> {
    state.new_lib(IO_LIB)?;
    create_meta(state)?;
    create_std_file(state, StdFileKind::Stdin, Some(IO_INPUT_KEY), b"stdin")?;
    create_std_file(state, StdFileKind::Stdout, Some(IO_OUTPUT_KEY), b"stdout")?;
    create_std_file(state, StdFileKind::Stderr, None, b"stderr")?;
    Ok(1)
}
