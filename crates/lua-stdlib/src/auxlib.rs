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
//! PORT NOTE: File-loading functions (`load_filex`) reference `std::fs` which
//! is banned outside `lua-cli`. Those functions carry `TODO(port)` markers.

// TODO(port): LuaState, LuaValue, LuaError, GcRef, LuaString, LuaUserData,
// LuaDebug, and LuaType are defined across lua-vm / lua-types. Imports will be
// wired in Phase B. Using local stubs for Phase A so rustc can parse the file.

use lua_types::{
    error::LuaError,
    value::LuaValue,
    LuaType,
};

// ── Phase-A stubs ─────────────────────────────────────────────────────────────
// These forward-declarations let the bodies below reference the cross-crate
// types without Phase-A compilation barriers. Phase B replaces them with real
// `use` paths.

/// Per-thread Lua interpreter state.
/// C: `lua_State *` — in Rust a method receiver `&mut LuaState`.
// TODO(port): replace with `use lua_vm::state::LuaState` in Phase B.
struct LuaState;

/// Intrusive reference-counted GC handle. Phase A: alias for `Rc<T>`.
// TODO(port): replace with real GcRef from lua-gc in Phase D.
struct GcRef<T>(std::marker::PhantomData<T>);

/// Interned or heap-allocated Lua byte-string. Never `String` or `&str`.
// TODO(port): import from lua-vm::string.
struct LuaString;

/// Full userdata object.
// TODO(port): import from lua-vm::object.
struct LuaUserData;

/// Debug introspection record produced by `lua_getinfo`.
/// C: `lua_Debug` — Phase E completes this.
// TODO(port): import from lua-vm::debug.
#[allow(dead_code)]
struct LuaDebug {
    /// C: `ar->name` — name of the active function (may be NULL → None).
    pub name: Option<Vec<u8>>,
    /// C: `ar->namewhat` — how the name was found ("local", "global", "method", …).
    pub namewhat: Vec<u8>,
    /// C: `ar->what` — function type: b'L' Lua, b'C' C, b'm' main chunk.
    pub what: u8,
    /// C: `ar->short_src` — truncated source string for messages.
    pub short_src: Vec<u8>,
    /// C: `ar->linedefined` — first line of the function definition.
    pub linedefined: i32,
    /// C: `ar->currentline` — current line being executed (−1 if unavailable).
    pub currentline: i32,
    /// C: `ar->istailcall` — whether this frame was a tail-call.
    pub istailcall: bool,
}

// ── Constants ─────────────────────────────────────────────────────────────────

/// Number of stack frames to show in the first part of a traceback.
/// C: `#define LEVELS1 10`
const LEVELS1: i32 = 10;

/// Number of stack frames to show in the second part of a traceback.
/// C: `#define LEVELS2 11`
const LEVELS2: i32 = 11;

/// Index (1-based) in the reference table that heads the free-list of recycled
/// references. Placed after the last predefined registry key.
/// C: `#define freelist (LUA_RIDX_LAST + 1)` where `LUA_RIDX_LAST = 2`.
const FREELIST_REF: i64 = 3; // LUA_RIDX_GLOBALS (2) + 1

/// Pseudo-reference returned by `lua_ref` when the pushed value was `nil`.
/// C: `#define LUA_REFNIL (-1)`
pub const LUA_REFNIL: i32 = -1;

/// Pseudo-reference meaning "no reference" (never created by `lua_ref`).
/// C: `#define LUA_NOREF (-2)`
pub const LUA_NOREF: i32 = -2;

/// Extended error code: file-related I/O error from `load_filex`.
/// C: `#define LUA_ERRFILE (LUA_ERRERR + 1)` = 6.
pub const LUA_ERRFILE: i32 = 6;

/// Registry key for the table of loaded modules.
/// C: `#define LUA_LOADED_TABLE "_LOADED"`
pub const LUA_LOADED_TABLE: &[u8] = b"_LOADED";

/// Registry key for the table of preloaded loaders.
/// C: `#define LUA_PRELOAD_TABLE "_PRELOAD"`
pub const LUA_PRELOAD_TABLE: &[u8] = b"_PRELOAD";

/// Name of the global environment table.
/// C: `#define LUA_GNAME "_G"`
pub const LUA_GNAME: &[u8] = b"_G";

/// Metatable name / file-handle key for the IO library.
/// C: `#define LUA_FILEHANDLE "FILE*"`
pub const LUA_FILE_HANDLE: &[u8] = b"FILE*";

/// Pseudo-index for the Lua registry.
/// C: `#define LUA_REGISTRYINDEX (-LUAI_MAXSTACK - 1000)`
const LUA_REGISTRYINDEX: i32 = -1_001_000;

/// Minimum number of extra stack slots `lua_checkstack` guarantees per call.
/// C: `LUA_MINSTACK = 20`
const LUA_MINSTACK: i32 = 20;

// ── Public types ──────────────────────────────────────────────────────────────

/// A function-registration entry for `set_funcs`.
///
/// C: `typedef struct luaL_Reg { const char *name; lua_CFunction func; } luaL_Reg;`
///
/// In Rust, `name` is `&'static [u8]` (never `&str`). A `None` func is a
/// placeholder that pushes `false` rather than a closure.
pub struct LuaReg {
    pub name: &'static [u8],
    pub func: Option<fn(&mut LuaState) -> Result<usize, LuaError>>,
}

/// Growable byte-buffer used by the auxiliary library for building strings.
///
/// C: `luaL_Buffer` from `lauxlib.h`
///
/// The C version uses a small inline initial buffer with overflow managed via
/// a Lua-stack userdata box. The Rust port collapses this to a plain `Vec<u8>`.
/// All buffer mutating functions take `&mut LuaState` as a separate parameter.
pub struct LuaBuffer {
    pub data: Vec<u8>,
}

/// File-stream handle used by the IO library.
///
/// C: `luaL_Stream` from `lauxlib.h`
///
/// `closef` in C is a `lua_CFunction`. In Rust we store an optional closer.
// TODO(port): file I/O belongs in lua-stdlib/src/io_lib.rs; this definition
// may move there. Keeping here to mirror the C header.
pub struct LuaStream {
    /// The underlying file handle. `None` for incompletely opened or closed streams.
    // TODO(port): use a real File type (e.g. `std::fs::File`) in Phase B,
    // noting std::fs is allowed in lua-stdlib for I/O library support.
    pub f: Option<Box<dyn std::io::Read>>,
    /// Optional close function (None for already-closed streams).
    pub closef: Option<fn(&mut LuaState) -> Result<usize, LuaError>>,
}

// ── Traceback ─────────────────────────────────────────────────────────────────

/// Search for `objidx` in the table at the top of the stack.
/// `objidx` must be an absolute API stack index.
/// Returns `true` (and leaves name string on top) when found.
///
/// C: `static int findfield(lua_State *L, int objidx, int level)`
fn find_field(
    state: &mut LuaState,
    objidx: i32,
    level: i32,
) -> Result<bool, LuaError> {
    // C: if (level == 0 || !lua_istable(L, -1)) return 0;
    if level == 0 || state.type_at(-1) != LuaType::Table {
        return Ok(false);
    }
    // C: lua_pushnil(L);  /* start 'next' loop */
    state.push(LuaValue::Nil)?;
    // C: while (lua_next(L, -2))
    while state.table_next(-2)? {
        // C: if (lua_type(L, -2) == LUA_TSTRING)
        if state.type_at(-2) == LuaType::String {
            // C: if (lua_rawequal(L, objidx, -1))
            if state.raw_equal(objidx, -1)? {
                state.pop_n(1); // remove value (keep name)
                return Ok(true);
            } else if find_field(state, objidx, level - 1)? {
                // stack: lib_name, lib_table, field_name (top)
                state.push_string(b".")?; // place '.' between the two names
                state.replace(-3); // in the slot occupied by table
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
/// C: `static int pushglobalfuncname(lua_State *L, lua_Debug *ar)`
fn push_global_func_name(
    state: &mut LuaState,
    ar: &mut LuaDebug,
) -> Result<bool, LuaError> {
    // C: int top = lua_gettop(L);
    let top = state.top_count();
    // C: lua_getinfo(L, "f", ar);  /* push function */
    state.get_info(b"f", ar)?;
    // C: lua_getfield(L, LUA_REGISTRYINDEX, LUA_LOADED_TABLE);
    state.get_field(LUA_REGISTRYINDEX, LUA_LOADED_TABLE)?;
    // C: luaL_checkstack(L, 6, "not enough stack");
    check_stack(state, 6, Some(b"not enough stack"))?;
    if find_field(state, top + 1, 2)? {
        // C: const char *name = lua_tostring(L, -1);
        // C: if (strncmp(name, LUA_GNAME ".", 3) == 0)
        if state.peek_bytes(-1).map_or(false, |n| n.starts_with(b"_G.")) {
            // C: lua_pushstring(L, name + 3); /* push name without prefix */
            let suffix = state.peek_bytes(-1)
                .map(|n| n[3..].to_vec())
                .unwrap_or_default();
            state.push_bytes(&suffix)?;
            // C: lua_remove(L, -2); /* remove original name */
            state.remove(-2)?;
        }
        // C: lua_copy(L, -1, top + 1);
        state.copy_value(-1, top + 1)?;
        // C: lua_settop(L, top + 1);
        state.set_top(top + 1)?;
        Ok(true)
    } else {
        // C: lua_settop(L, top);
        state.set_top(top)?;
        Ok(false)
    }
}

/// Push a human-readable name for the function described by `ar`.
///
/// C: `static void pushfuncname(lua_State *L, lua_Debug *ar)`
fn push_func_name(state: &mut LuaState, ar: &mut LuaDebug) -> Result<(), LuaError> {
    if push_global_func_name(state, ar)? {
        // C: lua_pushfstring(L, "function '%s'", lua_tostring(L, -1));
        let name = state.peek_bytes(-1).unwrap_or(b"?").to_vec();
        state.push_fstring(format_args!("function '{}'", BStr(&name)))?;
        // C: lua_remove(L, -2);
        state.remove(-2)?;
    } else if !ar.namewhat.is_empty() {
        // C: lua_pushfstring(L, "%s '%s'", ar->namewhat, ar->name);
        let namewhat = ar.namewhat.clone();
        let name = ar.name.clone().unwrap_or_else(|| b"?".to_vec());
        state.push_fstring(format_args!("{} '{}'", BStr(&namewhat), BStr(&name)))?;
    } else if ar.what == b'm' {
        // C: lua_pushliteral(L, "main chunk");
        state.push_string(b"main chunk")?;
    } else if ar.what != b'C' {
        // C: lua_pushfstring(L, "function <%s:%d>", ar->short_src, ar->linedefined);
        let src = ar.short_src.clone();
        let line = ar.linedefined;
        state.push_fstring(format_args!("function <{}:{}>", BStr(&src), line))?;
    } else {
        // C: lua_pushliteral(L, "?");
        state.push_string(b"?")?;
    }
    Ok(())
}

/// Binary-search for the last valid stack level in `state`.
///
/// C: `static int lastlevel(lua_State *L)`
fn last_level(state: &mut LuaState) -> i32 {
    let mut ar = LuaDebug::default();
    let mut li: i32 = 1;
    let mut le: i32 = 1;
    // C: while (lua_getstack(L, le, &ar)) { li = le; le *= 2; }
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
/// C: `LUALIB_API void luaL_traceback(lua_State *L, lua_State *L1, const char *msg, int level)`
pub fn traceback(
    state: &mut LuaState,
    other: &mut LuaState,
    msg: Option<&[u8]>,
    level: i32,
) -> Result<(), LuaError> {
    let mut b = LuaBuffer::new();
    let mut ar = LuaDebug::default();
    let last = last_level(other);
    // C: int limit2show = (last - level > LEVELS1 + LEVELS2) ? LEVELS1 : -1;
    let mut limit2show: i32 = if last - level > LEVELS1 + LEVELS2 { LEVELS1 } else { -1 };
    buf_init(state, &mut b);
    if let Some(m) = msg {
        add_lstring(&mut b, m);
        add_char(&mut b, b'\n');
    }
    add_lstring(&mut b, b"stack traceback:");
    let mut level = level;
    while other.get_stack(level, &mut ar) {
        level += 1;
        if limit2show == 0 {
            // C: int n = last - level - LEVELS2 + 1;
            let n = last - level - LEVELS2 + 1;
            // C: lua_pushfstring(L, "\n\t...\t(skipping %d levels)", n);
            state.push_fstring(format_args!("\n\t...\t(skipping {} levels)", n))?;
            add_value(state, &mut b)?;
            level += n;
        } else {
            limit2show -= 1;
            // C: lua_getinfo(L1, "Slnt", &ar);
            other.get_info(b"Slnt", &mut ar)?;
            if ar.currentline <= 0 {
                // C: lua_pushfstring(L, "\n\t%s: in ", ar.short_src);
                let src = ar.short_src.clone();
                state.push_fstring(format_args!("\n\t{}: in ", BStr(&src)))?;
            } else {
                // C: lua_pushfstring(L, "\n\t%s:%d: in ", ar.short_src, ar.currentline);
                let src = ar.short_src.clone();
                let line = ar.currentline;
                state.push_fstring(format_args!("\n\t{}:{}: in ", BStr(&src), line))?;
            }
            add_value(state, &mut b)?;
            push_func_name(state, &mut ar)?;
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
/// C: `LUALIB_API int luaL_argerror(lua_State *L, int arg, const char *extramsg)`
pub fn arg_error(
    state: &mut LuaState,
    mut arg: i32,
    extramsg: &[u8],
) -> Result<usize, LuaError> {
    let mut ar = LuaDebug::default();
    if !state.get_stack(0, &mut ar) {
        // C: return luaL_error(L, "bad argument #%d (%s)", arg, extramsg);
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
            state.peek_bytes(-1).unwrap_or(b"?").to_vec()
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
/// C: `LUALIB_API int luaL_typeerror(lua_State *L, int arg, const char *tname)`
pub fn type_error_arg(
    state: &mut LuaState,
    arg: i32,
    tname: &[u8],
) -> Result<usize, LuaError> {
    // C: if (luaL_getmetafield(L, arg, "__name") == LUA_TSTRING)
    //      typearg = lua_tostring(L, -1);
    //    else if (lua_type(L, arg) == LUA_TLIGHTUSERDATA)
    //      typearg = "light userdata";
    //    else
    //      typearg = luaL_typename(L, arg);
    let typearg: Vec<u8> = if get_metafield(state, arg, b"__name")? == LuaType::String {
        let bytes = state.peek_bytes(-1).unwrap_or(b"?").to_vec();
        state.pop_n(1);
        bytes
    } else if state.type_at(arg) == LuaType::LightUserData {
        b"light userdata".to_vec()
    } else {
        state.type_name_at(arg).to_vec()
    };
    // C: msg = lua_pushfstring(L, "%s expected, got %s", tname, typearg);
    // C: return luaL_argerror(L, arg, msg);
    let msg_owned = format!(
        "{} expected, got {}",
        BStr(tname),
        BStr(&typearg)
    );
    arg_error(state, arg, msg_owned.as_bytes())
}

/// Push a type-tag error for `arg`, using the Lua type name for `tag`.
///
/// C: `static void tag_error(lua_State *L, int arg, int tag)`
fn tag_error(state: &mut LuaState, arg: i32, tag: LuaType) -> Result<(), LuaError> {
    let name = state.type_name(tag);
    type_error_arg(state, arg, name)?;
    Ok(())
}

/// Push a string describing the location of the call at `level` onto the stack.
/// If no location is available, pushes an empty string.
///
/// C: `LUALIB_API void luaL_where(lua_State *L, int level)`
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
    // C: lua_pushfstring(L, "");  /* no information available */
    state.push_string(b"")?;
    Ok(())
}

/// Format a runtime error with source location and raise it.
/// Always returns `Err`.
///
/// C: `LUALIB_API int luaL_error(lua_State *L, const char *fmt, ...)`
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
/// C: `LUALIB_API int luaL_fileresult(lua_State *L, int stat, const char *fname)`
pub fn file_result(
    state: &mut LuaState,
    stat: bool,
    fname: Option<&[u8]>,
) -> Result<usize, LuaError> {
    if stat {
        // C: lua_pushboolean(L, 1);
        state.push(LuaValue::Bool(true))?;
        Ok(1)
    } else {
        // C: luaL_pushfail(L); = lua_pushnil
        state.push(LuaValue::Nil)?;
        // TODO(port): use std::io::Error::last_os_error() for errno-style message.
        let errmsg = b"(errno unavailable in Rust port)".to_vec();
        if let Some(name) = fname {
            let full = [name, b": ".as_slice(), &errmsg].concat();
            state.push_bytes(&full)?;
        } else {
            state.push_bytes(&errmsg)?;
        }
        // C: lua_pushinteger(L, en);
        // TODO(port): push actual errno integer once os-error helpers are available.
        state.push(LuaValue::Int(0))?;
        Ok(3)
    }
}

/// Push the result of a process-exit status onto the stack.
/// Returns 3 values: success-bool-or-nil, exit-kind string, status code.
///
/// C: `LUALIB_API int luaL_execresult(lua_State *L, int stat)`
// TODO(port): POSIX WIFEXITED / WIFSIGNALED inspection requires cfg(unix).
pub fn exec_result(state: &mut LuaState, stat: i32) -> Result<usize, LuaError> {
    if stat != 0 {
        return file_result(state, false, None);
    }
    // C: const char *what = "exit";
    let what = b"exit".as_slice();
    // C: if (*what == 'e' && stat == 0) lua_pushboolean(L, 1);
    state.push(LuaValue::Bool(true))?;
    state.push_bytes(what)?;
    state.push(LuaValue::Int(stat as i64))?;
    Ok(3)
}

// ── Userdata / metatable helpers ──────────────────────────────────────────────

/// Create a new metatable for type `tname` and register it in the registry.
/// Returns `true` (and leaves new metatable on stack) if the table was created;
/// returns `false` (and leaves existing table on stack) if already existed.
///
/// C: `LUALIB_API int luaL_newmetatable(lua_State *L, const char *tname)`
pub fn new_metatable(state: &mut LuaState, tname: &[u8]) -> Result<bool, LuaError> {
    // C: if (luaL_getmetatable(L, tname) != LUA_TNIL)  return 0;
    if get_metatable(state, tname)? != LuaType::Nil {
        return Ok(false); // leave previous value on top
    }
    state.pop_n(1);
    // C: lua_createtable(L, 0, 2);
    state.create_table(0, 2)?;
    // C: lua_pushstring(L, tname); lua_setfield(L, -2, "__name");
    state.push_bytes(tname)?;
    state.set_field(-2, b"__name")?;
    // C: lua_pushvalue(L, -1); lua_setfield(L, LUA_REGISTRYINDEX, tname);
    state.push_value(-1)?;
    state.set_field(LUA_REGISTRYINDEX, tname)?;
    Ok(true)
}

/// Set the metatable of the value at stack top to the one registered as `tname`.
///
/// C: `LUALIB_API void luaL_setmetatable(lua_State *L, const char *tname)`
pub fn set_metatable(state: &mut LuaState, tname: &[u8]) -> Result<(), LuaError> {
    // C: luaL_getmetatable(L, tname); lua_setmetatable(L, -2);
    get_metatable(state, tname)?;
    state.set_metatable(-2)?;
    Ok(())
}

/// Check whether the value at `ud` is a full userdata with metatable `tname`.
/// Returns `Some(userdata)` if yes, `None` otherwise.
///
/// C: `LUALIB_API void *luaL_testudata(lua_State *L, int ud, const char *tname)`
pub fn test_udata(
    state: &mut LuaState,
    ud: i32,
    tname: &[u8],
) -> Result<Option<GcRef<LuaUserData>>, LuaError> {
    // C: void *p = lua_touserdata(L, ud);
    let p = state.to_userdata(ud);
    if let Some(p) = p {
        // C: if (lua_getmetatable(L, ud))
        if state.get_metatable(ud)? {
            // C: luaL_getmetatable(L, tname);
            get_metatable(state, tname)?;
            // C: if (!lua_rawequal(L, -1, -2))  p = NULL;
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
/// C: `LUALIB_API void *luaL_checkudata(lua_State *L, int ud, const char *tname)`
pub fn check_udata(
    state: &mut LuaState,
    ud: i32,
    tname: &[u8],
) -> Result<GcRef<LuaUserData>, LuaError> {
    // C: void *p = luaL_testudata(L, ud, tname);
    // C: luaL_argexpected(L, p != NULL, ud, tname);
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
/// C: `LUALIB_API int luaL_checkoption(lua_State *L, int arg, const char *def, const char *const lst[])`
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
/// C: `LUALIB_API void luaL_checkstack(lua_State *L, int space, const char *msg)`
pub fn check_stack(
    state: &mut LuaState,
    space: i32,
    msg: Option<&[u8]>,
) -> Result<(), LuaError> {
    // C: if (l_unlikely(!lua_checkstack(L, space)))
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
/// C: `LUALIB_API void luaL_checktype(lua_State *L, int arg, int t)`
pub fn check_type(state: &mut LuaState, arg: i32, t: LuaType) -> Result<(), LuaError> {
    // C: if (l_unlikely(lua_type(L, arg) != t)) tag_error(L, arg, t);
    if state.type_at(arg) != t {
        tag_error(state, arg, t)?;
    }
    Ok(())
}

/// Assert that a value (not `none`) is present at `arg`.
///
/// C: `LUALIB_API void luaL_checkany(lua_State *L, int arg)`
pub fn check_any(state: &mut LuaState, arg: i32) -> Result<(), LuaError> {
    // C: if (l_unlikely(lua_type(L, arg) == LUA_TNONE))
    if state.type_at(arg) == LuaType::None {
        return Err(LuaError::arg_error(arg, b"value expected"));
    }
    Ok(())
}

/// Return the string at `arg` as bytes; raise a type error if not a string.
///
/// C: `LUALIB_API const char *luaL_checklstring(lua_State *L, int arg, size_t *len)`
pub fn check_lstring(state: &mut LuaState, arg: i32) -> Result<GcRef<LuaString>, LuaError> {
    // C: const char *s = lua_tolstring(L, arg, len);
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
/// C: `LUALIB_API const char *luaL_optlstring(lua_State *L, int arg, const char *def, size_t *len)`
pub fn opt_lstring(
    state: &mut LuaState,
    arg: i32,
    def: Option<&[u8]>,
) -> Result<Option<Vec<u8>>, LuaError> {
    // C: if (lua_isnoneornil(L, arg)) { ... return def; }
    if state.is_none_or_nil(arg) {
        return Ok(def.map(|d| d.to_vec()));
    }
    let s = check_lstring(state, arg)?;
    Ok(Some(s.as_bytes().to_vec()))
}

/// Return the number at `arg` as `f64`; raise a type error if not a number.
///
/// C: `LUALIB_API lua_Number luaL_checknumber(lua_State *L, int arg)`
pub fn check_number(state: &mut LuaState, arg: i32) -> Result<f64, LuaError> {
    // C: int isnum; lua_Number d = lua_tonumberx(L, arg, &isnum);
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
/// C: `LUALIB_API lua_Number luaL_optnumber(lua_State *L, int arg, lua_Number def)`
pub fn opt_number(state: &mut LuaState, arg: i32, def: f64) -> Result<f64, LuaError> {
    // C: return luaL_opt(L, luaL_checknumber, arg, def);
    if state.is_none_or_nil(arg) {
        Ok(def)
    } else {
        check_number(state, arg)
    }
}

/// Raise an error for a non-integer number argument.
///
/// C: `static void interror(lua_State *L, int arg)`
///
/// Always returns `Err`. The `Ok` arm uses `unreachable!()` to satisfy the
/// return type; `!` (never) is nightly-only so we use `Result<usize, LuaError>`.
fn int_error(state: &mut LuaState, arg: i32) -> Result<usize, LuaError> {
    if state.is_number(arg) {
        Err(LuaError::arg_error(
            arg,
            b"number has no integer representation",
        ))
    } else {
        tag_error(state, arg, LuaType::Number)?;
        unreachable!("tag_error always returns Err")
    }
}

/// Return the integer at `arg` as `i64`; raise if not an integer-convertible number.
///
/// C: `LUALIB_API lua_Integer luaL_checkinteger(lua_State *L, int arg)`
pub fn check_integer(state: &mut LuaState, arg: i32) -> Result<i64, LuaError> {
    // C: int isnum; lua_Integer d = lua_tointegerx(L, arg, &isnum);
    match state.to_integer_x(arg) {
        Some(d) => Ok(d),
        None => int_error(state, arg),
    }
}

/// Return the integer at `arg`; if absent/nil return `def`.
///
/// C: `LUALIB_API lua_Integer luaL_optinteger(lua_State *L, int arg, lua_Integer def)`
pub fn opt_integer(state: &mut LuaState, arg: i32, def: i64) -> Result<i64, LuaError> {
    // C: return luaL_opt(L, luaL_checkinteger, arg, def);
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
    /// C: the initial `luaL_Buffer` has a small inline array of `LUAL_BUFFERSIZE` bytes.
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
/// C: `LUALIB_API void luaL_buffinit(lua_State *L, luaL_Buffer *B)`
pub fn buf_init(state: &mut LuaState, buf: &mut LuaBuffer) {
    // PORT NOTE: C pushes a light-userdata placeholder onto the stack to hold
    // the buffer's position. We still push nil as a stack slot placeholder so
    // that add_value / push_result see the same stack layout.
    *buf = LuaBuffer::new();
    // C: lua_pushlightuserdata(L, (void*)B);  /* push placeholder */
    // We push nil; Phase B can revisit if this matters for GC interaction.
    let _ = state.push(LuaValue::Nil);
}

/// Initialize `buf`, reserve `sz` bytes, and return the writable region.
///
/// C: `LUALIB_API char *luaL_buffinitsize(lua_State *L, luaL_Buffer *B, size_t sz)`
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
/// C: `static size_t newbuffsize(luaL_Buffer *B, size_t sz)`
fn new_buff_size(buf: &LuaBuffer, sz: usize) -> Result<usize, LuaError> {
    // C: if (l_unlikely(MAX_SIZET - sz < B->n)) return luaL_error(...)
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
/// C: `static char *prepbuffsize(luaL_Buffer *B, size_t sz, int boxidx)`
/// C: `LUALIB_API char *luaL_prepbuffsize(luaL_Buffer *B, size_t sz)`
pub fn prep_buff_size(buf: &mut LuaBuffer, sz: usize) -> Result<(), LuaError> {
    if buf.data.capacity() - buf.data.len() < sz {
        let newcap = new_buff_size(buf, sz)?;
        buf.data.reserve(newcap - buf.data.len());
    }
    Ok(())
}

/// Append `s` to `buf`.
///
/// C: `LUALIB_API void luaL_addlstring(luaL_Buffer *B, const char *s, size_t l)`
pub fn add_lstring(buf: &mut LuaBuffer, s: &[u8]) {
    if !s.is_empty() {
        buf.data.extend_from_slice(s);
    }
}

/// Append a single byte to `buf`.
///
/// C: `#define luaL_addchar(B,c) ...`
pub fn add_char(buf: &mut LuaBuffer, c: u8) {
    buf.data.push(c);
}

/// Append `sz` to the length counter (used after writing directly into the buffer).
///
/// C: `#define luaL_addsize(B,s) ((B)->n += (s))`
pub fn add_size(buf: &mut LuaBuffer, sz: usize) {
    // PORT NOTE: In C this is a direct `n += sz` on the inline length field.
    // With Vec, length is implicit; this is a no-op unless caller wrote past len.
    // TODO(port): if direct-write into spare capacity is needed, switch to `unsafe`
    // set_len or redesign; for Phase A this is a no-op.
    let _ = sz;
}

/// Pop the string at top of `state`'s stack and append it to `buf`.
///
/// C: `LUALIB_API void luaL_addvalue(luaL_Buffer *B)`
pub fn add_value(state: &mut LuaState, buf: &mut LuaBuffer) -> Result<(), LuaError> {
    // C: const char *s = lua_tolstring(L, -1, &len);
    if let Some(bytes) = state.peek_bytes(-1) {
        let owned = bytes.to_vec();
        add_lstring(buf, &owned);
    }
    // C: lua_pop(L, 1);
    state.pop_n(1);
    Ok(())
}

/// Push the buffer contents as a Lua string onto `state`'s stack.
///
/// C: `LUALIB_API void luaL_pushresult(luaL_Buffer *B)`
pub fn push_result(state: &mut LuaState, buf: &mut LuaBuffer) -> Result<(), LuaError> {
    // C: lua_pushlstring(L, B->b, B->n);
    state.push_bytes(&buf.data)?;
    // C: if (buffonstack(B)) lua_closeslot(L, -2);
    // C: lua_remove(L, -2);  /* remove box or placeholder */
    state.remove(-2)?;
    Ok(())
}

/// Add `sz` bytes to the buffer count then call `push_result`.
///
/// C: `LUALIB_API void luaL_pushresultsize(luaL_Buffer *B, size_t sz)`
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
/// C: `LUALIB_API void luaL_addgsub(luaL_Buffer *b, const char *s, const char *p, const char *r)`
pub fn add_gsub(buf: &mut LuaBuffer, s: &[u8], pat: &[u8], repl: &[u8]) {
    if pat.is_empty() {
        add_lstring(buf, s);
        return;
    }
    let mut remaining = s;
    while let Some(pos) = find_bytes(remaining, pat) {
        // C: luaL_addlstring(b, s, wild - s);
        add_lstring(buf, &remaining[..pos]);
        // C: luaL_addstring(b, r);
        add_lstring(buf, repl);
        remaining = &remaining[pos + pat.len()..];
    }
    // C: luaL_addstring(b, s);  /* push last suffix */
    add_lstring(buf, remaining);
}

/// Build a string from `s` by replacing `pat` with `repl`, push it on the stack,
/// and return the bytes of the pushed string.
///
/// C: `LUALIB_API const char *luaL_gsub(lua_State *L, const char *s, const char *p, const char *r)`
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
    // C: return lua_tostring(L, -1);
    Ok(state.peek_bytes(-1).unwrap_or(b"").to_vec())
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
/// C: `LUALIB_API int luaL_ref(lua_State *L, int t)`
pub fn lua_ref(state: &mut LuaState, t: i32) -> Result<i32, LuaError> {
    // C: if (lua_isnil(L, -1)) { lua_pop(L, 1); return LUA_REFNIL; }
    if state.type_at(-1) == LuaType::Nil {
        state.pop_n(1);
        return Ok(LUA_REFNIL);
    }
    let t = state.abs_index(t);
    // C: if (lua_rawgeti(L, t, freelist) == LUA_TNIL)
    let ref_val: i32;
    if state.raw_get_i(t, FREELIST_REF)? == LuaType::Nil {
        ref_val = 0; // list is empty
        // C: lua_pushinteger(L, 0); lua_rawseti(L, t, freelist);
        state.push(LuaValue::Int(0))?;
        state.raw_set_i(t, FREELIST_REF)?;
    } else {
        // C: lua_assert(lua_isinteger(L, -1)); ref = (int)lua_tointeger(L, -1);
        debug_assert!(state.type_at(-1) == LuaType::Integer);
        ref_val = state.to_integer_x(-1).unwrap_or(0) as i32;
    }
    state.pop_n(1); // remove element from stack
    let next_ref: i32;
    if ref_val != 0 {
        // C: lua_rawgeti(L, t, ref); lua_rawseti(L, t, freelist);
        state.raw_get_i(t, ref_val as i64)?;
        state.raw_set_i(t, FREELIST_REF)?;
        next_ref = ref_val;
    } else {
        // C: ref = (int)lua_rawlen(L, t) + 1;
        next_ref = (state.raw_len(t) as i32) + 1;
    }
    // C: lua_rawseti(L, t, ref);
    state.raw_set_i(t, next_ref as i64)?;
    Ok(next_ref)
}

/// Release reference `ref` from table `t`, adding it to the free list.
///
/// C: `LUALIB_API void luaL_unref(lua_State *L, int t, int ref)`
pub fn lua_unref(state: &mut LuaState, t: i32, r: i32) -> Result<(), LuaError> {
    if r >= 0 {
        let t = state.abs_index(t);
        // C: lua_rawgeti(L, t, freelist);
        state.raw_get_i(t, FREELIST_REF)?;
        debug_assert!(state.type_at(-1) == LuaType::Integer);
        // C: lua_rawseti(L, t, ref);  /* t[ref] = t[freelist] */
        state.raw_set_i(t, r as i64)?;
        // C: lua_pushinteger(L, ref); lua_rawseti(L, t, freelist);
        state.push(LuaValue::Int(r as i64))?;
        state.raw_set_i(t, FREELIST_REF)?;
    }
    Ok(())
}

// ── Load functions ─────────────────────────────────────────────────────────────

// TODO(port): luaL_loadfilex / load_filex use std::fs::File (fopen/fread/fclose)
// which is banned outside lua-cli. The logic is preserved here with stubs;
// Phase B will either move it to lua-cli or re-evaluate the restriction for
// the stdlib I/O layer.

/// Internal chunk reader that returns a pre-read prefix then drains a file.
///
/// C: `static const char *getF(lua_State *L, void *ud, size_t *size)`
// TODO(port): std::fs::File needed; stub for Phase A.
fn get_f_reader() -> impl FnMut() -> Option<Vec<u8>> {
    // TODO(port): real implementation reads from a File handle.
    move || None
}

/// Internal chunk reader that returns a single buffer slice then signals EOF.
///
/// C: `static const char *getS(lua_State *L, void *ud, size_t *size)`
fn make_string_reader(data: Vec<u8>) -> impl FnMut() -> Option<Vec<u8>> {
    let mut remaining = Some(data);
    move || remaining.take()
}

/// Load a file as a Lua chunk. Returns `LUA_OK` on success or an error code.
///
/// C: `LUALIB_API int luaL_loadfilex(lua_State *L, const char *filename, const char *mode)`
// TODO(port): uses std::fs which is banned outside lua-cli; stub for Phase A.
pub fn load_filex(
    state: &mut LuaState,
    filename: Option<&[u8]>,
    mode: Option<&[u8]>,
) -> Result<i32, LuaError> {
    // TODO(port): open file with std::fs::File, handle BOM, skip shebang line.
    // For Phase A we always fail cleanly.
    let _ = (filename, mode);
    Err(LuaError::runtime(format_args!(
        "luaL_loadfilex not implemented in Phase A (std::fs banned outside lua-cli)"
    )))
}

/// Load a buffer as a Lua chunk.
///
/// C: `LUALIB_API int luaL_loadbufferx(lua_State *L, const char *buff, size_t size, const char *name, const char *mode)`
pub fn load_bufferx(
    state: &mut LuaState,
    buff: &[u8],
    name: &[u8],
    mode: Option<&[u8]>,
) -> Result<i32, LuaError> {
    // C: LoadS ls; ls.s = buff; ls.size = size;
    // C: return lua_load(L, getS, &ls, name, mode);
    let reader = make_string_reader(buff.to_vec());
    state.load(reader, name, mode)
}

/// Load a buffer as a Lua chunk (no mode argument).
///
/// C: `#define luaL_loadbuffer(L,s,sz,n) luaL_loadbufferx(L,s,sz,n,NULL)`
pub fn load_buffer(
    state: &mut LuaState,
    buff: &[u8],
    name: &[u8],
) -> Result<i32, LuaError> {
    load_bufferx(state, buff, name, None)
}

/// Load a NUL-terminated byte-string as a Lua chunk.
///
/// C: `LUALIB_API int luaL_loadstring(lua_State *L, const char *s)`
pub fn load_string(state: &mut LuaState, s: &[u8]) -> Result<i32, LuaError> {
    // C: return luaL_loadbuffer(L, s, strlen(s), s);
    load_buffer(state, s, s)
}

// ── Meta-field and misc helpers ───────────────────────────────────────────────

/// Push the metafield `event` of `obj` onto the stack and return its type.
/// If there is no metafield, nothing is pushed and `LuaType::Nil` is returned.
///
/// C: `LUALIB_API int luaL_getmetafield(lua_State *L, int obj, const char *event)`
pub fn get_metafield(
    state: &mut LuaState,
    obj: i32,
    event: &[u8],
) -> Result<LuaType, LuaError> {
    // C: if (!lua_getmetatable(L, obj)) return LUA_TNIL;
    if !state.get_metatable(obj)? {
        return Ok(LuaType::Nil);
    }
    // C: lua_pushstring(L, event); tt = lua_rawget(L, -2);
    state.push_bytes(event)?;
    let tt = state.raw_get(-2)?;
    if tt == LuaType::Nil {
        // C: lua_pop(L, 2); /* remove metatable and metafield */
        state.pop_n(2);
    } else {
        // C: lua_remove(L, -2); /* remove only metatable */
        state.remove(-2)?;
    }
    Ok(tt)
}

/// Call the metafield `event` of `obj` with `obj` as argument, pushing one result.
/// Returns `true` if the meta-method existed and was called.
///
/// C: `LUALIB_API int luaL_callmeta(lua_State *L, int obj, const char *event)`
pub fn call_meta(state: &mut LuaState, obj: i32, event: &[u8]) -> Result<bool, LuaError> {
    let obj = state.abs_index(obj);
    // C: if (luaL_getmetafield(L, obj, event) == LUA_TNIL) return 0;
    if get_metafield(state, obj, event)? == LuaType::Nil {
        return Ok(false);
    }
    // C: lua_pushvalue(L, obj); lua_call(L, 1, 1);
    state.push_value(obj)?;
    state.call(1, 1)?;
    Ok(true)
}

/// Return the length of the value at `idx` as a `i64`, raising an error if
/// the length is not an integer.
///
/// C: `LUALIB_API lua_Integer luaL_len(lua_State *L, int idx)`
pub fn lua_len(state: &mut LuaState, idx: i32) -> Result<i64, LuaError> {
    // C: lua_len(L, idx);
    state.len_op(idx)?;
    // C: l = lua_tointegerx(L, -1, &isnum);
    let l = match state.to_integer_x(-1) {
        Some(n) => n,
        None => {
            return Err(LuaError::runtime(format_args!(
                "object length is not an integer"
            )));
        }
    };
    // C: lua_pop(L, 1);
    state.pop_n(1);
    Ok(l)
}

/// Convert the value at `idx` to a byte-string representation (using `__tostring`
/// if available) and push it onto the stack.
///
/// C: `LUALIB_API const char *luaL_tolstring(lua_State *L, int idx, size_t *len)`
pub fn to_lua_string(state: &mut LuaState, idx: i32) -> Result<Vec<u8>, LuaError> {
    let idx = state.abs_index(idx);
    // C: if (luaL_callmeta(L, idx, "__tostring"))
    if call_meta(state, idx, b"__tostring")? {
        // C: if (!lua_isstring(L, -1)) luaL_error(...)
        if state.type_at(-1) != LuaType::String {
            return Err(LuaError::runtime(format_args!(
                "'__tostring' must return a string"
            )));
        }
    } else {
        match state.type_at(idx) {
            LuaType::Number => {
                // C: if (lua_isinteger(L, idx)) lua_pushfstring(L, "%I", ...)
                if state.is_integer(idx) {
                    let i = state.to_integer_x(idx).unwrap_or(0);
                    state.push_fstring(format_args!("{}", i))?;
                } else {
                    let f = state.to_number_x(idx).unwrap_or(0.0);
                    state.push_fstring(format_args!("{:?}", f))?;
                }
            }
            LuaType::String => {
                // C: lua_pushvalue(L, idx);
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
                // C: int tt = luaL_getmetafield(L, idx, "__name");
                let tt = get_metafield(state, idx, b"__name")?;
                let kind: Vec<u8> = if tt == LuaType::String {
                    state.peek_bytes(-1).unwrap_or(b"?").to_vec()
                } else {
                    state.type_name_at(idx).to_vec()
                };
                // C: lua_pushfstring(L, "%s: %p", kind, lua_topointer(L, idx));
                // TODO(port): lua_topointer gives a pointer address; in Rust use
                // a hash or allocation address for a stable identifier.
                state.push_fstring(format_args!("{}: 0x?", BStr(&kind)))?;
                if tt != LuaType::Nil {
                    // C: lua_remove(L, -2);
                    state.remove(-2)?;
                }
            }
        }
    }
    // C: return lua_tolstring(L, -1, len);
    Ok(state.peek_bytes(-1).unwrap_or(b"").to_vec())
}

/// Register the functions in `l` into the table at `-(nup + 1)`, giving each
/// closure the `nup` upvalues currently at the top of the stack.
///
/// C: `LUALIB_API void luaL_setfuncs(lua_State *L, const luaL_Reg *l, int nup)`
pub fn set_funcs(
    state: &mut LuaState,
    l: &[LuaReg],
    nup: i32,
) -> Result<(), LuaError> {
    check_stack(state, nup, Some(b"too many upvalues"))?;
    for reg in l {
        match reg.func {
            None => {
                // C: lua_pushboolean(L, 0);
                state.push(LuaValue::Bool(false))?;
            }
            Some(f) => {
                // C: for (i = 0; i < nup; i++) lua_pushvalue(L, -nup);
                for _ in 0..nup {
                    state.push_value(-nup)?;
                }
                // C: lua_pushcclosure(L, l->func, nup);
                state.push_c_closure(f, nup)?;
            }
        }
        // C: lua_setfield(L, -(nup + 2), l->name);
        state.set_field(-(nup + 2), reg.name)?;
    }
    // C: lua_pop(L, nup);
    state.pop_n(nup as usize);
    Ok(())
}

/// Ensure `state[idx][fname]` is a table; push it.
/// Returns `true` if the table already existed, `false` if newly created.
///
/// C: `LUALIB_API int luaL_getsubtable(lua_State *L, int idx, const char *fname)`
pub fn get_subtable(
    state: &mut LuaState,
    idx: i32,
    fname: &[u8],
) -> Result<bool, LuaError> {
    // C: if (lua_getfield(L, idx, fname) == LUA_TTABLE) return 1;
    if state.get_field(idx, fname)? == LuaType::Table {
        return Ok(true);
    }
    // C: lua_pop(L, 1);
    state.pop_n(1);
    let idx = state.abs_index(idx);
    // C: lua_newtable(L);
    state.new_table()?;
    // C: lua_pushvalue(L, -1); lua_setfield(L, idx, fname);
    state.push_value(-1)?;
    state.set_field(idx, fname)?;
    Ok(false)
}

/// Simplified `require`: open module `modname` via `openf`, register it in
/// `package.loaded`, and (if `glb`) in the global table.
/// Leaves the module on top of the stack.
///
/// C: `LUALIB_API void luaL_requiref(lua_State *L, const char *modname, lua_CFunction openf, int glb)`
pub fn requiref(
    state: &mut LuaState,
    modname: &[u8],
    openf: fn(&mut LuaState) -> Result<usize, LuaError>,
    glb: bool,
) -> Result<(), LuaError> {
    // C: luaL_getsubtable(L, LUA_REGISTRYINDEX, LUA_LOADED_TABLE);
    get_subtable(state, LUA_REGISTRYINDEX, LUA_LOADED_TABLE)?;
    // C: lua_getfield(L, -1, modname);
    state.get_field(-1, modname)?;
    // C: if (!lua_toboolean(L, -1))
    if !state.to_boolean(-1) {
        // C: lua_pop(L, 1);
        state.pop_n(1);
        // C: lua_pushcfunction(L, openf);
        state.push_c_function(openf)?;
        // C: lua_pushstring(L, modname);
        state.push_bytes(modname)?;
        // C: lua_call(L, 1, 1);
        state.call(1, 1)?;
        // C: lua_pushvalue(L, -1); lua_setfield(L, -3, modname);
        state.push_value(-1)?;
        state.set_field(-3, modname)?;
    }
    // C: lua_remove(L, -2);
    state.remove(-2)?;
    if glb {
        // C: lua_pushvalue(L, -1); lua_setglobal(L, modname);
        state.push_value(-1)?;
        state.set_global(modname)?;
    }
    Ok(())
}

// ── Helper for registry-based metatable lookup ─────────────────────────────────

/// Push `registry[tname]` and return its type.
///
/// C: `#define luaL_getmetatable(L,n) (lua_getfield(L, LUA_REGISTRYINDEX, (n)))`
pub fn get_metatable(state: &mut LuaState, tname: &[u8]) -> Result<LuaType, LuaError> {
    state.get_field(LUA_REGISTRYINDEX, tname)
}

// ── State creation and version check ─────────────────────────────────────────

/// Create a new `LuaState` with the default allocator, a panic handler, and
/// warnings disabled.
///
/// C: `LUALIB_API lua_State *luaL_newstate(void)`
pub fn new_state() -> Result<LuaState, LuaError> {
    // C: lua_State *L = lua_newstate(l_alloc, NULL);
    // PORT NOTE: Rust's allocator is used implicitly; no l_alloc hook needed.
    let mut state = LuaState::new()?;
    // C: lua_atpanic(L, &panic);
    state.set_panic_handler(default_panic_handler)?;
    // C: lua_setwarnf(L, warnfoff, L);  /* default is warnings off */
    state.set_warn_fn(warn_off)?;
    Ok(state)
}

/// Default panic handler: print message to stderr and return to abort.
///
/// C: `static int panic(lua_State *L)`
fn default_panic_handler(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: const char *msg = (lua_type(L, -1) == LUA_TSTRING) ? lua_tostring(L, -1) : "...";
    let msg = if state.type_at(-1) == LuaType::String {
        state.peek_bytes(-1).unwrap_or(b"?").to_vec()
    } else {
        b"error object is not a string".to_vec()
    };
    // C: lua_writestringerror(...)
    eprintln!("PANIC: unprotected error in call to Lua API ({})", BStr(&msg));
    Ok(0) // return to Lua to abort
}

/// Warning function: warnings are off.
///
/// C: `static void warnfoff(void *ud, const char *message, int tocont)`
fn warn_off(state: &mut LuaState, message: &[u8], tocont: bool) -> Result<(), LuaError> {
    check_control(state, message, tocont)?;
    Ok(())
}

/// Warning function: ready to start a new message.
///
/// C: `static void warnfon(void *ud, const char *message, int tocont)`
fn warn_on(state: &mut LuaState, message: &[u8], tocont: bool) -> Result<(), LuaError> {
    if check_control(state, message, tocont)? {
        return Ok(());
    }
    eprint!("Lua warning: ");
    warn_cont(state, message, tocont)
}

/// Warning function: continue writing a previous warning message.
///
/// C: `static void warnfcont(void *ud, const char *message, int tocont)`
fn warn_cont(state: &mut LuaState, message: &[u8], tocont: bool) -> Result<(), LuaError> {
    // C: lua_writestringerror("%s", message);
    eprint!("{}", BStr(message));
    if tocont {
        // C: lua_setwarnf(L, warnfcont, L);
        state.set_warn_fn(warn_cont)?;
    } else {
        eprintln!(); // finish message with end-of-line
        // C: lua_setwarnf(L, warnfon, L);
        state.set_warn_fn(warn_on)?;
    }
    Ok(())
}

/// Handle a warning control message (e.g. `"@on"`, `"@off"`).
/// Returns `true` if the message was a recognised control message.
///
/// C: `static int checkcontrol(lua_State *L, const char *message, int tocont)`
fn check_control(
    state: &mut LuaState,
    message: &[u8],
    tocont: bool,
) -> Result<bool, LuaError> {
    // C: if (tocont || *(message++) != '@') return 0;
    if tocont || message.first() != Some(&b'@') {
        return Ok(false);
    }
    let cmd = &message[1..];
    if cmd == b"off" {
        state.set_warn_fn(warn_off)?;
    } else if cmd == b"on" {
        state.set_warn_fn(warn_on)?;
    }
    Ok(true)
}

/// Version-compatibility check: error if numeric type sizes or version mismatch.
///
/// C: `LUALIB_API void luaL_checkversion_(lua_State *L, lua_Number ver, size_t sz)`
pub fn check_version(state: &mut LuaState, ver: f64, sz: usize) -> Result<(), LuaError> {
    // C: LUAL_NUMSIZES = sizeof(lua_Integer)*16 + sizeof(lua_Number)
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

impl Default for LuaDebug {
    fn default() -> Self {
        LuaDebug {
            name: None,
            namewhat: Vec::new(),
            what: b'C',
            short_src: Vec::new(),
            linedefined: -1,
            currentline: -1,
            istailcall: false,
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        src/lauxlib.c  (1127 lines, ~50 functions)
//   target_crate:  lua-stdlib
//   confidence:    medium
//   todos:         12
//   port_notes:    5
//   unsafe_blocks: 0
//   notes:         Buffer simplified from stack-based C UBox to Vec<u8>.
//                  File I/O (load_filex) stubbed; std::fs banned outside lua-cli.
//                  Warning system implemented with fn-pointer callbacks.
//                  LuaState/LuaDebug stubs present; wire imports in Phase B.
//                  add_size / set_len for buffer direct-write is a no-op in Phase A.
// ──────────────────────────────────────────────────────────────────────────
