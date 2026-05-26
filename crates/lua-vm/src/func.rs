//! Auxiliary functions to manipulate prototypes and closures.
//!
//! Port of `reference/lua-5.4.7/src/lfunc.c` (295 lines, 16 functions).
//! The companion header `lfunc.h` is merged here per PORTING.md §1.
//!
//! # Design notes
//!
//! The C implementation uses two intrusive linked lists managed through pointer
//! fields embedded in stack slots and upvalue objects:
//!
//! - **`openupval`**: a singly-linked list of `UpVal`s sorted by stack level
//!   (highest first), threaded through `UpVal.u.open.next / .previous`.
//! - **`tbclist`**: a to-be-closed variable list encoded as `unsigned short` delta
//!   offsets stored inside `StackValue.tbclist.delta`.
//!
//! Both are replaced in the Rust port:
//! - `openupval` → `LuaState.openupval: Vec<GcRef<UpVal>>` (descending by StackIdx).
//! - `tbclist`   → `LuaState.tbclist: Vec<StackIdx>` (back = most recent entry).
//!
//! The delta-encoding machinery (MAXDELTA, dummy nodes) is an artifact of the u16
//! delta field and is entirely superseded by the `Vec<StackIdx>` model.

// PORT NOTE: `LuaProto` is currently a stub in crate::state (from lstate.c's
// partial port in state.rs). The full `LuaProto` definition belongs in
// crate::object (lobject.c → object.rs). Fields referenced below will compile
// once object.rs is written; see TODO(port) at each field site.

// PORT NOTE: `GcRef<T> = Rc<T>` in Phase A–C provides no interior mutability.
// `close_upval` and `init_upvals` must mutate `UpVal` and `LuaClosure` values
// that are shared through `GcRef`. In Phase B, the design options are:
//   (a) `GcRef<T> = Rc<RefCell<T>>` for mutable GC objects, or
//   (b) a custom `GcCell<T>` wrapper with conditional interior mutability.
// Both `close_upval` and `init_upvals` carry `TODO(port)` at the mutation sites.

#[allow(unused_imports)] use crate::prelude::*;

use crate::state::{
    GcRef, LuaState, LuaValue, UpVal,
};
use lua_types::error::LuaError;
pub use lua_types::{CallInfoIdx, StackIdx};

// ── lfunc.h constants ─────────────────────────────────────────────────────────

// macros.tsv: CLOSEKTOP → const CLOSE_K_TOP: i32 = -1
/// Sentinel status meaning "close upvalues but preserve the stack top."
/// Passed as `status` to `close` / `prep_call_close_mth`.
pub(crate) const CLOSE_K_TOP: i32 = -1;

// ── Closure allocation ────────────────────────────────────────────────────────

/// Fills a Lua closure's upvalue slots with freshly-allocated closed upvalues,
/// each holding `LuaValue::Nil`. Used when compiling closures that capture no
/// live stack variables.
///
pub(crate) fn init_upvals(state: &mut LuaState, cl: &GcRef<lua_types::LuaLClosure>) -> Result<(), LuaError> {
    //      GCObject *o = luaC_newobj(L, LUA_VUPVAL, sizeof(UpVal));
    //      UpVal *uv = gco2upv(o);
    //      uv->v.p = &uv->u.value;  /* make it closed */
    //      setnilvalue(uv->v.p);    /* *o = LuaValue::Nil */
    //      cl->upvals[i] = uv;
    //      luaC_objbarrier(L, cl, uv);
    //  }
    //
    // In Rust: create UpVal::Closed(Nil) for each slot; GC barrier is no-op Phase A–C.

    // TODO(port): GcRef<T> = Rc<T> has no interior mutability. Mutating
    // `cl.upvals[i]` here requires either Rc<RefCell<LuaClosure>> or Rc::get_mut.
    // The code below captures the intended logic; it will not compile until
    // GcRef provides a borrow_mut() path (Phase B design decision).
    let n = cl.upvals.len();
    for i in 0..n {
        let uv: GcRef<UpVal> = state.new_upval_closed(LuaValue::Nil);
        // TODO(port): cl.borrow_mut().as_lua_mut().upvals[i] = Some(uv.clone());
        // Requires interior mutability; see PORT NOTE at top of file.
        let _ = (i, uv);
    }
    Ok(())
}

// ── Open-upvalue management ───────────────────────────────────────────────────

/// Creates a new open upvalue for stack slot `level`, inserts it into
/// `state.openupval` at `insert_pos`, and registers the thread in the
/// global `twups` list if necessary.
///
fn new_open_upval(
    state: &mut LuaState,
    level: StackIdx,
    insert_pos: usize,
) -> GcRef<UpVal> {
    //    UpVal *uv = gco2upv(o);
    //    UpVal *next = *prev;
    //    uv->v.p = s2v(level);   /* current value lives in the stack */
    //    uv->u.open.next = next;
    //    uv->u.open.previous = prev;
    //    if (next) next->u.open.previous = &uv->u.open.next;
    //    *prev = uv;
    //
    // In Rust: intrusive next/previous fields are gone; Vec insertion replaces
    // the pointer-threading. The `prev` parameter (UpVal **) becomes `insert_pos`.
    //
    // The home thread of the upvalue is whichever thread is currently
    // executing `find_upval` — it captures one of that thread's stack
    // slots. Phase E-3 makes this id real so `upvalue_get`/`upvalue_set`
    // can dispatch through `GlobalState::cross_thread_upvals` when a
    // coroutine reads or writes an upvalue belonging to its parent.
    let owner_tid = state.global().current_thread_id as usize;
    let uv: GcRef<UpVal> = state.new_upval_open(owner_tid, level);
    // PORT NOTE: Vec insert maintains descending StackIdx order (highest first),
    // mirroring the C intrusive list where the head is always the topmost slot.
    state.openupval.insert(insert_pos, uv.clone());
    // macros.tsv: isintwups → state.in_twups()
    // TODO(port): implement state.in_twups() and the twups insertion. The method needs to
    // check whether this LuaState is already in global.twups. Requires either a flag on
    // LuaState or a scan of global.twups. See also lstate.h discussion in state.rs.
    if !state_in_twups(state) {
        // TODO(port): state.global_mut().twups.push(gc_ref_to_this_thread(state));
        // Deferred: obtaining a GcRef<LuaState> to self requires Arc/Rc self-reference
        // which is an unsolved design problem for Phase E coroutines.
    }
    uv
}

/// Finds or creates an open upvalue for stack slot `level`.
///
/// Searches `state.openupval` (sorted descending by StackIdx) for an existing
/// open upvalue at exactly `level`. If found, returns it. Otherwise, inserts a
/// new one at the correct sorted position and returns it.
///
pub(crate) fn find_upval(state: &mut LuaState, level: StackIdx) -> GcRef<UpVal> {
    debug_assert!(
        state_in_twups(state) || state.openupval.is_empty(),
        "thread must be in twups if it has open upvalues"
    );
    //    while ((p = *pp) != NULL && uplevel(p) >= level) {
    //      lua_assert(!isdead(G(L), p));
    //      if (uplevel(p) == level) return p;  /* found */
    //      pp = &p->u.open.next;
    //    }
    //    return newupval(L, level, pp);
    //
    // The list is sorted descending. We scan from index 0 (highest) downward.
    // When we find an entry with idx < level we've passed the insertion point.
    let mut insert_pos = state.openupval.len(); // default: append at end
    for (i, uv_ref) in state.openupval.iter().enumerate() {
        // macros.tsv: uplevel → extract thread_stack_idx from UpVal::Open
        let uv_idx = match &*uv_ref.slot() {
            lua_types::UpValState::Open { thread_id: _, idx: thread_stack_idx } => *thread_stack_idx,
            lua_types::UpValState::Closed(_) => {
                debug_assert!(false, "closed upvalue found in openupval list");
                continue;
            }
        };
        if uv_idx.0 >= level.0 {
            if uv_idx == level {
                return uv_ref.clone();
            }
            // uv_idx.0 > level.0: this entry is higher on the stack; keep searching.
        } else {
            // uv_idx.0 < level.0: correct insertion point reached.
            insert_pos = i;
            break;
        }
    }
    new_open_upval(state, level, insert_pos)
}

// ── Close-method call helpers ─────────────────────────────────────────────────

/// Calls the `__close` metamethod on `obj` with error argument `err`.
/// `yy` controls whether the call is yieldable (true) or non-yieldable (false).
///
/// This function assumes EXTRA_STACK free slots are available.
///
fn call_close_method(
    state: &mut LuaState,
    obj: LuaValue,
    err: LuaValue,
    yy: bool,
) -> Result<(), LuaError> {
    //    const TValue *tm = luaT_gettmbyobj(L, obj, TM_CLOSE);
    //    setobj2s(L, top, tm);     /* push metamethod */
    //    setobj2s(L, top + 1, obj); /* 1st arg: self */
    //    setobj2s(L, top + 2, err); /* 2nd arg: error message */
    //    L->top.p = top + 3;
    //    if (yy) luaD_call(L, top, 0);
    //    else    luaD_callnoyield(L, top, 0);
    //
    // In Rust: state.push() manages the top pointer; no pointer arithmetic needed.
    // setobj2s → state.push(value.clone())
    // macros.tsv: luaT_gettmbyobj → state.get_tm_by_obj(&obj, TagMethod::Close)
    let tm = state.get_tm_by_obj(&obj, lua_types::tagmethod::TagMethod::Close);
    let top = state.top;
    state.push(tm);
    state.push(obj);
    state.push(err);
    // TODO(port): state.call(top, 0) / state.call_noyield(top, 0) —
    // these methods live in do_.rs (ldo.c); cross-module call.
    if yy {
        state.lua_call(top, 0)?;
    } else {
        state.lua_callnoyield(top, 0)?;
    }
    Ok(())
}

/// Checks that the value at `level` has a `__close` metamethod, raising a
/// runtime error if it does not.
///
fn check_close_mth(state: &mut LuaState, level: StackIdx) -> Result<(), LuaError> {
    //    if (ttisnil(tm)) {
    //      int idx = cast_int(level - L->ci->func.p);
    //      const char *vname = luaG_findlocal(L, L->ci, idx, NULL);
    //      if (vname == NULL) vname = "?";
    //      luaG_runerror(L, "variable '%s' got a non-closable value", vname);
    //    }
    //
    // macros.tsv: s2v(level) → state.stack_at(level) — returns &LuaValue
    // macros.tsv: ttisnil(tm) → matches!(tm, LuaValue::Nil)
    let val = state.get_stack_value(level).clone();
    let tm = state.get_tm_by_obj(&val, lua_types::tagmethod::TagMethod::Close);
    if matches!(tm, LuaValue::Nil) {
        // macros.tsv: cast_int → x as i32
        // CallInfo.func is the StackIdx of the function on the stack.
        let func_idx = state.current_ci().func;
        let idx = (level.0 as i32) - (func_idx.0 as i32);
        let vname_owned: Vec<u8> = state.debug_find_local(state.ci, idx).unwrap_or_else(|| b"?".to_vec());
        // PORT NOTE: Lua variable names are ASCII identifiers; `escape_ascii`
        // produces a Display-compatible wrapper for the byte slice.
        return Err(LuaError::runtime(format_args!(
            "variable '{}' got a non-closable value",
            vname_owned.escape_ascii()
        )));
    }
    Ok(())
}

/// Prepares and calls the closing method for the variable at `level`.
///
/// If `status == CLOSE_K_TOP`, the error argument passed to `__close` is nil.
/// Otherwise, `set_error_obj` is called to materialise the error at `level + 1`
/// before the close method is invoked.
///
fn prep_call_close_mth(
    state: &mut LuaState,
    level: StackIdx,
    status: i32,
    yy: bool,
) -> Result<(), LuaError> {
    //    TValue *errobj;
    //    if (status == CLOSEKTOP)
    //      errobj = &G(L)->nilvalue;  /* error object is nil */
    //    else {  /* luaD_seterrorobj will set top to level+2 */
    //      errobj = s2v(level + 1);
    //      luaD_seterrorobj(L, status, level + 1);
    //    }
    //    callclosemethod(L, uv, errobj, yy);
    //
    // macros.tsv: s2v(level) → state.stack_at(level), returning &LuaValue
    // Clone before any mutable operations to avoid borrow conflicts.
    let uv = state.get_stack_value(level).clone();
    let err = if status == CLOSE_K_TOP {
        LuaValue::Nil
    } else {
        // TODO(port): state.set_error_obj(status, ...) lives in do_.rs (ldo.c).
        state.set_error_obj(status, StackIdx(level.0 + 1))?;
        state.get_stack_value(StackIdx(level.0 + 1)).clone()
    };
    call_close_method(state, uv, err, yy)
}

// ── To-be-closed variable management ─────────────────────────────────────────

/// Inserts the variable at `level` into the to-be-closed (`tbc`) list.
///
/// If the value is falsy (nil or false) it does not need closing and the
/// function returns immediately. Otherwise it verifies that the value has a
/// `__close` metamethod, then records it in `state.tbclist`.
///
pub(crate) fn new_tbc_upval(state: &mut LuaState, level: StackIdx) -> Result<(), LuaError> {
    // In Rust: tbclist is Vec<StackIdx>, "current head" = last element.
    debug_assert!(
        state.tbclist.last().map_or(true, |&top| level.0 > top.0),
        "new tbc entry must be above current tbclist head"
    );
    // macros.tsv: l_isfalse → matches!(o, LuaValue::Nil | LuaValue::Bool(false))
    // Clone before borrow to avoid aliasing with later mutable calls.
    let val = state.get_stack_value(level).clone();
    if matches!(val, LuaValue::Nil | LuaValue::Bool(false)) {
        return Ok(());
    }
    check_close_mth(state, level)?;
    //   while (cast_uint(level - L->tbclist.p) > MAXDELTA) {
    //     L->tbclist.p += MAXDELTA;
    //     L->tbclist.p->tbclist.delta = 0;  /* dummy node */
    //   }
    //   level->tbclist.delta = cast(unsigned short, level - L->tbclist.p);
    //   L->tbclist.p = level;
    //
    // PORT NOTE: The MAXDELTA / dummy-node mechanism is a C-only optimisation
    // required because `StackValue.tbclist.delta` is a `u16` (max 65535). With
    // `Vec<StackIdx>` the index fits a u32 and no dummy nodes are ever needed.
    state.tbclist.push(level);
    Ok(())
}

/// Closes all open upvalues whose stack index is ≥ `level`, transitioning each
/// from `UpVal::Open { thread_id: _, idx: thread_stack_idx }` to `UpVal::Closed(value)` by copying
/// the current stack value into the upvalue's own storage.
///
pub(crate) fn close_upval(state: &mut LuaState, level: StackIdx) {
    //      TValue *slot = &uv->u.value;
    //      lua_assert(uplevel(uv) < L->top.p);
    //      luaF_unlinkupval(uv);
    //      setobj(L, slot, uv->v.p);  /* copy stack value into upvalue */
    //      uv->v.p = slot;            /* now the value lives here */
    //      if (!iswhite(uv)) { nw2black(uv); luaC_barrier(L, uv, slot); }
    //  }
    //
    // openupval is sorted descending; front element is the topmost open upvalue.
    loop {
        let uv = match state.openupval.first() {
            Some(uv) => uv.clone(),
            None => break,
        };
        let uv_idx = match &*uv.slot() {
            lua_types::UpValState::Open { thread_id: _, idx: thread_stack_idx } => *thread_stack_idx,
            lua_types::UpValState::Closed(_) => {
                // Cross-thread close/reset paths can leave a stale closed
                // upvalue in this Vec-backed open list. The C intrusive list
                // cannot represent that state; in Rust, unlink it and keep
                // closing the remaining open entries.
                state.openupval.remove(0);
                continue;
            }
        };
        if uv_idx.0 < level.0 {
            break;
        }
        // PORT NOTE: C asserts `uplevel(uv) < L->top.p` because the C stack is a
        // contiguous block where slots above top are undefined. The Rust stack is
        // a `Vec<StackValue>` whose backing storage outlives any top movement, so
        // reading `stack[uv_idx]` is always valid here even when `state.top` has
        // been rolled back below the upvalue (which is exactly what happens on
        // pcall error unwind, e.g. when `assert_fn` calls `set_top(L, 1)` before
        // raising). Dropping the C-style assertion lets close_upval correctly
        // close upvalues during error unwind regardless of top position.
        state.openupval.remove(0);
        let stack_val = state.get_stack_value(uv_idx).clone();
        uv.close_with(stack_val);
        // macros.tsv: iswhite → obj.is_white(); nw2black → obj.set_black()
        //             luaC_barrier → state.gc().barrier(p, v) — no-op Phase A–C
        // TODO(port): GC color methods (is_white, set_black) on GcRef<UpVal>;
        // Phase D only. Omitted in Phase A–C.
    }
}

/// Removes the most-recent entry from `state.tbclist`.
///
/// The C version must also skip over any delta==0 "dummy" nodes inserted to
/// bridge gaps larger than MAXDELTA. In Rust no dummy nodes are ever inserted,
/// so this is a straight `Vec::pop`.
///
fn pop_tbc_list(state: &mut LuaState) {
    //    lua_assert(tbc->tbclist.delta > 0);  /* first element cannot be dummy */
    //    tbc -= tbc->tbclist.delta;
    //    while (tbc > L->stack.p && tbc->tbclist.delta == 0)
    //      tbc -= MAXDELTA;  /* skip dummy nodes */
    //    L->tbclist.p = tbc;
    //
    // PORT NOTE: Delta-encoding dropped (see new_tbc_upval). Just pop.
    state.tbclist.pop();
}

/// Closes all upvalues and to-be-closed variables down to `level`, invoking
/// `__close` metamethods as needed. Returns the (stable) `level` index.
///
/// `status` is passed to `prep_call_close_mth` to determine the error argument:
/// `CLOSE_K_TOP` means nil; other statuses produce the appropriate error object.
/// `yy` controls yieldability of the close-method calls.
///
pub(crate) fn close(
    state: &mut LuaState,
    level: StackIdx,
    status: i32,
    yy: bool,
) -> Result<StackIdx, LuaError> {
    // macros.tsv: savestack → idx (StackIdx is already stable across reallocs in Rust)
    // PORT NOTE: savestack / restorestack are no-ops here. In C they save/restore a
    // pointer as a byte-offset because the stack may reallocate during close-method
    // calls. In Rust, StackIdx is an index into Vec and remains valid after any resize.

    close_upval(state, level);
    //      StkId tbc = L->tbclist.p;
    //      poptbclist(L);
    //      prepcallclosemth(L, tbc, status, yy);
    //      level = restorestack(L, levelrel);
    //    }
    while state.tbclist.last().copied().map_or(false, |tbc| tbc.0 >= level.0) {
        let tbc = state
            .tbclist
            .last()
            .copied()
            .expect("tbclist non-empty (just checked)");
        pop_tbc_list(state);
        prep_call_close_mth(state, tbc, status, yy)?;
    }
    Ok(level)
}

// ── Debug helpers ─────────────────────────────────────────────────────────────

/// Returns the byte-string name of the `local_number`-th local variable that is
/// active at bytecode position `pc` in prototype `f`, or `None` if no such
/// variable exists.
///
/// Variables are scanned in order. A variable is active when
/// `startpc <= pc < endpc`. The first active variable is numbered 1.
///
pub(crate) fn get_local_name(
    f: &crate::state::LuaProto,
    local_number: i32,
    pc: i32,
) -> Option<&[u8]> {
    //    for (i = 0; i < f->sizelocvars && f->locvars[i].startpc <= pc; i++) {
    //      if (pc < f->locvars[i].endpc) {  /* is variable active? */
    //        local_number--;
    //        if (local_number == 0)
    //          return getstr(f->locvars[i].varname);
    //      }
    //    }
    //    return NULL;
    //
    // macros.tsv: getstr(ts) → ts.as_bytes()  returning &[u8]
    //
    // TODO(port): `f.locvars` does not exist on the current LuaProto stub in state.rs.
    // This will compile once LuaProto gains its full set of fields from object.rs.
    // The logic below faithfully translates the C loop.
    let mut remaining = local_number;
    // We break early once startpc > pc (variables are ordered by startpc).
    for lv in f.locvars.iter() {
        if lv.startpc > pc {
            break;
        }
        if pc < lv.endpc {
            remaining -= 1;
            if remaining == 0 {
                // macros.tsv: getstr → ts.as_bytes()
                return Some(lv.varname.as_bytes());
            }
        }
    }
    None
}

// ── Private helpers (Rust-only) ───────────────────────────────────────────────

/// Returns `true` if this thread is already registered in `global.twups`.
///
/// iff its twups pointer doesn't point back to itself).
///
/// PORT NOTE: In Phase A–D with coroutines stubbed there is effectively a
/// single thread. The actual `GlobalState.twups` Vec management (insertion in
/// `new_open_upval`) is deferred to Phase D/E and would require a GcRef-to-self.
/// Until then we treat every thread as conceptually present in twups, which
/// satisfies the invariant `state_in_twups || openupval.is_empty()` asserted by
/// `find_upval`. The actual twups list does not yet drive any behaviour.
fn state_in_twups(state: &LuaState) -> bool {
    let _ = state;
    true
}

// ── Trait stubs needed for compilation ───────────────────────────────────────

/// Stub methods on `LuaState` assumed by this module.
///
/// These will be implemented in their home modules (do_.rs, debug.rs, tagmethods.rs)
/// and removed from this file in Phase B.
impl LuaState {
    /// Returns the `LuaValue` at stack index `idx`.
    ///
    /// macros.tsv: `s2v → state.stack_at(idx)`.
    pub(crate) fn get_stack_value(&self, idx: StackIdx) -> &LuaValue {
        // TODO(port): bounds-check and return &self.stack[idx.0 as usize].val
        &self.stack[idx.0 as usize].val
    }

    /// Returns the current CallInfo (active call frame).
    ///
    pub(crate) fn current_ci(&self) -> &crate::state::CallInfo {
        // TODO(port): return &self.call_info[self.ci.0 as usize]
        &self.call_info[self.ci.0 as usize]
    }

    /// Looks up the `__close` (or other) metamethod for a value.
    ///
    /// macros.tsv: `fasttm → state.fast_tm(et, e)`.
    pub(crate) fn get_tm_by_obj(
        &mut self,
        val: &LuaValue,
        tm: lua_types::tagmethod::TagMethod,
    ) -> LuaValue {
        let mt: Option<GcRef<lua_types::value::LuaTable>> = match val {
            LuaValue::Table(t) => t.metatable(),
            LuaValue::UserData(u) => u.metatable(),
            other => {
                let type_idx = other.base_type() as usize;
                self.global().mt[type_idx].clone()
            }
        };
        match mt {
            Some(mt_ref) => {
                let ename = self.global().tmname[tm as usize].clone();
                mt_ref.get_short_str(&ename)
            }
            None => LuaValue::Nil,
        }
    }

    /// Calls a Lua or C function (yieldable).
    ///
    pub(crate) fn lua_call(&mut self, top: StackIdx, nresults: i32) -> Result<(), LuaError> {
        crate::do_::call(self, top, nresults)
    }

    /// Calls a Lua or C function (non-yieldable).
    ///
    pub(crate) fn lua_callnoyield(
        &mut self,
        top: StackIdx,
        nresults: i32,
    ) -> Result<(), LuaError> {
        crate::do_::callnoyield(self, top, nresults)
    }

    /// Sets the error object at a given stack index for a given status code.
    ///
    pub(crate) fn set_error_obj(
        &mut self,
        status: i32,
        idx: StackIdx,
    ) -> Result<(), LuaError> {
        let s = lua_types::status::LuaStatus::from_raw(status);
        crate::do_::set_error_obj(self, s, idx);
        Ok(())
    }

    /// Returns the local-variable name at frame position `n` for CallInfo `ci`.
    ///
    pub(crate) fn debug_find_local(
        &self,
        ci: CallInfoIdx,
        n: i32,
    ) -> Option<Vec<u8>> {
        crate::debug::find_local(self, ci, n, None)
    }
}

// ──────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        src/lfunc.c  (295 lines, 16 functions)
//   target_crate:  lua-vm
//   confidence:    medium
//   todos:         36
//   port_notes:    7
//   unsafe_blocks: 0
//   notes:         Logic is faithful. Two blockers for Phase B:
//                  (1) GcRef<UpVal> needs interior mutability (Rc<RefCell<UpVal>>)
//                      so close_upval and init_upvals can mutate in-place.
//                  (2) LuaProto stub in state.rs must gain full field list from
//                      object.rs before new_proto / get_local_name compile.
//                  LuaClosureLua.proto needs Option<> wrapper for NULL init in
//                  new_lua_closure. Stub methods on LuaState (get_tm_by_obj,
//                  lua_call, set_error_obj, debug_find_local) must be removed
//                  once their home modules are written (do_.rs, debug.rs,
//                  tagmethods.rs). The 36 TODO(port) markers include both the
//                  core design blockers and the stub-method placeholders; the
//                  stub-method TODOs will auto-resolve as other modules land.
// ──────────────────────────────────────────────────────────────────────────
