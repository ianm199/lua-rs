//! Coroutine library — port of `lcorolib.c`.
//!
//! Provides the `coroutine.*` standard-library table: `create`, `resume`,
//! `running`, `status`, `wrap`, `yield`, `isyieldable`, and `close`.
//!
//! # Phase A–D stub notice
//!
//! Every function that requires actual coroutine execution (`resume`, `yield`,
//! cross-thread `xmove`, `new_thread`, `close_thread`) is **unimplemented** and
//! will panic at runtime.  The argument-checking and result-packaging logic is
//! translated faithfully so that Phase E can drop in the real implementations
//! without restructuring.  Phase E wires real stackful coroutines via
//! `corosensei`.  See PORTING.md §2 #6.
//!
//! Translated from: `reference/lua-5.4.7/src/lcorolib.c` (210 lines, 12 functions)
//! Target crate: `lua-stdlib`

// TODO(port): LuaState, GcRef<LuaState>, LuaStatus, and related types live in
// lua-vm / lua-types; all unresolved imports will be fixed in Phase B.
use lua_types::{
    error::LuaError,
    value::LuaValue,
    LuaType,
    LuaStatus,
    LuaClosure,
    UpValState,
    gc::GcRef,
};
use crate::state_stub::{LuaState, LuaStateStubExt as _, lua_CFunction, upvalue_index};

// ── Coroutine status codes ────────────────────────────────────────────────────

// C: #define COS_RUN   0
// C: #define COS_DEAD  1
// C: #define COS_YIELD 2
// C: #define COS_NORM  3

/// Coroutine is the currently running thread.
const COS_RUN: i32 = 0;

/// Coroutine has finished execution or encountered an error.
const COS_DEAD: i32 = 1;

/// Coroutine is suspended — either yielded or not yet started.
const COS_YIELD: i32 = 2;

/// Coroutine is normal — it resumed another coroutine and is waiting.
const COS_NORM: i32 = 3;

/// Human-readable status strings indexed by the `COS_*` constants above.
/// Pushed onto the Lua stack as byte strings.
///
/// C: `static const char *const statname[] = {"running","dead","suspended","normal"};`
const STAT_NAMES: [&[u8]; 4] = [b"running", b"dead", b"suspended", b"normal"];

// ── Registration table ────────────────────────────────────────────────────────

/// Registration table for the `coroutine` standard library.
///
/// C: `static const luaL_Reg co_funcs[]`
///
/// Each entry is `(name_bytes, function_pointer)`. Phase B resolves
/// `lua_CFunction` to the canonical type alias from `lua-types`.
pub const CO_FUNCS: &[(&[u8], lua_CFunction)] = &[
    (b"create",      co_create),
    (b"resume",      co_resume),
    (b"running",     co_running),
    (b"status",      co_status),
    (b"wrap",        co_wrap),
    (b"yield",       co_yield),
    (b"isyieldable", co_isyieldable),
    (b"close",       co_close),
];

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Retrieves the coroutine thread at stack index 1, raising a type error if
/// the argument is absent or not a thread.
///
/// C: `static lua_State *getco(lua_State *L)`
fn get_co(state: &mut LuaState) -> Result<GcRef<lua_types::value::LuaThread>, LuaError> {
    let co = state.to_thread(1);
    if co.is_none() {
        let got = state.arg(1);
        return Err(LuaError::type_arg_error(1, "thread", &got));
    }
    Ok(co.expect("checked above"))
}

/// Returns one of the `COS_*` status codes describing `co` relative to the
/// calling thread `state`. Mirrors `auxstatus` in `lcorolib.c` exactly,
/// reading the target coroutine's `status`, call-frame depth, and stack
/// top through `GlobalState::threads`.
///
/// The main thread (id 0) is never stored in the registry, so a value
/// pointing at it is always "running" when it is the current thread.
/// Phase E-1 cannot resume coroutines, so any registry-resident thread
/// is either suspended (initial state, function still on stack) or dead
/// (empty stack).
///
/// C: `static int auxstatus(lua_State *L, lua_State *co)`
fn aux_status(state: &mut LuaState, co: &GcRef<lua_types::value::LuaThread>) -> i32 {
    let co_id = co.id;
    let entry_rc = {
        let g = state.global();
        if co_id == g.current_thread_id {
            return COS_RUN;
        }
        match g.threads.get(&co_id) {
            Some(e) => e.state.clone(),
            None => return COS_DEAD,
        }
    };
    let co_state = entry_rc.borrow();
    let raw_status = co_state.status;
    if raw_status == LuaStatus::Yield as u8 {
        return COS_YIELD;
    }
    if raw_status != LuaStatus::Ok as u8 {
        return COS_DEAD;
    }
    let has_frames = co_state.ci.as_usize() > 0;
    if has_frames {
        return COS_NORM;
    }
    let ci_func = co_state.call_info[0].func.0;
    let top = co_state.top.0;
    let lua_gettop = top as i64 - ci_func as i64 - 1;
    if lua_gettop == 0 {
        COS_DEAD
    } else {
        COS_YIELD
    }
}

/// Transfers `narg` arguments from `state` to `co`, resumes the coroutine,
/// then transfers results (or error message) back to `state`.
///
/// Returns the number of result values (≥ 0) on success, or `-1` on error
/// with the error object left on top of `state`'s stack.
///
/// C: `static int auxresume(lua_State *L, lua_State *co, int narg)`
fn aux_resume(state: &mut LuaState, co: GcRef<lua_types::value::LuaThread>, narg: i32) -> i32 {
    let co_id = co.id;
    let entry_rc = {
        let g = state.global();
        match g.threads.get(&co_id) {
            Some(e) => e.state.clone(),
            None => {
                drop(g);
                push_lit_or_nil(state, b"cannot resume dead coroutine");
                return -1;
            }
        }
    };
    let parent_thread_id = state.global().current_thread_id;
    let top_before = state.get_top();
    if top_before < narg {
        push_lit_or_nil(state, b"not enough arguments to resume");
        return -1;
    }
    let first_arg_idx = top_before - narg + 1;
    let args: Vec<LuaValue> = (first_arg_idx..=top_before)
        .map(|i| state.value_at(i))
        .collect();
    lua_vm::api::set_top(state, (top_before - narg) as i32).ok();

    // Collect the open-upvalue indices of the coroutine's body closure so we
    // can snapshot the main thread's stack before the resume and write it back
    // afterward.  Only needed for the first resume (status == Ok); on later
    // resumes (Yield) the upvalues that matter have already been snapshotted or
    // closed by the first run.
    let open_upval_indices: Vec<u32> = {
        let co_state = entry_rc.borrow();
        // The body function was pushed at stack[1] by new_thread.
        if let LuaValue::Function(lua_types::closure::LuaClosure::Lua(ref lcl)) =
            co_state.get_at(lua_vm::state::StackIdx(1))
        {
            (0..lcl.upvals.len())
                .filter_map(|i| {
                    let uv = lcl.upval(i);
                    let slot = uv.slot().clone();
                    match slot {
                        lua_types::UpValState::Open { thread_id: 0, idx } => Some(idx.0),
                        _ => None,
                    }
                })
                .collect()
        } else {
            Vec::new()
        }
    };

    eprintln!("[DBG aux_resume] open_upval_indices={:?}", open_upval_indices);
    // Populate the parent-stack snapshot so upvalue_get/upvalue_set inside
    // the coroutine proxy through it rather than reading the coroutine's stack.
    for &idx in &open_upval_indices {
        let val = state.get_at(lua_vm::state::StackIdx(idx));
        eprintln!("[DBG aux_resume] snapshot idx={} val={:?}", idx, val);
        state.global_mut().parent_stack_snapshot.insert(idx, val);
    }

    let (status, results_or_err): (LuaStatus, Vec<LuaValue>) = {
        let mut co_state = entry_rc.borrow_mut();
        if co_state.check_stack(narg + 1).is_err() {
            drop(co_state);
            push_lit_or_nil(state, b"too many arguments to resume");
            return -1;
        }
        for v in args {
            co_state.push(v);
        }
        co_state.global_mut().current_thread_id = co_id;
        let mut nres: i32 = 0;
        let status = lua_vm::do_::lua_resume(&mut *co_state, Some(state), narg, &mut nres);
        co_state.global_mut().current_thread_id = parent_thread_id;
        let co_top = co_state.top_idx().0 as i32;
        let ci_func = co_state.current_call_info().func.0 as i32;
        let count = if status == LuaStatus::Ok || status == LuaStatus::Yield {
            nres
        } else {
            1
        };
        let start = co_top - count;
        let vals: Vec<LuaValue> = (start..co_top)
            .map(|i| co_state.get_at(lua_vm::state::StackIdx(i as u32)))
            .collect();
        let new_co_top = if status == LuaStatus::Ok || status == LuaStatus::Yield {
            (co_top - count).max(ci_func + 1)
        } else {
            ci_func + 1
        };
        co_state.set_top(lua_vm::state::StackIdx(new_co_top.max(0) as u32));
        (status, vals)
    };

    // Write back the snapshot values to the main thread's stack, then clear.
    // This propagates any writes the coroutine made to the parent's upvalues.
    let snapshot: Vec<(u32, LuaValue)> = state
        .global_mut()
        .parent_stack_snapshot
        .drain()
        .collect();
    for (idx, val) in snapshot {
        state.set_at(lua_vm::state::StackIdx(idx), val);
    }

    match status {
        LuaStatus::Ok | LuaStatus::Yield => {
            if state.check_stack(results_or_err.len() as i32 + 1).is_err() {
                push_lit_or_nil(state, b"too many results to resume");
                return -1;
            }
            let n = results_or_err.len();
            for v in results_or_err {
                state.push(v);
            }
            n as i32
        }
        _ => {
            for v in results_or_err {
                state.push(v);
            }
            -1
        }
    }
}

/// Helper: push a string literal or fall back to Nil on intern failure.
fn push_lit_or_nil(state: &mut LuaState, bytes: &[u8]) {
    match state.intern_str(bytes) {
        Ok(s) => state.push(LuaValue::Str(s)),
        Err(_) => state.push(LuaValue::Nil),
    }
}

// ── Public library functions ──────────────────────────────────────────────────

/// `coroutine.resume(co [, val1, ...])` — attempt to resume coroutine `co`.
///
/// On success pushes `true` followed by all values yielded or returned by `co`.
/// On failure pushes `false` followed by the error object.
///
/// C: `static int luaB_coresume(lua_State *L)`
pub fn co_resume(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: lua_State *co = getco(L);
    let co = get_co(state)?;
    // C: r = auxresume(L, co, lua_gettop(L) - 1);
    // PORT NOTE: lua_gettop returns the argument count; -1 excludes the coroutine
    // itself which sits at index 1.
    let narg = state.get_top() - 1;
    let r = aux_resume(state, co, narg);
    if r < 0 {
        // C: lua_pushboolean(L, 0); lua_insert(L, -2); return 2;
        state.push(LuaValue::Bool(false));
        state.insert(-2);
        Ok(2)
    } else {
        // C: lua_pushboolean(L, 1); lua_insert(L, -(r + 1)); return r + 1;
        state.push(LuaValue::Bool(true));
        state.insert(-(r + 1));
        Ok((r + 1) as usize)
    }
}

/// Closure body installed by `coroutine.wrap`. The wrapped coroutine
/// thread is stored in upvalue slot 1 as a `LuaValue::Thread`.
///
/// On call: forwards all args to `aux_resume` on the captured thread. On
/// success returns the yielded/returned values; on coroutine error raises
/// the error (matching `select(2, assert(resume(co, ...)))` semantics).
///
/// C: `static int luaB_auxwrap(lua_State *L)`
fn aux_wrap(state: &mut LuaState) -> Result<usize, LuaError> {
    let up = state.value_at(upvalue_index(1));
    let co = match up {
        LuaValue::Thread(t) => t,
        _ => {
            return Err(LuaError::runtime(format_args!(
                "coroutine.wrap: upvalue is not a thread"
            )))
        }
    };
    let narg = state.get_top();
    let r = aux_resume(state, co, narg);
    if r < 0 {
        let top = state.get_top();
        let err_val = state.value_at(top);
        Err(LuaError::from_value(err_val))
    } else {
        // DEBUG: show what aux_wrap returned
        let top = state.get_top();
        let result_type = if r == 0 { "nil(end)".to_string() } else { format!("{:?}", std::mem::discriminant(&state.value_at(top))) };
        eprintln!("[DBG aux_wrap] r={} top={} result={}", r, top, result_type);
        Ok(r as usize)
    }
}

/// Close all open upvalues of `val` (and transitively of any Lua closures
/// it refers to) by copying their current values from the parent thread's
/// stack into the `UpVal` struct.
///
/// When a closure is transferred to a new coroutine, its open upvalues
/// point into the parent thread's stack by `StackIdx`. The coroutine has
/// its own separate `stack` Vec, so reading an open upvalue from inside
/// the coroutine would access the wrong slot. Closing upvalues before
/// transfer converts `Open { idx }` → `Closed(current_value)`, making
/// the value self-contained and accessible across thread boundaries.
///
/// Recursion terminates because closing an upvalue changes it from
/// `Open` to `Closed`; any cycle (e.g. a self-referential `local function`)
/// is therefore visited at most twice before the second pass is a no-op.
fn close_open_upvals_for_coroutine(state: &mut LuaState, val: &LuaValue) {
    let cl = match val {
        LuaValue::Function(LuaClosure::Lua(cl)) => cl.clone(),
        _ => return,
    };
    for uv_cell in cl.upvals.iter() {
        let uv = uv_cell.borrow().clone();
        let idx_opt = match &*uv.slot() {
            UpValState::Open { idx, .. } => Some(*idx),
            UpValState::Closed(_) => None,
        };
        let idx = match idx_opt {
            Some(i) => i,
            None => continue,
        };
        let v = state.stack
            .get(idx.0 as usize)
            .map(|s| s.val.clone())
            .unwrap_or(LuaValue::Nil);
        *uv.state.borrow_mut() = UpValState::Closed(v.clone());
        state.openupval.retain(|u| !GcRef::ptr_eq(u, &uv));
        close_open_upvals_for_coroutine(state, &v);
    }
}

/// `coroutine.create(f)` — create a new coroutine that will run function `f`.
///
/// Pushes the new thread value and returns 1.
///
/// Phase E-1: allocates a real `LuaState` registered in
/// `GlobalState::threads`, with `f` staged on the new thread's stack so
/// `coroutine.status` reports `"suspended"`. The full `xmove` from the
/// caller's stack arrives in slice 02b; for this slice the body is
/// cloned via `value_at(1)`, which has the same net stack effect since
/// `lua_newthread` in C also leaves only the thread value on the
/// caller's stack.
///
/// C: `static int luaB_cocreate(lua_State *L)`
pub fn co_create(state: &mut LuaState) -> Result<usize, LuaError> {
    state.check_arg_type(1, LuaType::Function)?;
    let body = state.value_at(1);
    let _nl = state.new_thread(Some(body))?;
    Ok(1)
}

/// `coroutine.wrap(f)` — create a coroutine and return a resuming function.
///
/// The returned function, when called, resumes the coroutine as if by
/// `coroutine.resume`, but raises an error rather than returning `false`.
///
/// C: `static int luaB_cowrap(lua_State *L)`
///
/// Captures the new coroutine thread as upvalue 1 of `aux_wrap`.
pub fn co_wrap(state: &mut LuaState) -> Result<usize, LuaError> {
    co_create(state)?;
    state.push_cclosure(aux_wrap, 1)?;
    Ok(1)
}

/// `coroutine.yield([...])` — suspend the running coroutine.
///
/// All arguments are passed back as results of the corresponding `resume`.
///
/// C: `static int luaB_yield(lua_State *L)`
/// → `return lua_yield(L, lua_gettop(L));`
/// → `lua_yield(L,n)` is `lua_yieldk(L, n, 0, NULL)` (lua.h:316)
pub fn co_yield(state: &mut LuaState) -> Result<usize, LuaError> {
    let n = state.get_top();
    let r = lua_vm::do_::lua_yieldk(state, n, 0, None)?;
    Ok(r as usize)
}

/// `coroutine.status(co)` — return a string describing `co`'s current status.
///
/// Returns one of `"running"`, `"dead"`, `"suspended"`, or `"normal"`.
///
/// C: `static int luaB_costatus(lua_State *L)`
pub fn co_status(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: lua_State *co = getco(L);
    let co = get_co(state)?;
    // C: lua_pushstring(L, statname[auxstatus(L, co)]);
    let idx = aux_status(state, &co) as usize;
    let name: &[u8] = STAT_NAMES[idx];
    let interned = state.intern_str(name)?;
    state.push(LuaValue::Str(interned));
    Ok(1)
}

/// `coroutine.isyieldable([co])` — test whether a coroutine (default: current)
/// is in a yieldable state.
///
/// C: `static int luaB_yieldable(lua_State *L)`
pub fn co_isyieldable(state: &mut LuaState) -> Result<usize, LuaError> {
    let is_yieldable = if matches!(state.type_at(1), LuaType::None) {
        state.is_yieldable()
    } else {
        let co = get_co(state)?;
        let co_id = co.id;
        let entry_rc = {
            let g = state.global();
            if co_id == g.main_thread_id {
                None
            } else {
                Some(g
                    .threads
                    .get(&co_id)
                    .expect("thread value carries an id that must resolve in GlobalState::threads")
                    .state
                    .clone())
            }
        };
        match entry_rc {
            None => false,
            Some(rc) => rc.borrow().is_yieldable(),
        }
    };
    state.push(LuaValue::Bool(is_yieldable));
    Ok(1)
}

/// `coroutine.running()` — return the current coroutine plus a boolean.
///
/// The boolean is `true` when the current coroutine is the main thread.
///
/// C: `static int luaB_corunning(lua_State *L)`
pub fn co_running(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: int ismain = lua_pushthread(L);
    // TODO(port): push_thread pushes a Thread value for the current LuaState and
    // returns true iff it is the main thread; Phase B wire-up needed.
    let is_main = state.push_thread()?;
    // C: lua_pushboolean(L, ismain);
    state.push(LuaValue::Bool(is_main));
    Ok(2)
}

/// `coroutine.close(co)` — close a dead or suspended coroutine.
///
/// For a dead or suspended coroutine, runs all `__close` metamethods for
/// any to-be-closed variables remaining on the coroutine's stack (LIFO
/// order) and resets the thread to a dead state. Returns `true` on clean
/// completion, or `(false, errobj)` if a `__close` raised an error or the
/// coroutine was already dead with an uncaught error.
///
/// Calling on a running or normal coroutine raises an error.
///
/// C: `static int luaB_close(lua_State *L)`
pub fn co_close(state: &mut LuaState) -> Result<usize, LuaError> {
    let co = get_co(state)?;
    let status = aux_status(state, &co);
    match status {
        COS_DEAD | COS_YIELD => close_suspended_or_dead(state, co),
        _ => {
            let name = STAT_NAMES[status as usize];
            Err(LuaError::runtime(format_args!(
                "cannot close a {} coroutine",
                name.escape_ascii()
            )))
        }
    }
}

/// Performs the actual close for a suspended or dead coroutine.
///
/// Borrows the target coroutine's `LuaState`, swaps `current_thread_id`
/// into the closing thread (so any `__close` body sees the correct
/// running thread), invokes `lua_vm::state::close_thread` which runs the
/// tbc-list close loop, then swaps back.
///
/// On success pushes `true` and returns 1.
/// On error pushes `(false, errobj)`; the error object is moved from
/// the coroutine's stack — mirroring C-Lua's `lua_xmove(co, L, 1)` —
/// after the close path leaves it at slot 1 on the coroutine.
fn close_suspended_or_dead(
    state: &mut LuaState,
    co: GcRef<lua_types::value::LuaThread>,
) -> Result<usize, LuaError> {
    let co_id = co.id;
    let entry_rc_opt = {
        let g = state.global();
        g.threads.get(&co_id).map(|e| e.state.clone())
    };
    let entry_rc = match entry_rc_opt {
        Some(rc) => rc,
        None => {
            state.push(LuaValue::Bool(true));
            return Ok(1);
        }
    };
    let parent_thread_id = state.global().current_thread_id;
    let caller_c_calls = state.c_calls();

    let (status, err_value): (i32, Option<LuaValue>) = {
        let mut co_state = entry_rc.borrow_mut();
        co_state.global_mut().current_thread_id = co_id;
        co_state.nCcalls = caller_c_calls;
        let in_status = co_state.status as i32;
        let s = lua_vm::state::reset_thread(&mut *co_state, in_status);
        co_state.global_mut().current_thread_id = parent_thread_id;
        if s == LuaStatus::Ok as i32 {
            (s, None)
        } else {
            let top = co_state.top_idx().0;
            if top > 0 {
                let err = co_state.get_at(lua_vm::state::StackIdx(top - 1));
                co_state.set_top(lua_vm::state::StackIdx(top - 1));
                (s, Some(err))
            } else {
                (s, Some(LuaValue::Nil))
            }
        }
    };

    if status == LuaStatus::Ok as i32 {
        state.push(LuaValue::Bool(true));
        Ok(1)
    } else {
        state.push(LuaValue::Bool(false));
        if let Some(v) = err_value {
            state.push(v);
        } else {
            state.push(LuaValue::Nil);
        }
        Ok(2)
    }
}

// ── Module entry point ────────────────────────────────────────────────────────

/// Opens the `coroutine` standard library by pushing a new table containing
/// all `coroutine.*` functions.
///
/// C: `LUAMOD_API int luaopen_coroutine(lua_State *L)` — `LUAMOD_API` → `pub`.
pub fn open_coroutine(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: luaL_newlib(L, co_funcs);
    // TODO(port): state.new_lib(CO_FUNCS) creates a table from the registration
    // slice and leaves it on the stack; Phase B wire-up needed.
    state.new_lib(CO_FUNCS)?;
    Ok(1)
}

// ──────────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        src/lcorolib.c  (210 lines, 12 functions)
//   target_crate:  lua-stdlib
//   confidence:    medium
//   todos:         21
//   port_notes:    2
//   unsafe_blocks: 0
//   notes:         Phase E-2 (slices 02a/02b) wired real resume/yield. Phase
//                  E-4 (this slice, 02d) replaces the close_suspended/dead
//                  stub with a real lua_closethread invocation: it borrows
//                  the target coroutine, swaps current_thread_id, calls
//                  lua_vm::state::reset_thread (which now runs
//                  close_protected so __close metamethods fire on
//                  to-be-closed locals), and packages the resulting
//                  status as `(true)` or `(false, errobj)`.
// ──────────────────────────────────────────────────────────────────────────────
