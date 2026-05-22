//! Dynamic library loader for the Lua `package` library.
//!
//! Ported from `reference/lua-5.4.7/src/loadlib.c` (758 lines, ~25 functions).
//!
//! Provides `require`, `package.loadlib`, `package.searchpath`, and the four
//! built-in module searchers (preload, Lua-file, C-library, C-root).
//!
//! ## Platform-specific dynamic loading
//! The C source conditionally compiled one of three backends (POSIX dlfcn,
//! Windows LoadLibraryEx, or a fallback stub). All three `lsys_*` stubs here
//! return the "dynamic libraries not enabled" message because:
//! - The real implementations require `unsafe` (dlopen/GetProcAddress).
//! - `unsafe` is banned in `lua-stdlib` per PORTING.md §1.
//!
//! `readable()` also requires `std::fs`, which is banned outside `lua-cli`.
//!
//! Both issues are flagged with `TODO(port)` and must be resolved in Phase B
//! by either raising the unsafe budget or delegating to a capability interface.

use std::env;

use lua_types::{
    GcRef, LuaClosure, LuaError, LuaString, LuaType, LuaValue, StackIdx, LuaStatus,
};
use lua_types::value::LuaTable;
use crate::state_stub::{LuaState, LuaStateStubExt as _, lua_CFunction, upvalue_index, CompareOp, LuaDebug};

// ── Module-level constants ────────────────────────────────────────────────────

// C: #define LUA_POF "luaopen_"
const LUA_POF: &[u8] = b"luaopen_";

// C: #define LUA_OFSEP "_"
const LUA_OFSEP: &[u8] = b"_";

// C: static const char *const CLIBS = "_CLIBS";
const CLIBS: &[u8] = b"_CLIBS";

// C: #define LIB_FAIL "open"
const LIB_FAIL: &[u8] = b"open";

// C: LUA_PATH_SEP — path-list separator
const LUA_PATH_SEP: u8 = b';';

// C: LUA_PATH_MARK — wildcard character in path templates
const LUA_PATH_MARK: u8 = b'?';

// C: LUA_IGMARK — ignore-mark in C module names
const LUA_IGMARK: u8 = b'-';

// C: LUA_DIRSEP — directory separator (platform-specific)
#[cfg(target_os = "windows")]
const LUA_DIRSEP: u8 = b'\\';
#[cfg(not(target_os = "windows"))]
const LUA_DIRSEP: u8 = b'/';

// C: LUA_CSUBSEP / LUA_LSUBSEP — subseparators when mapping module names to paths
// Both default to LUA_DIRSEP on all platforms.
const LUA_CSUBSEP: u8 = LUA_DIRSEP;
const LUA_LSUBSEP: u8 = LUA_DIRSEP;

// C: #define ERRLIB 1  / #define ERRFUNC 2
// Non-fatal error codes returned by lookforfunc (error message on Lua stack).
const ERRLIB: i32 = 1;
const ERRFUNC: i32 = 2;

// C: DLMSG (fallback platform stub message)
const DLMSG: &[u8] = b"dynamic libraries not enabled; check your Lua installation";

// C: #define LUA_PATH_VAR "LUA_PATH" / LUA_CPATH_VAR "LUA_CPATH"
const LUA_PATH_VAR: &[u8] = b"LUA_PATH";
const LUA_CPATH_VAR: &[u8] = b"LUA_CPATH";

// C: LUA_PATH_DEFAULT / LUA_CPATH_DEFAULT (from luaconf.h, platform-dependent)
// TODO(port): These should come from a platform configuration crate, not be
// hardcoded. Lua's build system inserts the actual install prefix here.
#[cfg(not(target_os = "windows"))]
const LUA_PATH_DEFAULT: &[u8] =
    b"./?.lua;./?/init.lua;/usr/local/share/lua/5.4/?.lua;/usr/local/share/lua/5.4/?/init.lua";
#[cfg(target_os = "windows")]
const LUA_PATH_DEFAULT: &[u8] = b"./?.lua;./?/init.lua";

#[cfg(not(target_os = "windows"))]
const LUA_CPATH_DEFAULT: &[u8] =
    b"./?.so;/usr/local/lib/lua/5.4/?.so;/usr/local/lib/lua/5.4/loadall.so";
#[cfg(target_os = "windows")]
const LUA_CPATH_DEFAULT: &[u8] = b"./?.dll";

// C: LUA_VERSUFFIX (from luaconf.h) — e.g. "_5_4"
// TODO(port): Centralise version constants; this is duplicated from luaconf.h.
const LUA_VERSUFFIX: &[u8] = b"_5_4";

// ── Opaque library handle ─────────────────────────────────────────────────────

/// Opaque handle to a dynamically loaded C library.
///
/// C: `void *lib` (the return value of dlopen / LoadLibraryEx).
///
/// TODO(port): In a real implementation this would be `*mut c_void`. That
/// requires `unsafe` which is banned in `lua-stdlib`. Phase B should either
/// raise the unsafe budget or introduce a `DynLib` capability object that
/// lives in `lua-gc`/`lua-coro` and expose a safe API here.
///
/// For Phase A this is a `usize` placeholder; all lsys_* functions return
/// `None` so no real handle is ever stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LibHandle(usize);

// ── Byte-string utilities ─────────────────────────────────────────────────────

/// Append to `buf` the bytes of `s` with all non-overlapping occurrences of
/// `pattern` replaced by `replacement`.
///
/// C: equivalent of the substitution logic inside `luaL_gsub` / `luaL_addgsub`.
fn gsub_append(buf: &mut Vec<u8>, s: &[u8], pattern: &[u8], replacement: &[u8]) {
    if pattern.is_empty() {
        buf.extend_from_slice(s);
        return;
    }
    let mut pos = 0;
    while pos < s.len() {
        if s[pos..].starts_with(pattern) {
            buf.extend_from_slice(replacement);
            pos += pattern.len();
        } else {
            buf.push(s[pos]);
            pos += 1;
        }
    }
}

/// Return a new `Vec<u8>` with all non-overlapping occurrences of `pattern`
/// in `s` replaced by `replacement`.
fn gsub_bytes(s: &[u8], pattern: &[u8], replacement: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    gsub_append(&mut out, s, pattern, replacement);
    out
}

/// Find the byte offset of `needle` in `haystack`, or `None`.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

// ── Platform-specific dynamic-loading stubs ───────────────────────────────────

/// Unload a previously loaded C library.
///
/// C: `static void lsys_unloadlib(void *lib)`
///    — POSIX: `dlclose(lib)`; Windows: `FreeLibrary(lib)`.
///
/// TODO(port): unsafe needed — dlclose/FreeLibrary are FFI calls. Banned in
/// lua-stdlib. Phase B: delegate to a DynLib capability or raise unsafe budget.
fn lsys_unloadlib(_lib: LibHandle) {
    // PORT NOTE: stub — real unloading deferred until the unsafe budget is resolved.
}

/// Load a C library from `path`. If `see_glb` is true, make symbols globally
/// visible (POSIX RTLD_GLOBAL). On failure, pushes an error string onto `state`.
///
/// C: `static void *lsys_load(lua_State *L, const char *path, int seeglb)`
///    — POSIX: `dlopen(path, RTLD_NOW | (seeglb ? RTLD_GLOBAL : RTLD_LOCAL))`
///    — Windows: `LoadLibraryExA(path, NULL, LUA_LLE_FLAGS)`
///
/// TODO(port): unsafe needed — dlopen/LoadLibraryEx are FFI calls. Banned in
/// lua-stdlib. Phase B: delegate to a DynLib capability or raise unsafe budget.
fn lsys_load(state: &mut LuaState, _path: &[u8], _see_glb: bool) -> Option<LibHandle> {
    // C: lua_pushliteral(L, DLMSG);
    let s = state.intern_str(DLMSG).ok()?;
    state.push(LuaValue::Str(s));
    None
}

/// Find symbol `sym` in library `lib` and return it as a Lua C function.
/// On failure, pushes an error string onto `state`.
///
/// C: `static lua_CFunction lsys_sym(lua_State *L, void *lib, const char *sym)`
///    — POSIX: `cast_func(dlsym(lib, sym))`
///    — Windows: `(lua_CFunction)(voidf)GetProcAddress(lib, sym)`
///
/// TODO(port): unsafe needed — dlsym/GetProcAddress are FFI calls. Banned in
/// lua-stdlib. Phase B: delegate to a DynLib capability or raise unsafe budget.
fn lsys_sym(
    state: &mut LuaState,
    _lib: LibHandle,
    _sym: &[u8],
) -> Option<fn(&mut LuaState) -> Result<usize, LuaError>> {
    // C: lua_pushliteral(L, DLMSG);
    let s = state.intern_str(DLMSG).ok()?;
    state.push(LuaValue::Str(s));
    None
}

// ── Path helpers ──────────────────────────────────────────────────────────────

/// Return `registry["LUA_NOENV"]` as a boolean.
///
/// C: `static int noenv(lua_State *L)`
fn noenv(state: &mut LuaState) -> bool {
    // C: lua_getfield(L, LUA_REGISTRYINDEX, "LUA_NOENV");
    state.get_field_registry(b"LUA_NOENV");
    // C: b = lua_toboolean(L, -1);
    let b = state.to_boolean(-1);
    // C: lua_pop(L, 1);
    state.pop_n(1);
    b
}

/// Set `package[fieldname]` to the appropriate path value.
///
/// Priority: versioned env var (e.g. `LUA_PATH_5_4`) → unversioned env var
/// (`LUA_PATH`) → compiled-in default. When the env var contains `;;`, the
/// compiled-in default is spliced in place of `;;`.
///
/// C: `static void setpath(lua_State *L, const char *fieldname,
///                          const char *envname, const char *dft)`
///
/// PORT NOTE: C pushes the versioned env-var name string onto the Lua stack
/// (via `lua_pushfstring`) and pops it at the end so that `setfield` uses index
/// `-3`. In Rust we compute the versioned name without touching the Lua stack,
/// so after pushing the final path value the package table is at `-2`. The
/// caller must ensure the package table is at stack top when setpath is called.
fn setpath(
    state: &mut LuaState,
    fieldname: &[u8],
    envname: &[u8],
    dft: &[u8],
) -> Result<(), LuaError> {
    // C: const char *nver = lua_pushfstring(L, "%s%s", envname, LUA_VERSUFFIX);
    let mut nver = envname.to_vec();
    nver.extend_from_slice(LUA_VERSUFFIX);

    // C: path = getenv(nver);  (then fallback to getenv(envname))
    // TODO(port): std::env::var() accepts &str (UTF-8). Env-var names are
    // OS-level ASCII here (not Lua user data), so from_utf8 is acceptable, but
    // std::env::var_os + std::os::unix::ffi::OsStrExt would be more correct for
    // paths containing non-UTF-8 bytes on Unix. Revisit in Phase B.
    let nver_str = std::str::from_utf8(&nver).unwrap_or("");
    let envname_str = std::str::from_utf8(envname).unwrap_or("");

    let path_opt: Option<Vec<u8>> = env::var(nver_str)
        .ok()
        .map(|s| s.into_bytes())
        .or_else(|| env::var(envname_str).ok().map(|s| s.into_bytes()));

    let final_path: Vec<u8> = if path_opt.is_none() || noenv(state) {
        // C: lua_pushstring(L, dft);
        dft.to_vec()
    } else {
        let path = path_opt.unwrap();
        // C: dftmark = strstr(path, LUA_PATH_SEP LUA_PATH_SEP)
        let double_sep = [LUA_PATH_SEP, LUA_PATH_SEP];
        if let Some(dftmark_pos) = find_subslice(&path, &double_sep) {
            // Path contains ";;": replace with default.
            // C: luaL_Buffer b; luaL_buffinit(L, &b);
            let mut buf = Vec::new();
            if dftmark_pos > 0 {
                // C: luaL_addlstring(&b, path, dftmark - path);
                buf.extend_from_slice(&path[..dftmark_pos]);
                // C: luaL_addchar(&b, *LUA_PATH_SEP);
                buf.push(LUA_PATH_SEP);
            }
            // C: luaL_addstring(&b, dft);
            buf.extend_from_slice(dft);
            let after = dftmark_pos + 2;
            if after < path.len() {
                // C: luaL_addchar(&b, *LUA_PATH_SEP);
                buf.push(LUA_PATH_SEP);
                // C: luaL_addlstring(&b, dftmark + 2, (path + len - 2) - dftmark);
                buf.extend_from_slice(&path[after..]);
            }
            // C: luaL_pushresult(&b);
            buf
        } else {
            // C: lua_pushstring(L, path);
            path
        }
    };

    // C: setprogdir(L);
    // PORT NOTE: On Windows, setprogdir replaces LUA_EXEC_DIR in the path with
    // the directory of the running executable (GetModuleFileNameA). On all other
    // platforms it's a no-op ((void)0). Stubbed here; on Windows this would also
    // require unsafe (Win32 API). The EXEC_DIR substitution is therefore skipped.

    // C: lua_setfield(L, -3, fieldname);
    // PORT NOTE: In C the index is -3 because the versioned-name string is still
    // on the stack. In Rust it is -2 because we did not push the versioned name.
    let s = state.intern_str(&final_path)?;
    state.push(LuaValue::Str(s));
    state.set_field(-2, fieldname)?;

    // C: lua_pop(L, 1);  -- pop versioned variable name ('nver')
    // PORT NOTE: No nver was pushed in Rust; nothing to pop here.

    Ok(())
}

// ── CLIBS registry table ──────────────────────────────────────────────────────

/// Return the library handle stored at `registry._CLIBS[path]`, or `None`.
///
/// C: `static void *checkclib(lua_State *L, const char *path)`
fn checkclib(state: &mut LuaState, path: &[u8]) -> Option<LibHandle> {
    // C: lua_getfield(L, LUA_REGISTRYINDEX, CLIBS);
    state.get_field_registry(CLIBS);
    // C: lua_getfield(L, -1, path);
    state.get_field(-1, path);
    // C: plib = lua_touserdata(L, -1);
    // TODO(port): lua_touserdata extracts a *mut c_void (light userdata). In
    // Rust we store LibHandle(usize). When a real implementation stores a pointer
    // as a LuaValue::LightUserData, the extraction needs to recover the handle.
    let handle = state.to_light_userdata(-1).map(|p| LibHandle(p as usize));
    // C: lua_pop(L, 2);
    state.pop_n(2);
    handle
}

/// Register a library handle in the CLIBS table (both by path and sequentially).
///
/// C: `static void addtoclib(lua_State *L, const char *path, void *plib)`
fn addtoclib(state: &mut LuaState, path: &[u8], plib: LibHandle) -> Result<(), LuaError> {
    // C: lua_getfield(L, LUA_REGISTRYINDEX, CLIBS);
    state.get_field_registry(CLIBS);
    // C: lua_pushlightuserdata(L, plib);
    // TODO(port): In real code plib would be *mut c_void pushed as LightUserData.
    // Placeholder: push the usize value as an integer.
    state.push(LuaValue::Int(plib.0 as i64));
    // C: lua_pushvalue(L, -1);
    state.push_value(-1);
    // C: lua_setfield(L, -3, path);  -- CLIBS[path] = plib
    state.set_field(-3, path)?;
    // C: lua_rawseti(L, -2, luaL_len(L, -2) + 1);  -- CLIBS[#CLIBS + 1] = plib
    let n = state.len_at(-2);
    state.raw_seti(-2, n + 1)?;
    // C: lua_pop(L, 1);  -- pop CLIBS table
    state.pop_n(1);
    Ok(())
}

/// `__gc` metamethod for the CLIBS table: unloads all registered C libraries
/// in reverse order when the Lua state closes.
///
/// C: `static int gctm(lua_State *L)`
fn gctm(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: lua_Integer n = luaL_len(L, 1);
    let n = state.len_at(1);
    let mut i = n;
    // C: for (; n >= 1; n--)
    while i >= 1 {
        // C: lua_rawgeti(L, 1, n);  -- get handle CLIBS[n]
        state.raw_geti(1, i)?;
        // C: lsys_unloadlib(lua_touserdata(L, -1));
        // TODO(port): see checkclib — extracting a real *mut c_void handle needs
        // LightUserData support and unsafe casting.
        if let Some(handle) = state.to_light_userdata(-1).map(|p| LibHandle(p as usize)) {
            lsys_unloadlib(handle);
        }
        // C: lua_pop(L, 1);
        state.pop_n(1);
        i -= 1;
    }
    Ok(0)
}

// ── Dynamic function lookup ───────────────────────────────────────────────────

/// Look for a C function named `sym` in the dynamically loaded library at `path`.
///
/// On success, pushes the C function (or `true` for `*`-sentinel) and returns `Ok(0)`.
/// On non-fatal failure, pushes an error message string and returns `Ok(ERRLIB)`
/// or `Ok(ERRFUNC)`. Fatal errors (e.g. OOM) propagate via `Err`.
///
/// C: `static int lookforfunc(lua_State *L, const char *path, const char *sym)`
///
/// PORT NOTE: C returns raw `int` error codes. Rust encodes them as `Ok(i32)`
/// so the caller can distinguish "error code + message on stack" from "fatal Err".
fn lookforfunc(state: &mut LuaState, path: &[u8], sym: &[u8]) -> Result<i32, LuaError> {
    // C: void *reg = checkclib(L, path);
    let mut reg = checkclib(state, path);
    if reg.is_none() {
        // C: reg = lsys_load(L, path, *sym == '*');
        reg = lsys_load(state, path, sym.first() == Some(&b'*'));
        if reg.is_none() {
            // C: if (reg == NULL) return ERRLIB;
            return Ok(ERRLIB);
        }
        addtoclib(state, path, reg.unwrap())?;
    }
    // C: if (*sym == '*') { lua_pushboolean(L, 1); return 0; }
    if sym.first() == Some(&b'*') {
        state.push(LuaValue::Bool(true));
        return Ok(0);
    }
    // C: lua_CFunction f = lsys_sym(L, reg, sym);
    let f = lsys_sym(state, reg.unwrap(), sym);
    if let Some(func) = f {
        // C: lua_pushcfunction(L, f);
        // TODO(phase-b): LuaClosure::LightC currently typed fn() -> i32 in lua-types; use push_c_function until widened.
        state.push_c_function(func)?;
        Ok(0)
    } else {
        // C: return ERRFUNC;
        Ok(ERRFUNC)
    }
}

// ── Lua-callable package functions ────────────────────────────────────────────

/// `package.loadlib(filename, funcname)` — open a C library and return a
/// Lua-callable wrapper for `funcname`.
///
/// Returns: on success, the loader function (1 value).
/// On error: `false`, error-message string, and `"open"` or `"init"` (3 values).
///
/// C: `static int ll_loadlib(lua_State *L)`
pub fn ll_loadlib(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: const char *path = luaL_checkstring(L, 1);
    let path = state.check_arg_string(1)?.to_vec();
    // C: const char *init = luaL_checkstring(L, 2);
    let init = state.check_arg_string(2)?.to_vec();
    // C: int stat = lookforfunc(L, path, init);
    let stat = lookforfunc(state, &path, &init)?;
    if stat == 0 {
        // C: if (l_likely(stat == 0)) return 1;
        return Ok(1);
    }
    // C: luaL_pushfail(L);
    // PORT NOTE: luaL_pushfail pushes `false` in Lua 5.4 (changed from nil).
    state.push(LuaValue::Bool(false));
    // C: lua_insert(L, -2);  -- move fail below error message
    state.insert(-2);
    // C: lua_pushstring(L, (stat == ERRLIB) ? LIB_FAIL : "init");
    let where_bytes: &[u8] = if stat == ERRLIB { LIB_FAIL } else { b"init" };
    let where_s = state.intern_str(where_bytes)?;
    state.push(LuaValue::Str(where_s));
    // C: return 3;
    Ok(3)
}

// ── File existence check ──────────────────────────────────────────────────────

/// Try to open `filename` for reading; return `true` if it succeeds.
///
/// C: `static int readable(const char *filename)`
///    — `FILE *f = fopen(filename, "r"); if (f == NULL) return 0;`
///
/// TODO(port): `std::fs::File::open` is banned in `lua-stdlib` (std::fs
/// restriction from PORTING.md §1). This needs to either move to `lua-cli` or
/// be injected as a read-capability callback on `LuaState`. Stubbed `false`
/// here, which means `require` will never find Lua source files on disk.
fn readable(_filename: &[u8]) -> bool {
    false
}

// ── Path-component iterator ───────────────────────────────────────────────────

/// Iterator over `;`-separated path components.
///
/// C: `getnextfilename(char **path, char *end)` advanced a mutable pointer
/// through a buffer, temporarily zero-terminating each component. In Rust we
/// advance a slice reference without mutation.
///
/// PORT NOTE: The C implementation restored each separator after use (mutating
/// the buffer). This Rust version slices immutably, which changes the interface
/// but produces the same sequence of filenames.
struct PathComponents<'a> {
    remaining: &'a [u8],
}

impl<'a> PathComponents<'a> {
    fn new(path: &'a [u8]) -> Self {
        PathComponents { remaining: path }
    }
}

impl<'a> Iterator for PathComponents<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining.is_empty() {
            return None;
        }
        let component = match self.remaining.iter().position(|&b| b == LUA_PATH_SEP) {
            Some(sep_pos) => {
                let c = &self.remaining[..sep_pos];
                self.remaining = &self.remaining[sep_pos + 1..];
                c
            }
            None => {
                let c = self.remaining;
                self.remaining = &[];
                c
            }
        };
        Some(component)
    }
}

// ── Error-message helpers ─────────────────────────────────────────────────────

/// Push an error message listing all files in `path` that were not found.
///
/// Example output: `"no file 'a.lua'\n\tno file 'b.lua'"`
///
/// C: `static void pusherrornotfound(lua_State *L, const char *path)`
fn pusherrornotfound(state: &mut LuaState, path: &[u8]) -> Result<(), LuaError> {
    // C: luaL_Buffer b; luaL_buffinit(L, &b);
    let mut buf: Vec<u8> = Vec::new();
    // C: luaL_addstring(&b, "no file '");
    buf.extend_from_slice(b"no file '");
    // C: luaL_addgsub(&b, path, LUA_PATH_SEP, "'\n\tno file '");
    gsub_append(&mut buf, path, &[LUA_PATH_SEP], b"'\n\tno file '");
    // C: luaL_addstring(&b, "'");
    buf.push(b'\'');
    // C: luaL_pushresult(&b);
    let s = state.intern_str(&buf)?;
    state.push(LuaValue::Str(s));
    Ok(())
}

// ── Path search ───────────────────────────────────────────────────────────────

/// Search for a readable file matching `name` in the `;`-separated `path`.
///
/// In each path template, `?` is replaced by `name` (with `sep` bytes replaced
/// by `dirsep` first). Returns `Some(filename_bytes)` and pushes the filename
/// string on the Lua stack if found. Returns `None` and pushes an error message
/// string if not found.
///
/// C: `static const char *searchpath(lua_State *L, const char *name,
///                                    const char *path, const char *sep,
///                                    const char *dirsep)`
fn searchpath(
    state: &mut LuaState,
    name: &[u8],
    path: &[u8],
    sep: &[u8],
    dirsep: &[u8],
) -> Result<Option<Vec<u8>>, LuaError> {
    // C: if (*sep != '\0' && strchr(name, *sep) != NULL)
    //        name = luaL_gsub(L, name, sep, dirsep);
    let name_buf: Vec<u8> = if !sep.is_empty() && name.contains(&sep[0]) {
        gsub_bytes(name, sep, dirsep)
    } else {
        name.to_vec()
    };

    // C: luaL_buffinit(L, &buff);
    // C: luaL_addgsub(&buff, path, LUA_PATH_MARK, name);
    // Build pathname list: replace every '?' in path with the (adjusted) name.
    let pathname: Vec<u8> = gsub_bytes(path, &[LUA_PATH_MARK], &name_buf);

    // C: while ((filename = getnextfilename(&pathname, endpathname)) != NULL)
    for filename in PathComponents::new(&pathname) {
        // C: if (readable(filename)) return lua_pushstring(L, filename);
        if readable(filename) {
            let s = state.intern_str(filename)?;
            state.push(LuaValue::Str(s));
            return Ok(Some(filename.to_vec()));
        }
    }

    // C: luaL_pushresult(&buff);          -- push expanded path list
    // C: pusherrornotfound(L, lua_tostring(L, -1));
    // PORT NOTE: C uses the Lua-stack string of the expanded pathname as the
    // argument to pusherrornotfound. In Rust we have `pathname` already as a
    // Vec<u8>; we pass it directly without the round-trip through the Lua stack.
    pusherrornotfound(state, &pathname)?;
    Ok(None)
}

/// `package.searchpath(name, path [, sep [, rep]])`.
///
/// Returns the first readable file in `path` with `sep` occurrences in `name`
/// replaced by `rep`. On failure returns `false` plus the error message.
///
/// C: `static int ll_searchpath(lua_State *L)`
pub fn ll_searchpath(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: luaL_checkstring(L, 1) / luaL_checkstring(L, 2)
    let name = state.check_arg_string(1)?.to_vec();
    let path = state.check_arg_string(2)?.to_vec();
    // C: luaL_optstring(L, 3, ".")
    let sep = state.opt_arg_string(3, b".")?;
    // C: luaL_optstring(L, 4, LUA_DIRSEP)
    let dirsep_default = [LUA_DIRSEP];
    let dirsep = state.opt_arg_string(4, &dirsep_default)?;

    let found = searchpath(state, &name, &path, &sep, &dirsep)?;
    if found.is_some() {
        // C: if (f != NULL) return 1;
        return Ok(1);
    }
    // C: luaL_pushfail(L); lua_insert(L, -2); return 2;
    state.push(LuaValue::Bool(false));
    state.insert(-2);
    Ok(2)
}

/// Find a module file using the path stored in `package[pname]`.
///
/// C: `static const char *findfile(lua_State *L, const char *name,
///                                  const char *pname, const char *dirsep)`
///
/// TODO(port): Should return `Result<Option<Vec<u8>>, LuaError>` to properly
/// propagate the `"'package.<pname>' must be a string"` error. Currently returns
/// `None` and loses the error, which will cause a confusing failure downstream.
fn findfile(state: &mut LuaState, name: &[u8], pname: &[u8], dirsep: u8) -> Result<Option<Vec<u8>>, LuaError> {
    // C: lua_getfield(L, lua_upvalueindex(1), pname);
    // The package table is upvalue #1 for the searcher closures.
    let uv = state.upvalue_index(1);
    let _ = state.get_field(uv, pname);
    // C: path = lua_tostring(L, -1);
    let path_opt: Option<Vec<u8>> = state.to_bytes(-1);
    let Some(path) = path_opt else {
        // C: if (l_unlikely(path == NULL)) luaL_error(L, "'package.%s' must be a string", pname);
        // TODO(port): Cannot return Err here without changing the signature.
        //             For now pop the nil and return None (error is silently dropped).
        state.pop_n(1);
        return Ok(None);
    };
    state.pop_n(1);
    searchpath(state, name, &path, b".", &[dirsep])
}

/// Check whether a module load succeeded, returning the open function + filename
/// (2 values) on success or raising an error on failure.
///
/// C: `static int checkload(lua_State *L, int stat, const char *filename)`
fn checkload(state: &mut LuaState, stat: bool, filename: &[u8]) -> Result<usize, LuaError> {
    if stat {
        // C: lua_pushstring(L, filename);
        let s = state.intern_str(filename)?;
        state.push(LuaValue::Str(s));
        // C: return 2;
        Ok(2)
    } else {
        // C: return luaL_error(L, "error loading module '%s' from file '%s':\n\t%s",
        //                         lua_tostring(L, 1), filename, lua_tostring(L, -1));
        // PORT NOTE: The error message in C embeds the module name (stack[1]) and
        // the loader error message (stack top). In Rust we read those byte slices.
        // TODO(port): state.to_bytes(1) and state.to_bytes(-1) borrow from the
        //             stack simultaneously; in Phase B use index-snapshot clones.
        let modname = state.to_bytes(1).unwrap_or_else(|| b"?".to_vec());
        let loader_err = state.to_bytes(-1).unwrap_or_else(|| b"?".to_vec());

        let mut msg = b"error loading module '".to_vec();
        msg.extend_from_slice(&modname);
        msg.extend_from_slice(b"' from file '");
        msg.extend_from_slice(filename);
        msg.extend_from_slice(b"':\n\t");
        msg.extend_from_slice(&loader_err);

        // PERF(port): builds a heap Vec then interns; in Phase B use push_fstring.
        let s = state.intern_str(&msg)?;
        return Err(LuaError::from_value(LuaValue::Str(s)));
    }
}

// ── Searcher functions ────────────────────────────────────────────────────────

/// Searcher that looks in `package.path` for a Lua source file.
///
/// Returns 1 value (error-message string) if not found, or 2 values (loader
/// function, filename) if found and loaded successfully.
///
/// C: `static int searcher_Lua(lua_State *L)`
fn searcher_lua(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: const char *name = luaL_checkstring(L, 1);
    let name = state.check_arg_string(1)?.to_vec();
    // C: filename = findfile(L, name, "path", LUA_LSUBSEP);
    let filename = findfile(state, &name, b"path", LUA_LSUBSEP)?;
    if filename.is_none() {
        // C: if (filename == NULL) return 1;  -- error message on stack
        return Ok(1);
    }
    let filename = filename.unwrap();
    // C: return checkload(L, (luaL_loadfile(L, filename) == LUA_OK), filename);
    // TODO(port): luaL_loadfile reads and compiles a Lua source file. This
    //             requires both file-system access (std::fs — banned in
    //             lua-stdlib) and a full parse+compile pipeline. Stubbing as
    //             always-failed for Phase A; Phase B wires up state.load_file().
    let load_ok = false;
    checkload(state, load_ok, &filename)
}

/// Try to load `modname`'s open function from the C dynamic library at `filename`.
///
/// Handles the "ignore mark" (`-`) convention: `"foo-bar"` first tries
/// `luaopen_foo`, then `luaopen_bar` as a fallback.
///
/// C: `static int loadfunc(lua_State *L, const char *filename, const char *modname)`
fn loadfunc(state: &mut LuaState, filename: &[u8], modname: &[u8]) -> Result<i32, LuaError> {
    // C: modname = luaL_gsub(L, modname, ".", LUA_OFSEP);
    let modname: Vec<u8> = gsub_bytes(modname, b".", LUA_OFSEP);

    // C: mark = strchr(modname, *LUA_IGMARK);
    if let Some(mark_pos) = modname.iter().position(|&b| b == LUA_IGMARK) {
        // C: openfunc = lua_pushlstring(L, modname, mark - modname);
        let prefix = &modname[..mark_pos];
        // C: openfunc = lua_pushfstring(L, LUA_POF"%s", openfunc);
        let mut openfunc = LUA_POF.to_vec();
        openfunc.extend_from_slice(prefix);
        // C: stat = lookforfunc(L, filename, openfunc);
        let stat = lookforfunc(state, filename, &openfunc)?;
        if stat != ERRFUNC {
            // C: if (stat != ERRFUNC) return stat;
            return Ok(stat);
        }
        // C: modname = mark + 1;  -- else go ahead and try old-style name
        let tail = &modname[mark_pos + 1..];
        let mut openfunc2 = LUA_POF.to_vec();
        openfunc2.extend_from_slice(tail);
        return lookforfunc(state, filename, &openfunc2);
    }

    // C: openfunc = lua_pushfstring(L, LUA_POF"%s", modname);
    let mut openfunc = LUA_POF.to_vec();
    openfunc.extend_from_slice(&modname);
    lookforfunc(state, filename, &openfunc)
}

/// Searcher that looks in `package.cpath` for a C dynamic library.
///
/// C: `static int searcher_C(lua_State *L)`
fn searcher_c(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: const char *name = luaL_checkstring(L, 1);
    let name = state.check_arg_string(1)?.to_vec();
    // C: const char *filename = findfile(L, name, "cpath", LUA_CSUBSEP);
    let filename = findfile(state, &name, b"cpath", LUA_CSUBSEP)?;
    if filename.is_none() {
        // C: if (filename == NULL) return 1;
        return Ok(1);
    }
    let filename = filename.unwrap();
    // C: return checkload(L, (loadfunc(L, filename, name) == 0), filename);
    let stat = loadfunc(state, &filename, &name)?;
    checkload(state, stat == 0, &filename)
}

/// Searcher that looks in `package.cpath` using only the root component
/// (everything before the first `.`) of the module name.
///
/// C: `static int searcher_Croot(lua_State *L)`
fn searcher_croot(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: const char *name = luaL_checkstring(L, 1);
    let name = state.check_arg_string(1)?.to_vec();
    // C: const char *p = strchr(name, '.');
    let dot_pos = name.iter().position(|&b| b == b'.');
    if dot_pos.is_none() {
        // C: if (p == NULL) return 0;  -- is already root; not our responsibility
        return Ok(0);
    }
    let dot_pos = dot_pos.unwrap();

    // C: lua_pushlstring(L, name, p - name);  -- push root portion
    let root = &name[..dot_pos];
    let root_s = state.intern_str(root)?;
    state.push(LuaValue::Str(root_s));

    // C: filename = findfile(L, lua_tostring(L, -1), "cpath", LUA_CSUBSEP);
    // PORT NOTE: C reads the root string back from the stack; in Rust we use
    // the slice directly and then pop the stack entry below.
    let filename = findfile(state, root, b"cpath", LUA_CSUBSEP)?;
    // Pop the root string we pushed above (findfile does not consume it).
    state.pop_n(1);

    if filename.is_none() {
        // C: if (filename == NULL) return 1;
        return Ok(1);
    }
    let filename = filename.unwrap();

    // C: if ((stat = loadfunc(L, filename, name)) != 0) { ... }
    let stat = loadfunc(state, &filename, &name)?;
    if stat != 0 {
        if stat != ERRFUNC {
            // C: return checkload(L, 0, filename);  -- real error
            return checkload(state, false, &filename);
        } else {
            // C: lua_pushfstring(L, "no module '%s' in file '%s'", name, filename);
            // C: return 1;
            let mut msg = b"no module '".to_vec();
            msg.extend_from_slice(&name);
            msg.extend_from_slice(b"' in file '");
            msg.extend_from_slice(&filename);
            msg.push(b'\'');
            let s = state.intern_str(&msg)?;
            state.push(LuaValue::Str(s));
            return Ok(1);
        }
    }

    // C: lua_pushstring(L, filename);  -- 2nd argument to module
    let s = state.intern_str(&filename)?;
    state.push(LuaValue::Str(s));
    // C: return 2;
    Ok(2)
}

/// Searcher that looks in `package.preload` for a pre-registered loader.
///
/// C: `static int searcher_preload(lua_State *L)`
fn searcher_preload(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: const char *name = luaL_checkstring(L, 1);
    let name = state.check_arg_string(1)?.to_vec();
    // C: lua_getfield(L, LUA_REGISTRYINDEX, LUA_PRELOAD_TABLE);
    state.get_field_registry(b"_PRELOAD");
    // C: if (lua_getfield(L, -1, name) == LUA_TNIL) { ... }
    let ty = state.get_field(-1, &name)?;
    if ty == LuaType::Nil {
        // C: lua_pushfstring(L, "no field package.preload['%s']", name);
        let mut msg = b"no field package.preload['".to_vec();
        msg.extend_from_slice(&name);
        msg.push(b'\'');
        msg.push(b']');
        let s = state.intern_str(&msg)?;
        state.push(LuaValue::Str(s));
        // C: return 1;
        return Ok(1);
    }
    // C: lua_pushliteral(L, ":preload:");
    let tag = state.intern_str(b":preload:")?;
    state.push(LuaValue::Str(tag));
    // C: return 2;
    Ok(2)
}

// ── require implementation ────────────────────────────────────────────────────

/// Iterate through `package.searchers` to find a loader for module `name`.
///
/// On success, leaves `(loader_function, loader_data)` at the top of the stack
/// (below the searchers table). On failure, raises a runtime error.
///
/// C: `static void findloader(lua_State *L, const char *name)`
///
/// TODO(port): The exact absolute stack indices used in C (index 3 for the
/// searchers table) depend on the caller (`ll_require`) having set up the
/// stack in a specific way. In Rust we use relative indices. The behaviour
/// should match C but the index arithmetic must be verified in Phase B.
fn findloader(state: &mut LuaState, name: &[u8]) -> Result<(), LuaError> {
    // C: if (lua_getfield(L, lua_upvalueindex(1), "searchers") != LUA_TTABLE)
    //        luaL_error(L, "'package.searchers' must be a table");
    let uv = state.upvalue_index(1);
    let ty = state.get_field(uv, b"searchers")?;
    if ty != LuaType::Table {
        return Err(LuaError::runtime(format_args!(
            "'package.searchers' must be a table"
        )));
    }
    // Searchers table is now at the top of the stack.

    // C: luaL_buffinit(L, &msg);
    let mut msg_buf: Vec<u8> = Vec::new();

    let mut i: i64 = 1;
    loop {
        // C: luaL_addstring(&msg, "\n\t");
        msg_buf.extend_from_slice(b"\n\t");

        // C: if (lua_rawgeti(L, 3, i) == LUA_TNIL) { ... no more searchers }
        // PORT NOTE: In C the searchers table is at absolute index 3. In Rust
        // it is at -1 (relative to the top). TODO(port): verify this is correct
        // after accounting for whatever else the caller left on the stack.
        let item_ty = state.raw_geti(-1, i)?;
        if item_ty == LuaType::Nil {
            // C: lua_pop(L, 1);  -- remove nil
            state.pop_n(1);
            // C: luaL_buffsub(&msg, 2);
            let len = msg_buf.len();
            if len >= 2 {
                msg_buf.truncate(len - 2);
            }
            // C: luaL_pushresult(&msg); luaL_error(L, "module '%s' not found:%s", ...)
            // Build the error message as a Lua string then raise.
            let mut err = b"module '".to_vec();
            err.extend_from_slice(name);
            err.extend_from_slice(b"' not found:");
            err.extend_from_slice(&msg_buf);
            let err_s = state.intern_str(&err)?;
            return Err(LuaError::from_value(LuaValue::Str(err_s)));
        }

        // C: lua_pushstring(L, name);
        let name_s = state.intern_str(name)?;
        state.push(LuaValue::Str(name_s));

        // C: lua_call(L, 1, 2);
        state.call(1, 2)?;

        // After call: two return values r1 (at -2) and r2 (at -1) on top.
        // C: if (lua_isfunction(L, -2)) return;
        if state.type_at(-2) == LuaType::Function {
            // Loader found; leave (r1=function, r2=data) on stack and return.
            return Ok(());
        }

        // C: else if (lua_isstring(L, -2)) { lua_pop(L, 1); luaL_addvalue(&msg); }
        if state.type_at(-2) == LuaType::String {
            // r1 is an error-message string from the searcher.
            // C: lua_pop(L, 1)  -- remove r2 (the extra/nil return)
            state.pop_n(1);
            // C: luaL_addvalue(&msg)  -- append r1 (now at -1) to msg, pop it
            if let Some(bytes) = state.to_bytes(-1) {
                msg_buf.extend_from_slice(&bytes);
            }
            state.pop_n(1);
        } else {
            // C: lua_pop(L, 2);  -- remove both returns (no error message)
            state.pop_n(2);
            // C: luaL_buffsub(&msg, 2);  -- remove the "\n\t" prefix
            let len = msg_buf.len();
            if len >= 2 {
                msg_buf.truncate(len - 2);
            }
        }

        i += 1;
    }
}

/// `require(modname)` — load a module by name, using `package.loaded` as a
/// cache and `package.searchers` to find and load it if not already cached.
///
/// Returns the module value (and optionally the loader data) — 2 values.
///
/// C: `static int ll_require(lua_State *L)`
pub fn ll_require(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: const char *name = luaL_checkstring(L, 1);
    let name = state.check_arg_string(1)?.to_vec();

    // C: lua_settop(L, 1);  -- LOADED table will be at index 2
    state.set_top(1);

    // C: lua_getfield(L, LUA_REGISTRYINDEX, LUA_LOADED_TABLE);
    state.get_field_registry(b"_LOADED");

    // C: lua_getfield(L, 2, name);  -- LOADED[name]
    state.get_field(2, &name);

    // C: if (lua_toboolean(L, -1)) return 1;  -- package is already loaded
    if state.to_boolean(-1) {
        return Ok(1);
    }

    // C: lua_pop(L, 1);  -- remove 'getfield' result
    state.pop_n(1);

    // C: findloader(L, name);
    // After this, the stack has: [name(1), LOADED(2), searchers(3), loader(-2), loaderdata(-1)]
    findloader(state, &name)?;

    // C: lua_rotate(L, -2, 1);  -- function <-> loader data
    // Swaps loader and loaderdata: [..., loaderdata, loader]
    state.rotate(-2, 1);

    // C: lua_pushvalue(L, 1);  -- name is 1st argument to module loader
    state.push_value(1);

    // C: lua_pushvalue(L, -3);  -- loader data is 2nd argument
    // PORT NOTE: After the rotate, loaderdata is 3 from top (-3). In C this is
    // at absolute index 4 (but C uses the pre-rotate layout). TODO(port): verify.
    state.push_value(-3);

    // C: lua_call(L, 2, 1);  -- run loader to load module
    state.call(2, 1)?;

    // C: if (!lua_isnil(L, -1)) lua_setfield(L, 2, name);
    if state.type_at(-1) != LuaType::Nil {
        state.set_field(2, &name)?;
    } else {
        // C: else lua_pop(L, 1);
        state.pop_n(1);
    }

    // C: if (lua_getfield(L, 2, name) == LUA_TNIL) { ... module set no value }
    let ty = state.get_field(2, &name)?;
    if ty == LuaType::Nil {
        // C: lua_pushboolean(L, 1); lua_copy(L, -1, -2); lua_setfield(L, 2, name);
        state.push(LuaValue::Bool(true));
        state.copy_value(-1, -2);
        state.set_field(2, &name)?;
    }

    // C: lua_rotate(L, -2, 1);  -- loader data <-> module result
    state.rotate(-2, 1);

    // C: return 2;  -- return module result and loader data
    Ok(2)
}

// ── Package library setup ─────────────────────────────────────────────────────

/// Create the `searchers` table and install the four built-in searchers, each
/// with the `package` table as upvalue #1.
///
/// C: `static void createsearcherstable(lua_State *L)`
fn createsearcherstable(state: &mut LuaState) -> Result<(), LuaError> {
    // C: static const lua_CFunction searchers[] = { searcher_preload,
    //        searcher_Lua, searcher_C, searcher_Croot, NULL };
    let searchers: &[fn(&mut LuaState) -> Result<usize, LuaError>] = &[
        searcher_preload,
        searcher_lua,
        searcher_c,
        searcher_croot,
    ];

    // C: lua_createtable(L, sizeof(searchers)/sizeof(searchers[0]) - 1, 0);
    state.create_table(searchers.len() as i32, 0);

    // C: for (i=0; searchers[i] != NULL; i++) { ... lua_pushcclosure(L, searchers[i], 1); ... }
    for (i, &f) in searchers.iter().enumerate() {
        // C: lua_pushvalue(L, -2);  -- set 'package' as upvalue for all searchers
        state.push_value(-2);
        // C: lua_pushcclosure(L, searchers[i], 1);
        // TODO(port): push_c_closure takes the function and n upvalues from the
        //             stack. The package table upvalue must be correctly associated
        //             with each searcher closure so that findfile can access it
        //             via lua_upvalueindex(1). Verify in Phase B.
        state.push_c_closure(f, 1)?;
        // C: lua_rawseti(L, -2, i+1);
        state.raw_seti(-2, (i + 1) as i64)?;
    }
    // C: lua_setfield(L, -2, "searchers");
    state.set_field(-2, b"searchers")?;
    Ok(())
}

/// Create the `_CLIBS` registry table with a `__gc` finalizer that closes all
/// loaded C libraries when the Lua state is closed.
///
/// C: `static void createclibstable(lua_State *L)`
fn createclibstable(state: &mut LuaState) -> Result<(), LuaError> {
    // C: luaL_getsubtable(L, LUA_REGISTRYINDEX, CLIBS);
    state.get_subtable_registry(CLIBS)?;
    // C: lua_createtable(L, 0, 1);  -- metatable for CLIBS
    state.create_table(0, 1);
    // C: lua_pushcfunction(L, gctm);
    // TODO(phase-b): LuaClosure::LightC currently typed fn() -> i32 in lua-types; use push_c_function until widened.
    state.push_c_function(gctm)?;
    // C: lua_setfield(L, -2, "__gc");
    state.set_field(-2, b"__gc")?;
    // C: lua_setmetatable(L, -2);
    state.set_metatable(-2)?;
    Ok(())
}

/// Open the `package` library and return the `package` table.
///
/// C: `LUAMOD_API int luaopen_package(lua_State *L)`
pub fn luaopen_package(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: createclibstable(L);
    createclibstable(state)?;

    // C: luaL_newlib(L, pk_funcs);  -- create 'package' table
    // PORT NOTE: The C pk_funcs table also contains placeholder entries for
    // "preload", "cpath", "path", "searchers", "loaded" (all NULL). In Rust
    // those fields are set explicitly below; only the real functions are here.
    state.new_lib(&[
        (b"loadlib" as &[u8], ll_loadlib as fn(&mut LuaState) -> Result<usize, LuaError>),
        (b"searchpath", ll_searchpath as fn(&mut LuaState) -> Result<usize, LuaError>),
    ])?;

    // C: createsearcherstable(L);
    createsearcherstable(state)?;

    // C: setpath(L, "path", LUA_PATH_VAR, LUA_PATH_DEFAULT);
    setpath(state, b"path", LUA_PATH_VAR, LUA_PATH_DEFAULT)?;

    // C: setpath(L, "cpath", LUA_CPATH_VAR, LUA_CPATH_DEFAULT);
    setpath(state, b"cpath", LUA_CPATH_VAR, LUA_CPATH_DEFAULT)?;

    // C: lua_pushliteral(L, LUA_DIRSEP "\n" LUA_PATH_SEP "\n" LUA_PATH_MARK "\n"
    //                       LUA_EXEC_DIR "\n" LUA_IGMARK "\n");
    // The config string encodes platform separator characters, one per line.
    let mut config: Vec<u8> = Vec::new();
    config.push(LUA_DIRSEP);
    config.push(b'\n');
    config.push(LUA_PATH_SEP);
    config.push(b'\n');
    config.push(LUA_PATH_MARK);
    config.push(b'\n');
    config.push(b'!');   // LUA_EXEC_DIR
    config.push(b'\n');
    config.push(LUA_IGMARK);
    config.push(b'\n');
    let config_s = state.intern_str(&config)?;
    state.push(LuaValue::Str(config_s));

    // C: lua_setfield(L, -2, "config");
    state.set_field(-2, b"config")?;

    // C: luaL_getsubtable(L, LUA_REGISTRYINDEX, LUA_LOADED_TABLE);
    state.get_subtable_registry(b"_LOADED")?;
    // C: lua_setfield(L, -2, "loaded");
    state.set_field(-2, b"loaded")?;

    // C: luaL_getsubtable(L, LUA_REGISTRYINDEX, LUA_PRELOAD_TABLE);
    state.get_subtable_registry(b"_PRELOAD")?;
    // C: lua_setfield(L, -2, "preload");
    state.set_field(-2, b"preload")?;

    // C: lua_pushglobaltable(L);
    state.push_globals();
    // C: lua_pushvalue(L, -2);  -- set 'package' as upvalue for next lib
    state.push_value(-2);
    // C: luaL_setfuncs(L, ll_funcs, 1);  -- open lib into global table
    state.set_funcs_with_upvalues(
        &[(b"require" as &[u8], ll_require as fn(&mut LuaState) -> Result<usize, LuaError>)],
        1,
    )?;
    // C: lua_pop(L, 1);  -- pop global table
    state.pop_n(1);

    // C: return 1;  -- return 'package' table
    Ok(1)
}

// ──────────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        src/loadlib.c  (758 lines, 25 functions)
//   target_crate:  lua-stdlib
//   confidence:    medium
//   todos:         11
//   port_notes:    6
//   unsafe_blocks: 0   (must be 0 outside lua-gc/lua-coro)
//   notes:         All three lsys_* platform backends are stubs (unsafe needed
//                  for dlopen/LoadLibraryEx; banned in lua-stdlib). readable()
//                  is a stub (std::fs banned in lua-stdlib). searcher_lua is
//                  also a stub (no load_file yet). The path-finding and
//                  require logic are faithfully ported. Stack index arithmetic
//                  in findloader/ll_require should be verified in Phase B.
//                  LUA_PATH_DEFAULT / LUA_CPATH_DEFAULT are hardcoded and
//                  must be replaced with platform configuration constants.
// ──────────────────────────────────────────────────────────────────────────────
