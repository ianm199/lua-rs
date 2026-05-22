// C: lapi.c — Lua public C API
// C: $Id: lapi.c $
// C: See Copyright Notice in lua.h
//
// PORT NOTE: This is the Rust-native translation of lapi.c.
// The C-API surface (lua_State *, int stack-index protocol) is replaced by
// methods on LuaState.  `lua_lock` / `lua_unlock` are dropped (no-op in the
// single-threaded default build).  `api_incr_top` is dropped; `state.push()`
// already increments.  Stack pointers (StkId) become StackIdx (u32).

#![allow(dead_code)]

use std::convert::Infallible;
#[allow(unused_imports)] use crate::prelude::*;

use crate::state::{LuaState, LuaCFunction, GlobalState, CallInfo, CallInfoIdx, StackIdx,
    LuaValueExt, LuaTypeExt, StackIdxExt,
    LuaTableRefExt, LuaUserDataRefExt, LuaStringRefExt,
    LuaLClosureRefExt, LuaClosureExt, LuaProtoExt};
use lua_types::{
    LuaValue, LuaType, LuaError, LuaString, LuaUserData, LuaClosure, UpVal,
    GcRef, LuaStatus,
};
use lua_types::value::LuaTable;

// C: const char lua_ident[] = "$LuaVersion: " LUA_COPYRIGHT " $" ...
pub const LUA_IDENT: &[u8] =
    b"$LuaVersion: Lua 5.4.7  Copyright (C) 1994-2024 Lua.org, PUC-Rio $\
      $LuaAuthors: R. Ierusalimschy, L. H. Figueiredo, W. Celes $";

// C: #define LUA_REGISTRYINDEX  (-LUAI_MAXSTACK - 1000)
const LUA_REGISTRYINDEX: i32 = -(1_000_000) - 1000;

// C: #define LUA_MULTRET  (-1)
const LUA_MULTRET: i32 = -1;

// C: #define LUA_RIDX_GLOBALS  2
const LUA_RIDX_GLOBALS: i64 = 2;

// C: #define MAXUPVAL  255
const MAX_UPVAL: u8 = 255;

// C: #define ispseudo(i)  ((i) <= LUA_REGISTRYINDEX)
#[inline]
fn is_pseudo(idx: i32) -> bool {
    idx <= LUA_REGISTRYINDEX
}

// C: #define isupvalue(i)  ((i) < LUA_REGISTRYINDEX)
#[inline]
fn is_upvalue(idx: i32) -> bool {
    idx < LUA_REGISTRYINDEX
}

// C: #define isvalid(L, o)  (!ttisnil(o) || o != &G(L)->nilvalue)
// PORT NOTE: In C, the only "invalid" TValue is the global nilvalue singleton
// pointer returned by index2value when the index is out of range. In Rust we
// cannot do pointer-equality on a singleton, so validity is decided by whether
// the index resolves to a real stack/upvalue slot — see `is_valid_index`.
#[inline]
fn is_valid_index(state: &LuaState, idx: i32) -> bool {
    if idx == 0 {
        return false;
    }
    let ci = state.current_call_info();
    if idx > 0 {
        let slot = ci.func + idx;
        slot.0 < state.top_idx().0
    } else if !is_pseudo(idx) {
        (-idx) as u32 <= state.top_idx().0.saturating_sub(ci.func.0 + 1)
    } else if idx == LUA_REGISTRYINDEX {
        true
    } else {
        let upval_n = (LUA_REGISTRYINDEX - idx) as usize;
        let func_val = state.get_at(ci.func);
        if let LuaValue::Function(LuaClosure::C(ref ccl)) = func_val {
            upval_n >= 1 && upval_n <= ccl.upvalues.len()
        } else {
            false
        }
    }
}

// ── index helpers ─────────────────────────────────────────────────────────────

// C: static TValue *index2value (lua_State *L, int idx)
// PORT NOTE: In Rust we cannot return a pointer; we return a cloned LuaValue.
// Writers use a companion index_to_stack_idx() for actual stack slots.
fn index_to_value(state: &LuaState, idx: i32) -> LuaValue {
    // C: CallInfo *ci = L->ci;
    let ci = state.current_call_info();
    if idx > 0 {
        // C: StkId o = ci->func.p + idx;
        let func_idx = ci.func;
        let slot = func_idx + idx;
        // C: api_check(L, idx <= ci->top.p - (ci->func.p + 1), "unacceptable index");
        debug_assert!(
            idx as u32 <= ci.top.saturating_sub(func_idx + 1),
            "unacceptable index"
        );
        // C: if (o >= L->top.p) return &G(L)->nilvalue;
        if slot.0 >= state.top_idx().0 {
            LuaValue::Nil
        } else {
            state.get_at(slot)
        }
    } else if !is_pseudo(idx) {
        // negative index
        // C: api_check(L, idx != 0 && -idx <= L->top.p - (ci->func.p + 1), "invalid index");
        debug_assert!(
            idx != 0,
            "invalid index"
        );
        // C: return s2v(L->top.p + idx);
        let top = state.top_idx();
        let slot = (top.0 as i32 + idx) as u32;
        state.get_at(slot)
    } else if idx == LUA_REGISTRYINDEX {
        // C: return &G(L)->l_registry;
        state.registry_value()
    } else {
        // upvalues: idx = LUA_REGISTRYINDEX - idx  (idx < LUA_REGISTRYINDEX)
        let upval_n = (LUA_REGISTRYINDEX - idx) as usize;
        debug_assert!(upval_n <= MAX_UPVAL as usize + 1, "upvalue index too large");
        // C: if (ttisCclosure(s2v(ci->func.p))) { ... }
        let func_val = state.get_at(ci.func);
        if let LuaValue::Function(LuaClosure::C(ref ccl)) = func_val {
            // C closure upvalue
            if upval_n >= 1 && upval_n <= ccl.upvalues.len() {
                ccl.upvalues[upval_n - 1].clone()
            } else {
                LuaValue::Nil
            }
        } else {
            // C: ttislcf or Lua fn called through hook — no upvalues
            LuaValue::Nil
        }
    }
}

// C: l_sinline StkId index2stack (lua_State *L, int idx)
// Returns a StackIdx for a valid (non-pseudo) actual stack slot.
#[inline]
fn index_to_stack_idx(state: &LuaState, idx: i32) -> StackIdx {
    let ci = state.current_call_info();
    if idx > 0 {
        let slot = ci.func + idx;
        debug_assert!(slot.0 < state.top_idx().0, "invalid index");
        slot
    } else {
        // C: api_check(L, idx != 0 && -idx <= L->top.p - (ci->func.p + 1), "invalid index");
        // C: api_check(L, !ispseudo(idx), "invalid index");
        debug_assert!(idx != 0 && !is_pseudo(idx), "invalid index");
        StackIdx((state.top_idx().0 as i32 + idx) as u32)
    }
}

// ── stack manipulation ────────────────────────────────────────────────────────

// C: LUA_API int lua_checkstack (lua_State *L, int n)
pub fn check_stack(state: &mut LuaState, n: i32) -> bool {
    // C: api_check(L, n >= 0, "negative 'n'");
    debug_assert!(n >= 0, "negative 'n'");
    let available = state.stack_available();
    let res = if available > n as usize {
        true
    } else {
        state.grow_stack(n, false).is_ok()
    };
    if res {
        // C: if (res && ci->top.p < L->top.p + n) ci->top.p = L->top.p + n;
        let needed_top = state.top_idx() + n as i32;
        let ci = state.current_call_info_mut();
        if ci.top.0 < needed_top.0 {
            ci.top = needed_top;
        }
    }
    res
}

// C: LUA_API void lua_xmove (lua_State *from, lua_State *to, int n)
// TODO(port): lua_xmove requires two &mut LuaState simultaneously which
// violates Rust's aliasing rules; this can only work if from != to and they
// share a GlobalState via Rc<RefCell<GlobalState>>. Stubbed for Phase A.
pub fn xmove(_from: &mut LuaState, _to: &mut LuaState, _n: i32) {
    // TODO(port): moving values between independent LuaState instances requires
    // split-borrow or Arc/Mutex approach.  Defer to Phase B.
    todo!("lua_xmove: cross-thread stack transfer not yet implemented")
}

// C: LUA_API lua_CFunction lua_atpanic (lua_State *L, lua_CFunction panicf)
pub fn at_panic(
    state: &mut LuaState,
    panicf: Option<fn(&mut LuaState) -> Result<usize, LuaError>>,
) -> Option<fn(&mut LuaState) -> Result<usize, LuaError>> {
    // C: old = G(L)->panic; G(L)->panic = panicf; return old;
    let old = state.global_mut().panic;
    state.global_mut().panic = panicf;
    old
}

// C: LUA_API lua_Number lua_version (lua_State *L)
pub fn version(_state: &LuaState) -> f64 {
    // C: UNUSED(L); return LUA_VERSION_NUM;
    504.0
}

// C: LUA_API int lua_absindex (lua_State *L, int idx)
pub fn abs_index(state: &LuaState, idx: i32) -> i32 {
    // C: return (idx > 0 || ispseudo(idx)) ? idx
    //          : cast_int(L->top.p - L->ci->func.p) + idx;
    if idx > 0 || is_pseudo(idx) {
        idx
    } else {
        let ci = state.current_call_info();
        (state.top_idx().0 as i32 - ci.func.0 as i32) + idx
    }
}

// C: LUA_API int lua_gettop (lua_State *L)
pub fn get_top(state: &LuaState) -> i32 {
    // C: return cast_int(L->top.p - (L->ci->func.p + 1));
    let ci = state.current_call_info();
    (state.top_idx().0 as i32) - (ci.func.0 as i32 + 1)
}

// C: LUA_API void lua_settop (lua_State *L, int idx)
pub fn set_top(state: &mut LuaState, idx: i32) -> Result<(), LuaError> {
    let func = state.current_call_info().func;
    let ci_top = state.current_call_info().top;
    if idx >= 0 {
        // C: api_check(L, idx <= ci->top.p - (func + 1), "new top too large");
        debug_assert!(
            idx as u32 <= ci_top.saturating_sub(func + 1),
            "new top too large"
        );
        let new_top = func + 1 + idx as i32;
        let old_top = state.top_idx();
        if new_top.0 > old_top.0 {
            // C: for (; diff > 0; diff--) setnilvalue(s2v(L->top.p++));
            for i in old_top.0..new_top.0 {
                state.set_at(i, LuaValue::Nil);
            }
        }
        // C: if (diff < 0 && L->tbclist.p >= newtop) { newtop = luaF_close(...) }
        // TODO(port): to-be-closed variable closing on stack shrink;
        // luaF_close not yet translated. Skipping close logic for Phase A.
        state.set_top_idx(new_top);
    } else {
        // C: api_check(L, -(idx+1) <= (L->top.p - (func + 1)), "invalid new top");
        debug_assert!(
            -(idx + 1) <= (state.top_idx().0 as i32 - (func.0 as i32 + 1)),
            "invalid new top"
        );
        // C: diff = idx + 1  (negative, will subtract)
        let new_top = (state.top_idx().0 as i32 + idx + 1) as u32;
        // TODO(port): to-be-closed variable closing on stack shrink (same as above)
        state.set_top_idx(new_top);
    }
    Ok(())
}

// C: LUA_API void lua_closeslot (lua_State *L, int idx)
pub fn close_slot(state: &mut LuaState, idx: i32) -> Result<(), LuaError> {
    // C: level = index2stack(L, idx);
    let level = index_to_stack_idx(state, idx);
    // C: api_check(L, hastocloseCfunc(L->ci->nresults) && L->tbclist.p == level, ...)
    // TODO(port): tbc-list check and luaF_close not yet translated.
    // C: level = luaF_close(L, level, CLOSEKTOP, 0);
    // C: setnilvalue(s2v(level));
    state.set_at(level, LuaValue::Nil);
    Ok(())
}

// C: l_sinline void reverse (lua_State *L, StkId from, StkId to)
#[inline]
fn reverse_segment(state: &mut LuaState, from: StackIdx, to: StackIdx) {
    // C: for (; from < to; from++, to--) { TValue temp; setobj...; setobjs2s...; setobj2s...; }
    let mut lo = from.0;
    let mut hi = to.0;
    while lo < hi {
        let temp = state.get_at(StackIdx(lo));
        let hi_val = state.get_at(StackIdx(hi));
        state.set_at(StackIdx(lo), hi_val);
        state.set_at(StackIdx(hi), temp);
        lo += 1;
        hi -= 1;
    }
}

// C: LUA_API void lua_rotate (lua_State *L, int idx, int n)
pub fn rotate(state: &mut LuaState, idx: i32, n: i32) {
    // C: t = L->top.p - 1;  (end of segment)
    let t = state.top_idx() - 1;
    // C: p = index2stack(L, idx);  (start of segment)
    let p = index_to_stack_idx(state, idx);
    // C: api_check(L, (n >= 0 ? n : -n) <= (t - p + 1), "invalid 'n'");
    debug_assert!((n.unsigned_abs() as i32) <= ((t.0 as i32) - (p.0 as i32) + 1), "invalid 'n'");
    // C: m = (n >= 0 ? t - n : p - n - 1);  (end of prefix)
    let m = if n >= 0 {
        t - n
    } else {
        StackIdx((p.0 as i32 - n - 1) as u32)
    };
    // C: reverse(L, p, m); reverse(L, m+1, t); reverse(L, p, t);
    reverse_segment(state, p, m);
    reverse_segment(state, m + 1, t);
    reverse_segment(state, p, t);
}

// C: LUA_API void lua_copy (lua_State *L, int fromidx, int toidx)
pub fn copy(state: &mut LuaState, fromidx: i32, toidx: i32) {
    // C: fr = index2value(L, fromidx); to = index2value(L, toidx);
    let fr = index_to_value(state, fromidx);
    // C: api_check(L, isvalid(L, to), "invalid index");
    // C: setobj(L, to, fr);
    // C: if (isupvalue(toidx)) luaC_barrier(L, clCvalue(s2v(L->ci->func.p)), fr);
    if is_upvalue(toidx) {
        // Writing to a function upvalue pseudo-index
        let upval_n = (LUA_REGISTRYINDEX - toidx) as usize;
        let func_val = state.get_at(state.current_call_info().func);
        if let LuaValue::Function(LuaClosure::C(ref ccl)) = func_val {
            // TODO(port): CClosure upvalue write requires interior mutability on GcRef<CClosure>
            // state.gc().barrier(ccl, &fr);
            let _ = (upval_n, ccl);
        }
        // TODO(port): implement upvalue write for copy() to C closure upvalues
    } else if toidx == LUA_REGISTRYINDEX {
        // TODO(port): write to registry — needs GlobalState::set_registry(fr)
    } else {
        let to_slot = index_to_stack_idx(state, toidx);
        state.set_at(to_slot, fr);
    }
}

// C: LUA_API void lua_pushvalue (lua_State *L, int idx)
pub fn push_value(state: &mut LuaState, idx: i32) {
    // C: setobj2s(L, L->top.p, index2value(L, idx)); api_incr_top(L);
    let v = index_to_value(state, idx);
    state.push(v);
}

/// Inherent `push_copy` so the `LuaStateStubExt::push_copy` default
/// `todo!()` no longer fires. Phase-A `state.push_copy(idx)` call-sites
/// (base.rs, etc.) duplicate the value at `idx` onto the top of the stack —
/// the same semantics as `lua_pushvalue`.
impl LuaState {
    pub fn push_copy(&mut self, idx: i32) -> Result<(), LuaError> {
        push_value(self, idx);
        Ok(())
    }

    pub fn push_value_at(&mut self, idx: i32) -> Result<(), LuaError> {
        push_value(self, idx);
        Ok(())
    }

    pub fn insert(&mut self, idx: i32) -> Result<(), LuaError> {
        rotate(self, idx, 1);
        Ok(())
    }

    /// Inherent `length_at` mirroring `luaL_len` from `lauxlib.c`: push the
    /// value's length onto the stack (honouring `__len`), pop it as an
    /// integer, and error if the result is not an integer. Defined on
    /// `LuaState` so it overrides the `LuaStateStubExt::length_at` trait
    /// default `todo!()`.
    pub fn length_at(&mut self, idx: i32) -> Result<i64, LuaError> {
        len(self, idx)?;
        let l = match to_integer_x(self, -1) {
            Some(n) => n,
            None => {
                return Err(LuaError::runtime(format_args!(
                    "object length is not an integer"
                )));
            }
        };
        self.pop_n(1);
        Ok(l)
    }

    /// Write `msg` bytes verbatim to standard output. Mirrors the C macro
    /// `lua_writestring(s, l) = fwrite(s, 1, l, stdout)` from `lauxlib.h`,
    /// used by `print` and friends. A failed write is propagated as a
    /// `LuaError::runtime`; this matches C-Lua's behaviour where an I/O
    /// error during `lua_writestring` would surface through the host's
    /// error handling.
    pub fn write_output(&mut self, msg: &[u8]) -> Result<(), LuaError> {
        use std::io::Write;
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        handle
            .write_all(msg)
            .map_err(|e| LuaError::runtime(format_args!("{}", e)))?;
        handle
            .flush()
            .map_err(|e| LuaError::runtime(format_args!("{}", e)))?;
        Ok(())
    }

    /// Convert the value at `idx` to a display string, push the result onto
    /// the stack, and return a copy of its bytes. Mirrors `luaL_tolstring`
    /// from `lauxlib.c`. The default Lua formatting is used for primitives
    /// (`"true"`/`"false"`/`"nil"`, `%I` integers, `%.14g` floats); for other
    /// reference types the result is `"<typename>: 0x<hex pointer>"`.
    ///
    /// If the value has a `__tostring` metamethod, it is invoked first and its
    /// (string) result is used in place of the default formatting (matching
    /// C: `luaL_callmeta(L, idx, "__tostring")` inside `luaL_tolstring`).
    pub fn to_display_string(&mut self, idx: i32) -> Result<Vec<u8>, LuaError> {
        let abs = abs_index(self, idx);
        let v = index_to_value(self, abs);
        let mt: Option<GcRef<LuaTable>> = match &v {
            LuaValue::Table(t) => t.metatable(),
            LuaValue::UserData(u) => u.metatable(),
            _ => self.global().mt[v.base_type() as usize].clone(),
        };
        if let Some(mt_ref) = mt {
            let key = self.intern_str(b"__tostring")?;
            let f = mt_ref.get_short_str(&key);
            if !matches!(f, LuaValue::Nil) {
                let func_idx = self.top_idx();
                self.push(f);
                self.push(v.clone());
                if self.current_ci().is_lua_code() {
                    self.do_call(func_idx, 1)?;
                } else {
                    self.do_call_no_yield(func_idx, 1)?;
                }
                let top = self.top_idx();
                let result = self.get_at(StackIdx(top.0 - 1));
                if let LuaValue::Str(s) = result {
                    return Ok(s.as_bytes().to_vec());
                }
                return Err(LuaError::runtime(format_args!(
                    "'__tostring' must return a string"
                )));
            }
        }
        let bytes: Vec<u8> = match &v {
            LuaValue::Str(s) => {
                let out = s.as_bytes().to_vec();
                self.push(LuaValue::Str(s.clone()));
                out
            }
            LuaValue::Int(_) | LuaValue::Float(_) => {
                let s = crate::object::num_to_string(self, &v)?;
                let out = s.as_bytes().to_vec();
                self.push(LuaValue::Str(s));
                out
            }
            LuaValue::Bool(b) => {
                let lit: &[u8] = if *b { b"true" } else { b"false" };
                let s = self.intern_str(lit)?;
                self.push(LuaValue::Str(s));
                lit.to_vec()
            }
            LuaValue::Nil => {
                let s = self.intern_str(b"nil")?;
                self.push(LuaValue::Str(s));
                b"nil".to_vec()
            }
            _ => {
                let kind = crate::tagmethods::obj_type_name(self, &v)?;
                let ptr = to_pointer(self, abs).unwrap_or(0);
                let mut buf = kind;
                buf.extend_from_slice(b": 0x");
                buf.extend_from_slice(format!("{:x}", ptr).as_bytes());
                let s = self.intern_str(&buf)?;
                self.push(LuaValue::Str(s));
                buf
            }
        };
        Ok(bytes)
    }

    /// C: `lua_gettop(L)` — number of values in the active call frame
    /// (stack top minus the slot just after the frame's `func`).
    ///
    /// Receiver is `&mut self` to match the `LuaStateStubExt::top` trait
    /// signature exactly; with a different receiver shape (`&self`), Rust's
    /// method-resolution picks the trait default and the program panics on
    /// `todo!("phase-b-reconcile: top")`.
    pub fn top(&mut self) -> i32 {
        get_top(self)
    }

    /// C: `lua_gettop(L)` — same as [`Self::top`], named to match the
    /// `LuaStateStubExt::get_top` trait method. Inherent method shadows the
    /// trait default so the `todo!("phase-b-reconcile: get_top")` shim never
    /// fires.
    pub fn get_top(&mut self) -> i32 {
        get_top(self)
    }

    /// C: `lua_type(L, idx)` — returns the `LuaType` tag of the value at
    /// stack index `idx`, or `LuaType::None` if `idx` falls outside the
    /// active call frame. Inherent method shadows the
    /// `LuaStateStubExt::type_at` trait default so the `todo!()` shim
    /// never fires.
    pub fn type_at(&mut self, idx: i32) -> LuaType {
        lua_type_at(self, idx)
    }

    /// C: `luaL_checkany(L, arg)` from lauxlib.c — raises a `bad argument
    /// #N (value expected)` error if the slot at `arg` is `LUA_TNONE`
    /// (i.e. beyond the active call frame's top). Otherwise a no-op.
    ///
    /// Inherent method on LuaState shadows the `LuaStateStubExt::check_arg_any`
    /// trait default so the `todo!()` shim never fires.
    pub fn check_arg_any(&mut self, arg: i32) -> Result<(), LuaError> {
        if lua_type_at(self, arg) == LuaType::None {
            return Err(LuaError::arg_error(arg, "value expected"));
        }
        Ok(())
    }

    /// C: `luaL_checklstring(L, arg, NULL)` from lauxlib.c — converts the slot
    /// at `arg` to a string via `lua_tolstring` (which coerces numbers to
    /// their string form) and returns the bytes. Raises
    /// `bad argument #N (string expected, got <type>)` if the value is not a
    /// string and not number-coercible.
    ///
    /// Inherent method on LuaState shadows the `LuaStateStubExt::check_arg_string`
    /// trait default so the `todo!()` shim never fires. Uses the free `to_lua_string`
    /// helper here rather than `auxlib::check_lstring`, which routes through
    /// `state.to_lua_string` / `state.type_name` — both still trait stubs.
    pub fn check_arg_string(&mut self, arg: i32) -> Result<Vec<u8>, LuaError> {
        match to_lua_string(self, arg)? {
            Some(s) => Ok(s.as_bytes().to_vec()),
            None => {
                let got = index_to_value(self, arg);
                Err(LuaError::type_arg_error(arg, "string", &got))
            }
        }
    }

    /// C: `luaL_checkinteger(L, arg)` from lauxlib.c — converts the slot at
    /// `arg` to a `lua_Integer` (i64) via `lua_tointegerx` (which accepts
    /// ints, floats with exact integer value, and string-form integers).
    /// Raises `bad argument #N (number has no integer representation)` if
    /// the value is a number but not representable as an integer, or
    /// `bad argument #N (number expected, got <type>)` otherwise.
    ///
    /// Inherent method on LuaState shadows the `LuaStateStubExt::check_arg_integer`
    /// trait default so the `todo!()` shim never fires. Uses the free
    /// `to_integer_x` / `is_number` helpers in this file rather than
    /// `auxlib::check_integer`, which routes through `state.to_integer_x`
    /// and `state.type_name` — both still trait stubs.
    pub fn check_arg_integer(&mut self, arg: i32) -> Result<i64, LuaError> {
        match to_integer_x(self, arg) {
            Some(d) => Ok(d),
            None => {
                if is_number(self, arg) {
                    Err(LuaError::arg_error(
                        arg,
                        "number has no integer representation",
                    ))
                } else {
                    let got = index_to_value(self, arg);
                    Err(LuaError::type_arg_error(arg, "number", &got))
                }
            }
        }
    }

    /// C: `luaL_checknumber(L, arg)` from lauxlib.c — converts the slot at
    /// `arg` to an `f64` via `lua_tonumberx` (which accepts ints, floats,
    /// and number-shaped strings) and raises `bad argument #N (number
    /// expected, got <type>)` if the value is not number-coercible.
    ///
    /// Inherent method on LuaState shadows the `LuaStateStubExt::check_number`
    /// trait default so the `todo!()` shim never fires. Uses the free
    /// `to_number_x` helper here rather than `auxlib::check_number`, which
    /// routes through `state.to_number_x` and `state.type_name` — both still
    /// trait stubs.
    pub fn check_number(&mut self, arg: i32) -> Result<f64, LuaError> {
        match to_number_x(self, arg) {
            Some(d) => Ok(d),
            None => {
                let got = index_to_value(self, arg);
                Err(LuaError::type_arg_error(arg, "number", &got))
            }
        }
    }

    /// C: `luaL_optinteger(L, arg, def)` from lauxlib.c — if the slot at
    /// `arg` is absent (`LUA_TNONE`) or `nil`, return `def`; otherwise
    /// convert it to an integer (with the same string-to-number coercion
    /// `lua_tointegerx` applies) and raise on failure.
    ///
    /// Inherent method on LuaState shadows the `LuaStateStubExt::opt_arg_integer`
    /// trait default so the `todo!()` shim never fires. Implemented with the
    /// free-function helpers in this file rather than `auxlib::opt_integer`
    /// because the latter routes through `state.is_none_or_nil` and
    /// `state.to_integer_x`, which are themselves stubbed.
    pub fn opt_arg_integer(&mut self, arg: i32, def: i64) -> Result<i64, LuaError> {
        match lua_type_at(self, arg) {
            LuaType::None | LuaType::Nil => Ok(def),
            _ => match to_integer_x(self, arg) {
                Some(d) => Ok(d),
                None => {
                    if is_number(self, arg) {
                        Err(LuaError::arg_error(
                            arg,
                            "number has no integer representation",
                        ))
                    } else {
                        let got = index_to_value(self, arg);
                        Err(LuaError::type_arg_error(arg, "number", &got))
                    }
                }
            },
        }
    }

    /// C: `lua_pcall(L, nargs, nresults, msgh)` — a convenience wrapper over
    /// `lua_pcallk` with no continuation. Defers to the existing `pcall_k`
    /// free function, which routes through `protected_call_raw` and
    /// surfaces any runtime / syntax error as `Err(LuaError::Runtime|Syntax)`.
    ///
    /// Inherent method on LuaState shadows the `LuaStateStubExt::protected_call`
    /// trait default so the `todo!()` shim never fires.
    pub fn protected_call(&mut self, nargs: i32, nresults: i32, msgh: i32) -> Result<(), LuaError> {
        pcall_k(self, nargs, nresults, msgh, 0, None).map(|_| ())
    }

    pub fn push_string(&mut self, s: &[u8]) -> Result<(), LuaError> {
        push_lstring(self, s)?;
        Ok(())
    }

    pub fn push_c_closure(
        &mut self,
        f: fn(&mut LuaState) -> Result<usize, LuaError>,
        n: i32,
    ) -> Result<(), LuaError> {
        push_cclosure(self, f, n)
    }

    pub fn raw_seti(&mut self, idx: i32, n: i64) -> Result<(), LuaError> {
        raw_set_i(self, idx, n)
    }

    pub fn table_set_i(&mut self, idx: i32, n: i64) -> Result<(), LuaError> {
        set_i(self, idx, n)
    }

    pub fn create_table(&mut self, narr: i32, nrec: i32) -> Result<(), LuaError> {
        create_table(self, narr, nrec)
    }

    /// Pop the value on top of the stack and store it in the registry under
    /// the string `key`.
    ///
    /// C: `lua_setfield(L, LUA_REGISTRYINDEX, key)`.
    pub fn registry_set(&mut self, key: &[u8]) -> Result<(), LuaError> {
        set_field(self, LUA_REGISTRYINDEX, key)
    }

    /// Create a new metatable in the registry under key `tname`. Leaves the
    /// new metatable on top of the stack and returns `true` when newly
    /// created. If `registry[tname]` already exists, leaves it on top of the
    /// stack and returns `false`.
    ///
    /// C: `LUALIB_API int luaL_newmetatable(lua_State *L, const char *tname)`
    pub fn new_metatable(&mut self, tname: &[u8]) -> Result<bool, LuaError> {
        if get_field(self, LUA_REGISTRYINDEX, tname)? != LuaType::Nil {
            return Ok(false);
        }
        self.pop_n(1);
        create_table(self, 0, 2)?;
        push_lstring(self, tname)?;
        set_field(self, -2, b"__name")?;
        push_value(self, -1);
        set_field(self, LUA_REGISTRYINDEX, tname)?;
        Ok(true)
    }

    /// Create a new library table sized for `funcs` and register each entry as
    /// a closure field on it. Leaves the table on the top of the stack.
    ///
    /// C: `luaL_newlib(L, l)` =
    ///   `luaL_checkversion(L), luaL_newlibtable(L,l), luaL_setfuncs(L,l,0)`.
    /// `luaL_checkversion` is a no-op here (no ABI-version mismatch is
    /// possible inside the Rust port).
    pub fn new_lib(
        &mut self,
        funcs: &[(&[u8], LuaCFunction)],
    ) -> Result<(), LuaError> {
        create_table(self, 0, funcs.len() as i32)?;
        for (name, f) in funcs {
            push_cclosure(self, *f, 0)?;
            set_field(self, -2, name)?;
        }
        Ok(())
    }

    /// Create and populate a library table for `funcs`, leaving it on top of
    /// the stack. The `_name` argument is informational and matches the
    /// `luaL_register`-style call sites in the Phase-A stdlib; the actual
    /// global binding for the library happens later via `luaL_requiref`.
    ///
    /// C: `luaL_newlib(L, l)`.
    pub fn register_lib(
        &mut self,
        _name: &[u8],
        funcs: &[(&[u8], LuaCFunction)],
    ) -> Result<(), LuaError> {
        self.new_lib(funcs)
    }

    /// Create a new empty table presized to hold every entry in `funcs`, and
    /// leave it on top of the stack. No registration is performed — callers
    /// typically follow up with `set_funcs` / `set_funcs_with_upvalues` to
    /// populate the table.
    ///
    /// C: `luaL_newlibtable(L, l)` =
    ///   `lua_createtable(L, 0, sizeof(l)/sizeof((l)[0]) - 1)`. The C macro's
    /// `- 1` discounts the sentinel `{NULL, NULL}` entry; the Rust slice has
    /// no sentinel, so we use `funcs.len()` directly.
    pub fn new_lib_table(
        &mut self,
        funcs: &[(&[u8], LuaCFunction)],
    ) -> Result<(), LuaError> {
        create_table(self, 0, funcs.len() as i32)
    }

    /// Register each entry in `funcs` as a C closure on the table at index
    /// `-(nup + 2)`, sharing the `nup` values currently on top of the stack
    /// as upvalues. The upvalues are popped at the end.
    ///
    /// C: `luaL_setfuncs(L, l, nup)`.
    pub fn set_funcs_with_upvalues(
        &mut self,
        funcs: &[(&[u8], LuaCFunction)],
        nup: i32,
    ) -> Result<(), LuaError> {
        check_stack(self, nup);
        for (name, f) in funcs {
            for _ in 0..nup {
                push_value(self, -nup);
            }
            push_cclosure(self, *f, nup)?;
            set_field(self, -(nup + 2), name)?;
        }
        self.pop_n(nup as usize);
        Ok(())
    }

    pub fn set_metatable(&mut self, objindex: i32) -> Result<(), LuaError> {
        set_metatable(self, objindex)?;
        Ok(())
    }

    /// Fetch the metatable registered under `name` in the registry and assign
    /// it as the metatable of the value currently on top of the stack. The
    /// fetched metatable is popped after assignment, leaving the original top
    /// value in place.
    ///
    /// C: `LUALIB_API void luaL_setmetatable(lua_State *L, const char *tname)`
    pub fn set_metatable_by_name(&mut self, name: &[u8]) -> Result<(), LuaError> {
        get_field(self, LUA_REGISTRYINDEX, name)?;
        set_metatable(self, -2)?;
        Ok(())
    }

    /// Ensure `registry[name]` is a table; push it onto the stack.
    /// Returns `true` if the table already existed, `false` if newly created.
    ///
    /// C: `luaL_getsubtable(L, LUA_REGISTRYINDEX, name)`
    pub fn get_subtable_registry(&mut self, name: &[u8]) -> Result<bool, LuaError> {
        if get_field(self, LUA_REGISTRYINDEX, name)? == LuaType::Table {
            return Ok(true);
        }
        self.pop_n(1);
        let idx = abs_index(self, LUA_REGISTRYINDEX);
        let new_tbl = self.new_table();
        self.push(LuaValue::Table(new_tbl));
        push_value(self, -1);
        set_field(self, idx, name)?;
        Ok(false)
    }

    /// Allocate a fresh full-userdata block of `size` bytes with `nuvalue`
    /// nil-initialised user-value slots, push it on the stack, and return a
    /// `GcRef` to it. The `_name` parameter is advisory — callers typically
    /// follow up with `set_metatable_by_name(name)` to attach the registered
    /// metatable.
    ///
    /// C-correspondent: `lua_newuserdatauv(L, size, nuvalue)` (no name
    /// parameter on the C side; the Rust signature carries it for callers'
    /// convenience).
    pub fn new_userdata_typed(
        &mut self,
        _name: &[u8],
        size: usize,
        nuvalue: i32,
    ) -> Result<GcRef<LuaUserData>, LuaError> {
        debug_assert!(nuvalue >= 0 && nuvalue < u16::MAX as i32, "invalid value");
        let u = GcRef::new(LuaUserData {
            data: vec![0u8; size].into_boxed_slice(),
            uv: vec![LuaValue::Nil; nuvalue as usize],
            metatable: std::cell::RefCell::new(None),
        });
        self.push(LuaValue::UserData(u.clone()));
        self.gc().check_step();
        Ok(u)
    }
}

// ── access functions (stack → Rust) ──────────────────────────────────────────

// C: LUA_API int lua_type (lua_State *L, int idx)
pub fn lua_type_at(state: &LuaState, idx: i32) -> LuaType {
    // C: return (isvalid(L, o) ? ttype(o) : LUA_TNONE);
    if !is_valid_index(state, idx) {
        return LuaType::None;
    }
    index_to_value(state, idx).base_type()
}

// C: LUA_API const char *lua_typename (lua_State *L, int t)
pub fn type_name(_state: &LuaState, t: LuaType) -> &'static [u8] {
    // C: UNUSED(L); return ttypename(t);
    t.type_name()
}

// C: LUA_API int lua_iscfunction (lua_State *L, int idx)
pub fn is_cfunction(state: &LuaState, idx: i32) -> bool {
    // C: return (ttislcf(o) || (ttisCclosure(o)));
    let o = index_to_value(state, idx);
    matches!(o, LuaValue::Function(LuaClosure::LightC(_)) | LuaValue::Function(LuaClosure::C(_)))
}

// C: LUA_API int lua_isinteger (lua_State *L, int idx)
pub fn is_integer(state: &LuaState, idx: i32) -> bool {
    // C: return ttisinteger(o);
    let o = index_to_value(state, idx);
    matches!(o, LuaValue::Int(_))
}

// C: LUA_API int lua_isnumber (lua_State *L, int idx)
pub fn is_number(state: &LuaState, idx: i32) -> bool {
    // C: return tonumber(o, &n);
    let o = index_to_value(state, idx);
    o.to_number_with_strconv().is_some()
}

// C: LUA_API int lua_isstring (lua_State *L, int idx)
pub fn is_string(state: &LuaState, idx: i32) -> bool {
    // C: return (ttisstring(o) || cvt2str(o));
    let o = index_to_value(state, idx);
    matches!(o, LuaValue::Str(_) | LuaValue::Int(_) | LuaValue::Float(_))
}

// C: LUA_API int lua_isuserdata (lua_State *L, int idx)
pub fn is_userdata(state: &LuaState, idx: i32) -> bool {
    // C: return (ttisfulluserdata(o) || ttislightuserdata(o));
    let o = index_to_value(state, idx);
    matches!(o, LuaValue::UserData(_) | LuaValue::LightUserData(_))
}

// C: LUA_API int lua_rawequal (lua_State *L, int index1, int index2)
pub fn raw_equal(state: &LuaState, index1: i32, index2: i32) -> bool {
    // C: return (isvalid(L, o1) && isvalid(L, o2)) ? luaV_rawequalobj(o1, o2) : 0;
    if !is_valid_index(state, index1) || !is_valid_index(state, index2) {
        return false;
    }
    let o1 = index_to_value(state, index1);
    let o2 = index_to_value(state, index2);
    state.equal_obj(None, &o1, &o2)
}

// C: LUA_API void lua_arith (lua_State *L, int op)
// PORT NOTE: LUA_OPUNM / LUA_OPBNOT are unary; all others are binary.
pub fn arith(state: &mut LuaState, op: i32) -> Result<(), LuaError> {
    // C: if (op != LUA_OPUNM && op != LUA_OPBNOT) api_checknelems(L, 2);
    // C: else { api_checknelems(L, 1); setobjs2s copy; api_incr_top }
    // TODO(port): LUA_OPUNM and LUA_OPBNOT constant values not yet defined in
    // Rust; using raw i32 comparison for now.
    const LUA_OPUNM: i32 = 12;
    const LUA_OPBNOT: i32 = 14;
    if op == LUA_OPUNM || op == LUA_OPBNOT {
        // unary — duplicate top as fake second operand
        let top_val = state.get_at(state.top_idx() - 1);
        state.push(top_val);
    }
    // C: luaO_arith(L, op, s2v(L->top.p - 2), s2v(L->top.p - 1), L->top.p - 2);
    let top = state.top_idx();
    let a = state.get_at(top - 2);
    let b = state.get_at(top - 1);
    let result = state.arith_op(op, &a, &b)?;
    state.set_at(top - 2, result);
    // C: L->top.p--;
    state.pop();
    Ok(())
}

// C: LUA_API int lua_compare (lua_State *L, int index1, int index2, int op)
pub fn compare(state: &mut LuaState, index1: i32, index2: i32, op: i32) -> Result<bool, LuaError> {
    // C: may call tag method (hence lua_lock)
    let valid = is_valid_index(state, index1) && is_valid_index(state, index2);
    let o1 = index_to_value(state, index1);
    let o2 = index_to_value(state, index2);
    if valid {
        // C: LUA_OPEQ=0, LUA_OPLT=1, LUA_OPLE=2
        match op {
            0 => Ok(state.equal_obj_with_tm(&o1, &o2)?),
            1 => state.less_than(&o1, &o2),
            2 => state.less_equal(&o1, &o2),
            _ => {
                debug_assert!(false, "invalid option");
                Ok(false)
            }
        }
    } else {
        Ok(false)
    }
}

// C: LUA_API size_t lua_stringtonumber (lua_State *L, const char *s)
pub fn string_to_number(state: &mut LuaState, s: &[u8]) -> usize {
    // C: size_t sz = luaO_str2num(s, s2v(L->top.p));
    // C: if (sz != 0) api_incr_top(L);
    // TODO(port): luaO_str2num not yet translated; push result if successful.
    match state.str_to_num(s) {
        Some((val, consumed)) => {
            state.push(val);
            consumed
        }
        None => 0,
    }
}

// C: LUA_API lua_Number lua_tonumberx (lua_State *L, int idx, int *pisnum)
pub fn to_number_x(state: &LuaState, idx: i32) -> Option<f64> {
    // C: int isnum = tonumber(o, &n);
    let o = index_to_value(state, idx);
    o.to_number_with_strconv()
}

// C: LUA_API lua_Integer lua_tointegerx (lua_State *L, int idx, int *pisnum)
pub fn to_integer_x(state: &LuaState, idx: i32) -> Option<i64> {
    // C: int isnum = tointeger(o, &res);
    let o = index_to_value(state, idx);
    o.to_integer_with_strconv()
}

// C: LUA_API int lua_toboolean (lua_State *L, int idx)
pub fn to_boolean(state: &LuaState, idx: i32) -> bool {
    // C: return !l_isfalse(o);
    let o = index_to_value(state, idx);
    !matches!(o, LuaValue::Nil | LuaValue::Bool(false))
}

// C: LUA_API const char *lua_tolstring (lua_State *L, int idx, size_t *len)
// PORT NOTE: returns Option<GcRef<LuaString>> instead of raw C pointer+len.
pub fn to_lua_string(
    state: &mut LuaState,
    idx: i32,
) -> Result<Option<GcRef<LuaString>>, LuaError> {
    let o = index_to_value(state, idx);
    if let LuaValue::Str(s) = &o {
        return Ok(Some(s.clone()));
    }
    // C: if (!cvt2str(o)) return NULL;  (only numbers are convertible)
    if !matches!(o, LuaValue::Int(_) | LuaValue::Float(_)) {
        return Ok(None);
    }
    // C: luaO_tostring(L, o);  luaC_checkGC(L);
    // C: o = index2value(L, idx);  /* stack may have moved */
    state.obj_to_string(idx)?;
    // C: return getstr(tsvalue(o));
    state.gc().check_step();
    let updated = index_to_value(state, idx);
    if let LuaValue::Str(s) = updated {
        Ok(Some(s))
    } else {
        Ok(None)
    }
}

// C: LUA_API lua_Unsigned lua_rawlen (lua_State *L, int idx)
pub fn raw_len(state: &LuaState, idx: i32) -> u64 {
    let o = index_to_value(state, idx);
    // C: switch (ttypetag(o)) { case LUA_VSHRSTR: ... LUA_VLNGSTR: ... LUA_VUSERDATA: ... LUA_VTABLE: ... }
    match &o {
        LuaValue::Str(s) => s.len() as u64,
        LuaValue::UserData(u) => u.len() as u64,
        LuaValue::Table(t) => state.table_getn(t) as u64,
        _ => 0,
    }
}

// C: LUA_API lua_CFunction lua_tocfunction (lua_State *L, int idx)
pub fn to_cfunction(
    state: &LuaState,
    idx: i32,
) -> Option<fn(&mut LuaState) -> Result<usize, LuaError>> {
    let o = index_to_value(state, idx);
    match o {
        // C: if (ttislcf(o)) return fvalue(o);
        // TODO(phase-b): lua-types `LuaClosure::LightC` carries a placeholder
        // `fn() -> i32` until it can reference `LuaState`. The real cast
        // happens once lua-types absorbs the LuaState-aware signature.
        LuaValue::Function(LuaClosure::LightC(_f)) => None,
        // C: else if (ttisCclosure(o)) return clCvalue(o)->f;
        LuaValue::Function(LuaClosure::C(_ccl)) => None,
        _ => None,
    }
}

// C: l_sinline void *touserdata (const TValue *o)
#[inline]
fn to_userdata_ptr(o: &LuaValue) -> Option<*mut core::ffi::c_void> {
    match o {
        // C: case LUA_TUSERDATA: return getudatamem(uvalue(o));
        LuaValue::UserData(u) => {
            // TODO(port): getudatamem returns a pointer to the raw byte payload of Udata.
            // In Rust, LuaUserData carries a Box<[u8]>; we'd need to return a raw ptr.
            // This is only safe inside lua-gc; stubbing with None for Phase A.
            let _ = u;
            None
        }
        // C: case LUA_TLIGHTUSERDATA: return pvalue(o);
        LuaValue::LightUserData(p) => Some(*p),
        _ => None,
    }
}

// C: LUA_API void *lua_touserdata (lua_State *L, int idx)
pub fn to_userdata(state: &LuaState, idx: i32) -> Option<*mut core::ffi::c_void> {
    let o = index_to_value(state, idx);
    to_userdata_ptr(&o)
}

// C: LUA_API lua_State *lua_tothread (lua_State *L, int idx)
pub fn to_thread(state: &LuaState, idx: i32) -> Option<GcRef<lua_types::value::LuaThread>> {
    // C: return (!ttisthread(o)) ? NULL : thvalue(o);
    // TODO(phase-b): lua-vm's rich LuaState is not the same type as
    // lua_types::value::LuaThread; the latter is a placeholder. Resolve in
    // Phase B by unifying thread types.
    let o = index_to_value(state, idx);
    if let LuaValue::Thread(t) = o {
        Some(t)
    } else {
        None
    }
}

// C: LUA_API const void *lua_topointer (lua_State *L, int idx)
// PORT NOTE: returns a usize (opaque identity) rather than a raw void*.
// Raw pointers are only allowed in lua-gc / lua-coro.
pub fn to_pointer(state: &LuaState, idx: i32) -> Option<usize> {
    let o = index_to_value(state, idx);
    // C: case LUA_VLCF: return cast_voidp(cast_sizet(fvalue(o)));
    // C: case LUA_VUSERDATA: case LUA_VLIGHTUSERDATA: return touserdata(o);
    // C: default: if (iscollectable(o)) return gcvalue(o); else return NULL;
    // TODO(port): returning a raw pointer here is not safe outside lua-gc.
    // Returning the GC identity as a usize for opaque pointer identity purposes.
    match &o {
        LuaValue::Function(LuaClosure::LightC(f)) => Some(*f as usize),
        LuaValue::LightUserData(p) => Some(*p as usize),
        LuaValue::Str(s) => Some(GcRef::identity(s)),
        LuaValue::Table(t) => Some(GcRef::identity(t)),
        LuaValue::Function(LuaClosure::Lua(f)) => Some(GcRef::identity(f)),
        LuaValue::Function(LuaClosure::C(f)) => Some(GcRef::identity(f)),
        LuaValue::UserData(u) => Some(GcRef::identity(u)),
        LuaValue::Thread(t) => Some(GcRef::identity(t)),
        _ => None,
    }
}

// ── push functions (Rust → stack) ────────────────────────────────────────────

// C: LUA_API void lua_pushnil (lua_State *L)
pub fn push_nil(state: &mut LuaState) {
    // C: setnilvalue(s2v(L->top.p)); api_incr_top(L);
    state.push(LuaValue::Nil);
}

// C: LUA_API void lua_pushnumber (lua_State *L, lua_Number n)
pub fn push_number(state: &mut LuaState, n: f64) {
    // C: setfltvalue(s2v(L->top.p), n); api_incr_top(L);
    state.push(LuaValue::Float(n));
}

// C: LUA_API void lua_pushinteger (lua_State *L, lua_Integer n)
pub fn push_integer(state: &mut LuaState, n: i64) {
    // C: setivalue(s2v(L->top.p), n); api_incr_top(L);
    state.push(LuaValue::Int(n));
}

// C: LUA_API const char *lua_pushlstring (lua_State *L, const char *s, size_t len)
// PORT NOTE: returns the interned LuaString instead of a raw C pointer.
pub fn push_lstring(state: &mut LuaState, s: &[u8]) -> Result<GcRef<LuaString>, LuaError> {
    // C: ts = (len == 0) ? luaS_new(L, "") : luaS_newlstr(L, s, len);
    let ts = state.intern_str(s)?;
    // C: setsvalue2s(L, L->top.p, ts); api_incr_top(L);
    state.push(LuaValue::Str(ts.clone()));
    // C: luaC_checkGC(L);
    state.gc().check_step();
    Ok(ts)
}

// C: LUA_API const char *lua_pushstring (lua_State *L, const char *s)
pub fn push_string(state: &mut LuaState, s: Option<&[u8]>) -> Result<Option<GcRef<LuaString>>, LuaError> {
    // C: if (s == NULL) setnilvalue(s2v(L->top.p));
    match s {
        None => {
            state.push(LuaValue::Nil);
            state.gc().check_step();
            Ok(None)
        }
        Some(bytes) => {
            let ts = state.intern_str(bytes)?;
            state.push(LuaValue::Str(ts.clone()));
            state.gc().check_step();
            Ok(Some(ts))
        }
    }
}

// C: LUA_API const char *lua_pushvfstring (lua_State *L, const char *fmt, va_list argp)
// PORT NOTE: va_list is not representable in safe Rust; callers pass a pre-formatted &[u8].
// TODO(port): lua_pushvfstring uses C varargs (va_list); no direct Rust equivalent.
// The Rust API uses state.push_fstring(format_args!(...)) instead.
pub fn push_vfstring(state: &mut LuaState, formatted: &[u8]) -> Result<GcRef<LuaString>, LuaError> {
    let ts = state.intern_str(formatted)?;
    state.push(LuaValue::Str(ts.clone()));
    state.gc().check_step();
    Ok(ts)
}

// C: LUA_API const char *lua_pushfstring (lua_State *L, const char *fmt, ...)
// PORT NOTE: C varargs not used; callers use format_args! and push_fstring.
pub fn push_fstring(state: &mut LuaState, formatted: &[u8]) -> Result<GcRef<LuaString>, LuaError> {
    push_vfstring(state, formatted)
}

// C: LUA_API void lua_pushcclosure (lua_State *L, lua_CFunction fn, int n)
pub fn push_cclosure(
    state: &mut LuaState,
    f: fn(&mut LuaState) -> Result<usize, LuaError>,
    n: i32,
) -> Result<(), LuaError> {
    // C: lua_lock(L);
    //    if (n == 0) { setfvalue(s2v(L->top.p), fn); api_incr_top(L); }
    //    else { api_checknelems(L, n); api_check(L, n <= MAXUPVAL, ...);
    //           cl = luaF_newCclosure(L, n); cl->f = fn;
    //           L->top.p -= n;
    //           while (n--) setobj2n(L, &cl->upvalue[n], s2v(L->top.p + n));
    //           setclCvalue(L, s2v(L->top.p), cl); api_incr_top(L);
    //           luaC_checkGC(L); }
    //    lua_unlock(L);
    //
    // PORT NOTE: `LuaClosure::LightC` and `LuaCClosure` carry a `LuaCFnPtr`
    // (a `usize` index into `GlobalState.c_functions`) rather than the raw
    // function pointer, because lua-types cannot reference `LuaState`. We
    // register `f` in the per-state registry and store the resulting index.
    let idx: lua_types::closure::LuaCFnPtr = {
        let mut g = state.global_mut();
        if n == 0 {
            match g.c_functions.iter().position(|&existing| existing == f) {
                Some(i) => i,
                None => {
                    let i = g.c_functions.len();
                    g.c_functions.push(f);
                    i
                }
            }
        } else {
            let i = g.c_functions.len();
            g.c_functions.push(f);
            i
        }
    };
    if n == 0 {
        state.push(LuaValue::Function(LuaClosure::LightC(idx)));
    } else {
        debug_assert!(n > 0 && (n as u32) <= MAX_UPVAL as u32, "upvalue index too large");
        let n_usize = n as usize;
        let top = state.top_idx();
        debug_assert!((top.0 as usize) >= n_usize, "not enough elements on stack");
        let base = top.0 as usize - n_usize;
        let mut upvalues: Vec<LuaValue> = Vec::with_capacity(n_usize);
        for i in 0..n_usize {
            upvalues.push(state.get_at(crate::state::StackIdx((base + i) as u32)));
        }
        state.pop_n(n_usize);
        let cl = LuaClosure::C(GcRef::new(lua_types::closure::LuaCClosure {
            func: idx,
            upvalues,
        }));
        state.push(LuaValue::Function(cl));
        state.gc().check_step();
    }
    Ok(())
}

// C: LUA_API void lua_pushboolean (lua_State *L, int b)
pub fn push_boolean(state: &mut LuaState, b: bool) {
    // C: if (b) setbtvalue(...); else setbfvalue(...);
    state.push(LuaValue::Bool(b));
}

// C: LUA_API void lua_pushlightuserdata (lua_State *L, void *p)
pub fn push_light_userdata(state: &mut LuaState, p: *mut core::ffi::c_void) {
    // C: setpvalue(s2v(L->top.p), p); api_incr_top(L);
    state.push(LuaValue::LightUserData(p));
}

// C: LUA_API int lua_pushthread (lua_State *L)
// Returns true if pushed thread is the main thread.
pub fn push_thread(state: &mut LuaState) -> bool {
    // C: setthvalue(L, s2v(L->top.p), L); api_incr_top(L);
    // C: return (G(L)->mainthread == L);
    // TODO(port): pushing the current state as a Thread value requires a
    // GcRef<LuaState> pointing to self, which requires the state to be
    // heap-allocated behind a GcRef. Stubbed for Phase A.
    let is_main = state.is_main_thread();
    let _ = is_main;
    // TODO(port): state.push(LuaValue::Thread(state.self_gcref())) — needs self_gcref()
    is_main
}

// ── get functions (Lua → stack) ───────────────────────────────────────────────

// C: l_sinline int auxgetstr (lua_State *L, const TValue *t, const char *k)
fn aux_get_str(state: &mut LuaState, t: LuaValue, k: &[u8]) -> Result<LuaType, LuaError> {
    // C: TString *str = luaS_new(L, k);
    let str_val = {
        let ts = state.intern_str(k)?;
        LuaValue::Str(ts)
    };
    // C: if (luaV_fastget(L, t, str, slot, luaH_getstr)) { setobj2s push slot }
    // C: else { push str; luaV_finishget(...) }
    // TODO(port): luaV_fastget / luaV_finishget not yet translated; using
    // a simplified table_get that may miss metamethod chains.
    let result = state.table_get_with_tm(&t, &str_val)?;
    state.push(result);
    let top = state.top_idx();
    Ok(state.get_at(top - 1).base_type())
}

// C: #define getGtable(L)  (&hvalue(&G(L)->l_registry)->array[LUA_RIDX_GLOBALS - 1])
fn get_global_table(state: &LuaState) -> LuaValue {
    // PORT NOTE (phase-b-reconcile): The lua-types LuaTable placeholder has
    // no storage, so we cannot fetch the globals table from the registry's
    // array slot. init_registry now stashes globals in a direct
    // GlobalState field; read it from there until the LuaTable placeholder
    // reconciles with lua-vm::table::LuaTable.
    state.global().globals.clone()
}

// C: LUA_API int lua_getglobal (lua_State *L, const char *name)
pub fn get_global(state: &mut LuaState, name: &[u8]) -> Result<LuaType, LuaError> {
    // C: G = getGtable(L); return auxgetstr(L, G, name);
    let g = get_global_table(state);
    aux_get_str(state, g, name)
}

// C: LUA_API int lua_gettable (lua_State *L, int idx)
pub fn get_table(state: &mut LuaState, idx: i32) -> Result<LuaType, LuaError> {
    // C: t = index2value(L, idx); key is at top-1
    let t = index_to_value(state, idx);
    let top = state.top_idx();
    let key = state.get_at(top - 1);
    // C: if (luaV_fastget(L, t, s2v(L->top.p - 1), slot, luaH_get)) ...
    // C: else luaV_finishget(L, t, s2v(L->top.p - 1), L->top.p - 1, slot);
    let result = state.table_get_with_tm(&t, &key)?;
    state.set_at(top - 1, result);
    let val = state.get_at(top - 1);
    Ok(val.base_type())
}

// C: LUA_API int lua_getfield (lua_State *L, int idx, const char *k)
pub fn get_field(state: &mut LuaState, idx: i32, k: &[u8]) -> Result<LuaType, LuaError> {
    // C: return auxgetstr(L, index2value(L, idx), k);
    let t = index_to_value(state, idx);
    aux_get_str(state, t, k)
}

// C: LUA_API int lua_geti (lua_State *L, int idx, lua_Integer n)
pub fn get_i(state: &mut LuaState, idx: i32, n: i64) -> Result<LuaType, LuaError> {
    // C: t = index2value(L, idx);
    // C: if (luaV_fastgeti(L, t, n, slot)) setobj2s push slot
    // C: else { TValue aux; setivalue(&aux, n); luaV_finishget(...) }
    let t = index_to_value(state, idx);
    let key = LuaValue::Int(n);
    let result = state.table_get_with_tm(&t, &key)?;
    state.push(result);
    let top = state.top_idx();
    Ok(state.get_at(top - 1).base_type())
}

// C: l_sinline int finishrawget (lua_State *L, const TValue *val)
fn finish_raw_get(state: &mut LuaState, val: Option<LuaValue>) -> LuaType {
    // C: if (isempty(val)) setnilvalue(s2v(L->top.p)); else setobj2s(...)
    let v = val.unwrap_or(LuaValue::Nil);
    state.push(v);
    let top = state.top_idx();
    state.get_at(top - 1).base_type()
}

// C: static Table *gettable (lua_State *L, int idx)
fn get_table_value(state: &LuaState, idx: i32) -> Option<GcRef<LuaTable>> {
    // C: TValue *t = index2value(L, idx); api_check(L, ttistable(t), "table expected");
    let t = index_to_value(state, idx);
    debug_assert!(matches!(t, LuaValue::Table(_)), "table expected");
    if let LuaValue::Table(tbl) = t {
        Some(tbl)
    } else {
        None
    }
}

// C: LUA_API int lua_rawget (lua_State *L, int idx)
pub fn raw_get(state: &mut LuaState, idx: i32) -> LuaType {
    // C: t = gettable(L, idx); val = luaH_get(t, s2v(L->top.p - 1)); L->top.p--;
    let t = get_table_value(state, idx);
    let top = state.top_idx();
    let key = state.get_at(top - 1);
    let val = t.as_ref().map(|tbl| tbl.get(&key));
    state.set_top_idx(top - 1);
    finish_raw_get(state, val)
}

// C: LUA_API int lua_rawgeti (lua_State *L, int idx, lua_Integer n)
pub fn raw_get_i(state: &mut LuaState, idx: i32, n: i64) -> LuaType {
    // C: t = gettable(L, idx); return finishrawget(L, luaH_getint(t, n));
    let t = get_table_value(state, idx);
    let val = t.as_ref().map(|tbl| tbl.get_int(n));
    finish_raw_get(state, val)
}

// C: LUA_API int lua_rawgetp (lua_State *L, int idx, const void *p)
pub fn raw_get_p(state: &mut LuaState, idx: i32, p: *const core::ffi::c_void) -> LuaType {
    // C: setpvalue(&k, cast_voidp(p)); return finishrawget(L, luaH_get(t, &k));
    let t = get_table_value(state, idx);
    let key = LuaValue::LightUserData(p as *mut core::ffi::c_void);
    let val = t.as_ref().map(|tbl| tbl.get(&key));
    finish_raw_get(state, val)
}

// C: LUA_API void lua_createtable (lua_State *L, int narray, int nrec)
pub fn create_table(state: &mut LuaState, narray: i32, nrec: i32) -> Result<(), LuaError> {
    // C: t = luaH_new(L); sethvalue2s ...; api_incr_top;
    // C: if (narray > 0 || nrec > 0) luaH_resize(L, t, narray, nrec);
    let t = state.new_table();
    if narray > 0 || nrec > 0 {
        t.resize(state, narray as usize, nrec as usize)?;
    }
    state.push(LuaValue::Table(t));
    state.gc().check_step();
    Ok(())
}

// C: LUA_API int lua_getmetatable (lua_State *L, int objindex)
pub fn get_metatable(state: &mut LuaState, objindex: i32) -> bool {
    // C: obj = index2value(L, objindex);
    let obj = index_to_value(state, objindex);
    // C: switch (ttype(obj)) { LUA_TTABLE: ... LUA_TUSERDATA: ... default: G(L)->mt[ttype] }
    let mt: Option<GcRef<LuaTable>> = match &obj {
        LuaValue::Table(t) => t.metatable(),
        LuaValue::UserData(u) => u.metatable(),
        other => {
            let idx = other.base_type() as usize;
            state.global().mt[idx].clone()
        }
    };
    if let Some(mt_table) = mt {
        state.push(LuaValue::Table(mt_table));
        true
    } else {
        false
    }
}

// C: LUA_API int lua_getiuservalue (lua_State *L, int idx, int n)
pub fn get_i_uservalue(state: &mut LuaState, idx: i32, n: i32) -> LuaType {
    // C: o = index2value(L, idx); api_check(L, ttisfulluserdata(o), ...);
    let o = index_to_value(state, idx);
    debug_assert!(matches!(o, LuaValue::UserData(_)), "full userdata expected");
    if let LuaValue::UserData(ref u) = o {
        let uv_count = u.uv.len() as i32;
        if n <= 0 || n > uv_count {
            // C: setnilvalue(s2v(L->top.p)); t = LUA_TNONE;
            state.push(LuaValue::Nil);
            LuaType::None
        } else {
            // C: setobj2s(L, L->top.p, &uvalue(o)->uv[n - 1].uv);
            let val = u.uv[(n - 1) as usize].clone();
            let t = val.base_type();
            state.push(val);
            t
        }
    } else {
        state.push(LuaValue::Nil);
        LuaType::None
    }
}

// ── set functions (stack → Lua) ───────────────────────────────────────────────

// C: static void auxsetstr (lua_State *L, const TValue *t, const char *k)
fn aux_set_str(state: &mut LuaState, t: LuaValue, k: &[u8]) -> Result<(), LuaError> {
    // C: TString *str = luaS_new(L, k); api_checknelems(L, 1);
    let str_val = {
        let ts = state.intern_str(k)?;
        LuaValue::Str(ts)
    };
    // C: if (luaV_fastget(L, t, str, slot, luaH_getstr))
    //       luaV_finishfastset(L, t, slot, s2v(L->top.p - 1)); L->top.p--;
    //    else { setsvalue2s L->top.p str; api_incr_top;
    //           luaV_finishset(L, t, s2v(L->top.p-1), s2v(L->top.p-2), slot);
    //           L->top.p -= 2; }
    let top = state.top_idx();
    let val = state.get_at(top - 1);
    state.table_set_with_tm(&t, str_val, val)?;
    state.pop();
    Ok(())
}

// C: LUA_API void lua_setglobal (lua_State *L, const char *name)
pub fn set_global(state: &mut LuaState, name: &[u8]) -> Result<(), LuaError> {
    // C: G = getGtable(L); auxsetstr(L, G, name);
    let g = get_global_table(state);
    aux_set_str(state, g, name)
}

// C: LUA_API void lua_settable (lua_State *L, int idx)
pub fn set_table(state: &mut LuaState, idx: i32) -> Result<(), LuaError> {
    // C: t = index2value(L, idx); api_checknelems(L, 2);
    // C: key at top-2, value at top-1
    let t = index_to_value(state, idx);
    let top = state.top_idx();
    let key = state.get_at(top - 2);
    let val = state.get_at(top - 1);
    state.table_set_with_tm(&t, key, val)?;
    // C: L->top.p -= 2;
    state.set_top_idx(top - 2);
    Ok(())
}

// C: LUA_API void lua_setfield (lua_State *L, int idx, const char *k)
pub fn set_field(state: &mut LuaState, idx: i32, k: &[u8]) -> Result<(), LuaError> {
    // C: auxsetstr(L, index2value(L, idx), k);
    let t = index_to_value(state, idx);
    aux_set_str(state, t, k)
}

// C: LUA_API void lua_seti (lua_State *L, int idx, lua_Integer n)
pub fn set_i(state: &mut LuaState, idx: i32, n: i64) -> Result<(), LuaError> {
    // C: t = index2value(L, idx); api_checknelems(L, 1);
    let t = index_to_value(state, idx);
    let top = state.top_idx();
    let val = state.get_at(top - 1);
    let key = LuaValue::Int(n);
    state.table_set_with_tm(&t, key, val)?;
    // C: L->top.p--;
    state.pop();
    Ok(())
}

// C: static void aux_rawset (lua_State *L, int idx, TValue *key, int n)
fn aux_raw_set(state: &mut LuaState, idx: i32, key: LuaValue, n: u32) -> Result<(), LuaError> {
    // C: t = gettable(L, idx); luaH_set(L, t, key, s2v(L->top.p - 1));
    let t = get_table_value(state, idx)
        .ok_or_else(|| LuaError::runtime(format_args!("table expected")))?;
    let top = state.top_idx();
    let val = state.get_at(top - 1);
    t.raw_set(state, &key, val)?;
    t.invalidate_tm_cache();
    // C: luaC_barrierback(L, obj2gco(t), s2v(L->top.p - 1));
    let top_val = state.get_at(top - 1);
    state.gc().barrier_back(&t, &top_val);
    // C: L->top.p -= n;
    state.set_top_idx(top - n as i32);
    Ok(())
}

// C: LUA_API void lua_rawset (lua_State *L, int idx)
pub fn raw_set(state: &mut LuaState, idx: i32) -> Result<(), LuaError> {
    // C: aux_rawset(L, idx, s2v(L->top.p - 2), 2);
    let top = state.top_idx();
    let key = state.get_at(top - 2);
    aux_raw_set(state, idx, key, 2)
}

// C: LUA_API void lua_rawsetp (lua_State *L, int idx, const void *p)
pub fn raw_set_p(state: &mut LuaState, idx: i32, p: *const core::ffi::c_void) -> Result<(), LuaError> {
    // C: setpvalue(&k, cast_voidp(p)); aux_rawset(L, idx, &k, 1);
    let key = LuaValue::LightUserData(p as *mut core::ffi::c_void);
    aux_raw_set(state, idx, key, 1)
}

// C: LUA_API void lua_rawseti (lua_State *L, int idx, lua_Integer n)
pub fn raw_set_i(state: &mut LuaState, idx: i32, n: i64) -> Result<(), LuaError> {
    // C: t = gettable(L, idx); luaH_setint(L, t, n, s2v(L->top.p - 1));
    let t = get_table_value(state, idx)
        .ok_or_else(|| LuaError::runtime(format_args!("table expected")))?;
    let top = state.top_idx();
    let val = state.get_at(top - 1);
    t.raw_set_int(state, n, val)?;
    // C: luaC_barrierback(L, obj2gco(t), s2v(L->top.p - 1));
    let top_val = state.get_at(top - 1);
    state.gc().barrier_back(&t, &top_val);
    // C: L->top.p--;
    state.pop();
    Ok(())
}

// C: LUA_API int lua_setmetatable (lua_State *L, int objindex)
pub fn set_metatable(state: &mut LuaState, objindex: i32) -> Result<bool, LuaError> {
    // C: api_checknelems(L, 1);
    let top = state.top_idx();
    let mt_val = state.get_at(top - 1);
    // C: if (ttisnil(s2v(L->top.p - 1))) mt = NULL; else mt = hvalue(...)
    let mt: Option<GcRef<LuaTable>> = if matches!(mt_val, LuaValue::Nil) {
        None
    } else {
        debug_assert!(matches!(mt_val, LuaValue::Table(_)), "table expected");
        if let LuaValue::Table(t) = mt_val {
            Some(t)
        } else {
            None
        }
    };

    let obj = index_to_value(state, objindex);
    // C: switch (ttype(obj)) { LUA_TTABLE: ... LUA_TUSERDATA: ... default: G(L)->mt[ttype] }
    match obj {
        LuaValue::Table(ref tbl) => {
            if mt.is_some() {
                // C: luaC_objbarrier(L, gcvalue(obj), mt);
                // C: luaC_checkfinalizer(L, gcvalue(obj), mt);
                state.gc().obj_barrier(tbl, mt.as_ref().unwrap());
                // TODO(port): luaC_checkfinalizer
            }
            // C: hvalue(obj)->metatable = mt;
            tbl.set_metatable(mt);
        }
        LuaValue::UserData(ref ud) => {
            if let Some(ref mt_table) = mt {
                state.gc().obj_barrier(ud, mt_table);
                // TODO(port): luaC_checkfinalizer
            }
            ud.set_metatable(mt);
        }
        ref other => {
            let idx = other.base_type() as usize;
            state.global_mut().mt[idx] = mt;
        }
    }
    // C: L->top.p--;
    state.pop();
    Ok(true)
}

// C: LUA_API int lua_setiuservalue (lua_State *L, int idx, int n)
pub fn set_i_uservalue(state: &mut LuaState, idx: i32, n: i32) -> Result<bool, LuaError> {
    // C: api_checknelems(L, 1);
    let o = index_to_value(state, idx);
    debug_assert!(matches!(o, LuaValue::UserData(_)), "full userdata expected");
    let top = state.top_idx();
    let val = state.get_at(top - 1);
    let res = if let LuaValue::UserData(ref ud) = o {
        let nuvalue = ud.uv.len() as i32;
        // C: !(cast_uint(n) - 1u < cast_uint(uvalue(o)->nuvalue))
        if n < 1 || n > nuvalue {
            false
        } else {
            // C: setobj(L, &uvalue(o)->uv[n - 1].uv, s2v(L->top.p - 1));
            // TODO(port): LuaUserData uv field needs interior mutability for write
            // ud.uv[(n - 1) as usize] = val.clone();
            // C: luaC_barrierback(L, gcvalue(o), s2v(L->top.p - 1));
            state.gc().barrier_back(ud, &val);
            let _ = (n, ud);
            true
        }
    } else {
        false
    };
    // C: L->top.p--;
    state.pop();
    Ok(res)
}

// ── load/call functions ───────────────────────────────────────────────────────

// C: LUA_API void lua_callk (lua_State *L, int nargs, int nresults,
//                            lua_KContext ctx, lua_KFunction k)
pub fn call_k(
    state: &mut LuaState,
    nargs: i32,
    nresults: i32,
    ctx: isize,
    k: Option<fn(&mut LuaState, i32, isize) -> Result<usize, LuaError>>,
) -> Result<(), LuaError> {
    // C: api_check(L, k == NULL || !isLua(L->ci), "cannot use continuations inside hooks");
    // C: api_checknelems(L, nargs+1);
    // C: api_check(L, L->status == LUA_OK, "cannot do calls on non-normal thread");
    // C: func = L->top.p - (nargs+1);
    let top = state.top_idx();
    let func_idx = top - (nargs + 1);
    // C: if (k != NULL && yieldable(L)) { save continuation; luaD_call }
    // C: else luaD_callnoyield(L, func, nresults);
    // TODO(port): continuation (k) and yieldable check deferred to Phase E.
    state.call_no_yield(func_idx, nresults)?;
    // C: adjustresults(L, nresults);
    state.adjust_results(nresults);
    let _ = (ctx, k);
    Ok(())
}

// C: LUA_API int lua_pcallk (lua_State *L, int nargs, int nresults, int errfunc,
//                            lua_KContext ctx, lua_KFunction k)
pub fn pcall_k(
    state: &mut LuaState,
    nargs: i32,
    nresults: i32,
    errfunc: i32,
    ctx: isize,
    k: Option<fn(&mut LuaState, i32, isize) -> Result<usize, LuaError>>,
) -> Result<LuaStatus, LuaError> {
    // C: api_checknelems(L, nargs+1);
    // C: func (error handler) stack offset
    let err_handler_idx: isize = if errfunc == 0 {
        0
    } else {
        let o = index_to_stack_idx(state, errfunc);
        debug_assert!(
            matches!(state.get_at(o), LuaValue::Function(_)),
            "error handler must be a function"
        );
        o.0 as isize
    };
    let top = state.top_idx();
    let func_idx = top - (nargs + 1);
    // C: if (k == NULL || !yieldable(L)) { conventional protected call }
    // TODO(port): continuation and yieldable deferred to Phase E.
    let _ = (k, ctx);
    state.protected_call_raw(func_idx, nresults, StackIdx(err_handler_idx as u32))?;
    state.adjust_results(nresults);
    Ok(LuaStatus::Ok)
}

// C: LUA_API int lua_load (lua_State *L, lua_Reader reader, void *data,
//                          const char *chunkname, const char *mode)
// PORT NOTE: lua_Reader (void* callback) is replaced by Box<dyn FnMut>; mode
// is &[u8].
pub fn load(
    state: &mut LuaState,
    reader: Box<dyn FnMut() -> Option<Vec<u8>>>,
    chunkname: Option<&[u8]>,
    mode: Option<&[u8]>,
) -> Result<LuaStatus, LuaError> {
    let name = chunkname.unwrap_or(b"?");
    // C: luaZ_init(L, &z, reader, data); status = luaD_protectedparser(L, &z, chunkname, mode);
    let z = crate::zio::ZIO::new(reader);
    let status = state.protected_parser(z, name, mode);
    if status == LuaStatus::Ok {
        // C: LClosure *f = clLvalue(s2v(L->top.p - 1));
        // C: if (f->nupvalues >= 1) { set global table as 1st upvalue }
        let top = state.top_idx();
        let func_val = state.get_at(top - 1);
        if let LuaValue::Function(LuaClosure::Lua(lcl)) = func_val {
            if !lcl.upvals.is_empty() {
                // C: const TValue *gt = getGtable(L); setobj(L, f->upvals[0]->v.p, gt);
                // PORT NOTE: GcRef<UpVal> = Rc<UpVal> has no interior mutability,
                // and the closure's `upvals` Vec is reachable only behind a
                // shared Rc<LuaLClosure>. Rebuild the closure here: clone the
                // proto, swap upvals[0] for a fresh `Closed(globals)` upvalue,
                // wrap in a new GcRef, and replace the stack slot.
                let gt = get_global_table(state);
                let mut new_upvals = lcl.upvals.clone();
                new_upvals[0] = GcRef::new(UpVal::closed(gt));
                let new_lcl = GcRef::new(lua_types::LuaLClosure {
                    proto: lcl.proto.clone(),
                    upvals: new_upvals,
                });
                state.set_at(top - 1, LuaValue::Function(LuaClosure::Lua(new_lcl)));
            }
        }
    }
    Ok(status)
}

// C: LUA_API int lua_dump (lua_State *L, lua_Writer writer, void *data, int strip)
pub fn dump(
    state: &LuaState,
    writer: &mut dyn FnMut(&[u8]) -> Result<(), LuaError>,
    strip: bool,
) -> Result<bool, LuaError> {
    // C: api_checknelems(L, 1); o = s2v(L->top.p - 1);
    let top = state.top_idx();
    let o = state.get_at(top - 1);
    // C: if (isLfunction(o)) status = luaU_dump(L, getproto(o), writer, data, strip);
    if let LuaValue::Function(LuaClosure::Lua(ref lcl)) = o {
        state.dump_proto(&lcl.proto, writer, strip)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

// C: LUA_API int lua_status (lua_State *L)
pub fn status(state: &LuaState) -> LuaStatus {
    LuaStatus::from_raw(state.status as i32)
}

// ── garbage collection ────────────────────────────────────────────────────────

/// GC operation codes (C: LUA_GC* constants)
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcWhat {
    Stop = 0,
    Restart = 1,
    Collect = 2,
    Count = 3,
    CountB = 4,
    Step = 5,
    SetPause = 6,
    SetStepMul = 7,
    IsRunning = 9,
    Gen = 10,
    Inc = 11,
}

// C: LUA_API int lua_gc (lua_State *L, int what, ...)
// PORT NOTE: C varargs replaced by explicit GcArgs enum; callers supply parameters directly.
pub enum GcArgs {
    Stop,
    Restart,
    Collect,
    Count,
    CountB,
    Step { data: i32 },
    SetPause { value: i32 },
    SetStepMul { value: i32 },
    IsRunning,
    Gen { minormul: i32, majormul: i32 },
    Inc { pause: i32, stepmul: i32, stepsize: i32 },
}

pub fn gc(state: &mut LuaState, args: GcArgs) -> i32 {
    // C: if (g->gcstp & GCSTPGC) return -1;
    if state.global().is_gc_stopped_internally() {
        return -1;
    }
    match args {
        // C: case LUA_GCSTOP: g->gcstp = GCSTPUSR;
        GcArgs::Stop => {
            state.global_mut().set_gc_stop_user();
        }
        // C: case LUA_GCRESTART: luaE_setdebt(g, 0); g->gcstp = 0;
        GcArgs::Restart => {
            state.global_mut().set_gc_debt(0);
            state.global_mut().clear_gc_stop();
        }
        // C: case LUA_GCCOLLECT: luaC_fullgc(L, 0);
        GcArgs::Collect => {
            state.gc().full_collect();
        }
        // C: case LUA_GCCOUNT: res = cast_int(gettotalbytes(g) >> 10);
        GcArgs::Count => {
            return (state.global().total_bytes() >> 10) as i32;
        }
        // C: case LUA_GCCOUNTB: res = cast_int(gettotalbytes(g) & 0x3ff);
        GcArgs::CountB => {
            return (state.global().total_bytes() & 0x3ff) as i32;
        }
        // C: case LUA_GCSTEP: ...
        GcArgs::Step { data } => {
            // C: l_mem debt = 1; lu_byte oldstp = g->gcstp; g->gcstp = 0;
            let old_stp = {
                let mut g = state.global_mut();
                let old = g.gc_stop_flags();
                g.clear_gc_stop();
                old
            };
            if data == 0 {
                // C: luaE_setdebt(g, 0); luaC_step(L);
                state.global_mut().set_gc_debt(0);
                state.gc().step();
            } else {
                // C: debt = cast(l_mem, data) * 1024 + g->GCdebt; luaE_setdebt; luaC_checkGC
                let debt = data as isize * 1024 + state.global().gc_debt();
                state.global_mut().set_gc_debt(debt);
                state.gc().check_step();
            }
            state.global_mut().set_gc_stop_flags(old_stp);
            // C: if (debt > 0 && g->gcstate == GCSpause) res = 1;
            return if state.global().gc_at_pause() { 1 } else { 0 };
        }
        // C: case LUA_GCSETPAUSE:
        GcArgs::SetPause { value } => {
            // C: res = getgcparam(g->gcpause); setgcparam(g->gcpause, data);
            let old = state.global().gc_pause_param() as i32;
            state.global_mut().set_gc_pause_param(value as u8);
            return old;
        }
        // C: case LUA_GCSETSTEPMUL:
        GcArgs::SetStepMul { value } => {
            let old = state.global().gc_stepmul_param() as i32;
            state.global_mut().set_gc_stepmul_param(value as u8);
            return old;
        }
        // C: case LUA_GCISRUNNING: res = gcrunning(g);
        GcArgs::IsRunning => {
            return state.global().gc_running() as i32;
        }
        // C: case LUA_GCGEN:
        GcArgs::Gen { minormul, majormul } => {
            // C: res = isdecGCmodegen(g) ? LUA_GCGEN : LUA_GCINC;
            let old_mode = if state.global().is_gen_mode() { 10i32 } else { 11i32 };
            if minormul != 0 {
                state.global_mut().genminormul = minormul as u8;
            }
            if majormul != 0 {
                state.global_mut().set_gc_genmajormul(majormul as u8);
            }
            // C: luaC_changemode(L, KGC_GEN);
            state.gc().change_mode(crate::state::GcKind::Generational);
            return old_mode;
        }
        // C: case LUA_GCINC:
        GcArgs::Inc { pause, stepmul, stepsize } => {
            let old_mode = if state.global().is_gen_mode() { 10i32 } else { 11i32 };
            if pause != 0 {
                state.global_mut().set_gc_pause_param(pause as u8);
            }
            if stepmul != 0 {
                state.global_mut().set_gc_stepmul_param(stepmul as u8);
            }
            if stepsize != 0 {
                state.global_mut().gcstepsize = stepsize as u8;
            }
            // C: luaC_changemode(L, KGC_INC);
            state.gc().change_mode(crate::state::GcKind::Incremental);
            return old_mode;
        }
    }
    0
}

// ── miscellaneous functions ───────────────────────────────────────────────────

// C: LUA_API int lua_error (lua_State *L)
// PORT NOTE: returns Result<Infallible, _> — semantically "always Err". The
// translator originally wrote `Result<!, _>` but the `!` type in a return
// position is still nightly-only as of Rust 1.93; Infallible is the stable
// stand-in. Callsites just pattern-match on Err.
pub fn lua_error(state: &mut LuaState) -> Result<Infallible, LuaError> {
    // C: errobj = s2v(L->top.p - 1);
    // C: api_checknelems(L, 1);
    // C: if (ttisshrstring(errobj) && eqshrstr(tsvalue(errobj), G(L)->memerrmsg))
    //      luaM_error(L);  /* memory error */
    //    else
    //      luaG_errormsg(L);  /* regular error */
    let top = state.top_idx();
    let errobj = state.get_at(top - 1);
    // C: special-case OOM string
    let is_mem_err = if let LuaValue::Str(ref s) = errobj {
        let memerr = state.global().memerrmsg.clone();
        // C: eqshrstr(tsvalue(errobj), G(L)->memerrmsg) — short-string pointer equality
        GcRef::ptr_eq(s, &memerr)
    } else {
        false
    };
    if is_mem_err {
        Err(LuaError::Memory)
    } else {
        Err(LuaError::from_value(errobj))
    }
}

// C: LUA_API int lua_next (lua_State *L, int idx)
pub fn next(state: &mut LuaState, idx: i32) -> Result<bool, LuaError> {
    // C: t = gettable(L, idx); api_checknelems(L, 1);
    let t = get_table_value(state, idx)
        .ok_or_else(|| LuaError::runtime(format_args!("table expected")))?;
    let top = state.top_idx();
    let key = state.get_at(top - 1);
    // C: more = luaH_next(L, t, L->top.p - 1);
    match t.next(key)? {
        Some((next_key, next_val)) => {
            // C: if (more) api_incr_top(L); (key already at top-1, push value above)
            state.set_at(top - 1, next_key);
            state.push(next_val);
            Ok(true)
        }
        None => {
            // C: else L->top.p -= 1;  (remove key)
            state.set_top_idx(top - 1);
            Ok(false)
        }
    }
}

// C: LUA_API void lua_toclose (lua_State *L, int idx)
pub fn to_close(state: &mut LuaState, idx: i32) -> Result<(), LuaError> {
    // C: o = index2stack(L, idx); nresults = L->ci->nresults;
    // C: api_check(L, L->tbclist.p < o, "given index below or equal a marked one");
    // C: luaF_newtbcupval(L, o);
    // C: if (!hastocloseCfunc(nresults)) L->ci->nresults = codeNresults(nresults);
    let _level = index_to_stack_idx(state, idx);
    // TODO(port): luaF_newtbcupval and to-be-closed variable infrastructure
    // not yet translated. Stubbing for Phase A.
    Ok(())
}

// C: LUA_API void lua_concat (lua_State *L, int n)
pub fn concat(state: &mut LuaState, n: i32) -> Result<(), LuaError> {
    // C: api_checknelems(L, n);
    if n > 0 {
        // C: luaV_concat(L, n);
        state.concat(n)?;
    } else {
        // C: setsvalue2s(L, L->top.p, luaS_newlstr(L, "", 0)); api_incr_top(L);
        let empty = state.intern_str(b"")?;
        state.push(LuaValue::Str(empty));
    }
    state.gc().check_step();
    Ok(())
}

// C: LUA_API void lua_len (lua_State *L, int idx)
pub fn len(state: &mut LuaState, idx: i32) -> Result<(), LuaError> {
    // C: t = index2value(L, idx); luaV_objlen(L, L->top.p, t);
    let t = index_to_value(state, idx);
    let result = state.obj_len(&t)?;
    state.push(result);
    Ok(())
}

// C: LUA_API lua_Alloc lua_getallocf / lua_setallocf
// PORT NOTE: The custom allocator hook is not exposed in the Rust-native API.
// Rust's allocator handles all allocation.
// These are intentionally omitted.

// C: void lua_setwarnf (lua_State *L, lua_WarnFunction f, void *ud)
pub fn set_warn_f(
    state: &mut LuaState,
    f: Option<Box<dyn FnMut(&[u8], bool)>>,
) {
    // C: G(L)->ud_warn = ud; G(L)->warnf = f;
    // PORT NOTE: ud_warn userdata is folded into the closure per types.tsv.
    state.global_mut().warnf = f;
}

// C: void lua_warning (lua_State *L, const char *msg, int tocont)
pub fn warning(state: &mut LuaState, msg: &[u8], tocont: bool) {
    // C: luaE_warning(L, msg, tocont);
    state.emit_warning(msg, tocont);
}

// C: LUA_API void *lua_newuserdatauv (lua_State *L, size_t size, int nuvalue)
pub fn new_userdata_uv(
    state: &mut LuaState,
    size: usize,
    nuvalue: i32,
) -> Result<GcRef<LuaUserData>, LuaError> {
    // C: api_check(L, 0 <= nuvalue && nuvalue < USHRT_MAX, "invalid value");
    debug_assert!(nuvalue >= 0 && nuvalue < u16::MAX as i32, "invalid value");
    // C: u = luaS_newudata(L, size, nuvalue);
    let u = state.new_userdata(size, nuvalue as usize)?;
    state.push(LuaValue::UserData(u.clone()));
    state.gc().check_step();
    Ok(u)
}

// ── upvalue access ────────────────────────────────────────────────────────────

// C: static const char *aux_upvalue (TValue *fi, int n, TValue **val, GCObject **owner)
// PORT NOTE: Returns (name, value) instead of mutating output pointers. The name
// is returned as an owned Vec<u8> because Lua upvalue names live in the proto's
// LuaString table (GC heap), not in static storage.
fn aux_upvalue(
    fi: &LuaValue,
    n: i32,
) -> Option<(Vec<u8>, LuaValue)> {
    match fi {
        // C: case LUA_VCCL:
        LuaValue::Function(LuaClosure::C(ccl)) => {
            let nupvalues = ccl.upvalues.len() as i32;
            // C: if (!(cast_uint(n) - 1u < cast_uint(f->nupvalues))) return NULL;
            if n < 1 || n > nupvalues {
                return None;
            }
            // C: *val = &f->upvalue[n-1]; return "";
            Some((Vec::new(), ccl.upvalues[(n - 1) as usize].clone()))
        }
        // C: case LUA_VLCL:
        LuaValue::Function(LuaClosure::Lua(lcl)) => {
            let nupvalues = lcl.upvals.len() as i32;
            // C: if (!(cast_uint(n) - 1u < cast_uint(p->sizeupvalues))) return NULL;
            if n < 1 || n > nupvalues {
                return None;
            }
            // C: *val = f->upvals[n-1]->v.p;
            let upval = &lcl.upvals[(n - 1) as usize];
            let val = match &*upval.slot() {
                lua_types::UpValState::Closed(v) => v.clone(),
                lua_types::UpValState::Open { .. } => {
                    LuaValue::Nil
                }
            };
            // C: name = p->upvalues[n-1].name;
            // The proto records the static name of each upvalue (e.g. "_ENV"
            // for the main chunk's environment upvalue). Fall back to an
            // empty byte-string when the slot exists but has no recorded name
            // (matches C-Lua's behaviour for anonymous upvalues).
            let name: Vec<u8> = lcl
                .proto
                .upvalues
                .get((n - 1) as usize)
                .and_then(|ud| ud.name.as_ref())
                .map(|s| s.as_bytes().to_vec())
                .unwrap_or_default();
            Some((name, val))
        }
        _ => None,
    }
}

// C: LUA_API const char *lua_getupvalue (lua_State *L, int funcindex, int n)
pub fn get_upvalue(state: &mut LuaState, funcindex: i32, n: i32) -> Option<Vec<u8>> {
    // C: name = aux_upvalue(index2value(L, funcindex), n, &val, NULL);
    let fi = index_to_value(state, funcindex);
    if let Some((name, val)) = aux_upvalue(&fi, n) {
        // C: setobj2s(L, L->top.p, val); api_incr_top(L);
        state.push(val);
        Some(name)
    } else {
        None
    }
}

// C: LUA_API const char *lua_setupvalue (lua_State *L, int funcindex, int n)
pub fn setup_value(state: &mut LuaState, funcindex: i32, n: i32) -> Option<Vec<u8>> {
    // C: fi = index2value(L, funcindex); api_checknelems(L, 1);
    let fi = index_to_value(state, funcindex);
    // C: name = aux_upvalue(fi, n, &val, &owner);
    if let Some((name, _)) = aux_upvalue(&fi, n) {
        // C: L->top.p--; setobj(L, val, s2v(L->top.p)); luaC_barrier(L, owner, val);
        let new_val = state.pop();
        // TODO(port): writing back into the upvalue (C closure or open/closed Lua)
        // requires interior mutability. Stubbed for Phase A.
        let _ = (new_val, fi, n);
        Some(name)
    } else {
        None
    }
}

// C: static UpVal **getupvalref (lua_State *L, int fidx, int n, LClosure **pf)
// PORT NOTE: returns an index into the upvals vec rather than a pointer-to-pointer.
// Returns None if n is out of range.
fn get_upval_ref_idx(state: &LuaState, fidx: i32, n: i32) -> Option<usize> {
    let fi = index_to_value(state, fidx);
    debug_assert!(matches!(fi, LuaValue::Function(LuaClosure::Lua(_))), "Lua function expected");
    if let LuaValue::Function(LuaClosure::Lua(ref lcl)) = fi {
        let sizeupvalues = lcl.upvals.len() as i32;
        if n >= 1 && n <= sizeupvalues {
            Some((n - 1) as usize)
        } else {
            None
        }
    } else {
        None
    }
}

// C: LUA_API void *lua_upvalueid (lua_State *L, int fidx, int n)
// PORT NOTE: Returns Option<usize> identity instead of raw void*.
pub fn upvalue_id(state: &LuaState, fidx: i32, n: i32) -> Option<usize> {
    let fi = index_to_value(state, fidx);
    match &fi {
        // C: case LUA_VLCL: return *getupvalref(L, fidx, n, NULL);
        LuaValue::Function(LuaClosure::Lua(lcl)) => {
            let idx = get_upval_ref_idx(state, fidx, n)?;
            // Return the identity of the UpVal GcRef
            Some(GcRef::identity(&lcl.upvals[idx]))
        }
        // C: case LUA_VCCL: if (1 <= n && n <= f->nupvalues) return &f->upvalue[n-1];
        LuaValue::Function(LuaClosure::C(ccl)) => {
            if n >= 1 && n <= ccl.upvalues.len() as i32 {
                // TODO(port): returning address of upvalue slot not possible without raw ptr.
                // Return a synthetic identity based on the closure's identity + n.
                Some(GcRef::identity(ccl) ^ (n as usize))
            } else {
                None
            }
        }
        // C: case LUA_VLCF: return NULL;
        LuaValue::Function(LuaClosure::LightC(_)) => None,
        _ => {
            debug_assert!(false, "function expected");
            None
        }
    }
}

// C: LUA_API void lua_upvaluejoin (lua_State *L, int fidx1, int n1,
//                                               int fidx2, int n2)
pub fn upvalue_join(state: &mut LuaState, fidx1: i32, n1: i32, fidx2: i32, n2: i32) {
    // C: LClosure *f1; UpVal **up1 = getupvalref(L, fidx1, n1, &f1);
    // C: UpVal **up2 = getupvalref(L, fidx2, n2, NULL);
    // C: api_check(L, *up1 != NULL && *up2 != NULL, "invalid upvalue index");
    // C: *up1 = *up2; luaC_objbarrier(L, f1, *up1);
    let _idx1 = get_upval_ref_idx(state, fidx1, n1);
    let _idx2 = get_upval_ref_idx(state, fidx2, n2);
    // TODO(port): sharing an UpVal between two closures requires GcRef<UpVal>
    // cloning. The Lua closure's upvals Vec would need to replace upvals[idx1-1]
    // with the GcRef from closure2's upvals[idx2-1]. This requires interior
    // mutability on LuaClosure::Lua. Stubbed for Phase A.
}

// ──────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        src/lapi.c  (1464 lines, ~47 functions)
//   target_crate:  lua-vm
//   confidence:    low
//   todos:         18
//   port_notes:    8
//   unsafe_blocks: 0   (must be 0 outside lua-gc/lua-coro)
//   notes:         Heavy use of interior mutability TODOs (GcRef writes for
//                  metatables, upvalue writes, userdata uv writes). The
//                  index2value helper returns cloned LuaValue not a pointer,
//                  so write-back paths that C achieves with TValue* are
//                  stubbed. Stack pointer arithmetic faithfully translated to
//                  StackIdx (u32) arithmetic. va_list functions (pushvfstring,
//                  pushfstring) replaced by &[u8] forwarders. lua_gc varargs
//                  replaced by explicit GcArgs enum. Raw pointer returns
//                  (topointer, touserdata, upvalueid) return Option<usize>
//                  identity values; actual *mut void only legal in lua-gc.
//                  lua_pushthread stubbed (needs self_gcref()), lua_xmove
//                  stubbed (split-borrow), upvalue_join stubbed (GcRef write).
//                  Phase B must wire up: state.grow_stack, state.call_no_yield,
//                  state.protected_call_raw, state.adjust_results,
//                  state.table_get_with_tm, state.table_set_with_tm,
//                  state.arith_op, state.concat, state.obj_len,
//                  state.obj_to_string, state.str_to_num, state.table_getn,
//                  state.registry_value, state.registry_get,
//                  GcRef::identity, GcRef::ptr_eq, GlobalState GC accessors.
// ──────────────────────────────────────────────────────────────────────────
