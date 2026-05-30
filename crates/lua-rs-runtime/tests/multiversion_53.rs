//! Preliminary Lua 5.3 backend smoke tests.
//!
//! These pin the version seam end-to-end through the public embedding API:
//! a `LuaVersion::V53` instance must report the 5.3 `_VERSION`, run ordinary
//! 5.3-valid Lua, and exhibit concrete 5.3-vs-5.4 behavioral differences.
//!
//! Scope note: this is the PRELIMINARY 5.3 path (`specs/research/
//! 5.3-upstream-delta.md`). It proves a handful of cheap, real deltas; the
//! full 5.3 surface (RNG, for-loop wrap, coercion, error wording, etc.) is
//! not yet ported. See that spec for the remaining work.

use lua_rs_runtime::{Lua, LuaVersion, Value};

#[test]
fn v53_reports_its_version() {
    let lua = Lua::new_versioned(LuaVersion::V53);
    assert_eq!(lua.version(), LuaVersion::V53);

    let from_global: String = lua.globals().get("_VERSION").unwrap();
    assert_eq!(from_global, "Lua 5.3");

    let from_script: String = lua.load("return _VERSION").eval().unwrap();
    assert_eq!(from_script, "Lua 5.3");
}

#[test]
fn v53_runs_a_trivial_script() {
    let lua = Lua::new_versioned(LuaVersion::V53);
    let sum: i64 = lua
        .load("local t = 0; for i = 1, 10 do t = t + i end; return t")
        .eval()
        .unwrap();
    assert_eq!(sum, 55);
}

/// The headline syntactic delta: `<const>`/`<close>` local attributes are a
/// 5.4 addition. They parse under 5.4 and are a syntax error under 5.3
/// (delta #7).
#[test]
fn const_attribute_parses_under_54_but_errors_under_53() {
    let lua54 = Lua::new_versioned(LuaVersion::V54);
    let v: i64 = lua54
        .load("local x <const> = 42; return x")
        .eval()
        .unwrap();
    assert_eq!(v, 42);

    let lua53 = Lua::new_versioned(LuaVersion::V53);
    let err = lua53.load("local x <const> = 42; return x").exec();
    assert!(
        err.is_err(),
        "5.3 must reject the <const> attribute as a syntax error"
    );
}

/// `<close>` is likewise 5.4-only and must be rejected under 5.3.
#[test]
fn close_attribute_errors_under_53() {
    let lua53 = Lua::new_versioned(LuaVersion::V53);
    let err = lua53.load("local x <close> = nil").exec();
    assert!(err.is_err(), "5.3 must reject the <close> attribute");
}

/// A stdlib roster delta: `coroutine.close` is 5.4-only (delta #9). Under
/// 5.3 it is absent.
#[test]
fn coroutine_close_present_under_54_absent_under_53() {
    let lua54 = Lua::new_versioned(LuaVersion::V54);
    let close54: Value = lua54.load("return coroutine.close").eval().unwrap();
    assert!(
        !matches!(close54, Value::Nil),
        "coroutine.close should exist under 5.4"
    );

    let lua53 = Lua::new_versioned(LuaVersion::V53);
    let is_nil: bool = lua53
        .load("return coroutine.close == nil")
        .eval()
        .unwrap();
    assert!(is_nil, "coroutine.close must be nil under 5.3");
}

/// A second stdlib roster delta: the `bit32` library is default-on in 5.3
/// (delta #11) and removed in 5.4. The preliminary 5.3 backend ships a
/// minimal `bit32` whose operations mask to 32 bits.
#[test]
fn bit32_present_under_53_absent_under_54() {
    let lua54 = Lua::new_versioned(LuaVersion::V54);
    let absent: bool = lua54.load("return bit32 == nil").eval().unwrap();
    assert!(absent, "bit32 must be absent under 5.4");

    let lua53 = Lua::new_versioned(LuaVersion::V53);
    let present: bool = lua53.load("return bit32 ~= nil").eval().unwrap();
    assert!(present, "bit32 must be present under 5.3");

    let banded: i64 = lua53.load("return bit32.band(0xF0, 0x3C)").eval().unwrap();
    assert_eq!(banded, 0x30);

    // 32-bit masking: bnot of 0 is 0xFFFFFFFF, not -1.
    let bnot0: i64 = lua53.load("return bit32.bnot(0)").eval().unwrap();
    assert_eq!(bnot0, 0xFFFF_FFFF);
}
