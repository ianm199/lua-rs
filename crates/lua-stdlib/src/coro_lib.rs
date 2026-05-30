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

use lua_types::{
    error::LuaError,
    value::LuaValue,
    LuaType,
    LuaStatus,
    gc::GcRef,
};
use crate::state_stub::{LuaState, LuaStateStubExt as _, lua_CFunction, upvalue_index};

// ── Coroutine status codes ────────────────────────────────────────────────────


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
const STAT_NAMES: [&[u8]; 4] = [b"running", b"dead", b"suspended", b"normal"];

// ── Registration table ────────────────────────────────────────────────────────

/// Registration table for the `coroutine` standard library.
///
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
fn aux_status(state: &mut LuaState, co: &GcRef<lua_types::value::LuaThread>) -> i32 {
    let co_id = co.id;
    let entry_rc = {
        let g = state.global();
        if co_id == g.current_thread_id {
            return COS_RUN;
        }
        if co_id == g.main_thread_id {
            return COS_NORM;
        }
        match g.threads.get(&co_id) {
            Some(e) => e.state.clone(),
            None => return COS_DEAD,
        }
    };
    let co_state = match entry_rc.try_borrow() {
        Ok(state) => state,
        Err(_) => {
            // Nested resumes can hold a mutable borrow of a parent coroutine.
            // In that case, the safest fallback is to report the target as
            // "normal" (active but not suspended/dead), which matches the
            // common nested-resume status for the parent thread.
            return COS_NORM;
        }
    };
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
/// Phase E-3 adds cross-thread open-upvalue mirroring around the resume
/// boundary: before yielding control, the parent's open-upvalue values
/// are snapshotted into `GlobalState::cross_thread_upvals` so the
/// coroutine body can read and write them through
/// `LuaState::upvalue_get` / `upvalue_set`. On resume return, the
/// (possibly mutated) cache entries are flushed back into the parent's
/// stack. This is the alternative to a stack-refactor that would let
/// the parent's `LuaState` be reached through `Rc<RefCell<_>>` while it
/// is held by `&mut` further up the call stack.
///
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

    let parent_open_upval_slots: Vec<(u64, lua_vm::state::StackIdx)> = state
        .openupval
        .iter()
        .filter_map(|uv| match &*uv.slot() {
            lua_types::UpValState::Open { thread_id, idx } => {
                Some((*thread_id as u64, *idx))
            }
            lua_types::UpValState::Closed(_) => None,
        })
        .collect();
    {
        let mut g = state.global_mut();
        for (tid, idx) in &parent_open_upval_slots {
            let val = state.get_at(*idx);
            g.cross_thread_upvals.insert((*tid, *idx), val);
        }
    }

    push_parent_gc_snapshot(state);

    let (status, results_or_err): (LuaStatus, Vec<LuaValue>) = {
        let mut co_state = match entry_rc.try_borrow_mut() {
            Ok(b) => b,
            Err(_) => {
                pop_parent_gc_snapshot(state);
                let mut g = state.global_mut();
                for (tid, idx) in &parent_open_upval_slots {
                    g.cross_thread_upvals.remove(&(*tid, *idx));
                }
                drop(g);
                push_lit_or_nil(state, b"cannot resume non-suspended coroutine");
                return -1;
            }
        };
        if co_state.check_stack(narg + 1).is_err() {
            drop(co_state);
            pop_parent_gc_snapshot(state);
            let mut g = state.global_mut();
            for (tid, idx) in &parent_open_upval_slots {
                g.cross_thread_upvals.remove(&(*tid, *idx));
            }
            drop(g);
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
            co_top - count
        };
        co_state.set_top(lua_vm::state::StackIdx(new_co_top.max(0) as u32));
        (status, vals)
    };

    // Pop the parent stack snapshot — the coroutine has yielded or returned.
    pop_parent_gc_snapshot(state);

    {
        let mut g = state.global_mut();
        let mut flush: Vec<(lua_vm::state::StackIdx, LuaValue)> = Vec::new();
        for (tid, idx) in &parent_open_upval_slots {
            if let Some(v) = g.cross_thread_upvals.remove(&(*tid, *idx)) {
                flush.push((*idx, v));
            }
        }
        drop(g);
        for (idx, v) in flush {
            state.set_at(idx, v);
        }
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

fn push_parent_gc_snapshot(state: &mut LuaState) {
    let top = state.top_idx();
    let stack_snapshot: Vec<LuaValue> = (0..top.0)
        .map(|i| state.get_at(lua_vm::state::StackIdx(i)))
        .collect();
    let open_upval_snapshot = state.openupval.clone();
    let mut g = state.global_mut();
    g.suspended_parent_stacks.push(stack_snapshot);
    g.suspended_parent_open_upvals.push(open_upval_snapshot);
}

fn pop_parent_gc_snapshot(state: &mut LuaState) {
    let mut g = state.global_mut();
    g.suspended_parent_open_upvals.pop();
    g.suspended_parent_stacks.pop();
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
pub fn co_resume(state: &mut LuaState) -> Result<usize, LuaError> {
    let co = get_co(state)?;
    // PORT NOTE: lua_gettop returns the argument count; -1 excludes the coroutine
    // itself which sits at index 1.
    let narg = state.get_top() - 1;
    let r = aux_resume(state, co, narg);
    if r < 0 {
        // A sandbox budget trip is uncatchable: re-raise into the caller frame
        // instead of returning `false, msg`, so code cannot keep a runaway
        // coroutine alive by resuming it in a loop.
        if state.sandbox_aborting() {
            let top = state.get_top();
            let err_val = state.value_at(top);
            return Err(LuaError::from_value(err_val));
        }
        state.push(LuaValue::Bool(false));
        state.insert(-2)?;
        Ok(2)
    } else {
        state.push(LuaValue::Bool(true));
        state.insert(-(r + 1))?;
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
    let r = aux_resume(state, co.clone(), narg);
    if r < 0 {
        let top = state.get_top();
        let mut err_val = state.value_at(top);
        if aux_status(state, &co) == COS_DEAD {
            let old_err = state.pop();
            let nclose = close_suspended_or_dead(state, co)?;
            err_val = if nclose >= 2 {
                let top = state.get_top();
                state.value_at(top)
            } else {
                old_err
            };
            state.pop_n(nclose);
        }
        Err(LuaError::from_value(err_val))
    } else {
        Ok(r as usize)
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
pub fn co_status(state: &mut LuaState) -> Result<usize, LuaError> {
    let co = get_co(state)?;
    let idx = aux_status(state, &co) as usize;
    let name: &[u8] = STAT_NAMES[idx];
    let interned = state.intern_str(name)?;
    state.push(LuaValue::Str(interned));
    Ok(1)
}

/// `coroutine.isyieldable([co])` — test whether a coroutine (default: current)
/// is in a yieldable state.
///
pub fn co_isyieldable(state: &mut LuaState) -> Result<usize, LuaError> {
    let is_yieldable = if matches!(state.type_at(1), LuaType::None) {
        state.is_yieldable()
    } else {
        let co = get_co(state)?;
        let co_id = co.id;
        let (is_main, is_current) = {
            let g = state.global();
            (co_id == g.main_thread_id, co_id == g.current_thread_id)
        };
        if is_main {
            false
        } else if is_current {
            state.is_yieldable()
        } else {
            let entry_rc = {
                let g = state.global();
                g.threads
                    .get(&co_id)
                    .expect("thread value carries an id that must resolve in GlobalState::threads")
                    .state
                    .clone()
            };
            let target_is_yieldable = match entry_rc.try_borrow() {
                Ok(b) => b.is_yieldable(),
                Err(_) => false,
            };
            target_is_yieldable
        }
    };
    state.push(LuaValue::Bool(is_yieldable));
    Ok(1)
}

/// `coroutine.running()` — return the current coroutine plus a boolean.
///
/// The boolean is `true` when the current coroutine is the main thread.
///
pub fn co_running(state: &mut LuaState) -> Result<usize, LuaError> {
    // TODO(port): push_thread pushes a Thread value for the current LuaState and
    // returns true iff it is the main thread; Phase B wire-up needed.
    let is_main = state.push_thread()?;
    state.push(LuaValue::Bool(is_main));
    Ok(2)
}

/// `coroutine.close(co)` — close a dead or suspended coroutine.
///
/// Closes a coroutine, running any pending to-be-closed variables via
/// `__close` and resetting its status. Valid only when the target is
/// suspended (`Yield`) or dead (`Ok` with no active frames).
/// Calling on a running or normal coroutine raises an error.
///
pub fn co_close(state: &mut LuaState) -> Result<usize, LuaError> {
    lua_vm::state::inc_c_stack(state)?;
    let result = (|| {
        let co = get_co(state)?;
        let status = aux_status(state, &co);
        match status {
            COS_DEAD | COS_YIELD => close_suspended_or_dead(state, co),
            _ => {
                let name = if status == COS_RUN { "running" } else { "normal" };
                Err(LuaError::runtime(format_args!(
                    "cannot close a {} coroutine",
                    name
                )))
            }
        }
    })();
    state.n_ccalls -= 1;
    result
}

/// Performs the actual close for a suspended or dead coroutine.
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

    let parent_open_upval_slots: Vec<(u64, lua_vm::state::StackIdx)> = state
        .openupval
        .iter()
        .filter_map(|uv| match &*uv.slot() {
            lua_types::UpValState::Open { thread_id, idx } => {
                Some((*thread_id as u64, *idx))
            }
            lua_types::UpValState::Closed(_) => None,
        })
        .collect();
    {
        let mut g = state.global_mut();
        for (tid, idx) in &parent_open_upval_slots {
            let val = state.get_at(*idx);
            g.cross_thread_upvals.insert((*tid, *idx), val);
        }
    }

    push_parent_gc_snapshot(state);

    let (status, err_value): (i32, Option<LuaValue>) = {
        let mut co_state = entry_rc.borrow_mut();
        co_state.global_mut().current_thread_id = co_id;
        co_state.n_ccalls = caller_c_calls;
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

    pop_parent_gc_snapshot(state);

    {
        let mut g = state.global_mut();
        let mut flush: Vec<(lua_vm::state::StackIdx, LuaValue)> = Vec::new();
        for (tid, idx) in &parent_open_upval_slots {
            if let Some(v) = g.cross_thread_upvals.remove(&(*tid, *idx)) {
                flush.push((*idx, v));
            }
        }
        drop(g);
        for (idx, v) in flush {
            state.set_at(idx, v);
        }
    }

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
pub fn open_coroutine(state: &mut LuaState) -> Result<usize, LuaError> {
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
//   notes:         All coroutine execution primitives (resume, yield, xmove,
//                  new_thread, close_thread) are Phase E stubs that panic.
//                  Argument-checking / result-packaging logic is faithfully
//                  translated so Phase E can drop in real implementations.
//                  The CO_FUNCS table type references lua_CFunction which is
//                  resolved in Phase B.  LuaState / GcRef<LuaState> / LuaStatus
//                  imports are all deferred to Phase B.
// ──────────────────────────────────────────────────────────────────────────────
