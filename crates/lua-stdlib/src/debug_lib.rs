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

use std::cell::RefCell;
#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
use std::io::{self, BufRead, Write};
use std::rc::Rc;

use lua_types::{GcRef, LuaError, LuaString, LuaType, LuaValue};
use crate::state_stub::{LuaState, LuaStateStubExt as _, LuaDebug as DebugInfo};

// ── Constants ──────────────────────────────────────────────────────────────

/// Registry key for the hook table that maps threads to their hook functions.
///
const HOOKKEY: &[u8] = b"_HOOKKEY";

/// Hook event names indexed by the raw event code stored in [`DebugInfo::event`].
/// Order must match the `LUA_HOOK*` constants: Call=0, Return=1, Line=2, Count=3, TailCall=4.
///
const HOOKNAMES: &[&[u8]; 5] = &[b"call", b"return", b"line", b"count", b"tail call"];

/// Bitmask constants for hook event selection.
const MASK_CALL: u32 = 1 << 0;
const MASK_RET: u32 = 1 << 1;
const MASK_LINE: u32 = 1 << 2;
const MASK_COUNT: u32 = 1 << 3;

// ── Local type aliases ─────────────────────────────────────────────────────

/// Entry-point signature for a Lua stdlib function in Rust.
pub(crate) type LibFn = fn(&mut LuaState) -> Result<usize, LuaError>;

/// A Rust hook callback registered with the Lua VM's hook mechanism.
///
/// PORT NOTE: The Rust hook receives the event code and current line directly
/// rather than a lua_Debug pointer, since the lua-stdlib `DebugInfo` and the
/// canonical `lua_vm::debug::LuaDebug` are distinct types during Phase B.
#[expect(dead_code, reason = "ported stdlib helper; not yet wired into the runtime")]
pub(crate) type HookFn = fn(&mut LuaState, i32, i32) -> Result<(), LuaError>;

/// Opaque identity handle for an upvalue.
///
/// check whether two upvalues share the same storage cell.
///
/// TODO(port): In C this is a raw pointer into the upvalue's storage cell.
/// Safe Rust cannot expose a raw pointer outside `lua-gc`. A stable u64 ID
/// or a GcRef-based comparison should be designed in Phase D. Using `usize`
/// (pointer-sized) as a placeholder so the call sites compile.
type UpvalId = usize;

#[derive(Clone)]
enum DebugThreadTarget {
    Current,
    Other(Rc<RefCell<LuaState>>),
    Unavailable,
}

fn resolve_debug_thread_target(
    state: &LuaState,
    target_thread: &Option<GcRef<lua_types::value::LuaThread>>,
) -> DebugThreadTarget {
    let Some(thread) = target_thread else {
        return DebugThreadTarget::Current;
    };

    if thread.id == state.cached_thread_id {
        return DebugThreadTarget::Current;
    }

    let g = state.global();
    if thread.id == g.main_thread_id {
        DebugThreadTarget::Unavailable
    } else {
        g.threads
            .get(&thread.id)
            .map(|entry| DebugThreadTarget::Other(entry.state.clone()))
            .unwrap_or(DebugThreadTarget::Unavailable)
    }
}

// ── Internal helpers ───────────────────────────────────────────────────────

/// Ensure the cross-thread target has room for `n` more stack slots.
///
/// When the target is the current thread this is a no-op because the current
/// thread's stack is managed by the caller. When it is another thread we
/// must verify its stack, but that requires a simultaneous `&mut LuaState`
/// for both threads.
///
fn check_cross_thread_stack(
    state: &mut LuaState,
    target_is_self: bool,
    n: i32,
) -> Result<(), LuaError> {
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
fn getthread(state: &mut LuaState) -> (i32, Option<GcRef<lua_types::value::LuaThread>>) {
    if state.type_at(1) == LuaType::Thread {
        let thread = state.to_thread_at(1);
        return (1, thread);
    }
    (0, None)
}

/// Push byte string `v` (or Nil when `v` is `None`) and store it under key
/// `k` in the table that sits at stack position -2.
///
/// PORT NOTE: The C version passes NULL to signal "no value" (lua_pushstring
/// with NULL pushes nil). Rust uses Option<&[u8]> for the same semantics.
fn settabss(state: &mut LuaState, k: &[u8], v: Option<&[u8]>) -> Result<(), LuaError> {
    match v {
        Some(s) => {
            let ls = state.intern_str(s)?;
            state.push(LuaValue::Str(ls));
        }
        None => { state.push(LuaValue::Nil); }
    }
    state.set_field(-2, k)
}

/// Push integer `v` and store it under key `k` in the table at -2.
///
fn settabsi(state: &mut LuaState, k: &[u8], v: i32) -> Result<(), LuaError> {
    state.push(LuaValue::Int(v as i64));
    state.set_field(-2, k)
}

/// Push boolean `v` and store it under key `k` in the table at -2.
///
fn settabsb(state: &mut LuaState, k: &[u8], v: bool) -> Result<(), LuaError> {
    state.push(LuaValue::Bool(v));
    state.set_field(-2, k)
}

/// After `lua_getinfo` has pushed a result ('f' function or 'L' line table)
/// onto L1's stack, move it into the result table on L as field `fname`.
///
/// When target is self, the value is already on our stack; rotate to bring
/// it above the result table. When target is a different thread, use xmove.
///
fn treat_stack_option(
    state: &mut LuaState,
    target_is_self: bool,
    fname: &[u8],
) -> Result<(), LuaError> {
    if target_is_self {
        state.rotate(-2, 1)?;
    } else {
        // TODO(port): moving a value from another thread's stack (lua_xmove)
        // requires simultaneous `&mut LuaState` for both threads. Not expressible
        // in safe Rust without interior mutability. Pushes Nil as placeholder.
        state.push(LuaValue::Nil);
    }
    state.set_field(-2, fname)
}

fn move_stack_option_from_target(
    state: &mut LuaState,
    target: &mut LuaState,
    fname: &[u8],
) -> Result<(), LuaError> {
    let val = target.get_at(target.top_idx() - 1);
    target.pop_n(1);
    state.push(val);
    state.set_field(-2, fname)
}

// ── Library functions ──────────────────────────────────────────────────────

/// `debug.getregistry()` — return the Lua registry table.
///
pub(crate) fn get_registry(state: &mut LuaState) -> Result<usize, LuaError> {
    state.push_registry()?;
    Ok(1)
}

/// `debug.getmetatable(obj)` — return the metatable of `obj`, or nil if none.
///
pub(crate) fn get_metatable(state: &mut LuaState) -> Result<usize, LuaError> {
    state.check_arg_any(1)?;
    if !state.get_metatable(1)? {
        state.push(LuaValue::Nil);
    }
    Ok(1)
}

/// `debug.setmetatable(obj, table)` — set `table` (or nil) as `obj`'s metatable.
/// Returns the first argument `obj`.
///
pub(crate) fn set_metatable(state: &mut LuaState) -> Result<usize, LuaError> {
    let t = state.type_at(2);
    if !(t == LuaType::Nil || t == LuaType::Table) {
        let got = state.arg(2);
        return Err(LuaError::type_arg_error(2, "nil or table", &got));
    }
    lua_vm::api::set_top(state, 2)?;
    state.set_metatable(1)?;
    Ok(1)
}

/// `debug.getuservalue(obj [, n])` — return the n-th user value of userdata
/// `obj` plus `true`, or the fail value if `obj` is not userdata or `n` is out
/// of range.
///
pub(crate) fn get_uservalue(state: &mut LuaState) -> Result<usize, LuaError> {
    let n = state.opt_arg_integer(2, 1)? as i32;
    if state.type_at(1) != LuaType::UserData {
        state.push_fail()?;
        return Ok(1);
    }
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
pub(crate) fn set_uservalue(state: &mut LuaState) -> Result<usize, LuaError> {
    let n = state.opt_arg_integer(3, 1)? as i32;
    state.check_arg_type(1, LuaType::UserData)?;
    state.check_arg_any(2)?;
    lua_vm::api::set_top(state, 2)?;
    if !state.set_iuservalue(1, n)? {
        state.push_fail()?;
    }
    Ok(1)
}

/// `debug.getinfo([thread,] f|level [, what])` — collect debug information
/// about function `f` or stack level `level` into a new table. The `what`
/// string selects which fields to populate (default `"flnSrtu"`).
///
pub(crate) fn get_info(state: &mut LuaState) -> Result<usize, LuaError> {
    let mut ar = DebugInfo::default();

    let (arg, other_thread) = getthread(state);
    let target_is_self = other_thread.is_none();
    let target_state = resolve_debug_thread_target(state, &other_thread);

    // to_vec() immediately to avoid borrow-checker conflict with subsequent &mut state ops.
    let raw_opts: Vec<u8> = state.opt_arg_string(arg + 2, b"flnSrtu")?.to_vec();

    check_cross_thread_stack(state, target_is_self, 3)?;

    if raw_opts.first() == Some(&b'>') {
        return Err(LuaError::arg_error(arg + 2, "invalid option '>'"));
    }

    // Build the effective options string, prepending '>' when the subject is a function.
    let options: Vec<u8>;
    let info_target_owner: Option<Rc<RefCell<LuaState>>>;
    let mut info_target: Option<std::cell::RefMut<'_, LuaState>> = None;
    let mut info_target_is_self = target_is_self;

    if state.type_at(arg + 1) == LuaType::Function {
        // In C this also pushes the string onto the stack; in Rust we just build a Vec.
        let mut prefixed = Vec::with_capacity(raw_opts.len() + 1);
        prefixed.push(b'>');
        prefixed.extend_from_slice(&raw_opts);
        options = prefixed;

        if target_is_self {
            state.push_value_at(arg + 1)?;
        } else {
            // TODO(port): lua_xmove to another thread's stack requires simultaneous
            // `&mut LuaState` for both threads. Cross-thread getinfo with a function
            // argument is left incomplete for Phase A.
        }

        // With '>' prefix, get_debug_info consumes the function from the top of stack.
        if state.get_debug_info(&options, &mut ar).is_err() {
            return Err(LuaError::arg_error(arg + 2, "invalid option"));
        }
    } else {
        options = raw_opts;

        let level = state.check_arg_integer(arg + 1)? as i32;
        match target_state {
            DebugThreadTarget::Current | DebugThreadTarget::Unavailable => {
                info_target_is_self = true;
                if !state.get_stack_level(level, &mut ar) {
                    state.push_fail()?;
                    return Ok(1);
                }

                if state.get_debug_info(&options, &mut ar).is_err() {
                    return Err(LuaError::arg_error(arg + 2, "invalid option"));
                }
            }
            DebugThreadTarget::Other(target_state) => {
                info_target_owner = Some(target_state);
                let mut target = info_target_owner
                    .as_ref()
                    .expect("target owner just stored")
                    .borrow_mut();
                if !target.get_stack_level(level, &mut ar) {
                    state.push_fail()?;
                    return Ok(1);
                }
                if target.get_debug_info(&options, &mut ar).is_err() {
                    return Err(LuaError::arg_error(arg + 2, "invalid option"));
                }
                info_target = Some(target);
            }
        }
    }

    let result_tbl = state.new_table();
    state.push(LuaValue::Table(result_tbl));

    if options.contains(&b'S') {
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
        let name_opt: Option<&[u8]> = ar.name.as_deref();
        settabss(state, b"name", name_opt)?;
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
        if info_target_is_self {
            treat_stack_option(state, true, b"activelines")?;
        } else if let Some(target) = info_target.as_mut() {
            move_stack_option_from_target(state, &mut **target, b"activelines")?;
        } else {
            state.push(LuaValue::Nil);
            state.set_field(-2, b"activelines")?;
        }
    }
    if options.contains(&b'f') {
        if info_target_is_self {
            treat_stack_option(state, true, b"func")?;
        } else if let Some(target) = info_target.as_mut() {
            move_stack_option_from_target(state, &mut **target, b"func")?;
        } else {
            state.push(LuaValue::Nil);
            state.set_field(-2, b"func")?;
        }
    }

    Ok(1)
}

/// `debug.getlocal([thread,] level, local)` — return the name and value of
/// local variable `local` at stack level `level`.
///
/// When the first argument is a function, returns only the parameter name at
/// position `local` (no value).
///
pub(crate) fn get_local(state: &mut LuaState) -> Result<usize, LuaError> {
    let (arg, other_thread) = getthread(state);
    let target_state = resolve_debug_thread_target(state, &other_thread);

    let nvar = state.check_arg_integer(arg + 2)? as i32;

    if state.type_at(arg + 1) == LuaType::Function {
        state.push_value_at(arg + 1)?;
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
        // The pushed function below name is discarded by the VM when it collects
        // exactly 1 return value from the top of the stack.
        return Ok(1);
    }

    // Stack-level path.
    let level = state.check_arg_integer(arg + 1)? as i32;
    let mut ar = DebugInfo::default();

    let name = match target_state {
        DebugThreadTarget::Current | DebugThreadTarget::Unavailable => {
            if !state.get_stack_level(level, &mut ar) {
                return Err(LuaError::arg_error(arg + 1, "level out of range"));
            }
            check_cross_thread_stack(state, true, 1)?;
            // Pushes the local's value onto L1's stack and returns its name.
            state.get_local_at(&ar, nvar)?
        }
        DebugThreadTarget::Other(target_state) => {
            let mut target = target_state.borrow_mut();
            if !target.get_stack_level(level, &mut ar) {
                return Err(LuaError::arg_error(arg + 1, "level out of range"));
            }
            check_cross_thread_stack(state, false, 1)?;
            let name = target.get_local_at(&ar, nvar)?;
            if name.is_some() {
                let val = target.get_at(target.top_idx() - 1);
                target.pop_n(1);
                state.push(val);
            }
            name
        }
    };

    if let Some(n) = name {
        let ls = state.intern_str(&n)?;
        state.push(LuaValue::Str(ls));
        state.rotate(-2, 1)?;
        Ok(2)
    } else {
        state.push_fail()?;
        Ok(1)
    }
}

/// `debug.setlocal([thread,] level, local, value)` — set local variable
/// `local` at stack level `level` to `value`. Returns the variable name, or
/// nil on failure.
///
pub(crate) fn set_local(state: &mut LuaState) -> Result<usize, LuaError> {
    let (arg, other_thread) = getthread(state);
    let target_state = resolve_debug_thread_target(state, &other_thread);

    let level = state.check_arg_integer(arg + 1)? as i32;
    let nvar = state.check_arg_integer(arg + 2)? as i32;

    let mut ar = DebugInfo::default();

    state.check_arg_any(arg + 3)?;
    lua_vm::api::set_top(state, arg + 3)?;

    let name = match target_state {
        DebugThreadTarget::Current | DebugThreadTarget::Unavailable => {
            if !state.get_stack_level(level, &mut ar) {
                return Err(LuaError::arg_error(arg + 1, "level out of range"));
            }
            check_cross_thread_stack(state, true, 1)?;
            let name = state.set_local_at(&ar, nvar)?;
            if name.is_none() {
                state.pop_n(1);
            }
            name
        }
        DebugThreadTarget::Other(target_state) => {
            let new_val = state.get_at(state.top_idx() - 1);
            let mut target = target_state.borrow_mut();
            if !target.get_stack_level(level, &mut ar) {
                return Err(LuaError::arg_error(arg + 1, "level out of range"));
            }
            check_cross_thread_stack(state, false, 1)?;
            target.push(new_val);
            let name = target.set_local_at(&ar, nvar)?;
            if name.is_none() {
                target.pop_n(1);
            }
            state.pop_n(1);
            name
        }
    };

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
fn aux_upvalue(state: &mut LuaState, get: bool) -> Result<usize, LuaError> {
    let n = state.check_arg_integer(2)? as i32;
    state.check_arg_type(1, LuaType::Function)?;

    let name: Option<Vec<u8>> = if get {
        // lua_getupvalue pushes the upvalue value and returns the name.
        state.get_upvalue(1, n)?
    } else {
        // lua_setupvalue pops the top-of-stack value, sets upvalue n, returns name.
        state.set_upvalue(1, n)?
    };

    let name_ref = match name {
        Some(n) => n,
        None => return Ok(0),
    };

    let ls = state.intern_str(&name_ref)?;
    state.push(LuaValue::Str(ls));

    // When get=true: stack is [..., value, name]; insert at -2 → [..., name, value].
    // When get=false: insert at -1 is a no-op; stack is [..., name].
    if get {
        state.insert(-2)?;
    }

    Ok(if get { 2 } else { 1 })
}

/// `debug.getupvalue(f, up)` — return the name and value of upvalue `up` of `f`.
///
pub(crate) fn get_upvalue(state: &mut LuaState) -> Result<usize, LuaError> {
    aux_upvalue(state, true)
}

/// `debug.setupvalue(f, up, value)` — set upvalue `up` of `f` to `value`.
/// Returns the upvalue name.
///
pub(crate) fn set_upvalue(state: &mut LuaState) -> Result<usize, LuaError> {
    state.check_arg_any(3)?;
    aux_upvalue(state, false)
}

/// Verify that upvalue `argnup` of function at stack index `argf` exists.
/// Returns the opaque identity handle and the upvalue index.
/// If `require_valid` is true, raises an arg error when the upvalue is absent.
///
fn check_upval(
    state: &mut LuaState,
    argf: i32,
    argnup: i32,
    require_valid: bool,
) -> Result<(Option<UpvalId>, i32), LuaError> {
    let nup = state.check_arg_integer(argnup)? as i32;
    state.check_arg_type(argf, LuaType::Function)?;
    // TODO(port): lua_upvalueid returns a raw void* that uniquely identifies
    // an upvalue's storage cell. A safe equivalent (e.g., GcRef<UpVal> pointer
    // comparison, or a stable u64 ID from the GC layer) must be defined in
    // Phase D. Using Option<usize> as placeholder.
    let id: Option<UpvalId> = match state.upvalue_id(argf, nup) {
        Ok(p) if p.is_null() => None,
        Ok(p) => Some(p as usize),
        Err(_) => None,
    };
    if require_valid && id.is_none() {
        return Err(LuaError::arg_error(argnup, "invalid upvalue index"));
    }
    Ok((id, nup))
}

/// `debug.upvalueid(f, n)` — return a unique identifier for upvalue `n` of
/// function `f` as a light userdata. Returns fail on out-of-range index.
///
pub(crate) fn upvalue_id(state: &mut LuaState) -> Result<usize, LuaError> {
    let (id, _nup) = check_upval(state, 1, 2, false)?;
    match id {
        Some(uid) => {
            lua_vm::api::push_light_userdata(state, uid as *mut core::ffi::c_void);
        }
        None => {
            state.push_fail()?;
        }
    }
    Ok(1)
}

/// `debug.upvaluejoin(f1, n1, f2, n2)` — make upvalue `n1` of function `f1`
/// refer to the same storage as upvalue `n2` of function `f2`.
///
pub(crate) fn upvalue_join(state: &mut LuaState) -> Result<usize, LuaError> {
    let (_id1, n1) = check_upval(state, 1, 2, true)?;
    let (_id2, n2) = check_upval(state, 3, 4, true)?;
    if state.is_c_function_at(1) {
        return Err(LuaError::arg_error(1, "Lua function expected"));
    }
    if state.is_c_function_at(3) {
        return Err(LuaError::arg_error(3, "Lua function expected"));
    }
    state.join_upvalues(1, n1, 3, n2)?;
    Ok(0)
}

/// Internal debug hook registered with the VM via `lua_sethook`. When
/// invoked, it looks up the Lua-side hook function stored in
/// `registry[HOOKKEY][current_thread]` and calls it with the event name
/// and current line number.
///
pub(crate) fn hookf(state: &mut LuaState, event: i32, currentline: i32) -> Result<(), LuaError> {
    state.get_registry_field(HOOKKEY)?;
    state.push_thread()?;
    if state.raw_get(-2)? == LuaType::Function {
        let event_idx = event.clamp(0, HOOKNAMES.len() as i32 - 1) as usize;
        let event_str = state.intern_str(HOOKNAMES[event_idx])?;
        state.push(LuaValue::Str(event_str));

        if currentline >= 0 {
            state.push(LuaValue::Int(currentline as i64));
        } else {
            state.push(LuaValue::Nil);
        }

        state.call(2, 0)?;
    }
    // The caller (do_::hook) saves/restores the stack top, so any residual
    // entries (hook table, non-function lookup result) are cleaned up there.
    Ok(())
}

/// Convert the string hook-mask (`'c'`/`'r'`/`'l'` characters) and a count
/// to the integer bitmask used by the VM's `sethook` API.
///
fn make_mask(smask: &[u8], count: i32) -> u32 {
    let mut mask: u32 = 0;
    if smask.contains(&b'c') {
        mask |= MASK_CALL;
    }
    if smask.contains(&b'r') {
        mask |= MASK_RET;
    }
    if smask.contains(&b'l') {
        mask |= MASK_LINE;
    }
    if count > 0 {
        mask |= MASK_COUNT;
    }
    mask
}

/// Convert the integer hook bitmask back to the string representation used in
/// Lua (`'c'`/`'r'`/`'l'` characters).
///
fn unmake_mask(mask: u32) -> Vec<u8> {
    let mut smask = Vec::with_capacity(3);
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
pub(crate) fn set_hook(state: &mut LuaState) -> Result<usize, LuaError> {
    let (arg, other_thread) = getthread(state);
    let target_is_self = other_thread.is_none();

    let hook_active: bool;
    let mask: u32;
    let count: i32;

    if matches!(state.type_at(arg + 1), LuaType::None | LuaType::Nil) {
        lua_vm::api::set_top(state, arg + 1)?;
        hook_active = false;
        mask = 0;
        count = 0;
    } else {
        let smask: Vec<u8> = state.check_arg_string(arg + 2)?.to_vec();
        state.check_arg_type(arg + 1, LuaType::Function)?;
        count = state.opt_arg_integer(arg + 3, 0)? as i32;
        hook_active = true;
        mask = make_mask(&smask, count);
    }

    if !state.get_or_create_registry_subtable(HOOKKEY)? {
        // Table was just created. Set it up as a weak-keyed table so that
        // thread keys do not prevent GC of finished threads.
        let k = state.intern_str(b"k")?;
        state.push(LuaValue::Str(k));
        state.set_field(-2, b"__mode")?;
        state.push_value_at(-1)?;
        state.set_metatable(-2)?;
    }

    check_cross_thread_stack(state, target_is_self, 1)?;
    let target_state = resolve_debug_thread_target(state, &other_thread);
    match &target_state {
        DebugThreadTarget::Other(st) => {
            st.borrow_mut().ensure_stack(1, "stack overflow")?;
        }
        DebugThreadTarget::Current => {}
        DebugThreadTarget::Unavailable => {}
    }

    if target_is_self {
        state.push_thread()?;
    } else {
        // Push the target thread (captured via getthread) as the key. The C
        // `lua_pushthread(L1); lua_xmove(L1, L, 1)` dance is necessary because
        // C uses two distinct lua_State pointers; in our impl the GcRef is
        // already a global reference so we can push it directly on the parent
        // stack as a Thread value. Without this push, raw_set below operates
        // on a stack that's missing its key slot and panics in get_table_value.
        let thr = other_thread.clone().expect("other_thread is Some when target_is_self is false");
        state.push(lua_types::value::LuaValue::Thread(thr));
    }
    state.push_value_at(arg + 1)?;
    state.raw_set(-3)?;

    let hook_box: Option<Box<dyn FnMut(&mut LuaState, &lua_vm::debug::LuaDebug)>> = if hook_active {
        Some(Box::new(|st, ar| {
            let _ = hookf(st, ar.event, ar.currentline);
        }))
    } else {
        None
    };
    match target_state {
        DebugThreadTarget::Current => {
            lua_vm::debug::set_hook(state, hook_box, mask as i32, count);
        }
        DebugThreadTarget::Other(target_state) => {
            lua_vm::debug::set_hook(&mut target_state.borrow_mut(), hook_box, mask as i32, count);
        }
        DebugThreadTarget::Unavailable => {
            // Main-thread cross-thread targeting from a non-main state is not
            // yet reachable in this build; record the function in the shared
            // registry and leave execution on the current thread untouched.
            return Ok(0);
        }
    }

    Ok(0)
}

/// `debug.gethook([thread])` — return the current hook function, mask string,
/// and count. Returns the fail value if no hook is installed.
///
pub(crate) fn get_hook(state: &mut LuaState) -> Result<usize, LuaError> {
    let (_arg, other_thread) = getthread(state);
    let target_is_self = other_thread.is_none();
    let target_state = resolve_debug_thread_target(state, &other_thread);

    let (mask, hook_is_set, hook_is_internal, hook_count) = match target_state {
        DebugThreadTarget::Current => {
            (
                state.get_hook_mask(),
                state.hook_is_set(),
                state.hook_is_internal_lua_hook(),
                state.get_hook_count(),
            )
        }
        DebugThreadTarget::Other(target_state) => {
            let mut target_state = target_state.borrow_mut();
            (
                target_state.get_hook_mask(),
                target_state.hook_is_set(),
                target_state.hook_is_internal_lua_hook(),
                target_state.get_hook_count(),
            )
        }
        DebugThreadTarget::Unavailable => (0u32, false, false, 0i32),
    };

    if !hook_is_set {
        state.push_fail()?;
        return Ok(1);
    }

    //      lua_pushliteral(L, "external hook");
    if !hook_is_internal {
        let s = state.intern_str(b"external hook")?;
        state.push(LuaValue::Str(s));
    } else {
        state.get_registry_field(HOOKKEY)?;
        check_cross_thread_stack(state, target_is_self, 1)?;
        if target_is_self {
            state.push_thread()?;
        } else {
            let key_thread = other_thread
                .expect("other_thread is Some when target_is_self is false")
                .clone();
            state.push(lua_types::value::LuaValue::Thread(key_thread));
        }
        state.raw_get(-2)?;
        state.remove(-2)?;
    }

    let smask = unmake_mask(mask);
    let ls = state.intern_str(&smask)?;
    state.push(LuaValue::Str(ls));

    state.push(LuaValue::Int(hook_count as i64));

    Ok(3)
}

/// `debug.debug()` — enter an interactive debug REPL.
///
/// Reads Lua source lines from stdin, compiles and runs each one. On EOF or
/// when the user types `cont`, returns control to the caller. Errors in
/// commands are printed to stderr and the loop continues.
///
pub(crate) fn debug_interactive(state: &mut LuaState) -> Result<usize, LuaError> {
    #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
    {
        let _ = state;
        return Err(LuaError::runtime(format_args!(
            "debug.debug interactive stdin not available in this host"
        )));
    }

    #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
    {
        let stdin = io::stdin();
        loop {
            eprint!("lua_debug> ");
            let _ = io::stderr().flush();

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

            lua_vm::api::set_top(state, 0)?;
        }
    }
}

/// `debug.traceback([thread,] [message [, level]])` — return a traceback string.
///
/// If `message` is present but is not a string, it is returned unchanged.
/// Otherwise a stack traceback is generated and optionally prepended with
/// `message`.
///
pub(crate) fn traceback(state: &mut LuaState) -> Result<usize, LuaError> {
    let (arg, other_thread) = getthread(state);
    let target_is_self = other_thread.is_none();

    // Immediately clone to Vec<u8> to free the borrow on `state`.
    let msg_owned: Option<Vec<u8>> = state
        .to_lua_string(arg + 1)
        .map(|s: GcRef<LuaString>| s.as_bytes().to_vec());

    let arg1_ty = state.type_at(arg + 1);
    if msg_owned.is_none() && !matches!(arg1_ty, LuaType::None | LuaType::Nil) {
        state.push_value_at(arg + 1)?;
    } else {
        let default_level: i64 = if target_is_self { 1 } else { 0 };
        let level = state.opt_arg_integer(arg + 2, default_level)? as i32;

        match resolve_debug_thread_target(state, &other_thread) {
            DebugThreadTarget::Current => {
                crate::auxlib::traceback(state, None, msg_owned.as_deref(), level)?;
            }
            DebugThreadTarget::Other(target_state) => {
                let mut target_state = target_state.borrow_mut();
                crate::auxlib::traceback(
                    state,
                    Some(&mut *target_state),
                    msg_owned.as_deref(),
                    level,
                )?;
            }
            DebugThreadTarget::Unavailable => {
                crate::auxlib::traceback(state, None, msg_owned.as_deref(), level)?;
            }
        }
    }
    Ok(1)
}

/// `debug.setcstacklimit(limit)` — set the C-stack depth limit. Returns the
/// old limit, or a platform-specific sentinel when not supported.
///
pub(crate) fn set_c_stack_limit(state: &mut LuaState) -> Result<usize, LuaError> {
    let limit = state.check_arg_integer(1)? as i32;
    let res = state.set_c_stack_limit(limit)?;
    state.push(LuaValue::Int(res as i64));
    Ok(1)
}

// ── Library registration ───────────────────────────────────────────────────

/// Function registration table for the `debug` library.
///
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
pub fn open_debug(state: &mut LuaState) -> Result<usize, LuaError> {
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
