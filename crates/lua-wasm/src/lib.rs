//! Bare `wasm32-unknown-unknown` embedding exports for lua-rs.
//!
//! The generated module imports host capabilities through the `lua_rs_host`
//! module and exposes a tiny pointer/length ABI for loading Lua source from the
//! embedder.

use std::cell::RefCell;
use std::io::{self, SeekFrom};
use std::slice;

use lua_rs_runtime::{HostHooks, LuaError, LuaFileHandle, LuaRuntime};
use lua_types::LuaValue;

thread_local! {
    static LAST_ERROR: RefCell<Vec<u8>> = RefCell::new(Vec::new());
    static RUNTIME: RefCell<Option<LuaRuntime>> = RefCell::new(None);
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

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
fn imported_file_open(path: &[u8], mode: &[u8]) -> Result<Box<dyn LuaFileHandle>, LuaError> {
    // SAFETY: `path` and `mode` are live Rust slices for this synchronous import.
    // The host returns an opaque file id and must not retain the pointers.
    let id = unsafe { imported_open_file(path.as_ptr(), path.len(), mode.as_ptr(), mode.len()) };
    if id < 0 {
        Err(LuaError::runtime(format_args!(
            "host file open failed: {}",
            String::from_utf8_lossy(path)
        )))
    } else {
        Ok(Box::new(ImportedFileHandle::new(id)))
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

fn with_runtime<T>(f: impl FnOnce(&mut LuaRuntime) -> Result<T, LuaError>) -> Result<T, LuaError> {
    RUNTIME.with(|cell| {
        let mut runtime = cell.borrow_mut();
        if runtime.is_none() {
            *runtime = Some(LuaRuntime::with_hooks(imported_host_hooks())?);
        }
        let Some(runtime) = runtime.as_mut() else {
            return Err(LuaError::Memory);
        };
        f(runtime)
    })
}

fn reset_runtime() -> Result<(), LuaError> {
    RUNTIME.with(|cell| {
        *cell.borrow_mut() = Some(LuaRuntime::with_hooks(imported_host_hooks())?);
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

    match with_runtime(|runtime| runtime.exec(source, b"=wasm-hosted-script")) {
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
