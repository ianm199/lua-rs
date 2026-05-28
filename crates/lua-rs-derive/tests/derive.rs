//! End-to-end checks that `#[derive(LuaUserData)]` produces bindings that work
//! against the live runtime: field reads, field writes, `#[lua(skip)]`, and
//! `#[lua(readonly)]`.

use lua_rs_derive::LuaUserData;
use lua_rs_runtime::Lua;

#[derive(LuaUserData)]
struct Vec2 {
    x: f64,
    y: f64,
    #[lua(readonly)]
    label_len: i64,
    #[lua(skip)]
    _internal: u32,
}

#[test]
fn reads_and_writes_fields() {
    let lua = Lua::new();
    lua.globals()
        .set(
            "v",
            Vec2 {
                x: 3.0,
                y: 4.0,
                label_len: 2,
                _internal: 99,
            },
        )
        .unwrap();

    let sum: f64 = lua.load("return v.x + v.y").eval().unwrap();
    assert_eq!(sum, 7.0);

    lua.load("v.x = 10").exec().unwrap();
    let x: f64 = lua.load("return v.x").eval().unwrap();
    assert_eq!(x, 10.0);
}

#[test]
fn skipped_field_is_invisible() {
    let lua = Lua::new();
    lua.globals()
        .set(
            "v",
            Vec2 {
                x: 1.0,
                y: 2.0,
                label_len: 0,
                _internal: 42,
            },
        )
        .unwrap();
    let is_nil: bool = lua.load("return v._internal == nil").eval().unwrap();
    assert!(is_nil);
}

#[test]
fn readonly_field_reads_but_rejects_writes() {
    let lua = Lua::new();
    lua.globals()
        .set(
            "v",
            Vec2 {
                x: 1.0,
                y: 2.0,
                label_len: 5,
                _internal: 0,
            },
        )
        .unwrap();

    let len: i64 = lua.load("return v.label_len").eval().unwrap();
    assert_eq!(len, 5);

    assert!(lua.load("v.label_len = 9").exec().is_err());
}
