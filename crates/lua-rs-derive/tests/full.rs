//! The full feature, end to end: the `Vec2` from issue #23 with fields, methods,
//! `Display` -> `__tostring`, and `PartialEq`/`PartialOrd` -> `__eq`/`__lt`/`__le`,
//! all driven from Lua.

use std::fmt;

use lua_rs_derive::{lua_methods, LuaUserData};
use lua_rs_runtime::Lua;

#[derive(LuaUserData, PartialEq, PartialOrd)]
#[lua(methods)]
#[lua_impl(Display, PartialEq, PartialOrd)]
struct Vec2 {
    pub x: f64,
    pub y: f64,
}

impl fmt::Display for Vec2 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Vec2({}, {})", self.x, self.y)
    }
}

#[lua_methods]
impl Vec2 {
    pub fn length(&self) -> f64 {
        (self.x * self.x + self.y * self.y).sqrt()
    }
    pub fn scale(&mut self, k: f64) {
        self.x *= k;
        self.y *= k;
    }
    pub fn dot(&self, ox: f64, oy: f64) -> f64 {
        self.x * ox + self.y * oy
    }
}

#[test]
fn vec2_fields_methods_and_metamethods() {
    let lua = Lua::new();
    lua.globals().set("v", Vec2 { x: 3.0, y: 4.0 }).unwrap();

    // field read
    assert_eq!(lua.load("return v.x").eval::<f64>().unwrap(), 3.0);

    // no-arg method
    assert_eq!(lua.load("return v:length()").eval::<f64>().unwrap(), 5.0);

    // multi-arg method
    assert_eq!(lua.load("return v:dot(2, 3)").eval::<f64>().unwrap(), 18.0);

    // field write
    lua.load("v.x = 6").exec().unwrap();
    assert_eq!(lua.load("return v.x").eval::<f64>().unwrap(), 6.0);

    // mutating method, observed through a field
    lua.load("v:scale(2)").exec().unwrap();
    assert_eq!(lua.load("return v.x").eval::<f64>().unwrap(), 12.0);
    assert_eq!(lua.load("return v.y").eval::<f64>().unwrap(), 8.0);

    // __tostring
    assert_eq!(
        lua.load("return tostring(v)").eval::<String>().unwrap(),
        "Vec2(12, 8)"
    );
}

#[test]
fn vec2_equality_and_ordering() {
    let lua = Lua::new();
    lua.globals().set("a", Vec2 { x: 1.0, y: 1.0 }).unwrap();
    lua.globals().set("b", Vec2 { x: 1.0, y: 1.0 }).unwrap();
    lua.globals().set("c", Vec2 { x: 2.0, y: 2.0 }).unwrap();

    assert!(lua.load("return a == b").eval::<bool>().unwrap());
    assert!(lua.load("return a ~= c").eval::<bool>().unwrap());
    assert!(lua.load("return a < c").eval::<bool>().unwrap());
    assert!(lua.load("return a <= b").eval::<bool>().unwrap());
    assert!(lua.load("return c > a").eval::<bool>().unwrap());
}
