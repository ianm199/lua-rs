//! Preliminary Lua 5.5 backend smoke tests.
//!
//! These pin the V55 version seam end-to-end through the public embedding API:
//! a `LuaVersion::V55` instance must report the 5.5 `_VERSION`, run ordinary
//! Lua, recognize the new `global` declaration statement, and ship the new
//! `table.create` stdlib entry.
//!
//! Scope note: this is the PRELIMINARY 5.5 path (`specs/research/
//! 5.5-upstream-delta.md`). It proves the wire-up plus first groundwork on the
//! global-declaration feature — the `global` statement PARSES (and is voided to
//! a no-op for now). The full stateful scope model (implicit `global *`, voided
//! by an explicit decl, undeclared-name compile error, `<const>` globals,
//! `LUA_COMPAT_GLOBAL`) and the 5.5 bytecode shape (ivABC, GETVARG, ERRNNIL,
//! SHRI/SHLI swap) are NOT yet ported. See that spec for the remaining work.

use lua_rs_runtime::{Lua, LuaVersion, Value};

#[test]
fn v55_reports_its_version() {
    let lua = Lua::new_versioned(LuaVersion::V55);
    assert_eq!(lua.version(), LuaVersion::V55);

    let from_global: String = lua.globals().get("_VERSION").unwrap();
    assert_eq!(from_global, "Lua 5.5");

    let from_script: String = lua.load("return _VERSION").eval().unwrap();
    assert_eq!(from_script, "Lua 5.5");
}

#[test]
fn v55_runs_a_trivial_script() {
    let lua = Lua::new_versioned(LuaVersion::V55);
    let sum: i64 = lua
        .load("local t = 0; for i = 1, 10 do t = t + i end; return t")
        .eval()
        .unwrap();
    assert_eq!(sum, 55);
}

/// The headline 5.5 syntactic delta: `global` is a reserved word and the
/// `global` declaration statement is recognized. GROUNDWORK: the statement
/// currently parses and is a no-op (it does not yet enforce the stateful scope
/// model), so a chunk containing it must compile and run without error.
#[test]
fn v55_global_decl_statement_parses() {
    let lua = Lua::new_versioned(LuaVersion::V55);

    let ok: i64 = lua
        .load("global x; x = 41; return x + 1")
        .eval()
        .unwrap();
    assert_eq!(ok, 42);

    // Multi-name list with a <const> attribute on one name, plus initializer.
    lua.load("global a, b <const>, c = 1, 2, 3")
        .exec()
        .expect("5.5 global name-list with attribute should parse");

    // The collective `global *` form (re-enables global-by-default).
    lua.load("global *").exec().expect("5.5 `global *` should parse");

    // `global <const> *` collective-with-attribute form.
    lua.load("global <const> *")
        .exec()
        .expect("5.5 `global <const> *` should parse");
}

/// `global` is a reserved word ONLY on the V55 path. Under the default 5.4
/// backend the bytes `global` must remain a perfectly valid identifier — this
/// guards the IRON RULE that 5.4 does not regress.
#[test]
fn global_is_an_ordinary_identifier_under_54() {
    let lua54 = Lua::new_versioned(LuaVersion::V54);
    let v: i64 = lua54
        .load("local global = 7; return global * 6")
        .eval()
        .unwrap();
    assert_eq!(v, 42);
}

/// Stdlib roster delta: `table.create` is a 5.5 addition and is absent in 5.4.
#[test]
fn table_create_present_under_55_absent_under_54() {
    let lua54 = Lua::new_versioned(LuaVersion::V54);
    let absent: bool = lua54.load("return table.create == nil").eval().unwrap();
    assert!(absent, "table.create must be absent under 5.4");

    let lua55 = Lua::new_versioned(LuaVersion::V55);
    let present: Value = lua55.load("return table.create").eval().unwrap();
    assert!(
        !matches!(present, Value::Nil),
        "table.create must exist under 5.5"
    );

    // It returns an empty, usable table presized for nseq array slots.
    let len: i64 = lua55
        .load("local t = table.create(8); t[1] = 'a'; t[2] = 'b'; return #t")
        .eval()
        .unwrap();
    assert_eq!(len, 2);

    // The preallocated table is observably empty on creation.
    let empty: i64 = lua55
        .load("return #table.create(4, 2)")
        .eval()
        .unwrap();
    assert_eq!(empty, 0);
}
