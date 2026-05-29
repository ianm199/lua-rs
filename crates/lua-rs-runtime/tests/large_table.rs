//! Regression for #37: a table must hold well past the old ~1M entry cap.
//!
//! Before the fix, any table that grew beyond 1,048,576 entries failed with
//! "not enough memory" regardless of available memory. These build tables an
//! order of magnitude larger and assert they succeed.

use lua_rs_runtime::Lua;

#[test]
fn two_million_entry_array_builds() {
    let lua = Lua::new();
    let n: i64 = lua
        .load(
            r#"
            local t = {}
            for i = 1, 2000000 do t[i] = i end
            return #t
        "#,
        )
        .eval()
        .expect("a 2M-element array must build");
    assert_eq!(n, 2_000_000);
}

#[test]
fn two_million_string_keys_build() {
    let lua = Lua::new();
    let n: i64 = lua
        .load(
            r#"
            local t = {}
            for i = 1, 2000000 do t["k" .. i] = i end
            local c = 0
            for _ in pairs(t) do c = c + 1 end
            return c
        "#,
        )
        .eval()
        .expect("a 2M-key hash table must build");
    assert_eq!(n, 2_000_000);
}
