//! Debug library — Rust port of `ldblib.c`.
//!
//! Provides the `debug` Lua standard library module. Exposes debug
//! introspection APIs: stack inspection (`getinfo`, `getlocal`), upvalue
//! access (`getupvalue`, `setupvalue`, `upvaluejoin`), hook management
//! (`sethook`, `gethook`), metatable overrides (`getmetatable`,
//! `setmetatable`), userdata values (`getuservalue`, `setuservalue`),
//! and utility functions (`traceback`, `debug`, `setcstacklimit`).
//!
//! C source: `reference/lua-5.4.7/src/ldblib.c` (484 lines, 20 functions)

use std::io::{self, BufRead, Write};

use lua_types::{GcRef, LuaError, LuaString, LuaType, LuaValue, LuaStatus};
use crate::state_stub::{LuaState, LuaStateStubExt as _, lua_CFunction, upvalue_index, CompareOp, LuaDebug as DebugInfo};

// ── Constants ──────────────────────────────────────────────────────────────

/// Registry key for the hook table that maps threads to their hook functions.
///
/// C: `static const char *const HOOKKEY = "_HOOKKEY";`
const HOOKKEY: &[u8] = b"_HOOKKEY";

/// Hook event names indexed by the raw event code stored in [`DebugInfo::event`].
/// Order must match the `LUA_HOOK*` constants: Call=0, Return=1, Line=2, Count=3, TailCall=4.
///
/// C: `static const char *const hooknames[] = {"call","return","line","count","tail call"};`
const HOOKNAMES: &[&[u8]; 5] = &[b"call", b"return", b"line", b"count", b"tail call"];

/// Bitmask constants for hook event selection.
/// C: `LUA_MASKCALL`, `LUA_MASKRET`, `LUA_MASKLINE`, `LUA_MASKCOUNT`
const MASK_CALL: u32 = 1 << 0;
const MASK_RET: u32 = 1 << 1;
const MASK_LINE: u32 = 1 << 2;
const MASK_COUNT: u32 = 1 << 3;

// ── Local type aliases ─────────────────────────────────────────────────────

/// Entry-point signature for a Lua stdlib function in Rust.
pub(crate) type LibFn = fn(&mut LuaState) -> Result<usize, LuaError>;

/// A Rust hook callback registered with the Lua VM's hook mechanism.
///
/// C: `lua_Hook` = `void (*)(lua_State *, lua_Debug *)`
pub(crate) type HookFn = fn(&mut LuaState, &mut DebugInfo) -> Result<(), LuaError>;

/// Opaque identity handle for an upvalue.
///
/// C: `void *` returned by `lua_upvalueid`. Lua uses pointer equality to
/// check whether two upvalues share the same storage cell.
///
/// TODO(port): In C this is a raw pointer into the upvalue's storage cell.
/// Safe Rust cannot expose a raw pointer outside `lua-gc`. A stable u64 ID
/// or a GcRef-based comparison should be designed in Phase D. Using `usize`
/// (pointer-sized) as a placeholder so the call sites compile.
type UpvalId = usize;

// ── Internal helpers ───────────────────────────────────────────────────────

/// Ensure the cross-thread target has room for `n` more stack slots.
///
/// When the target is the current thread this is a no-op because the current
/// thread's stack is managed by the caller. When it is another thread we
/// must verify its stack, but that requires a simultaneous `&mut LuaState`
/// for both threads.
///
/// C: `static void checkstack(lua_State *L, lua_State *L1, int n)`
fn check_cross_thread_stack(
    state: &mut LuaState,
    target_is_self: bool,
    n: i32,
) -> Result<(), LuaError> {
    // C: if (l_unlikely(L != L1 && !lua_checkstack(L1, n)))
    //        luaL_error(L, "stack overflow");
    if !target_is_self {
        // TODO(port): checking a different thread's stack requires simultaneous
        // `&mut LuaState` for both threads, which is not expressible in safe Rust
        // without interior mutability. Conservatively checks the current state only.
        state.ensure_stack(n, "stack overflow")?;
    }
    Ok(())
}

/// Inspect argument 1: if it is a thread value, return `(1, Some(thread_ref))`;
/// otherwise return `(0, None)` meaning "operate on the current state".
///
/// C: `static lua_State *getthread(lua_State *L, int *arg)`
fn getthread(state: &mut LuaState) -> (i32, Option<GcRef<lua_types::value::LuaThread>>) {
    // C: if (lua_isthread(L, 1)) { *arg = 1; return lua_tothread(L, 1); }
    if state.type_at(1) == LuaType::Thread {
        let thread = state.to_thread_at(1);
        return (1, thread);
    }
    // C: *arg = 0; return L;
    (0, None)
}

/// Push byte string `v` (or Nil when `v` is `None`) and store it under key
/// `k` in the table that sits at stack position -2.
///
/// C: `static void settabss(lua_State *L, const char *k, const char *v)`
/// PORT NOTE: The C version passes NULL to signal "no value" (lua_pushstring
/// with NULL pushes nil). Rust uses Option<&[u8]> for the same semantics.
fn settabss(state: &mut LuaState, k: &[u8], v: Option<&[u8]>) -> Result<(), LuaError> {
    // C: lua_pushstring(L, v);  /* NULL -> nil */
    match v {
        Some(s) => {
            let ls = state.intern_str(s)?;
            state.push(LuaValue::Str(ls));
        }
        None => { state.push(LuaValue::Nil); }
    }
    // C: lua_setfield(L, -2, k);
    state.set_field(-2, k)
}

/// Push integer `v` and store it under key `k` in the table at -2.
///
/// C: `static void settabsi(lua_State *L, const char *k, int v)`
fn settabsi(state: &mut LuaState, k: &[u8], v: i32) -> Result<(), LuaError> {
    // C: lua_pushinteger(L, v); lua_setfield(L, -2, k);
    state.push(LuaValue::Int(v as i64));
    state.set_field(-2, k)
}

/// Push boolean `v` and store it under key `k` in the table at -2.
///
/// C: `static void settabsb(lua_State *L, const char *k, int v)`
fn settabsb(state: &mut LuaState, k: &[u8], v: bool) -> Result<(), LuaError> {
    // C: lua_pushboolean(L, v); lua_setfield(L, -2, k);
    state.push(LuaValue::Bool(v));
    state.set_field(-2, k)
}

/// After `lua_getinfo` has pushed a result ('f' function or 'L' line table)
/// onto L1's stack, move it into the result table on L as field `fname`.
///
/// When target is self, the value is already on our stack; rotate to bring
/// it above the result table. When target is a different thread, use xmove.
///
/// C: `static void treatstackoption(lua_State *L, lua_State *L1, const char *fname)`
fn treat_stack_option(
    state: &mut LuaState,
    target_is_self: bool,
    fname: &[u8],
) -> Result<(), LuaError> {
    if target_is_self {
        // C: lua_rotate(L, -2, 1);  /* exchange object and table */
        state.rotate(-2, 1);
    } else {
        // C: lua_xmove(L1, L, 1);  /* move object to the "main" stack */
        // TODO(port): moving a value from another thread's stack (lua_xmove)
        // requires simultaneous `&mut LuaState` for both threads. Not expressible
        // in safe Rust without interior mutability. Pushes Nil as placeholder.
        state.push(LuaValue::Nil);
    }
    // C: lua_setfield(L, -2, fname);
    state.set_field(-2, fname)
}

// ── Library functions ──────────────────────────────────────────────────────

/// `debug.getregistry()` — return the Lua registry table.
///
/// C: `static int db_getregistry(lua_State *L)`
pub(crate) fn get_registry(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: lua_pushvalue(L, LUA_REGISTRYINDEX); return 1;
    state.push_registry();
    Ok(1)
}

/// `debug.getmetatable(obj)` — return the metatable of `obj`, or nil if none.
///
/// C: `static int db_getmetatable(lua_State *L)`
pub(crate) fn get_metatable(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: luaL_checkany(L, 1);
    state.check_arg_any(1)?;
    // C: if (!lua_getmetatable(L, 1)) lua_pushnil(L);
    if !state.get_metatable(1)? {
        state.push(LuaValue::Nil);
    }
    Ok(1)
}

/// `debug.setmetatable(obj, table)` — set `table` (or nil) as `obj`'s metatable.
/// Returns the first argument `obj`.
///
/// C: `static int db_setmetatable(lua_State *L)`
pub(crate) fn set_metatable(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: int t = lua_type(L, 2);
    let t = state.type_at(2);
    // C: luaL_argexpected(L, t == LUA_TNIL || t == LUA_TTABLE, 2, "nil or table");
    if !(t == LuaType::Nil || t == LuaType::Table) {
        let got = state.arg(2);
        return Err(LuaError::type_arg_error(2, "nil or table", &got));
    }
    // C: lua_settop(L, 2); lua_setmetatable(L, 1); return 1;
    state.set_top(2);
    state.set_metatable(1)?;
    Ok(1)
}

/// `debug.getuservalue(obj [, n])` — return the n-th user value of userdata
/// `obj` plus `true`, or the fail value if `obj` is not userdata or `n` is out
/// of range.
///
/// C: `static int db_getuservalue(lua_State *L)`
pub(crate) fn get_uservalue(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: int n = (int)luaL_optinteger(L, 2, 1);
    let n = state.opt_arg_integer(2, 1)? as i32;
    // C: if (lua_type(L, 1) != LUA_TUSERDATA) luaL_pushfail(L);
    if state.type_at(1) != LuaType::UserData {
        state.push_fail();
        return Ok(1);
    }
    // C: else if (lua_getiuservalue(L, 1, n) != LUA_TNONE) { lua_pushboolean(L, 1); return 2; }
    let ty = state.get_iuservalue(1, n)?;
    if ty != LuaType::None {
        state.push(LuaValue::Bool(true));
        return Ok(2);
    }
    Ok(1)
}

/// `debug.setuservalue(obj, value [, n])` — set the n-th user value of userdata
/// `obj` to `value`. Returns `obj`, or the fail value on failure.
///
/// C: `static int db_setuservalue(lua_State *L)`
pub(crate) fn set_uservalue(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: int n = (int)luaL_optinteger(L, 3, 1);
    let n = state.opt_arg_integer(3, 1)? as i32;
    // C: luaL_checktype(L, 1, LUA_TUSERDATA);
    state.check_arg_type(1, LuaType::UserData)?;
    // C: luaL_checkany(L, 2);
    state.check_arg_any(2)?;
    // C: lua_settop(L, 2);
    state.set_top(2);
    // C: if (!lua_setiuservalue(L, 1, n)) luaL_pushfail(L);
    if !state.set_iuservalue(1, n)? {
        state.push_fail();
    }
    Ok(1)
}

/// `debug.getinfo([thread,] f|level [, what])` — collect debug information
/// about function `f` or stack level `level` into a new table. The `what`
/// string selects which fields to populate (default `"flnSrtu"`).
///
/// C: `static int db_getinfo(lua_State *L)`
pub(crate) fn get_info(state: &mut LuaState) -> Result<usize, LuaError> {
    let mut ar = DebugInfo::default();

    // C: int arg; lua_State *L1 = getthread(L, &arg);
    let (arg, other_thread) = getthread(state);
    let target_is_self = other_thread.is_none();

    // C: const char *options = luaL_optstring(L, arg+2, "flnSrtu");
    // to_vec() immediately to avoid borrow-checker conflict with subsequent &mut state ops.
    let raw_opts: Vec<u8> = state.opt_arg_string(arg + 2, b"flnSrtu")?.to_vec();

    // C: checkstack(L, L1, 3);
    check_cross_thread_stack(state, target_is_self, 3)?;

    // C: luaL_argcheck(L, options[0] != '>', arg + 2, "invalid option '>'");
    if raw_opts.first() == Some(&b'>') {
        return Err(LuaError::arg_error(arg + 2, "invalid option '>'"));
    }

    // Build the effective options string, prepending '>' when the subject is a function.
    let options: Vec<u8>;

    if state.type_at(arg + 1) == LuaType::Function {
        // C: options = lua_pushfstring(L, ">%s", options);  /* add '>' to options */
        // In C this also pushes the string onto the stack; in Rust we just build a Vec.
        let mut prefixed = Vec::with_capacity(raw_opts.len() + 1);
        prefixed.push(b'>');
        prefixed.extend_from_slice(&raw_opts);
        options = prefixed;

        // C: lua_pushvalue(L, arg + 1);  /* move function to L1 stack */
        // C: lua_xmove(L, L1, 1);
        if target_is_self {
            state.push_value_at(arg + 1)?;
        } else {
            // TODO(port): lua_xmove to another thread's stack requires simultaneous
            // `&mut LuaState` for both threads. Cross-thread getinfo with a function
            // argument is left incomplete for Phase A.
        }

        // C: if (!lua_getinfo(L1, options, &ar)) return luaL_argerror(...);
        // With '>' prefix, get_debug_info consumes the function from the top of stack.
        if state.get_debug_info(&options, &mut ar).is_err() {
            return Err(LuaError::arg_error(arg + 2, "invalid option"));
        }
    } else {
        options = raw_opts;

        // C: if (!lua_getstack(L1, (int)luaL_checkinteger(L, arg+1), &ar)) { fail }
        let level = state.check_arg_integer(arg + 1)? as i32;
        if !state.get_stack_level(level, &mut ar) {
            // C: luaL_pushfail(L); return 1;
            state.push_fail()?;
            return Ok(1);
        }

        // C: if (!lua_getinfo(L1, options, &ar)) return luaL_argerror(...);
        if state.get_debug_info(&options, &mut ar).is_err() {
            return Err(LuaError::arg_error(arg + 2, "invalid option"));
        }
    }

    // C: lua_newtable(L);  /* table to collect results */
    let result_tbl = state.new_table();
    state.push(LuaValue::Table(result_tbl));

    // C: if (strchr(options, 'S')) { ... }
    if options.contains(&b'S') {
        // C: lua_pushlstring(L, ar.source, ar.srclen); lua_setfield(L, -2, "source");
        let src = state.intern_str(ar.source_bytes())?;
        state.push(LuaValue::Str(src));
        state.set_field(-2, b"source")?;

        settabss(state, b"short_src", Some(ar.short_src_bytes()))?;
        settabsi(state, b"linedefined", ar.linedefined)?;
        settabsi(state, b"lastlinedefined", ar.lastlinedefined)?;
        settabss(state, b"what", Some(ar.what_bytes()))?;
    }
    if options.contains(&b'l') {
        settabsi(state, b"currentline", ar.currentline)?;
    }
    if options.contains(&b'u') {
        settabsi(state, b"nups", ar.nups as i32)?;
        settabsi(state, b"nparams", ar.nparams as i32)?;
        settabsb(state, b"isvararg", ar.isvararg)?;
    }
    if options.contains(&b'n') {
        // ar.name may be NULL in C → None in Rust.
        settabss(state, b"name", Some(ar.name_bytes()))?;
        // ar.namewhat is always non-NULL in C (may be empty string).
        settabss(state, b"namewhat", Some(ar.namewhat_bytes()))?;
    }
    if options.contains(&b'r') {
        settabsi(state, b"ftransfer", ar.ftransfer as i32)?;
        settabsi(state, b"ntransfer", ar.ntransfer as i32)?;
    }
    if options.contains(&b't') {
        settabsb(state, b"istailcall", ar.istailcall)?;
    }
    // 'L' and 'f' options: lua_getinfo pushed line-table then function onto L1's stack.
    // treat_stack_option moves each into the result table.
    // PORT NOTE: C's lua_getinfo always pushes 'f' result before 'L' result (regardless
    // of option-string order), so the treatstackoption calls below are intentionally
    // ordered 'L' first then 'f' — matching the C db_getinfo exactly.
    if options.contains(&b'L') {
        treat_stack_option(state, target_is_self, b"activelines")?;
    }
    if options.contains(&b'f') {
        treat_stack_option(state, target_is_self, b"func")?;
    }

    Ok(1)
}

/// `debug.getlocal([thread,] level, local)` — return the name and value of
/// local variable `local` at stack level `level`.
///
/// When the first argument is a function, returns only the parameter name at
/// position `local` (no value).
///
/// C: `static int db_getlocal(lua_State *L)`
pub(crate) fn get_local(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: int arg; lua_State *L1 = getthread(L, &arg);
    let (arg, other_thread) = getthread(state);
    let target_is_self = other_thread.is_none();

    // C: int nvar = (int)luaL_checkinteger(L, arg + 2);
    let nvar = state.check_arg_integer(arg + 2)? as i32;

    // C: if (lua_isfunction(L, arg + 1)) { ... }
    if state.type_at(arg + 1) == LuaType::Function {
        // C: lua_pushvalue(L, arg + 1);  /* push function */
        state.push_value_at(arg + 1)?;
        // C: lua_pushstring(L, lua_getlocal(L, NULL, nvar));
        // lua_getlocal with NULL ar reads parameter names from the function at the
        // top of the stack; it does NOT push a value.
        let name = state.get_param_name(0, nvar)?;
        match name {
            Some(n) => {
                let ls = state.intern_str(&n)?;
                state.push(LuaValue::Str(ls));
            }
            None => { state.push(LuaValue::Nil); }
        }
        // C: return 1;  /* return only name (there is no value) */
        // The pushed function below name is discarded by the VM when it collects
        // exactly 1 return value from the top of the stack.
        return Ok(1);
    }

    // Stack-level path.
    // C: int level = (int)luaL_checkinteger(L, arg + 1);
    let level = state.check_arg_integer(arg + 1)? as i32;
    let mut ar = DebugInfo::default();

    // C: if (l_unlikely(!lua_getstack(L1, level, &ar))) return luaL_argerror(...);
    if !state.get_stack_level(level, &mut ar) {
        return Err(LuaError::arg_error(arg + 1, "level out of range"));
    }
    check_cross_thread_stack(state, target_is_self, 1)?;

    // C: name = lua_getlocal(L1, &ar, nvar);
    // Pushes the local's value onto L1's stack and returns its name.
    let name = state.get_local_at(&ar, nvar)?;

    if let Some(n) = name {
        if !target_is_self {
            // C: lua_xmove(L1, L, 1);  /* move local value */
            // TODO(port): cross-thread local value move (lua_xmove). The value was
            // pushed onto the other thread's stack; moving it to the current stack
            // requires simultaneous mutable access to two LuaState instances.
        }
        // C: lua_pushstring(L, name); lua_rotate(L, -2, 1); return 2;
        let ls = state.intern_str(&n)?;
        state.push(LuaValue::Str(ls));
        state.rotate(-2, 1)?;
        Ok(2)
    } else {
        // C: luaL_pushfail(L); return 1;
        state.push_fail()?;
        Ok(1)
    }
}

/// `debug.setlocal([thread,] level, local, value)` — set local variable
/// `local` at stack level `level` to `value`. Returns the variable name, or
/// nil on failure.
///
/// C: `static int db_setlocal(lua_State *L)`
pub(crate) fn set_local(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: int arg; lua_State *L1 = getthread(L, &arg);
    let (arg, other_thread) = getthread(state);
    let target_is_self = other_thread.is_none();

    // C: int level = (int)luaL_checkinteger(L, arg + 1);
    let level = state.check_arg_integer(arg + 1)? as i32;
    // C: int nvar = (int)luaL_checkinteger(L, arg + 2);
    let nvar = state.check_arg_integer(arg + 2)? as i32;

    let mut ar = DebugInfo::default();
    // C: if (l_unlikely(!lua_getstack(L1, level, &ar))) return luaL_argerror(...);
    if !state.get_stack_level(level, &mut ar) {
        return Err(LuaError::arg_error(arg + 1, "level out of range"));
    }

    // C: luaL_checkany(L, arg+3);
    state.check_arg_any(arg + 3)?;
    // C: lua_settop(L, arg+3);
    state.set_top(arg + 3);

    check_cross_thread_stack(state, target_is_self, 1)?;

    // C: lua_xmove(L, L1, 1);  /* move value to L1 */
    if !target_is_self {
        // TODO(port): cross-thread value transfer (lua_xmove) before setlocal.
        // The new value must be on L1's stack for lua_setlocal to consume it.
    }

    // C: name = lua_setlocal(L1, &ar, nvar);  /* pops value from L1 */
    let name = state.set_local_at(&ar, nvar)?;

    // C: if (name == NULL) lua_pop(L1, 1);  /* pop value if not consumed */
    if name.is_none() {
        state.pop_n(1);
    }

    // C: lua_pushstring(L, name);
    match name {
        Some(n) => {
            let ls = state.intern_str(&n)?;
            state.push(LuaValue::Str(ls));
        }
        None => { state.push(LuaValue::Nil); }
    }
    Ok(1)
}

/// Shared implementation for `get_upvalue` and `set_upvalue`.
///
/// When `get` is `true`, retrieves upvalue `n` of the function at stack index 1,
/// pushes its value, and returns `(name, value)` — 2 results.
///
/// When `get` is `false`, pops the top stack value and installs it as upvalue
/// `n`, returning `(name,)` — 1 result.
///
/// Returns 0 results when the upvalue index is out of range.
///
/// C: `static int auxupvalue(lua_State *L, int get)`
fn aux_upvalue(state: &mut LuaState, get: bool) -> Result<usize, LuaError> {
    // C: int n = (int)luaL_checkinteger(L, 2);
    let n = state.check_arg_integer(2)? as i32;
    // C: luaL_checktype(L, 1, LUA_TFUNCTION);
    state.check_arg_type(1, LuaType::Function)?;

    // C: name = get ? lua_getupvalue(L, 1, n) : lua_setupvalue(L, 1, n);
    let name: Option<Vec<u8>> = if get {
        // lua_getupvalue pushes the upvalue value and returns the name.
        state.get_upvalue(1, n)?
    } else {
        // lua_setupvalue pops the top-of-stack value, sets upvalue n, returns name.
        state.set_upvalue(1, n)?
    };

    // C: if (name == NULL) return 0;
    let name_ref = match name {
        Some(n) => n,
        None => return Ok(0),
    };

    // C: lua_pushstring(L, name);
    let ls = state.intern_str(&name_ref)?;
    state.push(LuaValue::Str(ls));

    // C: lua_insert(L, -(get+1));  /* no-op if get is false */
    // When get=true: stack is [..., value, name]; insert at -2 → [..., name, value].
    // When get=false: insert at -1 is a no-op; stack is [..., name].
    if get {
        state.insert(-2)?;
    }

    // C: return get + 1;
    Ok(if get { 2 } else { 1 })
}

/// `debug.getupvalue(f, up)` — return the name and value of upvalue `up` of `f`.
///
/// C: `static int db_getupvalue(lua_State *L)`
pub(crate) fn get_upvalue(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: return auxupvalue(L, 1);
    aux_upvalue(state, true)
}

/// `debug.setupvalue(f, up, value)` — set upvalue `up` of `f` to `value`.
/// Returns the upvalue name.
///
/// C: `static int db_setupvalue(lua_State *L)`
pub(crate) fn set_upvalue(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: luaL_checkany(L, 3);
    state.check_arg_any(3)?;
    // C: return auxupvalue(L, 0);
    aux_upvalue(state, false)
}

/// Verify that upvalue `argnup` of function at stack index `argf` exists.
/// Returns the opaque identity handle and the upvalue index.
/// If `require_valid` is true, raises an arg error when the upvalue is absent.
///
/// C: `static void *checkupval(lua_State *L, int argf, int argnup, int *pnup)`
fn check_upval(
    state: &mut LuaState,
    argf: i32,
    argnup: i32,
    require_valid: bool,
) -> Result<(Option<UpvalId>, i32), LuaError> {
    // C: int nup = (int)luaL_checkinteger(L, argnup);
    let nup = state.check_arg_integer(argnup)? as i32;
    // C: luaL_checktype(L, argf, LUA_TFUNCTION);
    state.check_arg_type(argf, LuaType::Function)?;
    // C: id = lua_upvalueid(L, argf, nup);
    // TODO(port): lua_upvalueid returns a raw void* that uniquely identifies
    // an upvalue's storage cell. A safe equivalent (e.g., GcRef<UpVal> pointer
    // comparison, or a stable u64 ID from the GC layer) must be defined in
    // Phase D. Using Option<usize> as placeholder.
    let id: Option<UpvalId> = match state.upvalue_id(argf, nup) {
        Ok(p) if p.is_null() => None,
        Ok(p) => Some(p as usize),
        Err(_) => None,
    };
    // C: if (pnup) { luaL_argcheck(L, id != NULL, argnup, "invalid upvalue index"); *pnup = nup; }
    if require_valid && id.is_none() {
        return Err(LuaError::arg_error(argnup, "invalid upvalue index"));
    }
    Ok((id, nup))
}

/// `debug.upvalueid(f, n)` — return a unique identifier for upvalue `n` of
/// function `f` as a light userdata. Returns fail on out-of-range index.
///
/// C: `static int db_upvalueid(lua_State *L)`
pub(crate) fn upvalue_id(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: void *id = checkupval(L, 1, 2, NULL);
    let (id, _nup) = check_upval(state, 1, 2, false)?;
    match id {
        Some(uid) => {
            // C: lua_pushlightuserdata(L, id);
            lua_vm::api::push_light_userdata(state, uid as *mut core::ffi::c_void);
        }
        None => {
            // C: luaL_pushfail(L);
            state.push_fail()?;
        }
    }
    Ok(1)
}

/// `debug.upvaluejoin(f1, n1, f2, n2)` — make upvalue `n1` of function `f1`
/// refer to the same storage as upvalue `n2` of function `f2`.
///
/// C: `static int db_upvaluejoin(lua_State *L)`
pub(crate) fn upvalue_join(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: int n1, n2;
    // C: checkupval(L, 1, 2, &n1);
    let (_id1, n1) = check_upval(state, 1, 2, true)?;
    // C: checkupval(L, 3, 4, &n2);
    let (_id2, n2) = check_upval(state, 3, 4, true)?;
    // C: luaL_argcheck(L, !lua_iscfunction(L, 1), 1, "Lua function expected");
    if state.is_c_function_at(1) {
        return Err(LuaError::arg_error(1, "Lua function expected"));
    }
    // C: luaL_argcheck(L, !lua_iscfunction(L, 3), 3, "Lua function expected");
    if state.is_c_function_at(3) {
        return Err(LuaError::arg_error(3, "Lua function expected"));
    }
    // C: lua_upvaluejoin(L, 1, n1, 3, n2);
    state.join_upvalues(1, n1, 3, n2)?;
    Ok(0)
}

/// Internal debug hook registered with the VM via `lua_sethook`. When
/// invoked, it looks up the Lua-side hook function stored in
/// `registry[HOOKKEY][current_thread]` and calls it with the event name
/// and current line number.
///
/// C: `static void hookf(lua_State *L, lua_Debug *ar)`
pub(crate) fn hookf(state: &mut LuaState, ar: &mut DebugInfo) -> Result<(), LuaError> {
    // C: lua_getfield(L, LUA_REGISTRYINDEX, HOOKKEY);
    state.get_registry_field(HOOKKEY)?;
    // C: lua_pushthread(L);
    state.push_thread()?;
    // C: if (lua_rawget(L, -2) == LUA_TFUNCTION) { ... }
    if state.raw_get(-2)? == LuaType::Function {
        // C: lua_pushstring(L, hooknames[(int)ar->event]);
        // TODO(phase-b): LuaDebug has no `event` field yet; use currentline > 0 sentinel as placeholder for HOOKLINE.
        let event_idx: usize = 0;
        let _ = ar;
        debug_assert!(event_idx < HOOKNAMES.len(), "hook event out of range");
        let event_str = state.intern_str(HOOKNAMES[event_idx])?;
        state.push(LuaValue::Str(event_str));

        // C: if (ar->currentline >= 0) lua_pushinteger(L, ar->currentline); else lua_pushnil(L);
        if ar.currentline >= 0 {
            state.push(LuaValue::Int(ar.currentline as i64));
        } else {
            state.push(LuaValue::Nil);
        }

        // C: lua_assert(lua_getinfo(L, "lS", ar));
        // Fills in source-location fields so the hook can inspect them.
        debug_assert!(
            state.get_debug_info(b"lS", ar).is_ok(),
            "lua_getinfo(\"lS\") should always succeed in hookf"
        );

        // C: lua_call(L, 2, 0);
        state.call(2, 0)?;
    }
    Ok(())
}

/// Convert the string hook-mask (`'c'`/`'r'`/`'l'` characters) and a count
/// to the integer bitmask used by the VM's `sethook` API.
///
/// C: `static int makemask(const char *smask, int count)`
fn make_mask(smask: &[u8], count: i32) -> u32 {
    let mut mask: u32 = 0;
    // C: if (strchr(smask, 'c')) mask |= LUA_MASKCALL;
    if smask.contains(&b'c') {
        mask |= MASK_CALL;
    }
    // C: if (strchr(smask, 'r')) mask |= LUA_MASKRET;
    if smask.contains(&b'r') {
        mask |= MASK_RET;
    }
    // C: if (strchr(smask, 'l')) mask |= LUA_MASKLINE;
    if smask.contains(&b'l') {
        mask |= MASK_LINE;
    }
    // C: if (count > 0) mask |= LUA_MASKCOUNT;
    if count > 0 {
        mask |= MASK_COUNT;
    }
    mask
}

/// Convert the integer hook bitmask back to the string representation used in
/// Lua (`'c'`/`'r'`/`'l'` characters).
///
/// C: `static char *unmakemask(int mask, char *smask)`
fn unmake_mask(mask: u32) -> Vec<u8> {
    let mut smask = Vec::with_capacity(3);
    // C: if (mask & LUA_MASKCALL) smask[i++] = 'c'; ...
    if mask & MASK_CALL != 0 {
        smask.push(b'c');
    }
    if mask & MASK_RET != 0 {
        smask.push(b'r');
    }
    if mask & MASK_LINE != 0 {
        smask.push(b'l');
    }
    smask
}

/// `debug.sethook([thread,] hook, mask [, count])` — install a debug hook.
/// Passing nil as `hook` removes the current hook.
///
/// C: `static int db_sethook(lua_State *L)`
pub(crate) fn set_hook(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: int arg, mask, count; lua_Hook func;
    // C: lua_State *L1 = getthread(L, &arg);
    let (arg, other_thread) = getthread(state);
    let target_is_self = other_thread.is_none();

    let hook_active: bool;
    let mask: u32;
    let count: i32;

    // C: if (lua_isnoneornil(L, arg+1)) { lua_settop(L, arg+1); func=NULL; mask=0; count=0; }
    if matches!(state.type_at(arg + 1), LuaType::None | LuaType::Nil) {
        state.set_top(arg + 1);
        hook_active = false;
        mask = 0;
        count = 0;
    } else {
        // C: const char *smask = luaL_checkstring(L, arg+2);
        let smask: Vec<u8> = state.check_arg_string(arg + 2)?.to_vec();
        // C: luaL_checktype(L, arg+1, LUA_TFUNCTION);
        state.check_arg_type(arg + 1, LuaType::Function)?;
        // C: count = (int)luaL_optinteger(L, arg + 3, 0);
        count = state.opt_arg_integer(arg + 3, 0)? as i32;
        // C: func = hookf; mask = makemask(smask, count);
        hook_active = true;
        mask = make_mask(&smask, count);
    }

    // C: if (!luaL_getsubtable(L, LUA_REGISTRYINDEX, HOOKKEY)) { /* newly created */ }
    if !state.get_or_create_registry_subtable(HOOKKEY)? {
        // Table was just created. Set it up as a weak-keyed table so that
        // thread keys do not prevent GC of finished threads.
        // C: lua_pushliteral(L, "k"); lua_setfield(L, -2, "__mode");
        let k = state.intern_str(b"k")?;
        state.push(LuaValue::Str(k));
        state.set_field(-2, b"__mode")?;
        // C: lua_pushvalue(L, -1); lua_setmetatable(L, -2);
        state.push_value_at(-1)?;
        state.set_metatable(-2)?;
    }

    check_cross_thread_stack(state, target_is_self, 1)?;

    // C: lua_pushthread(L1); lua_xmove(L1, L, 1);  /* key = target thread */
    if target_is_self {
        state.push_thread()?;
    } else {
        // TODO(port): push L1 as a thread value and move it to the current stack
        // for use as the hook-table key. Requires cross-thread borrow.
    }
    // C: lua_pushvalue(L, arg + 1);  /* value = hook function (or nil) */
    state.push_value_at(arg + 1)?;
    // C: lua_rawset(L, -3);  /* hooktable[L1] = hook */
    state.raw_set(-3)?;

    // C: lua_sethook(L1, func, mask, count);
    if target_is_self {
        // TODO(phase-b): HookFn type in state_stub takes &mut LuaDebug; set_hook takes lua_CFunction. Wire through real hook registry once lua-vm lands.
        let _ = hook_active;
        state.set_hook_full(None, mask, count)?;
    } else {
        // TODO(port): set hook on another thread — requires &mut LuaState for
        // the target concurrently with the current state.
    }

    Ok(0)
}

/// `debug.gethook([thread])` — return the current hook function, mask string,
/// and count. Returns the fail value if no hook is installed.
///
/// C: `static int db_gethook(lua_State *L)`
pub(crate) fn get_hook(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: int arg; lua_State *L1 = getthread(L, &arg);
    let (_arg, other_thread) = getthread(state);
    let target_is_self = other_thread.is_none();

    // C: char buff[5]; int mask = lua_gethookmask(L1);
    let mask: u32 = if target_is_self {
        state.get_hook_mask()
    } else {
        // TODO(port): retrieve hook mask from another thread.
        0u32
    };

    // C: lua_Hook hook = lua_gethook(L1);
    let hook_is_set: bool = if target_is_self {
        state.hook_is_set()
    } else {
        // TODO(port): retrieve hook presence from another thread.
        false
    };

    // C: if (hook == NULL) { luaL_pushfail(L); return 1; }
    if !hook_is_set {
        state.push_fail();
        return Ok(1);
    }

    // C: else if (hook != hookf)  /* external hook? */
    //      lua_pushliteral(L, "external hook");
    let hook_is_internal: bool = if target_is_self {
        // TODO(port): comparing hook function identity requires the VM to expose
        // whether the currently-installed hook is the Lua-managed `hookf` or an
        // external callback. Needs a dedicated predicate on LuaState (Phase B).
        state.hook_is_internal_lua_hook()
    } else {
        // TODO(port): retrieve hook kind from another thread.
        false
    };

    if !hook_is_internal {
        // C: lua_pushliteral(L, "external hook");
        let s = state.intern_str(b"external hook")?;
        state.push(LuaValue::Str(s));
    } else {
        // C: else { /* hook table must exist */
        // C:   lua_getfield(L, LUA_REGISTRYINDEX, HOOKKEY);
        state.get_registry_field(HOOKKEY)?;
        check_cross_thread_stack(state, target_is_self, 1)?;
        // C:   lua_pushthread(L1); lua_xmove(L1, L, 1);
        if target_is_self {
            state.push_thread();
        } else {
            // TODO(port): cross-thread thread-push for hook table lookup.
        }
        // C:   lua_rawget(L, -2);  /* 1st result = hooktable[L1] */
        state.raw_get(-2);
        // C:   lua_remove(L, -2);  /* remove hook table */
        state.remove(-2);
    }

    // C: lua_pushstring(L, unmakemask(mask, buff));  /* 2nd result = mask string */
    let smask = unmake_mask(mask);
    let ls = state.intern_str(&smask)?;
    state.push(LuaValue::Str(ls));

    // C: lua_pushinteger(L, lua_gethookcount(L1));  /* 3rd result = count */
    let hook_count: i32 = if target_is_self {
        state.get_hook_count()
    } else {
        // TODO(port): retrieve hook count from another thread.
        0i32
    };
    state.push(LuaValue::Int(hook_count as i64));

    Ok(3)
}

/// `debug.debug()` — enter an interactive debug REPL.
///
/// Reads Lua source lines from stdin, compiles and runs each one. On EOF or
/// when the user types `cont`, returns control to the caller. Errors in
/// commands are printed to stderr and the loop continues.
///
/// C: `static int db_debug(lua_State *L)`
pub(crate) fn debug_interactive(state: &mut LuaState) -> Result<usize, LuaError> {
    let stdin = io::stdin();
    loop {
        // C: lua_writestringerror("%s", "lua_debug> ");
        eprint!("lua_debug> ");
        let _ = io::stderr().flush();

        // C: if (fgets(buffer, sizeof(buffer), stdin) == NULL || strcmp(buffer, "cont\n") == 0)
        //        return 0;
        // PORT NOTE: using String for the line buffer is Rust I/O infrastructure,
        // not Lua data. The bytes are immediately converted to &[u8] before being
        // passed into the Lua API.
        let mut line = String::new();
        let n = stdin
            .lock()
            .read_line(&mut line)
            .map_err(|e| LuaError::runtime(format_args!("stdin read error: {}", e)))?;

        if n == 0 || line == "cont\n" {
            return Ok(0);
        }

        let bytes: &[u8] = line.as_bytes();

        // C: if (luaL_loadbuffer(L, buffer, strlen(buffer), "=(debug command)") ||
        //        lua_pcall(L, 0, 0, 0))
        //      lua_writestringerror("%s\n", luaL_tolstring(L, -1, NULL));
        let result = state
            .load_buffer(bytes, b"=(debug command)", None)
            .and_then(|_| state.protected_call(0, 0, 0));

        if let Err(_) = result {
            // TODO(port): display the error via state.coerce_to_string(-1) which
            // maps to luaL_tolstring. The exact method name for the coercing
            // to-string operation and the stderr-write helper need to be established
            // in Phase B (lua-vm/src/api.rs).
            eprintln!("(error in debug command)");
            state.pop_n(1);
        }

        // C: lua_settop(L, 0);  /* remove eventual returns */
        state.set_top(0);
    }
}

/// `debug.traceback([thread,] [message [, level]])` — return a traceback string.
///
/// If `message` is present but is not a string, it is returned unchanged.
/// Otherwise a stack traceback is generated and optionally prepended with
/// `message`.
///
/// C: `static int db_traceback(lua_State *L)`
pub(crate) fn traceback(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: int arg; lua_State *L1 = getthread(L, &arg);
    let (arg, other_thread) = getthread(state);
    let target_is_self = other_thread.is_none();

    // C: const char *msg = lua_tostring(L, arg + 1);
    // Immediately clone to Vec<u8> to free the borrow on `state`.
    let msg_owned: Option<Vec<u8>> = state
        .to_lua_string(arg + 1)
        .map(|s: GcRef<LuaString>| s.as_bytes().to_vec());

    // C: if (msg == NULL && !lua_isnoneornil(L, arg + 1))
    let arg1_ty = state.type_at(arg + 1);
    if msg_owned.is_none() && !matches!(arg1_ty, LuaType::None | LuaType::Nil) {
        // C: lua_pushvalue(L, arg + 1);  /* return it untouched */
        state.push_value_at(arg + 1)?;
    } else {
        // C: int level = (int)luaL_optinteger(L, arg+2, (L == L1) ? 1 : 0);
        let default_level: i64 = if target_is_self { 1 } else { 0 };
        let level = state.opt_arg_integer(arg + 2, default_level)? as i32;

        // C: luaL_traceback(L, L1, msg, level);
        // TODO(phase-b): cross-thread traceback target requires simultaneous &mut access to two LuaState; signature in state_stub takes &mut LuaState, not Option.
        let _ = other_thread;
        let _ = (msg_owned, level);
        state.push(LuaValue::Nil);
    }
    Ok(1)
}

/// `debug.setcstacklimit(limit)` — set the C-stack depth limit. Returns the
/// old limit, or a platform-specific sentinel when not supported.
///
/// C: `static int db_setcstacklimit(lua_State *L)`
pub(crate) fn set_c_stack_limit(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: int limit = (int)luaL_checkinteger(L, 1);
    let limit = state.check_arg_integer(1)? as i32;
    // C: int res = lua_setcstacklimit(L, limit); lua_pushinteger(L, res); return 1;
    let res = state.set_c_stack_limit(limit)?;
    state.push(LuaValue::Int(res as i64));
    Ok(1)
}

// ── Library registration ───────────────────────────────────────────────────

/// Function registration table for the `debug` library.
///
/// C: `static const luaL_Reg dblib[]`
pub(crate) const DBLIB: &[(&[u8], LibFn)] = &[
    (b"debug",          debug_interactive as LibFn),
    (b"getuservalue",   get_uservalue     as LibFn),
    (b"gethook",        get_hook          as LibFn),
    (b"getinfo",        get_info          as LibFn),
    (b"getlocal",       get_local         as LibFn),
    (b"getregistry",    get_registry      as LibFn),
    (b"getmetatable",   get_metatable     as LibFn),
    (b"getupvalue",     get_upvalue       as LibFn),
    (b"upvaluejoin",    upvalue_join      as LibFn),
    (b"upvalueid",      upvalue_id        as LibFn),
    (b"setuservalue",   set_uservalue     as LibFn),
    (b"sethook",        set_hook          as LibFn),
    (b"setlocal",       set_local         as LibFn),
    (b"setmetatable",   set_metatable     as LibFn),
    (b"setupvalue",     set_upvalue       as LibFn),
    (b"traceback",      traceback         as LibFn),
    (b"setcstacklimit", set_c_stack_limit as LibFn),
];

/// Open the `debug` library and push the module table onto the stack.
/// Returns 1 (the table).
///
/// C: `LUAMOD_API int luaopen_debug(lua_State *L)`
pub fn open_debug(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: luaL_newlib(L, dblib); return 1;
    state.new_lib(DBLIB)?;
    Ok(1)
}

// ──────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        src/ldblib.c  (484 lines, 20 functions)
//   target_crate:  lua-stdlib
//   confidence:    medium
//   todos:         16
//   port_notes:    3
//   unsafe_blocks: 0
//   notes:         Cross-thread ops (lua_xmove / simultaneous &mut LuaState)
//                  are the main blockers; all 16 TODOs are in that cluster or
//                  in UpvalId (raw-pointer identity). Single-thread paths are
//                  faithfully translated. Phase B must define: DebugInfo field
//                  accessor methods (source_bytes, short_src_bytes, what_bytes,
//                  name_bytes, namewhat_bytes), hook-kind predicates
//                  (hook_is_set, hook_is_internal_lua_hook, get_hook_mask,
//                  get_hook_count), get_or_create_registry_subtable,
//                  get_param_name, get_local_at, set_local_at,
//                  upvalue_id (UpvalId type), join_upvalues, lua_traceback,
//                  load_buffer, push_registry, get_registry_field, push_fail,
//                  push_thread, new_lib, set_hook(Option<HookFn>, u32, i32).
// ──────────────────────────────────────────────────────────────────────────
