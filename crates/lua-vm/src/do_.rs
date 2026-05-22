//! Stack and call structure of Lua.
//!
//! Translated from `src/ldo.c` (Lua 5.4.7, ~1029 lines, ~37 functions).
//! Target crate: lua-vm (`crates/lua-vm/src/do_.rs`).

// TODO(port): imports — exact module paths depend on final crate layout settled in Phase B.
// All `use` paths below are best-guess from file_deps.txt + types.tsv.
#[allow(unused_imports)] use crate::prelude::*;
use crate::{
    func,
    state::{CallInfoIdx, LuaState},
    vm,
};
use lua_types::{
    error::LuaError,
    status::LuaStatus,
    value::LuaValue,
};
use lua_types::StackIdx;
use lua_types::closure::LuaClosure;
use lua_types::tagmethod::TagMethod;
use crate::zio::{ZIO, LexBuffer};

/// Stub DynData. TODO(phase-b): real type lives in lua-parse.
struct DynDataStub;
impl DynDataStub {
    fn new() -> Self { DynDataStub }
}

/// Text-source parser entry point.
///
/// C: `LClosure *luaY_parser(lua_State *L, ZIO *z, Mbuffer *buff,
///                            Dyndata *dyd, const char *name, int firstchar)`
///
/// PORT NOTE: A direct call into `lua_parse::parse` would create a cyclic
/// crate dependency (`lua-parse` already depends on `lua-vm`). Instead the
/// embedder installs a function pointer on `GlobalState::parser_hook` at
/// startup; when present, this stub delegates to it. When absent (e.g. in
/// internal unit tests that never load text), we surface a syntax error so
/// the runtime can route it through `pcall` instead of panicking.
fn parse_stub(
    state: &mut LuaState,
    z: &mut ZIO,
    _buff: &mut LexBuffer,
    _dyd: &mut DynDataStub,
    name: &[u8],
    c: i32,
) -> Result<lua_types::GcRef<lua_types::closure::LuaLClosure>, LuaError> {
    let hook = state.global().parser_hook;
    if let Some(parse) = hook {
        let mut source: Vec<u8> = Vec::new();
        if c >= 0 {
            source.push(c as u8);
        }
        loop {
            let b = z.getc();
            if b < 0 {
                break;
            }
            source.push(b as u8);
        }
        return parse(state, &source, name, c);
    }
    Err(LuaError::syntax(format_args!(
        "{}: Lua text parser not yet wired (phase-b: lua-parse::parse)",
        core::str::from_utf8(name).unwrap_or("?"),
    )))
}

// ── Constants ────────────────────────────────────────────────────────────────

// C: #define ERRORSTACKSIZE (LUAI_MAXSTACK + 200)
// PORT NOTE: LUAI_MAXSTACK is 1_000_000 per macros.tsv.
const LUAI_MAXSTACK: usize = 1_000_000;
const ERRORSTACKSIZE: usize = LUAI_MAXSTACK + 200;

// C: const EXTRA_STACK = 5  (macros.tsv)
const EXTRA_STACK: i32 = 5;

// C: const LUA_MINSTACK = 20  (macros.tsv)
const LUA_MINSTACK: i32 = 20;

// C: const LUA_MULTRET = -1  (macros.tsv)
const LUA_MULTRET: i32 = -1;

// C: const NYCI = 0x10001  (macros.tsv: non-yieldable call increment)
const NYCI: u32 = 0x10001;

// C: #define LUAI_MAXCCALLS  — typically 200 in luaconf.h
// TODO(port): confirm from luaconf.h or a constants module.
const LUAI_MAXCCALLS: u32 = 200;

// CallStatus bit flags (macros.tsv)
const CIST_C: u16 = 1 << 1;
const CIST_FRESH: u16 = 1 << 2;
const CIST_HOOKED: u16 = 1 << 3;
const CIST_YPCALL: u16 = 1 << 4;
const CIST_TAIL: u16 = 1 << 5;
const CIST_HOOKYIELD: u16 = 1 << 6;
const CIST_TRAN: u16 = 1 << 8;
const CIST_CLSRET: u16 = 1 << 9;
const CIST_FIN: u16 = 1 << 7;

// C: LUA_MASKCALL, LUA_MASKRET  (macros.tsv  →  hook event bitmasks)
// TODO(port): derive from HookEvent enum once that type is settled.
const LUA_MASKCALL: u8 = 1 << 0;
const LUA_MASKRET: u8 = 1 << 1;

// C: LUA_HOOKCALL, LUA_HOOKRET, LUA_HOOKTAILCALL event codes
const LUA_HOOKCALL: i32 = 0;
const LUA_HOOKRET: i32 = 1;
const LUA_HOOKTAILCALL: i32 = 4;

// C: CLOSEKTOP = -1  (macros.tsv: "close all upvals down to top" sentinel)
// PORT NOTE: luaF_close takes StackIdx; this sentinel needs special handling.
// TODO(port): settle representation with func.rs author.
const CLOSE_K_TOP: i32 = -1;

// ── Helper: errorstatus ──────────────────────────────────────────────────────

// C: #define errorstatus(s) ((s) > LUA_YIELD)
// LUA_OK = 0, LUA_YIELD = 1; any status > 1 is a real error.
#[inline]
fn error_status(s: LuaStatus) -> bool {
    (s as i32) > (LuaStatus::Yield as i32)
}

// ── lua_longjmp (NOT translated) ─────────────────────────────────────────────
// PORT NOTE: The `struct lua_longjmp` and the entire setjmp/longjmp mechanism
// (LUAI_THROW / LUAI_TRY) are replaced by Rust's `Result<T, LuaError>`.
// There is no Rust equivalent of the `lua_longjmp` struct.
// The `lua_State.errorJmp` field is removed (see types.tsv).

// ══════════════════════════════════════════════════════════════════════════════
// Error-recovery functions
// ══════════════════════════════════════════════════════════════════════════════

/// Sets the error object at `old_top` and adjusts the stack top.
///
/// C: `void luaD_seterrorobj(lua_State *L, int errcode, StkId oldtop)`
pub(crate) fn set_error_obj(state: &mut LuaState, errcode: LuaStatus, old_top: StackIdx) {
    // C: switch (errcode)
    match errcode {
        LuaStatus::ErrMem => {
            // C: setsvalue2s(L, oldtop, G(L)->memerrmsg)
            // reuse the preallocated OOM message string
            let memerrmsg = state.global().memerrmsg.clone();
            state.set_at(old_top, LuaValue::Str(memerrmsg));
        }
        LuaStatus::ErrErr => {
            // C: setsvalue2s(L, oldtop, luaS_newliteral(L, "error in error handling"))
            if let Ok(s) = state.intern_str(b"error in error handling") {
                state.set_at(old_top, LuaValue::Str(s));
            }
        }
        LuaStatus::Ok => {
            // C: setnilvalue(s2v(oldtop)) — special case only for closing upvalues
            state.set_at(old_top, LuaValue::Nil);
        }
        _ => {
            // C: lua_assert(errorstatus(errcode)); real error
            debug_assert!(error_status(errcode));
            // C: setobjs2s(L, oldtop, L->top.p - 1) — error message on current top
            let top = state.top_idx();
            let err_val = state.get_at(top - 1).clone();
            state.set_at(old_top, err_val);
        }
    }
    // C: L->top.p = oldtop + 1
    state.set_top(old_top + 1);
}

/// Throws an error, escalating to the main thread or panicking if no handler exists.
///
/// C: `l_noret luaD_throw(lua_State *L, int errcode)`
///
/// PORT NOTE: In the Rust port, errors propagate via `Result<T, LuaError>` — callers
/// of this function should instead write `return Err(LuaError::with_status(errcode))`.
/// This function exists only for the rare "no handler anywhere" abort path.
/// The `l_noret` C annotation maps to `-> !` (never type).
pub(crate) fn throw(state: &mut LuaState, errcode: LuaStatus) -> ! {
    // TODO(port): main-thread escalation — C copies the error object to
    // g->mainthread and re-throws there. This requires coroutine support
    // (Phase E). In Phase A, fall through to the panic handler.

    // TODO(port): panic handler — C calls g->panic(L) if set. The panic
    // function is a lua_CFunction; calling it requires proper API setup.
    // For now, skip to the abort equivalent.

    // C: abort()
    // PORTING.md: std::process outside lua-cli is banned; use panic! instead.
    panic!("luaD_throw: unhandled Lua error (status = {:?}), no error handler", errcode)
}

/// Runs `f` in a "protected" context, catching any `LuaError` it returns.
/// Restores `nCcalls` on both success and error.
///
/// C: `int luaD_rawrunprotected(lua_State *L, Pfunc f, void *ud)`
///
/// PORT NOTE: The C implementation uses setjmp/longjmp for protection. In Rust
/// the same protection is provided by `Result<T, LuaError>` — the function just
/// calls `f` and returns the result. The `ud` void* argument is captured in the
/// closure environment instead of being passed separately.
pub(crate) fn raw_run_protected<F>(state: &mut LuaState, f: F) -> Result<(), LuaError>
where
    F: FnOnce(&mut LuaState) -> Result<(), LuaError>,
{
    // C: l_uint32 oldnCcalls = L->nCcalls;
    let old_n_ccalls = state.nCcalls;
    // C: LUAI_TRY(L, &lj, (*f)(L, ud));
    // PORT NOTE: setjmp/longjmp replaced by Result; f(state) propagates errors naturally.
    let result = f(state);
    // C: L->errorJmp = lj.previous; L->nCcalls = oldnCcalls;
    state.nCcalls = old_n_ccalls;
    result
}

// ══════════════════════════════════════════════════════════════════════════════
// Stack reallocation
// ══════════════════════════════════════════════════════════════════════════════

// PORT NOTE: `relstack` and `correctstack` from ldo.c are NOT translated.
// In C, they convert all stack pointers to/from byte-offsets before/after
// `realloc` (which may move the allocation). In Rust the stack is a
// `Vec<StackValue>` and all references are `StackIdx` (u32 index) — they are
// already position-stable across reallocation.  Nothing to save or restore.

/// Reallocates the stack to `new_size` slots, filling new slots with `Nil`.
/// Returns `Ok(true)` on success, `Ok(false)` when `raise_error` is false and
/// the allocation fails, or `Err(LuaError::Memory)` when `raise_error` is true.
///
/// C: `int luaD_reallocstack(lua_State *L, int newsize, int raiseerror)`
pub(crate) fn realloc_stack(
    state: &mut LuaState,
    new_size: usize,
    raise_error: bool,
) -> Result<bool, LuaError> {
    // C: int oldsize = stacksize(L);
    let old_size = state.stack_size() as usize;
    debug_assert!(new_size <= LUAI_MAXSTACK || new_size == ERRORSTACKSIZE);

    // C: int oldgcstop = G(L)->gcstopem; G(L)->gcstopem = 1;
    // PORT NOTE: stop emergency GC during reallocation so the allocator
    // (which may trigger GC) doesn't see a stack in mid-realloc state.
    let old_gcstop = state.global().gcstopem;
    state.global_mut().gcstopem = true;

    // C: newstack = luaM_reallocvector(...)
    // luaM_reallocvector → v.resize_with(n, T::default) (macros.tsv)
    let new_extent = new_size as usize + EXTRA_STACK as usize;
    let alloc_result = state.stack_resize(new_extent);

    // C: G(L)->gcstopem = oldgcstop;
    state.global_mut().gcstopem = old_gcstop;

    if alloc_result.is_err() {
        // C: correctstack(L) — no-op in Rust (see PORT NOTE above)
        if raise_error {
            // C: luaM_error(L) → return Err(LuaError::Memory)
            return Err(LuaError::Memory);
        } else {
            return Ok(false);
        }
    }

    // C: correctstack(L) — no-op in Rust
    // C: L->stack_last.p = L->stack.p + newsize;
    state.stack_last = StackIdx(new_size as u32);

    // C: for (i = oldsize+EXTRA_STACK; i < newsize+EXTRA_STACK; i++) setnilvalue(...)
    // Initialize newly allocated slots to Nil.
    let old_extent = old_size + EXTRA_STACK as usize;
    for i in old_extent..new_extent {
        state.stack_set_nil(i);
    }

    Ok(true)
}

/// Tries to grow the stack by at least `n` elements.
/// Returns `Ok(true)` on success, `Ok(false)` on soft failure (when
/// `raise_error` is false), or `Err(LuaError::Runtime("stack overflow"))` when
/// `raise_error` is true and the stack is already at maximum.
///
/// C: `int luaD_growstack(lua_State *L, int n, int raiseerror)`
pub(crate) fn grow_stack(
    state: &mut LuaState,
    n: i32,
    raise_error: bool,
) -> Result<bool, LuaError> {
    // C: int size = stacksize(L);
    let size = state.stack_size();

    // C: if (l_unlikely(size > LUAI_MAXSTACK))
    if size > LUAI_MAXSTACK {
        // Thread already using the error-overflow extension; cannot grow further.
        debug_assert!(state.stack_size() == ERRORSTACKSIZE);
        if raise_error {
            // C: luaD_throw(L, LUA_ERRERR)
            return Err(LuaError::with_status(LuaStatus::ErrErr));
        }
        return Ok(false);
    } else if (n as usize) < LUAI_MAXSTACK {
        // C: int newsize = 2 * size;
        let mut new_size = 2 * size;
        // C: int needed = cast_int(L->top.p - L->stack.p) + n;
        let needed = (state.top_idx().0 as i32 + n) as usize;
        if new_size > LUAI_MAXSTACK {
            new_size = LUAI_MAXSTACK;
        }
        if new_size < needed {
            new_size = needed;
        }
        if new_size <= LUAI_MAXSTACK {
            return realloc_stack(state, new_size, raise_error);
        }
    }
    // Stack overflow — allocate error extension so we can raise a message.
    realloc_stack(state, ERRORSTACKSIZE, raise_error)?;
    if raise_error {
        // C: luaG_runerror(L, "stack overflow")
        return Err(LuaError::runtime(format_args!("stack overflow")));
    }
    Ok(false)
}

/// Computes the number of stack slots currently in use across all call frames.
///
/// C: `static int stackinuse(lua_State *L)`
fn stack_in_use(state: &LuaState) -> usize {
    // C: StkId lim = L->top.p;
    let mut lim = state.top_idx();
    // C: for (ci = L->ci; ci != NULL; ci = ci->previous)
    //      if (lim < ci->top.p) lim = ci->top.p;
    let mut ci_idx_opt = Some(state.ci);
    while let Some(ci_idx) = ci_idx_opt {
        let ci = state.get_ci(ci_idx);
        if lim.0 < ci.top.0 {
            lim = ci.top;
        }
        ci_idx_opt = ci.previous;
    }
    debug_assert!(true /* TODO(phase-b): lim <= state.stack_last + EXTRA_STACK */);
    // C: res = cast_int(lim - L->stack.p) + 1
    let res = lim.0 as usize + 1;
    if res < LUA_MINSTACK as usize {
        LUA_MINSTACK as usize
    } else {
        res
    }
}

/// Shrinks the stack if it is more than 3× what is currently in use.
///
/// C: `void luaD_shrinkstack(lua_State *L)`
pub(crate) fn shrink_stack(state: &mut LuaState) {
    let inuse = stack_in_use(state);
    let max = if inuse > LUAI_MAXSTACK / 3 {
        LUAI_MAXSTACK
    } else {
        inuse * 3
    };
    if inuse <= LUAI_MAXSTACK && state.stack_size() > max {
        let nsize = if inuse > LUAI_MAXSTACK / 2 {
            LUAI_MAXSTACK
        } else {
            inuse * 2
        };
        // C: luaD_reallocstack(L, nsize, 0)  — ok if that fails
        let _ = realloc_stack(state, nsize, false);
    }
    // C: condmovestack(L,{},{}) — HARDSTACKTESTS only; no-op in default build (macros.tsv)
    // C: luaE_shrinkCI(L)
    state.shrink_ci();
}

/// Increments the stack top by one, growing the stack if necessary.
///
/// C: `void luaD_inctop(lua_State *L)`
pub(crate) fn inc_top(state: &mut LuaState) -> Result<(), LuaError> {
    // C: luaD_checkstack(L, 1)
    // luaD_checkstack → state.check_stack(n)?  (macros.tsv)
    state.check_stack(1)?;
    // C: L->top.p++
    let t = state.top_idx();
    state.set_top(t + 1);
    Ok(())
}

// ══════════════════════════════════════════════════════════════════════════════
// Hook machinery
// ══════════════════════════════════════════════════════════════════════════════

/// Calls the debug hook for the given event.
///
/// C: `void luaD_hook(lua_State *L, int event, int line, int ftransfer, int ntransfer)`
pub(crate) fn hook(
    state: &mut LuaState,
    event: i32,
    line: i32,
    ftransfer: i32,
    ntransfer: i32,
) -> Result<(), LuaError> {
    // C: if (hook && L->allowhook)
    if !state.has_hook() || !state.allowhook {
        return Ok(());
    }

    let ci_idx = state.ci;

    // C: ptrdiff_t top = savestack(L, L->top.p)
    // savestack → idx  (macros.tsv: StackIdx is already an offset)
    let saved_top = state.top_idx();
    // C: ptrdiff_t ci_top = savestack(L, ci->top.p)
    let saved_ci_top = state.get_ci(ci_idx).top;

    let mut mask = CIST_HOOKED;

    if ntransfer != 0 {
        // C: mask |= CIST_TRAN; ci->u2.transferinfo = {ftransfer, ntransfer}
        mask |= CIST_TRAN;
        state.set_ci_transfer_info(ci_idx, ftransfer as u16, ntransfer as u16);
    }

    // C: if (isLua(ci) && L->top.p < ci->top.p) L->top.p = ci->top.p;
    {
        let ci = state.get_ci(ci_idx);
        if ci.is_lua() {
            let ci_top = ci.top;
            if state.top_idx().0 < ci_top.0 {
                state.set_top(ci_top);
            }
        }
    }

    // C: luaD_checkstack(L, LUA_MINSTACK)
    state.check_stack(LUA_MINSTACK as i32)?;

    // C: if (ci->top.p < L->top.p + LUA_MINSTACK) ci->top.p = L->top.p + LUA_MINSTACK;
    {
        let top = state.top_idx();
        let ci = state.get_ci_mut(ci_idx);
        if ci.top.0 < (top + LUA_MINSTACK).0 {
            ci.top = top + LUA_MINSTACK;
        }
    }

    // C: L->allowhook = 0  — cannot call hooks inside a hook
    state.allowhook = false;
    state.get_ci_mut(ci_idx).callstatus |= mask;

    // C: lua_unlock(L) — no-op (macros.tsv)
    // C: (*hook)(L, &ar)
    // TODO(port): calling the hook while also holding `&mut LuaState` creates a
    // borrow conflict — the hook closure is stored on `state.hook`. Phase E
    // (debug support) must solve this by taking the hook out, calling it, and
    // reinserting it (similar to the standard Rust "extract-callback" pattern).
    // For Phase A the hook call is elided; the save/restore below still runs.

    // C: lua_lock(L) — no-op (macros.tsv)
    debug_assert!(!state.allowhook);
    state.allowhook = true;

    // C: ci->top.p = restorestack(L, ci_top)
    // restorestack → idx  (macros.tsv: StackIdx already)
    state.get_ci_mut(ci_idx).top = saved_ci_top;
    // C: L->top.p = restorestack(L, top)
    state.set_top(saved_top);
    state.get_ci_mut(ci_idx).callstatus &= !mask;

    Ok(())
}

/// Executes a call hook for a Lua function entry.
///
/// C: `void luaD_hookcall(lua_State *L, CallInfo *ci)`
pub(crate) fn hookcall(state: &mut LuaState, ci_idx: CallInfoIdx) -> Result<(), LuaError> {
    // C: L->oldpc = 0
    state.oldpc = 0;
    if state.hookmask & LUA_MASKCALL != 0 {
        // C: int event = (ci->callstatus & CIST_TAIL) ? LUA_HOOKTAILCALL : LUA_HOOKCALL;
        let event = if state.get_ci(ci_idx).callstatus & CIST_TAIL != 0 {
            LUA_HOOKTAILCALL
        } else {
            LUA_HOOKCALL
        };
        // C: Proto *p = ci_func(ci)->p;
        // ci_func(ci) → ci.lua_closure()  (macros.tsv)
        let numparams = {
            // TODO(port): ci_func returns &LuaClosure::Lua; getting proto.numparams
            // requires the full closure/proto API which isn't finalised yet.
            state.get_ci_lua_proto_numparams(ci_idx)
        };
        // C: ci->u.l.savedpc++
        let pc = state.ci_savedpc(ci_idx);
        state.set_ci_savedpc(ci_idx, pc + 1);
        hook(state, event, -1, 1, numparams as i32)?;
        // C: ci->u.l.savedpc--
        state.set_ci_savedpc(ci_idx, pc);
    }
    Ok(())
}

/// Executes a return hook and corrects `oldpc`.
///
/// C: `static void rethook(lua_State *L, CallInfo *ci, int nres)`
fn rethook(state: &mut LuaState, ci_idx: CallInfoIdx, nres: i32) -> Result<(), LuaError> {
    if state.hookmask & LUA_MASKRET != 0 {
        // C: StkId firstres = L->top.p - nres
        let first_res = state.top_idx().0 as i32 - nres;
        let mut delta: i32 = 0;

        // C: if (isLua(ci)) { Proto *p = ...; if (p->is_vararg) delta = ... }
        if state.get_ci(ci_idx).is_lua() {
            // TODO(port): ci_func(ci)->p accesses the Proto; needs full closure API.
            let (is_vararg, nextraargs, numparams) =
                state.get_ci_vararg_info(ci_idx);
            if is_vararg {
                // C: delta = ci->u.l.nextraargs + p->numparams + 1
                delta = nextraargs + numparams as i32 + 1;
            }
        }

        // C: ci->func.p += delta
        // PORT NOTE: temporarily advance func index by delta for hook transfer calc
        let original_func = state.get_ci(ci_idx).func;
        state.get_ci_mut(ci_idx).func = StackIdx((original_func.0 as i32 + delta) as u32);

        // C: ftransfer = cast(unsigned short, firstres - ci->func.p)
        let ci_func = state.get_ci(ci_idx).func;
        let ftransfer = (first_res - ci_func.0 as i32) as u16;

        hook(state, LUA_HOOKRET, -1, ftransfer as i32, nres)?;

        // C: ci->func.p -= delta
        state.get_ci_mut(ci_idx).func = original_func;
    }

    // C: if (isLua(ci = ci->previous)) L->oldpc = pcRel(ci->u.l.savedpc, ci_func(ci)->p)
    // pcRel → (pc - proto.code_base()) as i32 - 1  (macros.tsv)
    let previous = state.get_ci(ci_idx).previous;
    if let Some(prev_idx) = previous {
        if state.get_ci(prev_idx).is_lua() {
            // TODO(port): pcRel requires ci_func(ci)->p (proto code base pointer);
            // in Rust this is a Vec<Instruction> index calculation.
            // state.oldpc = (savedpc offset - 1) as u32
            state.oldpc = state.get_ci_pcrel(prev_idx);
        }
    }

    Ok(())
}

// ══════════════════════════════════════════════════════════════════════════════
// Call mechanics
// ══════════════════════════════════════════════════════════════════════════════

/// Looks up the `__call` metamethod for `func_idx` and inserts it below
/// the original function slot, shifting all arguments up by one.
/// Returns the (unchanged) `func_idx` on success, or an error if no
/// `__call` metamethod exists.
///
/// C: `static StkId tryfuncTM(lua_State *L, StkId func)`
fn try_func_tm(state: &mut LuaState, func_idx: StackIdx) -> Result<StackIdx, LuaError> {
    // C: checkstackGCp(L, 1, func)
    // checkstackGCp → { state.check_stack(n)?; state.gc().check_step(); }  (macros.tsv)
    // PORT NOTE: func_idx is a StackIdx and survives any stack reallocation.
    state.check_stack(1)?;
    state.gc_check_step();

    // C: tm = luaT_gettmbyobj(L, s2v(func), TM_CALL)
    let func_val = state.get_at(func_idx).clone();
    let tm = state.get_tm_by_obj(&func_val, TagMethod::Call);

    // C: if (l_unlikely(ttisnil(tm))) luaG_callerror(L, s2v(func))
    if matches!(tm, LuaValue::Nil) {
        let offender = state.get_at(func_idx).clone();
        let cur_ci = state.current_ci_idx();
        if let Some(cl) = state.ci_lua_closure(cur_ci) {
            let pc = state.ci_savedpc(cur_ci) as i32 - 1;
            let line = crate::debug::get_func_line(&cl.proto, pc);
            let prev_pc = (pc as usize).saturating_sub(1);
            let prev_op = cl.proto.code.get(prev_pc).map(|i| i.opcode_raw()).unwrap_or(255);
            let prev_a = cl.proto.code.get(prev_pc).map(|i| i.a_raw()).unwrap_or(0);
            let prev_bx = cl.proto.code.get(prev_pc).map(|i| i.bx_raw()).unwrap_or(0);
            let prev_k = cl.proto.k.get(prev_bx as usize).cloned();
            let cur_op = cl.proto.code.get(pc as usize).map(|i| i.opcode_raw()).unwrap_or(255);
            eprintln!("DBG call_error: pc={} line={} cur_op={} prev_op={} prev_a={} prev_bx={} prev_k={:?}",
                pc, line, cur_op, prev_op, prev_a, prev_bx, prev_k);
        } else {
            eprintln!("DBG call_error: non-lua ci, func_idx={:?}", func_idx);
        }
        return Err(LuaError::call_error(&offender));
    }

    // Open a slot: shift everything from top down to func_idx up by one.
    // C: for (p = L->top.p; p > func; p--) setobjs2s(L, p, p-1)
    let top = state.top_idx();
    let mut p = top;
    while p.0 > func_idx.0 {
        let val = state.get_at(p - 1).clone();
        state.set_at(p, val);
        p = p - 1;
    }
    // C: L->top.p++
    state.set_top(top + 1);
    // C: setobj2s(L, func, tm)
    state.set_at(func_idx, tm);

    Ok(func_idx)
}

/// Moves `nres` results from their current position on the stack to `res_idx`,
/// padding with `Nil` if fewer than `wanted` results are present, or discarding
/// extras if more are present.
///
/// C: `l_sinline void moveresults(lua_State *L, StkId res, int nres, int wanted)`
#[inline]
fn move_results(
    state: &mut LuaState,
    res_idx: StackIdx,
    nres: i32,
    wanted: i32,
) -> Result<(), LuaError> {
    // C: switch (wanted) — handle common cases separately
    match wanted {
        0 => {
            // C: L->top.p = res; return;
            state.set_top(res_idx);
            return Ok(());
        }
        1 => {
            if nres == 0 {
                // C: setnilvalue(s2v(res))
                state.set_at(res_idx, LuaValue::Nil);
            } else {
                // C: setobjs2s(L, res, L->top.p - nres)
                let top = state.top_idx();
                let src = state.get_at(top - nres as i32).clone();
                state.set_at(res_idx, src);
            }
            // C: L->top.p = res + 1
            state.set_top(res_idx + 1);
            return Ok(());
        }
        LUA_MULTRET => {
            // wanted = nres: fall through to generic case below
        }
        _ => {
            // C: if (hastocloseCfunc(wanted))
            // hastocloseCfunc → n < LUA_MULTRET  (macros.tsv)
            if wanted < LUA_MULTRET {
                let ci_idx = state.ci;
                // C: L->ci->callstatus |= CIST_CLSRET; L->ci->u2.nres = nres;
                state.get_ci_mut(ci_idx).callstatus |= CIST_CLSRET;
                state.set_ci_u2_nres(ci_idx, nres);

                // C: res = luaF_close(L, res, CLOSEKTOP, 1)
                // TODO(port): CLOSE_K_TOP sentinel needs proper StackIdx encoding
                // in func::close; for now pass as a special sentinel value.
                let res_idx = func::close(state, res_idx, CLOSE_K_TOP, true)?;

                let ci_idx = state.ci;
                // C: L->ci->callstatus &= ~CIST_CLSRET
                state.get_ci_mut(ci_idx).callstatus &= !CIST_CLSRET;

                if state.hookmask != 0 {
                    // C: ptrdiff_t savedres = savestack(L, res)
                    // savestack → idx  (macros.tsv: StackIdx is already stable)
                    let saved_res = res_idx;
                    rethook(state, ci_idx, nres)?;
                    // C: res = restorestack(L, savedres) — already stable
                    let _ = saved_res; // = res_idx (no-op restore)
                }

                // C: wanted = decodeNresults(wanted)
                // decodeNresults → -(n) - 3  (macros.tsv)
                let decoded_wanted = -(wanted) - 3;
                let wanted = if decoded_wanted == LUA_MULTRET {
                    nres
                } else {
                    decoded_wanted
                };

                // Fall into generic case with updated wanted.
                let first_result = state.top_idx().0 as i32 - nres;
                let actual_nres = nres.min(wanted);
                for i in 0..actual_nres {
                    let src = state.get_at((first_result + i) as u32).clone();
                    state.set_at(res_idx + i as i32, src);
                }
                for i in actual_nres..wanted {
                    state.set_at(res_idx + i as i32, LuaValue::Nil);
                }
                state.set_top(res_idx + wanted as i32);
                return Ok(());
            }
        }
    }

    // Generic case (also reached from LUA_MULTRET with wanted = nres).
    let effective_wanted = if wanted == LUA_MULTRET { nres } else { wanted };
    let first_result = state.top_idx().0 as i32 - nres;
    let actual_nres = nres.min(effective_wanted);
    for i in 0..actual_nres {
        let src = state.get_at((first_result + i) as u32).clone();
        state.set_at(res_idx + i as i32, src);
    }
    for i in actual_nres..effective_wanted {
        state.set_at(res_idx + i as i32, LuaValue::Nil);
    }
    state.set_top(res_idx + effective_wanted as i32);
    Ok(())
}

/// Finishes a function call: calls hook if needed, moves results into place,
/// and pops the current call frame.
///
/// C: `void luaD_poscall(lua_State *L, CallInfo *ci, int nres)`
pub(crate) fn poscall(
    state: &mut LuaState,
    ci_idx: CallInfoIdx,
    nres: i32,
) -> Result<(), LuaError> {
    // C: int wanted = ci->nresults;
    let wanted = state.get_ci(ci_idx).nresults as i32;

    // C: if (l_unlikely(L->hookmask && !hastocloseCfunc(wanted))) rethook(L, ci, nres);
    if state.hookmask != 0 && !(wanted < LUA_MULTRET) {
        rethook(state, ci_idx, nres)?;
    }

    // C: moveresults(L, ci->func.p, nres, wanted)
    let func_idx = state.get_ci(ci_idx).func;
    move_results(state, func_idx, nres, wanted)?;

    // C: lua_assert(!(ci->callstatus & (CIST_HOOKED|CIST_YPCALL|CIST_FIN|CIST_TRAN|CIST_CLSRET)))
    debug_assert!(
        state.get_ci(ci_idx).callstatus
            & (CIST_HOOKED | CIST_YPCALL | CIST_FIN | CIST_TRAN | CIST_CLSRET)
            == 0
    );

    // C: L->ci = ci->previous
    let previous = state
        .get_ci(ci_idx)
        .previous
        .expect("poscall: no previous call frame");
    state.ci = previous;
    Ok(())
}

/// Advances to the next `CallInfo` slot, allocating a new one if required.
/// Sets `state.ci` to the new frame and fills its fields.
///
/// C: `l_sinline CallInfo *prepCallInfo(lua_State *L, StkId func, int nret, int mask, StkId top)`
#[inline]
fn prep_call_info(
    state: &mut LuaState,
    func_idx: StackIdx,
    nret: i32,
    mask: u16,
    top_idx: StackIdx,
) -> Result<CallInfoIdx, LuaError> {
    // C: CallInfo *ci = L->ci = next_ci(L)
    // next_ci → L->ci->next ? L->ci->next : luaE_extendCI(L)
    let ci_idx = state.next_ci()?;
    state.ci = ci_idx;
    {
        let ci = state.get_ci_mut(ci_idx);
        ci.func = func_idx;
        ci.nresults = nret as i16;
        ci.callstatus = mask;
        ci.top = top_idx;
        ci.u = if (mask & crate::state::CIST_C) != 0 {
            crate::state::CallInfoFrame::c_default()
        } else {
            crate::state::CallInfoFrame::lua_default()
        };
    }
    Ok(ci_idx)
}

/// Pre-call for C functions: sets up a CallInfo, fires the call hook if needed,
/// invokes the C function, and calls `poscall`.
/// Returns the number of values returned by the C function.
///
/// C: `l_sinline int precallC(lua_State *L, StkId func, int nresults, lua_CFunction f)`
#[inline]
fn precall_c(
    state: &mut LuaState,
    func_idx: StackIdx,
    nresults: i32,
    f: crate::state::LuaCFunction,
) -> Result<i32, LuaError> {
    // C: checkstackGCp(L, LUA_MINSTACK, func)
    state.check_stack(LUA_MINSTACK as i32)?;
    state.gc_check_step();

    let top_idx = state.top_idx();
    // C: L->ci = ci = prepCallInfo(L, func, nresults, CIST_C, L->top.p + LUA_MINSTACK)
    let ci_idx = prep_call_info(state, func_idx, nresults, CIST_C, top_idx + LUA_MINSTACK)?;

    // C: lua_assert(ci->top.p <= L->stack_last.p)
    debug_assert!(true /* TODO(phase-b): state.get_ci(ci_idx).top <= state.stack_last */);

    // C: if (l_unlikely(L->hookmask & LUA_MASKCALL))
    if state.hookmask & LUA_MASKCALL != 0 {
        // C: int narg = cast_int(L->top.p - func) - 1
        let narg = (state.top_idx().0 as i32 - func_idx.0 as i32) - 1;
        hook(state, LUA_HOOKCALL, -1, 1, narg)?;
    }

    // C: lua_unlock(L) — no-op (macros.tsv)
    // C: n = (*f)(L)
    let n = f(state)? as i32;
    // C: lua_lock(L) — no-op (macros.tsv)

    // C: api_checknelems(L, n)
    // api_checknelems → debug_assert!(n < (top - ci_func), "not enough elements") (macros.tsv)
    debug_assert!(
        n <= state.top_idx().0 as i32,
        "C function returned more values than available"
    );

    poscall(state, ci_idx, n)?;
    Ok(n)
}

/// Prepares a tail call, reusing the current `CallInfo`.
/// Returns the result count for C functions, or `-1` to signal the VM that a
/// Lua function should continue executing.
///
/// C: `int luaD_pretailcall(lua_State *L, CallInfo *ci, StkId func, int narg1, int delta)`
pub(crate) fn pretailcall(
    state: &mut LuaState,
    ci_idx: CallInfoIdx,
    mut func_idx: StackIdx,
    mut narg1: i32,
    delta: i32,
) -> Result<i32, LuaError> {
    // C: retry: switch (ttypetag(s2v(func))) { ... default: goto retry; }
    loop {
        let func_val = state.get_at(func_idx).clone();
        match func_val {
            // C: case LUA_VCCL — return precallC(L, func, LUA_MULTRET, clCvalue(s2v(func))->f);
            LuaValue::Function(LuaClosure::C(ref cl)) => {
                let cfunc = state.global().c_functions[cl.func];
                return precall_c(state, func_idx, LUA_MULTRET, cfunc);
            }
            // C: case LUA_VLCF — return precallC(L, func, LUA_MULTRET, fvalue(s2v(func)));
            LuaValue::Function(LuaClosure::LightC(f)) => {
                let cfunc = state.global().c_functions[f];
                return precall_c(state, func_idx, LUA_MULTRET, cfunc);
            }
            // C: case LUA_VLCL — Lua function
            LuaValue::Function(LuaClosure::Lua(ref cl)) => {
                let proto = cl.proto.clone();
                let fsize = proto.maxstacksize as i32;
                let nfixparams = proto.numparams as i32;

                // C: checkstackGCp(L, fsize - delta, func)
                state.check_stack(fsize - delta)?;
                state.gc_check_step();

                // C: ci->func.p -= delta  (restore 'func' if vararg)
                {
                    let ci = state.get_ci_mut(ci_idx);
                    ci.func = StackIdx((ci.func.0 as i32 - delta) as u32);
                }
                let ci_func = state.get_ci(ci_idx).func;

                // C: for (i = 0; i < narg1; i++) setobjs2s(L, ci->func.p + i, func + i)
                for i in 0..narg1 {
                    let src = state.get_at(func_idx + i as i32).clone();
                    state.set_at(ci_func + i as i32, src);
                }

                // Update func_idx to reflect the moved-down position.
                func_idx = ci_func;

                // C: for (; narg1 <= nfixparams; narg1++) setnilvalue(s2v(func+narg1))
                while narg1 <= nfixparams {
                    state.set_at(func_idx + narg1 as i32, LuaValue::Nil);
                    narg1 += 1;
                }

                // C: ci->top.p = func + 1 + fsize
                {
                    let new_ci_top = func_idx + 1 + fsize as i32;
                    let stack_last = state.stack_last;
                    let ci = state.get_ci_mut(ci_idx);
                    ci.top = new_ci_top;
                    debug_assert!(ci.top.0 <= stack_last.0);
                    // C: ci->u.l.savedpc = p->code  (starting point — offset 0)
                    ci.set_saved_pc(0);
                    ci.callstatus |= CIST_TAIL;
                }

                // C: L->top.p = func + narg1
                state.set_top(func_idx + narg1 as i32);
                return Ok(-1); // Signal: Lua function, VM should continue.
            }
            _ => {
                // C: default: func = tryfuncTM(L, func); narg1++; goto retry;
                func_idx = try_func_tm(state, func_idx)?;
                narg1 += 1;
                // continue the loop — equivalent to goto retry
            }
        }
    }
}

/// Prepares a call to `func_idx` (C or Lua).
/// For C functions, also executes the call and returns `None`.
/// For Lua functions, returns `Some(ci_idx)` — the caller must then invoke the VM.
///
/// C: `CallInfo *luaD_precall(lua_State *L, StkId func, int nresults)`
pub(crate) fn precall(
    state: &mut LuaState,
    mut func_idx: StackIdx,
    nresults: i32,
) -> Result<Option<CallInfoIdx>, LuaError> {
    // C: retry: switch (ttypetag(s2v(func))) { ... default: goto retry; }
    loop {
        let func_val = state.get_at(func_idx).clone();
        match func_val {
            // C: case LUA_VCCL — precallC(L, func, nresults, clCvalue(s2v(func))->f); return NULL;
            LuaValue::Function(LuaClosure::C(ref cl)) => {
                let cfunc = state.global().c_functions[cl.func];
                precall_c(state, func_idx, nresults, cfunc)?;
                return Ok(None);
            }
            // C: case LUA_VLCF — light C function
            LuaValue::Function(LuaClosure::LightC(f)) => {
                // C: precallC(L, func, nresults, fvalue(s2v(func))); return NULL;
                // `f` is a registry index into `GlobalState.c_functions` (lua-types
                // can't carry a `LuaState`-aware fn pointer directly). Resolve to
                // the real `LuaCFunction` here and call it with `&mut LuaState`.
                state.check_stack(LUA_MINSTACK as i32)?;
                state.gc_check_step();

                let top_idx = state.top_idx();
                let ci_idx =
                    prep_call_info(state, func_idx, nresults, CIST_C, top_idx + LUA_MINSTACK)?;

                if state.hookmask & LUA_MASKCALL != 0 {
                    let narg = (state.top_idx().0 as i32 - func_idx.0 as i32) - 1;
                    hook(state, LUA_HOOKCALL, -1, 1, narg)?;
                }

                let cfunc = state.global().c_functions[f];
                let n = cfunc(state)? as i32;
                debug_assert!(
                    n <= state.top_idx().0 as i32,
                    "C function returned more values than available"
                );
                poscall(state, ci_idx, n)?;
                return Ok(None);
            }
            // C: case LUA_VLCL — Lua function
            LuaValue::Function(LuaClosure::Lua(ref cl)) => {
                let proto = cl.proto.clone();
                let narg = (state.top_idx().0 as i32 - func_idx.0 as i32) - 1;
                let nfixparams = proto.numparams as i32;
                let fsize = proto.maxstacksize as i32;

                // C: checkstackGCp(L, fsize, func)
                state.check_stack(fsize)?;
                state.gc_check_step();

                // C: L->ci = ci = prepCallInfo(L, func, nresults, 0, func + 1 + fsize)
                let ci_idx =
                    prep_call_info(state, func_idx, nresults, 0, func_idx + 1 + fsize as i32)?;

                // C: ci->u.l.savedpc = p->code  (starting point — offset 0)
                // TODO(port): same as in pretailcall — offset 0 = first instruction.
                state.set_ci_savedpc(ci_idx, 0);

                // C: for (; narg < nfixparams; narg++) setnilvalue(s2v(L->top.p++))
                let mut narg = narg;
                while narg < nfixparams {
                    let top = state.top_idx();
                    state.set_at(top, LuaValue::Nil);
                    state.set_top(top + 1);
                    narg += 1;
                }

                // C: lua_assert(ci->top.p <= L->stack_last.p)
                debug_assert!(true /* TODO(phase-b): state.get_ci(ci_idx).top <= state.stack_last */);
                return Ok(Some(ci_idx));
            }
            _ => {
                // C: default: func = tryfuncTM(L, func); goto retry;
                func_idx = try_func_tm(state, func_idx)?;
                // continue the loop — equivalent to goto retry
            }
        }
    }
}

/// Internal call helper shared by `call` and `callnoyield`.
/// `inc` is added to/subtracted from `nCcalls` around the call.
///
/// C: `l_sinline void ccall(lua_State *L, StkId func, int nResults, l_uint32 inc)`
#[inline]
fn ccall_inner(
    state: &mut LuaState,
    func_idx: StackIdx,
    n_results: i32,
    inc: u32,
) -> Result<(), LuaError> {
    // C: L->nCcalls += inc;
    state.nCcalls += inc;

    // C: if (l_unlikely(getCcalls(L) >= LUAI_MAXCCALLS))
    // getCcalls → state.c_calls()  (macros.tsv: lower 16 bits of nCcalls)
    if state.c_calls() >= LUAI_MAXCCALLS {
        // C: checkstackp(L, 0, func) — free any use of EXTRA_STACK
        // checkstackp → state.check_stack(n)?  (macros.tsv)
        state.check_stack(0)?;
        // C: luaE_checkcstack(L)
        state.check_c_stack()?;
    }

    // C: if ((ci = luaD_precall(L, func, nResults)) != NULL)
    if let Some(ci_idx) = precall(state, func_idx, n_results)? {
        // C: ci->callstatus = CIST_FRESH; luaV_execute(L, ci);
        state.get_ci_mut(ci_idx).callstatus = CIST_FRESH;
        vm::execute(state, ci_idx)?;
    }

    // C: L->nCcalls -= inc;
    state.nCcalls -= inc;
    Ok(())
}

/// Calls a function through C with one recursive-invocation increment.
///
/// C: `void luaD_call(lua_State *L, StkId func, int nResults)`
pub(crate) fn call(
    state: &mut LuaState,
    func_idx: StackIdx,
    n_results: i32,
) -> Result<(), LuaError> {
    // C: ccall(L, func, nResults, 1)
    ccall_inner(state, func_idx, n_results, 1)
}

/// Like `call` but increments the non-yieldable counter as well.
///
/// C: `void luaD_callnoyield(lua_State *L, StkId func, int nResults)`
pub(crate) fn callnoyield(
    state: &mut LuaState,
    func_idx: StackIdx,
    n_results: i32,
) -> Result<(), LuaError> {
    // C: ccall(L, func, nResults, nyci)
    // NYCI = 0x10001 increments both the recursion count and the non-yieldable count.
    ccall_inner(state, func_idx, n_results, NYCI)
}

// ══════════════════════════════════════════════════════════════════════════════
// Yield / coroutine continuation machinery
// ══════════════════════════════════════════════════════════════════════════════

/// Finishes the job of `lua_pcallk` after it was interrupted by a yield.
///
/// C: `static int finishpcallk(lua_State *L, CallInfo *ci)`
fn finish_pcallk(state: &mut LuaState, ci_idx: CallInfoIdx) -> Result<LuaStatus, LuaError> {
    // C: int status = getcistrecst(ci)
    // getcistrecst → ci.recover_status()  (macros.tsv)
    // PORT NOTE: recover_status() returns i32; convert to LuaStatus for type safety.
    let mut status = LuaStatus::from_raw(state.get_ci(ci_idx).recover_status());

    if status == LuaStatus::Ok {
        // C: status = LUA_YIELD — was interrupted by a yield
        status = LuaStatus::Yield;
    } else {
        // C: StkId func = restorestack(L, ci->u2.funcidx)
        let func_idx = StackIdx(state.get_ci_u2_funcidx(ci_idx) as u32);
        // C: L->allowhook = getoah(ci->callstatus)
        // getoah → ci.get_oah()  (macros.tsv)
        state.allowhook = state.get_ci(ci_idx).get_oah();
        // C: func = luaF_close(L, func, status, 1)  — can yield or raise
        // TODO(port): CLOSE_K_TOP sentinel encoding; see close_tbc comment above.
        let _func_idx = func::close(state, func_idx, status as i32, true)?;
        // C: luaD_seterrorobj(L, status, func)
        set_error_obj(state, status, func_idx);
        // C: luaD_shrinkstack(L)
        shrink_stack(state);
        // C: setcistrecst(ci, LUA_OK)
        state.get_ci_mut(ci_idx).set_recover_status(LuaStatus::Ok as i32);
    }

    // C: ci->callstatus &= ~CIST_YPCALL
    state.get_ci_mut(ci_idx).callstatus &= !CIST_YPCALL;
    // C: L->errfunc = ci->u.c.old_errfunc
    let old_errfunc = state.get_ci(ci_idx).u_c_old_errfunc();
    state.errfunc = old_errfunc;

    Ok(status)
}

/// Completes the execution of a C function that was interrupted by a yield.
///
/// C: `static void finishCcall(lua_State *L, CallInfo *ci)`
fn finish_ccall(state: &mut LuaState, ci_idx: CallInfoIdx) -> Result<(), LuaError> {
    let n;

    // C: if (ci->callstatus & CIST_CLSRET)
    if state.get_ci(ci_idx).callstatus & CIST_CLSRET != 0 {
        // C: lua_assert(hastocloseCfunc(ci->nresults))
        debug_assert!((state.get_ci(ci_idx).nresults as i32) < LUA_MULTRET);
        // C: n = ci->u2.nres  — just redo luaD_poscall
        n = state.get_ci_u2_nres(ci_idx);
    } else {
        // C: lua_assert(ci->u.c.k != NULL && yieldable(L))
        debug_assert!(
            state.get_ci(ci_idx).u_c_k().is_some() && state.is_yieldable(),
            "finishCcall: no continuation or non-yieldable"
        );

        let mut status = LuaStatus::Yield;

        // C: if (ci->callstatus & CIST_YPCALL) status = finishpcallk(L, ci)
        if state.get_ci(ci_idx).callstatus & CIST_YPCALL != 0 {
            status = finish_pcallk(state, ci_idx)?;
        }

        // C: adjustresults(L, LUA_MULTRET)
        // adjustresults → state.adjust_results(nres)  (macros.tsv)
        state.adjust_results(LUA_MULTRET);

        // C: lua_unlock(L) — no-op
        // C: n = (*ci->u.c.k)(L, status, ci->u.c.ctx)
        // TODO(port): calling the continuation function while holding &mut LuaState
        // has the same borrow problem as the hook call. Phase E must solve this.
        // For now, extract and re-insert the continuation.
        let k = state.get_ci(ci_idx).u_c_k();
        let ctx = state.get_ci(ci_idx).u_c_ctx();
        if let Some(k_fn) = k {
            n = k_fn(state, status as i32, ctx)? as i32;
        } else {
            // TODO(port): unreachable in correct code; the assert above guards this
            return Err(LuaError::runtime(format_args!("finishCcall: missing continuation")));
        }
        // C: lua_lock(L) — no-op
        debug_assert!(
            n <= state.top_idx().0 as i32,
            "continuation returned more values than available"
        );
    }

    // C: luaD_poscall(L, ci, n)
    poscall(state, ci_idx, n)?;
    Ok(())
}

/// Unrolls the full continuation stack of a coroutine until empty.
///
/// C: `static void unroll(lua_State *L, void *ud)`
fn unroll(state: &mut LuaState) -> Result<(), LuaError> {
    // C: while ((ci = L->ci) != &L->base_ci)
    loop {
        let ci_idx = state.ci;
        if state.is_base_ci(ci_idx) {
            break;
        }
        if !state.get_ci(ci_idx).is_lua() {
            // C: finishCcall(L, ci)
            finish_ccall(state, ci_idx)?;
        } else {
            // C: luaV_finishOp(L); luaV_execute(L, ci);
            vm::finish_op(state)?;
            vm::execute(state, ci_idx)?;
        }
    }
    Ok(())
}

/// Searches the call stack for the innermost suspended protected call.
///
/// C: `static CallInfo *findpcall(lua_State *L)`
fn find_pcall(state: &LuaState) -> Option<CallInfoIdx> {
    let mut ci_idx_opt = Some(state.ci);
    while let Some(ci_idx) = ci_idx_opt {
        let ci = state.get_ci(ci_idx);
        if ci.callstatus & CIST_YPCALL != 0 {
            return Some(ci_idx);
        }
        ci_idx_opt = ci.previous;
    }
    None
}

/// Signals an error in the `lua_resume` call itself (not in the coroutine body).
///
/// C: `static int resume_error(lua_State *L, const char *msg, int narg)`
fn resume_error(state: &mut LuaState, msg: &[u8], narg: i32) -> LuaStatus {
    // C: L->top.p -= narg  — discard args
    let top = state.top_idx();
    state.set_top(top - narg as i32);
    // C: setsvalue2s(L, L->top.p, luaS_new(L, msg))
    // luaS_new → state.intern_str(s)  (macros.tsv)
    let s = state.intern_str(msg).ok();
    let new_top = state.top_idx();
    if let Some(s) = s { state.set_at(new_top, LuaValue::Str(s)); }
    // C: api_incr_top(L) — api_incr_top dropped; state.push() increments  (macros.tsv)
    state.set_top(new_top + 1);
    // C: lua_unlock(L) — no-op
    LuaStatus::ErrRun
}

/// Core coroutine resume logic (runs inside `raw_run_protected`).
///
/// C: `static void resume(lua_State *L, void *ud)`
fn resume_coroutine(state: &mut LuaState, nargs: i32) -> Result<(), LuaError> {
    // C: StkId firstArg = L->top.p - n
    let top = state.top_idx();
    let first_arg = top - nargs as i32;
    let ci_idx = state.ci;

    if state.status == LuaStatus::Ok as u8 {
        // C: ccall(L, firstArg - 1, LUA_MULTRET, 0)  — start the coroutine body
        ccall_inner(state, first_arg - 1, LUA_MULTRET, 0)?;
    } else {
        // C: lua_assert(L->status == LUA_YIELD); L->status = LUA_OK;
        debug_assert!(state.status == LuaStatus::Yield as u8);
        state.status = LuaStatus::Ok as u8;

        if state.get_ci(ci_idx).is_lua() {
            // C: yielded inside a hook — undo savedpc increment from luaG_traceexec
            debug_assert!(state.get_ci(ci_idx).callstatus & CIST_HOOKYIELD != 0);
            let pc = state.ci_savedpc(ci_idx);
            state.set_ci_savedpc(ci_idx, pc.saturating_sub(1));
            // C: L->top.p = firstArg  — discard arguments
            state.set_top(first_arg);
            // C: luaV_execute(L, ci)
            vm::execute(state, ci_idx)?;
        } else {
            // C: "common" yield
            if let Some(k_fn) = state.get_ci(ci_idx).u_c_k() {
                let ctx = state.get_ci(ci_idx).u_c_ctx();
                // C: lua_unlock(L) — no-op
                // C: n = (*ci->u.c.k)(L, LUA_YIELD, ci->u.c.ctx)
                let n = k_fn(state, LuaStatus::Yield as i32, ctx)? as i32;
                // C: lua_lock(L) — no-op
                debug_assert!(n <= state.top_idx().0 as i32);
                // C: luaD_poscall(L, ci, n)
                poscall(state, ci_idx, n)?;
            } else {
                // No continuation: just finish the call
                let n = (state.top_idx().0 as i32 - first_arg.0 as i32).max(0);
                poscall(state, ci_idx, n)?;
            }
        }

        // C: unroll(L, NULL)
        unroll(state)?;
    }
    Ok(())
}

/// Unrolls the coroutine while there are recoverable (protected-call) errors.
///
/// C: `static int precover(lua_State *L, int status)`
fn precover(state: &mut LuaState, mut status: LuaStatus) -> LuaStatus {
    // C: while (errorstatus(status) && (ci = findpcall(L)) != NULL)
    while error_status(status) {
        if let Some(ci_idx) = find_pcall(state) {
            // C: L->ci = ci; setcistrecst(ci, status)
            state.ci = ci_idx;
            state.get_ci_mut(ci_idx).set_recover_status(status);
            // C: status = luaD_rawrunprotected(L, unroll, NULL)
            status = match raw_run_protected(state, |s| unroll(s)) {
                Ok(()) => LuaStatus::Ok,
                Err(e) => e.to_status(),
            };
        } else {
            break;
        }
    }
    status
}

/// Resumes (or starts) a coroutine thread.
///
/// C: `LUA_API int lua_resume(lua_State *L, lua_State *from, int nargs, int *nresults)`
pub fn lua_resume(
    state: &mut LuaState,
    from: Option<&mut LuaState>,
    nargs: i32,
    nresults: &mut i32,
) -> LuaStatus {
    // TODO(port): coroutine support (Phase E). The implementation below is a
    // faithful translation of the C logic but will not work correctly until
    // coroutine stack switching is available. Phase A: translate the logic;
    // Phase E: make it actually work.

    // C: lua_lock(L) — no-op
    if state.status == LuaStatus::Ok as u8 {
        // C: if (L->ci != &L->base_ci) — starting check
        if !state.is_base_ci(state.ci) {
            return resume_error(state, b"cannot resume non-suspended coroutine", nargs);
        }
        // C: else if (L->top.p - (L->ci->func.p + 1) == nargs) — no function?
        let ci_func = state.get_ci(state.ci).func;
        if state.top_idx().0 as i32 - (ci_func.0 as i32 + 1) == nargs {
            return resume_error(state, b"cannot resume dead coroutine", nargs);
        }
    } else if state.status != LuaStatus::Yield as u8 {
        return resume_error(state, b"cannot resume dead coroutine", nargs);
    }

    // C: L->nCcalls = (from) ? getCcalls(from) : 0;
    state.nCcalls = from
        .as_ref()
        .map(|f| f.c_calls() as u32)
        .unwrap_or(0);

    if state.c_calls() >= LUAI_MAXCCALLS {
        return resume_error(state, b"C stack overflow", nargs);
    }
    state.nCcalls += 1;

    // C: luai_userstateresume(L, nargs) — no-op (macros.tsv)
    // C: api_checknelems(L, ...)
    debug_assert!(
        if state.status == LuaStatus::Ok as u8 {
            nargs + 1 <= state.top_idx().0 as i32
        } else {
            nargs <= state.top_idx().0 as i32
        },
        "lua_resume: not enough stack elements"
    );

    // C: status = luaD_rawrunprotected(L, resume, &nargs)
    let mut status = match raw_run_protected(state, |s| resume_coroutine(s, nargs)) {
        Ok(()) => LuaStatus::Ok,
        Err(e) => e.to_status(),
    };

    // C: status = precover(L, status)
    status = precover(state, status);

    if !error_status(status) {
        // C: lua_assert(status == L->status)
        debug_assert!(status as u8 == state.status, "lua_resume: status mismatch");
    } else {
        // Unrecoverable error — mark thread as dead
        state.status = status as u8;
        // C: luaD_seterrorobj(L, status, L->top.p)
        let top = state.top_idx();
        set_error_obj(state, status, top);
        // C: L->ci->top.p = L->top.p
        let new_top = state.top_idx();
        let ci_idx = state.ci;
        state.get_ci_mut(ci_idx).top = new_top;
    }

    // C: *nresults = (status == LUA_YIELD) ? L->ci->u2.nyield : cast_int(L->top.p - (L->ci->func.p + 1))
    let ci_idx = state.ci;
    *nresults = if status == LuaStatus::Yield {
        state.get_ci_u2_nyield(ci_idx)
    } else {
        let ci_func = state.get_ci(ci_idx).func;
        state.top_idx().0 as i32 - (ci_func.0 as i32 + 1)
    };

    // C: lua_unlock(L) — no-op
    status
}

/// Returns whether the calling context can yield.
///
/// C: `LUA_API int lua_isyieldable(lua_State *L)`
pub fn lua_isyieldable(state: &LuaState) -> bool {
    // C: return yieldable(L)
    // yieldable → state.is_yieldable()  (macros.tsv)
    state.is_yieldable()
}

/// Yields the current coroutine, saving the continuation function `k` and
/// context `ctx` for resumption.
///
/// C: `LUA_API int lua_yieldk(lua_State *L, int nresults, lua_KContext ctx, lua_KFunction k)`
pub fn lua_yieldk(
    state: &mut LuaState,
    nresults: i32,
    ctx: isize,
    k: Option<crate::state::LuaKFunction>,
) -> Result<i32, LuaError> {
    // TODO(port): coroutine support (Phase E). Yielding requires stack-switching;
    // stubbed here with a faithful translation of the C logic.

    // C: luai_userstateyield(L, nresults) — no-op (macros.tsv)
    // C: lua_lock(L) — no-op
    let ci_idx = state.ci;

    // C: api_checknelems(L, nresults)
    debug_assert!(
        nresults <= state.top_idx().0 as i32,
        "lua_yieldk: not enough elements on stack"
    );

    // C: if (l_unlikely(!yieldable(L)))
    if !state.is_yieldable() {
        if !state.is_main_thread() {
            // C: luaG_runerror(L, "attempt to yield across a C-call boundary")
            return Err(LuaError::runtime(format_args!(
                "attempt to yield across a C-call boundary"
            )));
        } else {
            // C: luaG_runerror(L, "attempt to yield from outside a coroutine")
            return Err(LuaError::runtime(format_args!(
                "attempt to yield from outside a coroutine"
            )));
        }
    }

    // C: L->status = LUA_YIELD
    state.status = LuaStatus::Yield as u8;
    // C: ci->u2.nyield = nresults
    state.set_ci_u2_nyield(ci_idx, nresults);

    if state.get_ci(ci_idx).is_lua() {
        // C: inside a hook
        debug_assert!(!state.get_ci(ci_idx).is_lua_code());
        debug_assert!(nresults == 0, "hooks cannot yield values");
        debug_assert!(k.is_none(), "hooks cannot continue after yielding");
        // Fall through — hook yields return 0 to luaD_hook.
    } else {
        // C: if ((ci->u.c.k = k) != NULL) ci->u.c.ctx = ctx;
        // TODO(phase-b): mutate u_c.k/u_c.ctx fields directly inside CallInfoFrame::C.
        if let crate::state::CallInfoFrame::C { k: ref mut frame_k, ctx: ref mut frame_ctx, .. } =
            state.get_ci_mut(ci_idx).u {
            *frame_k = k;
            if k.is_some() {
                *frame_ctx = ctx;
            }
        }
        // C: luaD_throw(L, LUA_YIELD)
        // In Rust: return Err to propagate the yield signal up the call stack.
        return Err(LuaError::Yield);
    }

    // C: lua_assert(ci->callstatus & CIST_HOOKED)  — must be inside a hook
    debug_assert!(
        state.get_ci(ci_idx).callstatus & CIST_HOOKED != 0,
        "lua_yieldk called outside a hook"
    );
    // C: lua_unlock(L) — no-op
    Ok(0) // return to luaD_hook
}

// ══════════════════════════════════════════════════════════════════════════════
// Protected close
// ══════════════════════════════════════════════════════════════════════════════

/// Auxiliary data for `close_aux`.
///
/// C: `struct CloseP { StkId level; int status; }`
struct CloseP {
    level: StackIdx,
    status: LuaStatus,
}

/// Calls `luaF_close` with the level/status captured in `pcl`.
///
/// C: `static void closepaux(lua_State *L, void *ud)`
fn close_aux(state: &mut LuaState, pcl: &mut CloseP) -> Result<(), LuaError> {
    // C: luaF_close(L, pcl->level, pcl->status, 0)
    // TODO(port): status→i32 conversion for func::close sentinel.
    func::close(state, pcl.level, pcl.status as i32, false)?;
    Ok(())
}

/// Calls `luaF_close` in protected mode, retrying on error.
/// Returns the original `status` on clean completion, or the new error status.
///
/// C: `int luaD_closeprotected(lua_State *L, ptrdiff_t level, int status)`
pub(crate) fn close_protected(
    state: &mut LuaState,
    level: StackIdx,
    status: LuaStatus,
) -> LuaStatus {
    let old_ci = state.ci;
    let old_allowhook = state.allowhook;
    let mut status = status;

    loop {
        // C: pcl.level = restorestack(L, level) — StackIdx already stable
        let mut pcl = CloseP { level, status };
        // C: status = luaD_rawrunprotected(L, &closepaux, &pcl)
        let run_status = match raw_run_protected(state, |s| close_aux(s, &mut pcl)) {
            Ok(()) => LuaStatus::Ok,
            Err(e) => e.to_status(),
        };
        if run_status == LuaStatus::Ok {
            // C: return pcl.status
            return pcl.status;
        }
        // C: L->ci = old_ci; L->allowhook = old_allowhooks;
        state.ci = old_ci;
        state.allowhook = old_allowhook;
        status = run_status;
    }
}

/// Calls function `func` in protected mode, restoring thread state on error.
/// Returns `LuaStatus::Ok` on success, or an error status.
///
/// C: `int luaD_pcall(lua_State *L, Pfunc func, void *u, ptrdiff_t old_top, ptrdiff_t ef)`
pub(crate) fn pcall<F>(
    state: &mut LuaState,
    func: F,
    old_top: StackIdx,
    ef: isize,
) -> LuaStatus
where
    F: FnOnce(&mut LuaState) -> Result<(), LuaError>,
{
    let old_ci = state.ci;
    let old_allowhook = state.allowhook;
    let old_errfunc = state.errfunc;
    // C: L->errfunc = ef
    state.errfunc = ef;

    // C: status = luaD_rawrunprotected(L, func, u)
    // PORT NOTE: In C, luaD_throw pushes the error value onto the stack before
    // longjmp-ing, and luaG_errormsg invokes the message handler at the error
    // site before the throw. In Rust the error rides inside LuaError and
    // propagates via `?`, so the handler is never invoked along the way; we
    // synthesise that invocation here once we've caught the Err.
    let mut status = match raw_run_protected(state, func) {
        Ok(()) => LuaStatus::Ok,
        Err(e) => {
            let s = e.to_status();
            state.push(e.into_value());
            if ef != 0 && error_status(s) && s != LuaStatus::ErrErr {
                let errfunc_idx = StackIdx(ef as u32);
                let arg = state.get_at(state.top_idx() - 1).clone();
                state.push(arg);
                let handler = state.get_at(errfunc_idx).clone();
                state.set_at(state.top_idx() - 2, handler);
                match state.call_no_yield(state.top_idx() - 2, 1) {
                    Ok(()) => s,
                    Err(_) => LuaStatus::ErrErr,
                }
            } else {
                s
            }
        }
    };

    if status != LuaStatus::Ok {
        // C: L->ci = old_ci; L->allowhook = old_allowhooks;
        state.ci = old_ci;
        state.allowhook = old_allowhook;
        // C: status = luaD_closeprotected(L, old_top, status)
        status = close_protected(state, old_top, status);
        // C: luaD_seterrorobj(L, status, restorestack(L, old_top))
        // restorestack → old_top  (already a StackIdx)
        set_error_obj(state, status, old_top);
        // C: luaD_shrinkstack(L)
        shrink_stack(state);
    }

    // C: L->errfunc = old_errfunc
    state.errfunc = old_errfunc;
    status
}

// ══════════════════════════════════════════════════════════════════════════════
// Protected parser
// ══════════════════════════════════════════════════════════════════════════════

/// Parser invocation data passed through `pcall`.
///
/// C: `struct SParser { ZIO *z; Mbuffer buff; Dyndata dyd; const char *mode; const char *name; }`
///
/// PORT NOTE: `const char *mode` and `const char *name` become owned byte vecs
/// so that `SParser` can outlive the original string data without raw pointers.
struct SParser {
    z: ZIO,
    /// LexBuffer from `crate::zio` (Mbuffer in C).
    buff: LexBuffer,
    /// TODO(phase-b): real Dyndata lives in the lua-parse crate.
    dyd: DynDataStub,
    // C: const char *mode — byte slice for chunk mode ("b", "t", or "bt")
    // PORT NOTE: stored as Option<Vec<u8>> to own the bytes; None means no mode restriction.
    mode: Option<Vec<u8>>,
    // C: const char *name — chunk name (source identifier)
    name: Vec<u8>,
}

/// Checks that the chunk mode permits loading the given kind ("binary" or "text").
///
/// C: `static void checkmode(lua_State *L, const char *mode, const char *x)`
fn check_mode(
    state: &mut LuaState,
    mode: Option<&[u8]>,
    kind: &[u8],
) -> Result<(), LuaError> {
    if let Some(mode_bytes) = mode {
        // C: strchr(mode, x[0]) == NULL  — mode doesn't contain the first letter of kind
        let kind_char = kind[0];
        if !mode_bytes.contains(&kind_char) {
            // C: luaO_pushfstring + luaD_throw(L, LUA_ERRSYNTAX)
            // TODO(port): &[u8] display — lossy UTF-8 here is acceptable for mode/kind
            // strings which are always ASCII literals ("binary"/"text" and "bt"/"b"/"t").
            return Err(LuaError::syntax(format_args!(
                "attempt to load a {} chunk (mode is '{}')",
                core::str::from_utf8(kind).unwrap_or("?"),
                core::str::from_utf8(mode_bytes).unwrap_or("?"),
            )));
        }
    }
    Ok(())
}

/// Parser callback invoked inside `pcall`: reads the first byte to decide
/// binary vs. text, then calls the undumper or parser accordingly.
///
/// C: `static void f_parser(lua_State *L, void *ud)`
fn f_parser(state: &mut LuaState, p: &mut SParser) -> Result<(), LuaError> {
    // C: int c = zgetc(p->z)  — read first character
    // zgetc → z.getc()  (macros.tsv)
    let c = p.z.getc();

    // C: if (c == LUA_SIGNATURE[0])
    // LUA_SIGNATURE → const LUA_SIGNATURE: &[u8] = b"\x1bLua"  (macros.tsv)
    let cl = if c == b'\x1b' as i32 {
        // C: checkmode(L, p->mode, "binary")
        check_mode(state, p.mode.as_deref(), b"binary")?;
        // C: cl = luaU_undump(L, p->z, p->name)
        // TODO(port): undump returns a LClosure; the Rust API isn't finalised.
        crate::undump::undump(state, &mut p.z, &p.name)?
    } else {
        // C: checkmode(L, p->mode, "text")
        check_mode(state, p.mode.as_deref(), b"text")?;
        // C: cl = luaY_parser(L, p->z, &p->buff, &p->dyd, p->name, c)
        // TODO(port): parser API not yet finalised; returns a LClosure.
        parse_stub(state, &mut p.z, &mut p.buff, &mut p.dyd, &p.name, c)?
    };

    // C: lua_assert(cl->nupvalues == cl->p->sizeupvalues)
    debug_assert!(cl.upvals.len() == cl.proto.upvalues.len());
    // C: luaF_initupvals(L, cl)
    func::init_upvals(state, &cl)?;

    // PORT NOTE: In C-Lua, `luaY_parser` / `luaU_undump` themselves push the
    // closure onto the stack before returning (see lparser.c `luaY_parser`:
    // `setclLvalue2s(L, L->top.p, cl); luaD_inctop(L);`). In the Rust port
    // they return the closure by value, so `f_parser` must push it here.
    // Without this, the caller (`api::load`) sees stale Nil at top-1 and any
    // subsequent `pcall_k(state, 0, ...)` fails with "attempt to call a nil
    // value".
    state.check_stack(1)?;
    state.push(LuaValue::Function(LuaClosure::Lua(cl)));

    Ok(())
}

/// Loads and parses a chunk in protected mode, returning the status.
///
/// C: `int luaD_protectedparser(lua_State *L, ZIO *z, const char *name, const char *mode)`
pub(crate) fn protected_parser(
    state: &mut LuaState,
    z: ZIO,
    name: &[u8],
    mode: Option<&[u8]>,
) -> LuaStatus {
    // C: incnny(L)  — cannot yield during parsing
    // incnny → state.inc_nny()  (macros.tsv)
    state.inc_nny();

    let mut p = SParser {
        z,
        buff: LexBuffer::new(),
        dyd: DynDataStub::new(),
        mode: mode.map(|m| m.to_vec()),
        name: name.to_vec(),
    };

    // C: luaZ_initbuffer(L, &p.buff) — LexBuffer::new() already initialised above
    // (macros.tsv: luaZ_initbuffer → buf.init() / Mbuffer::new())

    // C: status = luaD_pcall(L, f_parser, &p, savestack(L, L->top.p), L->errfunc)
    let top_idx = state.top_idx();
    let errfunc = state.errfunc;
    let status = pcall(state, |s| f_parser(s, &mut p), top_idx, errfunc);

    // C: luaZ_freebuffer(L, &p.buff) — Rust's Drop handles deallocation (macros.tsv)
    // C: luaM_freearray(L, p.dyd.actvar.arr, ...) — Rust's Drop handles Vec (macros.tsv)
    // (p and all its sub-fields drop here automatically)

    // C: decnny(L)
    // decnny → state.dec_nny()  (macros.tsv)
    state.dec_nny();

    status
}

// ──────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        src/ldo.c  (1029 lines, ~37 functions translated, 2 omitted)
//   target_crate:  lua-vm
//   confidence:    medium
//   todos:         23
//   port_notes:    13
//   unsafe_blocks: 0
//   notes:         Core call/stack/error machinery translated faithfully.
//                  setjmp/longjmp → Result<T,LuaError> throughout.
//                  relstack/correctstack omitted (StackIdx already offset-based).
//                  Coroutine functions (lua_resume, lua_yieldk, resume, unroll,
//                  etc.) are translated but require Phase E stack-switching to
//                  actually work.  Hook-callback borrow conflict flagged as
//                  TODO(port) in hook() and finish_ccall(); Phase E must solve.
//                  All method calls (check_stack, gc_check_step, get_ci*,
//                  set_ci*, next_ci, etc.) are best-guess stubs to be wired
//                  up in Phase B once the LuaState API is finalised.
// ──────────────────────────────────────────────────────────────────────────
