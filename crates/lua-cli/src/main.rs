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

use lua_stdlib::auxlib::{load_buffer, load_string};
use lua_stdlib::init::open_libs;
use lua_types::closure::LuaLClosure;
use lua_types::error::LuaError;
use lua_types::filehandle::LuaFileHandle;
use lua_types::gc::GcRef;
use lua_types::upval::UpVal;
use lua_types::value::LuaValue;
use lua_vm::api::{pcall_k, to_lua_string};
use lua_vm::state::{new_state, LuaState};

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
    Write(BufWriter<std::fs::File>),
    ReadWrite(std::fs::File, Option<u8>),
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
                Ok(FsFile::Write(BufWriter::new(f)))
            }
            (b'a', false) => {
                let mut f = std::fs::OpenOptions::new().append(true).create(true).open(&path)?;
                f.seek(SeekFrom::End(0))?;
                Ok(FsFile::Write(BufWriter::new(f)))
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
            FsFile::Write(_) => -1,
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
            FsFile::Write(_) => {}
        }
    }

    fn write_bytes(&mut self, data: &[u8]) -> io::Result<usize> {
        match self {
            FsFile::Write(w) => w.write(data),
            FsFile::ReadWrite(f, _) => f.write(data),
            FsFile::Read(_) => Err(io::Error::new(io::ErrorKind::PermissionDenied, "file not open for writing")),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            FsFile::Write(w) => w.flush(),
            FsFile::ReadWrite(f, _) => f.flush(),
            FsFile::Read(_) => Ok(()),
        }
    }

    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        match self {
            FsFile::Read(r) => r.seek(pos),
            FsFile::Write(w) => w.seek(pos),
            FsFile::ReadWrite(f, _) => f.seek(pos),
        }
    }

    fn tell(&mut self) -> io::Result<u64> {
        self.seek(SeekFrom::Current(0))
    }

    fn clear_error(&mut self) {}

    fn has_error(&self) -> bool { false }
}

impl Drop for FsFile {
    fn drop(&mut self) {
        if let FsFile::Write(w) = self {
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

fn file_open_hook(filename: &[u8], mode: &[u8]) -> Result<Box<dyn LuaFileHandle>, LuaError> {
    FsFile::open(filename, mode).map(|f| Box::new(f) as Box<dyn LuaFileHandle>).map_err(|err| {
        LuaError::runtime(format_args!(
            "cannot open '{}': {}",
            String::from_utf8_lossy(filename),
            err
        ))
    })
}

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
        upvals.push(std::cell::RefCell::new(GcRef::new(UpVal::closed(LuaValue::Nil))));
    }
    Ok(GcRef::new(LuaLClosure {
        proto: GcRef::new(*proto),
        upvals,
    }))
}

const MULTRET: i32 = -1;

fn render_lua_error(e: &LuaError) -> String {
    match e {
        LuaError::Runtime(v) | LuaError::Syntax(v) => match v {
            LuaValue::Str(s) => format!("{}: {}", e_tag(e), String::from_utf8_lossy(s.as_bytes())),
            other => format!("{}: {:?}", e_tag(e), other),
        },
        LuaError::Memory | LuaError::Error | LuaError::Yield
        | LuaError::File | LuaError::Gc => format!("{}", e_tag(e)),
    }
}

fn e_tag(e: &LuaError) -> &'static str {
    match e {
        LuaError::Runtime(_) => "Runtime",
        LuaError::Syntax(_)  => "Syntax",
        LuaError::Memory     => "Memory",
        LuaError::Error      => "Error",
        LuaError::Yield      => "Yield",
        LuaError::File       => "File",
        LuaError::Gc         => "Gc",
    }
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

fn main() -> ExitCode {
    let args_os: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if args_os.len() < 2 {
        let prog = args_os
            .first()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "lua-rs".to_string());
        eprintln!("usage: {prog} <script.lua | -e 'source'>");
        eprintln!("examples:");
        eprintln!("  {prog} script.lua");
        eprintln!("  {prog} -e 'print(\"hello\")'");
        return ExitCode::from(2);
    }

    let (source, chunkname): (Vec<u8>, Option<Vec<u8>>) = if args_os[1] == "-e" {
        if args_os.len() < 3 {
            eprintln!("-e requires an argument");
            return ExitCode::from(2);
        }
        (os_str_bytes(&args_os[2]), Some(b"=(command line)".to_vec()))
    } else {
        let path = std::path::Path::new(&args_os[1]);
        if path.is_file() {
            match std::fs::read(path) {
                Ok(bytes) => {
                    let mut name = vec![b'@'];
                    name.extend_from_slice(&os_str_bytes(&args_os[1]));
                    (bytes, Some(name))
                }
                Err(e) => {
                    eprintln!("cannot read {}: {}", path.display(), e);
                    return ExitCode::from(2);
                }
            }
        } else {
            (os_str_bytes(&args_os[1]), None)
        }
    };

    let verbose = std::env::var("LUA_RS_VERBOSE").is_ok();
    macro_rules! step { ($($t:tt)*) => { if verbose { eprintln!($($t)*); } }; }

    step!("[1/4] Creating LuaState...");
    let result = catch_unwind(AssertUnwindSafe(|| {
        let mut state = new_state().ok_or("new_state returned None")?;
        state.global_mut().parser_hook = Some(parser_hook);
        state.global_mut().file_loader_hook = Some(file_loader_hook);
        state.global_mut().file_open_hook = Some(file_open_hook);
        state.global_mut().file_remove_hook = Some(file_remove_hook);

        step!("[2/4] Opening standard library...");
        open_libs(&mut state).map_err(|e| format!("open_libs failed: {}", render_lua_error(&e)))?;

        step!("[3/4] Loading source (parse + compile)...");
        let status = match &chunkname {
            Some(name) => load_buffer(&mut state, &source, name)
                .map_err(|e| format!("load_buffer failed: {}", render_lua_error(&e)))?,
            None => load_string(&mut state, &source)
                .map_err(|e| format!("load_string failed: {}", render_lua_error(&e)))?,
        };
        if status != 0 {
            let msg = match to_lua_string(&mut state, -1) {
                Ok(Some(s)) => String::from_utf8_lossy(s.as_bytes()).into_owned(),
                _ => "(no error message on stack)".to_string(),
            };
            return Err(format!(
                "Syntax: {} (load_string status={})",
                msg, status
            ));
        }

        step!("[4/4] Executing chunk...");
        let final_status = pcall_k(&mut state, 0, MULTRET, 0, 0, None)
            .map_err(|e| format!("pcall_k failed: {}", render_lua_error(&e)))?;

        Ok::<_, String>(final_status)
    }));

    match result {
        Ok(Ok(status)) => {
            if verbose {
                eprintln!("[ok] execution completed, status={:?}", status);
            }
            let _ = status;
            ExitCode::SUCCESS
        }
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
//   unsafe_blocks: 0
//   notes:         drives new_state → open_libs → load_string → pcall_k.
//                  Designed to surface the first todo!() panic on a hello-
//                  world program, not to be a complete interpreter.
// ──────────────────────────────────────────────────────────────────────────
