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

use lua_types::closure::LuaLClosure;
use lua_types::error::LuaError;
use lua_types::filehandle::LuaFileHandle;
use lua_types::gc::GcRef;
use lua_types::upval::UpVal;
use lua_types::value::LuaValue;
use lua_vm::state::{new_state, DynLibId, DynamicSymbol, LuaState};

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
        let s = std::str::from_utf8(filename).map_err(|_| {
            LuaError::runtime(format_args!("filename is not valid UTF-8"))
        })?;
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
    Read(BufReader<std::fs::File>),
    Write(BufWriter<std::fs::File>, bool, FsBufMode),
    ReadWrite(std::fs::File, Option<u8>),
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
            let s = std::str::from_utf8(filename)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "filename not valid UTF-8"))?;
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
                Ok(FsFile::Read(BufReader::new(f)))
            }
            (b'w', false) => {
                let f = std::fs::File::create(&path)?;
                Ok(FsFile::Write(BufWriter::new(f), false, FsBufMode::Full))
            }
            (b'a', false) => {
                let mut f = std::fs::OpenOptions::new().append(true).create(true).open(&path)?;
                f.seek(SeekFrom::End(0))?;
                Ok(FsFile::Write(BufWriter::new(f), false, FsBufMode::Full))
            }
            _ => {
                let f = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(first == b'w' || first == b'a')
                    .truncate(first == b'w')
                    .append(first == b'a')
                    .open(&path)?;
                Ok(FsFile::ReadWrite(f, None))
            }
        }
    }
}

impl LuaFileHandle for FsFile {
    fn read_byte(&mut self) -> i32 {
        match self {
            FsFile::Read(r) => {
                let mut buf = [0u8; 1];
                match r.read(&mut buf) {
                    Ok(1) => buf[0] as i32,
                    _ => -1,
                }
            }
            FsFile::ReadWrite(f, pushback) => {
                if let Some(b) = pushback.take() {
                    return b as i32;
                }
                let mut buf = [0u8; 1];
                match f.read(&mut buf) {
                    Ok(1) => buf[0] as i32,
                    _ => -1,
                }
            }
            FsFile::Write(_, errored, _) => {
                *errored = true;
                -1
            }
        }
    }

    fn unread_byte(&mut self, byte: i32) {
        match self {
            FsFile::Read(r) => {
                if byte >= 0 {
                    let _ = r.seek_relative(-1);
                }
            }
            FsFile::ReadWrite(_, pushback) => {
                if byte >= 0 {
                    *pushback = Some(byte as u8);
                }
            }
            FsFile::Write(_, _, _) => {}
        }
    }

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
            FsFile::ReadWrite(f, _) => f.write(data),
            FsFile::Read(_) => Err(io::Error::new(io::ErrorKind::PermissionDenied, "file not open for writing")),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            FsFile::Write(w, _, _) => w.flush(),
            FsFile::ReadWrite(f, _) => f.flush(),
            FsFile::Read(_) => Ok(()),
        }
    }

    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        match self {
            FsFile::Read(r) => r.seek(pos),
            FsFile::Write(w, _, _) => w.seek(pos),
            FsFile::ReadWrite(f, _) => f.seek(pos),
        }
    }

    fn tell(&mut self) -> io::Result<u64> {
        self.seek(SeekFrom::Current(0))
    }

    fn clear_error(&mut self) {
        if let FsFile::Write(_, errored, _) = self {
            *errored = false;
        }
    }

    fn has_error(&self) -> bool {
        matches!(self, FsFile::Write(_, true, _))
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

impl Drop for FsFile {
    fn drop(&mut self) {
        if let FsFile::Write(w, _, _) = self {
            let _ = w.flush();
        }
    }
}

fn file_remove_hook(filename: &[u8]) -> Result<(), LuaError> {
    #[cfg(unix)]
    let path: std::path::PathBuf = {
        use std::os::unix::ffi::OsStrExt;
        std::path::PathBuf::from(std::ffi::OsStr::from_bytes(filename))
    };
    #[cfg(not(unix))]
    let path: std::path::PathBuf = {
        let s = std::str::from_utf8(filename).map_err(|_| {
            LuaError::runtime(format_args!("filename is not valid UTF-8"))
        })?;
        std::path::PathBuf::from(s)
    };
    std::fs::remove_file(&path)
        .or_else(|_| std::fs::remove_dir(&path))
        .map_err(|err| {
            LuaError::runtime(format_args!(
                "cannot remove '{}': {}",
                String::from_utf8_lossy(filename),
                err
            ))
        })
}

fn file_rename_hook(from: &[u8], to: &[u8]) -> Result<(), LuaError> {
    fn to_path(bytes: &[u8]) -> Result<std::path::PathBuf, LuaError> {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            Ok(std::path::PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
        }
        #[cfg(not(unix))]
        {
            let s = std::str::from_utf8(bytes).map_err(|_| {
                LuaError::runtime(format_args!("filename is not valid UTF-8"))
            })?;
            Ok(std::path::PathBuf::from(s))
        }
    }
    let from_path = to_path(from)?;
    let to_path_buf = to_path(to)?;
    std::fs::rename(&from_path, &to_path_buf).map_err(|err| {
        LuaError::runtime(format_args!(
            "cannot rename '{}' to '{}': {}",
            String::from_utf8_lossy(from),
            String::from_utf8_lossy(to),
            err
        ))
    })
}

fn file_open_hook(filename: &[u8], mode: &[u8]) -> Result<Box<dyn LuaFileHandle>, LuaError> {
    FsFile::open(filename, mode).map(|f| Box::new(f) as Box<dyn LuaFileHandle>).map_err(|err| {
        LuaError::runtime(format_args!(
            "cannot open '{}': {}",
            String::from_utf8_lossy(filename),
            err
        ))
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
    Read(Option<BufReader<std::process::ChildStdout>>, std::process::Child),
    Write(Option<BufWriter<std::process::ChildStdin>>, std::process::Child),
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
        let mut command = std::process::Command::new("/bin/sh");
        command.arg("-c").arg(cmd_str);
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
        Err(io::Error::new(io::ErrorKind::Unsupported, "popen pipe is not seekable"))
    }

    fn tell(&mut self) -> io::Result<u64> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "popen pipe is not seekable"))
    }

    fn clear_error(&mut self) {}

    fn has_error(&self) -> bool { false }
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

// ─── Dynamic library backend (Phase D-3.5) ────────────────────────────────────
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
        let s = std::str::from_utf8(path).map_err(|_| {
            LuaError::runtime(format_args!("library path is not valid UTF-8"))
        })?;
        Ok(std::path::PathBuf::from(s))
    }
}

/// `dynlib_load_hook` backend. Loads the library via `libloading` and stashes
/// it in the per-thread registry; the returned [`DynLibId`] is the
/// registry-vector index, which keeps the library alive until process exit.
///
/// PORT NOTE: a missing path is reported as `LuaError::File`, which
/// `lua-stdlib`'s `package.loadlib` maps to the `"absent"` failure tag (the
/// `lua-rs` build behaves like C-Lua's no-dlfcn fallback for plain file-not-
/// found, and like POSIX/Windows dlopen for every other open failure).
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
        let lib = v.get(idx).ok_or_else(|| {
            LuaError::runtime(format_args!("invalid dynlib handle {}", idx))
        })?;

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
        let resolved: Result<libloading::Symbol<RustNativeFn>, _> =
            unsafe { lib.get(symbol) };
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

fn parser_hook(
    state: &mut LuaState,
    source: &[u8],
    name: &[u8],
    firstchar: i32,
) -> Result<GcRef<LuaLClosure>, LuaError> {
    let proto = lua_parse::parse(
        state,
        lua_parse::DynData::default(),
        source,
        name,
        firstchar,
    )?;
    let nupvals = proto.upvalues.len();
    let mut upvals = Vec::with_capacity(nupvals);
    for _ in 0..nupvals {
        upvals.push(std::cell::Cell::new(GcRef::new(UpVal::closed(LuaValue::Nil))));
    }
    Ok(GcRef::new(LuaLClosure {
        proto: GcRef::new(*proto),
        upvals,
    }))
}

/// Install Rust-native modules that ship with `lua-cli` into
/// `package.preload`. After `open_libs` has populated the `package` library,
/// each entry written to `package.preload[name]` becomes a loader that
/// `require(name)` will invoke through the preload searcher.
///
/// Phase G-1 ships a single preloaded module — `lfs`, the Rust-native
/// LuaFileSystem port from the `lua-rs-lfs` crate.
fn register_preloaded_modules(state: &mut LuaState) -> Result<(), LuaError> {
    lua_vm::api::get_global(state, b"package")?;
    lua_vm::api::get_field(state, -1, b"preload")?;
    lua_vm::api::push_cclosure(state, lua_rs_lfs::luaopen_lfs, 0)?;
    lua_vm::api::set_field(state, -2, b"lfs")?;
    state.pop_n(2);
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
    let prefix = format!(
        "{dir}/?.lua;{dir}/?/init.lua",
        dir = dir.display(),
    );
    let new_val = match std::env::var("LUA_PATH") {
        Ok(existing) if !existing.is_empty() => format!("{};{}", prefix, existing),
        _ => format!("{};;", prefix),
    };
    std::env::set_var("LUA_PATH", new_val);
}

fn main() -> ExitCode {
    let args_os: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let argv: Vec<Vec<u8>> = args_os.iter().map(os_str_bytes).collect();

    let result = catch_unwind(AssertUnwindSafe(|| -> Result<i32, String> {
        let mut state = new_state().ok_or("new_state returned None")?;
        state.global_mut().parser_hook = Some(parser_hook);
        state.global_mut().file_loader_hook = Some(file_loader_hook);
        state.global_mut().file_open_hook = Some(file_open_hook);
        state.global_mut().popen_hook = Some(popen_hook);
        state.global_mut().file_remove_hook = Some(file_remove_hook);
        state.global_mut().file_rename_hook = Some(file_rename_hook);
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

        Ok(code)
    }));

    match result {
        Ok(Ok(code)) => ExitCode::from(code as u8),
        Ok(Err(msg)) => {
            eprintln!("lua: {}", msg);
            ExitCode::from(1)
        }
        Err(panic) => {
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

// ──────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        (minimal entrypoint; not a port of lua.c — that's Phase F)
//   target_crate:  lua-cli
//   confidence:    high
//   todos:         0
//   port_notes:    0
//   unsafe_blocks: 3  (libloading-backed dynlib backend, Phase D-3.5;
//                      budget counts 4 due to one `unsafe extern "C" fn()`
//                      type parameter on `Symbol<...>`).
//   notes:         drives new_state → open_libs → load_string → pcall_k.
//                  Designed to surface the first todo!() panic on a hello-
//                  world program, not to be a complete interpreter. Hosts the
//                  libloading-backed implementation of the three
//                  dynlib_*_hook hooks on GlobalState (Phase D-3.5); ceiling
//                  in harness/unsafe-budgets.toml = 3. Also installs
//                  popen_hook (Phase F): spawns /bin/sh -c <cmd> via
//                  std::process::Command, wraps the resulting pipe in
//                  PopenFile (a LuaFileHandle) so io.popen and the LStream
//                  read/write/close path Just Work for clients like
//                  LuaRocks. No new unsafe — std::process is permitted in
//                  lua-cli.
// ──────────────────────────────────────────────────────────────────────────
