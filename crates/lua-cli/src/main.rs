//! Standalone `lua-rs` interpreter — minimal entry point that exercises the
//! full pipeline: `new_state` → `open_libs` → `load_string` → `pcall_k`.
//!
//! This is intentionally minimal — its job is to surface which `todo!()`
//! stubs block real execution, NOT to be a complete Lua interpreter.
//!
//! Usage:
//!   lua-rs '<lua source>'
//! Examples:
//!   lua-rs 'print("hello")'
//!   lua-rs '1+1'

use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::process::ExitCode;

use lua_types::closure::{LuaClosure, LuaLClosure};
use lua_types::error::{LuaError, LuaExit, LuaThreadClose};
use lua_types::filehandle::LuaFileHandle;
use lua_types::gc::GcRef;
use lua_types::upval::UpVal;
use lua_types::value::LuaValue;
use lua_vm::state::{
    new_state, DynLibId, DynamicSymbol, GcKind, LuaState, OsExecuteReason, OsExecuteResult,
};

mod interp;
mod repl;

fn file_loader_hook(filename: &[u8]) -> Result<Vec<u8>, LuaError> {
    #[cfg(unix)]
    let path: std::path::PathBuf = {
        use std::os::unix::ffi::OsStrExt;
        std::path::PathBuf::from(std::ffi::OsStr::from_bytes(filename))
    };
    #[cfg(not(unix))]
    let path: std::path::PathBuf = {
        let s = std::str::from_utf8(filename)
            .map_err(|_| LuaError::runtime(format_args!("filename is not valid UTF-8")))?;
        std::path::PathBuf::from(s)
    };
    std::fs::read(&path).map_err(|err| {
        LuaError::runtime(format_args!(
            "cannot open '{}': {}",
            String::from_utf8_lossy(filename),
            err
        ))
    })
}

/// `std::fs::File`-backed implementation of [`LuaFileHandle`].
///
/// Wraps a `BufReader` for read paths and a `BufWriter` for write paths,
/// sharing the same underlying `std::fs::File` via cloning the handle.
/// The write wrapper is flushed on `Drop` (implicit close) so data is not
/// lost when `io.close()` drops the `Box<dyn LuaFileHandle>`.
enum FsFile {
    Read(BufReader<std::fs::File>, Option<(i32, String)>),
    Write(BufWriter<std::fs::File>, Option<(i32, String)>, FsBufMode),
    ReadWrite(std::fs::File, Option<u8>, Option<(i32, String)>),
}

/// Deterministic `/dev/full` stand-in with C stdio buffering fidelity (#305):
/// the ENOSPC surfaces where `fwrite` would surface it, pinned against the
/// reference binaries on a real `/dev/full`. Fully buffered (the stdio
/// default) accepts writes and fails at `flush`/close; unbuffered fails
/// immediately with 0 bytes written. Line-buffered mirrors glibc's stateful
/// `fwrite` byte accounting:
///
/// * The FIRST write on a fresh stream goes byte-at-a-time (glibc's
///   `_IO_default_xsputn` path before `_IO_CURRENTLY_PUTTING` is set), so it
///   counts the bytes before the FIRST newline and fails there —
///   `f:write("abc\n")` → `nil, "No space left on device", 28, 3` and
///   `f:write("ab\ncd\nef")` → counter 2.
/// * SUBSEQUENT writes take the bulk-scan path: bytes through the LAST
///   newline are consumed before the flush fails — `f:write("abc", "de\nf")`
///   → counter 6 (3 + `"de\n"`). When the data ENDS at that newline glibc's
///   `fwrite` reports 0 for the call — `f:write("abc", "de\n")` → counter 3.
/// * A flush attempt (newline-triggered or explicit) DISCARDS the buffer
///   whether or not it succeeds (`new_do_write` resets the write pointers
///   unconditionally), so `flush`/close only fail while undischarged bytes
///   are buffered: `f:write("abc\n"); f:close()` → `true`, but
///   `f:write("abc"); f:flush()` → the ENOSPC triple.
///
/// `pending_fail` bridges the seam between C's one-`fwrite`-per-argument and
/// [`g_write`]'s continuation loop: a newline-triggered flush failure returns
/// the glibc partial count and arms one `Err(ENOSPC)` for the retry that
/// `g_write` is guaranteed to issue for the unwritten remainder, so the loop
/// stops with exactly the reference totalbytes.
struct DevFullFile {
    errored: bool,
    buf_mode: FsBufMode,
    putting: bool,
    pending_fail: bool,
    dirty: bool,
}

#[derive(Clone, Copy)]
enum FsBufMode {
    No,
    Full,
    Line,
}

impl FsFile {
    fn open(filename: &[u8], mode: &[u8]) -> io::Result<Self> {
        #[cfg(unix)]
        let path: std::path::PathBuf = {
            use std::os::unix::ffi::OsStrExt;
            std::path::PathBuf::from(std::ffi::OsStr::from_bytes(filename))
        };
        #[cfg(not(unix))]
        let path: std::path::PathBuf = {
            let s = std::str::from_utf8(filename).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "filename not valid UTF-8")
            })?;
            std::path::PathBuf::from(s)
        };

        let first = mode.first().copied().unwrap_or(b'r');
        let update = mode.get(1).copied() == Some(b'+');

        if first != b'r' {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    let _ = std::fs::create_dir_all(parent);
                }
            }
        }

        match (first, update) {
            (b'r', false) => {
                let f = std::fs::File::open(&path)?;
                Ok(FsFile::Read(BufReader::new(f), None))
            }
            (b'w', false) => {
                let f = std::fs::File::create(&path)?;
                Ok(FsFile::Write(BufWriter::new(f), None, FsBufMode::Full))
            }
            (b'a', false) => {
                let mut f = std::fs::OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open(&path)?;
                f.seek(SeekFrom::End(0))?;
                Ok(FsFile::Write(BufWriter::new(f), None, FsBufMode::Full))
            }
            _ => {
                let f = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(first == b'w' || first == b'a')
                    .truncate(first == b'w')
                    .append(first == b'a')
                    .open(&path)?;
                Ok(FsFile::ReadWrite(f, None, None))
            }
        }
    }
}

/// Capture a read/write-error's `(errno, message)` for `last_error_info`.
///
/// The `unwrap_or(0)` is safe here (not a #301 errno-0 fallback): the io read
/// path treats a `0` code as "no OS errno" and rebuilds a `raw_os_error`-less
/// `io::Error` (`ErrorKind::Other`) before it reaches `file_result`, so a
/// missing errno surfaces to Lua as the honest 2-value `(nil, msg)`, never a
/// fabricated errno `0`. A real read/write syscall failure always carries a
/// non-zero `raw_os_error()`, which flows through as the faithful errno.
fn io_error_info(err: &io::Error) -> (i32, String) {
    (err.raw_os_error().unwrap_or(0), err.to_string())
}

impl LuaFileHandle for FsFile {
    /// Read one byte, or `-1` on EOF/error. The `Write` arm attempts the read
    /// against the real write-only descriptor so a wrong-mode read reports the
    /// OS's faithful errno (`EBADF`, "Bad file descriptor") that C's `fread`
    /// yields — a synthetic error would carry no `raw_os_error()` and reach Lua
    /// as errno-less (#301).
    fn read_byte(&mut self) -> i32 {
        match self {
            FsFile::Read(r, err) => {
                let mut buf = [0u8; 1];
                match r.read(&mut buf) {
                    Ok(1) => buf[0] as i32,
                    Ok(_) => -1,
                    Err(e) => {
                        *err = Some(io_error_info(&e));
                        -1
                    }
                }
            }
            FsFile::ReadWrite(f, pushback, err) => {
                if let Some(b) = pushback.take() {
                    return b as i32;
                }
                let mut buf = [0u8; 1];
                match f.read(&mut buf) {
                    Ok(1) => buf[0] as i32,
                    Ok(_) => -1,
                    Err(e) => {
                        *err = Some(io_error_info(&e));
                        -1
                    }
                }
            }
            FsFile::Write(w, err, _) => {
                let mut buf = [0u8; 1];
                let mut fref: &std::fs::File = w.get_ref();
                match fref.read(&mut buf) {
                    Ok(1) => buf[0] as i32,
                    Ok(_) => -1,
                    Err(e) => {
                        *err = Some(io_error_info(&e));
                        -1
                    }
                }
            }
        }
    }

    fn unread_byte(&mut self, byte: i32) {
        match self {
            FsFile::Read(r, _) => {
                if byte >= 0 {
                    let _ = r.seek_relative(-1);
                }
            }
            FsFile::ReadWrite(_, pushback, _) => {
                if byte >= 0 {
                    *pushback = Some(byte as u8);
                }
            }
            FsFile::Write(_, _, _) => {}
        }
    }

    /// Write a byte slice. The `Read` arm attempts the write against the real
    /// read-only descriptor for the same errno-fidelity reason as
    /// [`read_byte`](Self::read_byte): the OS yields `EBADF` rather than a
    /// synthetic, `raw_os_error`-less error (#301).
    fn write_bytes(&mut self, data: &[u8]) -> io::Result<usize> {
        match self {
            FsFile::Write(w, _, mode) => {
                let n = w.write(data)?;
                match mode {
                    FsBufMode::No => w.flush()?,
                    FsBufMode::Line if data[..n].contains(&b'\n') => w.flush()?,
                    FsBufMode::Line | FsBufMode::Full => {}
                }
                Ok(n)
            }
            FsFile::ReadWrite(f, _, _) => f.write(data),
            FsFile::Read(r, _) => {
                let mut fref: &std::fs::File = r.get_ref();
                fref.write(data)
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            FsFile::Write(w, _, _) => w.flush(),
            FsFile::ReadWrite(f, _, _) => f.flush(),
            FsFile::Read(_, _) => Ok(()),
        }
    }

    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        match self {
            FsFile::Read(r, _) => r.seek(pos),
            FsFile::Write(w, _, _) => w.seek(pos),
            FsFile::ReadWrite(f, _, _) => f.seek(pos),
        }
    }

    fn tell(&mut self) -> io::Result<u64> {
        self.seek(SeekFrom::Current(0))
    }

    fn clear_error(&mut self) {
        match self {
            FsFile::Read(_, err) => *err = None,
            FsFile::Write(_, err, _) => *err = None,
            FsFile::ReadWrite(_, _, err) => *err = None,
        }
    }

    fn has_error(&self) -> bool {
        match self {
            FsFile::Read(_, err) => err.is_some(),
            FsFile::Write(_, err, _) => err.is_some(),
            FsFile::ReadWrite(_, _, err) => err.is_some(),
        }
    }

    fn last_error_info(&self) -> Option<(i32, String)> {
        match self {
            FsFile::Read(_, err) => err.clone(),
            FsFile::ReadWrite(_, _, err) => err.clone(),
            FsFile::Write(_, err, _) => err.clone(),
        }
    }

    fn set_buf_mode(&mut self, mode: i32, _size: usize) -> io::Result<()> {
        if let FsFile::Write(w, _, current) = self {
            w.flush()?;
            *current = match mode {
                0 => FsBufMode::No,
                1 => FsBufMode::Full,
                2 => FsBufMode::Line,
                _ => FsBufMode::Full,
            };
        }
        Ok(())
    }
}

impl LuaFileHandle for DevFullFile {
    fn read_byte(&mut self) -> i32 {
        -1
    }

    fn unread_byte(&mut self, _byte: i32) {}

    fn write_bytes(&mut self, data: &[u8]) -> io::Result<usize> {
        match self.buf_mode {
            FsBufMode::No => {
                self.errored = true;
                Err(io::Error::from_raw_os_error(28))
            }
            FsBufMode::Line => {
                if self.pending_fail {
                    self.pending_fail = false;
                    self.errored = true;
                    return Err(io::Error::from_raw_os_error(28));
                }
                let first_write = !self.putting;
                self.putting = true;
                let newline = if first_write {
                    data.iter().position(|&b| b == b'\n')
                } else {
                    data.iter().rposition(|&b| b == b'\n')
                };
                match newline {
                    None => {
                        self.dirty = true;
                        Ok(data.len())
                    }
                    Some(p) if first_write && p == 0 => {
                        self.errored = true;
                        self.dirty = false;
                        Err(io::Error::from_raw_os_error(28))
                    }
                    Some(p) if !first_write && p + 1 == data.len() => {
                        self.errored = true;
                        self.dirty = false;
                        Err(io::Error::from_raw_os_error(28))
                    }
                    Some(p) => {
                        self.pending_fail = true;
                        self.dirty = false;
                        Ok(if first_write { p } else { p + 1 })
                    }
                }
            }
            FsBufMode::Full => {
                self.dirty = true;
                Ok(data.len())
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.dirty {
            self.dirty = false;
            self.errored = true;
            Err(io::Error::from_raw_os_error(28))
        } else {
            Ok(())
        }
    }

    fn seek(&mut self, _pos: SeekFrom) -> io::Result<u64> {
        Ok(0)
    }

    fn tell(&mut self) -> io::Result<u64> {
        Ok(0)
    }

    fn clear_error(&mut self) {
        self.errored = false;
    }

    fn has_error(&self) -> bool {
        self.errored
    }

    fn set_buf_mode(&mut self, mode: i32, _size: usize) -> io::Result<()> {
        self.buf_mode = match mode {
            0 => FsBufMode::No,
            2 => FsBufMode::Line,
            _ => FsBufMode::Full,
        };
        Ok(())
    }
}

impl Drop for FsFile {
    fn drop(&mut self) {
        if let FsFile::Write(w, _, _) = self {
            let _ = w.flush();
        }
    }
}

/// Removes a file or directory, mirroring C's `remove(3)` so the reported
/// errno matches the reference (#301). `lua-stdlib`'s `os.remove` renders the
/// `strerror`-shaped message and real errno itself via `io_lib::file_result`;
/// this hook must not pre-format a message or re-wrap the error, or the errno
/// is lost.
///
/// BSD/Darwin `remove(3)` is `lstat(path); S_ISDIR ? rmdir : unlink` — it
/// decides by the path's OWN type (`lstat`, not following symlinks) and
/// returns the `lstat` error if that fails. The previous
/// `remove_file(path).or_else(|_| remove_dir(path))` clobbered the real
/// `unlink` errno with a spurious `rmdir` one: `os.remove("/tmp")` (a symlink)
/// reported `ENOTDIR` from the fallback `rmdir` where C reports `EPERM` from
/// `unlink`. The `lstat`-first form matches Darwin exactly, and also matches
/// glibc's `unlink`-first `remove(3)` on the errno for every case (symlink,
/// directory, file, and missing path).
fn file_remove_hook(filename: &[u8]) -> io::Result<()> {
    #[cfg(unix)]
    let path: std::path::PathBuf = {
        use std::os::unix::ffi::OsStrExt;
        std::path::PathBuf::from(std::ffi::OsStr::from_bytes(filename))
    };
    #[cfg(not(unix))]
    let path: std::path::PathBuf = {
        let s = std::str::from_utf8(filename)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "filename is not valid UTF-8"))?;
        std::path::PathBuf::from(s)
    };
    let meta = std::fs::symlink_metadata(&path)?;
    if meta.is_dir() {
        std::fs::remove_dir(&path)
    } else {
        std::fs::remove_file(&path)
    }
}

/// Renames a file, preserving `raw_os_error()` end to end (#301). See
/// [`file_remove_hook`] for why no message is pre-formatted here.
fn file_rename_hook(from: &[u8], to: &[u8]) -> io::Result<()> {
    fn to_path(bytes: &[u8]) -> io::Result<std::path::PathBuf> {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            Ok(std::path::PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
        }
        #[cfg(not(unix))]
        {
            let s = std::str::from_utf8(bytes).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "filename is not valid UTF-8")
            })?;
            Ok(std::path::PathBuf::from(s))
        }
    }
    let from_path = to_path(from)?;
    let to_path_buf = to_path(to)?;
    std::fs::rename(&from_path, &to_path_buf)
}

/// Opens a file, preserving `raw_os_error()` end to end (#301). See
/// [`file_remove_hook`] for why no message is pre-formatted here.
fn file_open_hook(filename: &[u8], mode: &[u8]) -> io::Result<Box<dyn LuaFileHandle>> {
    if filename == b"/dev/full" {
        return Ok(Box::new(DevFullFile {
            errored: false,
            buf_mode: FsBufMode::Full,
            putting: false,
            pending_fail: false,
            dirty: false,
        }));
    }
    FsFile::open(filename, mode).map(|f| Box::new(f) as Box<dyn LuaFileHandle>)
}

fn stdout_hook(bytes: &[u8]) -> io::Result<()> {
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    handle.write_all(bytes)?;
    handle.flush()
}

fn stderr_hook(bytes: &[u8]) -> io::Result<()> {
    let stderr = std::io::stderr();
    let mut handle = stderr.lock();
    handle.write_all(bytes)?;
    handle.flush()
}

fn stdin_hook(buf: &mut [u8]) -> io::Result<usize> {
    std::io::stdin().lock().read(buf)
}

fn env_hook(name: &[u8]) -> Option<Vec<u8>> {
    #[cfg(unix)]
    {
        use std::ffi::OsStr;
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        std::env::var_os(OsStr::from_bytes(name)).map(|v| v.into_vec())
    }

    #[cfg(not(unix))]
    {
        let name = std::str::from_utf8(name).ok()?;
        std::env::var(name).ok().map(|v| v.into_bytes())
    }
}

fn unix_time_hook() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Monotonic baseline for `os.clock`, captured at process start by `main`.
static CLI_START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// Program CPU time for `os.clock`, reported as monotonic wall time elapsed since
/// process start. `std` exposes no `CLOCK_PROCESS_CPUTIME_ID` equivalent, so this
/// matches the emulation wasi-libc and Emscripten use for C's `clock()`: faithful
/// for elapsed-time benchmarking, but it counts idle wall time (e.g. at an
/// interactive prompt) that true CPU time would not.
fn cpu_clock_hook() -> f64 {
    CLI_START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_secs_f64()
}

/// Returns the host's local timezone offset (seconds east of UTC) at instant
/// `t`, matching C `os.date`/`os.time` local-time semantics (DST included).
///
/// Unix reproduces C directly: `localtime_r` fills a `struct tm` whose
/// `tm_gmtoff` field is the offset. Windows' MSVCRT `struct tm` has no
/// `tm_gmtoff` (and libc exposes no `localtime_r`), so the Windows arm below
/// derives the same value by decomposing `t` as both local and UTC and
/// differencing the two wall clocks.
#[cfg(not(windows))]
fn local_offset_hook(t: i64) -> i64 {
    // SAFETY: `localtime_r` writes a fully-initialised `struct tm` into the
    // stack-allocated `tm` and returns a pointer to it (or null on failure).
    // We pass valid pointers to `t` and `tm`; nothing escapes the call. On a
    // null return (overflow / unrepresentable) we leave `tm` zeroed and report
    // offset 0 (UTC), the safe degenerate matching the no-hook path.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        let tt = t as libc::time_t;
        if libc::localtime_r(&tt, &mut tm).is_null() {
            0
        } else {
            tm.tm_gmtoff as i64
        }
    }
}

/// One wall-clock decomposition of an instant — the `struct tm` subset the
/// Windows local-offset derivation reads. `year` counts from 1900
/// (`tm_year`); `yday` is 0-based day-of-year (`tm_yday`).
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Clone, Copy)]
struct WallClock {
    year: i32,
    yday: i32,
    hour: i32,
    min: i32,
    sec: i32,
}

/// Seconds of `loc` minus `utc`, both decompositions of the SAME instant —
/// the `tm_gmtoff` substitute for platforms whose `struct tm` lacks the
/// field. A real timezone offset is within ±24h, so the day component is at
/// most ±1: when the years differ the two wall clocks straddle Jan 1 and the
/// day delta is exactly ±1 in the direction of the newer year (the raw
/// `tm_yday` difference would report a bogus ±364/365 there); within one
/// year it is the `tm_yday` difference. Compiled (and unit-tested) on every
/// platform; only the Windows [`local_offset_hook`] calls it in production.
#[cfg_attr(not(windows), allow(dead_code))]
fn wall_clock_offset_seconds(loc: WallClock, utc: WallClock) -> i64 {
    let days = if loc.year != utc.year {
        if loc.year > utc.year {
            1
        } else {
            -1
        }
    } else {
        i64::from(loc.yday - utc.yday)
    };
    days * 86_400
        + i64::from(loc.hour - utc.hour) * 3_600
        + i64::from(loc.min - utc.min) * 60
        + i64::from(loc.sec - utc.sec)
}

/// Windows local timezone offset. MSVCRT's libc exposes neither `localtime_r`
/// nor a `tm_gmtoff` field nor `mktime`, so we cannot read the offset directly.
/// Instead we decompose the same instant `t` two ways — `_localtime64_s`
/// (local wall clock) and `_gmtime64_s` (UTC wall clock) — and return their
/// difference via [`wall_clock_offset_seconds`].
#[cfg(windows)]
fn local_offset_hook(t: i64) -> i64 {
    fn to_wall(tm: &libc::tm) -> WallClock {
        WallClock {
            year: tm.tm_year,
            yday: tm.tm_yday,
            hour: tm.tm_hour,
            min: tm.tm_min,
            sec: tm.tm_sec,
        }
    }
    // SAFETY: `_localtime64_s`/`_gmtime64_s` each write a fully-initialised
    // `struct tm` into the stack-allocated slot and return 0 on success. We
    // pass valid pointers; nothing escapes the calls. On any error return we
    // report offset 0 (UTC), the safe degenerate matching the no-hook path.
    //
    // We call the underscore-prefixed 64-bit variants directly rather than
    // `libc::localtime_s`/`gmtime_s` because MSVCRT exports only
    // `_localtime64_s`/`_gmtime64_s` as linker symbols; the unprefixed names
    // are inline CRT-header wrappers and are never present in any import
    // library, causing LNK2019 "unresolved external symbol" at link time.
    unsafe {
        extern "C" {
            fn _localtime64_s(tm: *mut libc::tm, time: *const libc::time_t) -> libc::c_int;
            fn _gmtime64_s(tm: *mut libc::tm, time: *const libc::time_t) -> libc::c_int;
        }
        let tt = t as libc::time_t;
        let mut loc: libc::tm = std::mem::zeroed();
        let mut utc: libc::tm = std::mem::zeroed();
        if _localtime64_s(&mut loc, &tt) != 0 || _gmtime64_s(&mut utc, &tt) != 0 {
            return 0;
        }
        wall_clock_offset_seconds(to_wall(&loc), to_wall(&utc))
    }
}

fn entropy_hook() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    nanos ^ ((std::process::id() as u64) << 32)
}

fn temp_name_hook() -> Result<Vec<u8>, LuaError> {
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let mut path: Vec<u8> = {
        let tmp = std::env::temp_dir();
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            tmp.as_os_str().as_bytes().to_vec()
        }
        #[cfg(not(unix))]
        {
            tmp.to_string_lossy().as_bytes().to_vec()
        }
    };
    if path.last().copied() != Some(b'/') && path.last().copied() != Some(b'\\') {
        path.push(b'/');
    }

    let unique = format!(
        "lua_rs_{:x}_{:x}_{:x}",
        std::process::id(),
        entropy_hook(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    path.extend_from_slice(unique.as_bytes());
    Ok(path)
}

/// The platform shell invocation behind `os.execute`/`io.popen`, mirroring
/// what C's `system(3)`/`popen(3)` do: `/bin/sh -c <cmd>` on POSIX.
#[cfg(not(windows))]
fn shell_command(cmd_str: &str) -> std::process::Command {
    let mut command = std::process::Command::new("/bin/sh");
    command.arg("-c").arg(cmd_str);
    command
}

/// The platform shell invocation behind `os.execute`/`io.popen`, mirroring
/// what the MSVC CRT's `system`/`_popen` do: run `%COMSPEC% /C <cmd>`, with
/// `cmd.exe` when `COMSPEC` is unset (the CRT's own contract). The command
/// text is appended via `raw_arg` because `cmd.exe` does not parse the MSVC
/// quoting rules `Command`'s default escaper targets — the CRT hands the
/// string to the shell verbatim, and so do we.
#[cfg(windows)]
fn shell_command(cmd_str: &str) -> std::process::Command {
    use std::os::windows::process::CommandExt;
    let comspec = std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into());
    let mut command = std::process::Command::new(comspec);
    command.arg("/C");
    command.raw_arg(cmd_str);
    command
}

fn os_execute_hook(cmd: &[u8]) -> Result<OsExecuteResult, LuaError> {
    let cmd_str = std::str::from_utf8(cmd)
        .map_err(|_| LuaError::runtime(format_args!("os.execute command not valid UTF-8")))?;
    let status = shell_command(cmd_str)
        .status()
        .map_err(|err| LuaError::runtime(format_args!("os.execute failed: {}", err)))?;

    if let Some(code) = status.code() {
        return Ok(OsExecuteResult {
            success: status.success(),
            reason: OsExecuteReason::Exit,
            code,
        });
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return Ok(OsExecuteResult {
                success: false,
                reason: OsExecuteReason::Signal,
                code: signal,
            });
        }
    }

    Ok(OsExecuteResult {
        success: false,
        reason: OsExecuteReason::Exit,
        code: -1,
    })
}

/// `LuaFileHandle` backed by a `std::process::Child` and one of its pipes.
///
/// Mirrors POSIX `popen(3)` semantics: mode `"r"` exposes the child's stdout
/// as a readable stream, mode `"w"` exposes the child's stdin as a writable
/// stream. The pipe is stored as `Option<_>` so the closing path can take()
/// it before calling `wait()`, ensuring the child sees EOF and exits — if
/// the BufWriter/BufReader were left in place across `wait()`, write-mode
/// children like `cat` or `wc` would block indefinitely.
enum PopenFile {
    Read(
        Option<BufReader<std::process::ChildStdout>>,
        std::process::Child,
    ),
    Write(
        Option<BufWriter<std::process::ChildStdin>>,
        std::process::Child,
    ),
}

impl PopenFile {
    fn spawn(cmd: &[u8], mode: &[u8]) -> io::Result<Self> {
        let cmd_str = std::str::from_utf8(cmd).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "popen command not valid UTF-8")
        })?;
        let read_mode = match mode {
            b"r" => true,
            b"w" => false,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "popen mode must be \"r\" or \"w\"",
                ));
            }
        };
        let mut command = shell_command(cmd_str);
        if read_mode {
            command.stdout(std::process::Stdio::piped());
        } else {
            command.stdin(std::process::Stdio::piped());
        }
        let mut child = command.spawn()?;
        if read_mode {
            let out = child.stdout.take().ok_or_else(|| {
                io::Error::new(io::ErrorKind::Other, "popen: child stdout was None")
            })?;
            Ok(PopenFile::Read(Some(BufReader::new(out)), child))
        } else {
            let inp = child.stdin.take().ok_or_else(|| {
                io::Error::new(io::ErrorKind::Other, "popen: child stdin was None")
            })?;
            Ok(PopenFile::Write(Some(BufWriter::new(inp)), child))
        }
    }
}

impl LuaFileHandle for PopenFile {
    fn read_byte(&mut self) -> i32 {
        match self {
            PopenFile::Read(Some(r), _) => {
                let mut buf = [0u8; 1];
                match r.read(&mut buf) {
                    Ok(1) => buf[0] as i32,
                    _ => -1,
                }
            }
            _ => -1,
        }
    }

    fn unread_byte(&mut self, _byte: i32) {}

    fn write_bytes(&mut self, data: &[u8]) -> io::Result<usize> {
        match self {
            PopenFile::Write(Some(w), _) => w.write(data),
            _ => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "popen pipe not open for writing",
            )),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            PopenFile::Write(Some(w), _) => w.flush(),
            _ => Ok(()),
        }
    }

    fn seek(&mut self, _pos: SeekFrom) -> io::Result<u64> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "popen pipe is not seekable",
        ))
    }

    fn tell(&mut self) -> io::Result<u64> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "popen pipe is not seekable",
        ))
    }

    fn clear_error(&mut self) {}

    fn has_error(&self) -> bool {
        false
    }
}

impl Drop for PopenFile {
    fn drop(&mut self) {
        match self {
            PopenFile::Read(reader, child) => {
                drop(reader.take());
                let _ = child.wait();
            }
            PopenFile::Write(writer, child) => {
                if let Some(mut w) = writer.take() {
                    let _ = w.flush();
                }
                let _ = child.wait();
            }
        }
    }
}

fn popen_hook(cmd: &[u8], mode: &[u8]) -> Result<Box<dyn LuaFileHandle>, LuaError> {
    PopenFile::spawn(cmd, mode)
        .map(|p| Box::new(p) as Box<dyn LuaFileHandle>)
        .map_err(|err| {
            LuaError::runtime(format_args!(
                "cannot popen '{}': {}",
                String::from_utf8_lossy(cmd),
                err
            ))
        })
}

// ─── Dynamic library backend ──────────────────────────────────────────────────
//
// `lua-stdlib` cannot use `libloading` because it forbids `unsafe`. The CLI
// owns a per-process registry of loaded libraries and exposes it to
// `package.loadlib` via three function-pointer hooks on `GlobalState`. The
// registry is a `thread_local!` because `lua-rs` is single-threaded by
// construction; `libloading::Library` is not `Sync`. Libraries are leaked
// for the lifetime of the process so any function pointer resolved from
// them stays valid — that's `libloading`'s safety model.

thread_local! {
    static DYNLIB_REGISTRY: std::cell::RefCell<Vec<libloading::Library>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

fn path_from_bytes(path: &[u8]) -> Result<std::path::PathBuf, LuaError> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(std::path::PathBuf::from(std::ffi::OsStr::from_bytes(path)))
    }
    #[cfg(not(unix))]
    {
        let s = std::str::from_utf8(path)
            .map_err(|_| LuaError::runtime(format_args!("library path is not valid UTF-8")))?;
        Ok(std::path::PathBuf::from(s))
    }
}

/// `dynlib_load_hook` backend. Loads the library via `libloading` and stashes
/// it in the per-thread registry; the returned [`DynLibId`] is the
/// registry-vector index, which keeps the library alive until process exit.
///
/// A missing path is reported as `LuaError::File`, which `lua-stdlib`'s
/// `package.loadlib` maps to the `"absent"` failure tag (this build behaves
/// like C-Lua's no-dlfcn fallback for plain file-not-found, and like
/// POSIX/Windows dlopen for every other open failure).
fn dynlib_load(
    _state: &mut LuaState,
    path: &[u8],
    _see_global: bool,
) -> Result<DynLibId, LuaError> {
    let p = path_from_bytes(path)?;
    if !p.exists() {
        return Err(LuaError::File);
    }
    // SAFETY: `libloading::Library::new` executes the dynamic linker, which
    // may run arbitrary initializer code in the loaded library. We trust the
    // operator-supplied path; this is the same trust model as stock Lua's
    // `package.loadlib`. We never call any symbol from the library through
    // an unchecked ABI: only `DynamicSymbol::RustNative` is invoked, and
    // those must match our Rust function-pointer ABI exactly. Libraries are
    // stored in `DYNLIB_REGISTRY` and never unloaded mid-state, so symbol
    // pointers resolved later stay valid for as long as the state can call
    // them.
    let lib = unsafe { libloading::Library::new(&p) }.map_err(|err| {
        LuaError::runtime(format_args!(
            "cannot load '{}': {}",
            String::from_utf8_lossy(path),
            err
        ))
    })?;
    let id = DYNLIB_REGISTRY.with(|reg| {
        let mut v = reg.borrow_mut();
        let idx = v.len() as u64;
        v.push(lib);
        idx
    });
    Ok(DynLibId(id))
}

/// Conservative heuristic: stock Lua C ABI module entry points are named
/// `luaopen_<name>` and take a `lua_State *` followed by a single return.
/// Without a way to inspect the symbol's signature at run time, we treat any
/// symbol whose name starts with `luaopen_` as a C-ABI symbol and refuse it
/// with the "ABI not supported" message; everything else is treated as a
/// Rust-native entry compatible with `fn(&mut LuaState) -> Result<usize,
/// LuaError>`.
fn looks_like_c_abi(sym: &[u8]) -> bool {
    sym.starts_with(b"luaopen_")
}

/// `dynlib_symbol_hook` backend. Resolves `symbol` in the library identified
/// by `handle`; returns `RustNative` for non-`luaopen_*` symbols and
/// `LuaCAbi` (a null pointer placeholder) for `luaopen_*` symbols so
/// `package.loadlib` can refuse them with a clear `"init"` error.
fn dynlib_symbol(
    _state: &mut LuaState,
    handle: DynLibId,
    symbol: &[u8],
) -> Result<DynamicSymbol, LuaError> {
    let idx = handle.0 as usize;
    DYNLIB_REGISTRY.with(|reg| {
        let v = reg.borrow();
        let lib = v
            .get(idx)
            .ok_or_else(|| LuaError::runtime(format_args!("invalid dynlib handle {}", idx)))?;

        if looks_like_c_abi(symbol) {
            // SAFETY: We only resolve the symbol address; we never call
            // through this pointer. The `DynamicSymbol::LuaCAbi` variant is
            // a placeholder so `package.loadlib` can report an "init"
            // failure with the unsupported-ABI message. The library outlives
            // the pointer because `DYNLIB_REGISTRY` retains it for the
            // process lifetime.
            let resolved: Result<libloading::Symbol<unsafe extern "C" fn()>, _> =
                unsafe { lib.get(symbol) };
            return match resolved {
                Ok(_) => Ok(DynamicSymbol::LuaCAbi(std::ptr::null())),
                Err(err) => Err(LuaError::runtime(format_args!(
                    "cannot find symbol '{}': {}",
                    String::from_utf8_lossy(symbol),
                    err
                ))),
            };
        }

        type RustNativeFn = fn(&mut LuaState) -> Result<usize, LuaError>;
        // SAFETY: We assume the loaded library was built against this build's
        // Rust-native module ABI: it exports `symbol` as a function pointer
        // with signature `fn(&mut LuaState) -> Result<usize, LuaError>`.
        // Verified by convention (operator-supplied path + opt-in `_rs`
        // suffix); calling a symbol with the wrong signature is undefined
        // behaviour and the operator's responsibility. The library outlives
        // the function pointer (kept alive in `DYNLIB_REGISTRY` until
        // process exit).
        let resolved: Result<libloading::Symbol<RustNativeFn>, _> = unsafe { lib.get(symbol) };
        match resolved {
            Ok(sym) => Ok(DynamicSymbol::RustNative(*sym)),
            Err(err) => Err(LuaError::runtime(format_args!(
                "cannot find symbol '{}': {}",
                String::from_utf8_lossy(symbol),
                err
            ))),
        }
    })
}

/// `dynlib_unload_hook` backend. No-op: libraries are kept alive for the
/// lifetime of the process to honour `libloading`'s safety model (symbol
/// pointers must not outlive the library). Closing libraries at state
/// shutdown is platform-dependent and best deferred to OS-level cleanup.
fn dynlib_unload(_handle: DynLibId) {}

/// `LUA_RS_LIST_BYTECODE=1 lua-rs file.lua` prints one uppercase mnemonic per
/// emitted instruction, per function (depth-first), then exits before
/// execution. Consumed by `harness/bench/bytecode-parity.py`, which diffs the
/// stream against `luac -l -l` to gate codegen parity with C-Lua
/// (PERF_PUSH_SPEC W2.4 / P2.3).
fn list_bytecode(proto: &lua_types::proto::LuaProto, path: &mut Vec<usize>) {
    let label: Vec<String> = path.iter().map(|i| i.to_string()).collect();
    println!("function {}", label.join("."));
    for ins in &proto.code {
        let raw = ins.0 & 0x7f;
        match lua_vm::vm::OpCode::from_u32(raw) {
            Some(op) => println!("{}", format!("{op:?}").to_uppercase()),
            None => println!("UNKNOWN_{raw}"),
        }
    }
    for (i, sub) in proto.p.iter().enumerate() {
        path.push(i);
        list_bytecode(sub, path);
        path.pop();
    }
}

fn parser_hook(
    state: &mut LuaState,
    z: &mut lua_vm::zio::ZIO,
    name: &[u8],
    firstchar: i32,
) -> Result<GcRef<LuaLClosure>, LuaError> {
    let proto = lua_parse::parse(
        state,
        lua_parse::DynData::default(),
        z,
        name,
        firstchar,
    )?;
    if std::env::var_os("LUA_RS_LIST_BYTECODE").is_some() {
        list_bytecode(&proto, &mut vec![0]);
        std::process::exit(0);
    }
    let nupvals = proto.upvalues.len();
    let mut upvals = Vec::with_capacity(nupvals);
    for _ in 0..nupvals {
        upvals.push(std::cell::Cell::new(GcRef::new(UpVal::closed(
            LuaValue::Nil,
        ))));
    }
    let proto_ref = GcRef::new(*proto);
    proto_ref.account_buffer(proto_ref.buffer_bytes() as isize);
    let closure = GcRef::new(LuaLClosure {
        proto: proto_ref,
        upvals: upvals.into_boxed_slice(),
    });
    closure.account_buffer(closure.buffer_bytes() as isize);
    Ok(closure)
}

fn testc_push_string(state: &mut LuaState, bytes: &[u8]) -> Result<(), LuaError> {
    let s = state.intern_str(bytes)?;
    state.push(LuaValue::Str(s));
    Ok(())
}

fn testc_gc_age(value: LuaValue) -> Option<lua_gc::GcAge> {
    match value {
        LuaValue::Str(v) => Some(v.0.age()),
        LuaValue::Table(v) => Some(v.0.age()),
        LuaValue::Function(LuaClosure::Lua(v)) => Some(v.0.age()),
        LuaValue::Function(LuaClosure::C(v)) => Some(v.0.age()),
        LuaValue::UserData(v) => Some(v.0.age()),
        LuaValue::Thread(v) => Some(v.0.age()),
        LuaValue::Nil
        | LuaValue::Bool(_)
        | LuaValue::Int(_)
        | LuaValue::Float(_)
        | LuaValue::LightUserData(_)
        | LuaValue::Function(LuaClosure::LightC(_)) => None,
    }
}

fn testc_gc_color(value: LuaValue) -> Option<lua_gc::Color> {
    match value {
        LuaValue::Str(v) => Some(v.0.color()),
        LuaValue::Table(v) => Some(v.0.color()),
        LuaValue::Function(LuaClosure::Lua(v)) => Some(v.0.color()),
        LuaValue::Function(LuaClosure::C(v)) => Some(v.0.color()),
        LuaValue::UserData(v) => Some(v.0.color()),
        LuaValue::Thread(v) => Some(v.0.color()),
        LuaValue::Nil
        | LuaValue::Bool(_)
        | LuaValue::Int(_)
        | LuaValue::Float(_)
        | LuaValue::LightUserData(_)
        | LuaValue::Function(LuaClosure::LightC(_)) => None,
    }
}

fn testc_gcage(state: &mut LuaState) -> Result<usize, LuaError> {
    lua_vm::api::push_value(state, 1);
    let value = state.pop();
    let name = match testc_gc_age(value) {
        Some(lua_gc::GcAge::New) => b"new".as_slice(),
        Some(lua_gc::GcAge::Survival) => b"survival".as_slice(),
        Some(lua_gc::GcAge::Old0) => b"old0".as_slice(),
        Some(lua_gc::GcAge::Old1) => b"old1".as_slice(),
        Some(lua_gc::GcAge::Old) => b"old".as_slice(),
        Some(lua_gc::GcAge::Touched1) => b"touched1".as_slice(),
        Some(lua_gc::GcAge::Touched2) => b"touched2".as_slice(),
        None => b"no collectable".as_slice(),
    };
    testc_push_string(state, name)?;
    Ok(1)
}

fn testc_gccolor(state: &mut LuaState) -> Result<usize, LuaError> {
    lua_vm::api::push_value(state, 1);
    let value = state.pop();
    let name = match testc_gc_color(value) {
        Some(color) if color.is_white() => b"white".as_slice(),
        Some(lua_gc::Color::Gray) => b"gray".as_slice(),
        Some(lua_gc::Color::Black) => b"black".as_slice(),
        Some(_) => b"white".as_slice(),
        None => b"no collectable".as_slice(),
    };
    testc_push_string(state, name)?;
    Ok(1)
}

fn testc_gc_state_name(gc_state: lua_gc::GcState) -> &'static [u8] {
    match gc_state {
        lua_gc::GcState::Pause => b"pause",
        lua_gc::GcState::Propagate => b"propagate",
        lua_gc::GcState::EnterAtomic => b"atomic",
        lua_gc::GcState::Atomic => b"enteratomic",
        lua_gc::GcState::SweepAllGc => b"sweepallgc",
        lua_gc::GcState::SweepFinObj => b"sweepfinobj",
        lua_gc::GcState::SweepToBeFnz => b"sweeptobefnz",
        lua_gc::GcState::SweepEnd => b"sweepend",
        lua_gc::GcState::CallFin => b"callfin",
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TestcGcState {
    Propagate,
    EnterAtomic,
    Atomic,
    SweepAllGc,
    SweepFinObj,
    SweepToBeFnz,
    SweepEnd,
    CallFin,
    Pause,
}

impl TestcGcState {
    fn heap_state(self) -> lua_gc::GcState {
        match self {
            TestcGcState::Propagate => lua_gc::GcState::Propagate,
            TestcGcState::EnterAtomic => lua_gc::GcState::EnterAtomic,
            TestcGcState::Atomic => lua_gc::GcState::Atomic,
            TestcGcState::SweepAllGc => lua_gc::GcState::SweepAllGc,
            TestcGcState::SweepFinObj => lua_gc::GcState::SweepFinObj,
            TestcGcState::SweepToBeFnz => lua_gc::GcState::SweepToBeFnz,
            TestcGcState::SweepEnd => lua_gc::GcState::SweepEnd,
            TestcGcState::CallFin => lua_gc::GcState::CallFin,
            TestcGcState::Pause => lua_gc::GcState::Pause,
        }
    }
}

fn testc_gc_state_target(bytes: &[u8]) -> Option<TestcGcState> {
    match bytes {
        b"propagate" => Some(TestcGcState::Propagate),
        b"atomic" => Some(TestcGcState::EnterAtomic),
        b"enteratomic" => Some(TestcGcState::Atomic),
        b"sweepallgc" => Some(TestcGcState::SweepAllGc),
        b"sweepfinobj" => Some(TestcGcState::SweepFinObj),
        b"sweeptobefnz" => Some(TestcGcState::SweepToBeFnz),
        b"sweepend" => Some(TestcGcState::SweepEnd),
        b"callfin" => Some(TestcGcState::CallFin),
        b"pause" => Some(TestcGcState::Pause),
        b"" => None,
        _ => None,
    }
}

fn testc_drive_gcstate(state: &mut LuaState, target: TestcGcState) -> Result<(), LuaError> {
    if state.global().gckind == GcKind::Generational as u8 {
        return Err(LuaError::runtime(format_args!(
            "cannot change states in generational mode"
        )));
    }

    if state.gc().run_until_gc_state_for_test(target.heap_state()) {
        Ok(())
    } else {
        Err(LuaError::runtime(format_args!(
            "could not reach requested GC state"
        )))
    }
}

fn testc_gcstate(state: &mut LuaState) -> Result<usize, LuaError> {
    if lua_vm::api::get_top(state) == 0 {
        let name = testc_gc_state_name(state.global().heap.gc_state());
        testc_push_string(state, name)?;
        return Ok(1);
    }

    let option = state.check_arg_string(1)?;
    if option.is_empty() {
        let name = testc_gc_state_name(state.global().heap.gc_state());
        testc_push_string(state, name)?;
        return Ok(1);
    }

    let Some(target) = testc_gc_state_target(&option) else {
        return Err(LuaError::runtime(format_args!(
            "unknown GC state '{}'",
            String::from_utf8_lossy(&option)
        )));
    };
    testc_drive_gcstate(state, target)?;
    Ok(0)
}

fn testc_newuserdata(state: &mut LuaState) -> Result<usize, LuaError> {
    let size = state.opt_arg_integer(1, 0)?;
    let nuvalue = state.opt_arg_integer(2, 0)?;
    if size < 0 {
        return Err(LuaError::runtime(format_args!(
            "userdata size must be non-negative"
        )));
    }
    if nuvalue < 0 {
        return Err(LuaError::runtime(format_args!(
            "userdata value count must be non-negative"
        )));
    }
    state.new_userdata_typed(b"testC", size as usize, nuvalue as i32)?;
    Ok(1)
}

fn testc_type_count(state: &LuaState, name: &[u8]) -> Result<usize, LuaError> {
    let count = match name {
        b"string" => state
            .global()
            .heap
            .type_name_count(|ty| ty.contains("LuaString")),
        b"table" => state
            .global()
            .heap
            .type_name_count(|ty| ty.contains("LuaTable")),
        b"function" => state
            .global()
            .heap
            .type_name_count(|ty| ty.contains("LuaLClosure") || ty.contains("LuaCClosure")),
        b"userdata" => state
            .global()
            .heap
            .type_name_count(|ty| ty.contains("LuaUserData")),
        b"thread" => state
            .global()
            .heap
            .type_name_count(|ty| ty.contains("LuaThread")),
        _ => {
            return Err(LuaError::runtime(format_args!(
                "unknown type '{}'",
                String::from_utf8_lossy(name)
            )));
        }
    };
    Ok(count)
}

fn testc_push_usize(state: &mut LuaState, value: usize) {
    state.push(LuaValue::Int(value.min(i64::MAX as usize) as i64));
}

fn testc_totalmem(state: &mut LuaState) -> Result<usize, LuaError> {
    if lua_vm::api::get_top(state) == 0 {
        let total = state.global().total_bytes();
        let blocks = state.global().heap.allgc_count();
        let limit = state.global().sandbox.mem_limit.get().unwrap_or(usize::MAX);
        testc_push_usize(state, total);
        testc_push_usize(state, blocks);
        testc_push_usize(state, limit);
        return Ok(3);
    }

    if let Some(limit) = lua_vm::api::to_integer_x(state, 1) {
        let limit = if limit <= 0 {
            None
        } else {
            Some(limit as usize)
        };
        state.global().sandbox.mem_limit.set(limit);
        return Ok(0);
    }

    let name = state.check_arg_string(1)?;
    let count = testc_type_count(state, &name)?;
    testc_push_usize(state, count);
    Ok(1)
}

fn testc_checkmemory(state: &mut LuaState) -> Result<usize, LuaError> {
    let known = [
        b"string".as_slice(),
        b"table".as_slice(),
        b"function".as_slice(),
        b"userdata".as_slice(),
        b"thread".as_slice(),
    ]
    .into_iter()
    .try_fold(0usize, |acc, name| {
        testc_type_count(state, name).map(|n| acc + n)
    })?;
    let allgc = state.global().heap.allgc_count();
    if known > allgc {
        return Err(LuaError::runtime(format_args!(
            "GC type telemetry exceeds allgc count"
        )));
    }
    if state.global().heap.bytes_used() == 0 && allgc != 0 {
        return Err(LuaError::runtime(format_args!(
            "GC has live objects but zero tracked bytes"
        )));
    }
    Ok(0)
}

fn testc_gcstats(state: &mut LuaState) -> Result<usize, LuaError> {
    let (
        mode,
        gc_state,
        bytes,
        debt,
        threshold,
        allgc,
        allgc_cohorts,
        collections,
        minor_collections,
        full_collections,
        weak,
        weaklive,
        weakdead,
        weakretained,
        weakvalues,
        ephemeron,
        allweak,
        grayagain,
        pendingfin,
        tobefin,
        pendingfinyoung,
        pendingfinold,
        tobefinyoung,
        tobefinold,
        finobjnew,
        finobjsur,
        finobjold1,
        finobjrold,
        finobjscan,
        markstats,
        sweepstats,
    ) = {
        let g = state.global();
        let finstats = g.finalizers.stats();
        let weakstats = g.weak_tables_registry.stats();
        let allgc_cohorts = g.heap.allgc_cohort_stats();
        (
            if g.is_gen_mode() {
                "generational"
            } else {
                "incremental"
            },
            String::from_utf8_lossy(testc_gc_state_name(g.heap.gc_state())).into_owned(),
            g.total_bytes(),
            g.gc_debt(),
            g.heap.threshold_bytes(),
            g.heap.allgc_count(),
            allgc_cohorts,
            g.heap.collections(),
            g.heap.minor_collections(),
            g.heap.full_collections(),
            g.weak_tables_registry.len(),
            weakstats.snapshot_live,
            weakstats.snapshot_dead,
            weakstats.retained,
            weakstats.weak_values,
            weakstats.ephemeron,
            weakstats.all_weak,
            g.heap.grayagain_count(),
            g.finalizers.pending_len(),
            g.finalizers.to_be_finalized_len(),
            finstats.pending_young,
            finstats.pending_old,
            finstats.to_be_finalized_young,
            finstats.to_be_finalized_old,
            finstats.finobj_new,
            finstats.finobj_survival,
            finstats.finobj_old1,
            finstats.finobj_reallyold,
            finstats.finobj_minor_scan,
            g.heap.last_mark_stats(),
            g.heap.last_sweep_stats(),
        )
    };
    let tables = testc_type_count(state, b"table")?;
    let functions = testc_type_count(state, b"function")?;
    let threads = testc_type_count(state, b"thread")?;
    let userdata = testc_type_count(state, b"userdata")?;
    let strings = testc_type_count(state, b"string")?;
    let stats = format!(
        "mode={} state={} bytes={} debt={} threshold={} allgc={} allgcnew={} allgcsurvival={} allgcold1={} allgcold={} collections={} minorcollections={} fullcollections={} weak={} weaklive={} weakdead={} weakretained={} weakvalues={} ephemeron={} allweak={} grayagain={} pendingfin={} tobefin={} pendingfinyoung={} pendingfinold={} tobefinyoung={} tobefinold={} finobjnew={} finobjsur={} finobjold1={} finobjrold={} finobjscan={} marked={} markedyoung={} markedold={} traced={} tracedyoung={} tracedold={} sweepvisited={} sweepvisitedyoung={} sweepvisitedold={} sweeprevisit={} sweepfreed={} sweepfreedbytes={} tables={} functions={} threads={} userdata={} strings={}",
        mode,
        gc_state,
        bytes,
        debt,
        threshold,
        allgc,
        allgc_cohorts.new,
        allgc_cohorts.survival,
        allgc_cohorts.old1,
        allgc_cohorts.old,
        collections,
        minor_collections,
        full_collections,
        weak,
        weaklive,
        weakdead,
        weakretained,
        weakvalues,
        ephemeron,
        allweak,
        grayagain,
        pendingfin,
        tobefin,
        pendingfinyoung,
        pendingfinold,
        tobefinyoung,
        tobefinold,
        finobjnew,
        finobjsur,
        finobjold1,
        finobjrold,
        finobjscan,
        markstats.marked,
        markstats.marked_young,
        markstats.marked_old,
        markstats.traced,
        markstats.traced_young,
        markstats.traced_old,
        sweepstats.visited,
        sweepstats.visited_young,
        sweepstats.visited_old,
        sweepstats.revisit,
        sweepstats.freed,
        sweepstats.freed_bytes,
        tables,
        functions,
        threads,
        userdata,
        strings,
    );
    testc_push_string(state, stats.as_bytes())?;
    Ok(1)
}

fn register_testc_table(state: &mut LuaState) -> Result<(), LuaError> {
    state.enable_test_warning_handler()?;
    let funcs: &[(&[u8], lua_vm::state::LuaCFunction)] = &[
        (b"checkmemory", testc_checkmemory),
        (b"gcage", testc_gcage),
        (b"gccolor", testc_gccolor),
        (b"gcstate", testc_gcstate),
        (b"gcstats", testc_gcstats),
        (b"newuserdata", testc_newuserdata),
        (b"totalmem", testc_totalmem),
    ];
    state.new_lib(funcs)?;
    lua_vm::api::set_global(state, b"T")
}

pub(crate) fn write_gc_profile_path_from_env(var: &str, state: &LuaState) -> io::Result<()> {
    let Some(path) = std::env::var_os(var) else {
        return Ok(());
    };
    if path == "-" {
        let stderr = io::stderr();
        let mut lock = stderr.lock();
        return write_gc_profile(&mut lock, state);
    }
    let mut file = std::fs::File::create(std::path::PathBuf::from(path))?;
    write_gc_profile(&mut file, state)
}

fn write_gc_profile_from_env(state: &LuaState) -> io::Result<()> {
    write_gc_profile_path_from_env("LUA_RS_GC_PROFILE", state)
}

fn write_gc_profile(mut writer: impl Write, state: &LuaState) -> io::Result<()> {
    let (
        mode,
        gc_state,
        bytes,
        debt,
        threshold,
        allgc,
        allgc_cohorts,
        collections,
        minor_collections,
        full_collections,
        grayagain,
        interned_short_strings,
        markstats,
        sweepstats,
    ) = {
        let g = state.global();
        (
            if g.is_gen_mode() {
                "generational"
            } else {
                "incremental"
            },
            String::from_utf8_lossy(testc_gc_state_name(g.heap.gc_state())).into_owned(),
            g.total_bytes(),
            g.gc_debt(),
            g.heap.threshold_bytes(),
            g.heap.allgc_count(),
            g.heap.allgc_cohort_stats(),
            g.heap.collections(),
            g.heap.minor_collections(),
            g.heap.full_collections(),
            g.heap.grayagain_count(),
            g.interned_lt.len(),
            g.heap.last_mark_stats(),
            g.heap.last_sweep_stats(),
        )
    };

    writeln!(writer, "metric\tvalue")?;
    writeln!(writer, "mode\t{mode}")?;
    writeln!(writer, "state\t{gc_state}")?;
    writeln!(writer, "bytes\t{bytes}")?;
    writeln!(writer, "debt\t{debt}")?;
    writeln!(writer, "threshold\t{threshold}")?;
    writeln!(writer, "allgc\t{allgc}")?;
    writeln!(writer, "allgc_new\t{}", allgc_cohorts.new)?;
    writeln!(writer, "allgc_survival\t{}", allgc_cohorts.survival)?;
    writeln!(writer, "allgc_old1\t{}", allgc_cohorts.old1)?;
    writeln!(writer, "allgc_old\t{}", allgc_cohorts.old)?;
    writeln!(writer, "collections\t{collections}")?;
    writeln!(writer, "minor_collections\t{minor_collections}")?;
    writeln!(writer, "full_collections\t{full_collections}")?;
    writeln!(writer, "grayagain\t{grayagain}")?;
    writeln!(writer, "interned_short_strings\t{interned_short_strings}")?;
    writeln!(writer, "marked\t{}", markstats.marked)?;
    writeln!(writer, "marked_young\t{}", markstats.marked_young)?;
    writeln!(writer, "marked_old\t{}", markstats.marked_old)?;
    writeln!(writer, "traced\t{}", markstats.traced)?;
    writeln!(writer, "traced_young\t{}", markstats.traced_young)?;
    writeln!(writer, "traced_old\t{}", markstats.traced_old)?;
    writeln!(writer, "sweep_visited\t{}", sweepstats.visited)?;
    writeln!(writer, "sweep_visited_young\t{}", sweepstats.visited_young)?;
    writeln!(writer, "sweep_visited_old\t{}", sweepstats.visited_old)?;
    writeln!(writer, "sweep_revisit\t{}", sweepstats.revisit)?;
    writeln!(writer, "sweep_freed\t{}", sweepstats.freed)?;
    writeln!(writer, "sweep_freed_bytes\t{}", sweepstats.freed_bytes)
}

/// Install Rust-native modules that ship with `lua-cli` into
/// `package.preload`. After `open_libs` has populated the `package` library,
/// each entry written to `package.preload[name]` becomes a loader that
/// `require(name)` will invoke through the preload searcher.
///
/// This preloads `lfs`, the Rust-native LuaFileSystem port from the
/// `lua-rs-lfs` crate. `LUA_RS_TESTC=1` also installs a small internal `T`
/// table for official-test instrumentation.
fn register_preloaded_modules(state: &mut LuaState) -> Result<(), LuaError> {
    lua_vm::api::get_global(state, b"package")?;
    lua_vm::api::get_field(state, -1, b"preload")?;
    lua_vm::api::push_cclosure(state, lua_rs_lfs::luaopen_lfs, 0)?;
    lua_vm::api::set_field(state, -2, b"lfs")?;
    state.pop_n(2);
    if std::env::var_os("LUA_RS_TESTC").is_some() {
        register_testc_table(state)?;
    }
    Ok(())
}

#[cfg(unix)]
fn os_str_bytes(s: &std::ffi::OsString) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    s.as_bytes().to_vec()
}
#[cfg(not(unix))]
fn os_str_bytes(s: &std::ffi::OsString) -> Vec<u8> {
    s.to_string_lossy().into_owned().into_bytes()
}

/// Prepend `dir/?.lua;dir/?/init.lua` to `LUA_PATH` so that `require` can
/// find modules that live next to the running script.
///
/// When `LUA_PATH` is already set the script-dir entries are prepended so the
/// existing value is preserved.  When `LUA_PATH` is not set we write
/// `dir/?.lua;dir/?/init.lua;;` — the trailing `;;` causes `setpath` to
/// splice in the compiled-in default at that position, matching C-Lua's
/// behaviour when `LUA_PATH` is absent.
pub(crate) fn prepend_lua_path(dir: &std::path::Path) {
    let prefix = format!("{dir}/?.lua;{dir}/?/init.lua", dir = dir.display(),);
    let new_val = match std::env::var("LUA_PATH") {
        Ok(existing) if !existing.is_empty() => format!("{};{}", prefix, existing),
        _ => format!("{};;", prefix),
    };
    std::env::set_var("LUA_PATH", new_val);
}

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

/// Opt-in faster global allocator. The default build stays pure Rust (system
/// allocator, no C toolchain); `--features fast-alloc` swaps in mimalloc, which
/// measures ~9% faster on allocation-heavy scripts (binarytrees) and ~20% on
/// `gc_pressure`. Off by default so the pure-Rust, no-C-dependency build is
/// the standard one. Suppressed under `dhat-heap` (only one global allocator).
#[cfg(all(feature = "fast-alloc", not(feature = "dhat-heap")))]
#[global_allocator]
static FAST_ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Single resolution point for the version-override environment variable.
///
/// `OMNILUA_VERSION` is canonical; `LUA_RS_VERSION` is read only as a documented
/// legacy fallback. Returns the variable name actually consulted alongside its
/// value so the caller can name the right variable in error messages. Reading
/// new-else-old keeps one source of truth — no fallback-chain sprawl elsewhere.
fn resolve_version_override() -> Option<(&'static str, String)> {
    if let Ok(v) = std::env::var("OMNILUA_VERSION") {
        return Some(("OMNILUA_VERSION", v));
    }
    if let Ok(v) = std::env::var("LUA_RS_VERSION") {
        return Some(("LUA_RS_VERSION", v));
    }
    None
}

fn main() -> ExitCode {
    #[cfg(feature = "dhat-heap")]
    let _dhat = dhat::Profiler::new_heap();

    CLI_START.get_or_init(std::time::Instant::now);

    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if info.payload().downcast_ref::<LuaExit>().is_none()
            && info.payload().downcast_ref::<LuaThreadClose>().is_none()
        {
            previous_hook(info);
        }
    }));

    let args_os: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let argv: Vec<Vec<u8>> = args_os.iter().map(os_str_bytes).collect();

    let result = catch_unwind(AssertUnwindSafe(|| -> Result<i32, String> {
        let mut state = new_state().ok_or("new_state returned None")?;
        if let Some((var_name, v)) = resolve_version_override() {
            let lv = match v.trim() {
                "5.1" | "51" => lua_types::LuaVersion::V51,
                "5.2" | "52" => lua_types::LuaVersion::V52,
                "5.3" | "53" => lua_types::LuaVersion::V53,
                "5.4" | "54" => lua_types::LuaVersion::V54,
                "5.5" | "55" => lua_types::LuaVersion::V55,
                other => return Err(format!("unknown {var_name}: {other}")),
            };
            if !lv.is_supported() {
                return Err(format!(
                    "{} is not yet supported (supported: 5.2, 5.3, 5.4, 5.5)",
                    lv.version_str()
                ));
            }
            state.global_mut().lua_version = lv;
        }
        lua_vm::state::set_versioned_registry_slots(&mut state)
            .map_err(|e| format!("failed to initialize registry: {e:?}"))?;
        state.global_mut().parser_hook = Some(parser_hook);
        state.global_mut().file_loader_hook = Some(file_loader_hook);
        state.global_mut().file_open_hook = Some(file_open_hook);
        state.global_mut().stdout_hook = Some(stdout_hook);
        state.global_mut().stderr_hook = Some(stderr_hook);
        state.global_mut().stdin_hook = Some(stdin_hook);
        state.global_mut().env_hook = Some(env_hook);
        state.global_mut().unix_time_hook = Some(unix_time_hook);
        state.global_mut().cpu_clock_hook = Some(cpu_clock_hook);
        state.global_mut().local_offset_hook = Some(local_offset_hook);
        state.global_mut().entropy_hook = Some(entropy_hook);
        state.global_mut().temp_name_hook = Some(temp_name_hook);
        state.global_mut().popen_hook = Some(popen_hook);
        state.global_mut().file_remove_hook = Some(file_remove_hook);
        state.global_mut().file_rename_hook = Some(file_rename_hook);
        state.global_mut().os_execute_hook = Some(os_execute_hook);
        state.global_mut().dynlib_load_hook = Some(dynlib_load);
        state.global_mut().dynlib_symbol_hook = Some(dynlib_symbol);
        state.global_mut().dynlib_unload_hook = Some(dynlib_unload);

        let code = interp::run(&mut state, &argv, register_preloaded_modules);

        if std::env::var("LUA_RS_GC_DIAG").is_ok() {
            let tracked = state.global().heap.bytes_used();
            let allgc = state.global().heap.allgc_count();
            let threshold = state.global().heap.threshold_bytes();
            let collections = state.global().heap.collections();
            let paused = state.global().heap.is_paused();
            let gc_state = state.global().heap.gc_state();
            eprintln!(
                "[gc-diag] tracked={:.1}MB  allgc={}  threshold={:.1}MB  collections={}  paused={}  state={:?}",
                tracked as f64 / (1024.0 * 1024.0),
                allgc,
                threshold as f64 / (1024.0 * 1024.0),
                collections,
                paused,
                gc_state,
            );
        }

        if let Err(err) = write_gc_profile_from_env(&state) {
            eprintln!("[gc-profile] failed to write report: {}", err);
        }

        #[cfg(feature = "opcode-profile")]
        if let Err(err) = lua_vm::opcode_profile::write_report_from_env() {
            eprintln!("[opcode-profile] failed to write report: {}", err);
        }

        lua_vm::state::close(state);

        Ok(code)
    }));

    match result {
        Ok(Ok(code)) => ExitCode::from(code as u8),
        Ok(Err(msg)) => {
            eprintln!("lua: {}", msg);
            ExitCode::from(1)
        }
        Err(panic) => {
            if let Some(exit) = panic.downcast_ref::<LuaExit>() {
                return ExitCode::from(exit.0 as u8);
            }
            let msg = if let Some(s) = panic.downcast_ref::<String>() {
                s.clone()
            } else if let Some(s) = panic.downcast_ref::<&str>() {
                s.to_string()
            } else {
                "(non-string panic payload)".to_string()
            };
            eprintln!("[panic] {}", msg);
            ExitCode::from(101)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{wall_clock_offset_seconds, WallClock};

    fn wc(year: i32, yday: i32, hour: i32, min: i32, sec: i32) -> WallClock {
        WallClock {
            year,
            yday,
            hour,
            min,
            sec,
        }
    }

    /// Plain same-day offsets, both signs, including sub-hour zones
    /// (+05:45 Kathmandu-style needs the minute term).
    #[test]
    fn same_day_offsets() {
        assert_eq!(
            wall_clock_offset_seconds(wc(126, 10, 14, 0, 0), wc(126, 10, 9, 0, 0)),
            5 * 3_600
        );
        assert_eq!(
            wall_clock_offset_seconds(wc(126, 10, 4, 0, 0), wc(126, 10, 9, 0, 0)),
            -5 * 3_600
        );
        assert_eq!(
            wall_clock_offset_seconds(wc(126, 10, 15, 15, 0), wc(126, 10, 9, 30, 0)),
            5 * 3_600 + 45 * 60
        );
        assert_eq!(
            wall_clock_offset_seconds(wc(126, 10, 9, 0, 0), wc(126, 10, 9, 0, 0)),
            0
        );
    }

    /// Midnight straddles within one year: the `tm_yday` difference carries
    /// the ±1 day term.
    #[test]
    fn day_boundary_same_year() {
        assert_eq!(
            wall_clock_offset_seconds(wc(126, 11, 0, 30, 0), wc(126, 10, 23, 30, 0)),
            3_600
        );
        assert_eq!(
            wall_clock_offset_seconds(wc(126, 10, 23, 30, 0), wc(126, 11, 0, 30, 0)),
            -3_600
        );
    }

    /// Jan 1 straddles: the years differ, `tm_yday` jumps between 0 and
    /// 364/365, and the year comparison must pin the day delta to ±1 —
    /// feeding the raw yday difference here would be off by a whole year.
    #[test]
    fn year_boundary_pins_day_delta() {
        assert_eq!(
            wall_clock_offset_seconds(wc(126, 0, 0, 30, 0), wc(125, 364, 23, 30, 0)),
            3_600
        );
        assert_eq!(
            wall_clock_offset_seconds(wc(125, 364, 23, 0, 0), wc(126, 0, 1, 0, 0)),
            -7_200
        );
        assert_eq!(
            wall_clock_offset_seconds(wc(125, 365, 23, 0, 0), wc(126, 0, 1, 0, 0)),
            -7_200
        );
    }
}
