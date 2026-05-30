# lua-rs-runtime

Embed Lua 5.4 in a Rust program. `lua-rs-runtime` is the embedding API for
[lua-rs](https://github.com/ianm199/lua-rs), a Lua 5.4 implementation written in
safe Rust. Being pure Rust, it builds for `wasm32-unknown-unknown` and needs no
C toolchain or `liblua`. It's young and it isn't LuaJIT, so if you need either,
use `mlua`.

```toml
[dependencies]
lua-rs-runtime = "0.0.17"
```

## Calling Rust from Lua

```rust
use lua_rs_runtime::{Lua, Result};

fn main() -> Result<()> {
    let lua = Lua::new();
    let f = lua.create_function(|_, name: String| Ok(format!("hello, {name}")))?;
    lua.globals().set("greet", f)?;

    let out: String = lua.load(r#"return greet("lua-rs")"#).eval()?;
    assert_eq!(out, "hello, lua-rs");
    Ok(())
}
```

The API is shaped after `mlua`: owned GC-rooted handles (`Value`, `Table`,
`Function`, `AnyUserData`), closure callbacks, conversion traits, and
`UserData` for binding Rust types with methods, fields, and metamethods. A
`#[derive(LuaUserData)]` / `#[lua_methods]` macro pair generates the
boilerplate.

## Scope: lending non-`'static` borrows

`Lua::scope` lends Lua a value that lives on the Rust stack for one call
(typically a game engine's `&mut World`). The borrow is invalidated when the
scope returns, so a script that stashes a handle and uses it later gets a clean
Lua error instead of touching freed memory.

```rust
lua.scope(|s| {
    let world = s.create_userdata_ref_mut(&lua, &mut my_world)?;
    lua.globals().set("world", &world)?;
    lua.load("world:spawn('player')").exec()
})?;
```

`Scope::create_function` does the same for closures that capture non-`'static`
borrows, and `AnyUserData::delegate` returns a sub-userdata that re-borrows a
field of its parent per call, so an `App -> World -> Component` chain stays a
chain of short borrows. Runnable example:
`cargo run -p lua-rs-runtime --example scope_world`.

## Links

- Worked Bevy 0.18 integration, native and in the browser:
  [bevy-lua-rs-starter](https://github.com/ianm199/bevy-lua-rs-starter)
  ([live demo](https://ianm199.github.io/bevy-lua-rs-starter/)).
- WebAssembly distribution: [`lua-rs-wasm`](https://www.npmjs.com/package/lua-rs-wasm).
- Project, benchmarks, and conformance: [github.com/ianm199/lua-rs](https://github.com/ianm199/lua-rs).

## License

A port of [Lua](https://www.lua.org/) (PUC-Rio). MIT-licensed.
