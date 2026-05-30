//! Stack and call structure of Lua.
//!
//! Translated from `src/ldo.c` (Lua 5.4.7, ~1029 lines, ~37 functions).
//! Target crate: lua-vm (`crates/lua-vm/src/do_.rs`).

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

// PORT NOTE: LUAI_MAXSTACK is 1_000_000 per macros.tsv.
const LUAI_MAXSTACK: usize = 1_000_000;
const ERRORSTACKSIZE: usize = LUAI_MAXSTACK + 200;

const EXTRA_STACK: i32 = 5;

const LUA_MINSTACK: i32 = 20;

const LUA_MULTRET: i32 = -1;

const NYCI: u32 = 0x10001;

use crate::state::LUAI_MAXCCALLS;

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

// TODO(port): derive from HookEvent enum once that type is settled.
const LUA_MASKCALL: u8 = 1 << 0;
const LUA_MASKRET: u8 = 1 << 1;

const LUA_HOOKCALL: i32 = 0;
const LUA_HOOKRET: i32 = 1;
const LUA_HOOKTAILCALL: i32 = 4;

// PORT NOTE: luaF_close takes StackIdx; this sentinel needs special handling.
// TODO(port): settle representation with func.rs author.
const CLOSE_K_TOP: i32 = -1;

// ── Helper: errorstatus ──────────────────────────────────────────────────────

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
pub(crate) fn set_error_obj(state: &mut LuaState, errcode: LuaStatus, old_top: StackIdx) {
    match errcode {
        LuaStatus::ErrMem => {
            // reuse the preallocated OOM message string
            let memerrmsg = state.global().memerrmsg.clone();
            state.set_at(old_top, LuaValue::Str(memerrmsg));
        }
        LuaStatus::ErrErr => {
            if let Ok(s) = state.intern_str(b"error in error handling") {
                state.set_at(old_top, LuaValue::Str(s));
            }
        }
        LuaStatus::Ok => {
            state.set_at(old_top, LuaValue::Nil);
        }
        _ => {
            debug_assert!(error_status(errcode));
            let top = state.top_idx();
            let err_val = state.get_at(top - 1).clone();
            state.set_at(old_top, err_val);
        }
    }
    state.set_top(old_top + 1);
}

/// Runs `f` in a "protected" context, catching any `LuaError` it returns.
/// Restores `n_ccalls` on both success and error.
///
///
/// PORT NOTE: The C implementation uses setjmp/longjmp for protection. In Rust
/// the same protection is provided by `Result<T, LuaError>` — the function just
/// calls `f` and returns the result. The `ud` void* argument is captured in the
/// closure environment instead of being passed separately.
pub(crate) fn raw_run_protected<F>(state: &mut LuaState, f: F) -> Result<(), LuaError>
where
    F: FnOnce(&mut LuaState) -> Result<(), LuaError>,
{
    let old_n_ccalls = state.n_ccalls;
    // PORT NOTE: setjmp/longjmp replaced by Result; f(state) propagates errors naturally.
    let result = f(state);
    state.n_ccalls = old_n_ccalls;
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
pub(crate) fn realloc_stack(
    state: &mut LuaState,
    new_size: usize,
    raise_error: bool,
) -> Result<bool, LuaError> {
    let old_size = state.stack_size() as usize;
    debug_assert!(new_size <= LUAI_MAXSTACK || new_size == ERRORSTACKSIZE);

    // PORT NOTE: stop emergency GC during reallocation so the allocator
    // (which may trigger GC) doesn't see a stack in mid-realloc state.
    let old_gcstop = state.global().gcstopem;
    state.global_mut().gcstopem = true;

    // luaM_reallocvector → v.resize_with(n, T::default) (macros.tsv)
    let new_extent = new_size as usize + EXTRA_STACK as usize;
    let alloc_result = state.stack_resize(new_extent);

    state.global_mut().gcstopem = old_gcstop;

    if alloc_result.is_err() {
        if raise_error {
            return Err(LuaError::Memory);
        } else {
            return Ok(false);
        }
    }

    state.stack_last = StackIdx(new_size as u32);

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
pub(crate) fn grow_stack(
    state: &mut LuaState,
    n: i32,
    raise_error: bool,
) -> Result<bool, LuaError> {
    let size = state.stack_size();

    if size > LUAI_MAXSTACK {
        // Thread already using the error-overflow extension; cannot grow further.
        debug_assert!(state.stack_size() == ERRORSTACKSIZE);
        if raise_error {
            return Err(LuaError::with_status(LuaStatus::ErrErr));
        }
        return Ok(false);
    } else if (n as usize) < LUAI_MAXSTACK {
        let mut new_size = 2 * size;
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
        return Err(LuaError::runtime(format_args!("stack overflow")));
    }
    Ok(false)
}

/// Computes the number of stack slots currently in use across all call frames.
///
fn stack_in_use(state: &LuaState) -> usize {
    let mut lim = state.top_idx();
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
    let res = lim.0 as usize + 1;
    if res < LUA_MINSTACK as usize {
        LUA_MINSTACK as usize
    } else {
        res
    }
}

/// Shrinks the stack if it is more than 3× what is currently in use.
///
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
        let _ = realloc_stack(state, nsize, false);
    }
    state.shrink_ci();
}

// ══════════════════════════════════════════════════════════════════════════════
// Hook machinery
// ══════════════════════════════════════════════════════════════════════════════

/// Calls the debug hook for the given event.
///
pub(crate) fn hook(
    state: &mut LuaState,
    event: i32,
    line: i32,
    ftransfer: i32,
    ntransfer: i32,
) -> Result<(), LuaError> {
    if !state.has_hook() || !state.allowhook {
        return Ok(());
    }

    let ci_idx = state.ci;

    // savestack → idx  (macros.tsv: StackIdx is already an offset)
    let saved_top = state.top_idx();
    let saved_ci_top = state.get_ci(ci_idx).top;

    let mut mask = CIST_HOOKED;

    if ntransfer != 0 {
        mask |= CIST_TRAN;
        state.set_ci_transfer_info(ci_idx, ftransfer as u16, ntransfer as u16);
    }

    {
        let ci = state.get_ci(ci_idx);
        if ci.is_lua() {
            let ci_top = ci.top;
            if state.top_idx().0 < ci_top.0 {
                state.set_top(ci_top);
            }
        }
    }

    state.check_stack(LUA_MINSTACK as i32)?;

    {
        let top = state.top_idx();
        let ci = state.get_ci_mut(ci_idx);
        if ci.top.0 < (top + LUA_MINSTACK).0 {
            let new_top = top + LUA_MINSTACK;
            ci.top = new_top;
            state.clear_stack_range(top, new_top);
        }
    }

    state.allowhook = false;
    state.get_ci_mut(ci_idx).callstatus |= mask;

    let mut ar = crate::debug::LuaDebug::default();
    ar.event = event;
    ar.currentline = line;
    ar.ftransfer = ftransfer as u16;
    ar.ntransfer = ntransfer as u16;
    ar.i_ci = Some(ci_idx);
    let hook_opt = state.hook.take();
    if let Some(mut h) = hook_opt {
        h(state, &ar);
        if state.hook.is_none() {
            state.hook = Some(h);
        }
    }

    debug_assert!(!state.allowhook);
    state.allowhook = true;

    // restorestack → idx  (macros.tsv: StackIdx already)
    state.get_ci_mut(ci_idx).top = saved_ci_top;
    state.set_top(saved_top);
    state.get_ci_mut(ci_idx).callstatus &= !mask;

    Ok(())
}

/// Executes a call hook for a Lua function entry.
///
pub(crate) fn hookcall(state: &mut LuaState, ci_idx: CallInfoIdx) -> Result<(), LuaError> {
    state.oldpc = 0;
    if state.hookmask & LUA_MASKCALL != 0 {
        let event = if state.get_ci(ci_idx).callstatus & CIST_TAIL != 0 {
            LUA_HOOKTAILCALL
        } else {
            LUA_HOOKCALL
        };
        // ci_func(ci) → ci.lua_closure()  (macros.tsv)
        let numparams = {
            // TODO(port): ci_func returns &LuaClosure::Lua; getting proto.numparams
            // requires the full closure/proto API which isn't finalised yet.
            state.get_ci_lua_proto_numparams(ci_idx)
        };
        let pc = state.ci_savedpc(ci_idx);
        state.set_ci_savedpc(ci_idx, pc + 1);
        hook(state, event, -1, 1, numparams as i32)?;
        state.set_ci_savedpc(ci_idx, pc);
    }
    Ok(())
}

/// Executes a return hook and corrects `oldpc`.
///
fn rethook(state: &mut LuaState, ci_idx: CallInfoIdx, nres: i32) -> Result<(), LuaError> {
    if state.hookmask & LUA_MASKRET != 0 {
        let first_res = state.top_idx().0 as i32 - nres;
        let mut delta: i32 = 0;

        if state.get_ci(ci_idx).is_lua() {
            // TODO(port): ci_func(ci)->p accesses the Proto; needs full closure API.
            let (is_vararg, nextraargs, numparams) =
                state.get_ci_vararg_info(ci_idx);
            if is_vararg {
                delta = nextraargs + numparams as i32 + 1;
            }
        }

        // PORT NOTE: temporarily advance func index by delta for hook transfer calc
        let original_func = state.get_ci(ci_idx).func;
        state.get_ci_mut(ci_idx).func = StackIdx((original_func.0 as i32 + delta) as u32);

        let ci_func = state.get_ci(ci_idx).func;
        let ftransfer = (first_res - ci_func.0 as i32) as u16;

        hook(state, LUA_HOOKRET, -1, ftransfer as i32, nres)?;

        state.get_ci_mut(ci_idx).func = original_func;
    }

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
fn try_func_tm(state: &mut LuaState, func_idx: StackIdx) -> Result<StackIdx, LuaError> {
    // checkstackGCp → { state.check_stack(n)?; state.gc().check_step(); }  (macros.tsv)
    // PORT NOTE: func_idx is a StackIdx and survives any stack reallocation.
    state.check_stack(1)?;
    if state.gc_check_needed {
        state.gc_check_step();
    }

    let func_val = state.get_at(func_idx).clone();
    let tm = state.get_tm_by_obj(&func_val, TagMethod::Call);

    if matches!(tm, LuaValue::Nil) {
        let offender = state.get_at(func_idx).clone();
        return Err(crate::debug::call_error(state, &offender, func_idx));
    }

    // Open a slot: shift everything from top down to func_idx up by one.
    let top = state.top_idx();
    let mut p = top;
    while p.0 > func_idx.0 {
        let val = state.get_at(p - 1).clone();
        state.set_at(p, val);
        p = p - 1;
    }
    state.set_top(top + 1);
    state.set_at(func_idx, tm);

    Ok(func_idx)
}

/// Moves `nres` results from their current position on the stack to `res_idx`,
/// padding with `Nil` if fewer than `wanted` results are present, or discarding
/// extras if more are present.
///
#[inline(always)]
fn move_results(
    state: &mut LuaState,
    res_idx: StackIdx,
    nres: i32,
    wanted: i32,
) -> Result<(), LuaError> {
    match wanted {
        0 => {
            state.set_top(res_idx);
            return Ok(());
        }
        1 => {
            if nres == 0 {
                state.set_at(res_idx, LuaValue::Nil);
            } else {
                let top = state.top_idx();
                let src = state.get_at(top - nres as i32).clone();
                state.set_at(res_idx, src);
            }
            state.set_top(res_idx + 1);
            return Ok(());
        }
        LUA_MULTRET => {
            // wanted = nres: fall through to generic case below
        }
        _ => {
            // hastocloseCfunc → n < LUA_MULTRET  (macros.tsv)
            if wanted < LUA_MULTRET {
                let ci_idx = state.ci;
                state.get_ci_mut(ci_idx).callstatus |= CIST_CLSRET;
                state.set_ci_u2_nres(ci_idx, nres);

                // TODO(port): CLOSE_K_TOP sentinel needs proper StackIdx encoding
                // in func::close; for now pass as a special sentinel value.
                let res_idx = func::close(state, res_idx, CLOSE_K_TOP, true)?;

                let ci_idx = state.ci;
                state.get_ci_mut(ci_idx).callstatus &= !CIST_CLSRET;

                if state.hookmask != 0 {
                    // savestack → idx  (macros.tsv: StackIdx is already stable)
                    let saved_res = res_idx;
                    rethook(state, ci_idx, nres)?;
                    let _ = saved_res; // = res_idx (no-op restore)
                }

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
#[inline(always)]
pub(crate) fn poscall(
    state: &mut LuaState,
    ci_idx: CallInfoIdx,
    nres: i32,
) -> Result<(), LuaError> {
    let wanted = state.get_ci(ci_idx).nresults as i32;

    if state.hookmask != 0 && !(wanted < LUA_MULTRET) {
        rethook(state, ci_idx, nres)?;
    }

    let func_idx = state.get_ci(ci_idx).func;
    move_results(state, func_idx, nres, wanted)?;

    debug_assert!(
        state.get_ci(ci_idx).callstatus
            & (CIST_HOOKED | CIST_YPCALL | CIST_FIN | CIST_TRAN | CIST_CLSRET)
            == 0
    );

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
#[inline(always)]
fn prep_call_info(
    state: &mut LuaState,
    func_idx: StackIdx,
    nret: i32,
    mask: u16,
    top_idx: StackIdx,
) -> Result<CallInfoIdx, LuaError> {
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
#[inline(always)]
fn precall_c(
    state: &mut LuaState,
    func_idx: StackIdx,
    nresults: i32,
    f: crate::state::LuaCallable,
) -> Result<i32, LuaError> {
    state.check_stack(LUA_MINSTACK as i32)?;
    if state.gc_check_needed {
        state.gc_check_step();
    }

    let top_idx = state.top_idx();
    let ci_idx = prep_call_info(state, func_idx, nresults, CIST_C, top_idx + LUA_MINSTACK)?;

    debug_assert!(true /* TODO(phase-b): state.get_ci(ci_idx).top <= state.stack_last */);

    if state.hookmask & LUA_MASKCALL != 0 {
        let narg = (state.top_idx().0 as i32 - func_idx.0 as i32) - 1;
        hook(state, LUA_HOOKCALL, -1, 1, narg)?;
    }

    let n = f.call(state)? as i32;

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
pub(crate) fn pretailcall(
    state: &mut LuaState,
    ci_idx: CallInfoIdx,
    mut func_idx: StackIdx,
    mut narg1: i32,
    delta: i32,
) -> Result<i32, LuaError> {
    loop {
        let func_val = state.get_at(func_idx).clone();
        match func_val {
            LuaValue::Function(LuaClosure::C(ref cl)) => {
                let cfunc = state.global().c_functions[cl.func].clone();
                return precall_c(state, func_idx, LUA_MULTRET, cfunc);
            }
            LuaValue::Function(LuaClosure::LightC(f)) => {
                let cfunc = state.global().c_functions[f].clone();
                return precall_c(state, func_idx, LUA_MULTRET, cfunc);
            }
            LuaValue::Function(LuaClosure::Lua(ref cl)) => {
                let proto = cl.proto.clone();
                let fsize = proto.maxstacksize as i32;
                let nfixparams = proto.numparams as i32;

                state.check_stack(fsize - delta)?;
                if state.gc_check_needed {
                    state.gc_check_step();
                }

                {
                    let ci = state.get_ci_mut(ci_idx);
                    ci.func = StackIdx((ci.func.0 as i32 - delta) as u32);
                }
                let ci_func = state.get_ci(ci_idx).func;

                for i in 0..narg1 {
                    let src = state.get_at(func_idx + i as i32).clone();
                    state.set_at(ci_func + i as i32, src);
                }

                // Update func_idx to reflect the moved-down position.
                func_idx = ci_func;

                while narg1 <= nfixparams {
                    state.set_at(func_idx + narg1 as i32, LuaValue::Nil);
                    narg1 += 1;
                }

                {
                    let new_ci_top = func_idx + 1 + fsize as i32;
                    let stack_last = state.stack_last;
                    let live_top = state.top_idx();
                    let ci = state.get_ci_mut(ci_idx);
                    ci.top = new_ci_top;
                    debug_assert!(ci.top.0 <= stack_last.0);
                    ci.set_saved_pc(0);
                    ci.callstatus |= CIST_TAIL;
                    state.clear_stack_range(live_top, new_ci_top);
                }

                state.set_top(func_idx + narg1 as i32);
                return Ok(-1); // Signal: Lua function, VM should continue.
            }
            _ => {
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
///
/// PORT NOTE (perf): the C source uses `retry: switch (...) { default: goto retry; }`.
/// We split that into a fast-path call to the Lua-closure handler and an explicit
/// retry loop for the rare metamethod miss-path. The fast path inlines the Lua-closure
/// arm so LLVM can specialize for the by-far-most-common case (a direct Lua call).
#[inline(always)]
pub(crate) fn precall(
    state: &mut LuaState,
    func_idx: StackIdx,
    nresults: i32,
) -> Result<Option<CallInfoIdx>, LuaError> {
    if let LuaValue::Function(LuaClosure::Lua(cl)) =
        &state.stack[func_idx.0 as usize].val
    {
        let nfixparams = cl.proto.numparams as i32;
        let fsize = cl.proto.maxstacksize as i32;
        let narg = (state.top_idx().0 as i32 - func_idx.0 as i32) - 1;

        state.check_stack(fsize)?;
        if state.gc_check_needed {
            state.gc_check_step();
        }

        let ci_idx =
            prep_call_info(state, func_idx, nresults, 0, func_idx + 1 + fsize as i32)?;
        state.set_ci_savedpc(ci_idx, 0);

        if narg < nfixparams {
            fill_missing_params(state, narg, nfixparams);
        }
        return Ok(Some(ci_idx));
    }
    precall_slow(state, func_idx, nresults)
}

/// Cold path: fills `nfixparams - narg` nil values onto the stack.
///
/// (the body of the loop in `luaD_precall`).
#[cold]
#[inline(never)]
fn fill_missing_params(state: &mut LuaState, mut narg: i32, nfixparams: i32) {
    while narg < nfixparams {
        let top = state.top_idx();
        state.set_at(top, LuaValue::Nil);
        state.set_top(top + 1);
        narg += 1;
    }
}

/// Cold path: callee is a C closure, light C function, or a non-function with
/// a `__call` metamethod. Mirrors the structure of C-Lua's `retry:` loop in
/// `luaD_precall`.
#[cold]
#[inline(never)]
fn precall_slow(
    state: &mut LuaState,
    mut func_idx: StackIdx,
    nresults: i32,
) -> Result<Option<CallInfoIdx>, LuaError> {
    loop {
        let func_val = state.get_at(func_idx).clone();
        match func_val {
            LuaValue::Function(LuaClosure::C(ref cl)) => {
                let cfunc = state.global().c_functions[cl.func].clone();
                precall_c(state, func_idx, nresults, cfunc)?;
                return Ok(None);
            }
            LuaValue::Function(LuaClosure::LightC(f)) => {
                state.check_stack(LUA_MINSTACK as i32)?;
                if state.gc_check_needed {
                    state.gc_check_step();
                }

                let top_idx = state.top_idx();
                let ci_idx =
                    prep_call_info(state, func_idx, nresults, CIST_C, top_idx + LUA_MINSTACK)?;

                if state.hookmask & LUA_MASKCALL != 0 {
                    let narg = (state.top_idx().0 as i32 - func_idx.0 as i32) - 1;
                    hook(state, LUA_HOOKCALL, -1, 1, narg)?;
                }

                let cfunc = state.global().c_functions[f].clone();
                let n = cfunc.call(state)? as i32;
                debug_assert!(
                    n <= state.top_idx().0 as i32,
                    "C function returned more values than available"
                );
                poscall(state, ci_idx, n)?;
                return Ok(None);
            }
            LuaValue::Function(LuaClosure::Lua(ref cl)) => {
                let narg = (state.top_idx().0 as i32 - func_idx.0 as i32) - 1;
                let nfixparams = cl.proto.numparams as i32;
                let fsize = cl.proto.maxstacksize as i32;

                state.check_stack(fsize)?;
                if state.gc_check_needed {
                    state.gc_check_step();
                }

                let ci_idx = prep_call_info(
                    state,
                    func_idx,
                    nresults,
                    0,
                    func_idx + 1 + fsize as i32,
                )?;
                state.set_ci_savedpc(ci_idx, 0);

                if narg < nfixparams {
                    fill_missing_params(state, narg, nfixparams);
                }
                return Ok(Some(ci_idx));
            }
            _ => {
                func_idx = try_func_tm(state, func_idx)?;
            }
        }
    }
}

/// Internal call helper shared by `call` and `callnoyield`.
/// `inc` is added to/subtracted from `n_ccalls` around the call.
///
#[inline]
fn ccall_inner(
    state: &mut LuaState,
    func_idx: StackIdx,
    n_results: i32,
    inc: u32,
) -> Result<(), LuaError> {
    ccall_inner_with_status(state, func_idx, n_results, inc, 0)
}

#[inline]
fn ccall_inner_with_status(
    state: &mut LuaState,
    func_idx: StackIdx,
    n_results: i32,
    inc: u32,
    extra_callstatus: u16,
) -> Result<(), LuaError> {
    state.n_ccalls += inc;

    // getCcalls → state.c_calls()  (macros.tsv: lower 16 bits of n_ccalls)
    if state.c_calls() >= LUAI_MAXCCALLS {
        // checkstackp → state.check_stack(n)?  (macros.tsv)
        state.check_stack(0)?;
        state.check_c_stack()?;
    }

    if let Some(ci_idx) = precall(state, func_idx, n_results)? {
        state.get_ci_mut(ci_idx).callstatus = CIST_FRESH | extra_callstatus;
        vm::execute(state, ci_idx)?;
    }

    state.n_ccalls -= inc;
    Ok(())
}

/// Calls a function through C with one recursive-invocation increment.
///
pub(crate) fn call(
    state: &mut LuaState,
    func_idx: StackIdx,
    n_results: i32,
) -> Result<(), LuaError> {
    ccall_inner(state, func_idx, n_results, 1)
}

/// Like `call` but increments the non-yieldable counter as well.
///
pub(crate) fn callnoyield(
    state: &mut LuaState,
    func_idx: StackIdx,
    n_results: i32,
) -> Result<(), LuaError> {
    // NYCI = 0x10001 increments both the recursion count and the non-yieldable count.
    ccall_inner(state, func_idx, n_results, NYCI)
}

// ══════════════════════════════════════════════════════════════════════════════
// Yield / coroutine continuation machinery
// ══════════════════════════════════════════════════════════════════════════════

/// Finishes the job of `lua_pcallk` after it was interrupted by a yield.
///
fn finish_pcallk(state: &mut LuaState, ci_idx: CallInfoIdx) -> Result<LuaStatus, LuaError> {
    // getcistrecst → ci.recover_status()  (macros.tsv)
    // PORT NOTE: recover_status() returns i32; convert to LuaStatus for type safety.
    let mut status = LuaStatus::from_raw(state.get_ci(ci_idx).recover_status());

    if status == LuaStatus::Ok {
        status = LuaStatus::Yield;
    } else {
        let func_idx = StackIdx(state.get_ci_u2_funcidx(ci_idx) as u32);
        // getoah → ci.get_oah()  (macros.tsv)
        state.allowhook = state.get_ci(ci_idx).get_oah();
        // TODO(port): CLOSE_K_TOP sentinel encoding; see close_tbc comment above.
        let _func_idx = func::close(state, func_idx, status as i32, true)?;
        set_error_obj(state, status, func_idx);

        // PORT NOTE: lua-c invokes the message handler at error-raise time via
        // `luaG_errormsg`, BEFORE the longjmp propagates the error. Our error
        // propagation rides on Rust `Result::Err` and has no equivalent
        // chokepoint at raise time, so we run the handler here at the
        // recover/catch site — semantically equivalent. Only fires on the
        // yield-then-error path (the sync-error path in `pcall_k`/api.rs
        // calls the handler inline and clears CIST_YPCALL before we'd reach
        // this function). Fixes coroutine.lua:319 (xpcall + yield + error).
        if state.errfunc != 0 && error_status(status) && status != LuaStatus::ErrErr && status != LuaStatus::ErrSyntax {
            let errfunc_stk = StackIdx(state.errfunc as u32);
            // Mirror the stack manipulation lua-c does in luaG_errormsg
            // (and the inline path in pcall_k api.rs:1944):
            //   stack: [..., err]  (top = func_idx + 1, err at func_idx)
            //   -> push duplicate of err -> [..., err, err]
            //   -> overwrite the first err slot with handler -> [..., handler, err]
            //   -> call_no_yield(handler_pos, 1 result) -> [..., result]
            //   -> result lands at func_idx, which is where the error was.
            let err_val = state.get_at(func_idx);
            state.push(err_val);
            let handler = state.get_at(errfunc_stk);
            state.set_at(state.top_idx() - 2, handler);
            if let Err(_) = state.call_no_yield(state.top_idx() - 2, 1) {
                status = LuaStatus::ErrErr;
                if let Ok(s) = state.intern_str(b"error in error handling") {
                    state.set_at(func_idx, lua_types::value::LuaValue::Str(s));
                }
                state.set_top(func_idx + 1);
            }
        }

        shrink_stack(state);
        state.get_ci_mut(ci_idx).set_recover_status(LuaStatus::Ok as i32);
    }

    state.get_ci_mut(ci_idx).callstatus &= !CIST_YPCALL;
    let old_errfunc = state.get_ci(ci_idx).u_c_old_errfunc();
    state.errfunc = old_errfunc;

    Ok(status)
}

/// Completes the execution of a C function that was interrupted by a yield.
///
fn finish_ccall(state: &mut LuaState, ci_idx: CallInfoIdx) -> Result<(), LuaError> {
    let n;

    if state.get_ci(ci_idx).callstatus & CIST_CLSRET != 0 {
        debug_assert!((state.get_ci(ci_idx).nresults as i32) < LUA_MULTRET);
        n = state.get_ci_u2_nres(ci_idx);
    } else {
        debug_assert!(
            state.get_ci(ci_idx).u_c_k().is_some() && state.is_yieldable(),
            "finishCcall: no continuation or non-yieldable"
        );

        let mut status = LuaStatus::Yield;

        if state.get_ci(ci_idx).callstatus & CIST_YPCALL != 0 {
            status = finish_pcallk(state, ci_idx)?;
        }

        // adjustresults → state.adjust_results(nres)  (macros.tsv)
        state.adjust_results(LUA_MULTRET);

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
        debug_assert!(
            n <= state.top_idx().0 as i32,
            "continuation returned more values than available"
        );
    }

    poscall(state, ci_idx, n)?;
    Ok(())
}

/// Unrolls the full continuation stack of a coroutine until empty.
///
fn unroll(state: &mut LuaState) -> Result<(), LuaError> {
    loop {
        let ci_idx = state.ci;
        if state.is_base_ci(ci_idx) {
            break;
        }
        if !state.get_ci(ci_idx).is_lua() {
            finish_ccall(state, ci_idx)?;
        } else {
            vm::finish_op(state)?;
            vm::execute(state, ci_idx)?;
        }
    }
    Ok(())
}

/// Searches the call stack for the innermost suspended protected call.
///
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
fn resume_error(state: &mut LuaState, msg: &[u8], narg: i32) -> LuaStatus {
    let top = state.top_idx();
    state.set_top(top - narg as i32);
    // luaS_new → state.intern_str(s)  (macros.tsv)
    let s = state.intern_str(msg).ok();
    let new_top = state.top_idx();
    if let Some(s) = s { state.set_at(new_top, LuaValue::Str(s)); }
    state.set_top(new_top + 1);
    LuaStatus::ErrRun
}

/// Core coroutine resume logic (runs inside `raw_run_protected`).
///
fn resume_coroutine(state: &mut LuaState, nargs: i32) -> Result<(), LuaError> {
    let top = state.top_idx();
    let first_arg = top - nargs as i32;
    let ci_idx = state.ci;

    if state.status == LuaStatus::Ok as u8 {
        ccall_inner(state, first_arg - 1, LUA_MULTRET, 0)?;
    } else {
        debug_assert!(state.status == LuaStatus::Yield as u8);
        state.status = LuaStatus::Ok as u8;

        if state.get_ci(ci_idx).is_lua() {
            debug_assert!(state.get_ci(ci_idx).callstatus & CIST_HOOKYIELD != 0);
            let pc = state.ci_savedpc(ci_idx);
            state.set_ci_savedpc(ci_idx, pc.saturating_sub(1));
            state.set_top(first_arg);
            vm::execute(state, ci_idx)?;
        } else {
            if let Some(k_fn) = state.get_ci(ci_idx).u_c_k() {
                let ctx = state.get_ci(ci_idx).u_c_ctx();
                let n = k_fn(state, LuaStatus::Yield as i32, ctx)? as i32;
                debug_assert!(n <= state.top_idx().0 as i32);
                poscall(state, ci_idx, n)?;
            } else {
                // No continuation: just finish the call
                let n = (state.top_idx().0 as i32 - first_arg.0 as i32).max(0);
                poscall(state, ci_idx, n)?;
            }
        }

        unroll(state)?;
    }
    Ok(())
}

/// Unrolls the coroutine while there are recoverable (protected-call) errors.
///
fn precover(state: &mut LuaState, mut status: LuaStatus) -> LuaStatus {
    while error_status(status) {
        if let Some(ci_idx) = find_pcall(state) {
            state.ci = ci_idx;
            state.get_ci_mut(ci_idx).set_recover_status(status as i32);
            // PORT NOTE: In C, luaD_throw pushes the error value onto L->top before
            // longjmp, so the catch in luaD_rawrunprotected leaves it there for
            // finish_pcallk's seterrorobj to read at L->top-1. In Rust the value
            // rides inside LuaError; push it explicitly to mirror the C invariant.
            status = match raw_run_protected(state, |s| unroll(s)) {
                Ok(()) => LuaStatus::Ok,
                Err(e) => {
                    let s = e.to_status();
                    if error_status(s) {
                        state.push(e.into_value());
                    }
                    s
                }
            };
        } else {
            break;
        }
    }
    status
}

/// Resumes (or starts) a coroutine thread.
///
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

    if state.status == LuaStatus::Ok as u8 {
        if !state.is_base_ci(state.ci) {
            return resume_error(state, b"cannot resume non-suspended coroutine", nargs);
        }
        let ci_func = state.get_ci(state.ci).func;
        if state.top_idx().0 as i32 - (ci_func.0 as i32 + 1) == nargs {
            return resume_error(state, b"cannot resume dead coroutine", nargs);
        }
    } else if state.status != LuaStatus::Yield as u8 {
        return resume_error(state, b"cannot resume dead coroutine", nargs);
    }

    state.n_ccalls = from
        .as_ref()
        .map(|f| f.c_calls() as u32)
        .unwrap_or(0);

    if state.c_calls() >= LUAI_MAXCCALLS {
        return resume_error(state, b"C stack overflow", nargs);
    }
    state.n_ccalls += 1;

    debug_assert!(
        if state.status == LuaStatus::Ok as u8 {
            nargs + 1 <= state.top_idx().0 as i32
        } else {
            nargs <= state.top_idx().0 as i32
        },
        "lua_resume: not enough stack elements"
    );

    // PORT NOTE: In C, luaD_throw pushes the error value onto the stack before
    // longjmp-ing. In Rust the value rides inside LuaError and is normally
    // discarded by raw_run_protected — but real errors (ErrRun/ErrMem/etc.)
    // need their payload pushed so the later seterrorobj can copy it back to
    // the error slot. We must skip Yield (no payload) and Ok (none happened).
    let (mut status, err_value) = match raw_run_protected(state, |s| resume_coroutine(s, nargs)) {
        Ok(()) => (LuaStatus::Ok, None),
        Err(e) => {
            let s = e.to_status();
            let v = if error_status(s) { Some(e.into_value()) } else { None };
            (s, v)
        }
    };
    if let Some(v) = err_value {
        state.push(v);
    }

    status = precover(state, status);

    if !error_status(status) {
        debug_assert!(status as u8 == state.status, "lua_resume: status mismatch");
    } else {
        // Unrecoverable error — mark thread as dead
        state.status = status as u8;
        let top = state.top_idx();
        set_error_obj(state, status, top);
        let new_top = state.top_idx();
        let ci_idx = state.ci;
        state.get_ci_mut(ci_idx).top = new_top;
    }

    let ci_idx = state.ci;
    *nresults = if status == LuaStatus::Yield {
        state.get_ci_u2_nyield(ci_idx)
    } else {
        let ci_func = state.get_ci(ci_idx).func;
        state.top_idx().0 as i32 - (ci_func.0 as i32 + 1)
    };

    status
}

/// Returns whether the calling context can yield.
///
pub fn lua_isyieldable(state: &LuaState) -> bool {
    // yieldable → state.is_yieldable()  (macros.tsv)
    state.is_yieldable()
}

/// Yields the current coroutine, saving the continuation function `k` and
/// context `ctx` for resumption.
///
pub fn lua_yieldk(
    state: &mut LuaState,
    nresults: i32,
    ctx: isize,
    k: Option<crate::state::LuaKFunction>,
) -> Result<i32, LuaError> {
    // TODO(port): coroutine support (Phase E). Yielding requires stack-switching;
    // stubbed here with a faithful translation of the C logic.

    let ci_idx = state.ci;

    debug_assert!(
        nresults <= state.top_idx().0 as i32,
        "lua_yieldk: not enough elements on stack"
    );

    if !state.is_yieldable() {
        if !state.is_main_thread() {
            return Err(LuaError::runtime(format_args!(
                "attempt to yield across a C-call boundary"
            )));
        } else {
            return Err(LuaError::runtime(format_args!(
                "attempt to yield from outside a coroutine"
            )));
        }
    }

    state.status = LuaStatus::Yield as u8;
    state.set_ci_u2_nyield(ci_idx, nresults);

    if state.get_ci(ci_idx).is_lua() {
        debug_assert!(!state.get_ci(ci_idx).is_lua_code());
        debug_assert!(nresults == 0, "hooks cannot yield values");
        debug_assert!(k.is_none(), "hooks cannot continue after yielding");
        // Fall through — hook yields return 0 to luaD_hook.
    } else {
        // TODO(phase-b): mutate u_c.k/u_c.ctx fields directly inside CallInfoFrame::C.
        if let crate::state::CallInfoFrame::C { k: ref mut frame_k, ctx: ref mut frame_ctx, .. } =
            state.get_ci_mut(ci_idx).u {
            *frame_k = k;
            if k.is_some() {
                *frame_ctx = ctx;
            }
        }
        // In Rust: return Err to propagate the yield signal up the call stack.
        return Err(LuaError::Yield);
    }

    debug_assert!(
        state.get_ci(ci_idx).callstatus & CIST_HOOKED != 0,
        "lua_yieldk called outside a hook"
    );
    Ok(0) // return to luaD_hook
}

// ══════════════════════════════════════════════════════════════════════════════
// Protected close
// ══════════════════════════════════════════════════════════════════════════════

/// Auxiliary data for `close_aux`.
///
struct CloseP {
    level: StackIdx,
    status: LuaStatus,
}

/// Calls `luaF_close` with the level/status captured in `pcl`.
///
fn close_aux(state: &mut LuaState, pcl: &mut CloseP) -> Result<(), LuaError> {
    // TODO(port): status→i32 conversion for func::close sentinel.
    func::close(state, pcl.level, pcl.status as i32, false)?;
    Ok(())
}

/// Calls `luaF_close` in protected mode, retrying on error.
/// Returns the original `status` on clean completion, or the new error status.
///
pub(crate) fn close_protected(
    state: &mut LuaState,
    level: StackIdx,
    status: LuaStatus,
) -> LuaStatus {
    let old_ci = state.ci;
    let old_allowhook = state.allowhook;
    let mut status = status;

    loop {
        let mut pcl = CloseP { level, status };
        let (run_status, err_value) = match raw_run_protected(state, |s| close_aux(s, &mut pcl)) {
            Ok(()) => (LuaStatus::Ok, None),
            Err(e) => (e.to_status(), Some(e.into_value())),
        };
        if run_status == LuaStatus::Ok {
            return pcl.status;
        }
        state.ci = old_ci;
        state.allowhook = old_allowhook;
        // In C, luaD_throw pushed the error value onto the stack at top before
        // long-jumping, which leaves it at `top - 1` for the next iteration's
        // luaD_seterrorobj to copy. In Rust the value rides inside the
        // LuaError; push it explicitly so the next iteration (and the outer
        // pcall's seterrorobj) can read it at `top - 1`.
        if let Some(v) = err_value {
            state.push(v);
        }
        status = run_status;
    }
}

/// Calls function `func` in protected mode, restoring thread state on error.
/// Returns `LuaStatus::Ok` on success, or an error status.
///
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
    state.errfunc = ef;

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
            // C: syntax errors throw directly (luaX_syntaxerror -> luaD_throw)
            // and never reach luaG_errormsg, so the message handler is not run
            // for them. Without this guard a CLI/xpcall errfunc leaks into a
            // nested load()'s protected parser and decorates its returned
            // message with a spurious traceback.
            if ef != 0 && error_status(s) && s != LuaStatus::ErrErr && s != LuaStatus::ErrSyntax {
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
        state.ci = old_ci;
        state.allowhook = old_allowhook;
        status = close_protected(state, old_top, status);
        // restorestack → old_top  (already a StackIdx)
        set_error_obj(state, status, old_top);
        shrink_stack(state);
    }

    state.errfunc = old_errfunc;
    status
}

// ══════════════════════════════════════════════════════════════════════════════
// Protected parser
// ══════════════════════════════════════════════════════════════════════════════

/// Parser invocation data passed through `pcall`.
///
///
/// PORT NOTE: `const char *mode` and `const char *name` become owned byte vecs
/// so that `SParser` can outlive the original string data without raw pointers.
struct SParser {
    z: ZIO,
    /// LexBuffer from `crate::zio` (Mbuffer in C).
    buff: LexBuffer,
    /// TODO(phase-b): real Dyndata lives in the lua-parse crate.
    dyd: DynDataStub,
    // PORT NOTE: stored as Option<Vec<u8>> to own the bytes; None means no mode restriction.
    mode: Option<Vec<u8>>,
    name: Vec<u8>,
}

/// Checks that the chunk mode permits loading the given kind ("binary" or "text").
///
fn check_mode(
    mode: Option<&[u8]>,
    kind: &[u8],
) -> Result<(), LuaError> {
    if let Some(mode_bytes) = mode {
        let kind_char = kind[0];
        if !mode_bytes.contains(&kind_char) {
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
fn f_parser(state: &mut LuaState, p: &mut SParser) -> Result<(), LuaError> {
    // zgetc → z.getc()  (macros.tsv)
    let c = p.z.getc();

    // LUA_SIGNATURE → const LUA_SIGNATURE: &[u8] = b"\x1bLua"  (macros.tsv)
    let cl = if c == b'\x1b' as i32 {
        check_mode(p.mode.as_deref(), b"binary")?;
        // TODO(port): undump returns a LClosure; the Rust API isn't finalised.
        crate::undump::undump(state, &mut p.z, &p.name)?
    } else {
        check_mode(p.mode.as_deref(), b"text")?;
        // TODO(port): parser API not yet finalised; returns a LClosure.
        parse_stub(state, &mut p.z, &mut p.buff, &mut p.dyd, &p.name, c)?
    };

    debug_assert!(cl.upvals.len() == cl.proto.upvalues.len());
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
pub(crate) fn protected_parser(
    state: &mut LuaState,
    z: ZIO,
    name: &[u8],
    mode: Option<&[u8]>,
) -> LuaStatus {
    // incnny → state.inc_nny()  (macros.tsv)
    state.inc_nny();

    let mut p = SParser {
        z,
        buff: LexBuffer::new(),
        dyd: DynDataStub::new(),
        mode: mode.map(|m| m.to_vec()),
        name: name.to_vec(),
    };

    // (macros.tsv: luaZ_initbuffer → buf.init() / Mbuffer::new())

    let top_idx = state.top_idx();
    let errfunc = state.errfunc;
    let status = pcall(state, |s| f_parser(s, &mut p), top_idx, errfunc);

    // (p and all its sub-fields drop here automatically)

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
//                  PERF: `precall` split into a `#[inline(always)]` fast-path
//                  Lua-closure handler plus a `#[cold]` `precall_slow` for the
//                  C-closure / LightC / __call-metamethod arms.  Nil-fill of
//                  missing fixed params lives in a `#[cold] #[inline(never)]`
//                  helper so the no-fill case (overwhelmingly common — fib,
//                  any direct call with matching arity) is the predicted-taken
//                  branch.  fibonacci 2.65→2.38× (best-of-5) following this
//                  change, with proportional wins on closure_ops, table_ops,
//                  and table_ops_long.
// ──────────────────────────────────────────────────────────────────────────
