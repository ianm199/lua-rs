//! Exploratory sandbox behavior tests.
//!
//! Proves the three sandbox controls — instruction budget, memory ceiling,
//! and capability stripping — actually bound untrusted code, and that a
//! non-sandboxed run is unaffected.

use lua_rs_runtime::{Lua, SandboxConfig, TripReason};

/// A tight infinite loop must be aborted by the instruction budget rather
/// than hanging the process.
#[test]
fn infinite_loop_is_aborted() {
    let config = SandboxConfig {
        instruction_limit: Some(200_000),
        memory_limit_bytes: None,
        check_interval: 256,
        remove_globals: Vec::new(),
    };
    let (lua, sandbox) = Lua::sandboxed(config).unwrap();

    let result = lua.load("while true do end").exec();

    assert!(result.is_err(), "infinite loop should be aborted");
    assert_eq!(sandbox.tripped(), Some(TripReason::Instructions));
    assert_eq!(sandbox.instructions_remaining(), Some(0));
}

/// A recursive infinite loop (exercises call dispatch, not just JMP) is also
/// bounded.
#[test]
fn runaway_recursion_is_aborted() {
    let config = SandboxConfig {
        instruction_limit: Some(500_000),
        memory_limit_bytes: None,
        check_interval: 512,
        remove_globals: Vec::new(),
    };
    let (lua, sandbox) = Lua::sandboxed(config).unwrap();

    let result = lua
        .load("local function f() return 1 + (function() while true do end end)() end f()")
        .exec();

    assert!(result.is_err());
    assert_eq!(sandbox.tripped(), Some(TripReason::Instructions));
}

/// Work that finishes inside the budget runs normally and does not trip.
#[test]
fn work_within_budget_completes() {
    let config = SandboxConfig {
        instruction_limit: Some(10_000_000),
        memory_limit_bytes: None,
        check_interval: 1000,
        remove_globals: Vec::new(),
    };
    let (lua, sandbox) = Lua::sandboxed(config).unwrap();

    let result = lua
        .load("local s = 0 for i = 1, 100000 do s = s + i end assert(s == 5000050000)")
        .exec();

    assert!(result.is_ok(), "in-budget work should run: {result:?}");
    assert_eq!(sandbox.tripped(), None);
    assert!(sandbox.instructions_used().unwrap() > 0);
}

/// A memory bomb (unbounded allocation) trips the memory ceiling.
#[test]
fn memory_bomb_is_aborted() {
    let config = SandboxConfig {
        instruction_limit: None,
        memory_limit_bytes: Some(8 * 1024 * 1024),
        check_interval: 256,
        remove_globals: Vec::new(),
    };
    let (lua, sandbox) = Lua::sandboxed(config).unwrap();

    let result = lua
        .load("local t = {} local i = 0 while true do i = i + 1 t[i] = string.rep('x', 1024) end")
        .exec();

    assert!(result.is_err(), "memory bomb should be aborted");
    assert_eq!(sandbox.tripped(), Some(TripReason::Memory));
}

/// The strict preset removes host-access and code-loading globals while
/// leaving pure libraries intact.
#[test]
fn strict_preset_strips_capabilities() {
    let (lua, _sandbox) = Lua::sandboxed(SandboxConfig::strict()).unwrap();

    let result = lua
        .load(
            r#"
            assert(os.execute == nil, "os.execute should be removed")
            assert(os.exit == nil, "os.exit should be removed")
            assert(io == nil, "io should be removed")
            assert(load == nil, "load should be removed")
            assert(dofile == nil, "dofile should be removed")
            assert(require == nil, "require should be removed")
            assert(package == nil, "package should be removed")
            assert(debug == nil, "debug should be removed")
            -- pure libraries remain
            assert(string.rep ~= nil, "string should remain")
            assert(math.sqrt ~= nil, "math should remain")
            assert(table.insert ~= nil, "table should remain")
            assert(os.time ~= nil, "os.time should remain")
            assert(tostring ~= nil, "tostring should remain")
        "#,
        )
        .exec();

    assert!(result.is_ok(), "capability assertions failed: {result:?}");
}

/// After a trip, `reset()` refills the budget so the same state can run more
/// code.
#[test]
fn reset_refills_budget() {
    let config = SandboxConfig {
        instruction_limit: Some(50_000),
        memory_limit_bytes: None,
        check_interval: 256,
        remove_globals: Vec::new(),
    };
    let (lua, sandbox) = Lua::sandboxed(config).unwrap();

    assert!(lua.load("while true do end").exec().is_err());
    assert_eq!(sandbox.tripped(), Some(TripReason::Instructions));

    sandbox.reset();
    assert_eq!(sandbox.tripped(), None);
    assert_eq!(sandbox.instructions_remaining(), Some(50_000));

    let result = lua.load("assert(1 + 1 == 2)").exec();
    assert!(result.is_ok(), "post-reset run should succeed: {result:?}");
}

/// The budget follows code running inside a coroutine — the escape that the
/// GlobalState-backed design exists to close. Without metering spanning threads
/// this hangs forever.
#[test]
fn coroutine_is_metered() {
    use std::time::{Duration, Instant};
    let config = SandboxConfig {
        instruction_limit: Some(300_000),
        memory_limit_bytes: None,
        check_interval: 256,
        remove_globals: Vec::new(),
    };
    let (lua, sandbox) = Lua::sandboxed(config).unwrap();

    let start = Instant::now();
    let result = lua
        .load("local co = coroutine.wrap(function() while true do end end) co()")
        .exec();
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "coroutine ran unmetered -> budget escaped"
    );
    assert!(result.is_err(), "coroutine infinite loop should be aborted");
    assert_eq!(sandbox.tripped(), Some(TripReason::Instructions));
}

/// A coroutine that yields and resumes normally still runs to completion when
/// it stays within budget.
#[test]
fn yielding_coroutine_within_budget_completes() {
    let config = SandboxConfig {
        instruction_limit: Some(10_000_000),
        memory_limit_bytes: None,
        check_interval: 1000,
        remove_globals: Vec::new(),
    };
    let (lua, sandbox) = Lua::sandboxed(config).unwrap();

    let result = lua
        .load(
            r#"
            local co = coroutine.wrap(function()
                local s = 0
                for i = 1, 1000 do s = s + i coroutine.yield(s) end
                return s
            end)
            local last = 0
            for _ = 1, 1000 do last = co() end
            assert(last == 500500)
        "#,
        )
        .exec();
    assert!(result.is_ok(), "in-budget coroutine should run: {result:?}");
    assert_eq!(sandbox.tripped(), None);
}

/// The budget trip is uncatchable: a `pcall` loop cannot keep runaway code
/// alive. Without re-raising, this runs forever.
#[test]
fn pcall_loop_cannot_escape() {
    use std::time::{Duration, Instant};
    let config = SandboxConfig {
        instruction_limit: Some(300_000),
        memory_limit_bytes: None,
        check_interval: 256,
        remove_globals: Vec::new(),
    };
    let (lua, sandbox) = Lua::sandboxed(config).unwrap();

    let start = Instant::now();
    let result = lua
        .load("while true do pcall(function() while true do end end) end")
        .exec();
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "pcall loop escaped the budget"
    );
    assert!(result.is_err(), "pcall loop should abort, not run forever");
    assert_eq!(sandbox.tripped(), Some(TripReason::Instructions));
}

/// A single `pcall` around a runaway cannot swallow the trip: the chunk must
/// error out, not return normally.
#[test]
fn single_pcall_cannot_swallow_trip() {
    let config = SandboxConfig {
        instruction_limit: Some(300_000),
        memory_limit_bytes: None,
        check_interval: 256,
        remove_globals: Vec::new(),
    };
    let (lua, sandbox) = Lua::sandboxed(config).unwrap();

    let result = lua
        .load("local ok = pcall(function() while true do end end) return ok")
        .exec();
    assert!(result.is_err(), "pcall must not swallow the budget trip");
    assert_eq!(sandbox.tripped(), Some(TripReason::Instructions));
}

/// `xpcall`'s message handler cannot run on (and thus cannot loop on or
/// swallow) a budget trip.
#[test]
fn xpcall_cannot_swallow_trip() {
    let config = SandboxConfig {
        instruction_limit: Some(300_000),
        memory_limit_bytes: None,
        check_interval: 256,
        remove_globals: Vec::new(),
    };
    let (lua, sandbox) = Lua::sandboxed(config).unwrap();

    let result = lua
        .load(
            "local ok = xpcall(function() while true do end end, function() return 'handled' end) return ok",
        )
        .exec();
    assert!(result.is_err(), "xpcall must not swallow the budget trip");
    assert_eq!(sandbox.tripped(), Some(TripReason::Instructions));
}

/// Resuming a runaway coroutine in a loop cannot keep it alive — `resume`
/// re-raises the trip rather than returning `false, msg`.
#[test]
fn resume_loop_cannot_escape() {
    use std::time::{Duration, Instant};
    let config = SandboxConfig {
        instruction_limit: Some(300_000),
        memory_limit_bytes: None,
        check_interval: 256,
        remove_globals: Vec::new(),
    };
    let (lua, sandbox) = Lua::sandboxed(config).unwrap();

    let start = Instant::now();
    let result = lua
        .load(
            r#"
            while true do
                local co = coroutine.create(function() while true do end end)
                coroutine.resume(co)
            end
        "#,
        )
        .exec();
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "resume loop escaped the budget"
    );
    assert!(result.is_err());
    assert_eq!(sandbox.tripped(), Some(TripReason::Instructions));
}

/// `pcall` still works normally (catches ordinary errors) when no sandbox is
/// active — the re-raise is gated on an in-flight abort.
#[test]
fn pcall_still_catches_ordinary_errors_under_sandbox() {
    let config = SandboxConfig {
        instruction_limit: Some(10_000_000),
        memory_limit_bytes: None,
        check_interval: 1000,
        remove_globals: Vec::new(),
    };
    let (lua, sandbox) = Lua::sandboxed(config).unwrap();

    let result = lua
        .load(
            r#"
            local ok, msg = pcall(function() error("boom") end)
            assert(ok == false, "pcall should catch ordinary errors")
            assert(tostring(msg):find("boom"), "message should propagate")
        "#,
        )
        .exec();
    assert!(result.is_ok(), "ordinary pcall must still work: {result:?}");
    assert_eq!(sandbox.tripped(), None);
}

/// A plain (non-sandboxed) runtime is unaffected: no hook, no stripping.
#[test]
fn plain_runtime_is_unbounded() {
    let lua = Lua::new().unwrap();
    let result = lua
        .load("local s = 0 for i = 1, 1000000 do s = s + 1 end assert(s == 1000000)")
        .exec();
    assert!(result.is_ok(), "plain runtime should run freely: {result:?}");
}
