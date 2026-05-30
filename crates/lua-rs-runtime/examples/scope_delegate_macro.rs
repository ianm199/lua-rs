//! The `#[lua_methods]` delegate ergonomics: a method that returns `&mut T`
//! is exposed to Lua as a live sub-reference automatically, no hand-written
//! `add_function` + `delegate` glue. Chains compose.
//!
//!   cargo run -p lua-rs-runtime --example scope_delegate_macro --features derive
//!
//! Compare to `scope_world.rs`, where the same `world:position(id)` accessor
//! is written out by hand. Here every accessor below is just a normal Rust
//! method.

use lua_rs_runtime::{lua_methods, Lua, LuaUserData, Result};

#[derive(LuaUserData)]
#[lua(methods)]
struct Vec2 {
    x: f64,
    y: f64,
}

#[lua_methods]
impl Vec2 {
    pub fn translate(&mut self, dx: f64, dy: f64) {
        self.x += dx;
        self.y += dy;
    }
    pub fn len(&self) -> f64 {
        (self.x * self.x + self.y * self.y).sqrt()
    }
}

#[derive(LuaUserData)]
#[lua(methods)]
struct Entity {
    #[lua(skip)]
    pos: Vec2,
    pub name: String,
}

#[lua_methods]
impl Entity {
    // Returns `&mut Vec2` -> auto-registered as a mutable delegate.
    pub fn pos(&mut self) -> &mut Vec2 {
        &mut self.pos
    }
}

#[derive(LuaUserData)]
#[lua(methods)]
struct Scene {
    #[lua(skip)]
    entities: Vec<Entity>,
}

#[lua_methods]
impl Scene {
    // `&mut Entity` with an argument -> mutable delegate, arg captured.
    pub fn entity(&mut self, i: i64) -> &mut Entity {
        &mut self.entities[i as usize]
    }
    pub fn count(&self) -> i64 {
        self.entities.len() as i64
    }
}

fn main() -> Result<()> {
    let lua = Lua::new();
    let mut scene = Scene {
        entities: vec![
            Entity {
                pos: Vec2 { x: 0.0, y: 0.0 },
                name: "a".into(),
            },
            Entity {
                pos: Vec2 { x: 10.0, y: 10.0 },
                name: "b".into(),
            },
        ],
    };

    lua.scope(|s| {
        let ud = s.create_userdata_ref_mut(&lua, &mut scene)?;
        lua.globals().set("scene", &ud)?;
        lua.load(
            r#"
            -- Chain through two macro-generated delegates: Scene -> Entity -> Vec2.
            scene:entity(0):pos():translate(3, 4)

            local p = scene:entity(1):pos()
            p:translate(-1, -1)

            print("entities:", scene:count())
            print("a moved to len", scene:entity(0):pos():len())
        "#,
        )
        .exec()
    })?;

    println!(
        "a.pos = ({}, {}), b.pos = ({}, {})",
        scene.entities[0].pos.x,
        scene.entities[0].pos.y,
        scene.entities[1].pos.x,
        scene.entities[1].pos.y
    );
    Ok(())
}
