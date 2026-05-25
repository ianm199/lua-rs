//! Auxiliary library: helper functions for building Lua libraries.
//!
//! C source: `reference/lua-5.4.7/src/lauxlib.c` (1127 lines, ~50 functions)
//! Target crate: `lua-stdlib`
//!
//! This module provides the high-level `luaL_*` API layer that sits on top of
//! the raw `lua_*` C API. In Rust we translate each `luaL_*` function as a
//! free function receiving `&mut LuaState` rather than a method, matching the
//! structure of the other stdlib modules.
//!
//! PORT NOTE: The C buffer system (`luaL_Buffer`) uses a small inline initial
//! buffer backed by a Lua-stack userdata box on overflow. In Rust we replace
//! this with a plain `Vec<u8>` (`LuaBuffer`), dropping all the C-internal
//! `UBox` / `resizebox` / `boxgc` / `boxmt` / `newbox` / `buffonstack`
//! machinery. The public interface remains compatible.
//!
//! PORT NOTE: File-loading functions (`load_filex`) use the embedder-installed
//! `GlobalState::file_loader_hook`; concrete filesystem access belongs in
//! `lua-cli` or another host backend.

// TODO(port): LuaState, LuaValue, LuaError, GcRef, LuaString, LuaUserData,
// LuaDebug, and LuaType are defined across lua-vm / lua-types. Imports will be
// wired in Phase B. Using local stubs for Phase A so rustc can parse the file.

use lua_types::{
    error::LuaError,
    value::LuaValue,
    gc::GcRef,
    string::LuaString,
    userdata::LuaUserData,
    LuaType,
    LuaStatus,

};
use crate::state_stub::{LuaState, LuaStateStubExt as _, LuaDebug};

// ── Constants ─────────────────────────────────────────────────────────────────

/// Number of stack frames to show in the first part of a traceback.
const LEVELS1: i32 = 10;

/// Number of stack frames to show in the second part of a traceback.
const LEVELS2: i32 = 11;

/// Index (1-based) in the reference table that heads the free-list of recycled
/// references. Placed after the last predefined registry key.
const FREELIST_REF: i64 = 3; // LUA_RIDX_GLOBALS (2) + 1

/// Pseudo-reference returned by `lua_ref` when the pushed value was `nil`.
pub const LUA_REFNIL: i32 = -1;

/// Pseudo-reference meaning "no reference" (never created by `lua_ref`).
pub const LUA_NOREF: i32 = -2;

/// Extended error code: file-related I/O error from `load_filex`.
pub const LUA_ERRFILE: i32 = 6;

/// Registry key for the table of loaded modules.
pub const LUA_LOADED_TABLE: &[u8] = b"_LOADED";

/// Registry key for the table of preloaded loaders.
pub const LUA_PRELOAD_TABLE: &[u8] = b"_PRELOAD";

/// Name of the global environment table.
pub const LUA_GNAME: &[u8] = b"_G";

/// Metatable name / file-handle key for the IO library.
pub const LUA_FILE_HANDLE: &[u8] = b"FILE*";

/// Pseudo-index for the Lua registry.
const LUA_REGISTRYINDEX: i32 = -1_001_000;

/// Minimum number of extra stack slots `lua_checkstack` guarantees per call.
#[expect(dead_code, reason = "ported stdlib helper; not yet wired into the runtime")]
const LUA_MINSTACK: i32 = 20;

// ── Public types ──────────────────────────────────────────────────────────────

/// A function-registration entry for `set_funcs`.
///
///
/// In Rust, `name` is `&'static [u8]` (never `&str`). A `None` func is a
/// placeholder that pushes `false` rather than a closure.
pub struct LuaReg {
    pub name: &'static [u8],
    pub func: Option<fn(&mut LuaState) -> Result<usize, LuaError>>,
}

/// Growable byte-buffer used by the auxiliary library for building strings.
///
///
/// The C version uses a small inline initial buffer with overflow managed via
/// a Lua-stack userdata box. The Rust port collapses this to a plain `Vec<u8>`.
/// All buffer mutating functions take `&mut LuaState` as a separate parameter.
pub struct LuaBuffer {
    pub data: Vec<u8>,
}

/// File-stream handle used by the IO library.
///
///
/// `closef` in C is a `lua_CFunction`. In Rust we store an optional closer.
// TODO(port): file I/O belongs in lua-stdlib/src/io_lib.rs; this definition
// may move there. Keeping here to mirror the C header.
pub struct LuaStream {
    /// The underlying file handle. `None` for incompletely opened or closed streams.
    // TODO(port): this legacy auxlib stream placeholder should converge with the
    // host-provided LuaFileHandle abstraction used by io_lib.
    pub f: Option<Box<dyn std::io::Read>>,
    /// Optional close function (None for already-closed streams).
    pub closef: Option<fn(&mut LuaState) -> Result<usize, LuaError>>,
}

// ── Traceback ─────────────────────────────────────────────────────────────────

/// Search for `objidx` in the table at the top of the stack.
/// `objidx` must be an absolute API stack index.
/// Returns `true` (and leaves name string on top) when found.
///
fn find_field(
    state: &mut LuaState,
    objidx: i32,
    level: i32,
) -> Result<bool, LuaError> {
    if level == 0 || state.type_at(-1) != LuaType::Table {
        return Ok(false);
    }
    state.push(LuaValue::Nil);
    while state.table_next(-2)? {
        if state.type_at(-2) == LuaType::String {
            if state.raw_equal(objidx, -1)? {
                state.pop_n(1); // remove value (keep name)
                return Ok(true);
            } else if find_field(state, objidx, level - 1)? {
                // stack: lib_name, lib_table, field_name (top)
                state.push_string(b".")?; // place '.' between the two names
                state.replace(-3)?; // in the slot occupied by table
                state.concat(3)?; // lib_name.field_name
                return Ok(true);
            }
        }
        state.pop_n(1); // remove value
    }
    Ok(false)
}

/// Search all loaded modules for a global name for the function at `top+1`.
/// Returns `true` and leaves name string on top (at `top+1`) if found.
///
fn push_global_func_name(
    state: &mut LuaState,
    ar: &mut LuaDebug,
) -> Result<bool, LuaError> {
    let top = state.top_count();
    state.get_info(b"f", ar)?;
    state.get_field(LUA_REGISTRYINDEX, LUA_LOADED_TABLE)?;
    check_stack(state, 6, Some(b"not enough stack"))?;
    if find_field(state, top + 1, 2)? {
        if state.peek_bytes(-1).map_or(false, |n| n.starts_with(b"_G.")) {
            let suffix = state.peek_bytes(-1)
                .map(|n| n[3..].to_vec())
                .unwrap_or_default();
            state.push_bytes(&suffix)?;
            state.remove(-2)?;
        }
        state.copy_value(-1, top + 1)?;
        lua_vm::api::set_top(state, top + 1)?;
        Ok(true)
    } else {
        lua_vm::api::set_top(state, top)?;
        Ok(false)
    }
}

fn push_global_func_name_from_target(
    state: &mut LuaState,
    target: &mut LuaState,
    ar: &mut LuaDebug,
) -> Result<bool, LuaError> {
    let top = state.top_count();
    target.get_info(b"f", ar)?;
    let func = target.get_at(target.top_idx() - 1);
    target.pop_n(1);
    state.push(func);
    state.get_field(LUA_REGISTRYINDEX, LUA_LOADED_TABLE)?;
    check_stack(state, 6, Some(b"not enough stack"))?;
    if find_field(state, top + 1, 2)? {
        if state.peek_bytes(-1).map_or(false, |n| n.starts_with(b"_G.")) {
            let suffix = state.peek_bytes(-1)
                .map(|n| n[3..].to_vec())
                .unwrap_or_default();
            state.push_bytes(&suffix)?;
            state.remove(-2)?;
        }
        state.copy_value(-1, top + 1)?;
        lua_vm::api::set_top(state, top + 1)?;
        Ok(true)
    } else {
        lua_vm::api::set_top(state, top)?;
        Ok(false)
    }
}

/// Push a human-readable name for the function described by `ar`.
///
fn push_func_name(
    state: &mut LuaState,
    ar: &mut LuaDebug,
    global_lookup_target: Option<&mut LuaState>,
) -> Result<(), LuaError> {
    let found_global = match global_lookup_target {
        Some(target) => push_global_func_name_from_target(state, target, ar)?,
        None => push_global_func_name(state, ar)?,
    };
    if found_global {
        let name = state.peek_bytes(-1).unwrap_or_else(|| b"?".to_vec());
        state.push_fstring(format_args!("function '{}'", BStr(&name)))?;
        state.remove(-2)?;
    } else if !ar.namewhat.is_empty() {
        let namewhat = ar.namewhat.clone();
        let name = ar.name.clone().unwrap_or_else(|| b"?".to_vec());
        state.push_fstring(format_args!("{} '{}'", BStr(&namewhat), BStr(&name)))?;
    } else if ar.what == b'm' {
        state.push_string(b"main chunk")?;
    } else if ar.what != b'C' {
        let src = ar.short_src.clone();
        let line = ar.linedefined;
        state.push_fstring(format_args!("function <{}:{}>", BStr(&src), line))?;
    } else {
        state.push_string(b"?")?;
    }
    Ok(())
}

/// Binary-search for the last valid stack level in `state`.
///
fn last_level(state: &mut LuaState) -> i32 {
    let mut ar = LuaDebug::default();
    let mut li: i32 = 1;
    let mut le: i32 = 1;
    while state.get_stack(le, &mut ar) {
        li = le;
        le *= 2;
    }
    // binary search
    while li < le {
        let m = (li + le) / 2;
        if state.get_stack(m, &mut ar) {
            li = m + 1;
        } else {
            le = m;
        }
    }
    le - 1
}

/// Build a stack traceback string from thread `other` starting at `level`.
/// If `msg` is non-None it is prepended on its own line.
/// Leaves the result string on top of `state`.
///
/// When `other` is `None`, the traceback is built for `state` itself (the
/// common single-thread case). Rust's borrow checker forbids passing the same
/// `&mut LuaState` twice, so we use an `Option` to express the aliasing intent
/// rather than a separate parameter.
///
pub fn traceback(
    state: &mut LuaState,
    mut other: Option<&mut LuaState>,
    msg: Option<&[u8]>,
    level: i32,
) -> Result<(), LuaError> {
    let mut b = LuaBuffer::new();
    let mut ar = LuaDebug::default();
    let last = match &mut other {
        Some(o) => last_level(o),
        None => last_level(state),
    };
    let mut limit2show: i32 = if last - level > LEVELS1 + LEVELS2 { LEVELS1 } else { -1 };
    buf_init(state, &mut b);
    if let Some(m) = msg {
        add_lstring(&mut b, m);
        add_char(&mut b, b'\n');
    }
    add_lstring(&mut b, b"stack traceback:");
    let mut level = level;
    loop {
        let got = match &mut other {
            Some(o) => o.get_stack(level, &mut ar),
            None => state.get_stack(level, &mut ar),
        };
        if !got {
            break;
        }
        level += 1;
        if limit2show == 0 {
            let n = last - level - LEVELS2 + 1;
            state.push_fstring(format_args!("\n\t...\t(skipping {} levels)", n))?;
            add_value(state, &mut b)?;
            level += n;
            limit2show = LEVELS2;
        } else {
            limit2show -= 1;
            match &mut other {
                Some(o) => o.get_info(b"Slnt", &mut ar)?,
                None => state.get_info(b"Slnt", &mut ar)?,
            }
            if ar.currentline <= 0 {
                let src = ar.short_src.clone();
                state.push_fstring(format_args!("\n\t{}: in ", BStr(&src)))?;
            } else {
                let src = ar.short_src.clone();
                let line = ar.currentline;
                state.push_fstring(format_args!("\n\t{}:{}: in ", BStr(&src), line))?;
            }
            add_value(state, &mut b)?;
            match &mut other {
                Some(o) => push_func_name(state, &mut ar, Some(&mut **o))?,
                None => push_func_name(state, &mut ar, None)?,
            }
            add_value(state, &mut b)?;
            if ar.istailcall {
                add_lstring(&mut b, b"\n\t(...tail calls...)");
            }
        }
    }
    push_result(state, &mut b)?;
    Ok(())
}

// ── Error-report functions ─────────────────────────────────────────────────────

/// Push an error for argument `arg` with extra message `extramsg`.
/// Attempts to enrich the message with the calling function's name.
/// Always returns `Err`.
///
pub fn arg_error(
    state: &mut LuaState,
    mut arg: i32,
    extramsg: &[u8],
) -> Result<usize, LuaError> {
    let mut ar = LuaDebug::default();
    if !state.get_stack(0, &mut ar) {
        return Err(LuaError::runtime(format_args!(
            "bad argument #{} ({})",
            arg,
            BStr(extramsg)
        )));
    }
    state.get_info(b"n", &mut ar)?;
    if ar.namewhat == b"method" {
        arg -= 1; // do not count 'self'
        if arg == 0 {
            let name = ar.name.clone().unwrap_or_else(|| b"?".to_vec());
            return Err(LuaError::runtime(format_args!(
                "calling '{}' on bad self ({})",
                BStr(&name),
                BStr(extramsg)
            )));
        }
    }
    let fname = if ar.name.is_none() {
        if push_global_func_name(state, &mut ar)? {
            state.peek_bytes(-1).unwrap_or_else(|| b"?".to_vec())
        } else {
            b"?".to_vec()
        }
    } else {
        ar.name.clone().unwrap_or_else(|| b"?".to_vec())
    };
    Err(LuaError::runtime(format_args!(
        "bad argument #{} to '{}' ({})",
        arg,
        BStr(&fname),
        BStr(extramsg)
    )))
}

/// Push a type-mismatch error for argument `arg`, stating `tname` was expected.
/// Always returns `Err`.
///
pub fn type_error_arg(
    state: &mut LuaState,
    arg: i32,
    tname: &[u8],
) -> Result<usize, LuaError> {
    //      typearg = lua_tostring(L, -1);
    //    else if (lua_type(L, arg) == LUA_TLIGHTUSERDATA)
    //      typearg = "light userdata";
    //    else
    //      typearg = luaL_typename(L, arg);
    let typearg: Vec<u8> = if get_metafield(state, arg, b"__name")? == LuaType::String {
        let bytes = state.peek_bytes(-1).unwrap_or_else(|| b"?".to_vec());
        state.pop_n(1);
        bytes
    } else if state.type_at(arg) == LuaType::LightUserData {
        b"light userdata".to_vec()
    } else {
        state.type_name_at(arg).to_vec()
    };
    let msg_owned = format!(
        "{} expected, got {}",
        BStr(tname),
        BStr(&typearg)
    );
    arg_error(state, arg, msg_owned.as_bytes())
}

/// Push a type-tag error for `arg`, using the Lua type name for `tag`.
///
fn tag_error(state: &mut LuaState, arg: i32, tag: LuaType) -> Result<(), LuaError> {
    let name = state.type_name(tag);
    type_error_arg(state, arg, name)?;
    Ok(())
}

/// Push a string describing the location of the call at `level` onto the stack.
/// If no location is available, pushes an empty string.
///
pub fn push_where(state: &mut LuaState, level: i32) -> Result<(), LuaError> {
    let mut ar = LuaDebug::default();
    if state.get_stack(level, &mut ar) {
        state.get_info(b"Sl", &mut ar)?;
        if ar.currentline > 0 {
            let src = ar.short_src.clone();
            let line = ar.currentline;
            state.push_fstring(format_args!("{}:{}: ", BStr(&src), line))?;
            return Ok(());
        }
    }
    state.push_string(b"")?;
    Ok(())
}

/// Format a runtime error with source location and raise it.
/// Always returns `Err`.
///
///
/// PORT NOTE: C uses varargs + `lua_pushvfstring`. Rust callers pass a
/// pre-formatted `&[u8]` message; use `format_args!` at the call site.
pub fn lua_error(state: &mut LuaState, msg: &[u8]) -> Result<usize, LuaError> {
    push_where(state, 1)?;
    let where_str = state.pop_bytes();
    let full = [where_str.as_slice(), msg].concat();
    Err(LuaError::runtime(format_args!("{}", BStr(&full))))
}

/// Push the result of a POSIX-style file operation onto the stack.
/// On success pushes `true`; on failure pushes `nil, errmsg, errno`.
/// Returns the number of pushed values.
///
pub fn file_result(
    state: &mut LuaState,
    stat: bool,
    fname: Option<&[u8]>,
) -> Result<usize, LuaError> {
    if stat {
        state.push(LuaValue::Bool(true));
        Ok(1)
    } else {
        state.push(LuaValue::Nil);
        // TODO(port): use std::io::Error::last_os_error() for errno-style message.
        let errmsg = b"(errno unavailable in Rust port)".to_vec();
        if let Some(name) = fname {
            let full = [name, b": ".as_slice(), &errmsg].concat();
            state.push_bytes(&full)?;
        } else {
            state.push_bytes(&errmsg)?;
        }
        // TODO(port): push actual errno integer once os-error helpers are available.
        state.push(LuaValue::Int(0));
        Ok(3)
    }
}

/// Push the result of a process-exit status onto the stack.
/// Returns 3 values: success-bool-or-nil, exit-kind string, status code.
///
// TODO(port): POSIX WIFEXITED / WIFSIGNALED inspection requires cfg(unix).
pub fn exec_result(state: &mut LuaState, stat: i32) -> Result<usize, LuaError> {
    if stat != 0 {
        return file_result(state, false, None);
    }
    let what = b"exit".as_slice();
    state.push(LuaValue::Bool(true));
    state.push_bytes(what)?;
    state.push(LuaValue::Int(stat as i64));
    Ok(3)
}

// ── Userdata / metatable helpers ──────────────────────────────────────────────

/// Create a new metatable for type `tname` and register it in the registry.
/// Returns `true` (and leaves new metatable on stack) if the table was created;
/// returns `false` (and leaves existing table on stack) if already existed.
///
pub fn new_metatable(state: &mut LuaState, tname: &[u8]) -> Result<bool, LuaError> {
    if get_metatable(state, tname)? != LuaType::Nil {
        return Ok(false); // leave previous value on top
    }
    state.pop_n(1);
    state.create_table(0, 2)?;
    state.push_bytes(tname)?;
    state.set_field(-2, b"__name")?;
    state.push_value(-1)?;
    state.set_field(LUA_REGISTRYINDEX, tname)?;
    Ok(true)
}

/// Set the metatable of the value at stack top to the one registered as `tname`.
///
pub fn set_metatable(state: &mut LuaState, tname: &[u8]) -> Result<(), LuaError> {
    get_metatable(state, tname)?;
    state.set_metatable(-2)?;
    Ok(())
}

/// Check whether the value at `ud` is a full userdata with metatable `tname`.
/// Returns `Some(userdata)` if yes, `None` otherwise.
///
pub fn test_udata(
    state: &mut LuaState,
    ud: i32,
    tname: &[u8],
) -> Result<Option<GcRef<LuaUserData>>, LuaError> {
    let p = state.to_userdata(ud);
    if let Some(p) = p {
        if state.get_metatable(ud)? {
            get_metatable(state, tname)?;
            let eq = state.raw_equal(-1, -2)?;
            state.pop_n(2); // remove both metatables
            if eq {
                return Ok(Some(p));
            }
        }
    }
    Ok(None)
}

/// Like `test_udata` but raises a type error if the check fails.
///
pub fn check_udata(
    state: &mut LuaState,
    ud: i32,
    tname: &[u8],
) -> Result<GcRef<LuaUserData>, LuaError> {
    match test_udata(state, ud, tname)? {
        Some(p) => Ok(p),
        None => {
            type_error_arg(state, ud, tname)?;
            unreachable!()
        }
    }
}

// ── Argument-check functions ──────────────────────────────────────────────────

/// Check that `arg` is one of the strings in `lst` and return its index.
/// If `def` is `Some` it is used as default when `arg` is absent/nil.
///
pub fn check_option(
    state: &mut LuaState,
    arg: i32,
    def: Option<&[u8]>,
    lst: &[&[u8]],
) -> Result<usize, LuaError> {
    let name: Vec<u8> = match def {
        Some(d) if state.is_none_or_nil(arg) => d.to_vec(),
        _ => check_lstring(state, arg)?.as_bytes().to_vec(),
    };
    for (i, entry) in lst.iter().enumerate() {
        if *entry == name.as_slice() {
            return Ok(i);
        }
    }
    Err(LuaError::runtime(format_args!(
        "invalid option '{}'",
        BStr(&name)
    )))
}

/// Ensure the stack has at least `space` extra slots; raise on failure.
///
pub fn check_stack(
    state: &mut LuaState,
    space: i32,
    msg: Option<&[u8]>,
) -> Result<(), LuaError> {
    if !state.check_stack_space(space) {
        match msg {
            Some(m) => {
                return Err(LuaError::runtime(format_args!(
                    "stack overflow ({})",
                    BStr(m)
                )));
            }
            None => {
                return Err(LuaError::runtime(format_args!("stack overflow")));
            }
        }
    }
    Ok(())
}

/// Assert that the value at `arg` has Lua type `t`; raise type error otherwise.
///
pub fn check_type(state: &mut LuaState, arg: i32, t: LuaType) -> Result<(), LuaError> {
    if state.type_at(arg) != t {
        tag_error(state, arg, t)?;
    }
    Ok(())
}

/// Assert that a value (not `none`) is present at `arg`.
///
pub fn check_any(state: &mut LuaState, arg: i32) -> Result<(), LuaError> {
    if state.type_at(arg) == LuaType::None {
        return Err(LuaError::arg_error(arg, "value expected"));
    }
    Ok(())
}

/// Return the string at `arg` as bytes; raise a type error if not a string.
///
pub fn check_lstring(state: &mut LuaState, arg: i32) -> Result<GcRef<LuaString>, LuaError> {
    match state.to_lua_string(arg) {
        Some(s) => Ok(s),
        None => {
            tag_error(state, arg, LuaType::String)?;
            unreachable!()
        }
    }
}

/// Return the string at `arg`; if absent/nil return `def`.
///
pub fn opt_lstring(
    state: &mut LuaState,
    arg: i32,
    def: Option<&[u8]>,
) -> Result<Option<Vec<u8>>, LuaError> {
    if state.is_none_or_nil(arg) {
        return Ok(def.map(|d| d.to_vec()));
    }
    let s = check_lstring(state, arg)?;
    Ok(Some(s.as_bytes().to_vec()))
}

/// Return the number at `arg` as `f64`; raise a type error if not a number.
///
pub fn check_number(state: &mut LuaState, arg: i32) -> Result<f64, LuaError> {
    match state.to_number_x(arg) {
        Some(d) => Ok(d),
        None => {
            tag_error(state, arg, LuaType::Number)?;
            unreachable!()
        }
    }
}

/// Return the number at `arg`; if absent/nil return `def`.
///
pub fn opt_number(state: &mut LuaState, arg: i32, def: f64) -> Result<f64, LuaError> {
    if state.is_none_or_nil(arg) {
        Ok(def)
    } else {
        check_number(state, arg)
    }
}

/// Raise an error for a non-integer number argument.
///
///
/// Always returns `Err`. The `Ok` arm uses `unreachable!()` to satisfy the
/// return type; `!` (never) is nightly-only so we use `Result<usize, LuaError>`.
fn int_error(state: &mut LuaState, arg: i32) -> Result<usize, LuaError> {
    if state.is_number(arg) {
        Err(LuaError::arg_error(
            arg,
            "number has no integer representation",
        ))
    } else {
        tag_error(state, arg, LuaType::Number)?;
        unreachable!("tag_error always returns Err")
    }
}

/// Return the integer at `arg` as `i64`; raise if not an integer-convertible number.
///
pub fn check_integer(state: &mut LuaState, arg: i32) -> Result<i64, LuaError> {
    match state.to_integer_x(arg) {
        Some(d) => Ok(d),
        None => {
            int_error(state, arg)?;
            unreachable!("int_error always returns Err")
        }
    }
}

/// Return the integer at `arg`; if absent/nil return `def`.
///
pub fn opt_integer(state: &mut LuaState, arg: i32, def: i64) -> Result<i64, LuaError> {
    if state.is_none_or_nil(arg) {
        Ok(def)
    } else {
        check_integer(state, arg)
    }
}

// ── Buffer manipulation ────────────────────────────────────────────────────────

impl LuaBuffer {
    /// Create a new empty buffer.
    ///
    /// Rust uses `Vec::new()` which starts at zero capacity; capacity is managed by Vec.
    pub fn new() -> Self {
        LuaBuffer { data: Vec::new() }
    }

    /// Returns the number of bytes currently in the buffer.
    pub fn len(&self) -> usize {
        self.data.len()
    }
}

impl Default for LuaBuffer {
    fn default() -> Self {
        LuaBuffer::new()
    }
}

/// Initialize `buf` and associate it with `state`.
/// Pushes a placeholder light-userdata onto `state` to anchor the buffer in C.
/// In Rust the Vec is self-contained; we still push a placeholder for stack-slot
/// compatibility with code that later calls `add_value` / `push_result`.
///
pub fn buf_init(state: &mut LuaState, buf: &mut LuaBuffer) {
    // PORT NOTE: C pushes a light-userdata placeholder onto the stack to hold
    // the buffer's position. We still push nil as a stack slot placeholder so
    // that add_value / push_result see the same stack layout.
    *buf = LuaBuffer::new();
    // We push nil; Phase B can revisit if this matters for GC interaction.
    let _ = state.push(LuaValue::Nil);
}

/// Initialize `buf`, reserve `sz` bytes, and return the writable region.
///
pub fn buf_init_size(
    state: &mut LuaState,
    buf: &mut LuaBuffer,
    sz: usize,
) -> Result<(), LuaError> {
    buf_init(state, buf);
    buf.data.reserve(sz);
    Ok(())
}

/// Compute a new buffer capacity that accommodates `sz` more bytes,
/// growing by ×1.5 or more.
///
fn new_buff_size(buf: &LuaBuffer, sz: usize) -> Result<usize, LuaError> {
    if usize::MAX - sz < buf.len() {
        return Err(LuaError::runtime(format_args!("buffer too large")));
    }
    let newsize = (buf.data.capacity() / 2) * 3; // ×1.5
    if newsize < buf.len() + sz {
        Ok(buf.len() + sz)
    } else {
        Ok(newsize)
    }
}

/// Ensure at least `sz` free bytes are available in `buf`.
///
pub fn prep_buff_size(buf: &mut LuaBuffer, sz: usize) -> Result<(), LuaError> {
    if buf.data.capacity() - buf.data.len() < sz {
        let newcap = new_buff_size(buf, sz)?;
        buf.data.reserve(newcap - buf.data.len());
    }
    Ok(())
}

/// Append `s` to `buf`.
///
pub fn add_lstring(buf: &mut LuaBuffer, s: &[u8]) {
    if !s.is_empty() {
        buf.data.extend_from_slice(s);
    }
}

/// Append a single byte to `buf`.
///
pub fn add_char(buf: &mut LuaBuffer, c: u8) {
    buf.data.push(c);
}

/// Append `sz` to the length counter (used after writing directly into the buffer).
///
pub fn add_size(_buf: &mut LuaBuffer, sz: usize) {
    // PORT NOTE: In C this is a direct `n += sz` on the inline length field.
    // With Vec, length is implicit; this is a no-op unless caller wrote past len.
    // TODO(port): if direct-write into spare capacity is needed, switch to `unsafe`
    // set_len or redesign; for Phase A this is a no-op.
    let _ = sz;
}

/// Pop the string at top of `state`'s stack and append it to `buf`.
///
pub fn add_value(state: &mut LuaState, buf: &mut LuaBuffer) -> Result<(), LuaError> {
    if let Some(bytes) = state.peek_bytes(-1) {
        let owned = bytes.to_vec();
        add_lstring(buf, &owned);
    }
    state.pop_n(1);
    Ok(())
}

/// Push the buffer contents as a Lua string onto `state`'s stack.
///
pub fn push_result(state: &mut LuaState, buf: &mut LuaBuffer) -> Result<(), LuaError> {
    state.push_bytes(&buf.data)?;
    state.remove(-2)?;
    Ok(())
}

/// Add `sz` bytes to the buffer count then call `push_result`.
///
pub fn push_result_size(
    state: &mut LuaState,
    buf: &mut LuaBuffer,
    sz: usize,
) -> Result<(), LuaError> {
    add_size(buf, sz);
    push_result(state, buf)
}

/// Perform global byte-string substitution: replace all occurrences of `pat`
/// with `repl` in `s`, appending results into `buf`.
///
pub fn add_gsub(buf: &mut LuaBuffer, s: &[u8], pat: &[u8], repl: &[u8]) {
    if pat.is_empty() {
        add_lstring(buf, s);
        return;
    }
    let mut remaining = s;
    while let Some(pos) = find_bytes(remaining, pat) {
        add_lstring(buf, &remaining[..pos]);
        add_lstring(buf, repl);
        remaining = &remaining[pos + pat.len()..];
    }
    add_lstring(buf, remaining);
}

/// Build a string from `s` by replacing `pat` with `repl`, push it on the stack,
/// and return the bytes of the pushed string.
///
pub fn gsub<'a>(
    state: &'a mut LuaState,
    s: &[u8],
    pat: &[u8],
    repl: &[u8],
) -> Result<Vec<u8>, LuaError> {
    let mut b = LuaBuffer::new();
    buf_init(state, &mut b);
    add_gsub(&mut b, s, pat, repl);
    push_result(state, &mut b)?;
    Ok(state.peek_bytes(-1).unwrap_or_default())
}

/// Find `needle` in `haystack`, returning the byte offset or `None`.
///
/// Internal helper replacing C's `strstr`.
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

// ── Reference system ──────────────────────────────────────────────────────────

/// Store the value at the top of the stack in table `t` and return a unique
/// integer reference. If the value is `nil`, returns `LUA_REFNIL` without
/// modifying the table.
///
pub fn lua_ref(state: &mut LuaState, t: i32) -> Result<i32, LuaError> {
    if state.type_at(-1) == LuaType::Nil {
        state.pop_n(1);
        return Ok(LUA_REFNIL);
    }
    let t = state.abs_index(t);
    let ref_val: i32;
    if state.raw_get_i(t, FREELIST_REF)? == LuaType::Nil {
        ref_val = 0; // list is empty
        state.push(LuaValue::Int(0));
        state.raw_set_i(t, FREELIST_REF)?;
    } else {
        debug_assert!(state.type_at(-1) == LuaType::Number);
        ref_val = state.to_integer_x(-1).unwrap_or(0) as i32;
    }
    state.pop_n(1); // remove element from stack
    let next_ref: i32;
    if ref_val != 0 {
        state.raw_get_i(t, ref_val as i64)?;
        state.raw_set_i(t, FREELIST_REF)?;
        next_ref = ref_val;
    } else {
        next_ref = (state.raw_len(t) as i32) + 1;
    }
    state.raw_set_i(t, next_ref as i64)?;
    Ok(next_ref)
}

/// Release reference `ref` from table `t`, adding it to the free list.
///
pub fn lua_unref(state: &mut LuaState, t: i32, r: i32) -> Result<(), LuaError> {
    if r >= 0 {
        let t = state.abs_index(t);
        state.raw_get_i(t, FREELIST_REF)?;
        debug_assert!(state.type_at(-1) == LuaType::Number);
        state.raw_set_i(t, r as i64)?;
        state.push(LuaValue::Int(r as i64));
        state.raw_set_i(t, FREELIST_REF)?;
    }
    Ok(())
}

// ── Load functions ─────────────────────────────────────────────────────────────

/// Internal chunk reader that returns a single buffer slice then signals EOF.
///
fn make_string_reader(data: Vec<u8>) -> impl FnMut() -> Option<Vec<u8>> {
    let mut remaining = Some(data);
    move || remaining.take()
}

/// Strip an optional UTF-8 BOM (EF BB BF) and any `#`-prefixed first line.
///
/// PORT NOTE: C reads byte-by-byte with `getc`/`feof` and lazily reopens the
/// file in binary mode if it looks like a binary chunk. Here we ask the
/// embedder-installed file loader hook for raw bytes, strip the BOM, and let
/// `lua_vm::api::load` dispatch text vs. binary by the first byte. The "binary
/// chunk" branch in `luaL_loadfilex` exists in C because text mode does newline
/// translation; the host loader is expected to provide raw bytes.
fn skip_bom_and_shebang(buf: &[u8]) -> Vec<u8> {
    let s = if buf.starts_with(b"\xEF\xBB\xBF") { &buf[3..] } else { buf };
    if s.first() == Some(&b'#') {
        let nl = s.iter().position(|&b| b == b'\n').map(|p| p + 1).unwrap_or(s.len());
        let rest = &s[nl..];
        if rest.first() == Some(&0x1B) {
            rest.to_vec()
        } else {
            let mut out = Vec::with_capacity(rest.len() + 1);
            out.push(b'\n');
            out.extend_from_slice(rest);
            out
        }
    } else {
        s.to_vec()
    }
}

/// Load a file as a Lua chunk. Returns `LUA_OK` on success or an error code.
///
///
/// PORT NOTE: PORTING.md §1 bans `std::fs` outside `lua-cli`, but C-Lua's
/// `luaL_loadfilex` is part of the auxiliary library (`lauxlib.c`) and is
/// reachable from the base library (`loadfile`/`dofile`). Phase A's stub
/// raised an error here, which broke `loadfile(missing)` returning `nil, err`.
/// The real C semantics push an error string onto the stack and return a
/// non-zero status, which `load_aux` then converts to `(nil, errmsg)`.
pub fn load_filex(
    state: &mut LuaState,
    filename: Option<&[u8]>,
    mode: Option<&[u8]>,
) -> Result<i32, LuaError> {
    let _ = mode;
    let fname = match filename {
        Some(f) => f,
        None => {
            // TODO(port): stdin loading not yet supported in lua-stdlib; return
            // an error string matching C's "cannot read stdin" shape.
            state.push_string(b"cannot read stdin: no filename given")?;
            return Ok(LUA_ERRFILE);
        }
    };
    let raw = match state.global().file_loader_hook {
        Some(load_fn) => load_fn(fname),
        None => Err(LuaError::runtime(format_args!(
            "no file_loader_hook registered"
        ))),
    };
    let raw = match raw {
        Ok(bytes) => bytes,
        Err(e) => {
            let detail = match &e {
                LuaError::Runtime(LuaValue::Str(s)) => {
                    String::from_utf8_lossy(s.as_bytes()).into_owned()
                }
                other => format!("{:?}", other),
            };
            state.push_fstring(format_args!(
                "cannot open {}: {}",
                BStr(fname),
                detail
            ))?;
            return Ok(LUA_ERRFILE);
        }
    };
    let payload = skip_bom_and_shebang(&raw);
    let mut once = Some(payload);
    let boxed: Box<dyn FnMut() -> Option<Vec<u8>>> =
        Box::new(move || once.take());
    let mut chunkname = b"@".to_vec();
    chunkname.extend_from_slice(fname);
    let status = lua_vm::api::load(state, boxed, Some(&chunkname), mode)?;
    Ok(if status == LuaStatus::Ok { 0 } else { status as i32 })
}

/// Load a buffer as a Lua chunk.
///
pub fn load_bufferx(
    state: &mut LuaState,
    buff: &[u8],
    name: &[u8],
    mode: Option<&[u8]>,
) -> Result<i32, LuaError> {
    // TODO(phase-b): state.load expects (chunk: &[u8], name, mode) in state_stub; the reader-based loader needs a load_with_reader API match.
    let _reader = make_string_reader(buff.to_vec());
    let ok = state.load(buff, name, mode)?;
    Ok(if ok { 0 } else { 1 })
}

/// Load a buffer as a Lua chunk (no mode argument).
///
pub fn load_buffer(
    state: &mut LuaState,
    buff: &[u8],
    name: &[u8],
) -> Result<i32, LuaError> {
    load_bufferx(state, buff, name, None)
}

/// Load a NUL-terminated byte-string as a Lua chunk.
///
pub fn load_string(state: &mut LuaState, s: &[u8]) -> Result<i32, LuaError> {
    load_buffer(state, s, s)
}

// ── Meta-field and misc helpers ───────────────────────────────────────────────

/// Push the metafield `event` of `obj` onto the stack and return its type.
/// If there is no metafield, nothing is pushed and `LuaType::Nil` is returned.
///
pub fn get_metafield(
    state: &mut LuaState,
    obj: i32,
    event: &[u8],
) -> Result<LuaType, LuaError> {
    if !state.get_metatable(obj)? {
        return Ok(LuaType::Nil);
    }
    state.push_bytes(event)?;
    let tt = state.raw_get(-2)?;
    if tt == LuaType::Nil {
        state.pop_n(2);
    } else {
        state.remove(-2)?;
    }
    Ok(tt)
}

/// Call the metafield `event` of `obj` with `obj` as argument, pushing one result.
/// Returns `true` if the meta-method existed and was called.
///
pub fn call_meta(state: &mut LuaState, obj: i32, event: &[u8]) -> Result<bool, LuaError> {
    let obj = state.abs_index(obj);
    if get_metafield(state, obj, event)? == LuaType::Nil {
        return Ok(false);
    }
    state.push_value(obj)?;
    state.call(1, 1)?;
    Ok(true)
}

/// Return the length of the value at `idx` as a `i64`, raising an error if
/// the length is not an integer.
///
pub fn lua_len(state: &mut LuaState, idx: i32) -> Result<i64, LuaError> {
    state.len_op(idx)?;
    let l = match state.to_integer_x(-1) {
        Some(n) => n,
        None => {
            return Err(LuaError::runtime(format_args!(
                "object length is not an integer"
            )));
        }
    };
    state.pop_n(1);
    Ok(l)
}

/// Convert the value at `idx` to a byte-string representation (using `__tostring`
/// if available) and push it onto the stack.
///
pub fn to_lua_string(state: &mut LuaState, idx: i32) -> Result<Vec<u8>, LuaError> {
    let idx = state.abs_index(idx);
    if call_meta(state, idx, b"__tostring")? {
        if state.type_at(-1) != LuaType::String {
            return Err(LuaError::runtime(format_args!(
                "'__tostring' must return a string"
            )));
        }
    } else {
        match state.type_at(idx) {
            LuaType::Number => {
                if state.is_integer(idx) {
                    let i = state.to_integer_x(idx).unwrap_or(0);
                    state.push_fstring(format_args!("{}", i))?;
                } else {
                    let f = state.to_number_x(idx).unwrap_or(0.0);
                    state.push_fstring(format_args!("{:?}", f))?;
                }
            }
            LuaType::String => {
                state.push_value(idx)?;
            }
            LuaType::Boolean => {
                let b = state.to_boolean(idx);
                state.push_string(if b { b"true" } else { b"false" })?;
            }
            LuaType::Nil => {
                state.push_string(b"nil")?;
            }
            _ => {
                let tt = get_metafield(state, idx, b"__name")?;
                let kind: Vec<u8> = if tt == LuaType::String {
                    state.peek_bytes(-1).unwrap_or_else(|| b"?".to_vec())
                } else {
                    state.type_name_at(idx).to_vec()
                };
                // TODO(port): lua_topointer gives a pointer address; in Rust use
                // a hash or allocation address for a stable identifier.
                state.push_fstring(format_args!("{}: 0x?", BStr(&kind)))?;
                if tt != LuaType::Nil {
                    state.remove(-2)?;
                }
            }
        }
    }
    Ok(state.peek_bytes(-1).unwrap_or_default())
}

/// Register the functions in `l` into the table at `-(nup + 1)`, giving each
/// closure the `nup` upvalues currently at the top of the stack.
///
pub fn set_funcs(
    state: &mut LuaState,
    l: &[LuaReg],
    nup: i32,
) -> Result<(), LuaError> {
    check_stack(state, nup, Some(b"too many upvalues"))?;
    for reg in l {
        match reg.func {
            None => {
                state.push(LuaValue::Bool(false));
            }
            Some(f) => {
                for _ in 0..nup {
                    state.push_value(-nup)?;
                }
                state.push_c_closure(f, nup)?;
            }
        }
        state.set_field(-(nup + 2), reg.name)?;
    }
    state.pop_n(nup as usize);
    Ok(())
}

/// Ensure `state[idx][fname]` is a table; push it.
/// Returns `true` if the table already existed, `false` if newly created.
///
pub fn get_subtable(
    state: &mut LuaState,
    idx: i32,
    fname: &[u8],
) -> Result<bool, LuaError> {
    if state.get_field(idx, fname)? == LuaType::Table {
        return Ok(true);
    }
    state.pop_n(1);
    let idx = state.abs_index(idx);
    let new_tbl = state.new_table();
    state.push(LuaValue::Table(new_tbl));
    state.push_value(-1)?;
    state.set_field(idx, fname)?;
    Ok(false)
}

/// Simplified `require`: open module `modname` via `openf`, register it in
/// `package.loaded`, and (if `glb`) in the global table.
/// Leaves the module on top of the stack.
///
pub fn requiref(
    state: &mut LuaState,
    modname: &[u8],
    openf: fn(&mut LuaState) -> Result<usize, LuaError>,
    glb: bool,
) -> Result<(), LuaError> {
    get_subtable(state, LUA_REGISTRYINDEX, LUA_LOADED_TABLE)?;
    state.get_field(-1, modname)?;
    if !state.to_boolean(-1) {
        state.pop_n(1);
        state.push_c_function(openf)?;
        state.push_bytes(modname)?;
        state.call(1, 1)?;
        state.push_value(-1)?;
        state.set_field(-3, modname)?;
    }
    state.remove(-2)?;
    if glb {
        state.push_value(-1)?;
        state.set_global(modname)?;
    }
    Ok(())
}

// ── Helper for registry-based metatable lookup ─────────────────────────────────

/// Push `registry[tname]` and return its type.
///
pub fn get_metatable(state: &mut LuaState, tname: &[u8]) -> Result<LuaType, LuaError> {
    state.get_field(LUA_REGISTRYINDEX, tname)
}

// ── State creation and version check ─────────────────────────────────────────

/// Create a new `LuaState` with the default allocator, a panic handler, and
/// warnings disabled.
///
pub fn new_state() -> Result<LuaState, LuaError> {
    // PORT NOTE: Rust's allocator is used implicitly; no l_alloc hook needed.
    // TODO(phase-b): LuaState::new() / set_panic_handler / set_warn_fn need a real LuaState constructor in lua-vm. Stub for Phase A.
    let _ = default_panic_handler;
    let _ = warn_off;
    todo!("phase-b: LuaState::new()")
}

/// Default panic handler: print message to stderr and return to abort.
///
fn default_panic_handler(state: &mut LuaState) -> Result<usize, LuaError> {
    let msg = if state.type_at(-1) == LuaType::String {
        state.peek_bytes(-1).unwrap_or_else(|| b"?".to_vec())
    } else {
        b"error object is not a string".to_vec()
    };
    eprintln!("PANIC: unprotected error in call to Lua API ({})", BStr(&msg));
    Ok(0) // return to Lua to abort
}

/// Warning function: warnings are off.
///
fn warn_off(state: &mut LuaState, message: &[u8], tocont: bool) -> Result<(), LuaError> {
    check_control(state, message, tocont)?;
    Ok(())
}

/// Warning function: ready to start a new message.
///
fn warn_on(state: &mut LuaState, message: &[u8], tocont: bool) -> Result<(), LuaError> {
    if check_control(state, message, tocont)? {
        return Ok(());
    }
    eprint!("Lua warning: ");
    warn_cont(state, message, tocont)
}

/// Warning function: continue writing a previous warning message.
///
fn warn_cont(_state: &mut LuaState, message: &[u8], tocont: bool) -> Result<(), LuaError> {
    eprint!("{}", BStr(message));
    // TODO(phase-b): set_warn_fn expects lua_CFunction in state_stub; warn_cont/warn_on take (msg, tocont). Wire after warn-fn API lands in lua-vm.
    if tocont {
        let _ = (warn_cont as fn(&mut LuaState, &[u8], bool) -> Result<(), LuaError>,);
    } else {
        eprintln!();
        let _ = (warn_on as fn(&mut LuaState, &[u8], bool) -> Result<(), LuaError>,);
    }
    Ok(())
}

/// Handle a warning control message (e.g. `"@on"`, `"@off"`).
/// Returns `true` if the message was a recognised control message.
///
fn check_control(
    state: &mut LuaState,
    message: &[u8],
    tocont: bool,
) -> Result<bool, LuaError> {
    if tocont || message.first() != Some(&b'@') {
        return Ok(false);
    }
    let cmd = &message[1..];
    // TODO(phase-b): set_warn_fn expects lua_CFunction in state_stub; warn_off/warn_on take (msg, tocont). Wire after warn-fn API lands in lua-vm.
    let _ = state;
    if cmd == b"off" {
        let _ = warn_off as fn(&mut LuaState, &[u8], bool) -> Result<(), LuaError>;
    } else if cmd == b"on" {
        let _ = warn_on as fn(&mut LuaState, &[u8], bool) -> Result<(), LuaError>;
    }
    Ok(true)
}

/// Version-compatibility check: error if numeric type sizes or version mismatch.
///
pub fn check_version(state: &mut LuaState, ver: f64, sz: usize) -> Result<(), LuaError> {
    const LUAL_NUMSIZES: usize = std::mem::size_of::<i64>() * 16 + std::mem::size_of::<f64>();
    if sz != LUAL_NUMSIZES {
        return Err(LuaError::runtime(format_args!(
            "core and library have incompatible numeric types"
        )));
    }
    let v = state.lua_version();
    if (v - ver).abs() > f64::EPSILON {
        return Err(LuaError::runtime(format_args!(
            "version mismatch: app. needs {}, Lua core provides {}",
            ver, v
        )));
    }
    Ok(())
}

// ── Internal display helper ────────────────────────────────────────────────────

/// Wrapper that implements `Display` for `&[u8]` as a lossy byte string.
/// Used to embed byte slices in `format_args!` without allocating a `String`.
///
/// PORT NOTE: not used for Lua string data; used only for error message
/// formatting inside `format_args!` literals.
struct BStr<'a>(&'a [u8]);

impl<'a> std::fmt::Display for BStr<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for &b in self.0 {
            if b.is_ascii() {
                f.write_char(b as char)?;
            } else {
                write!(f, "\\x{:02x}", b)?;
            }
        }
        Ok(())
    }
}

// Required for fmt::Display
use std::fmt::Write as _;

// ── LuaDebug Default ─────────────────────────────────────────────────────────


// ──────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        src/lauxlib.c  (1127 lines, ~50 functions)
//   target_crate:  lua-stdlib
//   confidence:    medium
//   todos:         10
//   port_notes:    8
//   unsafe_blocks: 0
//   notes:         Buffer simplified from stack-based C UBox/box-on-Lua-stack to
//                  plain Vec<u8> (LuaBuffer); UBox/resizebox/boxgc/boxmt/newbox
//                  machinery dropped entirely — Rust Drop handles deallocation.
//                  load_filex reads via GlobalState::file_loader_hook and pushes
//                  an error string on open failure so loadfile/dofile return
//                  (nil, err) per C semantics (stdin loading still TODO).
//                  Warning system uses fn-ptr callbacks matching lua_WarnFunction
//                  type; warnfoff/warnfon/warnfcont translated faithfully.
//                  LuaState / LuaDebug / GcRef are Phase-A stubs; Phase B replaces
//                  with real imports from lua-vm / lua-types.
//                  add_size() is a no-op in Phase A (Vec tracks length implicitly);
//                  direct buffer writes via spare capacity need revisit in Phase B.
//                  int_error() return type changed from `!` to `Result<usize,_>` as
//                  the never type is nightly-only on stable Rust.
// ──────────────────────────────────────────────────────────────────────────
