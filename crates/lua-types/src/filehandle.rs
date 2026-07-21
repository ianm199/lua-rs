//! Minimal file-handle abstraction shared between `lua-vm` (hook type) and
//! `lua-stdlib` (io library).
//!
//! `std::fs` is banned in `lua-stdlib` by PORTING.md §1. The concrete
//! implementation (backed by `std::fs::File`) lives in `lua-cli` and is
//! installed on [`lua_vm::state::GlobalState`] via the `FileOpenHook` /
//! `FileRemoveHook` / `FileRenameHook` function pointers. Those hooks return
//! `std::io::Result` (not `LuaError`): only `std::io::Error` carries
//! `raw_os_error()`, and `io.open`/`os.remove`/`os.rename` must report the real
//! errno as their third return value the way C's `luaL_fileresult` does — a
//! `LuaError` return would drop it (#301). This trait is the shared seam that
//! lets `lua-stdlib` program against file handles without importing `std::fs`.
//!
//! On the `wasm32-unknown-unknown` host boundary there is no `io::Error` to pass
//! by value, so the `open_file` host import encodes the outcome in its `i32`
//! return: `>= 0` is a live handle id, `-1` is failure with no errno available
//! (mapped to a `raw_os_error`-less error → a 2-value `(nil, msg)` result), and
//! `<= -2` encodes `errno = -(id + 1)` (mapped via
//! `io::Error::from_raw_os_error`); the one-unit shift keeps `-1` as the
//! exclusive no-errno sentinel so EPERM (errno 1) stays representable (#305).
//! See `lua-wasm`'s `imported_file_open` and the JS host's `openFile`.
//!
//! ## Trait design
//! The trait mirrors the subset of `LuaFileOps` (defined in `lua-stdlib`) that
//! is required to run the built-in io library at the level needed for
//! `attrib.lua`-class tests: sequential write, byte-by-byte read, flush, and
//! seek. `LuaFileOps` in `lua-stdlib` extends this trait so that a single
//! concrete type (the `FsFile` in `lua-cli`) satisfies both.

use std::io::{self, SeekFrom};

/// Capabilities required by the io library from an OS file handle.
///
/// Designed to be object-safe (`Box<dyn LuaFileHandle>`). Implementations
/// backed by `std::fs::File` live in `lua-cli`; implementations for the
/// standard streams live in `lua-stdlib/src/io_lib.rs`.
pub trait LuaFileHandle: Send {
    /// Read one byte from the handle; return it as `i32`, or `-1` on EOF/error.
    fn read_byte(&mut self) -> i32;

    /// Push back a previously-read byte so the next `read_byte` returns it.
    fn unread_byte(&mut self, byte: i32);

    /// Write a byte slice; return the number of bytes actually written.
    fn write_bytes(&mut self, data: &[u8]) -> io::Result<usize>;

    /// Flush any write buffers to the underlying OS handle.
    fn flush(&mut self) -> io::Result<()>;

    /// Seek within the file.
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64>;

    /// Return the current file position without moving it.
    fn tell(&mut self) -> io::Result<u64>;

    /// Clear the error/EOF flag on the handle.
    fn clear_error(&mut self);

    /// Return `true` if the handle has a pending error.
    fn has_error(&self) -> bool;

    /// Return the last pending OS error as `(errno, message)` when available.
    ///
    /// Some real Lua programs probe platform behavior through specific errno
    /// values. LuaRocks' macOS directory detection, for example, expects
    /// reading an opened directory to report `EISDIR` rather than look like EOF.
    fn last_error_info(&self) -> Option<(i32, String)> {
        None
    }

    /// Control write buffering. Mode values mirror `file:setvbuf` option order:
    /// 0 = no buffering, 1 = full buffering, 2 = line buffering.
    fn set_buf_mode(&mut self, _mode: i32, _size: usize) -> io::Result<()> {
        Ok(())
    }
}
