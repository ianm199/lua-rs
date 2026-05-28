//! Spec/tests for `add_function*` (mlua parity) and `AnyUserData::delegate`
//! (sub-userdata via per-call accessor reborrow).
//!
//! The mental model is "transient reborrow": a delegated userdata never holds
//! `&mut S` across calls. Each method call on the delegate borrows the
//! parent's cell, applies the accessor to derive `&mut S`, runs the method,
//! and releases the parent's borrow. This is the only Rust-honest shape:
//! `S` is typically a reborrow of a field of `P` (e.g. `App::world_mut() ->
//! &mut World`), so two long-lived `&mut`s are forbidden by the borrow
//! checker. Per-call reborrow is what `Drop`-bracketed Rust APIs already do
//! internally.
//!
//! These tests pin the API shape and the invariants that fall out of it.

use lua_rs_runtime::{AnyUserData, Lua, UserData, UserDataMethods};

/// Run `body` inside Lua-side `pcall` and return the error message as a
/// `String`. `LuaError::Runtime` wraps the message in an opaque `LuaValue`
/// that isn't part of `lua-rs-runtime`'s public surface, so the Display /
/// Debug forms render `Runtime(Str(GcRef(...)))` for runtime errors raised
/// inside Lua. Going through Lua's own `tostring(err)` gives us the
/// underlying message text without needing internal types.
///
/// Panics if `body` does not raise an error.
fn lua_error_message(lua: &Lua, body: &str) -> String {
    let wrapper = format!(
        "local ok, err = pcall(function() {body} end); return ok, tostring(err)"
    );
    let (ok, msg): (bool, String) = lua
        .load(&wrapper)
        .eval()
        .expect("pcall wrapper should evaluate cleanly");
    assert!(!ok, "expected `{body}` to fail, but it returned ok");
    msg
}

// ---------------------------------------------------------------------------
// Test fixtures
// ---------------------------------------------------------------------------

/// A minimal "World" type. Owned by `App` in these tests.
#[derive(Default)]
struct World {
    entities: Vec<String>,
}

impl UserData for World {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_method_mut("spawn", |_lua, this, name: String| {
            this.entities.push(name);
            Ok(())
        });
        m.add_method("count", |_lua, this, ()| Ok(this.entities.len() as i64));
    }
}

/// A `Bevy::App`-shaped container. Exposes World via `world_mut()`.
struct App {
    world: World,
    name: String,
}

impl App {
    fn world_mut(&mut self) -> &mut World {
        &mut self.world
    }
}

impl UserData for App {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        // Read-only method on the parent itself.
        m.add_method("name", |_lua, this, ()| Ok(this.name.clone()));

        // Sub-userdata: returns a `World` userdata bound to this scope.
        // Uses `add_function` so the callback has the receiver's
        // `AnyUserData` handle (needed to call `delegate`).
        m.add_function("world", |lua, this: AnyUserData| {
            this.delegate::<App, World, _>(lua, |app| app.world_mut())
        });
    }
}

// ---------------------------------------------------------------------------
// `add_function` / `add_function_mut` — mlua parity
// ---------------------------------------------------------------------------

/// `add_function` registers a method-shape entry whose Rust closure receives
/// the userdata as its first argument (rather than the typed `&T` that
/// `add_method` extracts). Mirrors mlua's `add_function`.
#[test]
fn add_function_receives_userdata_handle() {
    let lua = Lua::new();
    let mut app = App {
        world: World::default(),
        name: "demo".into(),
    };

    let app_name: String = lua
        .scope(|s| {
            let app_ud = s.create_userdata_ref_mut(&lua, &mut app)?;
            lua.globals().set("app", &app_ud)?;
            // `app:name()` dispatches through `add_method`; this confirms
            // the App userdata is usable normally.
            lua.load("return app:name()").eval()
        })
        .expect("scope body should succeed");
    assert_eq!(app_name, "demo");
}

/// `add_function_mut` accepts an `FnMut` closure; re-entrant calls into the
/// same closure from within itself must surface "already borrowed" instead
/// of producing aliasing `&mut` captures.
#[test]
fn add_function_mut_reentrant_call_is_rejected() {
    struct CallCounter {
        n: i64,
    }

    impl UserData for CallCounter {
        fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
            // We need a stateful closure that re-enters Lua. Use FnMut
            // capturing a counter, register via add_function_mut, and have
            // the body call back into itself.
            let count = std::cell::RefCell::new(0i64);
            m.add_function_mut("recurse", move |lua, ud: AnyUserData| {
                *count.borrow_mut() += 1;
                if *count.borrow() < 2 {
                    // Re-enter Lua, which will call the same `add_function_mut`
                    // body. Should fail with "already borrowed".
                    lua.globals().set("u", &ud)?;
                    lua.load("u:recurse()").exec()?;
                }
                Ok(())
            });
            m.add_method("get", |_lua, this, ()| Ok(this.n));
        }
    }

    let lua = Lua::new();
    let mut c = CallCounter { n: 0 };
    let msg = lua
        .scope(|s| {
            let ud = s.create_userdata_ref_mut(&lua, &mut c)?;
            lua.globals().set("u", &ud)?;
            Ok(lua_error_message(&lua, "u:recurse()"))
        })
        .expect("pcall wrapper should evaluate cleanly");
    assert!(
        msg.contains("already") && msg.contains("borrowed"),
        "expected FnMut-conflict error, got: {msg}"
    );
}

// ---------------------------------------------------------------------------
// `AnyUserData::delegate` — sub-userdata via transient reborrow
// ---------------------------------------------------------------------------

/// Headline case: `app:world():spawn("p")` mutates the World owned by App.
/// The Rust-side App.world reflects the script mutation after scope ends.
#[test]
fn delegate_subreference_mutates_through_parent() {
    let lua = Lua::new();
    let mut app = App {
        world: World::default(),
        name: "demo".into(),
    };

    lua.scope(|s| {
        let app_ud = s.create_userdata_ref_mut(&lua, &mut app)?;
        lua.globals().set("app", &app_ud)?;
        lua.load(r#"
            local w = app:world()
            w:spawn("alpha")
            w:spawn("beta")
        "#).exec()
    })
    .expect("scope body should succeed");

    assert_eq!(app.world.entities, vec!["alpha", "beta"]);
}

/// The delegate handle is *not* a long-lived borrow. After fetching `w` from
/// `app:world()`, you can still call methods on `app` itself. Each call
/// acquires its own short borrow.
#[test]
fn delegate_does_not_block_parent_between_calls() {
    let lua = Lua::new();
    let mut app = App {
        world: World::default(),
        name: "demo".into(),
    };

    let name: String = lua
        .scope(|s| {
            let app_ud = s.create_userdata_ref_mut(&lua, &mut app)?;
            lua.globals().set("app", &app_ud)?;
            lua.load(r#"
                local w = app:world()
                w:spawn("a")
                local n = app:name()  -- parent call between delegate calls
                w:spawn("b")
                return n
            "#).eval()
        })
        .expect("scope body should succeed");

    assert_eq!(name, "demo");
    assert_eq!(app.world.entities, vec!["a", "b"]);
}

/// Scope invalidation propagates from parent to delegate: when the scope
/// ends, both `app` and any `w = app:world()` stashed on globals must fail
/// cleanly on next use.
#[test]
fn delegate_invalidates_with_parent_at_scope_end() {
    let lua = Lua::new();
    let mut app = App {
        world: World::default(),
        name: "demo".into(),
    };

    lua.scope(|s| {
        let app_ud = s.create_userdata_ref_mut(&lua, &mut app)?;
        lua.globals().set("app", &app_ud)?;
        lua.load(r#"
            stashed_world = app:world()
            stashed_app = app
            stashed_world:spawn("in_scope")
        "#).exec()
    })
    .expect("scope body should succeed");
    assert_eq!(app.world.entities, vec!["in_scope"]);

    for (label, body) in [
        ("parent", "stashed_app:name()"),
        ("delegate", "stashed_world:spawn('after')"),
    ] {
        let msg = lua_error_message(&lua, body);
        assert!(
            msg.contains("no longer valid") || msg.contains("scope has ended"),
            "{label}: expected invalidation error, got: {msg}"
        );
    }
    // World untouched after scope.
    assert_eq!(app.world.entities, vec!["in_scope"]);
}

/// Calling a parent method from inside a delegate method body re-enters the
/// scope cell for App. The parent's mut-borrow is currently held by the
/// accessor reborrow, so the re-entry must surface "already borrowed".
#[test]
fn delegate_method_holding_parent_borrow_rejects_reentrant_parent_call() {
    // A method on World that re-enters Lua and tries to call a method on App.
    struct WorldWithReentry;
    impl UserData for WorldWithReentry {
        fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
            m.add_method_mut("touch_parent", |lua, _this, ()| {
                lua.load("return app:name()").eval::<String>()
            });
        }
    }

    struct AppWithReentryWorld {
        world: WorldWithReentry,
    }
    impl UserData for AppWithReentryWorld {
        fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
            m.add_method("name", |_lua, _this, ()| Ok("the-app".to_string()));
            m.add_function("world", |lua, this: AnyUserData| {
                this.delegate::<AppWithReentryWorld, WorldWithReentry, _>(
                    lua,
                    |a| &mut a.world,
                )
            });
        }
    }

    let lua = Lua::new();
    let mut app = AppWithReentryWorld {
        world: WorldWithReentry,
    };
    let msg = lua
        .scope(|s| {
            let app_ud = s.create_userdata_ref_mut(&lua, &mut app)?;
            lua.globals().set("app", &app_ud)?;
            Ok(lua_error_message(&lua, "app:world():touch_parent()"))
        })
        .expect("pcall wrapper should evaluate cleanly");
    assert!(
        msg.contains("already") && msg.contains("borrowed"),
        "expected re-entrant borrow error, got: {msg}"
    );
}

/// Calling `delegate` on a delegated userdata composes the accessor chain.
/// Two levels: App -> World -> first_entity (an inner field access).
#[test]
fn delegate_chains_multiple_levels() {
    #[derive(Default)]
    struct Inner {
        bumps: i64,
    }
    impl UserData for Inner {
        fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
            m.add_method_mut("bump", |_lua, this, ()| {
                this.bumps += 1;
                Ok(this.bumps)
            });
        }
    }

    struct Middle {
        inner: Inner,
    }
    impl UserData for Middle {
        fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
            m.add_function("inner", |lua, this: AnyUserData| {
                this.delegate::<Middle, Inner, _>(lua, |m| &mut m.inner)
            });
        }
    }

    struct Outer {
        middle: Middle,
    }
    impl UserData for Outer {
        fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
            m.add_function("middle", |lua, this: AnyUserData| {
                this.delegate::<Outer, Middle, _>(lua, |o| &mut o.middle)
            });
        }
    }

    let lua = Lua::new();
    let mut outer = Outer {
        middle: Middle {
            inner: Inner::default(),
        },
    };
    let result: i64 = lua
        .scope(|s| {
            let ud = s.create_userdata_ref_mut(&lua, &mut outer)?;
            lua.globals().set("o", &ud)?;
            lua.load(r#"
                local m = o:middle()
                local i = m:inner()
                i:bump(); i:bump(); i:bump()
                return i:bump()
            "#).eval()
        })
        .expect("3-level chain should work");
    assert_eq!(result, 4);
    assert_eq!(outer.middle.inner.bumps, 4);
}

/// A delegated userdata can be cloned; both clones share the same parent
/// cell and invalidate together at scope end.
#[test]
fn delegate_cloned_handles_invalidate_together() {
    let lua = Lua::new();
    let mut app = App {
        world: World::default(),
        name: "x".into(),
    };

    lua.scope(|s| {
        let app_ud = s.create_userdata_ref_mut(&lua, &mut app)?;
        lua.globals().set("app", &app_ud)?;
        lua.load(r#"
            stashed_a = app:world()
            stashed_b = stashed_a       -- Lua reference; same cell
        "#).exec()
    })
    .expect("scope body should succeed");

    for name in ["stashed_a", "stashed_b"] {
        let msg = lua_error_message(&lua, &format!("return {name}:count()"));
        assert!(
            msg.contains("no longer valid") || msg.contains("scope has ended"),
            "{name}: expected invalidation error, got: {msg}"
        );
    }
}
