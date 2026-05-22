//! Minimal file-handle abstraction shared between `lua-vm` (hook type) and
//! `lua-stdlib` (io library).
//!
//! `std::fs` and `std::io` are banned in `lua-stdlib` by PORTING.md §1. The
//! concrete implementation (backed by `std::fs::File`) lives in `lua-cli` and
//! is installed via [`crate::error::LuaError`]-returning function pointers on
//! [`lua_vm::state::GlobalState`]. This trait is the shared seam that allows
//! `lua-stdlib` to program against file handles without importing `std::fs`.
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
}

// ──────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        (new abstraction — not a direct port of a C file)
//   target_crate:  lua-types
//   confidence:    high
//   todos:         0
//   port_notes:    0
//   unsafe_blocks: 0
//   notes:         Introduced in Phase B to break the dependency cycle that
//                  would arise if `lua-vm` tried to use `LuaFileOps` from
//                  `lua-stdlib`. The concrete `FsFile` implementation in
//                  `lua-cli` implements this trait; `LuaFileOps` in
//                  `lua-stdlib` is a type alias for this same trait.
// ──────────────────────────────────────────────────────────────────────────
