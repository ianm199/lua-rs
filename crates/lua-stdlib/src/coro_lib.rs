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
/// calling thread `state`.
///
/// C: `static int auxstatus(lua_State *L, lua_State *co)`
fn aux_status(_state: &mut LuaState, _co: &GcRef<lua_types::value::LuaThread>) -> i32 {
    // TODO(phase-b): needs lua_vm cross-thread access to status, has_frames,
    // get_top, is_same_thread. Phase E wires real coroutines.
    todo!("phase-b: coroutine aux_status")
}

/// Transfers `narg` arguments from `state` to `co`, resumes the coroutine,
/// then transfers results (or error message) back to `state`.
///
/// Returns the number of result values (≥ 0) on success, or `-1` on error
/// with the error object left on top of `state`'s stack.
///
/// C: `static int auxresume(lua_State *L, lua_State *co, int narg)`
fn aux_resume(state: &mut LuaState, _co: GcRef<lua_types::value::LuaThread>, _narg: i32) -> i32 {
    // TODO(port): coroutine stub — the complete body requires all of:
    //
    //   1. if !lua_checkstack(co, narg) { push "too many arguments"; return -1; }
    //      → co.ensure_stack(narg) — grow co's stack for the incoming arguments.
    //
    //   2. lua_xmove(L, co, narg)
    //      → state.xmove(&co, narg) — transfer narg values from L's stack to co's.
    //
    //   3. status = lua_resume(co, L, narg, &nres)
    //      → co.resume(state, narg) — execute co until it yields, returns, or errors.
    //        Returns (LuaStatus, nres: i32).
    //
    //   4. On LUA_OK / LUA_YIELD success path:
    //      if !lua_checkstack(L, nres + 1) { lua_pop(co, nres); push "too many results"; return -1; }
    //      lua_xmove(co, L, nres) — move results back.
    //      return nres;
    //
    //   5. On error path:
    //      lua_xmove(co, L, 1) — move error message back.
    //      return -1;
    //
    // All cross-thread operations depend on Phase E / corosensei runtime support.
    let _ = state;
    todo!(
        "phase-e: coroutine_resume — stackful coroutines need corosensei (PORTING.md §2 #6)"
    );
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

/// Closure body installed by `coroutine.wrap`.  The wrapped coroutine is
/// stored in upvalue slot 1.
///
/// On error the message is augmented with location info (if a string), then
/// re-raised.  If the coroutine is in an error state (not simply suspended),
/// its to-be-closed variables are cleaned up before propagation.
///
/// C: `static int luaB_auxwrap(lua_State *L)`
fn aux_wrap(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: lua_State *co = lua_tothread(L, lua_upvalueindex(1));
    // TODO(port): coroutine stub — upvalue_index(1) gives the first C-closure
    // upvalue pseudo-index; to_thread reads GcRef<LuaState> from that slot.
    let co = match state.to_thread(upvalue_index(1)) {
        Some(c) => c,
        None => panic!("coroutine.wrap: upvalue 1 is not a thread (Phase A–D stub)"),
    };
    // C: int r = auxresume(L, co, lua_gettop(L));
    let narg = state.get_top();
    let r = aux_resume(state, co.clone(), narg);
    let _ = co;
    if r < 0 {
        // TODO(phase-b): needs cross-thread status, close_thread, xmove.
        todo!("phase-b: coroutine wrap error path")
    } else {
        Ok(r as usize)
    }
}

/// `coroutine.create(f)` — create a new coroutine that will run function `f`.
///
/// Pushes the new thread value and returns 1.
///
/// C: `static int luaB_cocreate(lua_State *L)`
pub fn co_create(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: luaL_checktype(L, 1, LUA_TFUNCTION);
    state.check_arg_type(1, LuaType::Function)?;
    // C: NL = lua_newthread(L);
    // TODO(port): coroutine stub — new_thread allocates a fresh LuaState coroutine
    // and pushes a Thread value for it; Phase E needed.
    let _nl = state.new_thread()?;
    // C: lua_pushvalue(L, 1);          /* move function to top */
    // C: lua_xmove(L, NL, 1);          /* move function from L to NL */
    // PORT NOTE: in C the function copy is pushed and then xmove pops it
    // off L's stack into NL's. The Phase E xmove is not yet wired, but the
    // net stack effect on L is "thread on top". Skip the push entirely so
    // co_wrap's push_cclosure captures the thread (not the function) as
    // upvalue 1. Phase E will restore the push + xmove pair.
    // TODO(port): coroutine stub — xmove transfers the function from L's stack to
    // NL's stack so it becomes the coroutine body; Phase E needed.
    Ok(1)
}

/// `coroutine.wrap(f)` — create a coroutine and return a resuming function.
///
/// The returned function, when called, resumes the coroutine as if by
/// `coroutine.resume`, but raises an error rather than returning `false`.
///
/// C: `static int luaB_cowrap(lua_State *L)`
pub fn co_wrap(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: luaB_cocreate(L);
    co_create(state)?;
    // C: lua_pushcclosure(L, luaB_auxwrap, 1);
    // TODO(port): push_cclosure(aux_wrap, 1) creates a C closure with the thread
    // (currently on top of the stack) as upvalue 1; Phase B wire-up needed.
    state.push_cclosure(aux_wrap, 1)?;
    Ok(1)
}

/// `coroutine.yield([...])` — suspend the running coroutine.
///
/// All arguments are passed back as results of the corresponding `resume`.
///
/// C: `static int luaB_yield(lua_State *L)`
pub fn co_yield(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: return lua_yield(L, lua_gettop(L));
    // TODO(port): coroutine stub — state.yield_(n) suspends the coroutine and
    // transfers n values back to the caller of resume; Phase E needed.
    let _nargs = state.get_top();
    panic!(
        "coroutine.yield is not yet implemented (Phase A–D stub; see PORTING.md §2 #6)"
    );
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
        let _co = get_co(state)?;
        // TODO(phase-b): needs cross-thread is_yieldable; Phase E.
        todo!("phase-b: cross-thread is_yieldable")
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
/// Runs the to-be-closed variable finalizers.  Returns `true` on success, or
/// `false` plus an error object on failure.  Raises an error if `co` is
/// running or normal.
///
/// C: `static int luaB_close(lua_State *L)`
pub fn co_close(state: &mut LuaState) -> Result<usize, LuaError> {
    let co = get_co(state)?;
    let status = aux_status(state, &co);
    match status {
        s if s == COS_DEAD || s == COS_YIELD => {
            // TODO(phase-b): needs cross-thread close_thread + xmove.
            todo!("phase-b: coroutine close")
        }
        _ => {
            let name = match status {
                COS_RUN => "running",
                COS_NORM => "normal",
                _ => "unknown",
            };
            Err(LuaError::runtime(format_args!(
                "cannot close a {} coroutine",
                name
            )))
        }
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
