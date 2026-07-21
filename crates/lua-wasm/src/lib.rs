//! Bare `wasm32-unknown-unknown` embedding exports for lua-rs.
//!
//! The generated module imports host capabilities through the `lua_rs_host`
//! module and exposes a tiny pointer/length ABI for loading Lua source from the
//! embedder.

use std::cell::RefCell;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
use std::io::{self, SeekFrom};
use std::slice;

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
use omnilua::LuaFileHandle;
use omnilua::{HostHooks, LuaError, LuaRuntime, LuaVersion, SandboxConfig, TripReason};
use lua_types::LuaValue;

thread_local! {
    static LAST_ERROR: RefCell<Vec<u8>> = RefCell::new(Vec::new());
    static RUNTIME: RefCell<Option<LuaRuntime>> = RefCell::new(None);
    /// Sandbox limits to (re)apply whenever the runtime is created or reset.
    /// `None` = unsandboxed. Set via `lua_rs_wasm_set_limits`.
    static SANDBOX_CFG: RefCell<Option<SandboxConfig>> = RefCell::new(None);
    /// Language version the next-created runtime speaks. Defaults to 5.4 and is
    /// re-applied on every create/reset. Set via `lua_rs_wasm_set_version`.
    static SELECTED_VERSION: RefCell<LuaVersion> = RefCell::new(LuaVersion::default());
}

/// Map a one-byte version code (the `luac` version byte: `0x51`..=`0x55`, i.e.
/// `5.1`..=`5.5`) onto a [`LuaVersion`]. Returns `None` for any other code —
/// there is no fallback, an unknown code is rejected by the caller.
fn version_from_code(code: u32) -> Option<LuaVersion> {
    match code {
        0x51 => Some(LuaVersion::V51),
        0x52 => Some(LuaVersion::V52),
        0x53 => Some(LuaVersion::V53),
        0x54 => Some(LuaVersion::V54),
        0x55 => Some(LuaVersion::V55),
        _ => None,
    }
}

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
#[link(wasm_import_module = "lua_rs_host")]
extern "C" {
    #[link_name = "write_stdout"]
    fn imported_write_stdout(ptr: *const u8, len: usize) -> i32;

    #[link_name = "read_stdin"]
    fn imported_read_stdin(out_ptr: *mut u8, out_len: usize) -> i32;

    #[link_name = "unix_time"]
    fn imported_unix_time() -> i64;

    #[link_name = "env_len"]
    fn imported_env_len(name_ptr: *const u8, name_len: usize) -> i32;

    #[link_name = "env_read"]
    fn imported_env_read(
        name_ptr: *const u8,
        name_len: usize,
        out_ptr: *mut u8,
        out_len: usize,
    ) -> i32;

    #[link_name = "file_len"]
    fn imported_file_len(path_ptr: *const u8, path_len: usize) -> i32;

    #[link_name = "file_read"]
    fn imported_file_read(
        path_ptr: *const u8,
        path_len: usize,
        out_ptr: *mut u8,
        out_len: usize,
    ) -> i32;

    #[link_name = "open_file"]
    fn imported_open_file(
        path_ptr: *const u8,
        path_len: usize,
        mode_ptr: *const u8,
        mode_len: usize,
    ) -> i32;

    #[link_name = "file_read_byte"]
    fn imported_file_read_byte(id: i32) -> i32;

    #[link_name = "file_write"]
    fn imported_file_write(id: i32, ptr: *const u8, len: usize) -> i32;

    #[link_name = "file_flush"]
    fn imported_file_flush(id: i32) -> i32;

    #[link_name = "file_seek"]
    fn imported_file_seek(id: i32, whence: i32, offset: i64) -> i64;

    #[link_name = "file_set_buf_mode"]
    fn imported_file_set_buf_mode(id: i32, mode: i32, size: usize) -> i32;

    #[link_name = "file_error_code"]
    fn imported_file_error_code(id: i32) -> i32;

    #[link_name = "file_error_len"]
    fn imported_file_error_len(id: i32) -> i32;

    #[link_name = "file_error_read"]
    fn imported_file_error_read(id: i32, out_ptr: *mut u8, out_len: usize) -> i32;
}

fn clear_last_error() {
    LAST_ERROR.with(|cell| cell.borrow_mut().clear());
}

fn set_last_error(message: impl Into<Vec<u8>>) {
    LAST_ERROR.with(|cell| {
        *cell.borrow_mut() = message.into();
    });
}

fn set_lua_error(error: LuaError) {
    set_last_error(lua_error_bytes(error));
}

fn lua_error_bytes(error: LuaError) -> Vec<u8> {
    match error {
        LuaError::Runtime(value) | LuaError::Syntax(value) => lua_value_bytes(value),
        LuaError::RuntimeMsg(b) | LuaError::SyntaxMsg(b) => b.into_vec(),
        other => other.to_string().into_bytes(),
    }
}

fn lua_value_bytes(value: LuaValue) -> Vec<u8> {
    match value {
        LuaValue::Str(value) => value.as_bytes().to_vec(),
        LuaValue::Nil => b"nil".to_vec(),
        LuaValue::Bool(value) => value.to_string().into_bytes(),
        LuaValue::Int(value) => value.to_string().into_bytes(),
        LuaValue::Float(value) => value.to_string().into_bytes(),
        other => format!("{other:?}").into_bytes(),
    }
}

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
fn imported_stdout(bytes: &[u8]) -> io::Result<()> {
    // SAFETY: `bytes` is a live Rust slice for the duration of the synchronous
    // host import. The host receives only pointer/length and must not retain it.
    let status = unsafe { imported_write_stdout(bytes.as_ptr(), bytes.len()) };
    if status == 0 {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::Other,
            "host stdout callback failed",
        ))
    }
}

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
fn imported_stdin(buf: &mut [u8]) -> io::Result<usize> {
    if buf.is_empty() {
        return Ok(0);
    }
    // SAFETY: `buf` is initialized writable storage and lives until the
    // synchronous host import returns. The host writes at most `buf.len()` bytes.
    let read = unsafe { imported_read_stdin(buf.as_mut_ptr(), buf.len()) };
    if read < 0 || read as usize > buf.len() {
        Err(io::Error::new(
            io::ErrorKind::Other,
            "host stdin callback failed",
        ))
    } else {
        Ok(read as usize)
    }
}

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
fn imported_time() -> i64 {
    // SAFETY: zero-argument host import with no memory aliasing.
    unsafe { imported_unix_time() }
}

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
fn imported_env(name: &[u8]) -> Option<Vec<u8>> {
    // SAFETY: `name` is a live Rust slice for this synchronous import. The host
    // reports the required buffer size without retaining the pointer.
    let len = unsafe { imported_env_len(name.as_ptr(), name.len()) };
    if len < 0 {
        return None;
    }

    let mut out = vec![0; len as usize];
    // SAFETY: `out` has exactly `len` bytes of initialized writable storage and
    // both slices stay alive until the import returns.
    let written =
        unsafe { imported_env_read(name.as_ptr(), name.len(), out.as_mut_ptr(), out.len()) };
    if written == len {
        Some(out)
    } else {
        None
    }
}

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
fn imported_file_loader(path: &[u8]) -> Result<Vec<u8>, LuaError> {
    // SAFETY: `path` is a live Rust slice for this synchronous import. The host
    // reports only a length and must not retain the pointer.
    let len = unsafe { imported_file_len(path.as_ptr(), path.len()) };
    if len < 0 {
        return Err(LuaError::runtime(format_args!(
            "host file not found: {}",
            String::from_utf8_lossy(path)
        )));
    }

    let mut out = vec![0; len as usize];
    // SAFETY: `out` has enough writable storage for the length reported by the
    // host, and all pointers are valid until the import returns.
    let written =
        unsafe { imported_file_read(path.as_ptr(), path.len(), out.as_mut_ptr(), out.len()) };
    if written == len {
        Ok(out)
    } else {
        Err(LuaError::runtime(format_args!(
            "host file read failed: {}",
            String::from_utf8_lossy(path)
        )))
    }
}

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
struct ImportedFileHandle {
    id: i32,
    unread: Option<u8>,
    errored: bool,
}

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
impl ImportedFileHandle {
    fn new(id: i32) -> Self {
        Self {
            id,
            unread: None,
            errored: false,
        }
    }
}

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
impl LuaFileHandle for ImportedFileHandle {
    fn read_byte(&mut self) -> i32 {
        if let Some(byte) = self.unread.take() {
            return byte as i32;
        }
        // SAFETY: file ids are opaque host handles returned by `open_file`.
        // The import does not access Rust memory.
        let byte = unsafe { imported_file_read_byte(self.id) };
        if byte < -1 {
            self.errored = true;
            -1
        } else {
            byte
        }
    }

    fn unread_byte(&mut self, byte: i32) {
        if (0..=u8::MAX as i32).contains(&byte) {
            self.unread = Some(byte as u8);
        }
    }

    fn write_bytes(&mut self, data: &[u8]) -> io::Result<usize> {
        // SAFETY: `data` is a live Rust slice for the synchronous host import.
        // The host must copy from it before returning.
        let written = unsafe { imported_file_write(self.id, data.as_ptr(), data.len()) };
        if written < 0 {
            self.errored = true;
            Err(io::Error::new(
                io::ErrorKind::Other,
                "host file write failed",
            ))
        } else {
            Ok(written as usize)
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        // SAFETY: host import uses only the opaque file id.
        let status = unsafe { imported_file_flush(self.id) };
        if status == 0 {
            Ok(())
        } else {
            self.errored = true;
            Err(io::Error::new(
                io::ErrorKind::Other,
                "host file flush failed",
            ))
        }
    }

    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let (whence, offset) = match pos {
            SeekFrom::Start(offset) => (0, offset as i64),
            SeekFrom::Current(offset) => (1, offset),
            SeekFrom::End(offset) => (2, offset),
        };
        // SAFETY: host import uses only scalar values and the opaque file id.
        let new_pos = unsafe { imported_file_seek(self.id, whence, offset) };
        if new_pos < 0 {
            self.errored = true;
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "host file seek failed",
            ))
        } else {
            Ok(new_pos as u64)
        }
    }

    fn tell(&mut self) -> io::Result<u64> {
        self.seek(SeekFrom::Current(0))
    }

    fn clear_error(&mut self) {
        self.errored = false;
    }

    fn has_error(&self) -> bool {
        self.errored
    }

    fn last_error_info(&self) -> Option<(i32, String)> {
        // SAFETY: host imports use only the opaque file id and do not access
        // Rust memory.
        let code = unsafe { imported_file_error_code(self.id) };
        let len = unsafe { imported_file_error_len(self.id) };
        if code == 0 && len <= 0 {
            return None;
        }

        let mut out = vec![0; len.max(0) as usize];
        // SAFETY: `out` is writable for `out.len()` bytes and lives until the
        // synchronous host import returns.
        let written = unsafe { imported_file_error_read(self.id, out.as_mut_ptr(), out.len()) };
        let msg = if written == len {
            String::from_utf8_lossy(&out).into_owned()
        } else {
            "host file error".to_string()
        };
        Some((code, msg))
    }

    fn set_buf_mode(&mut self, mode: i32, size: usize) -> io::Result<()> {
        // SAFETY: host import uses only scalar values and the opaque file id.
        let status = unsafe { imported_file_set_buf_mode(self.id, mode, size) };
        if status == 0 {
            Ok(())
        } else {
            self.errored = true;
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "host file setvbuf failed",
            ))
        }
    }
}

/// Opens a host file, preserving the host's errno across the wasm boundary (#301).
///
/// The `open_file` host import returns `id >= 0` for a live handle; a negative
/// return signals failure and carries the errno so `io_lib::file_result` can
/// report the faithful `(nil, msg, errno)` triple instead of errno 0:
///   * `id == -1` — failure with **no** errno available (the host has no OS
///     error to report). Mapped to a `raw_os_error`-less `io::Error`, which
///     `file_result` renders as the honest 2-value `(nil, msg)`. This is the
///     original `-1 == fail` sentinel, kept for host backward-compatibility.
///   * `id <= -2` — failure carrying `errno = -(id + 1)` (e.g. `-2` → EPERM
///     (errno 1), `-3` → ENOENT (errno 2), `-23` → EINVAL (errno 22)). The
///     one-unit shift reserves `-1` as the exclusive no-errno sentinel so that
///     EPERM (errno 1) is representable without displacing it (#305).
///     Mapped to `io::Error::from_raw_os_error(-(id + 1))`.
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
fn imported_file_open(path: &[u8], mode: &[u8]) -> io::Result<Box<dyn LuaFileHandle>> {
    // SAFETY: `path` and `mode` are live Rust slices for this synchronous import.
    // The host returns an opaque file id and must not retain the pointers.
    let id = unsafe { imported_open_file(path.as_ptr(), path.len(), mode.as_ptr(), mode.len()) };
    if id >= 0 {
        Ok(Box::new(ImportedFileHandle::new(id)))
    } else if id == -1 {
        Err(io::Error::new(
            io::ErrorKind::Other,
            "host file open failed",
        ))
    } else {
        Err(io::Error::from_raw_os_error(-(id + 1)))
    }
}

fn deterministic_entropy() -> u64 {
    0x4c75_612d_7761_736d
}

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
fn imported_host_hooks() -> HostHooks {
    HostHooks::new()
        .stdout(imported_stdout)
        .stderr(imported_stdout)
        .stdin(imported_stdin)
        .unix_time(imported_time)
        .env(imported_env)
        .file_loader(imported_file_loader)
        .file_open(imported_file_open)
        .entropy(deterministic_entropy)
}

#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
fn imported_host_hooks() -> HostHooks {
    HostHooks::new().entropy(deterministic_entropy)
}

/// Create a fresh runtime with the host hooks, the currently-selected language
/// version, and the currently-configured sandbox limits (if any) installed, so
/// a fresh budget and the chosen backend are in force after every create/reset.
fn new_configured_runtime() -> Result<LuaRuntime, LuaError> {
    let version = SELECTED_VERSION.with(|cell| *cell.borrow());
    let mut runtime = LuaRuntime::with_hooks_versioned(imported_host_hooks(), version)?;
    SANDBOX_CFG.with(|cfg| -> Result<(), LuaError> {
        if let Some(config) = cfg.borrow().as_ref() {
            runtime.install_sandbox(config.clone())?;
        }
        Ok(())
    })?;
    Ok(runtime)
}

fn with_runtime<T>(f: impl FnOnce(&mut LuaRuntime) -> Result<T, LuaError>) -> Result<T, LuaError> {
    RUNTIME.with(|cell| {
        let mut runtime = cell.borrow_mut();
        if runtime.is_none() {
            *runtime = Some(new_configured_runtime()?);
        }
        let Some(runtime) = runtime.as_mut() else {
            return Err(LuaError::Memory);
        };
        f(runtime)
    })
}

fn reset_runtime() -> Result<(), LuaError> {
    RUNTIME.with(|cell| {
        *cell.borrow_mut() = Some(new_configured_runtime()?);
        Ok(())
    })
}

#[no_mangle]
pub extern "C" fn lua_rs_wasm_alloc(len: usize) -> *mut u8 {
    if len == 0 {
        return std::ptr::null_mut();
    }
    let mut buf: Vec<u8> = Vec::with_capacity(len);
    let ptr = buf.as_mut_ptr();
    std::mem::forget(buf);
    ptr
}

#[no_mangle]
pub extern "C" fn lua_rs_wasm_dealloc(ptr: *mut u8, len: usize) {
    if ptr.is_null() || len == 0 {
        return;
    }
    // SAFETY: `ptr` must have been returned by `lua_rs_wasm_alloc` with the
    // same `len`. Rebuilding a zero-length Vec with that capacity releases the
    // allocation without reading uninitialized memory.
    unsafe {
        drop(Vec::from_raw_parts(ptr, 0, len));
    }
}

#[no_mangle]
pub extern "C" fn lua_rs_wasm_run(ptr: *const u8, len: usize) -> i32 {
    if ptr.is_null() && len != 0 {
        set_last_error(b"null Lua source pointer".to_vec());
        return 0;
    }

    let source = if len == 0 {
        &[]
    } else {
        // SAFETY: the embedder passes a pointer/length pair into exported WASM
        // memory. The null case is rejected above and the slice is read-only for
        // this call.
        unsafe { slice::from_raw_parts(ptr, len) }
    };

    match with_runtime(|runtime| {
        runtime
            .exec(source, b"=wasm-hosted-script")
            .map_err(LuaError::from)
    }) {
        Ok(()) => {
            clear_last_error();
            1
        }
        Err(error) => {
            set_lua_error(error);
            0
        }
    }
}

#[no_mangle]
pub extern "C" fn lua_rs_wasm_reset() -> i32 {
    match reset_runtime() {
        Ok(()) => {
            clear_last_error();
            1
        }
        Err(error) => {
            set_lua_error(error);
            0
        }
    }
}

/// Select the Lua language version for subsequent runs and rebuild the runtime
/// on that backend.
///
/// `code` is the one-byte `luac` version byte: `0x51`..=`0x55` for Lua
/// 5.1..=5.5. The selection persists across [`lua_rs_wasm_reset`] and
/// [`lua_rs_wasm_set_limits`] (both re-create the runtime on the selected
/// backend). Resetting the runtime here means the per-version stdlib roster and
/// `_VERSION` are rebuilt for the chosen version.
///
/// Returns 1 on success, 0 on failure (unknown code, or backend init failure
/// with the error in the last-error buffer).
#[no_mangle]
pub extern "C" fn lua_rs_wasm_set_version(code: u32) -> i32 {
    let Some(version) = version_from_code(code) else {
        set_last_error(format!("unknown Lua version code: {code:#x}").into_bytes());
        return 0;
    };
    SELECTED_VERSION.with(|cell| *cell.borrow_mut() = version);
    match reset_runtime() {
        Ok(()) => {
            clear_last_error();
            1
        }
        Err(error) => {
            set_lua_error(error);
            0
        }
    }
}

/// The currently-selected Lua language version as its one-byte `luac` version
/// byte (`0x51`..=`0x55`). Lets the embedder confirm which backend a runtime is
/// speaking without re-deriving it.
#[no_mangle]
pub extern "C" fn lua_rs_wasm_version() -> u32 {
    SELECTED_VERSION.with(|cell| cell.borrow().luac_version_byte() as u32)
}

#[no_mangle]
pub extern "C" fn lua_rs_wasm_last_error_len() -> usize {
    LAST_ERROR.with(|cell| cell.borrow().len())
}

#[no_mangle]
pub extern "C" fn lua_rs_wasm_last_error_read(out_ptr: *mut u8, out_len: usize) -> i32 {
    LAST_ERROR.with(|cell| {
        let error = cell.borrow();
        if error.is_empty() {
            return 0;
        }
        if out_ptr.is_null() || out_len < error.len() || error.len() > i32::MAX as usize {
            return -1;
        }
        // SAFETY: caller provided an output buffer at least `error.len()` bytes
        // long, checked above. Source and destination do not overlap because the
        // source is the thread-local Rust-owned error buffer.
        unsafe {
            std::ptr::copy_nonoverlapping(error.as_ptr(), out_ptr, error.len());
        }
        error.len() as i32
    })
}

/// Configure the sandbox for subsequent `lua_rs_wasm_run` calls and reset the
/// runtime so the limits take effect on a fresh state.
///
/// - `max_instructions`: instruction budget; `0` = unlimited.
/// - `max_memory`: GC-byte ceiling; `0` = unlimited.
/// - `strict`: non-zero strips the host-access globals (`os.execute`, `io`,
///   `load`, `require`, `debug`, …) and applies 10M-instruction / 64 MiB
///   defaults for any limit left at `0`.
///
/// Returns 1 on success, 0 on failure (with the error in the last-error buffer).
#[no_mangle]
pub extern "C" fn lua_rs_wasm_set_limits(
    max_instructions: u64,
    max_memory: u64,
    strict: i32,
) -> i32 {
    let strict = strict != 0;
    let mut config = if strict {
        SandboxConfig::strict()
    } else {
        SandboxConfig {
            instruction_limit: None,
            memory_limit_bytes: None,
            check_interval: 1000,
            remove_globals: Vec::new(),
        }
    };
    if max_instructions != 0 {
        config.instruction_limit = Some(max_instructions);
    }
    if max_memory != 0 {
        config.memory_limit_bytes = Some(max_memory as usize);
    }

    SANDBOX_CFG.with(|cell| *cell.borrow_mut() = Some(config));

    match reset_runtime() {
        Ok(()) => {
            clear_last_error();
            1
        }
        Err(error) => {
            set_lua_error(error);
            0
        }
    }
}

/// Which sandbox limit (if any) aborted the most recent `lua_rs_wasm_run`:
/// 0 = none / ordinary error, 1 = instruction budget, 2 = memory ceiling.
#[no_mangle]
pub extern "C" fn lua_rs_wasm_last_trip() -> i32 {
    RUNTIME.with(
        |cell| match cell.borrow().as_ref().and_then(LuaRuntime::sandbox_tripped) {
            Some(TripReason::Instructions) => 1,
            Some(TripReason::Memory) => 2,
            None => 0,
        },
    )
}

/// Refill the instruction budget and clear the trip flag without recreating the
/// runtime, so the same loaded state can run another chunk. Returns 1.
#[no_mangle]
pub extern "C" fn lua_rs_wasm_sandbox_reset() -> i32 {
    RUNTIME.with(|cell| {
        if let Some(runtime) = cell.borrow().as_ref() {
            runtime.sandbox_reset();
        }
    });
    1
}
