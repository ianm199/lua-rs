//! Visibility-as-scriptability-boundary model for `#[derive(LuaUserData)]`
//! (lua-rs#56, lua-rs#57). Public named fields auto-expose; private fields and
//! tuple/unit structs are opaque userdata handles; `#[lua(field)]` force-exposes
//! a private field.

use lua_rs_derive::{lua_methods, LuaUserData};
use lua_rs_runtime::Lua;

// ---------------------------------------------------------------------------
// Case 1 (issue #56): a struct with a private, non-`Clone` field derives
// cleanly and the field is invisible to Lua. If the derive still cloned every
// field this would fail to compile, because `Engine` is deliberately not
// `Clone`.
// ---------------------------------------------------------------------------

/// Not `Clone`, not marshalable to Lua. Stands in for `bevy::App`.
struct Engine {
    _frame: u64,
}

#[derive(LuaUserData)]
#[lua(methods)]
struct ScriptApp {
    engine: Engine,
    pub tick: i64,
}

#[lua_methods]
impl ScriptApp {
    pub fn step(&mut self) -> i64 {
        self.engine._frame += 1;
        self.tick += 1;
        self.tick
    }
}

#[test]
fn private_non_clone_field_is_opaque_but_methods_work() {
    let lua = Lua::new();
    lua.globals()
        .set(
            "app",
            ScriptApp {
                engine: Engine { _frame: 0 },
                tick: 0,
            },
        )
        .unwrap();

    // The private `engine` field is invisible to Lua.
    let engine_is_nil: bool = lua.load("return app.engine == nil").eval().unwrap();
    assert!(engine_is_nil, "private field must not be exposed");

    // The public `tick` field is exposed, and the method drives the private one.
    let after: i64 = lua.load("app:step(); return app:step()").eval().unwrap();
    assert_eq!(after, 2);
    let tick: i64 = lua.load("return app.tick").eval().unwrap();
    assert_eq!(tick, 2);
}

// ---------------------------------------------------------------------------
// Case 2 (issue #57): a tuple/newtype struct derives, exposes no fields, and
// works as an opaque handle that carries methods.
// ---------------------------------------------------------------------------

#[derive(LuaUserData)]
#[lua(methods)]
#[repr(transparent)]
struct Counter(i64);

#[lua_methods]
impl Counter {
    pub fn bump(&mut self, by: i64) -> i64 {
        self.0 += by;
        self.0
    }
    pub fn get(&self) -> i64 {
        self.0
    }
}

#[test]
fn tuple_newtype_is_opaque_handle_with_methods() {
    let lua = Lua::new();
    lua.globals().set("c", Counter(10)).unwrap();

    let got: i64 = lua.load("c:bump(5); return c:get()").eval().unwrap();
    assert_eq!(got, 15);

    // No positional field access leaks through.
    let pos_is_nil: bool = lua
        .load("return c[0] == nil and c[1] == nil")
        .eval()
        .unwrap();
    assert!(pos_is_nil, "tuple struct must expose no positional fields");
}

// ---------------------------------------------------------------------------
// Case 3: a public field still auto-exposes with the existing
// `Clone + IntoLua + FromLua` round-trip behavior (read and write).
// ---------------------------------------------------------------------------

#[derive(LuaUserData)]
struct Record {
    pub x: f64,
    pub name: String,
}

#[test]
fn public_fields_still_auto_expose_and_round_trip() {
    let lua = Lua::new();
    lua.globals()
        .set(
            "r",
            Record {
                x: 1.5,
                name: "init".to_string(),
            },
        )
        .unwrap();

    // Read.
    let x: f64 = lua.load("return r.x").eval().unwrap();
    assert_eq!(x, 1.5);
    let name: String = lua.load("return r.name").eval().unwrap();
    assert_eq!(name, "init");

    // Write (exercises the FromLua setter path).
    lua.load("r.x = 9.0; r.name = 'updated'").exec().unwrap();
    let x: f64 = lua.load("return r.x").eval().unwrap();
    assert_eq!(x, 9.0);
    let name: String = lua.load("return r.name").eval().unwrap();
    assert_eq!(name, "updated");
}

// ---------------------------------------------------------------------------
// Escape hatch: `#[lua(field)]` force-exposes a private field for callers who
// relied on private-field exposure under the old expose-everything default.
// ---------------------------------------------------------------------------

#[derive(LuaUserData)]
struct Legacy {
    #[lua(field)]
    hidden: i64,
    pub shown: i64,
}

#[test]
fn lua_field_force_exposes_a_private_field() {
    let lua = Lua::new();
    lua.globals()
        .set("o", Legacy { hidden: 7, shown: 3 })
        .unwrap();

    let hidden: i64 = lua.load("return o.hidden").eval().unwrap();
    assert_eq!(hidden, 7, "#[lua(field)] should force-expose a private field");
    let shown: i64 = lua.load("return o.shown").eval().unwrap();
    assert_eq!(shown, 3);

    // Force-exposed fields are writable like any other exposed field.
    lua.load("o.hidden = 100").exec().unwrap();
    let hidden: i64 = lua.load("return o.hidden").eval().unwrap();
    assert_eq!(hidden, 100);
}
