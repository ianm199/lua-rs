//! Proves that a type whose methods come from the `#[lua_methods]` macro works
//! unchanged as (a) an owned userdata, (b) a scope-borrowed userdata, and
//! (c) a delegated sub-userdata. The macro emits ordinary
//! `UserDataMethods::add_method{,_mut}` calls, and all three usages drive the
//! same `T::add_methods`, so none of them need macro-specific handling.
//!
//! Requires the `derive` feature:
//!   cargo test -p lua-rs-runtime --features derive --test scope_with_derive
#![cfg(feature = "derive")]

use lua_rs_runtime::{lua_methods, AnyUserData, Lua, LuaUserData, UserData, UserDataMethods};

/// Child type: all of its Lua methods are macro-generated.
#[derive(LuaUserData)]
#[lua(methods)]
struct Knob {
    setting: i64,
}

#[lua_methods]
impl Knob {
    pub fn turn(&mut self, by: i64) -> i64 {
        self.setting += by;
        self.setting
    }
    pub fn read(&self) -> i64 {
        self.setting
    }
}

/// Parent type, hand-written, exposing `panel:knob()` as a delegate. The
/// accessor must use `add_function` (it needs the receiver handle to build
/// the delegate); everything the delegate then dispatches is `Knob`'s
/// macro-generated methods.
struct Panel {
    knob: Knob,
}

impl UserData for Panel {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_function("knob", |lua, this: AnyUserData| {
            this.delegate::<Panel, Knob, _>(lua, |p| &mut p.knob)
        });
    }
}

/// (a) Owned: `Lua::create_userdata(Knob { .. })`.
#[test]
fn lua_methods_type_works_as_owned_userdata() {
    let lua = Lua::new();
    let knob = lua.create_userdata(Knob { setting: 10 }).unwrap();
    lua.globals().set("k", &knob).unwrap();
    let out: i64 = lua.load("k:turn(5); return k:read()").eval().unwrap();
    assert_eq!(out, 15);
}

/// (b) Borrowed: `Scope::create_userdata_ref_mut(&mut knob)`. Same macro
/// methods, and the mutation lands on the Rust-side value after the scope.
#[test]
fn lua_methods_type_works_as_scoped_userdata() {
    let lua = Lua::new();
    let mut knob = Knob { setting: 100 };
    let out: i64 = lua
        .scope(|s| {
            let ud = s.create_userdata_ref_mut(&lua, &mut knob)?;
            lua.globals().set("k", &ud)?;
            lua.load("k:turn(-30); return k:read()").eval()
        })
        .unwrap();
    assert_eq!(out, 70);
    assert_eq!(knob.setting, 70);
}

/// (c) Delegated: `panel:knob()` returns a sub-userdata whose methods are
/// `Knob`'s macro-generated ones, re-borrowed from the parent per call.
#[test]
fn lua_methods_type_works_as_delegate() {
    let lua = Lua::new();
    let mut panel = Panel {
        knob: Knob { setting: 0 },
    };
    let out: i64 = lua
        .scope(|s| {
            let ud = s.create_userdata_ref_mut(&lua, &mut panel)?;
            lua.globals().set("panel", &ud)?;
            lua.load(
                r#"
                local kn = panel:knob()
                kn:turn(3)
                kn:turn(4)
                return kn:read()
            "#,
            )
            .eval()
        })
        .unwrap();
    assert_eq!(out, 7);
    assert_eq!(panel.knob.setting, 7);
}
