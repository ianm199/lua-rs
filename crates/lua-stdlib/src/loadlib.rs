//! The Lua `package` library: `require`, `package.loadlib`,
//! `package.searchpath`, and the four built-in module searchers (preload,
//! Lua-file, C-library, C-root).
//!
//! ## Graduation (Idiomatization Sprint 2 / Phase 2 — cold, platform-FFI module)
//!
//! Split cleanly into two regimes, and treated as such:
//!
//! * **Deterministic pure-Lua package logic** — now guarded by
//!   `tests/loadlib_strengthen.rs` (16 reference-pinned cross-version
//!   assertions). Strengthening that net FIRST caught **seven** divergences our
//!   weaker net hid: the 5.1 `package.config` trailing newline, `require`'s 5.4+
//!   2nd return value, the 5.1 preload-loader arg count, a C-root searcher
//!   message truncation, the `nil`-vs-`false` `luaL_pushfail` value, the 5.1
//!   absence of `package.searchpath`, and the 5.2/5.3 searchpath-error leading
//!   separator. All were fixed via single-source version helpers; the version
//!   gates are explicit and load-bearing. See `GRADUATED.md` "loadlib".
//! * **Platform / dynamic-loading FFI** — left LOAD-BEARING and untouched. The
//!   three platform calls (`lsys_load`, `lsys_sym`, `lsys_unloadlib`) dispatch
//!   through embedder hooks on [`lua_vm::state::GlobalState`]
//!   (`dynlib_load_hook`, `dynlib_symbol_hook`, `dynlib_unload_hook`); `lua-cli`
//!   installs a `libloading`-backed (genuinely `unsafe`) implementation, while
//!   embeddings that omit the hooks behave like C-Lua's fallback stub
//!   (`LIB_FAIL = "absent"`). This indirection keeps `lua-stdlib` itself
//!   `unsafe`-free (`unsafe_code = "forbid"`); the real FFI bridge lives in
//!   `lua-cli`. Its behavior — the dlopen/dlsym path, the platform error
//!   strings, the `"open"`/`"absent"`/`"init"` tags — needs a real shared
//!   object and host loader, so it is NOT reference-pinnable and is a documented
//!   honest-negative (the analogue of math's platform `rand()`).

use crate::state_stub::{lua_CFunction, LuaState, LuaStateStubExt as _};
use lua_types::{LuaError, LuaType, LuaValue};
use lua_vm::state::{DynLibId, DynamicSymbol};

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

// The fail-tag spelling travels with `LookForFuncStatus` (below) rather than a
// single compile-time `LIB_FAIL` constant, so each failure carries its own tag.

// Pushed when no `dynlib_load_hook`/`dynlib_symbol_hook` is registered on
// `GlobalState`. With a backend installed the CLI supplies its own error
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

/// Build the `package.config` string for `version`.
///
/// Five lines encoding the platform separators: directory separator, path
/// separator, the `?` substitution mark, the `!` exec-dir mark, and the `-`
/// ignore mark. The trailing newline after the ignore mark is a **5.2 addition**
/// (`LUA_IGMARK "\n"` in 5.2+ `loadlib.c`); 5.1's string ends at `-`, so 5.1 is
/// 9 bytes and 5.2+ are 10 (pinned in `tests/loadlib_strengthen.rs`).
fn package_config(version: lua_types::LuaVersion) -> Vec<u8> {
    let mut config = vec![
        LUA_DIRSEP,
        b'\n',
        LUA_PATH_SEP,
        b'\n',
        LUA_PATH_MARK,
        b'\n',
        b'!',
        b'\n',
        LUA_IGMARK,
    ];
    if !matches!(version, lua_types::LuaVersion::V51) {
        config.push(b'\n');
    }
    config
}

// ── Version-derived package-path defaults (issue #273) ───────────────────────
//
// `GlobalState::lua_version` is the single source of truth for every byte
// below; nothing here is a per-lookup computation — `luaopen_package` (the
// library-init cold path, run once per `Lua` instance) is the only caller.
//
// Every default was captured directly from the unmodified upstream `make
// macosx` build of each version (`specs/oracle/CONTRACT.md`,
// `/tmp/lua-refs/bin/lua5.x`), not read off of `luaconf.h` — see
// `tests/loadlib_strengthen.rs` for the pinned assertions and this crate's
// PR for the per-version diff transcript.

/// The version-directory segment baked into default package paths and into
/// versioned environment-variable names (e.g. `5.4` in
/// `/usr/local/share/lua/5.4/?.lua` and in `LUA_PATH_5_4`). Mirrors upstream's
/// `LUA_VDIR` (`luaconf.h`).
///
/// The trailing wildcard arm is unreachable in practice: every public
/// constructor (`Lua::with_hooks_versioned` and friends) refuses to build a
/// `LuaVersion` for which [`lua_types::LuaVersion::is_supported`] is false,
/// and today's five supported variants are all matched above it. It exists
/// only because `LuaVersion` is `#[non_exhaustive]` from this crate's point
/// of view.
fn lua_vdir(version: lua_types::LuaVersion) -> &'static [u8] {
    match version {
        lua_types::LuaVersion::V51 => b"5.1",
        lua_types::LuaVersion::V52 => b"5.2",
        lua_types::LuaVersion::V53 => b"5.3",
        lua_types::LuaVersion::V54 => b"5.4",
        lua_types::LuaVersion::V55 => b"5.5",
        _ => b"5.4",
    }
}

/// The `LUA_VERSUFFIX` value for `version` (e.g. `_5_4`), appended to
/// `LUA_PATH`/`LUA_CPATH` to build the versioned environment-variable name an
/// instance consults first (`LUA_PATH_5_4`, `LUA_CPATH_5_3`, ...). See
/// [`lua_vdir`] for the wildcard-arm note.
fn lua_versuffix(version: lua_types::LuaVersion) -> &'static [u8] {
    match version {
        lua_types::LuaVersion::V51 => b"_5_1",
        lua_types::LuaVersion::V52 => b"_5_2",
        lua_types::LuaVersion::V53 => b"_5_3",
        lua_types::LuaVersion::V54 => b"_5_4",
        lua_types::LuaVersion::V55 => b"_5_5",
        _ => b"_5_4",
    }
}

/// Whether `version` consults a versioned environment variable
/// (`LUA_PATH_5_x`/`LUA_CPATH_5_x`) at all before falling back to the
/// unversioned `LUA_PATH`/`LUA_CPATH`. Versioned env vars are a **5.2+**
/// addition; 5.1 only ever reads the unversioned name — verified against
/// `lua5.1.5`, where setting `LUA_PATH_5_1` has no effect on `package.path`.
fn has_versioned_env_vars(version: lua_types::LuaVersion) -> bool {
    !matches!(version, lua_types::LuaVersion::V51)
}

/// `LUA_LDIR` for `version`: where installed pure-Lua modules live
/// (`/usr/local/share/lua/<vdir>/`).
fn lua_ldir(version: lua_types::LuaVersion) -> Vec<u8> {
    let mut dir = b"/usr/local/share/lua/".to_vec();
    dir.extend_from_slice(lua_vdir(version));
    dir.push(b'/');
    dir
}

/// `LUA_CDIR` for `version`: where installed C modules live
/// (`/usr/local/lib/lua/<vdir>/`).
fn lua_cdir(version: lua_types::LuaVersion) -> Vec<u8> {
    let mut dir = b"/usr/local/lib/lua/".to_vec();
    dir.extend_from_slice(lua_vdir(version));
    dir.push(b'/');
    dir
}

/// The compiled-in `package.path` default for `version`. The entry SHAPE, not
/// just the version segment, differs by era:
/// - **5.1**: `./?.lua` FIRST, then `LDIR`/`CDIR`, no trailing `./?/init.lua`.
/// - **5.2**: `LDIR`/`CDIR` first, `./?.lua` LAST, no `./?/init.lua` at all.
/// - **5.3/5.4/5.5**: `LDIR`/`CDIR` first, then BOTH `./?.lua` and
///   `./?/init.lua` last (the `LUA_SHRDIR`-derived shape 5.3 introduced).
#[cfg(not(target_os = "windows"))]
fn lua_path_default(version: lua_types::LuaVersion) -> Vec<u8> {
    let ldir = lua_ldir(version);
    let cdir = lua_cdir(version);
    let mut path = Vec::new();
    match version {
        lua_types::LuaVersion::V51 => {
            path.extend_from_slice(b"./?.lua;");
            path.extend_from_slice(&ldir);
            path.extend_from_slice(b"?.lua;");
            path.extend_from_slice(&ldir);
            path.extend_from_slice(b"?/init.lua;");
            path.extend_from_slice(&cdir);
            path.extend_from_slice(b"?.lua;");
            path.extend_from_slice(&cdir);
            path.extend_from_slice(b"?/init.lua");
        }
        lua_types::LuaVersion::V52 => {
            path.extend_from_slice(&ldir);
            path.extend_from_slice(b"?.lua;");
            path.extend_from_slice(&ldir);
            path.extend_from_slice(b"?/init.lua;");
            path.extend_from_slice(&cdir);
            path.extend_from_slice(b"?.lua;");
            path.extend_from_slice(&cdir);
            path.extend_from_slice(b"?/init.lua;./?.lua");
        }
        _ => {
            path.extend_from_slice(&ldir);
            path.extend_from_slice(b"?.lua;");
            path.extend_from_slice(&ldir);
            path.extend_from_slice(b"?/init.lua;");
            path.extend_from_slice(&cdir);
            path.extend_from_slice(b"?.lua;");
            path.extend_from_slice(&cdir);
            path.extend_from_slice(b"?/init.lua;./?.lua;./?/init.lua");
        }
    }
    path
}

/// The Windows fallback default is version-flat, unchanged by this fix: the
/// `!`-relative (`LUA_EXEC_DIR`) substitution real Windows builds perform via
/// `setprogdir` is not implemented on this platform (see `setpath`'s doc
/// comment on the same gap), so there is no faithful versioned
/// installed-module directory to build here either.
#[cfg(target_os = "windows")]
fn lua_path_default(_version: lua_types::LuaVersion) -> Vec<u8> {
    b"./?.lua;./?/init.lua".to_vec()
}

/// The compiled-in `package.cpath` default for `version` — same era split as
/// [`lua_path_default`]: 5.1 puts `./?.so` FIRST with no `./` alternative
/// after `CDIR`'s entries; 5.2+ share one shape (`CDIR` entries, then
/// `./?.so` last).
#[cfg(not(target_os = "windows"))]
fn lua_cpath_default(version: lua_types::LuaVersion) -> Vec<u8> {
    let cdir = lua_cdir(version);
    let mut path = Vec::new();
    match version {
        lua_types::LuaVersion::V51 => {
            path.extend_from_slice(b"./?.so;");
            path.extend_from_slice(&cdir);
            path.extend_from_slice(b"?.so;");
            path.extend_from_slice(&cdir);
            path.extend_from_slice(b"loadall.so");
        }
        _ => {
            path.extend_from_slice(&cdir);
            path.extend_from_slice(b"?.so;");
            path.extend_from_slice(&cdir);
            path.extend_from_slice(b"loadall.so;./?.so");
        }
    }
    path
}

/// See [`lua_path_default`]'s Windows note — unchanged, version-flat.
#[cfg(target_os = "windows")]
fn lua_cpath_default(_version: lua_types::LuaVersion) -> Vec<u8> {
    b"./?.dll".to_vec()
}

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
/// Returns `(handle, lib_fail_tag)`. The tag is `"absent"` when no hook is
/// registered (matching C's fallback-stub `LIB_FAIL`) and `"open"` when the
/// hook itself reports a failure (matching POSIX/Windows builds).
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
        // `LuaError::File` is reserved for "no shared library at this path":
        // map it to the fallback-stub `"absent"` tag so a probe like
        // `package.loadlib("./nonexistent.so", ...)` reports `"absent"`
        // regardless of whether a backend is installed. Every other `Err` is a
        // true open-time failure → `"open"`.
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
    match e.message_bytes() {
        Some(b) => b.to_vec(),
        None => format!("{:?}", e).into_bytes(),
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
/// Priority: versioned env var (e.g. `LUA_PATH_5_4`, only consulted when
/// [`has_versioned_env_vars`] is true for this instance's version — 5.1 has
/// no versioned env vars at all) → unversioned env var (`LUA_PATH`) →
/// compiled-in default. When the env var contains `;;`, the compiled-in
/// default is spliced in place of `;;`. The caller must leave the `package`
/// table at the stack top; the path value is set on it directly (the
/// versioned env-var name is computed off-stack, so no index bookkeeping is
/// needed).
fn setpath(
    state: &mut LuaState,
    fieldname: &[u8],
    envname: &[u8],
    dft: &[u8],
) -> Result<(), LuaError> {
    let version = state.global().lua_version;

    let path_opt = if noenv(state) {
        None
    } else if has_versioned_env_vars(version) {
        let mut nver = envname.to_vec();
        nver.extend_from_slice(lua_versuffix(version));
        getenv_bytes(state, &nver).or_else(|| getenv_bytes(state, envname))
    } else {
        getenv_bytes(state, envname)
    };

    let final_path: Vec<u8> = match path_opt {
        None => dft.to_vec(),
        Some(path) if double_semicolon_splice_is_legacy(version) => {
            legacy_double_semicolon_splice(&path, dft)
        }
        Some(path) => modern_double_semicolon_splice(&path, dft),
    };

    // The Windows `setprogdir` step (replace `LUA_EXEC_DIR` with the running
    // executable's directory via `GetModuleFileNameA`, a Win32/`unsafe` call) is
    // a no-op on every other platform and is not yet implemented here, so the
    // `LUA_EXEC_DIR` substitution is skipped.
    let s = state.intern_str(&final_path)?;
    state.push(LuaValue::Str(s));
    state.set_field(-2, fieldname)?;

    Ok(())
}

/// Whether `version`'s `;;`-in-env-var default splice is the LEGACY,
/// position-independent `gsub` (5.1/5.2/5.3's `luaL_gsub(path, ";;",
/// ";AUXMARK;")` then `luaL_gsub(_, AUXMARK, dft)`: EVERY non-overlapping
/// `;;` occurrence gets the default spliced in, wrapped in separators on
/// both sides regardless of position) rather than the single-shot,
/// position-aware splice **5.4** introduced (only the FIRST `;;` is
/// replaced, and a boundary separator is omitted when `;;` sits at the very
/// start or end of the string). Verified against `lua5.1.5`/`lua5.4.7`:
/// `LUA_PATH="/a?;;;;/b"` splices the default TWICE on 5.1 (once per `;;`
/// pair) but only once on 5.4, leaving the remaining `;;` literal.
fn double_semicolon_splice_is_legacy(version: lua_types::LuaVersion) -> bool {
    matches!(
        version,
        lua_types::LuaVersion::V51 | lua_types::LuaVersion::V52 | lua_types::LuaVersion::V53
    )
}

/// The 5.1/5.2/5.3 `;;` default splice: every non-overlapping occurrence of
/// `;;` in `path` becomes `;` + `dft` + `;`, unconditionally. Collapses
/// upstream's two-step `gsub(path, ";;", ";AUXMARK;")` then
/// `gsub(_, AUXMARK, dft)` into one pass — the `AUXMARK` indirection exists
/// upstream only to avoid re-scanning `dft` for the first gsub's pattern, and
/// a single combined replacement already has that property (`dft` is
/// inserted verbatim into the output, never re-scanned for `;;`).
fn legacy_double_semicolon_splice(path: &[u8], dft: &[u8]) -> Vec<u8> {
    let double_sep = [LUA_PATH_SEP, LUA_PATH_SEP];
    if find_subslice(path, &double_sep).is_none() {
        return path.to_vec();
    }
    let mut replacement = vec![LUA_PATH_SEP];
    replacement.extend_from_slice(dft);
    replacement.push(LUA_PATH_SEP);
    gsub_bytes(path, &double_sep, &replacement)
}

/// The 5.4/5.5 `;;` default splice: only the FIRST occurrence of `;;` is
/// replaced; the leading separator is omitted when `;;` starts the string
/// (no prefix) and the trailing separator is omitted when `;;` ends it (no
/// suffix).
fn modern_double_semicolon_splice(path: &[u8], dft: &[u8]) -> Vec<u8> {
    let double_sep = [LUA_PATH_SEP, LUA_PATH_SEP];
    let Some(dftmark_pos) = find_subslice(path, &double_sep) else {
        return path.to_vec();
    };
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

/// Outcome of looking for a C function in a dynamically loaded library.
///
/// On success the function (or `true` for the `*` sentinel) is on the stack;
/// on a non-fatal failure an error-message string is on the stack and the
/// variant tells the caller what to report. Fatal errors propagate via `Err`.
/// `Ok` is C's success; `ErrLib(tag)` is C's `ERRLIB` carrying the `LIB_FAIL`
/// string (`"open"` for a true dlopen failure, `"absent"` when no backend is
/// installed or the file does not exist); `ErrFunc` is C's `ERRFUNC` (the
/// library opened but the symbol was not found).
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
    // `luaL_pushfail` is `lua_pushnil` on every version (5.4 included); the fail
    // value is `nil`, not `false`. The `LIB_FAIL` tag is chosen at run time: the
    // CLI backend reports `LuaError::File` for a missing library → `"absent"`
    // (matching C-Lua's no-dlfcn fallback), a true `dlopen` failure → `"open"`,
    // and the "init" branch (symbol resolution failed after the library opened)
    // is identical in every build.
    state.push(LuaValue::Nil);
    state.insert(-2)?;
    let where_s = state.intern_str(where_bytes)?;
    state.push(LuaValue::Str(where_s));
    Ok(3)
}

// ── File existence check ──────────────────────────────────────────────────────

/// Whether `filename` can be opened for reading.
///
/// `std::fs` is banned in `lua-stdlib`, so the probe is delegated to the
/// embedder-registered `file_loader_hook` on `GlobalState`. Without a hook
/// installed, `readable` reports `false` (the file system is unreachable) — so
/// the in-process searcher tests deterministically see every path as not-found.
fn readable(state: &LuaState, filename: &[u8]) -> bool {
    match state.global().file_loader_hook {
        Some(hook) => hook(filename).is_ok(),
        None => false,
    }
}

// ── Path-component iterator ───────────────────────────────────────────────────

/// Iterator over `;`-separated path-template components, yielding each as an
/// immutable slice (the C original walked one mutable buffer, swapping each
/// separator for a NUL and back; this produces the identical sequence).
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
/// `sep` bytes in `name` are first replaced by `dirsep`; then each template's
/// `?` is replaced by the adjusted name. On the first readable match, pushes the
/// filename string and returns `Some(filename_bytes)`; otherwise pushes the
/// not-found message and returns `None`.
fn searchpath(
    state: &mut LuaState,
    name: &[u8],
    path: &[u8],
    sep: &[u8],
    dirsep: &[u8],
) -> Result<Option<Vec<u8>>, LuaError> {
    let name_buf: Vec<u8> = if !sep.is_empty() && name.contains(&sep[0]) {
        gsub_bytes(name, sep, dirsep)
    } else {
        name.to_vec()
    };

    let pathname: Vec<u8> = gsub_bytes(path, &[LUA_PATH_MARK], &name_buf);

    for filename in PathComponents::new(&pathname) {
        if readable(state, filename) {
            let s = state.intern_str(filename)?;
            state.push(LuaValue::Str(s));
            return Ok(Some(filename.to_vec()));
        }
    }

    pusherrornotfound(state, &pathname)?;
    Ok(None)
}

/// `package.searchpath(name, path [, sep [, rep]])`.
///
/// Returns the first readable file in `path` with `sep` occurrences in `name`
/// replaced by `rep`. On failure returns `luaL_pushfail` (a `nil`, NOT `false`,
/// on every version) plus the error message. See [`ll_loadlib`] for the same
/// `luaL_pushfail` = `lua_pushnil` translation.
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
    if searchpath_error_has_leading_separator(state.global().lua_version) {
        prepend_searchpath_separator(state)?;
    }
    state.push(LuaValue::Nil);
    state.insert(-2)?;
    Ok(2)
}

/// Whether the standalone `package.searchpath` error message carries a leading
/// `\n\t` separator before its first `no file '…'` line.
///
/// In 5.2/5.3 the `searchpath` helper builds each entry as `"\n\tno file '%s'"`,
/// so the first line is prefixed too; 5.4 moved that prefix into `findloader`'s
/// per-iteration accumulator and made `searchpath`'s own message bare (the form
/// this port's `pusherrornotfound` produces). The `require` trace is unaffected
/// either way — there `findloader` supplies the single `\n\t` per searcher — so
/// the seam is observable ONLY through the standalone Lua function. (5.1 has no
/// `package.searchpath`.) Pinned in `tests/loadlib_strengthen.rs`.
fn searchpath_error_has_leading_separator(version: lua_types::LuaVersion) -> bool {
    matches!(version, lua_types::LuaVersion::V52 | lua_types::LuaVersion::V53)
}

/// Replace the not-found message on the stack top with one carrying a leading
/// `\n\t` (the 5.2/5.3 `searchpath` form). The message produced by
/// `pusherrornotfound` is bare; this restores the legacy prefix.
fn prepend_searchpath_separator(state: &mut LuaState) -> Result<(), LuaError> {
    let Some(bare) = state.to_bytes(-1) else {
        return Ok(());
    };
    state.pop_n(1);
    let mut prefixed = b"\n\t".to_vec();
    prefixed.extend_from_slice(&bare);
    let s = state.intern_str(&prefixed)?;
    state.push(LuaValue::Str(s));
    Ok(())
}

/// Find a module file using the path stored in `package[pname]` (e.g.
/// `package.path` / `package.cpath`), read from upvalue #1 of the searcher
/// closure. Errors if that field is not a string.
fn findfile(
    state: &mut LuaState,
    name: &[u8],
    pname: &[u8],
    dirsep: u8,
) -> Result<Option<Vec<u8>>, LuaError> {
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
        // The error embeds the module name (the `require` arg at stack[1]) and
        // the loader's own error message (the searcher's pushed string at the
        // stack top). Both are owned byte copies, so there is no aliasing.
        let modname = state.to_bytes(1).unwrap_or_else(|| b"?".to_vec());
        let loader_err = state.to_bytes(-1).unwrap_or_else(|| b"?".to_vec());

        let mut msg = b"error loading module '".to_vec();
        msg.extend_from_slice(&modname);
        msg.extend_from_slice(b"' from file '");
        msg.extend_from_slice(filename);
        msg.extend_from_slice(b"':\n\t");
        msg.extend_from_slice(&loader_err);

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
    // `std::fs` is banned in `lua-stdlib`, so file contents arrive via the
    // embedder-registered `file_loader_hook` on `GlobalState`; the bytes are then
    // parsed through `state.load(...)` (which dispatches to the parser hook) and
    // the resulting closure is left on the stack for `checkload` to pair with the
    // filename.
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
                    let msg = match e.message_bytes() {
                        Some(b) => b.to_vec(),
                        None => format!("{:?}", &e).into_bytes(),
                    };
                    let s = state.intern_str(&msg)?;
                    state.push(LuaValue::Str(s));
                    false
                }
            }
        }
        Err(e) => {
            let msg = match e.message_bytes() {
                Some(b) => b.to_vec(),
                None => format!("{:?}", &e).into_bytes(),
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

    let filename = findfile(state, root, b"cpath", LUA_CSUBSEP)?;

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
/// On a hit, every version leaves the loader function on the stack. From **5.4**
/// the searcher also returns the `:preload:` sentinel as loader data (a 2nd
/// value); 5.1/5.2/5.3 return only the function. See [`require_returns_loader_data`].
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
    if !require_returns_loader_data(state.global().lua_version) {
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
/// The accumulated `module '<name>' not found:` message lists one searcher per
/// line; the per-iteration `\n\t` prefix matches 5.4+ `findloader`, while the
/// pre-5.4 searchers prepend their own separator (the two regimes converge on
/// the identical trace, pinned in `tests/loadlib_strengthen.rs`).
fn findloader(state: &mut LuaState, name: &[u8]) -> Result<(), LuaError> {
    let uv = state.upvalue_index(1);
    // In 5.1 the searcher list lives in `package.loaders`; 5.2 renamed it to
    // `package.searchers` (5.2 keeps `loaders` as an alias). Read the name this
    // version exposes. See specs/followup/5.1-roster-syntax.md §1.
    let field: &[u8] = if matches!(state.global().lua_version, lua_types::LuaVersion::V51) {
        b"loaders"
    } else {
        b"searchers"
    };
    let ty = state.get_field(uv, field)?;
    if ty != LuaType::Table {
        return Err(LuaError::runtime(format_args!(
            "'package.searchers' must be a table"
        )));
    }

    let mut msg_buf: Vec<u8> = Vec::new();

    let mut i: i64 = 1;
    loop {
        msg_buf.extend_from_slice(b"\n\t");

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
    let version = state.global().lua_version;

    // Use the public-API `set_top` (relative to the current C-frame's `func`),
    // not the inherent `LuaState::set_top`, which sets an absolute index and
    // would truncate the whole stack.
    lua_vm::api::set_top(state, 1)?;

    state.get_field_registry(b"_LOADED")?;

    state.get_field(2, &name)?;

    if state.to_boolean(-1) {
        return Ok(1);
    }

    state.pop_n(1);

    // `findloader` leaves (loader function, loader data) at the top.
    findloader(state, &name)?;

    if require_passes_loader_data(version) {
        // 5.2+: the loader receives (name, loader data). 5.4+ additionally
        // returns the loader data as `require`'s 2nd value, so the data is kept
        // below the function (rotate) and re-pushed; 5.2/5.3 pass it but discard
        // it (return 1).
        state.rotate(-2, 1)?;
        state.push_value(1)?;
        state.push_value(-3)?;
        state.call(2, 1)?;
    } else {
        // 5.1: the loader receives only the name; there is no loader data.
        state.pop_n(1);
        state.push_value(1)?;
        state.call(1, 1)?;
    }

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

    if require_returns_loader_data(version) {
        // 5.4+: return (module result, loader data). The loader data is still on
        // the stack below the module result; swap them to module-result-first.
        state.rotate(-2, 1)?;
        Ok(2)
    } else {
        // 5.1/5.2/5.3: `ll_require` returns only the module (return 1). On the
        // 5.2/5.3 path the loader data is still on the stack below the result;
        // drop it so the single return value is the module.
        if require_passes_loader_data(version) {
            state.remove(-2)?;
        }
        Ok(1)
    }
}

/// Whether `require` passes the searcher's loader data to the module loader as
/// a SECOND argument (after the module name).
///
/// 5.1's `ll_require` calls the loader with one argument (`lua_call(L, 1, 1)`);
/// 5.2 widened it to two (`lua_call(L, 2, 1)`), so every later version passes the
/// loader data too. Pinned in `tests/loadlib_strengthen.rs`.
fn require_passes_loader_data(version: lua_types::LuaVersion) -> bool {
    !matches!(version, lua_types::LuaVersion::V51)
}

/// Whether `require` returns the searcher's loader data as a SECOND result.
///
/// This is a **5.4** addition (`ll_require`'s `return 2`); 5.1/5.2/5.3 return only
/// the module (`return 1`), so `local _, d = require(m)` yields `d == nil` there.
/// It is the same seam the preload searcher's `:preload:` sentinel rides on
/// (a searcher only bothers returning loader data on a version that surfaces it).
/// Pinned in `tests/loadlib_strengthen.rs`.
fn require_returns_loader_data(version: lua_types::LuaVersion) -> bool {
    matches!(version, lua_types::LuaVersion::V54 | lua_types::LuaVersion::V55)
}

// ── Package library setup ─────────────────────────────────────────────────────

/// Create the `searchers` table and install the four built-in searchers, each
/// with the `package` table as upvalue #1.
///
fn createsearcherstable(state: &mut LuaState) -> Result<(), LuaError> {
    let searchers: &[fn(&mut LuaState) -> Result<usize, LuaError>] =
        &[searcher_preload, searcher_lua, searcher_c, searcher_croot];

    state.create_table(searchers.len() as i32, 0)?;

    for (i, &f) in searchers.iter().enumerate() {
        // Each searcher closes over the `package` table (upvalue #1) so
        // `findfile` can read `package.path`/`package.cpath` via
        // `lua_upvalueindex(1)`.
        state.push_value(-2)?;
        state.push_c_closure(f, 1)?;
        state.raw_seti(-2, (i + 1) as i64)?;
    }
    // Roster name deltas for the searcher list:
    //  - 5.1: the table is named `package.loaders`; there is NO
    //    `package.searchers` (verified against lua5.1.5: `package.searchers` is
    //    nil, `package.loaders` is a table).
    //  - 5.2: renamed to `package.searchers` but kept `package.loaders` as a
    //    compat alias (both point at the same list).
    //  - 5.3+: `package.searchers` only.
    let version = state.global().lua_version;
    let has_loaders = matches!(
        version,
        lua_types::LuaVersion::V51 | lua_types::LuaVersion::V52
    );
    let has_searchers = !matches!(version, lua_types::LuaVersion::V51);
    if has_loaders {
        state.push_value(-1)?;
        state.set_field(-3, b"loaders")?;
    }
    if has_searchers {
        state.set_field(-2, b"searchers")?;
    } else {
        // No `searchers` field under 5.1; drop the table copy left on the stack.
        state.pop_n(1);
    }
    Ok(())
}

/// Create the `_CLIBS` registry table with a `__gc` finalizer that closes all
/// loaded C libraries when the Lua state is closed.
///
fn createclibstable(state: &mut LuaState) -> Result<(), LuaError> {
    state.get_subtable_registry(CLIBS)?;
    state.create_table(0, 1)?;
    state.push_c_function(gctm)?;
    state.set_field(-2, b"__gc")?;
    state.set_metatable(-2)?;
    Ok(())
}

// ── Lua 5.1 `module` / `package.seeall` (deprecated module system) ────────────
//
// These ship only in the default lua5.1.5 build (`loadlib.c`) and were removed
// in 5.2. Registered under the V51 backend; see
// specs/followup/5.1-roster-syntax.md §1. They lean on the 5.1 fenv globals
// model: `module` sets its caller's environment to the module table (via
// `crate::base::set_func_env_at_level`), and `package.seeall` points a module
// table's `__index` at `_G`.

/// `package.seeall(module)` — make a module table inherit globals.
///
/// Sets (creating if absent) `module`'s metatable `__index` to the global
/// table. Mirrors `ll_seeall` in 5.1 `loadlib.c`. Verified against lua5.1.5.
fn ll_seeall(state: &mut LuaState) -> Result<usize, LuaError> {
    state.check_arg_type(1, LuaType::Table)?;
    if !state.get_metatable(1)? {
        state.create_table(0, 1)?;
        state.push_value(-1)?;
        state.set_metatable(1)?;
    }
    state.push_globals()?;
    state.set_field(-2, b"__index")?;
    Ok(0)
}

/// Walk a dotted module name from a table on the stack, creating intermediate
/// tables as needed, leaving the final (sub)table on the stack top. A faithful
/// reduction of `luaL_findtable(L, idx, name, 1)`; returns `Err` on a name
/// conflict (an intermediate path component is a non-table, non-nil value).
fn findtable(state: &mut LuaState, table_idx: i32, name: &[u8]) -> Result<(), LuaError> {
    // Start from a copy of the base table on the stack top.
    state.push_value_at(table_idx)?;
    for part in name.split(|&b| b == b'.') {
        // Stack top holds the current table; fetch current[part].
        let ty = state.get_field(-1, part)?;
        if ty == LuaType::Nil {
            state.pop_n(1); // remove nil
            state.create_table(0, 1)?; // new subtable
            state.push_value(-1)?; // duplicate it
            state.set_field(-3, part)?; // current[part] = subtable
                                        // Stack: ..., current, subtable. Remove the parent, keep subtable.
            state.remove(-2)?;
        } else if ty == LuaType::Table {
            // Stack: ..., current, value. Remove the parent, keep value.
            state.remove(-2)?;
        } else {
            return Err(LuaError::runtime(format_args!(
                "name conflict for module '{}'",
                String::from_utf8_lossy(name)
            )));
        }
    }
    Ok(())
}

/// `module(name [, ...])` — Lua 5.1 only.
///
/// Creates (or reuses) a module table named `name`, registers it in
/// `package.loaded`, initializes its `_NAME`/`_M`/`_PACKAGE` fields, applies any
/// option functions (e.g. `package.seeall`), and sets the calling chunk's
/// environment to the module table. Mirrors `ll_module` in 5.1 `loadlib.c`.
fn ll_module(state: &mut LuaState) -> Result<usize, LuaError> {
    let modname: Vec<u8> = state.check_arg_string(1)?;
    let n_opts = state.top() as i32;

    // Fetch _LOADED[modname]; create the module table if absent.
    state.get_field_registry(b"_LOADED")?;
    let loaded_idx = state.top() as i32;
    state.get_field(loaded_idx, &modname)?;
    if state.type_at(-1) != LuaType::Table {
        state.pop_n(1); // remove non-table result
                        // Find/create a global table named `modname` (supporting dotted names).
        state.push_globals()?;
        let g_idx = state.top() as i32;
        findtable(state, g_idx, &modname)?;
        state.remove(g_idx)?; // drop the globals table copy, keep the module table
        state.push_value(-1)?;
        state.set_field(loaded_idx, &modname)?; // _LOADED[modname] = module
    }

    // Initialize the module if it has no `_NAME` yet.
    let has_name = state.get_field(-1, b"_NAME")? != LuaType::Nil;
    state.pop_n(1);
    if !has_name {
        // module._M = module
        state.push_value(-1)?;
        state.set_field(-2, b"_M")?;
        // module._NAME = modname
        state.push_string(&modname)?;
        state.set_field(-2, b"_NAME")?;
        // module._PACKAGE = full name minus the last dotted component.
        let pkg: &[u8] = match modname.iter().rposition(|&b| b == b'.') {
            Some(dot) => &modname[..=dot],
            None => b"",
        };
        state.push_string(pkg)?;
        state.set_field(-2, b"_PACKAGE")?;
    }

    // Set the caller's environment to the module table (the running closure that
    // invoked `module`, i.e. level 1 relative to this C function).
    let module_tbl = state.value_at(-1);
    crate::base::set_func_env_at_level(state, 1, module_tbl)?;

    // Apply option functions: for each extra arg, call `option(module)`.
    let mut i = 2;
    while i <= n_opts {
        state.push_value_at(i)?; // option function
        state.push_value(-2)?; // module table
        state.call(1, 0)?;
        i += 1;
    }
    Ok(0)
}

/// Open the `package` library and return the `package` table.
///
pub fn luaopen_package(state: &mut LuaState) -> Result<usize, LuaError> {
    createclibstable(state)?;

    // The C `pk_funcs` table also has placeholder entries for "preload",
    // "cpath", "path", "searchers", "loaded" (all NULL); those fields are set
    // explicitly below. Only `loadlib` is unconditional — `package.searchpath`
    // was added in 5.2 (absent on 5.1), so it is registered separately below.
    state.new_lib(&[(
        b"loadlib" as &[u8],
        ll_loadlib as fn(&mut LuaState) -> Result<usize, LuaError>,
    )])?;

    if !matches!(state.global().lua_version, lua_types::LuaVersion::V51) {
        state.push_c_function(ll_searchpath)?;
        state.set_field(-2, b"searchpath")?;
    }

    createsearcherstable(state)?;

    let path_default = lua_path_default(state.global().lua_version);
    setpath(state, b"path", LUA_PATH_VAR, &path_default)?;

    let cpath_default = lua_cpath_default(state.global().lua_version);
    setpath(state, b"cpath", LUA_CPATH_VAR, &cpath_default)?;

    let config = package_config(state.global().lua_version);
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
        &[(
            b"require" as &[u8],
            ll_require as fn(&mut LuaState) -> Result<usize, LuaError>,
        )],
        1,
    )?;
    state.pop_n(1);

    // The deprecated module system: `package.seeall` (a field on the package
    // table) and the `module` global. Present in 5.1 and kept in 5.2.4 via the
    // default-on `LUA_COMPAT_MODULE`; fully removed in 5.3. Verified against
    // lua5.1.5 and lua5.2.4. See specs/followup/5.1-roster-syntax.md §1.
    if matches!(
        state.global().lua_version,
        lua_types::LuaVersion::V51 | lua_types::LuaVersion::V52
    ) {
        // The package table is on top of the stack here.
        state.push_c_function(ll_seeall)?;
        state.set_field(-2, b"seeall")?;
        // `module` is a *global*, not a `package` field.
        state.push_c_function(ll_module)?;
        state.set_global(b"module")?;
    }

    Ok(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lua_types::LuaVersion;

    const ALL: [LuaVersion; 5] =
        [LuaVersion::V51, LuaVersion::V52, LuaVersion::V53, LuaVersion::V54, LuaVersion::V55];

    /// `lua_vdir` is the literal version-directory segment used both in
    /// default package paths and to build the versioned env-var name — one
    /// value per version, never `5.4` for anything else.
    #[test]
    fn vdir_is_version_exact() {
        assert_eq!(lua_vdir(LuaVersion::V51), b"5.1");
        assert_eq!(lua_vdir(LuaVersion::V52), b"5.2");
        assert_eq!(lua_vdir(LuaVersion::V53), b"5.3");
        assert_eq!(lua_vdir(LuaVersion::V54), b"5.4");
        assert_eq!(lua_vdir(LuaVersion::V55), b"5.5");
    }

    #[test]
    fn versuffix_is_version_exact() {
        assert_eq!(lua_versuffix(LuaVersion::V51), b"_5_1");
        assert_eq!(lua_versuffix(LuaVersion::V52), b"_5_2");
        assert_eq!(lua_versuffix(LuaVersion::V53), b"_5_3");
        assert_eq!(lua_versuffix(LuaVersion::V54), b"_5_4");
        assert_eq!(lua_versuffix(LuaVersion::V55), b"_5_5");
    }

    /// 5.1 is the one version with NO versioned environment variables at
    /// all — confirmed against `lua5.1.5`, where `LUA_PATH_5_1` has no
    /// effect on `package.path`. 5.2 onward all have them.
    #[test]
    fn only_5_1_lacks_versioned_env_vars() {
        assert!(!has_versioned_env_vars(LuaVersion::V51));
        for v in [LuaVersion::V52, LuaVersion::V53, LuaVersion::V54, LuaVersion::V55] {
            assert!(has_versioned_env_vars(v), "{v:?}");
        }
    }

    /// Byte-for-byte against the unmodified upstream `make macosx` build of
    /// each version (`specs/oracle/CONTRACT.md`), captured from
    /// `/tmp/lua-refs/bin/lua5.x -e 'print(package.path)'`. The entry SHAPE
    /// (not just the `5.x` segment) differs by era — see `lua_path_default`'s
    /// doc comment.
    #[test]
    #[cfg(not(target_os = "windows"))]
    fn path_default_is_version_exact() {
        assert_eq!(
            lua_path_default(LuaVersion::V51),
            b"./?.lua;/usr/local/share/lua/5.1/?.lua;/usr/local/share/lua/5.1/?/init.lua;\
              /usr/local/lib/lua/5.1/?.lua;/usr/local/lib/lua/5.1/?/init.lua"
                .to_vec()
        );
        assert_eq!(
            lua_path_default(LuaVersion::V52),
            b"/usr/local/share/lua/5.2/?.lua;/usr/local/share/lua/5.2/?/init.lua;\
              /usr/local/lib/lua/5.2/?.lua;/usr/local/lib/lua/5.2/?/init.lua;./?.lua"
                .to_vec()
        );
        for v in [LuaVersion::V53, LuaVersion::V54, LuaVersion::V55] {
            let vdir = lua_vdir(v);
            let mut expected = Vec::new();
            expected.extend_from_slice(b"/usr/local/share/lua/");
            expected.extend_from_slice(vdir);
            expected.extend_from_slice(b"/?.lua;/usr/local/share/lua/");
            expected.extend_from_slice(vdir);
            expected.extend_from_slice(b"/?/init.lua;/usr/local/lib/lua/");
            expected.extend_from_slice(vdir);
            expected.extend_from_slice(b"/?.lua;/usr/local/lib/lua/");
            expected.extend_from_slice(vdir);
            expected.extend_from_slice(b"/?/init.lua;./?.lua;./?/init.lua");
            assert_eq!(lua_path_default(v), expected, "{v:?}");
        }
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn cpath_default_is_version_exact() {
        assert_eq!(
            lua_cpath_default(LuaVersion::V51),
            b"./?.so;/usr/local/lib/lua/5.1/?.so;/usr/local/lib/lua/5.1/loadall.so".to_vec()
        );
        for v in [LuaVersion::V52, LuaVersion::V53, LuaVersion::V54, LuaVersion::V55] {
            let vdir = lua_vdir(v);
            let mut expected = Vec::new();
            expected.extend_from_slice(b"/usr/local/lib/lua/");
            expected.extend_from_slice(vdir);
            expected.extend_from_slice(b"/?.so;/usr/local/lib/lua/");
            expected.extend_from_slice(vdir);
            expected.extend_from_slice(b"/loadall.so;./?.so");
            assert_eq!(lua_cpath_default(v), expected, "{v:?}");
        }
    }

    /// No two versions may collide on their directory segment or defaults —
    /// a regression here would silently point every version at the same
    /// installed-module directory again (the shape of issue #273).
    #[test]
    #[cfg(not(target_os = "windows"))]
    fn every_version_has_a_distinct_path_default() {
        for (i, a) in ALL.iter().enumerate() {
            for b in &ALL[i + 1..] {
                assert_ne!(lua_path_default(*a), lua_path_default(*b), "{a:?} vs {b:?}");
                assert_ne!(lua_cpath_default(*a), lua_cpath_default(*b), "{a:?} vs {b:?}");
            }
        }
    }

    /// Only 5.1/5.2/5.3 use the legacy gsub-based `;;` splice; 5.4/5.5 use
    /// the position-aware one.
    #[test]
    fn double_semicolon_splice_is_legacy_matrix() {
        for v in [LuaVersion::V51, LuaVersion::V52, LuaVersion::V53] {
            assert!(double_semicolon_splice_is_legacy(v), "{v:?}");
        }
        for v in [LuaVersion::V54, LuaVersion::V55] {
            assert!(!double_semicolon_splice_is_legacy(v), "{v:?}");
        }
    }

    /// The legacy splice replaces EVERY non-overlapping `;;` pair,
    /// unconditionally wrapping the default in separators regardless of
    /// where the pair sits — verified against `lua5.1.5`.
    #[test]
    fn legacy_splice_replaces_every_occurrence_both_sides() {
        assert_eq!(
            legacy_double_semicolon_splice(b"/a/?.lua;;;;/b/?.lua", b"DEFAULT"),
            b"/a/?.lua;DEFAULT;;DEFAULT;/b/?.lua".to_vec()
        );
        assert_eq!(
            legacy_double_semicolon_splice(b";;/b/?.lua", b"DEFAULT"),
            b";DEFAULT;/b/?.lua".to_vec()
        );
        assert_eq!(
            legacy_double_semicolon_splice(b"/a/?.lua;;", b"DEFAULT"),
            b"/a/?.lua;DEFAULT;".to_vec()
        );
        assert_eq!(
            legacy_double_semicolon_splice(b"/a/?.lua", b"DEFAULT"),
            b"/a/?.lua".to_vec()
        );
    }

    /// The modern splice replaces only the FIRST `;;` pair and omits a
    /// boundary separator when the pair sits at the very start or end —
    /// verified against `lua5.4.7`.
    #[test]
    fn modern_splice_replaces_first_occurrence_and_omits_boundary_separators() {
        assert_eq!(
            modern_double_semicolon_splice(b"/a/?.lua;;;;/b/?.lua", b"DEFAULT"),
            b"/a/?.lua;DEFAULT;;;/b/?.lua".to_vec()
        );
        assert_eq!(
            modern_double_semicolon_splice(b";;/b/?.lua", b"DEFAULT"),
            b"DEFAULT;/b/?.lua".to_vec()
        );
        assert_eq!(
            modern_double_semicolon_splice(b"/a/?.lua;;", b"DEFAULT"),
            b"/a/?.lua;DEFAULT".to_vec()
        );
        assert_eq!(
            modern_double_semicolon_splice(b"/a/?.lua", b"DEFAULT"),
            b"/a/?.lua".to_vec()
        );
    }
}
