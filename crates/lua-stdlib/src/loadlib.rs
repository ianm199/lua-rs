//! Dynamic library loader for the Lua `package` library.
//!
//! Ported from `reference/lua-5.4.7/src/loadlib.c` (758 lines, ~25 functions).
//!
//! Provides `require`, `package.loadlib`, `package.searchpath`, and the four
//! built-in module searchers (preload, Lua-file, C-library, C-root).
//!
//! ## Platform-specific dynamic loading
//!
//! The three platform calls (`lsys_load`, `lsys_sym`, `lsys_unloadlib`) are
//! dispatched through embedder hooks on [`lua_vm::state::GlobalState`]:
//! `dynlib_load_hook`, `dynlib_symbol_hook`, `dynlib_unload_hook`. `lua-cli`
//! installs a `libloading`-backed implementation; embeddings that omit the
//! hooks behave like C-Lua's fallback platform stub (`LIB_FAIL = "absent"`).
//!
//! Keeping the platform calls behind hooks lets `lua-stdlib` stay free of
//! `unsafe` per PORTING.md §1; `libloading` lives entirely in `lua-cli`.

use lua_types::{
 LuaError, LuaType, LuaValue,
};
use lua_vm::state::{DynLibId, DynamicSymbol};
use crate::state_stub::{LuaState, LuaStateStubExt as _, lua_CFunction};

// ── Module-level constants ────────────────────────────────────────────────────

const LUA_POF: &[u8] = b"luaopen_";

const LUA_OFSEP: &[u8] = b"_";

const CLIBS: &[u8] = b"_CLIBS";

// `lsys_load` chooses the tag at runtime: `"open"` when a load hook is
// installed (matching POSIX/Windows behaviour) and `"absent"` when no hook
// is registered (matching the fallback stub). The constant below carries the
// fallback-stub spelling; the load-hook path uses `b"open"` directly.
const LIB_FAIL_ABSENT: &[u8] = b"absent";

const LUA_PATH_SEP: u8 = b';';

const LUA_PATH_MARK: u8 = b'?';

const LUA_IGMARK: u8 = b'-';

#[cfg(target_os = "windows")]
const LUA_DIRSEP: u8 = b'\\';
#[cfg(not(target_os = "windows"))]
const LUA_DIRSEP: u8 = b'/';

// Both default to LUA_DIRSEP on all platforms.
const LUA_CSUBSEP: u8 = LUA_DIRSEP;
const LUA_LSUBSEP: u8 = LUA_DIRSEP;

// In the Rust port these became enum variants of `LookForFuncStatus` so the
// failure-tag string travels with the status (the C code always uses the
// single compile-time `LIB_FAIL`). See `LookForFuncStatus` below.

// is registered on `GlobalState`. The CLI backend supplies its own error
// strings via the hook's `Err` return for "open" failures.
const DLMSG: &[u8] = b"dynamic libraries not enabled; check your Lua installation";

// Message returned via `(false, msg, "init")` when a hook resolves a symbol
// against stock Lua 5.4's `lua_State *` C ABI. That ABI is not callable
// against this build's `LuaState`; supporting it is a separate compatibility
// project (see docs/LUA_PHASE_E_RUNTIME_SPEC.md Part 3).
const C_ABI_UNSUPPORTED_MSG: &[u8] =
    b"dynamic library loaded, but Lua C ABI modules are not supported by this build";

const LUA_PATH_VAR: &[u8] = b"LUA_PATH";
const LUA_CPATH_VAR: &[u8] = b"LUA_CPATH";

// Matches C-Lua's luaconf.h defaults exactly: LUA_LDIR entries first, then
// LUA_CDIR entries, then the local ./? fallback last.
// TODO(port): These should come from a platform configuration crate, not be
// hardcoded. Lua's build system inserts the actual install prefix here.
#[cfg(not(target_os = "windows"))]
const LUA_PATH_DEFAULT: &[u8] = b"/usr/local/share/lua/5.4/?.lua;/usr/local/share/lua/5.4/?/init.lua;/usr/local/lib/lua/5.4/?.lua;/usr/local/lib/lua/5.4/?/init.lua;./?.lua;./?/init.lua";
#[cfg(target_os = "windows")]
const LUA_PATH_DEFAULT: &[u8] = b"./?.lua;./?/init.lua";

#[cfg(not(target_os = "windows"))]
const LUA_CPATH_DEFAULT: &[u8] =
    b"/usr/local/lib/lua/5.4/?.so;/usr/local/lib/lua/5.4/loadall.so;./?.so";
#[cfg(target_os = "windows")]
const LUA_CPATH_DEFAULT: &[u8] = b"./?.dll";

// TODO(port): Centralise version constants; this is duplicated from luaconf.h.
const LUA_VERSUFFIX: &[u8] = b"_5_4";

fn getenv_bytes(state: &LuaState, name: &[u8]) -> Option<Vec<u8>> {
    if let Some(env_fn) = state.global().env_hook {
        return env_fn(name);
    }

    #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
    {
        None
    }

    #[cfg(all(unix, not(all(target_arch = "wasm32", target_os = "unknown"))))]
    {
        use std::ffi::OsStr;
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let os_name = OsStr::from_bytes(name);
        std::env::var_os(os_name).map(|v| v.into_vec())
    }

    #[cfg(all(not(unix), not(all(target_arch = "wasm32", target_os = "unknown"))))]
    {
        std::str::from_utf8(name)
            .ok()
            .and_then(|name_str| std::env::var(name_str).ok())
            .map(|s| s.into_bytes())
    }
}

// ── Opaque library handle ─────────────────────────────────────────────────────
//
//
// In this port, the library identity is the opaque `DynLibId(u64)` allocated
// by the embedder-installed [`DynLibLoadHook`]. `lua-stdlib` never inspects
// the value; it stashes the raw `u64` in `_CLIBS` as light userdata (cast
// through `*mut c_void` to match C-Lua's representation) and hands it back to
// the symbol and unload hooks.

// ── Byte-string utilities ─────────────────────────────────────────────────────

/// Append to `buf` the bytes of `s` with all non-overlapping occurrences of
/// `pattern` replaced by `replacement`.
///
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

// ── Platform-specific dynamic-loading dispatch ────────────────────────────────

/// Unload a previously loaded C library.
///
///    — POSIX: `dlclose(lib)`; Windows: `FreeLibrary(lib)`.
///
/// Delegates to [`GlobalState::dynlib_unload_hook`]. When no hook is
/// registered the library is leaked, which matches `libloading`'s safety
/// model (the library must outlive every symbol it exports, and the simplest
/// correct policy is to keep it alive for the state's lifetime).
fn lsys_unloadlib(state: &mut LuaState, lib: DynLibId) {
    if let Some(hook) = state.global().dynlib_unload_hook {
        hook(lib);
    }
}

/// Load a C library from `path`. If `see_glb` is true, make symbols globally
/// visible (POSIX RTLD_GLOBAL). On failure, pushes an error string onto `state`.
///
///    — POSIX: `dlopen(path, RTLD_NOW | (seeglb ? RTLD_GLOBAL : RTLD_LOCAL))`
///    — Windows: `LoadLibraryExA(path, NULL, LUA_LLE_FLAGS)`
///
/// PORT NOTE: returns `(handle, lib_fail_tag)`. The tag is `"absent"` when no
/// hook is registered (matching C's fallback-stub `LIB_FAIL`) and `"open"`
/// when the hook itself reports a failure (matching POSIX/Windows builds).
fn lsys_load(
    state: &mut LuaState,
    path: &[u8],
    see_glb: bool,
) -> (Option<DynLibId>, &'static [u8]) {
    let hook = state.global().dynlib_load_hook;
    let Some(load_fn) = hook else {
        let s = match state.intern_str(DLMSG) {
            Ok(s) => s,
            Err(_) => return (None, LIB_FAIL_ABSENT),
        };
        state.push(LuaValue::Str(s));
        return (None, LIB_FAIL_ABSENT);
    };
    match load_fn(state, path, see_glb) {
        Ok(id) => (Some(id), b"open"),
        // PORT NOTE: `LuaError::File` is reserved for "no shared library at
        // this path". Map it to the fallback-stub `"absent"` tag so that a
        // probe like `package.loadlib("./nonexistent.so", ...)` reports
        // `"absent"` regardless of whether a backend is installed. Every
        // other `Err` is a true open-time failure → `"open"`.
        Err(LuaError::File) => {
            let mut msg = b"cannot find library '".to_vec();
            msg.extend_from_slice(path);
            msg.push(b'\'');
            let s = match state.intern_str(&msg) {
                Ok(s) => s,
                Err(_) => return (None, LIB_FAIL_ABSENT),
            };
            state.push(LuaValue::Str(s));
            (None, LIB_FAIL_ABSENT)
        }
        Err(err) => {
            let msg = error_to_bytes(&err);
            let s = match state.intern_str(&msg) {
                Ok(s) => s,
                Err(_) => return (None, b"open"),
            };
            state.push(LuaValue::Str(s));
            (None, b"open")
        }
    }
}

/// Find symbol `sym` in library `lib` and either push it as a callable Lua
/// function (returning `SymOutcome::Found`) or push an error message string
/// and report which failure category the caller should propagate.
///
///    — POSIX: `cast_func(dlsym(lib, sym))`
///    — Windows: `(lua_CFunction)(voidf)GetProcAddress(lib, sym)`
fn lsys_sym(state: &mut LuaState, lib: DynLibId, sym: &[u8]) -> SymOutcome {
    let hook = state.global().dynlib_symbol_hook;
    let Some(sym_fn) = hook else {
        let s = match state.intern_str(DLMSG) {
            Ok(s) => s,
            Err(_) => return SymOutcome::Missing,
        };
        state.push(LuaValue::Str(s));
        return SymOutcome::Missing;
    };
    match sym_fn(state, lib, sym) {
        Ok(DynamicSymbol::RustNative(f)) => SymOutcome::Found(f),
        Ok(DynamicSymbol::LuaCAbi(_)) => {
            let s = match state.intern_str(C_ABI_UNSUPPORTED_MSG) {
                Ok(s) => s,
                Err(_) => return SymOutcome::Missing,
            };
            state.push(LuaValue::Str(s));
            SymOutcome::Missing
        }
        Ok(DynamicSymbol::Unsupported { reason }) => {
            let s = match state.intern_str(&reason) {
                Ok(s) => s,
                Err(_) => return SymOutcome::Missing,
            };
            state.push(LuaValue::Str(s));
            SymOutcome::Missing
        }
        Err(err) => {
            let msg = error_to_bytes(&err);
            let s = match state.intern_str(&msg) {
                Ok(s) => s,
                Err(_) => return SymOutcome::Missing,
            };
            state.push(LuaValue::Str(s));
            SymOutcome::Missing
        }
    }
}

/// Outcome of `lsys_sym`.
///
/// `Missing` covers every non-success path (unknown symbol, ABI mismatch, hook
/// absent, embedder-supplied refusal); in every case an error-message string
/// has already been pushed onto the Lua stack, so the caller maps `Missing`
/// to `ERRFUNC` / `"init"` without further work.
enum SymOutcome {
    /// Resolved to a Rust-native callable.
    Found(lua_CFunction),
    /// Resolution failed; an error-message string is on the stack.
    Missing,
}

/// Extract a byte-string error message from a `LuaError`, falling back to a
/// debug rendering for non-string variants.
fn error_to_bytes(e: &LuaError) -> Vec<u8> {
    match e {
        LuaError::Runtime(LuaValue::Str(s)) | LuaError::Syntax(LuaValue::Str(s)) => {
            s.as_bytes().to_vec()
        }
        other => format!("{:?}", other).into_bytes(),
    }
}

/// Encode a [`DynLibId`] as a `*mut c_void` for storage in `_CLIBS` as light
/// userdata. The cast is the inverse of [`decode_dynlib_id`]; neither side
/// ever dereferences the pointer.
fn encode_dynlib_id(id: DynLibId) -> *mut std::ffi::c_void {
    id.0 as usize as *mut std::ffi::c_void
}

/// Decode a [`DynLibId`] previously stored via [`encode_dynlib_id`].
fn decode_dynlib_id(p: *mut std::ffi::c_void) -> DynLibId {
    DynLibId(p as usize as u64)
}

// ── Path helpers ──────────────────────────────────────────────────────────────

/// Return `registry["LUA_NOENV"]` as a boolean.
///
fn noenv(state: &mut LuaState) -> bool {
    let _ = state.get_field_registry(b"LUA_NOENV");
    let b = state.to_boolean(-1);
    state.pop_n(1);
    b
}

/// Set `package[fieldname]` to the appropriate path value.
///
/// Priority: versioned env var (e.g. `LUA_PATH_5_4`) → unversioned env var
/// (`LUA_PATH`) → compiled-in default. When the env var contains `;;`, the
/// compiled-in default is spliced in place of `;;`.
///
/// const char *envname, const char *dft)`
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
    let mut nver = envname.to_vec();
    nver.extend_from_slice(LUA_VERSUFFIX);

    let path_opt = if noenv(state) {
        None
    } else {
        getenv_bytes(state, &nver).or_else(|| getenv_bytes(state, envname))
    };

    let final_path: Vec<u8> = if path_opt.is_none() {
        dft.to_vec()
    } else {
        let path = path_opt.unwrap();
        let double_sep = [LUA_PATH_SEP, LUA_PATH_SEP];
        if let Some(dftmark_pos) = find_subslice(&path, &double_sep) {
            // Path contains ";;": replace with default.
            let mut buf = Vec::new();
            if dftmark_pos > 0 {
                buf.extend_from_slice(&path[..dftmark_pos]);
                buf.push(LUA_PATH_SEP);
            }
            buf.extend_from_slice(dft);
            let after = dftmark_pos + 2;
            if after < path.len() {
                buf.push(LUA_PATH_SEP);
                buf.extend_from_slice(&path[after..]);
            }
            buf
        } else {
            path
        }
    };

    // PORT NOTE: On Windows, setprogdir replaces LUA_EXEC_DIR in the path with
    // the directory of the running executable (GetModuleFileNameA). On all other
    // platforms it's a no-op ((void)0). Stubbed here; on Windows this would also
    // require unsafe (Win32 API). The EXEC_DIR substitution is therefore skipped.

    // PORT NOTE: In C the index is -3 because the versioned-name string is still
    // on the stack. In Rust it is -2 because we did not push the versioned name.
    let s = state.intern_str(&final_path)?;
    state.push(LuaValue::Str(s));
    state.set_field(-2, fieldname)?;

    // PORT NOTE: No nver was pushed in Rust; nothing to pop here.

    Ok(())
}

// ── CLIBS registry table ──────────────────────────────────────────────────────

/// Return the library handle stored at `registry._CLIBS[path]`, or `None`.
///
fn checkclib(state: &mut LuaState, path: &[u8]) -> Option<DynLibId> {
    let _ = state.get_field_registry(CLIBS);
    let _ = state.get_field(-1, path);
    let handle = state.to_light_userdata(-1).map(decode_dynlib_id);
    state.pop_n(2);
    handle
}

/// Register a library handle in the CLIBS table (both by path and sequentially).
///
fn addtoclib(state: &mut LuaState, path: &[u8], plib: DynLibId) -> Result<(), LuaError> {
    state.get_field_registry(CLIBS)?;
    state.push(LuaValue::LightUserData(encode_dynlib_id(plib)));
    state.push_value(-1)?;
    state.set_field(-3, path)?;
    let n = state.len_at(-2);
    state.raw_seti(-2, n + 1)?;
    state.pop_n(1);
    Ok(())
}

/// `__gc` metamethod for the CLIBS table: unloads all registered C libraries
/// in reverse order when the Lua state closes.
///
fn gctm(state: &mut LuaState) -> Result<usize, LuaError> {
    let n = state.len_at(1);
    let mut i = n;
    while i >= 1 {
        state.raw_geti(1, i)?;
        if let Some(handle) = state.to_light_userdata(-1).map(decode_dynlib_id) {
            lsys_unloadlib(state, handle);
        }
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
///
/// PORT NOTE: C returns raw `int` error codes. Rust encodes them as `Ok(i32)`
/// so the caller can distinguish "error code + message on stack" from "fatal Err".
/// Status of `lookforfunc`. `Ok(0)` corresponds to C's `0` "success",
/// `ErrLib(tag)` to C's `ERRLIB` (tag is the `LIB_FAIL` string the caller
/// should attach: `"open"` for true dlopen failures, `"absent"` when no
/// backend or the file doesn't exist), `ErrFunc` to C's `ERRFUNC`.
enum LookForFuncStatus {
    /// Loader successfully resolved a symbol (function pushed on stack).
    Ok,
    /// Library could not be opened. `tag` is the `LIB_FAIL` string.
    ErrLib(&'static [u8]),
    /// Library opened but symbol could not be resolved.
    ErrFunc,
}

fn lookforfunc(
    state: &mut LuaState,
    path: &[u8],
    sym: &[u8],
) -> Result<LookForFuncStatus, LuaError> {
    let reg = match checkclib(state, path) {
        Some(handle) => handle,
        None => {
            let (loaded, tag) = lsys_load(state, path, sym.first() == Some(&b'*'));
            match loaded {
                Some(handle) => {
                    addtoclib(state, path, handle)?;
                    handle
                }
                None => return Ok(LookForFuncStatus::ErrLib(tag)),
            }
        }
    };
    if sym.first() == Some(&b'*') {
        state.push(LuaValue::Bool(true));
        return Ok(LookForFuncStatus::Ok);
    }
    match lsys_sym(state, reg, sym) {
        SymOutcome::Found(func) => {
            state.push_c_function(func)?;
            Ok(LookForFuncStatus::Ok)
        }
        SymOutcome::Missing => Ok(LookForFuncStatus::ErrFunc),
    }
}

// ── Lua-callable package functions ────────────────────────────────────────────

/// `package.loadlib(filename, funcname)` — open a C library and return a
/// Lua-callable wrapper for `funcname`.
///
/// Returns: on success, the loader function (1 value).
/// On error: `false`, error-message string, and `"open"` or `"init"` (3 values).
///
pub fn ll_loadlib(state: &mut LuaState) -> Result<usize, LuaError> {
    let path = state.check_arg_string(1)?.to_vec();
    let init = state.check_arg_string(2)?.to_vec();
    let stat = lookforfunc(state, &path, &init)?;
    let where_bytes: &[u8] = match stat {
        LookForFuncStatus::Ok => return Ok(1),
        LookForFuncStatus::ErrLib(tag) => tag,
        LookForFuncStatus::ErrFunc => b"init",
    };
    // PORT NOTE: luaL_pushfail pushes `false` in Lua 5.4 (changed from nil).
    state.push(LuaValue::Bool(false));
    state.insert(-2)?;
    //
    // PORT NOTE: the `LIB_FAIL` tag is chosen at run time. The CLI backend
    // reports `LuaError::File` for a missing library → `"absent"` (matching
    // C-Lua's no-dlfcn fallback); a true `dlopen` failure → `"open"`. The
    // "init" branch (symbol resolution failed after the library opened) is
    // identical in every build.
    let where_s = state.intern_str(where_bytes)?;
    state.push(LuaValue::Str(where_s));
    Ok(3)
}

// ── File existence check ──────────────────────────────────────────────────────

/// Try to open `filename` for reading; return `true` if it succeeds.
///
///    — `FILE *f = fopen(filename, "r"); if (f == NULL) return 0;`
///
/// PORT NOTE: `std::fs` is banned in `lua-stdlib`, so the actual file probe is
/// delegated to the embedder-registered `file_loader_hook` on `GlobalState`.
/// Without a hook installed, `readable` reports `false` (file system unreachable).
fn readable(state: &LuaState, filename: &[u8]) -> bool {
    match state.global().file_loader_hook {
        Some(hook) => hook(filename).is_ok(),
        None => false,
    }
}

// ── Path-component iterator ───────────────────────────────────────────────────

/// Iterator over `;`-separated path components.
///
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
fn pusherrornotfound(state: &mut LuaState, path: &[u8]) -> Result<(), LuaError> {
    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(b"no file '");
    gsub_append(&mut buf, path, &[LUA_PATH_SEP], b"'\n\tno file '");
    buf.push(b'\'');
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
/// const char *path, const char *sep,
/// const char *dirsep)`
fn searchpath(
    state: &mut LuaState,
    name: &[u8],
    path: &[u8],
    sep: &[u8],
    dirsep: &[u8],
) -> Result<Option<Vec<u8>>, LuaError> {
    //        name = luaL_gsub(L, name, sep, dirsep);
    let name_buf: Vec<u8> = if !sep.is_empty() && name.contains(&sep[0]) {
        gsub_bytes(name, sep, dirsep)
    } else {
        name.to_vec()
    };

    // Build pathname list: replace every '?' in path with the (adjusted) name.
    let pathname: Vec<u8> = gsub_bytes(path, &[LUA_PATH_MARK], &name_buf);

    for filename in PathComponents::new(&pathname) {
        if readable(state, filename) {
            let s = state.intern_str(filename)?;
            state.push(LuaValue::Str(s));
            return Ok(Some(filename.to_vec()));
        }
    }

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
pub fn ll_searchpath(state: &mut LuaState) -> Result<usize, LuaError> {
    let name = state.check_arg_string(1)?.to_vec();
    let path = state.check_arg_string(2)?.to_vec();
    let sep = state.opt_arg_string(3, b".")?;
    let dirsep_default = [LUA_DIRSEP];
    let dirsep = state.opt_arg_string(4, &dirsep_default)?;

    let found = searchpath(state, &name, &path, &sep, &dirsep)?;
    if found.is_some() {
        return Ok(1);
    }
    state.push(LuaValue::Bool(false));
    state.insert(-2)?;
    Ok(2)
}

/// Find a module file using the path stored in `package[pname]`.
///
/// const char *pname, const char *dirsep)`
fn findfile(state: &mut LuaState, name: &[u8], pname: &[u8], dirsep: u8) -> Result<Option<Vec<u8>>, LuaError> {
    // The package table is upvalue #1 for the searcher closures.
    let uv = state.upvalue_index(1);
    let _ = state.get_field(uv, pname);
    let path_opt: Option<Vec<u8>> = state.to_bytes(-1);
    let Some(path) = path_opt else {
        state.pop_n(1);
        return Err(LuaError::runtime(format_args!(
            "'package.{}' must be a string",
            String::from_utf8_lossy(pname)
        )));
    };
    state.pop_n(1);
    searchpath(state, name, &path, b".", &[dirsep])
}

/// Check whether a module load succeeded, returning the open function + filename
/// (2 values) on success or raising an error on failure.
///
fn checkload(state: &mut LuaState, stat: bool, filename: &[u8]) -> Result<usize, LuaError> {
    if stat {
        let s = state.intern_str(filename)?;
        state.push(LuaValue::Str(s));
        Ok(2)
    } else {
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
fn searcher_lua(state: &mut LuaState) -> Result<usize, LuaError> {
    let name = state.check_arg_string(1)?.to_vec();
    let filename = findfile(state, &name, b"path", LUA_LSUBSEP)?;
    if filename.is_none() {
        return Ok(1);
    }
    let filename = filename.unwrap();
    //
    // PORT NOTE: `std::fs` is banned in `lua-stdlib`, so file contents come in
    // via the embedder-registered `file_loader_hook` on `GlobalState`. We then
    // parse them through `state.load(...)` (which dispatches to the parser
    // hook) and place the resulting closure on the stack so `checkload` can
    // pair it with the filename.
    let chunk = match state.global().file_loader_hook {
        Some(hook) => hook(&filename),
        None => Err(LuaError::runtime(format_args!(
            "no file_loader_hook registered; cannot read '{}'",
            String::from_utf8_lossy(&filename)
        ))),
    };
    let load_ok = match chunk {
        Ok(bytes) => {
            // Use a chunk name of the form `@filename` matching C's luaL_loadfilex.
            let mut chunkname = b"@".to_vec();
            chunkname.extend_from_slice(&filename);
            match state.load(&bytes, &chunkname, None) {
                Ok(true) => true,
                Ok(false) => false,
                Err(e) => {
                    let msg = match e {
                        LuaError::Syntax(LuaValue::Str(ref s))
                        | LuaError::Runtime(LuaValue::Str(ref s)) => s.as_bytes().to_vec(),
                        other => format!("{:?}", other).into_bytes(),
                    };
                    let s = state.intern_str(&msg)?;
                    state.push(LuaValue::Str(s));
                    false
                }
            }
        }
        Err(e) => {
            let msg = match e {
                LuaError::Runtime(LuaValue::Str(ref s)) => s.as_bytes().to_vec(),
                other => format!("{:?}", other).into_bytes(),
            };
            let s = state.intern_str(&msg)?;
            state.push(LuaValue::Str(s));
            false
        }
    };
    checkload(state, load_ok, &filename)
}

/// Try to load `modname`'s open function from the C dynamic library at `filename`.
///
/// Handles the "ignore mark" (`-`) convention: `"foo-bar"` first tries
/// `luaopen_foo`, then `luaopen_bar` as a fallback.
///
fn loadfunc(
    state: &mut LuaState,
    filename: &[u8],
    modname: &[u8],
) -> Result<LookForFuncStatus, LuaError> {
    let modname: Vec<u8> = gsub_bytes(modname, b".", LUA_OFSEP);

    if let Some(mark_pos) = modname.iter().position(|&b| b == LUA_IGMARK) {
        let prefix = &modname[..mark_pos];
        let mut openfunc = LUA_POF.to_vec();
        openfunc.extend_from_slice(prefix);
        let stat = lookforfunc(state, filename, &openfunc)?;
        if !matches!(stat, LookForFuncStatus::ErrFunc) {
            return Ok(stat);
        }
        let tail = &modname[mark_pos + 1..];
        let mut openfunc2 = LUA_POF.to_vec();
        openfunc2.extend_from_slice(tail);
        return lookforfunc(state, filename, &openfunc2);
    }

    let mut openfunc = LUA_POF.to_vec();
    openfunc.extend_from_slice(&modname);
    lookforfunc(state, filename, &openfunc)
}

/// Searcher that looks in `package.cpath` for a C dynamic library.
///
fn searcher_c(state: &mut LuaState) -> Result<usize, LuaError> {
    let name = state.check_arg_string(1)?.to_vec();
    let filename = findfile(state, &name, b"cpath", LUA_CSUBSEP)?;
    if filename.is_none() {
        return Ok(1);
    }
    let filename = filename.unwrap();
    let stat = loadfunc(state, &filename, &name)?;
    let ok = matches!(stat, LookForFuncStatus::Ok);
    checkload(state, ok, &filename)
}

/// Searcher that looks in `package.cpath` using only the root component
/// (everything before the first `.`) of the module name.
///
fn searcher_croot(state: &mut LuaState) -> Result<usize, LuaError> {
    let name = state.check_arg_string(1)?.to_vec();
    let dot_pos = name.iter().position(|&b| b == b'.');
    if dot_pos.is_none() {
        return Ok(0);
    }
    let dot_pos = dot_pos.unwrap();

    let root = &name[..dot_pos];
    let root_s = state.intern_str(root)?;
    state.push(LuaValue::Str(root_s));

    // PORT NOTE: C reads the root string back from the stack; in Rust we use
    // the slice directly and then pop the stack entry below.
    let filename = findfile(state, root, b"cpath", LUA_CSUBSEP)?;
    // Pop the root string we pushed above (findfile does not consume it).
    state.pop_n(1);

    if filename.is_none() {
        return Ok(1);
    }
    let filename = filename.unwrap();

    let stat = loadfunc(state, &filename, &name)?;
    match stat {
        LookForFuncStatus::Ok => {}
        LookForFuncStatus::ErrFunc => {
            let mut msg = b"no module '".to_vec();
            msg.extend_from_slice(&name);
            msg.extend_from_slice(b"' in file '");
            msg.extend_from_slice(&filename);
            msg.push(b'\'');
            let s = state.intern_str(&msg)?;
            state.push(LuaValue::Str(s));
            return Ok(1);
        }
        LookForFuncStatus::ErrLib(_) => {
            return checkload(state, false, &filename);
        }
    }

    let s = state.intern_str(&filename)?;
    state.push(LuaValue::Str(s));
    Ok(2)
}

/// Searcher that looks in `package.preload` for a pre-registered loader.
///
fn searcher_preload(state: &mut LuaState) -> Result<usize, LuaError> {
    let name = state.check_arg_string(1)?.to_vec();
    state.get_field_registry(b"_PRELOAD")?;
    let ty = state.get_field(-1, &name)?;
    if ty == LuaType::Nil {
        let mut msg = b"no field package.preload['".to_vec();
        msg.extend_from_slice(&name);
        msg.push(b'\'');
        msg.push(b']');
        let s = state.intern_str(&msg)?;
        state.push(LuaValue::Str(s));
        return Ok(1);
    }
    let tag = state.intern_str(b":preload:")?;
    state.push(LuaValue::Str(tag));
    Ok(2)
}

// ── require implementation ────────────────────────────────────────────────────

/// Iterate through `package.searchers` to find a loader for module `name`.
///
/// On success, leaves `(loader_function, loader_data)` at the top of the stack
/// (below the searchers table). On failure, raises a runtime error.
///
///
/// TODO(port): The exact absolute stack indices used in C (index 3 for the
/// searchers table) depend on the caller (`ll_require`) having set up the
/// stack in a specific way. In Rust we use relative indices. The behaviour
/// should match C but the index arithmetic must be verified in Phase B.
fn findloader(state: &mut LuaState, name: &[u8]) -> Result<(), LuaError> {
    //        luaL_error(L, "'package.searchers' must be a table");
    let uv = state.upvalue_index(1);
    let ty = state.get_field(uv, b"searchers")?;
    if ty != LuaType::Table {
        return Err(LuaError::runtime(format_args!(
            "'package.searchers' must be a table"
        )));
    }
    // Searchers table is now at the top of the stack.

    let mut msg_buf: Vec<u8> = Vec::new();

    let mut i: i64 = 1;
    loop {
        msg_buf.extend_from_slice(b"\n\t");

        // PORT NOTE: In C the searchers table is at absolute index 3. In Rust
        // it is at -1 (relative to the top). TODO(port): verify this is correct
        // after accounting for whatever else the caller left on the stack.
        let item_ty = state.raw_geti(-1, i)?;
        if item_ty == LuaType::Nil {
            state.pop_n(1);
            let len = msg_buf.len();
            if len >= 2 {
                msg_buf.truncate(len - 2);
            }
            // Build the error message as a Lua string then raise.
            let mut err = b"module '".to_vec();
            err.extend_from_slice(name);
            err.extend_from_slice(b"' not found:");
            err.extend_from_slice(&msg_buf);
            let err_s = state.intern_str(&err)?;
            return Err(LuaError::from_value(LuaValue::Str(err_s)));
        }

        let name_s = state.intern_str(name)?;
        state.push(LuaValue::Str(name_s));

        state.call(1, 2)?;

        // After call: two return values r1 (at -2) and r2 (at -1) on top.
        if state.type_at(-2) == LuaType::Function {
            // Loader found; leave (r1=function, r2=data) on stack and return.
            return Ok(());
        }

        if state.type_at(-2) == LuaType::String {
            // r1 is an error-message string from the searcher.
            state.pop_n(1);
            if let Some(bytes) = state.to_bytes(-1) {
                msg_buf.extend_from_slice(&bytes);
            }
            state.pop_n(1);
        } else {
            state.pop_n(2);
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
pub fn ll_require(state: &mut LuaState) -> Result<usize, LuaError> {
    let name = state.check_arg_string(1)?.to_vec();

    // PORT NOTE: must use the public-API `set_top` (relative to the current
    // C-frame's `func`), not `LuaState::set_top` which is an inherent that
    // sets an absolute stack index and would truncate the entire stack.
    lua_vm::api::set_top(state, 1)?;

    state.get_field_registry(b"_LOADED")?;

    state.get_field(2, &name)?;

    if state.to_boolean(-1) {
        return Ok(1);
    }

    state.pop_n(1);

    // After this, the stack has: [name(1), LOADED(2), searchers(3), loader(-2), loaderdata(-1)]
    findloader(state, &name)?;

    // Swaps loader and loaderdata: [..., loaderdata, loader]
    state.rotate(-2, 1)?;

    state.push_value(1)?;

    // PORT NOTE: After the rotate, loaderdata is 3 from top (-3). In C this is
    // at absolute index 4 (but C uses the pre-rotate layout). TODO(port): verify.
    state.push_value(-3)?;

    state.call(2, 1)?;

    if state.type_at(-1) != LuaType::Nil {
        state.set_field(2, &name)?;
    } else {
        state.pop_n(1);
    }

    let ty = state.get_field(2, &name)?;
    if ty == LuaType::Nil {
        state.push(LuaValue::Bool(true));
        state.copy_value(-1, -2)?;
        state.set_field(2, &name)?;
    }

    state.rotate(-2, 1)?;

    Ok(2)
}

// ── Package library setup ─────────────────────────────────────────────────────

/// Create the `searchers` table and install the four built-in searchers, each
/// with the `package` table as upvalue #1.
///
fn createsearcherstable(state: &mut LuaState) -> Result<(), LuaError> {
    //        searcher_Lua, searcher_C, searcher_Croot, NULL };
    let searchers: &[fn(&mut LuaState) -> Result<usize, LuaError>] = &[
        searcher_preload,
        searcher_lua,
        searcher_c,
        searcher_croot,
    ];

    state.create_table(searchers.len() as i32, 0)?;

    for (i, &f) in searchers.iter().enumerate() {
        state.push_value(-2)?;
        // TODO(port): push_c_closure takes the function and n upvalues from the
        //             stack. The package table upvalue must be correctly associated
        //             with each searcher closure so that findfile can access it
        //             via lua_upvalueindex(1). Verify in Phase B.
        state.push_c_closure(f, 1)?;
        state.raw_seti(-2, (i + 1) as i64)?;
    }
    state.set_field(-2, b"searchers")?;
    Ok(())
}

/// Create the `_CLIBS` registry table with a `__gc` finalizer that closes all
/// loaded C libraries when the Lua state is closed.
///
fn createclibstable(state: &mut LuaState) -> Result<(), LuaError> {
    state.get_subtable_registry(CLIBS)?;
    state.create_table(0, 1)?;
    // TODO(phase-b): LuaClosure::LightC currently typed fn() -> i32 in lua-types; use push_c_function until widened.
    state.push_c_function(gctm)?;
    state.set_field(-2, b"__gc")?;
    state.set_metatable(-2)?;
    Ok(())
}

/// Open the `package` library and return the `package` table.
///
pub fn luaopen_package(state: &mut LuaState) -> Result<usize, LuaError> {
    createclibstable(state)?;

    // PORT NOTE: The C pk_funcs table also contains placeholder entries for
    // "preload", "cpath", "path", "searchers", "loaded" (all NULL). In Rust
    // those fields are set explicitly below; only the real functions are here.
    state.new_lib(&[
        (b"loadlib" as &[u8], ll_loadlib as fn(&mut LuaState) -> Result<usize, LuaError>),
        (b"searchpath", ll_searchpath as fn(&mut LuaState) -> Result<usize, LuaError>),
    ])?;

    createsearcherstable(state)?;

    setpath(state, b"path", LUA_PATH_VAR, LUA_PATH_DEFAULT)?;

    setpath(state, b"cpath", LUA_CPATH_VAR, LUA_CPATH_DEFAULT)?;

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

    state.set_field(-2, b"config")?;

    state.get_subtable_registry(b"_LOADED")?;
    state.set_field(-2, b"loaded")?;

    state.get_subtable_registry(b"_PRELOAD")?;
    state.set_field(-2, b"preload")?;

    state.push_globals()?;
    state.push_value(-2)?;
    state.set_funcs_with_upvalues(
        &[(b"require" as &[u8], ll_require as fn(&mut LuaState) -> Result<usize, LuaError>)],
        1,
    )?;
    state.pop_n(1);

    Ok(1)
}

// ──────────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        src/loadlib.c  (758 lines, 25 functions)
//   target_crate:  lua-stdlib
//   confidence:    medium
//   todos:         8
//   port_notes:    7
//   unsafe_blocks: 0   (must be 0 outside explicit unsafe-budget crates)
//   notes:         lsys_load/lsys_sym/lsys_unloadlib now dispatch through
//                  dynlib_*_hook on GlobalState (Phase D-3.5); lua-cli
//                  installs a libloading-backed backend. With no hook
//                  installed, LIB_FAIL is "absent" (matches the C fallback
//                  stub); with a hook installed it is "open". Stock Lua C
//                  ABI symbols resolve but fail with "init" + a clear
//                  unsupported-ABI message (DynamicSymbol::LuaCAbi case);
//                  full C-ABI compatibility is a separate project. readable()
//                  and searcher_lua are wired through file_loader_hook on
//                  GlobalState. Stack-index arithmetic in findloader /
//                  ll_require should be verified in Phase B. LUA_PATH_DEFAULT
//                  / LUA_CPATH_DEFAULT are hardcoded and must be replaced
//                  with platform configuration constants.
// ──────────────────────────────────────────────────────────────────────────────
