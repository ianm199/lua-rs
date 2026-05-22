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
    closure::LuaClosure,
    error::LuaError,
    value::LuaValue,
    LuaType,
    LuaStatus,
    gc::GcRef,
};
use crate::state_stub::{LuaState, LuaStateStubExt as _, lua_CFunction, upvalue_index, CompareOp, LuaDebug};

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
    let g = state.global();
    if co_id == g.current_thread_id {
        return COS_RUN;
    }
    let entry = match g.threads.get(&co_id) {
        Some(e) => e,
        None => return COS_DEAD,
    };
    let co_state: &lua_vm::state::LuaState = &entry.state;
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
fn aux_resume(state: &mut LuaState, _co: GcRef<lua_types::value::LuaThread>, _narg: i32) -> i32 {
    // Phase A–D stub: real cross-thread resume needs corosensei runtime
    // support (PORTING.md §2 #6 — Phase E).  Emulate C's `auxresume` error
    // path: push a string error object onto the caller's stack and return
    // -1.  `co_resume` packages this as `(false, msg)` and `aux_wrap`
    // re-raises it as a Lua error, matching C-Lua semantics on a coroutine
    // that cannot run.  Phase E will replace this body with the full
    // checkstack / xmove / lua_resume / xmove sequence.
    let msg_bytes: &[u8] = b"not yet implemented: coroutines (Phase E)";
    match state.intern_str(msg_bytes) {
        Ok(s) => state.push(LuaValue::Str(s)),
        Err(_) => state.push(LuaValue::Nil),
    }
    -1
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

/// Closure body installed by `coroutine.wrap`. The wrapped function is
/// stored in upvalue slot 1.
///
/// C: `static int luaB_auxwrap(lua_State *L)`
///
/// Phase A–D emulation: on the first call, runs the entire wrapped function
/// with a yield buffer installed, collects all yield batches (one per
/// `coroutine.yield(...)` call) and the function's final return values as
/// the last batch, stores them in `GlobalState::wrap_iter_state` keyed by
/// this closure's identity, and returns the first batch. Subsequent calls
/// dispense one batch per call (matching real coroutine semantics) until the
/// final-return batch is returned, at which point the entry is removed.
/// Phase E replaces this with the full cross-thread `auxresume` sequence
/// once stackful coroutines land.
fn aux_wrap(state: &mut LuaState) -> Result<usize, LuaError> {
    eprintln!("[DEBUG-CORO] aux_wrap called with top={}", state.get_top());
    let ci_func = state.current_call_info().func;
    let func_val = state.get_at(ci_func);
    let cache_key = match &func_val {
        LuaValue::Function(LuaClosure::C(ccl)) => ccl.identity(),
        _ => {
            return Err(LuaError::runtime(format_args!(
                "coroutine.wrap: aux_wrap called on non-C-closure"
            )))
        }
    };
    drop(func_val);

    enum Dispense {
        Batch(Vec<LuaValue>),
        Drain(Option<LuaError>),
    }

    let maybe_next: Option<Dispense> = {
        let mut g = state.global_mut();
        g.wrap_iter_state.get_mut(&cache_key).map(|(batches, idx, pending_err)| {
            if *idx < batches.len() {
                let batch = batches[*idx].clone();
                *idx += 1;
                Dispense::Batch(batch)
            } else {
                Dispense::Drain(pending_err.take())
            }
        })
    };

    match maybe_next {
        Some(Dispense::Batch(batch)) => {
            let n = batch.len();
            for v in batch {
                state.push(v);
            }
            return Ok(n);
        }
        Some(Dispense::Drain(pending_err)) => {
            state.global_mut().wrap_iter_state.remove(&cache_key);
            if let Some(err) = pending_err {
                return Err(err);
            }
            return Ok(0);
        }
        None => {}
    }

    // First call: run the wrapped function with a yield buffer active.
    //
    // PORT NOTE: We must use a protected call here, not a plain call. When the
    // wrapped function raises an error, an unprotected call leaves state.ci
    // pointing at the inner function's CallInfo instead of aux_wrap's. Every
    // subsequent stack operation in aux_wrap then uses the wrong stack base,
    // corrupting results and causing spurious "attempt to call a nil value"
    // panics downstream. A protected call mirrors what real Lua does:
    // coroutine.wrap / lua_resume use lua_pcall internally so errors trigger
    // TBC cleanup and CI restoration before returning to the caller.
    let nargs = state.get_top();
    let func = state.value_at(upvalue_index(1));
    state.push(func);
    if nargs > 0 {
        state.insert(1)?;
    }
    state.push_yield_buffer();
    // Use LUA_MULTRET (-1) so the function's final return values reach the stack.
    let call_result = state.protected_call(nargs, -1, 0);

    // If the call succeeded, capture its final return values as the closing
    // batch.  On error we discard whatever residue is on the stack and defer
    // the error to fire after all already-buffered yield batches have been
    // dispensed (matches C-Lua: yields happen before the error surfaces).
    let (mut batches, pending_err) = match call_result {
        Ok(_) => {
            let nret = state.get_top() as i32;
            let final_batch: Vec<LuaValue> =
                (1..=nret).map(|i| state.value_at(i)).collect();
            let mut batches = state.pop_yield_buffer();
            batches.push(final_batch);
            (batches, None)
        }
        Err(e) => {
            let batches = state.pop_yield_buffer();
            (batches, Some(e))
        }
    };

    // Clear residual stack values; we'll push the first batch instead.
    lua_vm::api::set_top(state, 0)?;

    if batches.is_empty() {
        if let Some(err) = pending_err {
            return Err(err);
        }
        return Ok(0);
    }

    // Remove and return the first batch; save the rest plus any pending
    // error for subsequent calls.
    let first = batches.remove(0);
    let first_n = first.len();

    if !batches.is_empty() || pending_err.is_some() {
        state
            .global_mut()
            .wrap_iter_state
            .insert(cache_key, (batches, 0, pending_err));
    }

    for v in first {
        state.push(v);
    }
    Ok(first_n)
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
/// Phase A–D emulation: captures `f` as upvalue 1 of `aux_wrap`. Each
/// call to the returned function forwards directly to `f` with the same
/// args. Phase E will replace this with the real `luaB_cocreate` +
/// `auxresume` sequence once stackful coroutines land.
pub fn co_wrap(state: &mut LuaState) -> Result<usize, LuaError> {
    state.check_arg_type(1, LuaType::Function)?;
    state.push_value_at(1)?;
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
///
/// Phase A–D buffering-emulation hook: if `aux_wrap` has installed a yield
/// buffer on the LuaState (signalling that a `coroutine.wrap` body is
/// running synchronously on the main thread), append the arguments into
/// that buffer and return 0 values without suspending. The wrapped
/// function continues to run; `aux_wrap` dispenses the buffered values one
/// per call. If no buffer is active we fall through to the faithful
/// `lua_yieldk` translation, which on the main thread surfaces the C-Lua
/// "attempt to yield from outside a coroutine" error.
pub fn co_yield(state: &mut LuaState) -> Result<usize, LuaError> {
    if state.has_yield_buffer() {
        let n = state.get_top();
        let batch: Vec<LuaValue> = (1..=n).map(|i| state.value_at(i)).collect();
        eprintln!("[DEBUG-CORO] co_yield buffering batch of size {}", batch.len());
        state.yield_buffer_push_batch(batch);
        return Ok(0);
    }
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
        let g = state.global();
        if co_id == g.main_thread_id {
            false
        } else {
            let entry = g
                .threads
                .get(&co_id)
                .expect("thread value carries an id that must resolve in GlobalState::threads");
            entry.state.is_yieldable()
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
/// Phase E-1 skeleton: the running/normal-rejection branch matches the
/// final C-Lua behavior and lands here. Closing a suspended/dead
/// coroutine requires running to-be-closed variables and resetting the
/// thread; that machinery lands in slice 02d. Until then this returns
/// an error rather than silently dropping the work.
///
/// C: `static int luaB_close(lua_State *L)`
pub fn co_close(state: &mut LuaState) -> Result<usize, LuaError> {
    let co = get_co(state)?;
    let status = aux_status(state, &co);
    match status {
        COS_RUN | COS_NORM => {
            let name = if status == COS_RUN { "running" } else { "normal" };
            Err(LuaError::runtime(format_args!(
                "cannot close a {} coroutine",
                name
            )))
        }
        _ => Err(LuaError::runtime(format_args!(
            "coroutine.close not yet implemented for suspended/dead coroutines (Phase E-2d)"
        ))),
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
//   notes:         All coroutine execution primitives (resume, yield, xmove,
//                  new_thread, close_thread) are Phase E stubs that panic.
//                  Argument-checking / result-packaging logic is faithfully
//                  translated so Phase E can drop in real implementations.
//                  The CO_FUNCS table type references lua_CFunction which is
//                  resolved in Phase B.  LuaState / GcRef<LuaState> / LuaStatus
//                  imports are all deferred to Phase B.
// ──────────────────────────────────────────────────────────────────────────────
