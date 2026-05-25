//! Bare `wasm32-unknown-unknown` runtime smoke exports.
//!
//! This crate deliberately avoids `lua-cli` and native OS-backed defaults. It is
//! built as a `cdylib` and instantiated by `harness/wasm/unknown-smoke.mjs`.

use std::io::{self, SeekFrom};
use std::slice;
use std::sync::atomic::{AtomicUsize, Ordering};

use lua_rs_runtime::{HostHooks, LuaError, LuaFileHandle, LuaRuntime};

static OUTPUT_BYTES: AtomicUsize = AtomicUsize::new(0);
static INPUT_POS: AtomicUsize = AtomicUsize::new(0);
static INPUT_BYTES: &[u8] = b"alpha\nbeta\n";

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
#[link(wasm_import_module = "lua_rs_host")]
extern "C" {
    #[link_name = "write_stdout"]
    fn imported_write_stdout(ptr: *const u8, len: usize) -> i32;

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

fn capture_stdout(bytes: &[u8]) -> std::io::Result<()> {
    OUTPUT_BYTES.fetch_add(bytes.len(), Ordering::Relaxed);
    Ok(())
}

fn scripted_stdin(buf: &mut [u8]) -> std::io::Result<usize> {
    if buf.is_empty() {
        return Ok(0);
    }
    let pos = INPUT_POS.fetch_add(1, Ordering::Relaxed);
    if pos >= INPUT_BYTES.len() {
        return Ok(0);
    }
    buf[0] = INPUT_BYTES[pos];
    Ok(1)
}

fn deterministic_entropy() -> u64 {
    0x4c75_612d_7273_7761
}

fn scripted_env(name: &[u8]) -> Option<Vec<u8>> {
    match name {
        b"LUA_PATH_5_4" => Some(b"./from_env/?.lua".to_vec()),
        b"LUA_PATH" => Some(b"./fallback/?.lua".to_vec()),
        _ => None,
    }
}

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
fn imported_stdout(bytes: &[u8]) -> std::io::Result<()> {
    let status = unsafe { imported_write_stdout(bytes.as_ptr(), bytes.len()) };
    if status == 0 {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "host stdout callback failed",
        ))
    }
}

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
fn imported_time() -> i64 {
    unsafe { imported_unix_time() }
}

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
fn imported_env(name: &[u8]) -> Option<Vec<u8>> {
    let len = unsafe { imported_env_len(name.as_ptr(), name.len()) };
    if len < 0 {
        return None;
    }

    let mut out = vec![0; len as usize];
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
    let len = unsafe { imported_file_len(path.as_ptr(), path.len()) };
    if len < 0 {
        return Err(LuaError::runtime(format_args!(
            "host file not found: {}",
            String::from_utf8_lossy(path)
        )));
    }

    let mut out = vec![0; len as usize];
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
        let code = unsafe { imported_file_error_code(self.id) };
        let len = unsafe { imported_file_error_len(self.id) };
        if code == 0 && len <= 0 {
            return None;
        }

        let mut out = vec![0; len.max(0) as usize];
        let written = unsafe { imported_file_error_read(self.id, out.as_mut_ptr(), out.len()) };
        let msg = if written == len {
            String::from_utf8_lossy(&out).into_owned()
        } else {
            "host file error".to_string()
        };
        Some((code, msg))
    }

    fn set_buf_mode(&mut self, mode: i32, size: usize) -> io::Result<()> {
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

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
fn imported_host_hooks() -> HostHooks {
    HostHooks::new()
        .stdout(imported_stdout)
        .stderr(imported_stdout)
        .unix_time(imported_time)
        .env(imported_env)
        .file_loader(imported_file_loader)
        .file_open(imported_file_open)
        .entropy(deterministic_entropy)
}

fn new_smoke_runtime(
    with_output: bool,
    with_input: bool,
    with_env: bool,
) -> Result<LuaRuntime, ()> {
    let mut hooks = HostHooks::new().entropy(deterministic_entropy);
    if with_output {
        hooks = hooks.stdout(capture_stdout).stderr(capture_stdout);
    }
    if with_input {
        INPUT_POS.store(0, Ordering::Relaxed);
        hooks = hooks.stdin(scripted_stdin);
    }
    if with_env {
        hooks = hooks.env(scripted_env);
    }
    LuaRuntime::with_hooks(hooks).map_err(|_| ())
}

fn run_script(
    source: &[u8],
    with_output: bool,
    with_input: bool,
    with_env: bool,
) -> Result<(), ()> {
    let mut runtime = new_smoke_runtime(with_output, with_input, with_env)?;
    runtime.exec(source, b"=wasm-smoke").map_err(|_| ())
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
    unsafe {
        drop(Vec::from_raw_parts(ptr, 0, len));
    }
}

#[no_mangle]
pub extern "C" fn lua_rs_wasm_run_hosted_script(ptr: *const u8, len: usize) -> i32 {
    #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
    {
        let _ = (ptr, len);
        0
    }

    #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
    {
        if ptr.is_null() && len != 0 {
            return 0;
        }
        let source = if len == 0 {
            &[]
        } else {
            unsafe { slice::from_raw_parts(ptr, len) }
        };
        let mut runtime = match LuaRuntime::with_hooks(imported_host_hooks()) {
            Ok(runtime) => runtime,
            Err(_) => return 0,
        };
        match runtime.exec(source, b"=wasm-hosted-script") {
            Ok(()) => 1,
            Err(_) => 0,
        }
    }
}

#[no_mangle]
pub extern "C" fn lua_rs_wasm_pure_compute_smoke() -> i32 {
    let script = br#"
local function fib(n)
  if n < 2 then return n end
  return fib(n - 1) + fib(n - 2)
end
assert(fib(20) == 6765)
local t = {5, 4, 3, 2, 1}
table.sort(t)
assert(table.concat(t, ",") == "1,2,3,4,5")
assert(type(math.random()) == "number")
"#;
    match run_script(script, false, false, false) {
        Ok(()) => 1,
        Err(()) => 0,
    }
}

#[no_mangle]
pub extern "C" fn lua_rs_wasm_js_host_hook_smoke() -> i32 {
    #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
    {
        0
    }

    #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
    {
        let mut runtime = match LuaRuntime::with_hooks(imported_host_hooks()) {
            Ok(runtime) => runtime,
            Err(_) => return 0,
        };
        let script = br#"
assert(os.getenv("LUA_PATH") == "./hosted/?.lua")
assert(package.path == "./hosted-54/?.lua")
assert(os.time() == 1700000000)
local greeter = require("greeter")
assert(greeter.answer() == 42)
assert(greeter.message("wasm") == "hello wasm")
local f = assert(io.open("./hosted-54/data.txt", "r"))
assert(f:read(5) == "alpha")
assert(f:seek("cur") == 5)
assert(f:seek("set", 6) == 6)
assert(f:read("a") == "beta\n")
assert(f:seek("end", -5) == 6)
assert(f:read("a") == "beta\n")
assert(f:close())
local dir = assert(io.open("./hosted-54/dir", "r"))
local ok, msg, errno = dir:read("a")
assert(ok == false)
assert(type(msg) == "string")
assert(errno == 21)
assert(dir:close())
local out = assert(io.open("./hosted-54/out.txt", "w"))
assert(out:setvbuf("no"))
assert(out:write("from lua ", greeter.message("file")))
assert(out:setvbuf("full", 32))
assert(out:seek("set", 4) == 4)
assert(out:setvbuf("line", 16))
assert(out:write("WASM"))
assert(out:flush())
assert(out:close())
print("js host print")
io.write("js host io\n")
"#;
        match runtime.exec(script, b"=wasm-js-host-smoke") {
            Ok(()) => 1,
            Err(_) => 0,
        }
    }
}

#[no_mangle]
pub extern "C" fn lua_rs_wasm_output_hook_smoke() -> i32 {
    OUTPUT_BYTES.store(0, Ordering::Relaxed);
    let script = br#"
print("hello wasm")
io.write("hooked output\n")
"#;
    match run_script(script, true, false, false) {
        Ok(()) => OUTPUT_BYTES.load(Ordering::Relaxed) as i32,
        Err(()) => -1,
    }
}

#[no_mangle]
pub extern "C" fn lua_rs_wasm_unsupported_host_smoke() -> i32 {
    let env_script = br#"
assert(os.getenv("PATH") == nil)
"#;
    if run_script(env_script, false, false, false).is_err() {
        return 10;
    }

    let time_script = br#"
local ok_time = pcall(function() return os.time() end)
assert(ok_time == false)
"#;
    if run_script(time_script, false, false, false).is_err() {
        return 20;
    }

    let date_script = br#"
local ok_date = pcall(function() return os.date() end)
assert(ok_date == false)
"#;
    if run_script(date_script, false, false, false).is_err() {
        return 30;
    }

    let tmpname_script = br#"
local ok_tmpname = pcall(function() return os.tmpname() end)
assert(ok_tmpname == false)
"#;
    if run_script(tmpname_script, false, false, false).is_err() {
        return 40;
    }

    let tmpfile_script = br#"
local f, err = io.tmpfile()
assert(f == false and type(err) == "string")
"#;
    if run_script(tmpfile_script, false, false, false).is_err() {
        return 50;
    }

    let debug_script = br#"
local ok_debug = pcall(function() return debug.debug() end)
assert(ok_debug == false)
"#;
    if run_script(debug_script, false, false, false).is_err() {
        return 60;
    }

    1
}

#[no_mangle]
pub extern "C" fn lua_rs_wasm_input_hook_smoke() -> i32 {
    let script = br#"
assert(io.read("l") == "alpha")
assert(io.read("l") == "beta")
assert(io.read("l") == nil)
"#;
    match run_script(script, false, true, false) {
        Ok(()) => 1,
        Err(()) => 0,
    }
}

#[no_mangle]
pub extern "C" fn lua_rs_wasm_env_hook_smoke() -> i32 {
    let script = br#"
assert(os.getenv("LUA_PATH") == "./fallback/?.lua")
assert(package.path == "./from_env/?.lua")
"#;
    match run_script(script, false, false, true) {
        Ok(()) => 1,
        Err(()) => 0,
    }
}
