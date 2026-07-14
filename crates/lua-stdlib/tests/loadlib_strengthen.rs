//! Behavioral net for the **deterministic, pure-Lua** surface of the `package`
//! library (`src/loadlib.rs`).
//!
//! `loadlib` is split into two regimes:
//!
//! * **Platform / FFI (NOT pinnable here):** actual C dynamic loading
//!   (`dlopen`/`dlsym`) and the embedder-installed file probe. A real
//!   `package.loadlib`/`require` of a `.so` needs a built shared object and the
//!   host loader; its failure message is the platform dyld string (the macOS
//!   path list), and the `"open"`/`"absent"` tag depends on which backend is
//!   installed. Those are the genuine-`unsafe` FFI bridge and are deliberately
//!   absent here (documented as honest-negatives in
//!   `crates/lua-stdlib/GRADUATED.md`, the analogue of math's platform-`rand()`).
//! * **Pure package logic (what this file pins):** the `package.config` string,
//!   `require`'s `package.loaded` caching + identity, the preload searcher, the
//!   missing-module error assembly (the four-searcher trace), `package.searchpath`
//!   string resolution, the `package.loaders`(5.1)/`package.searchers`(5.2+)
//!   naming, the `module`/`package.seeall` 5.1 roster, and the `nil`-vs-`false`
//!   fail value. All of this runs with NO shared object — the searchers report
//!   "not found" because the file probe never resolves — so it is byte-pinnable
//!   against the version-suffixed reference binaries (`/tmp/lua-refs/bin/lua5.x`,
//!   captured 2026-06-14).
//!
//! Pinning these seams across all five versions caught **seven** real divergences
//! (our impl applied a modern-version rule to ALL versions, or carried a wrong
//! `luaL_pushfail` translation); the relevant tests FAILED at baseline and PASS
//! after the fix — the Phase-2 net-strengthening discipline (cf. `table.remove`,
//! the string matcher, the `os` date/time seams).
//!
//! `omnilua` is a dev-dependency (it depends on `lua-stdlib`, so it can only be
//! a dev-dep — see `Cargo.toml`).

use omnilua::{HostHooks, Lua, LuaVersion, Value};

const ALL: [LuaVersion; 5] = [
    LuaVersion::V51,
    LuaVersion::V52,
    LuaVersion::V53,
    LuaVersion::V54,
    LuaVersion::V55,
];

/// Evaluate `code` under `version`, returning a string return value as bytes.
fn eval_str(version: LuaVersion, code: &str) -> Vec<u8> {
    let lua = Lua::new_versioned(version);
    match lua.load(code).eval::<Value>() {
        Ok(Value::String(s)) => s
            .as_bytes()
            .unwrap_or_else(|e| panic!("string bytes under {version:?} for `{code}`: {e:?}"))
            .to_vec(),
        Ok(other) => panic!("`{code}` under {version:?} returned {other:?}, expected a string"),
        Err(e) => panic!("eval of `{code}` failed under {version:?}: {e:?}"),
    }
}

/// Evaluate `code` under `version`, returning an integer return value.
fn eval_int(version: LuaVersion, code: &str) -> i64 {
    let lua = Lua::new_versioned(version);
    match lua.load(code).eval::<Value>() {
        Ok(Value::Integer(i)) => i,
        Ok(Value::Number(n)) if n.fract() == 0.0 => n as i64,
        Ok(other) => panic!("`{code}` under {version:?} returned {other:?}, expected an integer"),
        Err(e) => panic!("eval of `{code}` failed under {version:?}: {e:?}"),
    }
}

/// Evaluate `code` under `version`, returning the raw [`Value`] so a test can
/// distinguish `nil` from `false` (the `luaL_pushfail` seam).
fn eval_value(version: LuaVersion, code: &str) -> Value {
    let lua = Lua::new_versioned(version);
    lua.load(code)
        .eval::<Value>()
        .unwrap_or_else(|e| panic!("eval of `{code}` failed under {version:?}: {e:?}"))
}

/// Evaluate `code`, expecting a boolean-`true` return (an invariant probe).
fn assert_true(version: LuaVersion, code: &str) {
    match eval_value(version, code) {
        Value::Boolean(true) => {}
        other => panic!("`{code}` under {version:?} returned {other:?}, expected true"),
    }
}

// ── package.config — the platform-separator string (a version seam) ───────────

/// `package.config` is five lines: dir-sep, path-sep, the `?` mark, the exec-dir
/// `!`, and the ignore mark `-`. **5.1's string ends at `-` (9 bytes); the
/// trailing newline after the ignore mark was added in 5.2 (10 bytes).** This
/// pin caught our impl emitting the 10-byte (trailing-newline) form on 5.1.
#[test]
fn config_string_is_version_exact() {
    // 5.1: "/\n;\n?\n!\n-" — no trailing newline.
    assert_eq!(eval_str(LuaVersion::V51, "return package.config"), b"/\n;\n?\n!\n-");
    assert_eq!(eval_int(LuaVersion::V51, "return #package.config"), 9);
    // 5.2+: "/\n;\n?\n!\n-\n" — trailing newline present.
    for v in [LuaVersion::V52, LuaVersion::V53, LuaVersion::V54, LuaVersion::V55] {
        assert_eq!(eval_str(v, "return package.config"), b"/\n;\n?\n!\n-\n", "{v:?}");
        assert_eq!(eval_int(v, "return #package.config"), 10, "{v:?}");
    }
    // The first line is always the directory separator (POSIX `/` on this host).
    for v in ALL {
        assert_eq!(eval_str(v, "return package.config:sub(1,1)"), b"/", "{v:?}");
    }
}

// ── require + package.loaded caching (version-independent) ─────────────────────

/// A `package.preload`-registered loader is found by `require`, its return value
/// becomes the module, and it is cached in `package.loaded` by identity. The
/// `preload` entry itself survives the load.
#[test]
fn require_preload_returns_module_and_caches() {
    for v in ALL {
        // The returned module IS the loader's return value, and `package.loaded`
        // holds the same object (identity), and `package.preload` is untouched.
        let probe = "\
            package.preload['mymod'] = function() return {answer = 42} end \
            local m = require('mymod') \
            return m.answer == 42 \
              and m == package.loaded['mymod'] \
              and type(package.preload['mymod']) == 'function'";
        assert_true(v, probe);
    }
}

/// The loader runs exactly once: a second `require` returns the cached object
/// without re-invoking the loader.
#[test]
fn require_caches_loader_runs_once() {
    for v in ALL {
        let probe = "\
            local n = 0 \
            package.preload['m'] = function() n = n + 1; return {id = n} end \
            local a = require('m') \
            local b = require('m') \
            return a == b and a.id == 1 and n == 1";
        assert_true(v, probe);
    }
}

/// A pre-seeded `package.loaded` entry short-circuits the searchers entirely:
/// `require` returns the cached value without running any searcher.
#[test]
fn require_preseeded_loaded_short_circuits_searchers() {
    for v in ALL {
        assert_eq!(
            eval_str(v, "package.loaded['pre'] = 'CACHED'; return require('pre')"),
            b"CACHED",
            "{v:?}"
        );
    }
}

/// `require`'s SECOND return value is the loader data — but only on **5.4/5.5**.
/// `ll_require`'s `return 2` (module + loader data) was added in 5.4; 5.1/5.2/5.3
/// `return 1` (module only), so the 2nd value is `nil`. This pin caught our impl
/// returning the preload searcher's `:preload:` tag on every version.
#[test]
fn require_second_value_is_loader_data_only_5_4_plus() {
    let probe = "\
        package.preload['m'] = function() return 'MOD' end \
        local a, b = require('m') \
        return b";
    // 5.1/5.2/5.3: only the module is returned; the 2nd value is nil.
    for v in [LuaVersion::V51, LuaVersion::V52, LuaVersion::V53] {
        assert!(
            matches!(eval_value(v, probe), Value::Nil),
            "{v:?}: require's 2nd value must be nil pre-5.4, got {:?}",
            eval_value(v, probe)
        );
    }
    // 5.4/5.5: the preload searcher's loader data (`:preload:`) is the 2nd value.
    for v in [LuaVersion::V54, LuaVersion::V55] {
        assert_eq!(eval_str(v, probe), b":preload:", "{v:?}");
    }
}

/// The preload searcher (like every searcher) passes the module name to the
/// loader as the FIRST argument; from **5.2** it also passes the loader data as
/// a SECOND argument. On 5.1 the loader sees ONE argument; on 5.2+ it sees TWO.
/// `ll_require`'s `lua_call(L, 1, 1)` became `lua_call(L, 2, 1)` in 5.2. This
/// pin caught our impl passing two args on 5.1.
#[test]
fn preload_loader_argument_count_is_one_on_5_1_two_after() {
    let probe = "\
        package.preload['m'] = function(...) return select('#', ...) end \
        return (require('m'))";
    assert_eq!(eval_int(LuaVersion::V51, probe), 1, "5.1 passes the name only");
    for v in [LuaVersion::V52, LuaVersion::V53, LuaVersion::V54, LuaVersion::V55] {
        assert_eq!(eval_int(v, probe), 2, "{v:?} passes name + loader data");
        // The first argument is the module name on every version.
        assert_eq!(
            eval_str(
                v,
                "package.preload['m'] = function(n) return n end; return (require('m'))"
            ),
            b"m",
            "{v:?}"
        );
    }
    assert_eq!(
        eval_str(
            LuaVersion::V51,
            "package.preload['m'] = function(n) return n end; return (require('m'))"
        ),
        b"m"
    );
}

// ── The missing-module error: the four-searcher not-found trace ───────────────

/// `require` of an unfindable module raises a runtime error of the shape
/// `module 'NAME' not found:` followed by one tab-indented line per searcher
/// that failed: the preload `no field …` line, then the lua-file, C-lib, and
/// C-root `no file '…'` lines (the path templates with `?` filled in and `.`
/// mapped to the directory separator). With explicit `./?.lua` / `./?.so`
/// paths this trace is byte-identical on every version. This pin caught our
/// C-root searcher truncating its `no file '…'` line to the module root.
#[test]
fn require_missing_module_error_is_full_four_searcher_trace() {
    let setup_and_require = "\
        package.path = './?.lua'; package.cpath = './?.so' \
        local ok, err = pcall(require, 'no.such.mod') \
        assert(not ok) \
        return err";
    let expected: &[u8] = b"module 'no.such.mod' not found:\n\
        \tno field package.preload['no.such.mod']\n\
        \tno file './no/such/mod.lua'\n\
        \tno file './no/such/mod.so'\n\
        \tno file './no.so'";
    for v in ALL {
        assert_eq!(eval_str(v, setup_and_require), expected, "{v:?}");
    }
}

/// The C-root searcher only contributes a line when the module name contains a
/// dot (it searches `cpath` for the root component before the first `.`). For a
/// dotless name it returns no value, so the trace has exactly three lines.
#[test]
fn require_missing_dotless_module_trace_has_no_croot_line() {
    let probe = "\
        package.path = './?.lua'; package.cpath = './?.so' \
        local ok, err = pcall(require, 'solo') \
        assert(not ok) \
        return err";
    let expected: &[u8] = b"module 'solo' not found:\n\
        \tno field package.preload['solo']\n\
        \tno file './solo.lua'\n\
        \tno file './solo.so'";
    for v in ALL {
        assert_eq!(eval_str(v, probe), expected, "{v:?}");
    }
}

/// The preload searcher's not-found line is exactly
/// `no field package.preload['NAME']`. With an empty path/cpath only the preload
/// line appears on 5.1/5.2/5.3 (their lua/C searchers produce a `no file ''`
/// line too on 5.4/5.5 — that delta is incidental to the empty path and not
/// pinned here; the explicit-path trace above is the stable one).
#[test]
fn require_preload_not_found_line_wording() {
    for v in ALL {
        let probe = "\
            local ok, err = pcall(require, 'absent_xyz') \
            assert(not ok) \
            return err:match(\"no field package%.preload%['absent_xyz'%]\") ~= nil";
        assert_true(v, probe);
        // The leading `module '…' not found:` header is present on every version.
        let probe2 = "\
            local ok, err = pcall(require, 'absent_xyz') \
            return err:sub(1, 30) == \"module 'absent_xyz' not found:\"";
        assert_true(v, probe2);
    }
}

// ── package.searchpath — string resolution (5.2+, absent on 5.1) ──────────────

/// `package.searchpath` was added in **5.2**; it is absent (nil) on 5.1. This
/// pin caught our impl exposing it on 5.1.
#[test]
fn searchpath_absent_on_5_1_present_after() {
    assert_eq!(eval_str(LuaVersion::V51, "return type(package.searchpath)"), b"nil");
    for v in [LuaVersion::V52, LuaVersion::V53, LuaVersion::V54, LuaVersion::V55] {
        assert_eq!(eval_str(v, "return type(package.searchpath)"), b"function", "{v:?}");
    }
}

/// On failure, `package.searchpath` returns `luaL_pushfail` (which is
/// `lua_pushnil` on every version, 5.4 included) followed by the error message —
/// i.e. **`nil`, message**, NOT `false`. This pin caught our impl returning
/// `false` (a wrong `luaL_pushfail` translation, shared with `package.loadlib`).
#[test]
fn searchpath_failure_returns_nil_not_false() {
    for v in [LuaVersion::V52, LuaVersion::V53, LuaVersion::V54, LuaVersion::V55] {
        assert!(
            matches!(eval_value(v, "return (package.searchpath('x', 'noq'))"), Value::Nil),
            "{v:?}: searchpath fail value must be nil, got {:?}",
            eval_value(v, "return (package.searchpath('x', 'noq'))")
        );
    }
    // The 2nd return value is the not-found message — bare on 5.4/5.5, with the
    // legacy leading `\n\t` separator on 5.2/5.3 (see the dedicated test below).
    let probe = "local _, err = package.searchpath('x', 'noq'); return err";
    for v in [LuaVersion::V52, LuaVersion::V53] {
        assert_eq!(eval_str(v, probe), b"\n\tno file 'noq'", "{v:?}");
    }
    for v in [LuaVersion::V54, LuaVersion::V55] {
        assert_eq!(eval_str(v, probe), b"no file 'noq'", "{v:?}");
    }
}

/// `package.searchpath`'s not-found message lists one `no file '…'` line per
/// template (`?` → name, with `.` → `sep`-replacement first). On **5.2/5.3** the
/// message carries a leading `\n\t` separator; **5.4/5.5** dropped it. This pin
/// caught our impl omitting the 5.2/5.3 leading separator.
#[test]
fn searchpath_error_message_leading_separator_is_5_2_5_3_only() {
    let probe = "\
        local _, err = package.searchpath('a.b.c', './?.lua;/x/?.lua') \
        return err";
    // 5.2/5.3: a leading `\n\t` precedes the first `no file` line.
    for v in [LuaVersion::V52, LuaVersion::V53] {
        assert_eq!(
            eval_str(v, probe),
            b"\n\tno file './a/b/c.lua'\n\tno file '/x/a/b/c.lua'",
            "{v:?}"
        );
    }
    // 5.4/5.5: no leading separator.
    for v in [LuaVersion::V54, LuaVersion::V55] {
        assert_eq!(
            eval_str(v, probe),
            b"no file './a/b/c.lua'\n\tno file '/x/a/b/c.lua'",
            "{v:?}"
        );
    }
}

/// `package.searchpath`'s default separator is `.` and default rep is the
/// directory separator: dotted names map to nested paths. An explicit empty
/// `sep` disables the dot→dirsep mapping (the name is used verbatim).
#[test]
fn searchpath_separator_and_rep_logic() {
    for v in [LuaVersion::V52, LuaVersion::V53, LuaVersion::V54, LuaVersion::V55] {
        // Default sep `.` → dirsep `/`: `a.b` searched as `./a/b.lua`.
        let probe = "\
            local _, err = package.searchpath('a.b', './?.lua') \
            return err:match(\"no file './a/b%.lua'\") ~= nil";
        assert_true(v, probe);
        // Explicit empty sep: no mapping, `a.b` used verbatim → `./a.b.lua`.
        let probe2 = "\
            local _, err = package.searchpath('a.b', './?.lua', '') \
            return err:match(\"no file './a%.b%.lua'\") ~= nil";
        assert_true(v, probe2);
        // Explicit sep `/` with rep `_`: `a/b` → `a_b` → `./a_b.lua`.
        let probe3 = "\
            local _, err = package.searchpath('a/b', './?.lua', '/', '_') \
            return err:match(\"no file './a_b%.lua'\") ~= nil";
        assert_true(v, probe3);
    }
}

// ── package.loaders / package.searchers naming (the 5.1→5.2 rename) ───────────

/// The searcher list was `package.loaders` in 5.1; 5.2 renamed it to
/// `package.searchers` and kept `loaders` as a same-object alias; 5.3+ dropped
/// `loaders`. (Already covered in `multiversion_oracle`; pinned here too so the
/// strengthened net stands alone, and with the alias-identity check the oracle
/// lacks.)
#[test]
fn searcher_list_name_matrix() {
    // 5.1: loaders is a table, searchers is nil.
    assert_eq!(eval_str(LuaVersion::V51, "return type(package.loaders)"), b"table");
    assert_eq!(eval_str(LuaVersion::V51, "return type(package.searchers)"), b"nil");
    // 5.2: both present, and they are the SAME object (alias).
    assert_eq!(eval_str(LuaVersion::V52, "return type(package.loaders)"), b"table");
    assert_eq!(eval_str(LuaVersion::V52, "return type(package.searchers)"), b"table");
    assert_true(LuaVersion::V52, "return package.loaders == package.searchers");
    // 5.3+: searchers only, loaders dropped.
    for v in [LuaVersion::V53, LuaVersion::V54, LuaVersion::V55] {
        assert_eq!(eval_str(v, "return type(package.searchers)"), b"table", "{v:?}");
        assert_eq!(eval_str(v, "return type(package.loaders)"), b"nil", "{v:?}");
    }
    // The preload searcher is always the FIRST entry in the list.
    for v in ALL {
        let list = if matches!(v, LuaVersion::V51) { "loaders" } else { "searchers" };
        let probe = format!("return type(package.{list}[1])");
        assert_eq!(eval_str(v, &probe), b"function", "{v:?}");
    }
}

// ── module / package.seeall — the 5.1 deprecated module system ────────────────

/// `module` (global) and `package.seeall` exist only in 5.1 and the
/// compat-on 5.2 build; both are removed in 5.3+. (Already in the oracle;
/// pinned here so the net stands alone.)
#[test]
fn module_and_seeall_roster_matrix() {
    for v in [LuaVersion::V51, LuaVersion::V52] {
        assert_eq!(eval_str(v, "return type(module)"), b"function", "{v:?}");
        assert_eq!(eval_str(v, "return type(package.seeall)"), b"function", "{v:?}");
    }
    for v in [LuaVersion::V53, LuaVersion::V54, LuaVersion::V55] {
        assert_eq!(eval_str(v, "return type(module)"), b"nil", "{v:?}");
        assert_eq!(eval_str(v, "return type(package.seeall)"), b"nil", "{v:?}");
    }
}

/// `module('name', package.seeall)` on 5.1 creates a module table, sets the
/// caller's `_NAME`/`_M`, registers it in `package.loaded`, and points its
/// metatable `__index` at `_G` so globals remain visible.
#[test]
fn module_seeall_creates_and_registers_module() {
    let probe = "\
        module('foo', package.seeall) \
        return _NAME .. ',' .. tostring(_M == foo) .. ',' \
            .. tostring(package.loaded.foo == foo) .. ',' .. tostring(print ~= nil)";
    assert_eq!(eval_str(LuaVersion::V51, probe), b"foo,true,true,true");
}

// ── package.path/cpath — version-derived defaults + env precedence (#273) ────
//
// Before the #273 fix, `LUA_VERSUFFIX` and the default path/cpath were
// hardcoded to 5.4's values for EVERY version, so a 5.1 instance reported
// 5.4's `/usr/local/.../5.4/` directories and consulted `LUA_PATH_5_4`
// instead of its own versioned name (or, on 5.1, a versioned name real 5.1
// never consults at all). These tests pin the fix at the public `Lua` API
// boundary; the pure per-version derivation functions (`lua_vdir`,
// `lua_path_default`, the `;;`-splice algorithms, ...) have their own
// exhaustive unit tests in `src/loadlib.rs`.
//
// Every hook below installs a deterministic FAKE environment via
// `HostHooks::env` — `bootstrap_state` installs host hooks before opening
// the standard libraries, so `luaopen_package`'s one-time `setpath` calls see
// only what the hook returns, never the test process's real environment.
// This keeps the tests hermetic and safely parallel (no `std::env::set_var`
// anywhere).

/// Force the compiled-in default: no env var is ever visible.
fn no_env_hook(_name: &[u8]) -> Option<Vec<u8>> {
    None
}

fn env_versioned_wins_hook(name: &[u8]) -> Option<Vec<u8>> {
    match name {
        b"LUA_PATH_5_2" | b"LUA_PATH_5_3" | b"LUA_PATH_5_4" | b"LUA_PATH_5_5" => {
            Some(b"/versioned/?.lua".to_vec())
        }
        b"LUA_PATH" => Some(b"/unversioned/?.lua".to_vec()),
        _ => None,
    }
}

fn env_5_1_hook(name: &[u8]) -> Option<Vec<u8>> {
    match name {
        b"LUA_PATH_5_1" => Some(b"/should-be-ignored/?.lua".to_vec()),
        b"LUA_PATH" => Some(b"/unversioned/?.lua".to_vec()),
        _ => None,
    }
}

/// Evaluate `code` under `version` with `hooks` installed, returning a string
/// return value as bytes.
fn eval_str_with_hooks(version: LuaVersion, hooks: HostHooks, code: &str) -> Vec<u8> {
    let lua = Lua::with_hooks_versioned(hooks, version).expect("Lua runtime should initialize");
    match lua.load(code).eval::<Value>() {
        Ok(Value::String(s)) => s
            .as_bytes()
            .unwrap_or_else(|e| panic!("string bytes under {version:?} for `{code}`: {e:?}"))
            .to_vec(),
        Ok(other) => panic!("`{code}` under {version:?} returned {other:?}, expected a string"),
        Err(e) => panic!("eval of `{code}` failed under {version:?}: {e:?}"),
    }
}

/// With no env var visible, `package.path` is the compiled-in default — a
/// DIFFERENT `/usr/local/.../<vdir>/` directory per version, not 5.4's for
/// everyone. Captured from `/tmp/lua-refs/bin/lua5.x` (unmodified upstream
/// `make macosx` builds — `specs/oracle/CONTRACT.md`). The entry SHAPE
/// (not just the version segment) differs by era: 5.1 puts `./?.lua` first
/// with no trailing `./?/init.lua`; 5.2 puts `./?.lua` last with no
/// `./?/init.lua` at all; 5.3+ put both `./?.lua` and `./?/init.lua` last.
#[test]
#[cfg(not(target_os = "windows"))]
fn default_package_path_is_version_exact() {
    assert_eq!(
        eval_str_with_hooks(LuaVersion::V51, HostHooks::new().env(no_env_hook), "return package.path"),
        b"./?.lua;/usr/local/share/lua/5.1/?.lua;/usr/local/share/lua/5.1/?/init.lua;\
          /usr/local/lib/lua/5.1/?.lua;/usr/local/lib/lua/5.1/?/init.lua"
            .to_vec()
    );
    assert_eq!(
        eval_str_with_hooks(LuaVersion::V52, HostHooks::new().env(no_env_hook), "return package.path"),
        b"/usr/local/share/lua/5.2/?.lua;/usr/local/share/lua/5.2/?/init.lua;\
          /usr/local/lib/lua/5.2/?.lua;/usr/local/lib/lua/5.2/?/init.lua;./?.lua"
            .to_vec()
    );
    for (v, vdir) in [(LuaVersion::V53, "5.3"), (LuaVersion::V54, "5.4"), (LuaVersion::V55, "5.5")] {
        let expected = format!(
            "/usr/local/share/lua/{vdir}/?.lua;/usr/local/share/lua/{vdir}/?/init.lua;\
             /usr/local/lib/lua/{vdir}/?.lua;/usr/local/lib/lua/{vdir}/?/init.lua;\
             ./?.lua;./?/init.lua"
        );
        assert_eq!(
            eval_str_with_hooks(v, HostHooks::new().env(no_env_hook), "return package.path"),
            expected.into_bytes(),
            "{v:?}"
        );
    }
}

/// Same as `default_package_path_is_version_exact` for `package.cpath`: 5.1
/// puts `./?.so` first (no trailing `./` alternative); 5.2+ share one shape.
#[test]
#[cfg(not(target_os = "windows"))]
fn default_package_cpath_is_version_exact() {
    assert_eq!(
        eval_str_with_hooks(LuaVersion::V51, HostHooks::new().env(no_env_hook), "return package.cpath"),
        b"./?.so;/usr/local/lib/lua/5.1/?.so;/usr/local/lib/lua/5.1/loadall.so".to_vec()
    );
    for (v, vdir) in [
        (LuaVersion::V52, "5.2"),
        (LuaVersion::V53, "5.3"),
        (LuaVersion::V54, "5.4"),
        (LuaVersion::V55, "5.5"),
    ] {
        let expected =
            format!("/usr/local/lib/lua/{vdir}/?.so;/usr/local/lib/lua/{vdir}/loadall.so;./?.so");
        assert_eq!(
            eval_str_with_hooks(v, HostHooks::new().env(no_env_hook), "return package.cpath"),
            expected.into_bytes(),
            "{v:?}"
        );
    }
}

/// From **5.2** onward, a versioned `LUA_PATH_5_x` wins over the unversioned
/// `LUA_PATH` when both are set — verified against `lua5.2.4`..`lua5.5.0`.
#[test]
fn versioned_env_var_wins_over_unversioned_from_5_2() {
    for v in [LuaVersion::V52, LuaVersion::V53, LuaVersion::V54, LuaVersion::V55] {
        assert_eq!(
            eval_str_with_hooks(
                v,
                HostHooks::new().env(env_versioned_wins_hook),
                "return package.path"
            ),
            b"/versioned/?.lua".to_vec(),
            "{v:?}"
        );
    }
}

/// **5.1** has no versioned environment variables at all: setting
/// `LUA_PATH_5_1` has zero effect on `package.path`, and the unversioned
/// `LUA_PATH` applies directly — verified against `lua5.1.5`.
#[test]
fn v5_1_has_no_versioned_env_vars() {
    assert_eq!(
        eval_str_with_hooks(LuaVersion::V51, HostHooks::new().env(env_5_1_hook), "return package.path"),
        b"/unversioned/?.lua".to_vec()
    );
}
