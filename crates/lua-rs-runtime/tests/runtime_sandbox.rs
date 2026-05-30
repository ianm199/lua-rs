//! The lower-level `LuaRuntime` sandbox surface (mirrors `Lua::sandboxed`), used
//! by the WASM embedding.

use lua_rs_runtime::{LuaRuntime, SandboxConfig, TripReason};

#[test]
fn runtime_install_sandbox_bounds_loop_and_resets() {
    let mut rt = LuaRuntime::new().unwrap();
    rt.install_sandbox(SandboxConfig {
        instruction_limit: Some(200_000),
        memory_limit_bytes: None,
        check_interval: 256,
        remove_globals: Vec::new(),
    })
    .unwrap();

    let result = rt.exec(b"while true do end", b"=runaway");
    assert!(result.is_err(), "infinite loop must abort");
    assert_eq!(rt.sandbox_tripped(), Some(TripReason::Instructions));

    rt.sandbox_reset();
    assert_eq!(rt.sandbox_tripped(), None);
    assert!(
        rt.exec(b"assert(1 + 1 == 2)", b"=ok").is_ok(),
        "post-reset run should succeed"
    );
}

#[test]
fn runtime_strict_strips_capabilities() {
    let mut rt = LuaRuntime::new().unwrap();
    rt.install_sandbox(SandboxConfig::strict()).unwrap();
    let result = rt.exec(
        b"assert(os.execute == nil and io == nil and load == nil and string ~= nil)",
        b"=caps",
    );
    assert!(result.is_ok(), "strict caps assertion failed: {result:?}");
}
