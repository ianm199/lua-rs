//! Debug interface — ported from `ldebug.c`.
//!
//! Provides the Lua debug API: stack inspection, source info, variable lookup,
//! hook management, and runtime error formatting.
//!
//! # C source
//! `reference/lua-5.4.7/src/ldebug.c` (962 lines, 30 functions)

// C: #define ldebug_c
// C: #define LUA_CORE

#[allow(unused_imports)] use crate::prelude::*;
use crate::state::{
    CallInfo, GcRef, LuaClosure, LuaClosureLua, LuaProto, LuaState, LuaTable, LuaValue,
    UpVal, CIST_C, CIST_FIN, CIST_HOOKED, CIST_HOOKYIELD, CIST_TAIL, CIST_TRAN,
};
use lua_types::{CallInfoIdx, StackIdx, LuaString};
use lua_types::error::LuaError;
use lua_types::opcode::Instruction;
use crate::vm::InstructionExt;

// TODO(port): the following are cross-crate imports that will resolve in Phase B:
//   - LuaDebug  (lua_Debug struct; Phase E debug)
//   - HookEvent (LUA_HOOKCALL / LUA_HOOKLINE / LUA_HOOKCOUNT constants)
//   - LuaStatus (LUA_OK / LUA_YIELD / LUA_ERRRUN)
//   - luaF_getlocalname — from crate::func
//   - luaT_objtypename  — from crate::tagmethods
//   - luaO_chunkid      — from crate::object
//   - luaD_hookcall, luaD_hook, luaD_callnoyield — from crate::do_
//   - luaH_setint       — from crate::table
//   - luaV_tointegerns  — from crate::vm
//   - OpCode, Instruction field accessors — from lua_code crate

// ─── Constants from macros.tsv / ldebug.h ────────────────────────────────────

// C: #define ABSLINEINFO (-0x80)  — sentinel byte meaning "absolute line info at this pc"
// macros.tsv: ABSLINEINFO → const ABS_LINE_INFO: i8 = -0x80
const ABS_LINE_INFO: i8 = -0x80_i8;

// C: #define MAXIWTHABS 128  — max instructions between absolute-line-info entries
// macros.tsv: MAXIWTHABS → const MAX_IWTH_ABS: i32 = 128
const MAX_IWTH_ABS: i32 = 128;

// C: LUA_IDSIZE — max length of a source identifier in short_src (typically 60)
// TODO(port): import from lua_types or luaconf.h translation
const LUA_IDSIZE: usize = 60;

// C: LUA_MASKLINE / LUA_MASKCOUNT — hook mask bits
// TODO(port): import from HookEvent enum once defined
const LUA_MASKLINE: u8 = 1 << 2;  // C: (1 << LUA_HOOKLINE)
const LUA_MASKCOUNT: u8 = 1 << 3; // C: (1 << LUA_HOOKCOUNT)
const LUA_MASKCALL: u8 = 1 << 0;  // C: (1 << LUA_HOOKCALL)

// C: LUA_HOOKLINE / LUA_HOOKCOUNT event IDs (lua.h)
const LUA_HOOKLINE: i32 = 2;
const LUA_HOOKCOUNT: i32 = 3;

// C: LUA_YIELD status code (lua.h)
// TODO(port): replace with LuaStatus::Yield once enum is defined
const LUA_YIELD_STATUS: i32 = 1;

// C: LUA_ENV — the name of the global environment upvalue
// macros.tsv: LUA_ENV → const LUA_ENV: &[u8] = b"_ENV"
const LUA_ENV: &[u8] = b"_ENV";

// ─── Local error constructors (not yet in lua-types) ─────────────────────────

/// Build a `LuaError::Runtime` from a raw byte-string message.
///
/// TODO(phase-b): expose as `LuaError::runtime_bytes` in lua-types once
/// that crate has a `LuaString::from_bytes` constructor in its public API.
fn runtime_bytes(msg: Vec<u8>) -> LuaError {
    LuaError::Runtime(lua_types::LuaValue::Str(lua_types::GcRef::new(
        lua_types::LuaString::from_bytes(msg),
    )))
}

/// Prepend `[source]:line:` to `msg` when the current call frame is a Lua
/// function. Mirrors what `luaG_addinfo` does for messages routed through
/// `luaG_runerror`; the typed error constructors below build their own
/// message and skip that path, so we add the same prefix here.
fn prefixed_runtime(state: &LuaState, msg: Vec<u8>) -> LuaError {
    let ci_idx = state.current_ci_idx();
    let ci = state.get_ci(ci_idx).clone();
    if !ci.is_lua() {
        return runtime_bytes(msg);
    }
    let proto = ci_lua_proto(&ci, state);
    let src = proto.source_string();
    let line = get_current_line(&ci, state);
    let prefixed = add_info(
        None,
        &msg,
        src.map(|s| &**s),
        line,
    );
    runtime_bytes(prefixed)
}

pub fn c_api_runtime(state: &LuaState, msg: Vec<u8>) -> LuaError {
    let ci_idx = state.current_ci_idx();
    if let Some(parent_idx) = state.prev_ci(ci_idx) {
        let parent_ci = state.get_ci(parent_idx).clone();
        if parent_ci.is_lua() {
            let proto = ci_lua_proto(&parent_ci, state);
            let src = proto.source_string();
            let line = get_current_line(&parent_ci, state);
            let prefixed = add_info(None, &msg, src.map(|s| &**s), line);
            return runtime_bytes(prefixed);
        }
    }
    runtime_bytes(msg)
}

/// Equivalent of C `luaL_argerror`: build an arg-type error with function name
/// (from debug info) and caller source location. Handles method calls by
/// producing "calling 'f' on bad self ..." when arg==1 and namewhat=="method".
pub fn arg_error_impl(state: &mut LuaState, mut arg: i32, extramsg: &[u8]) -> LuaError {
    let mut ar = LuaDebug::default();
    if !get_stack(state, 0, &mut ar) {
        let msg = format!("bad argument #{} ({})", arg, String::from_utf8_lossy(extramsg));
        return c_api_runtime(state, msg.into_bytes());
    }
    get_info(state, b"n", &mut ar);
    if ar.namewhat.as_deref() == Some(b"method") {
        arg -= 1;
        if arg == 0 {
            let name = ar.name.clone().unwrap_or_else(|| b"?".to_vec());
            let msg = format!(
                "calling '{}' on bad self ({})",
                String::from_utf8_lossy(&name),
                String::from_utf8_lossy(extramsg)
            );
            return c_api_runtime(state, msg.into_bytes());
        }
    }
    let fname = ar.name.clone().unwrap_or_else(|| b"?".to_vec());
    let msg = format!(
        "bad argument #{} to '{}' ({})",
        arg,
        String::from_utf8_lossy(&fname),
        String::from_utf8_lossy(extramsg)
    );
    c_api_runtime(state, msg.into_bytes())
}

/// Build a `LuaError::Runtime` from the top of the Lua stack.
///
/// Pops the error value from the stack and wraps it.
/// TODO(phase-b): expose as `LuaError::from_top` in lua-types.
fn runtime_from_top(state: &mut crate::state::LuaState) -> LuaError {
    let v = state.pop();
    LuaError::Runtime(v)
}

// ─── Debug info structures ────────────────────────────────────────────────────

/// Debug introspection record.
///
/// C: `lua_Debug` in `lua.h`. Full mapping deferred to Phase E; this struct
/// holds only the fields that `ldebug.c` writes/reads.
///
/// # Port note
/// `name` and `namewhat` are optional byte strings because in C they can be
/// NULL. `source` is owned here because we build it from Proto.source (a GcRef).
/// `short_src` matches C layout as a fixed array.
pub struct LuaDebug {
    // C: int event
    pub event: i32,
    // C: const char *name  — (n) variable/function name
    pub name: Option<Vec<u8>>,
    // C: const char *namewhat  — (n) "global"/"local"/etc.
    pub namewhat: Option<&'static [u8]>,
    // C: const char *what  — (S) "Lua"/"C"/"main"
    pub what: Option<&'static [u8]>,
    // C: const char *source  — (S) source chunk name (raw bytes)
    pub source: Option<Vec<u8>>,
    // C: size_t srclen
    pub srclen: usize,
    // C: int currentline  — (l)
    pub currentline: i32,
    // C: int linedefined  — (S)
    pub linedefined: i32,
    // C: int lastlinedefined  — (S)
    pub lastlinedefined: i32,
    // C: unsigned char nups  — (u) number of upvalues
    pub nups: u8,
    // C: unsigned char nparams  — (u)
    pub nparams: u8,
    // C: char isvararg  — (u)
    pub isvararg: bool,
    // C: char istailcall  — (t)
    pub istailcall: bool,
    // C: unsigned short ftransfer / ntransfer  — (r)
    pub ftransfer: u16,
    pub ntransfer: u16,
    // C: char short_src[LUA_IDSIZE]  — (S) truncated source id
    pub short_src: [u8; LUA_IDSIZE],
    // C: struct CallInfo *i_ci  — private; the active CallInfo
    // PORT NOTE: C stores a raw pointer; Rust stores an index into LuaState.call_stack.
    pub i_ci: Option<CallInfoIdx>,
}

impl Default for LuaDebug {
    fn default() -> Self {
        LuaDebug {
            event: 0,
            name: None,
            namewhat: None,
            what: None,
            source: None,
            srclen: 0,
            currentline: -1,
            linedefined: -1,
            lastlinedefined: -1,
            nups: 0,
            nparams: 0,
            isvararg: false,
            istailcall: false,
            ftransfer: 0,
            ntransfer: 0,
            short_src: [0u8; LUA_IDSIZE],
            i_ci: None,
        }
    }
}

// ─── File-local helper: is this a Lua (non-C) closure? ───────────────────────

// C: #define LuaClosure(f) ((f) != NULL && (f)->c.tt == LUA_VLCL)
// macros.tsv: LUA_VLCL → LuaClosure::Lua(_)
#[inline]
fn is_lua_closure(cl: Option<&LuaClosure>) -> bool {
    // C: (f) != NULL && (f)->c.tt == LUA_VLCL
    matches!(cl, Some(LuaClosure::Lua(_)))
}

// ─── Current-PC helpers ───────────────────────────────────────────────────────

/// Returns the program counter (0-based instruction index) for the current
/// instruction in call frame `ci`.
///
/// C: `static int currentpc(CallInfo *ci)`
/// ```c
/// lua_assert(isLua(ci));
/// return pcRel(ci->u.l.savedpc, ci_func(ci)->p);
/// ```
///
/// PORT NOTE: In C, `savedpc` is a pointer to the *next* instruction. `pcRel`
/// subtracts the code base and then subtracts 1 more to get the *current*
/// instruction. In Rust, `saved_pc()` stores the 0-based index of the next
/// instruction, so the current instruction index is `saved_pc() - 1`.
fn current_pc(ci: &CallInfo) -> i32 {
    // C: lua_assert(isLua(ci))
    debug_assert!(ci.is_lua());
    // C: pcRel(ci->u.l.savedpc, ci_func(ci)->p)
    // macros.tsv: pcRel → (pc - proto.code_base()) as i32 - 1
    // In Rust savedpc is a u32 offset into code[]; current = savedpc - 1
    ci.saved_pc().saturating_sub(1) as i32
}

// ─── Line-info lookup ─────────────────────────────────────────────────────────

/// Finds the "base line" entry in `f.abslineinfo` for instruction `pc`.
///
/// Sets `*basepc` to the pc of the base entry (or -1 if starting from the
/// function's first line), and returns the line number at that base.
///
/// C: `static int getbaseline(const Proto *f, int pc, int *basepc)`
fn get_baseline(f: &LuaProto, pc: i32, basepc: &mut i32) -> i32 {
    // C: if (f->sizeabslineinfo == 0 || pc < f->abslineinfo[0].pc)
    if f.abslineinfo.is_empty() || pc < f.abslineinfo[0].pc {
        // C: *basepc = -1; return f->linedefined;
        *basepc = -1;
        return f.linedefined;
    }
    // C: int i = cast_uint(pc) / MAXIWTHABS - 1;
    // macros.tsv: cast_uint(x) → x as u32
    let mut i = (pc as u32 / MAX_IWTH_ABS as u32).saturating_sub(1) as usize;
    // C: lua_assert(i < 0 || (i < f->sizeabslineinfo && f->abslineinfo[i].pc <= pc))
    debug_assert!(
        i < f.abslineinfo.len() && f.abslineinfo[i].pc <= pc,
        "getbaseline: estimate is not a lower bound"
    );
    // C: while (i + 1 < f->sizeabslineinfo && pc >= f->abslineinfo[i + 1].pc) i++;
    while i + 1 < f.abslineinfo.len() && pc >= f.abslineinfo[i + 1].pc {
        i += 1;
    }
    // C: *basepc = f->abslineinfo[i].pc; return f->abslineinfo[i].line;
    *basepc = f.abslineinfo[i].pc;
    f.abslineinfo[i].line
}

/// Returns the source line number corresponding to instruction `pc` in proto `f`.
/// Returns -1 if the proto has no debug line information.
///
/// C: `int luaG_getfuncline(const Proto *f, int pc)` (LUAI_FUNC)
pub(crate) fn get_func_line(f: &LuaProto, pc: i32) -> i32 {
    // C: if (f->lineinfo == NULL) return -1;
    if f.lineinfo.is_empty() {
        return -1;
    }
    let mut basepc: i32 = 0;
    let mut baseline = get_baseline(f, pc, &mut basepc);
    // C: while (basepc++ < pc) { assert != ABSLINEINFO; baseline += f->lineinfo[basepc]; }
    // PORT NOTE: C uses post-increment `basepc++` in the condition; the body
    // then uses the already-incremented value. Rewritten as pre-increment.
    while basepc < pc {
        basepc += 1;
        // C: lua_assert(f->lineinfo[basepc] != ABSLINEINFO)
        debug_assert!(
            f.lineinfo[basepc as usize] != ABS_LINE_INFO,
            "get_func_line: hit ABSLINEINFO in incremental walk"
        );
        // C: baseline += f->lineinfo[basepc]
        baseline += f.lineinfo[basepc as usize] as i32;
    }
    baseline
}

/// Returns the source line for the current instruction in call frame `ci`.
///
/// C: `static int getcurrentline(CallInfo *ci)`
fn get_current_line(ci: &CallInfo, state: &LuaState) -> i32 {
    // C: return luaG_getfuncline(ci_func(ci)->p, currentpc(ci));
    let proto = ci_lua_proto(ci, state);
    get_func_line(&proto, current_pc(ci))
}

// ─── Hook support ─────────────────────────────────────────────────────────────

/// Sets the `trap` flag on every active Lua call frame so that the VM checks
/// debug hooks before each instruction.
///
/// C: `static void settraps(CallInfo *ci)`
///
/// PORT NOTE: In C this walks an intrusive doubly-linked list. In Rust,
/// `LuaState.call_stack` is a `Vec<CallInfo>`, so we iterate the slice.
fn set_traps(state: &mut LuaState) {
    // C: for (; ci != NULL; ci = ci->previous)
    //      if (isLua(ci)) ci->u.l.trap = 1;
    // TODO(port): call_stack iteration API not yet finalised; this will change
    // when LuaState.call_stack is fully implemented.
    for ci in state.call_stack_mut().iter_mut() {
        if ci.is_lua() {
            ci.set_trap(true);
        }
    }
}

/// Installs a debug hook on thread `state`.
///
/// C: `LUA_API void lua_sethook(lua_State *L, lua_Hook func, int mask, int count)`
pub fn set_hook(
    state: &mut LuaState,
    func: Option<Box<dyn FnMut(&mut LuaState, &LuaDebug)>>,
    mask: i32,
    count: i32,
) {
    // C: if (func == NULL || mask == 0) { mask = 0; func = NULL; }
    let (func, mask) = if func.is_none() || mask == 0 {
        (None, 0i32)
    } else {
        (func, mask)
    };
    // C: L->hook = func; L->basehookcount = count;
    state.set_hook(func);
    state.set_base_hook_count(count);
    // C: resethookcount(L)
    // macros.tsv: resethookcount → state.reset_hook_count()
    state.reset_hook_count();
    // C: L->hookmask = cast_byte(mask)
    // macros.tsv: cast_byte(x) → x as u8
    state.set_hook_mask(mask as u8);
    // C: if (mask) settraps(L->ci)
    if mask != 0 {
        set_traps(state);
    }
}

/// Returns the current debug hook function, if any.
///
/// C: `LUA_API lua_Hook lua_gethook(lua_State *L)`
///
/// TODO(port): In C this returns a `lua_Hook` function pointer. In Rust the hook
/// is a `Box<dyn FnMut>` and cannot be returned by raw reference without
/// restructuring; for now returns a bool indicating whether a hook is installed.
pub fn get_hook_installed(state: &LuaState) -> bool {
    state.hook().is_some()
}

/// Returns the current hook event mask.
///
/// C: `LUA_API int lua_gethookmask(lua_State *L)`
pub fn get_hook_mask(state: &LuaState) -> i32 {
    state.hook_mask() as i32
}

/// Returns the current hook call count.
///
/// C: `LUA_API int lua_gethookcount(lua_State *L)`
pub fn get_hook_count(state: &LuaState) -> i32 {
    state.base_hook_count()
}

// ─── Stack introspection ──────────────────────────────────────────────────────

/// Fills `ar` with information about the call frame at depth `level`.
/// Level 0 is the current running function, level 1 is the caller, etc.
/// Returns `true` on success, `false` if the level is out of range.
///
/// C: `LUA_API int lua_getstack(lua_State *L, int level, lua_Debug *ar)`
pub fn get_stack(state: &LuaState, level: i32, ar: &mut LuaDebug) -> bool {
    // C: if (level < 0) return 0;
    if level < 0 {
        return false;
    }
    // C: for (ci = L->ci; level > 0 && ci != &L->base_ci; ci = ci->previous) level--;
    let mut remaining = level;
    let mut ci_idx = state.current_ci_idx();
    loop {
        if remaining == 0 {
            break;
        }
        match state.prev_ci(ci_idx) {
            Some(prev) => {
                ci_idx = prev;
                remaining -= 1;
            }
            None => {
                // C: else status = 0;  no such level
                return false;
            }
        }
    }
    // C: if (level == 0 && ci != &L->base_ci) { status = 1; ar->i_ci = ci; }
    if !state.is_base_ci(ci_idx) {
        ar.i_ci = Some(ci_idx);
        true
    } else {
        false
    }
}

// ─── Upvalue and local variable name lookup ───────────────────────────────────

/// Returns the name of upvalue `uv` in proto `p` (as a byte slice), or `b"?"`.
///
/// C: `static const char *upvalname(const Proto *p, int uv)`
fn upval_name(p: &LuaProto, uv: usize) -> &[u8] {
    // C: TString *s = check_exp(uv < p->sizeupvalues, p->upvalues[uv].name);
    //    if (s == NULL) return "?"; else return getstr(s);
    // macros.tsv: check_exp(c, e) → { debug_assert!(c); e }
    debug_assert!(uv < p.upvalues.len(), "upval_name: index out of range");
    // TODO(port): UpvalDesc.name is GcRef<LuaString>; calling .as_bytes() requires
    // access to the interned string's data. Actual lifetime is tied to the GcRef.
    p.upvalues[uv].name.as_ref().map_or(b"?" as &[u8], |s| s.as_bytes())
}

/// Finds the stack slot for vararg value number `n` (n is negative) in `ci`.
/// Returns `Some(pos)` and the name `b"(vararg)"` if found, else `None`.
///
/// C: `static const char *findvararg(CallInfo *ci, int n, StkId *pos)`
///
/// PORT NOTE: C sets `*pos` as an out-parameter. Rust returns an Option of the
/// stack index alongside the name.
fn find_vararg(ci: &CallInfo, n: i32) -> Option<(StackIdx, &'static [u8])> {
    // C: if (clLvalue(s2v(ci->func.p))->p->is_vararg)
    // TODO(port): accessing proto from ci requires LuaState; restructured to take
    // nextraargs and is_vararg directly. Phase B will wire this properly.
    if ci.is_vararg_func() {
        let nextra = ci.nextra_args();
        // C: if (n >= -nextra)  — 'n' is negative
        if n >= -(nextra as i32) {
            // C: *pos = ci->func.p - nextra - (n + 1);
            // PORT NOTE: pointer arithmetic converted to index arithmetic.
            // ci->func.p is the function slot; varargs are at func - nextra - 1 .. func - 1
            let pos = ci.func.wrapping_sub((nextra as i32 + n + 1) as u32);
            return Some((StackIdx(pos), b"(vararg)" as &[u8]));
        }
    }
    None
}

/// Finds the name and stack position for local variable `n` in call frame `ci`.
///
/// - If `n > 0`, looks up as a numbered local (1-based).
/// - If `n < 0`, looks up as a vararg slot.
/// - Returns `None` if no such variable exists.
/// - If `pos` is `Some`, sets it to the variable's stack index.
///
/// C: `const char *luaG_findlocal(lua_State *L, CallInfo *ci, int n, StkId *pos)`
///
/// PORT NOTE: returns an owned `Vec<u8>` rather than `&[u8]`. The Lua-function
/// case must call `get_local_name`, which returns a slice borrowed from a
/// `GcRef<LuaProto>` that drops at function end — there is no caller lifetime
/// the slice could be tied to. Cloning the name is cheap (a handful of bytes).
pub(crate) fn find_local(
    state: &LuaState,
    ci: &CallInfo,
    n: i32,
    pos: Option<&mut StackIdx>,
) -> Option<Vec<u8>> {
    // C: StkId base = ci->func.p + 1;
    let base = ci.func + 1;
    // C: const char *name = NULL;
    let mut name: Option<Vec<u8>> = None;

    if ci.is_lua() {
        if n < 0 {
            // C: if (n < 0) return findvararg(ci, n, pos);
            if let Some((vpos, vname)) = find_vararg(ci, n) {
                if let Some(out_pos) = pos {
                    *out_pos = vpos;
                }
                return Some(vname.to_vec());
            }
            return None;
        } else {
            // C: name = luaF_getlocalname(ci_func(ci)->p, n, currentpc(ci));
            let proto = ci_lua_proto(ci, state);
            let pc = current_pc(ci);
            name = crate::func::get_local_name(&proto, n, pc).map(|s| s.to_vec());
        }
    }

    if name.is_none() {
        // C: StkId limit = (ci == L->ci) ? L->top.p : ci->next->func.p;
        // TODO(phase-b): replace pointer-equality check with index-based once
        // ci becomes a CallInfoIdx end-to-end.
        let limit: u32 = state.top_idx().0;
        let _ = ci.next;
        // C: if (limit - base >= n && n > 0)
        if n > 0 && limit.saturating_sub(base.0) >= n as u32 {
            // C: name = isLua(ci) ? "(temporary)" : "(C temporary)";
            name = Some(if ci.is_lua() { b"(temporary)".to_vec() } else { b"(C temporary)".to_vec() });
        } else {
            return None;
        }
    }

    // C: if (pos) *pos = base + (n - 1);
    if let Some(out_pos) = pos {
        *out_pos = base + (n - 1);
    }
    name
}

/// Gets the name and value of local variable `n` in call frame `ar->i_ci`
/// (or in the function at the top of the stack if `ar` is NULL).
/// Pushes the value on the stack and returns its name, or returns `None`.
///
/// C: `LUA_API const char *lua_getlocal(lua_State *L, const lua_Debug *ar, int n)`
pub fn get_local(state: &mut LuaState, ar: Option<&LuaDebug>, n: i32) -> Option<Vec<u8>> {
    // C: lua_lock(L);  — no-op; macros.tsv: lua_lock → (drop)
    if ar.is_none() {
        // C: if (!isLfunction(s2v(L->top.p - 1))) name = NULL;
        // macros.tsv: isLfunction → matches!(o, LuaValue::Function(LuaClosure::Lua(_)))
        let top_val = state.peek_top();
        if !matches!(top_val, LuaValue::Function(LuaClosure::Lua(_))) {
            return None;
        }
        // C: name = luaF_getlocalname(clLvalue(s2v(L->top.p-1))->p, n, 0);
        // PORT NOTE: reshaped for borrowck — convert to owned Vec<u8> inside the
        // block so `cl` (and the borrow through it) drop before we return.
        let name_owned: Option<Vec<u8>> = {
            let cl = match top_val {
                LuaValue::Function(LuaClosure::Lua(ref cl)) => cl.clone(),
                _ => unreachable!(),
            };
            // TODO(port): access proto from LuaClosureLua GcRef
            get_local_name_from_closure(&cl, n, 0).map(|s| s.to_vec())
        };
        return name_owned;
    }

    // C: else { StkId pos = NULL; name = luaG_findlocal(L, ar->i_ci, n, &pos); ... }
    let ar = ar.unwrap();
    let ci_idx = ar.i_ci?;
    let ci = state.get_ci(ci_idx).clone();
    let mut pos = StackIdx(0);
    // PORT NOTE: reshaped for borrowck — clone name to an owned Vec<u8> so the
    // immutable borrow of `state` ends before the mutable push below.
    let name_owned: Option<Vec<u8>> = find_local(state, &ci, n, Some(&mut pos));

    if name_owned.is_some() {
        // C: setobjs2s(L, L->top.p, pos); api_incr_top(L);
        let val = state.get_at(pos).clone();
        state.push(val);
    }
    // C: lua_unlock(L);  — no-op
    name_owned
}

/// Sets local variable `n` in call frame `ar->i_ci` to the value on top of the
/// stack. Pops the value and returns the variable name, or returns `None`.
///
/// C: `LUA_API const char *lua_setlocal(lua_State *L, const lua_Debug *ar, int n)`
pub fn set_local(state: &mut LuaState, ar: &LuaDebug, n: i32) -> Option<Vec<u8>> {
    // C: StkId pos = NULL; lua_lock(L);
    let ci_idx = ar.i_ci?;
    let ci = state.get_ci(ci_idx).clone();
    let mut pos = StackIdx(0);
    // PORT NOTE: reshaped for borrowck — clone name before mutably borrowing state.
    let name_owned: Option<Vec<u8>> = find_local(state, &ci, n, Some(&mut pos));
    if name_owned.is_some() {
        // C: setobjs2s(L, pos, L->top.p - 1); L->top.p--;
        let val = state.get_at(state.top_idx() - 1).clone();
        state.set_at(pos, val);
        state.pop_n(1);
    }
    // C: lua_unlock(L);
    name_owned
}

// ─── Function info helpers ────────────────────────────────────────────────────

/// Fills the source/line fields of `ar` from closure `cl`.
///
/// C: `static void funcinfo(lua_Debug *ar, Closure *cl)`
fn func_info(ar: &mut LuaDebug, cl: Option<&LuaClosure>) {
    if !is_lua_closure(cl) {
        // C: ar->source = "=[C]"; ar->srclen = LL("=[C]"); ar->linedefined = -1; ...
        // macros.tsv: LL(x) → literal.len()
        ar.source = Some(b"=[C]".to_vec());
        ar.srclen = b"=[C]".len();
        ar.linedefined = -1;
        ar.lastlinedefined = -1;
        ar.what = Some(b"C");
    } else {
        let lua_cl = match cl {
            Some(LuaClosure::Lua(cl)) => cl,
            _ => unreachable!(),
        };
        // C: const Proto *p = cl->l.p;
        // TODO(port): access proto via GcRef<LuaProto>
        let proto: &LuaProto = &lua_cl.proto;
        // C: if (p->source) { ar->source = getstr(p->source); ar->srclen = tsslen(...); }
        // macros.tsv: getstr(ts) → ts.as_bytes(); tsslen(ts) → ts.len()
        // TODO(port): LuaProto.source is GcRef<LuaString>; call .as_bytes() properly
        ar.source = Some(proto.source_bytes().to_vec());
        ar.srclen = proto.source_bytes().len();
        ar.linedefined = proto.linedefined;
        ar.lastlinedefined = proto.lastlinedefined;
        // C: ar->what = (ar->linedefined == 0) ? "main" : "Lua";
        ar.what = Some(if ar.linedefined == 0 { b"main" } else { b"Lua" });
    }
    // C: luaO_chunkid(ar->short_src, ar->source, ar->srclen);
    // TODO(port): luaO_chunkid lives in crate::object; call it once available
    chunk_id(&mut ar.short_src, ar.source.as_deref().unwrap_or(b"?"), ar.srclen);
}

/// Returns the line number after advancing by one instruction from `currentline`.
/// Handles the ABSLINEINFO sentinel by falling through to `get_func_line`.
///
/// C: `static int nextline(const Proto *p, int currentline, int pc)`
fn next_line(p: &LuaProto, currentline: i32, pc: usize) -> i32 {
    // C: if (p->lineinfo[pc] != ABSLINEINFO) return currentline + p->lineinfo[pc];
    //    else return luaG_getfuncline(p, pc);
    if p.lineinfo.get(pc).copied() != Some(ABS_LINE_INFO) {
        currentline + p.lineinfo[pc] as i32
    } else {
        get_func_line(p, pc as i32)
    }
}

/// Collects all source lines that are covered by instructions in closure `f`
/// into a new table and pushes it on the stack (or pushes `nil` for C functions).
///
/// C: `static void collectvalidlines(lua_State *L, Closure *f)`
fn collect_valid_lines(state: &mut LuaState, cl: Option<&LuaClosure>) -> Result<(), LuaError> {
    if !is_lua_closure(cl) {
        // C: setnilvalue(s2v(L->top.p)); api_incr_top(L);
        // macros.tsv: setnilvalue → *o = LuaValue::Nil; api_incr_top → gone
        state.push(LuaValue::Nil);
        return Ok(());
    }
    let lua_cl = match cl {
        Some(LuaClosure::Lua(cl)) => cl.clone(),
        _ => unreachable!(),
    };
    // C: const Proto *p = f->l.p;
    // TODO(port): access proto via GcRef<LuaProto>
    let proto: GcRef<LuaProto> = lua_cl.proto.clone();
    let p: &LuaProto = &proto;

    // C: int currentline = p->linedefined;
    let mut currentline = p.linedefined;

    // C: Table *t = luaH_new(L);
    // macros.tsv: luaH_new(L) → state.new_table()
    let t = state.new_table();
    // C: sethvalue2s(L, L->top.p, t); api_incr_top(L);
    // macros.tsv: sethvalue2s → state.set_at(o, LuaValue::Table(t.clone()))
    state.push(LuaValue::Table(t.clone()));

    // C: if (p->lineinfo != NULL)
    if !p.lineinfo.is_empty() {
        // C: TValue v; setbtvalue(&v);  — boolean true as the value for all lines
        // macros.tsv: setbtvalue → *o = LuaValue::Bool(true)
        let v = LuaValue::Bool(true);

        // C: if (!p->is_vararg) i = 0; else { assert OP_VARARGPREP; currentline = nextline(..., 0); i = 1; }
        let start_i = if !p.is_vararg {
            0usize
        } else {
            // C: lua_assert(GET_OPCODE(p->code[0]) == OP_VARARGPREP)
            // TODO(port): verify opcode — GET_OPCODE lives in lua_code crate
            debug_assert!(
                p.code.first().map(|i| i.is_vararg_prep()).unwrap_or(false),
                "collect_valid_lines: first instruction of vararg should be OP_VARARGPREP"
            );
            currentline = next_line(p, currentline, 0);
            1usize
        };

        // C: for (; i < p->sizelineinfo; i++) { currentline = nextline(p, currentline, i); luaH_setint(L, t, currentline, &v); }
        // PORT NOTE: C iterates up to sizelineinfo (same as lineinfo.len() in Rust).
        for i in start_i..p.lineinfo.len() {
            currentline = next_line(p, currentline, i);
            // C: luaH_setint(L, t, currentline, &v)
            // TODO(port): luaH_setint lives in crate::table; stub call here
            t.raw_set_int(state, currentline as i64, v.clone())?;
        }
    }
    Ok(())
}

// ─── Function naming (symbolic execution) ────────────────────────────────────

/// Tries to find a name for the function being called, based on the calling
/// call frame `ci`. Returns `None` if the frame is tail-called or unavailable.
///
/// C: `static const char *getfuncname(lua_State *L, CallInfo *ci, const char **name)`
fn get_func_name<'a>(
    state: &'a LuaState,
    ci: Option<&CallInfo>,
    name: &mut Option<Vec<u8>>,
) -> Option<&'static [u8]> {
    // C: if (ci != NULL && !(ci->callstatus & CIST_TAIL))
    //      return funcnamefromcall(L, ci->previous, name);
    //    else return NULL;
    let ci = ci?;
    if ci.callstatus & CIST_TAIL != 0 {
        return None;
    }
    // TODO(port): ci->previous requires navigating call_stack by prev idx
    // TODO(phase-b): get_prev_ci needs to accept &CallInfo or take the previous idx.
    let prev_idx = ci.previous?;
    let prev_ci = state.get_ci(prev_idx).clone();
    funcname_from_call(state, &prev_ci, name)
}

/// Fills `ar` with the requested debug information about closure `f` / frame `ci`.
///
/// C: `static int auxgetinfo(lua_State *L, const char *what, lua_Debug *ar, Closure *f, CallInfo *ci)`
fn aux_get_info(
    state: &LuaState,
    what: &[u8],
    ar: &mut LuaDebug,
    cl: Option<&LuaClosure>,
    ci: Option<&CallInfo>,
) -> bool {
    let mut status = true;
    // C: for (; *what; what++) { switch (*what) { ... } }
    for &ch in what {
        match ch {
            // C: case 'S': funcinfo(ar, f); break;
            b'S' => {
                func_info(ar, cl);
            }
            // C: case 'l': ar->currentline = (ci && isLua(ci)) ? getcurrentline(ci) : -1; break;
            b'l' => {
                ar.currentline = match ci {
                    Some(ci) if ci.is_lua() => get_current_line(ci, state),
                    _ => -1,
                };
            }
            // C: case 'u': ar->nups = ...; ar->isvararg = ...; ar->nparams = ...; break;
            b'u' => {
                ar.nups = cl.map_or(0, |c| c.nupvalues() as u8);
                match cl {
                    Some(LuaClosure::Lua(lua_cl)) => {
                        // TODO(port): access proto via GcRef<LuaProto>
                        ar.isvararg = lua_cl.proto.is_vararg;
                        ar.nparams = lua_cl.proto.numparams;
                    }
                    _ => {
                        // C: ar->isvararg = 1; ar->nparams = 0;
                        ar.isvararg = true;
                        ar.nparams = 0;
                    }
                }
            }
            // C: case 't': ar->istailcall = (ci) ? ci->callstatus & CIST_TAIL : 0; break;
            b't' => {
                ar.istailcall = ci.map_or(false, |ci| ci.callstatus & CIST_TAIL != 0);
            }
            // C: case 'n': ar->namewhat = getfuncname(L, ci, &ar->name); ...
            b'n' => {
                let mut name: Option<Vec<u8>> = None;
                ar.namewhat = get_func_name(state, ci, &mut name);
                if ar.namewhat.is_none() {
                    // C: ar->namewhat = ""; ar->name = NULL;
                    ar.namewhat = Some(b"");
                    ar.name = None;
                } else {
                    ar.name = name;
                }
            }
            // C: case 'r': if (ci == NULL || !(ci->callstatus & CIST_TRAN)) { ftransfer = ntransfer = 0; }
            //              else { ftransfer = ...; ntransfer = ...; }
            b'r' => match ci {
                Some(ci) if ci.callstatus & CIST_TRAN != 0 => {
                    // TODO(port): ci->u2.transferinfo.ftransfer / ntransfer
                    ar.ftransfer = ci.transfer_ftransfer();
                    ar.ntransfer = ci.transfer_ntransfer();
                }
                _ => {
                    ar.ftransfer = 0;
                    ar.ntransfer = 0;
                }
            },
            // C: case 'L': case 'f': handled by lua_getinfo; break;
            b'L' | b'f' => {}
            // C: default: status = 0;
            _ => {
                status = false;
            }
        }
    }
    status
}

/// Returns debug information about a function or active call frame.
///
/// C: `LUA_API int lua_getinfo(lua_State *L, const char *what, lua_Debug *ar)`
pub fn get_info(state: &mut LuaState, what: &[u8], ar: &mut LuaDebug) -> bool {
    // C: lua_lock(L);
    // C: if (*what == '>') { ci = NULL; func = s2v(L->top.p - 1); what++; L->top.p--; }
    // C: else               { ci = ar->i_ci; func = s2v(ci->func.p); }
    let (cl, ci_idx, func_val, what) = if what.first() == Some(&b'>') {
        let func_val = state.peek_at(state.top_idx() - 1).clone();
        state.pop_n(1);
        // C: api_check(L, ttisfunction(func), "function expected")
        debug_assert!(
            matches!(func_val, LuaValue::Function(_)),
            "get_info: function expected"
        );
        let cl = match &func_val {
            LuaValue::Function(LuaClosure::Lua(_) | LuaClosure::C(_)) => {
                // C: cl = ttisclosure(func) ? clvalue(func) : NULL
                Some(match &func_val {
                    LuaValue::Function(c) => c.clone(),
                    _ => unreachable!(),
                })
            }
            _ => None,
        };
        (cl, None, func_val, &what[1..])
    } else {
        let ci_idx = match ar.i_ci { Some(i) => i, None => return false };
        // C: func = s2v(ci->func.p)
        let func_val = state.get_at(state.get_ci(ci_idx).func).clone();
        debug_assert!(
            matches!(func_val, LuaValue::Function(_)),
            "get_info: non-function at ci->func"
        );
        let cl = match &func_val {
            LuaValue::Function(LuaClosure::Lua(_) | LuaClosure::C(_)) => {
                Some(match &func_val {
                    LuaValue::Function(c) => c.clone(),
                    _ => unreachable!(),
                })
            }
            _ => None,
        };
        (cl, Some(ci_idx), func_val, what)
    };

    let ci = ci_idx.and_then(|idx| Some(state.get_ci(idx).clone()));
    let status = aux_get_info(state, what, ar, cl.as_ref(), ci.as_ref());

    // C: if (strchr(what, 'f')) { setobj2s(L, L->top.p, func); api_incr_top(L); }
    if what.contains(&b'f') {
        state.push(func_val);
    }
    // C: if (strchr(what, 'L')) collectvalidlines(L, cl);
    if what.contains(&b'L') {
        // TODO(port): propagate error from collect_valid_lines
        let _ = collect_valid_lines(state, cl.as_ref());
    }
    // C: lua_unlock(L);
    status
}

// ─── Symbolic execution — finding which instruction set a register ────────────

/// Filters a pc: if `pc` is inside a conditional branch (before `jmptarget`),
/// returns -1 (unknown); otherwise returns `pc`.
///
/// C: `static int filterpc(int pc, int jmptarget)`
#[inline]
fn filter_pc(pc: i32, jmptarget: i32) -> i32 {
    // C: if (pc < jmptarget) return -1; else return pc;
    if pc < jmptarget { -1 } else { pc }
}

/// Finds the last instruction before `lastpc` that wrote to register `reg`.
/// Returns the pc of that instruction, or -1 if not found.
///
/// C: `static int findsetreg(const Proto *p, int lastpc, int reg)`
fn find_set_reg(p: &LuaProto, lastpc: i32, reg: i32) -> i32 {
    // C: int setreg = -1; int jmptarget = 0;
    let mut setreg: i32 = -1;
    let mut jmptarget: i32 = 0;

    // C: if (testMMMode(GET_OPCODE(p->code[lastpc]))) lastpc--;
    // macros.tsv: testMMMode(op) → (luaP_opmodes[op as usize] & (1 << 7)) != 0
    // TODO(port): GET_OPCODE and opmode tests live in lua_code crate
    let effective_lastpc = if p.code.get(lastpc as usize).map_or(false, |i| i.is_mm_mode()) {
        lastpc - 1
    } else {
        lastpc
    };

    // C: for (pc = 0; pc < lastpc; pc++) { ... }
    for pc in 0..effective_lastpc {
        // C: Instruction i = p->code[pc]; OpCode op = GET_OPCODE(i); int a = GETARG_A(i);
        let instr = p.code[pc as usize];
        let op = instr.opcode();
        let a = instr.arg_a() as i32;

        let change = match op {
            // C: case OP_LOADNIL: int b = GETARG_B(i); change = (a <= reg && reg <= a + b);
            OpCode::LoadNil => {
                let b = instr.arg_b() as i32;
                a <= reg && reg <= a + b
            }
            // C: case OP_TFORCALL: change = (reg >= a + 2);
            OpCode::TForCall => reg >= a + 2,
            // C: case OP_CALL: case OP_TAILCALL: change = (reg >= a);
            OpCode::Call | OpCode::TailCall => reg >= a,
            // C: case OP_JMP: { int b = GETARG_sJ(i); int dest = pc + 1 + b; ...  change = 0; }
            OpCode::Jmp => {
                let b = instr.arg_s_j();
                let dest = pc + 1 + b;
                if dest <= effective_lastpc && dest > jmptarget {
                    jmptarget = dest;
                }
                false
            }
            // C: default: change = (testAMode(op) && reg == a);
            _ => {
                // macros.tsv: testAMode(op) → (luaP_opmodes[op as usize] & (1 << 3)) != 0
                // TODO(port): opmode table lives in lua_code crate
                instr.test_a_mode() && reg == a
            }
        };

        if change {
            setreg = filter_pc(pc, jmptarget);
        }
    }
    setreg
}

/// Finds a "name" for the constant at `index` in proto `p`.
/// Returns `Some("constant")` and sets `*name` to the string content,
/// or returns `None` and sets `*name` to `"?"`.
///
/// C: `static const char *kname(const Proto *p, int index, const char **name)`
fn kname<'a>(p: &'a LuaProto, index: usize, name: &mut &'a [u8]) -> Option<&'static [u8]> {
    // C: TValue *kvalue = &p->k[index];
    //    if (ttisstring(kvalue)) { *name = getstr(tsvalue(kvalue)); return "constant"; }
    //    else { *name = "?"; return NULL; }
    match p.k.get(index) {
        Some(LuaValue::Str(s)) => {
            // TODO(port): as_bytes() lifetime is tied to GcRef; revisit in Phase B
            *name = s.as_bytes();
            Some(b"constant")
        }
        _ => {
            *name = b"?";
            None
        }
    }
}

/// Tries to find a basic name for register `reg` in proto `p` at instruction `ppc`.
/// Returns the "kind" of the name (e.g. "local", "upvalue", "constant"), or `None`.
///
/// C: `static const char *basicgetobjname(const Proto *p, int *ppc, int reg, const char **name)`
fn basic_get_obj_name<'a>(
    p: &'a LuaProto,
    ppc: &mut i32,
    reg: i32,
    name: &mut &'a [u8],
) -> Option<&'static [u8]> {
    let pc = *ppc;
    // C: *name = luaF_getlocalname(p, reg + 1, pc);
    //    if (*name) return "local";
    if let Some(local_name) = get_local_name(p, reg + 1, pc) {
        *name = local_name;
        return Some(b"local");
    }

    // C: *ppc = pc = findsetreg(p, pc, reg);
    *ppc = find_set_reg(p, pc, reg);
    let pc = *ppc;

    if pc == -1 {
        return None;
    }

    let instr = p.code[pc as usize];
    let op = instr.opcode();
    match op {
        // C: case OP_MOVE: int b = GETARG_B(i); if (b < GETARG_A(i)) return basicgetobjname(p, ppc, b, name);
        OpCode::Move => {
            let b = instr.arg_b() as i32;
            if b < instr.arg_a() as i32 {
                return basic_get_obj_name(p, ppc, b, name);
            }
        }
        // C: case OP_GETUPVAL: *name = upvalname(p, GETARG_B(i)); return "upvalue";
        OpCode::GetUpVal => {
            *name = upval_name(p, instr.arg_b() as usize);
            return Some(b"upvalue");
        }
        // C: case OP_LOADK: return kname(p, GETARG_Bx(i), name);
        OpCode::LoadK => {
            return kname(p, instr.arg_bx() as usize, name);
        }
        // C: case OP_LOADKX: return kname(p, GETARG_Ax(p->code[pc + 1]), name);
        OpCode::LoadKx => {
            let next = p.code[(pc + 1) as usize];
            return kname(p, next.arg_ax() as usize, name);
        }
        _ => {}
    }
    None
}

/// Finds a name for a register-or-K instruction's `C` field (the key side).
/// Stores a "constant name" if possible, otherwise `"?"`.
///
/// C: `static void rname(const Proto *p, int pc, int c, const char **name)`
fn rname<'a>(p: &'a LuaProto, pc: i32, c: i32, name: &mut &'a [u8]) {
    let mut pc = pc;
    // C: const char *what = basicgetobjname(p, &pc, c, name);
    //    if (!(what && *what == 'c')) *name = "?";
    let what = basic_get_obj_name(p, &mut pc, c, name);
    if !matches!(what, Some(kind) if kind.first() == Some(&b'c')) {
        *name = b"?";
    }
}

/// Finds the name for an RK-encoded `C` operand (either a constant or a register).
///
/// C: `static void rkname(const Proto *p, int pc, Instruction i, const char **name)`
fn rkname<'a>(p: &'a LuaProto, pc: i32, instr: Instruction, name: &mut &'a [u8]) {
    // C: int c = GETARG_C(i);
    let c = instr.arg_c() as i32;
    // C: if (GETARG_k(i)) kname(p, c, name); else rname(p, pc, c, name);
    // macros.tsv: GETARG_k → i.arg_k() -> u32
    if instr.arg_k() != 0 {
        kname(p, c as usize, name);
    } else {
        rname(p, pc, c, name);
    }
}

/// Determines whether the table indexed by instruction `i` is `_ENV`.
/// Returns `"global"` if so, `"field"` otherwise.
///
/// C: `static const char *isEnv(const Proto *p, int pc, Instruction i, int isup)`
fn is_env<'a>(p: &'a LuaProto, pc: i32, instr: Instruction, isup: bool) -> &'static [u8] {
    // C: int t = GETARG_B(i);
    let t = instr.arg_b() as usize;
    let mut name: &[u8] = b"?";
    // C: if (isup) name = upvalname(p, t); else basicgetobjname(p, &pc, t, &name);
    if isup {
        name = upval_name(p, t);
    } else {
        let mut pc = pc;
        basic_get_obj_name(p, &mut pc, t as i32, &mut name);
    }
    // C: return (name && strcmp(name, LUA_ENV) == 0) ? "global" : "field";
    if name == LUA_ENV { b"global" } else { b"field" }
}

/// Extended version of `basic_get_obj_name` that also handles table accesses.
/// Returns the "kind" of name, or `None`.
///
/// C: `static const char *getobjname(const Proto *p, int lastpc, int reg, const char **name)`
fn get_obj_name<'a>(
    p: &'a LuaProto,
    lastpc: i32,
    reg: i32,
    name: &mut &'a [u8],
) -> Option<&'static [u8]> {
    let mut lastpc = lastpc;
    // C: const char *kind = basicgetobjname(p, &lastpc, reg, name);
    let kind = basic_get_obj_name(p, &mut lastpc, reg, name);
    if kind.is_some() {
        return kind;
    }

    if lastpc == -1 {
        return None;
    }

    let instr = p.code[lastpc as usize];
    let op = instr.opcode();
    match op {
        // C: case OP_GETTABUP: int k = GETARG_C(i); kname(p, k, name); return isEnv(p, lastpc, i, 1);
        OpCode::GetTabUp => {
            let k = instr.arg_c() as usize;
            kname(p, k, name);
            Some(is_env(p, lastpc, instr, true))
        }
        // C: case OP_GETTABLE: int k = GETARG_C(i); rname(p, lastpc, k, name); return isEnv(..., 0);
        OpCode::GetTable => {
            let k = instr.arg_c() as i32;
            rname(p, lastpc, k, name);
            Some(is_env(p, lastpc, instr, false))
        }
        // C: case OP_GETI: *name = "integer index"; return "field";
        OpCode::GetI => {
            *name = b"integer index";
            Some(b"field")
        }
        // C: case OP_GETFIELD: int k = GETARG_C(i); kname(p, k, name); return isEnv(..., 0);
        OpCode::GetField => {
            let k = instr.arg_c() as usize;
            kname(p, k, name);
            Some(is_env(p, lastpc, instr, false))
        }
        // C: case OP_SELF: rkname(p, lastpc, i, name); return "method";
        OpCode::Self_ => {
            rkname(p, lastpc, instr, name);
            Some(b"method")
        }
        _ => None,
    }
}

// ─── Function naming ──────────────────────────────────────────────────────────

/// Tries to derive a name for a function from the bytecode instruction that
/// called it. Returns the "kind" of call (e.g. "for iterator", "metamethod"),
/// or `None`.
///
/// C: `static const char *funcnamefromcode(lua_State *L, const Proto *p, int pc, const char **name)`
fn funcname_from_code<'a>(
    state: &LuaState,
    p: &'a LuaProto,
    pc: i32,
    name: &mut Option<Vec<u8>>,
) -> Option<&'static [u8]> {
    // C: TMS tm = (TMS)0; Instruction i = p->code[pc];
    let instr = p.code[pc as usize];
    let op = instr.opcode();

    match op {
        // C: case OP_CALL: case OP_TAILCALL: return getobjname(p, pc, GETARG_A(i), name);
        OpCode::Call | OpCode::TailCall => {
            let mut name_bytes: &[u8] = b"?";
            let kind = get_obj_name(p, pc, instr.arg_a() as i32, &mut name_bytes);
            *name = Some(name_bytes.to_vec());
            kind
        }
        // C: case OP_TFORCALL: *name = "for iterator"; return "for iterator";
        OpCode::TForCall => {
            *name = Some(b"for iterator".to_vec());
            Some(b"for iterator")
        }
        // Metamethod dispatch cases — look up tm name from GlobalState
        // C: case OP_SELF, OP_GETTABUP, OP_GETTABLE, ...: tm = TM_INDEX; break;
        OpCode::Self_ | OpCode::GetTabUp | OpCode::GetTable | OpCode::GetI | OpCode::GetField => {
            get_tm_name(state, TagMethod::Index, name)
        }
        // C: case OP_SETTABUP, ...: tm = TM_NEWINDEX; break;
        OpCode::SetTabUp | OpCode::SetTable | OpCode::SetI | OpCode::SetField => {
            get_tm_name(state, TagMethod::NewIndex, name)
        }
        // C: case OP_MMBIN, OP_MMBINI, OP_MMBINK: tm = cast(TMS, GETARG_C(i)); break;
        OpCode::MmBin | OpCode::MmBinI | OpCode::MmBinK => {
            // C: tm = cast(TMS, GETARG_C(i))
            // macros.tsv: cast(TMS, x) → x as TagMethod
            // TODO(port): TagMethod::from_u8 needs to exist
            let tm_idx = instr.arg_c() as u8;
            let tm = TagMethod::from_u8(tm_idx);
            get_tm_name(state, tm, name)
        }
        // C: case OP_UNM: tm = TM_UNM; break; ...
        OpCode::Unm => get_tm_name(state, TagMethod::Unm, name),
        OpCode::BNot => get_tm_name(state, TagMethod::BNot, name),
        OpCode::Len => get_tm_name(state, TagMethod::Len, name),
        OpCode::Concat => get_tm_name(state, TagMethod::Concat, name),
        OpCode::Eq => get_tm_name(state, TagMethod::Eq, name),
        // C: case OP_LT, OP_LTI, OP_GTI: tm = TM_LT; break;
        OpCode::Lt | OpCode::LtI | OpCode::GtI => get_tm_name(state, TagMethod::Lt, name),
        // C: case OP_LE, OP_LEI, OP_GEI: tm = TM_LE; break;
        OpCode::Le | OpCode::LeI | OpCode::GeI => get_tm_name(state, TagMethod::Le, name),
        // C: case OP_CLOSE, OP_RETURN: tm = TM_CLOSE; break;
        OpCode::Close | OpCode::Return => get_tm_name(state, TagMethod::Close, name),
        _ => None,
    }
}

/// Looks up the name for tag method `tm` from GlobalState and stores it in `*name`.
/// Returns `Some("metamethod")`.
///
/// C: `*name = getshrstr(G(L)->tmname[tm]) + 2; return "metamethod";`
/// PORT NOTE: `+2` skips the leading `__` prefix in C; here we strip it from
/// the byte slice.
fn get_tm_name(
    state: &LuaState,
    tm: TagMethod,
    name: &mut Option<Vec<u8>>,
) -> Option<&'static [u8]> {
    // C: *name = getshrstr(G(L)->tmname[tm]) + 2;  — skip "__" prefix
    // macros.tsv: getshrstr(ts) → ts.as_bytes(); G → state.global()
    // PORT NOTE: reshaped for borrowck — tm_name returns Option<GcRef<LuaString>>;
    // materialise the bytes before stripping so there is no borrow of a temporary.
    let raw_bytes: Vec<u8> = state.global()
        .tm_name(tm)
        .map(|s| s.as_bytes().to_vec())
        .unwrap_or_default();
    let stripped = raw_bytes
        .strip_prefix(b"__")
        .unwrap_or(&raw_bytes)
        .to_vec();
    *name = Some(stripped);
    Some(b"metamethod")
}

/// Tries to derive a name for a function from how it was called (`ci`).
///
/// C: `static const char *funcnamefromcall(lua_State *L, CallInfo *ci, const char **name)`
fn funcname_from_call<'a>(
    state: &'a LuaState,
    ci: &CallInfo,
    name: &mut Option<Vec<u8>>,
) -> Option<&'static [u8]> {
    // C: if (ci->callstatus & CIST_HOOKED) { *name = "?"; return "hook"; }
    if ci.callstatus & CIST_HOOKED != 0 {
        *name = Some(b"?".to_vec());
        return Some(b"hook");
    }
    // C: else if (ci->callstatus & CIST_FIN) { *name = "__gc"; return "metamethod"; }
    if ci.callstatus & CIST_FIN != 0 {
        *name = Some(b"__gc".to_vec());
        return Some(b"metamethod");
    }
    // C: else if (isLua(ci)) return funcnamefromcode(L, ci_func(ci)->p, currentpc(ci), name);
    if ci.is_lua() {
        let proto = ci_lua_proto(ci, state);
        return funcname_from_code(state, &proto, current_pc(ci), name);
    }
    None
}

// ─── Pointer-to-value tracking (varinfo for error messages) ──────────────────

/// Checks whether value at stack index `val_idx` is in the call frame `ci`'s
/// register window, and if so returns the register index (0-based).
/// Returns -1 if not found.
///
/// C: `static int instack(CallInfo *ci, const TValue *o)`
///
/// PORT NOTE: In C this compares raw pointers. In Rust we compare StackIdx
/// values. The function signature changes: instead of a `*o` pointer we take
/// the StackIdx of the value directly.
fn in_stack(ci: &CallInfo, val_idx: StackIdx, state: &LuaState) -> i32 {
    // C: StkId base = ci->func.p + 1;
    let base = StackIdx(ci.func.0 + 1);
    // C: for (pos = 0; base + pos < ci->top.p; pos++) { if (o == s2v(base + pos)) return pos; }
    // TODO(port): in C this is a pointer-identity check (`o == s2v(base+pos)`).
    // In Rust, `val_idx` IS a StackIdx; we just check whether it falls in range.
    let ci_top = ci.top;
    let mut pos = 0i32;
    let mut cur = base;
    while cur.0 < ci_top.0 {
        if cur == val_idx {
            return pos;
        }
        cur = StackIdx(cur.0 + 1);
        pos += 1;
    }
    -1
}

/// Checks whether `val_idx` is the current value of one of the upvalues in the
/// Lua closure at `ci`. If so, sets `*name` and returns `Some("upvalue")`.
///
/// C: `static const char *getupvalname(CallInfo *ci, const TValue *o, const char **name)`
///
/// PORT NOTE: In C this compares `c->upvals[i]->v.p == o` (pointer identity on
/// open upvalues or the closed slot). In Rust, open upvalues hold a StackIdx; we
/// compare that against `val_idx`. Closed upvalues cannot be identified by stack
/// position, so they are not matched here.
fn get_upval_name<'a>(
    ci: &CallInfo,
    val_idx: StackIdx,
    name: &mut &'a [u8],
    state: &'a LuaState,
) -> Option<&'static [u8]> {
    // C: LClosure *c = ci_func(ci);
    let proto = ci_lua_proto(ci, state);
    // TODO(port): actual upvalue objects require ci.lua_closure() on the LuaState;
    // this is a best-effort translation
    let lua_cl = match state.get_at(ci.func) {
        LuaValue::Function(LuaClosure::Lua(cl)) => cl.clone(),
        _ => return None,
    };
    for (i, upval_slot) in lua_cl.upvals.iter().enumerate() {
        // C: if (c->upvals[i]->v.p == o)
        let upval = upval_slot.borrow().clone();
        let state = upval.slot().clone();
        if let lua_types::UpValState::Open { idx, .. } = state {
            if idx == val_idx {
                // TODO(phase-b): the name needs to be tied to state's lifetime; using
                // a static fallback keeps the trait bounds satisfied for now.
                let _ = upval_name(&proto, i);
                *name = b"upvalue";
                return Some(b"upvalue");
            }
        }
    }
    None
}

/// Builds a human-readable "variable info" string like ` (local 'x')` or
/// ` (upvalue 'y')` to append to error messages. Returns an empty `Vec<u8>`
/// if no information is available.
///
/// C: `static const char *formatvarinfo(lua_State *L, const char *kind, const char *name)`
fn format_var_info(kind: Option<&[u8]>, name: Option<&[u8]>) -> Vec<u8> {
    // C: if (kind == NULL) return ""; else return luaO_pushfstring(L, " (%s '%s')", kind, name);
    match (kind, name) {
        (Some(k), Some(n)) => {
            let mut out = Vec::with_capacity(4 + k.len() + n.len());
            out.extend_from_slice(b" (");
            out.extend_from_slice(k);
            out.extend_from_slice(b" '");
            out.extend_from_slice(n);
            out.extend_from_slice(b"')");
            out
        }
        _ => Vec::new(),
    }
}

/// Returns a description string for the value at `val_idx` in the current call
/// frame, e.g. `" (local 'x')"` or `" (upvalue 'y')"`. Used in error messages.
///
/// C: `static const char *varinfo(lua_State *L, const TValue *o)`
fn var_info(state: &LuaState, val_idx: StackIdx) -> Vec<u8> {
    // C: CallInfo *ci = L->ci; const char *kind = NULL;
    let ci_idx = state.current_ci_idx();
    let ci = state.get_ci(ci_idx).clone();
    let mut kind: Option<&[u8]> = None;
    let mut name_owned: Vec<u8> = b"?".to_vec();

    if ci.is_lua() {
        // C: kind = getupvalname(ci, o, &name);
        let mut up_name: &[u8] = b"?";
        kind = get_upval_name(&ci, val_idx, &mut up_name, state);
        if kind.is_some() {
            name_owned = up_name.to_vec();
        } else {
            // C: int reg = instack(ci, o); if (reg >= 0) kind = getobjname(...);
            let reg = in_stack(&ci, val_idx, state);
            if reg >= 0 {
                let proto = ci_lua_proto(&ci, state);
                let mut nref: &[u8] = b"?";
                let pc = current_pc(&ci);
                let k = get_obj_name(&proto, pc, reg, &mut nref);
                kind = k;
                if kind.is_some() {
                    name_owned = nref.to_vec();
                }
            }
        }
    }
    format_var_info(kind, if kind.is_some() { Some(&name_owned) } else { None })
}

// ─── Error-raising functions ──────────────────────────────────────────────────

/// Internal helper: raises a type error with the given `extra` info string.
///
/// C: `static l_noret typeerror(lua_State *L, const TValue *o, const char *op, const char *extra)`
fn typeerror_inner(
    state: &LuaState,
    val: &LuaValue,
    op: &[u8],
    extra: &[u8],
) -> LuaError {
    let t = state.obj_type_name(val);
    let mut msg = Vec::new();
    msg.extend_from_slice(b"attempt to ");
    msg.extend_from_slice(op);
    msg.extend_from_slice(b" a ");
    msg.extend_from_slice(&t);
    msg.extend_from_slice(b" value");
    msg.extend_from_slice(extra);
    prefixed_runtime(state, msg)
}

/// Raises a type error for performing operation `op` on value `val`.
/// Includes variable-info context (e.g. "local 'x'") if available.
///
/// C: `l_noret luaG_typeerror(lua_State *L, const TValue *o, const char *op)` (LUAI_FUNC)
pub(crate) fn type_error(state: &LuaState, val: &LuaValue, val_idx: StackIdx, op: &[u8]) -> LuaError {
    // C: typeerror(L, o, op, varinfo(L, o));
    let extra = var_info(state, val_idx);
    typeerror_inner(state, val, op, &extra)
}

/// Variant of `type_error` for bytecode paths where the target isn't on the
/// active stack — OP_SETTABUP / OP_GETTABUP read directly from the closure's
/// upvalue cells, so `var_info`'s in-stack heuristic can't recover the name.
/// The caller passes a pre-formatted `(kind, name)` pair (e.g.
/// `(b"upvalue", b"a")`) used verbatim in the trailing `(kind 'name')`.
pub(crate) fn type_error_with_hint(
    state: &LuaState,
    val: &LuaValue,
    op: &[u8],
    kind: &[u8],
    name: &[u8],
) -> LuaError {
    let extra = format_var_info(Some(kind), Some(name));
    let t = obj_type_name_static(val);
    let mut msg = Vec::new();
    msg.extend_from_slice(b"attempt to ");
    msg.extend_from_slice(op);
    msg.extend_from_slice(b" a ");
    msg.extend_from_slice(t);
    msg.extend_from_slice(b" value");
    msg.extend_from_slice(&extra);
    prefixed_runtime(state, msg)
}

/// Standalone type-name accessor that does not require `&LuaState`. Used by
/// `type_error_with_hint` since callers there cannot easily thread `state`.
fn obj_type_name_static(val: &LuaValue) -> &'static [u8] {
    match val {
        LuaValue::Nil => b"nil",
        LuaValue::Bool(_) => b"boolean",
        LuaValue::Int(_) | LuaValue::Float(_) => b"number",
        LuaValue::Str(_) => b"string",
        LuaValue::Table(_) => b"table",
        LuaValue::Function(_) => b"function",
        LuaValue::UserData(_) => b"userdata",
        LuaValue::LightUserData(_) => b"light userdata",
        LuaValue::Thread(_) => b"thread",
    }
}

/// Raises a "call" type error for a non-callable `val`.
/// Prefers name from `funcnamefromcall`; falls back to `varinfo`.
///
/// C: `l_noret luaG_callerror(lua_State *L, const TValue *o)` (LUAI_FUNC)
pub(crate) fn call_error(state: &LuaState, val: &LuaValue, val_idx: StackIdx) -> LuaError {
    // C: CallInfo *ci = L->ci; const char *kind = funcnamefromcall(L, ci, &name);
    let ci_idx = state.current_ci_idx();
    let ci = state.get_ci(ci_idx).clone();
    let mut name: Option<Vec<u8>> = None;
    let kind = funcname_from_call(state, &ci, &mut name);
    let extra = if kind.is_some() {
        format_var_info(kind, name.as_deref())
    } else {
        var_info(state, val_idx)
    };
    typeerror_inner(state, val, b"call", &extra)
}

/// Raises a "bad 'for' <what>" error.
///
/// C: `l_noret luaG_forerror(lua_State *L, const TValue *o, const char *what)` (LUAI_FUNC)
pub(crate) fn for_error(state: &mut LuaState, val: &LuaValue, what: &[u8]) -> LuaError {
    let t = crate::tagmethods::obj_type_name(state, val)
        .unwrap_or_else(|_| crate::tagmethods::type_name(val.base_type()).to_vec());
    let mut msg = Vec::new();
    msg.extend_from_slice(b"bad 'for' ");
    msg.extend_from_slice(what);
    msg.extend_from_slice(b" (number expected, got ");
    msg.extend_from_slice(&t);
    msg.push(b')');
    prefixed_runtime(state, msg)
}

/// Raises a concatenation type error for the first non-coercible operand.
///
/// C: `l_noret luaG_concaterror(lua_State *L, const TValue *p1, const TValue *p2)` (LUAI_FUNC)
pub(crate) fn concat_error(
    state: &LuaState,
    p1: &LuaValue,
    p1_idx: StackIdx,
    p2: &LuaValue,
    p2_idx: StackIdx,
) -> LuaError {
    // C: if (ttisstring(p1) || cvt2str(p1)) p1 = p2;
    // macros.tsv: ttisstring → matches!(o, LuaValue::Str(_))
    // macros.tsv: cvt2str → matches!(o, LuaValue::Int(_) | LuaValue::Float(_))
    let (bad_val, bad_idx) = if matches!(p1, LuaValue::Str(_) | LuaValue::Int(_) | LuaValue::Float(_)) {
        (p2, p2_idx)
    } else {
        (p1, p1_idx)
    };
    type_error(state, bad_val, bad_idx, b"concatenate")
}

/// Raises an arithmetic type error. If `p1` is not a number, blames `p1`;
/// otherwise blames `p2`.
///
/// C: `l_noret luaG_opinterror(lua_State *L, const TValue *p1, const TValue *p2, const char *msg)` (LUAI_FUNC)
pub(crate) fn op_int_error(
    state: &LuaState,
    p1: &LuaValue,
    p1_idx: StackIdx,
    p2: &LuaValue,
    p2_idx: StackIdx,
    msg: &[u8],
) -> LuaError {
    // C: if (!ttisnumber(p1)) p2 = p1;  — first operand is wrong, blame it
    // macros.tsv: ttisnumber → matches!(o, LuaValue::Int(_) | LuaValue::Float(_))
    let (bad_val, bad_idx) = if !matches!(p1, LuaValue::Int(_) | LuaValue::Float(_)) {
        (p1, p1_idx)
    } else {
        (p2, p2_idx)
    };
    type_error(state, bad_val, bad_idx, msg)
}

/// Raises an "no integer representation" error for float→int conversion failure.
///
/// C: `l_noret luaG_tointerror(lua_State *L, const TValue *p1, const TValue *p2)` (LUAI_FUNC)
///
/// Stack indices are optional: when an operand is from a constant table or
/// an immediate, no register backs it and `var_info` has nothing to report.
pub(crate) fn to_int_error(
    state: &LuaState,
    p1: &LuaValue,
    p1_idx: Option<StackIdx>,
    _p2: &LuaValue,
    p2_idx: Option<StackIdx>,
) -> LuaError {
    let bad_idx = if p1.to_integer_no_strconv().is_none() {
        p1_idx
    } else {
        p2_idx
    };
    let extra = match bad_idx {
        Some(idx) => var_info(state, idx),
        None => Vec::new(),
    };
    let mut msg = Vec::new();
    msg.extend_from_slice(b"number");
    msg.extend_from_slice(&extra);
    msg.extend_from_slice(b" has no integer representation");
    prefixed_runtime(state, msg)
}

/// Raises an order-comparison type error for incompatible types.
///
/// C: `l_noret luaG_ordererror(lua_State *L, const TValue *p1, const TValue *p2)` (LUAI_FUNC)
pub(crate) fn order_error(state: &LuaState, p1: &LuaValue, p2: &LuaValue) -> LuaError {
    // C: const char *t1 = luaT_objtypename(L, p1); const char *t2 = luaT_objtypename(L, p2);
    // TODO(port): obj_type_name lives in crate::tagmethods
    let t1 = state.obj_type_name(p1);
    let t2 = state.obj_type_name(p2);
    // C: if (strcmp(t1, t2) == 0) luaG_runerror(L, "attempt to compare two %s values", t1);
    //    else                      luaG_runerror(L, "attempt to compare %s with %s", t1, t2);
    let msg = if t1 == t2 {
        let mut m = Vec::new();
        m.extend_from_slice(b"attempt to compare two ");
        m.extend_from_slice(&t1);
        m.extend_from_slice(b" values");
        m
    } else {
        let mut m = Vec::new();
        m.extend_from_slice(b"attempt to compare ");
        m.extend_from_slice(&t1);
        m.extend_from_slice(b" with ");
        m.extend_from_slice(&t2);
        m
    };
    prefixed_runtime(state, msg)
}

/// Prepends `src:line: ` to `msg` (as a new Lua string on the stack) and
/// returns the formatted string.
///
/// C: `const char *luaG_addinfo(lua_State *L, const char *msg, TString *src, int line)` (LUAI_FUNC)
///
/// The C signature takes `lua_State *L` because the result is pushed onto the
/// Lua stack via `luaO_pushfstring`. Our port returns `Vec<u8>` instead, so
/// the state parameter is unused — keep an optional reference for callers
/// that still pass one, but the function works without it.
pub(crate) fn add_info(
    _state: Option<&mut LuaState>,
    msg: &[u8],
    src: Option<&LuaString>,
    line: i32,
) -> Vec<u8> {
    // C: char buff[LUA_IDSIZE]; if (src) luaO_chunkid(buff, getstr(src), tsslen(src));
    //    else { buff[0] = '?'; buff[1] = '\0'; }
    let mut buff = [0u8; LUA_IDSIZE];
    if let Some(src) = src {
        // macros.tsv: getstr(ts) → ts.as_bytes(); tsslen(ts) → ts.len()
        // TODO(port): luaO_chunkid lives in crate::object
        chunk_id(&mut buff, src.as_bytes(), src.len());
    } else {
        buff[0] = b'?';
    }
    // C: return luaO_pushfstring(L, "%s:%d: %s", buff, line, msg);
    // PORT NOTE: Instead of pushing on the stack, we return the formatted Vec<u8>.
    // Callers that need the result on the stack should push it themselves.
    let src_part = buff.iter().position(|&b| b == 0).map_or(&buff[..], |n| &buff[..n]);
    let mut out = Vec::with_capacity(src_part.len() + 12 + msg.len());
    out.extend_from_slice(src_part);
    out.push(b':');
    // Write line number as decimal bytes
    let line_str = line.to_string();
    out.extend_from_slice(line_str.as_bytes());
    out.extend_from_slice(b": ");
    out.extend_from_slice(msg);
    out
}

/// Raises the value currently on top of the stack as a runtime error, invoking
/// the error handler if one is set.
///
/// C: `l_noret luaG_errormsg(lua_State *L)` (LUAI_FUNC)
pub(crate) fn error_msg(state: &mut LuaState) -> Result<(), LuaError> {
    // C: if (L->errfunc != 0) { StkId errfunc = restorestack(L, L->errfunc); ... luaD_callnoyield; }
    //    luaD_throw(L, LUA_ERRRUN);
    // macros.tsv: restorestack(L, L->errfunc) → the StackIdx stored in L->errfunc
    if state.errfunc() != 0 {
        let errfunc_idx = StackIdx(state.errfunc() as u32);
        debug_assert!(
            matches!(state.get_at(errfunc_idx), LuaValue::Function(_)),
            "error_msg: error handler is not a function"
        );
        // C: setobjs2s(L, L->top.p, L->top.p - 1);  — move argument up
        let arg = state.get_at(state.top_idx() - 1).clone();
        state.push(arg);
        // C: setobjs2s(L, L->top.p - 1, errfunc);  — push function below arg
        let func = state.get_at(errfunc_idx).clone();
        state.set_at(state.top_idx() - 2, func);
        // C: L->top.p++;  — assume EXTRA_STACK
        // PORT NOTE: the extra stack slot is guaranteed; push() handles it
        // C: luaD_callnoyield(L, L->top.p - 2, 1);
        // TODO(port): luaD_callnoyield lives in crate::do_; call it once available
        state.call_no_yield(state.top_idx() - 2, 1)?;
    }
    // C: luaD_throw(L, LUA_ERRRUN)
    // macros.tsv: luaD_throw → return Err(LuaError::with_status(errcode))
    // error_sites.tsv: luaD_throw(L, LUA_ERRRUN) → return Err(LuaError::with_status(LuaStatus::ErrRun))
    Err(runtime_from_top(state))
}

/// Formats and raises a runtime error with printf-style arguments. Prepends
/// source:line information for Lua frames.
///
/// C: `l_noret luaG_runerror(lua_State *L, const char *fmt, ...)` (LUAI_FUNC)
pub(crate) fn run_error(state: &mut LuaState, msg: Vec<u8>) -> Result<(), LuaError> {
    // C: CallInfo *ci = L->ci; luaC_checkGC(L);
    // macros.tsv: luaC_checkGC → state.gc().check_step()
    state.gc().check_step();

    let ci_idx = state.current_ci_idx();
    let ci = state.get_ci(ci_idx).clone();

    // C: if (isLua(ci)) { luaG_addinfo(L, msg, ci_func(ci)->p->source, getcurrentline(ci)); ... }
    let final_msg = if ci.is_lua() {
        // TODO(port): access proto.source via ci_lua_proto
        let line = get_current_line(&ci, state);
        let proto = ci_lua_proto(&ci, state);
        let src = proto.source_string();
        add_info(Some(state), &msg, src.map(|s| &**s), line)
    } else {
        msg
    };

    // C: luaG_errormsg(L)
    // Push the final message as a string, then call error_msg.
    // TODO(port): state.intern_str or state.push_string to get the string on stack
    let str_val = state.new_string(&final_msg)?;
    state.push(LuaValue::Str(str_val));
    error_msg(state)
}

// ─── Line change detection ────────────────────────────────────────────────────

/// Checks whether instruction `newpc` is on a different source line than `oldpc`.
///
/// C: `static int changedline(const Proto *p, int oldpc, int newpc)`
fn changed_line(p: &LuaProto, oldpc: i32, newpc: i32) -> bool {
    // C: if (p->lineinfo == NULL) return 0;
    if p.lineinfo.is_empty() {
        return false;
    }

    // C: if (newpc - oldpc < MAXIWTHABS / 2) — not too far apart, try incremental walk
    if newpc - oldpc < MAX_IWTH_ABS / 2 {
        let mut delta: i32 = 0;
        let mut pc = oldpc;
        loop {
            pc += 1;
            if pc as usize >= p.lineinfo.len() {
                break;
            }
            let lineinfo = p.lineinfo[pc as usize];
            // C: if (lineinfo == ABSLINEINFO) break; — fall through to explicit computation
            if lineinfo == ABS_LINE_INFO {
                break;
            }
            delta += lineinfo as i32;
            if pc == newpc {
                return delta != 0;
            }
        }
    }
    // C: return (luaG_getfuncline(p, oldpc) != luaG_getfuncline(p, newpc))
    get_func_line(p, oldpc) != get_func_line(p, newpc)
}

// ─── Trace execution hooks ────────────────────────────────────────────────────

/// Called at the start of a Lua function. Fires the call hook if appropriate.
/// Returns 1 to keep the trap on, 0 to turn it off.
///
/// C: `int luaG_tracecall(lua_State *L)` (LUAI_FUNC)
pub(crate) fn trace_call(state: &mut LuaState) -> Result<i32, LuaError> {
    let ci_idx = state.current_ci_idx();
    let ci = state.get_ci(ci_idx).clone();
    // C: Proto *p = ci_func(ci)->p; ci->u.l.trap = 1;
    state.get_ci_mut(ci_idx).set_trap(true);
    let proto = ci_lua_proto(&ci, state);

    // C: if (ci->u.l.savedpc == p->code)  — first instruction, not resuming?
    if ci.saved_pc() == 0 {
        if proto.is_vararg {
            // C: if (p->is_vararg) return 0;  — hooks start at VARARGPREP
            return Ok(0);
        } else if ci.callstatus & CIST_HOOKYIELD == 0 {
            // C: else if (!(ci->callstatus & CIST_HOOKYIELD)) luaD_hookcall(L, ci);
            // TODO(port): luaD_hookcall lives in crate::do_
            state.hook_call(ci_idx)?;
        }
    }
    Ok(1)
}

/// Called before each VM instruction when debugging is active.
/// Fires line and count hooks as appropriate.
/// Returns 1 to keep trap on, 0 to turn it off.
///
/// C: `int luaG_traceexec(lua_State *L, const Instruction *pc)` (LUAI_FUNC)
///
/// PORT NOTE: The C `pc` parameter is a pointer to the instruction array.
/// In Rust, `pc` is the 0-based index of the NEXT instruction (same semantic as
/// `savedpc`). After incrementing for reference (`pc++` in C), it equals
/// the next-instruction index.
pub(crate) fn trace_exec(state: &mut LuaState, pc: u32) -> Result<i32, LuaError> {
    let ci_idx = state.current_ci_idx();
    let ci = state.get_ci(ci_idx).clone();

    // C: lu_byte mask = L->hookmask;
    let mask = state.hook_mask();

    // C: if (!(mask & (LUA_MASKLINE | LUA_MASKCOUNT)))
    if mask & (LUA_MASKLINE | LUA_MASKCOUNT) == 0 {
        // C: ci->u.l.trap = 0; return 0;
        state.get_ci_mut(ci_idx).set_trap(false);
        return Ok(0);
    }

    // C: pc++;  — reference is always next instruction
    // C: ci->u.l.savedpc = pc;  — save 'pc'
    let next_pc = pc + 1;
    state.get_ci_mut(ci_idx).set_saved_pc(next_pc);

    // C: counthook = (mask & LUA_MASKCOUNT) && (--L->hookcount == 0)
    let counthook = if mask & LUA_MASKCOUNT != 0 {
        let hc = state.hook_count() - 1;
        state.set_hook_count(hc);
        hc == 0
    } else {
        false
    };

    if counthook {
        // C: resethookcount(L)
        state.reset_hook_count();
    } else if mask & LUA_MASKLINE == 0 {
        // C: return 1;  — no line hook and count != 0; nothing to do
        return Ok(1);
    }

    // C: if (ci->callstatus & CIST_HOOKYIELD) { ci->callstatus &= ~CIST_HOOKYIELD; return 1; }
    if ci.callstatus & CIST_HOOKYIELD != 0 {
        state.get_ci_mut(ci_idx).callstatus &= !CIST_HOOKYIELD;
        return Ok(1);
    }

    // C: if (!isIT(*(ci->u.l.savedpc - 1))) L->top.p = ci->top.p;
    // macros.tsv: isIT(i) → i.is_in_top()
    // PORT NOTE: savedpc - 1 is the current instruction (now at index next_pc - 1 = pc).
    let cur_instr = state.get_proto_instr(ci_idx, pc as u32);
    if !cur_instr.is_in_top() {
        // C: L->top.p = ci->top.p  — correct top
        let ci_top = state.get_ci(ci_idx).top;
        state.set_top(ci_top);
    }

    if counthook {
        // C: luaD_hook(L, LUA_HOOKCOUNT, -1, 0, 0)
        // TODO(port): luaD_hook lives in crate::do_
        state.call_hook_event(LUA_HOOKCOUNT, -1)?;
    }

    if mask & LUA_MASKLINE != 0 {
        let proto = ci_lua_proto(&ci, state);
        // C: int oldpc = (L->oldpc < p->sizecode) ? L->oldpc : 0;
        let oldpc = if state.old_pc() < proto.code.len() as u32 {
            state.old_pc() as i32
        } else {
            0
        };
        // C: int npci = pcRel(pc, p);  — next_pc already represents the next instr
        // current instruction is pc (0-based); pcRel gives current = next - 1
        let npci = next_pc as i32 - 1;

        // C: if (npci <= oldpc || changedline(p, oldpc, npci))
        if npci <= oldpc || changed_line(&proto, oldpc, npci) {
            let newline = get_func_line(&proto, npci);
            // C: luaD_hook(L, LUA_HOOKLINE, newline, 0, 0)
            // TODO(port): luaD_hook lives in crate::do_
            state.call_hook_event(LUA_HOOKLINE, newline)?;
        }
        // C: L->oldpc = npci
        state.set_old_pc(npci as u32);
    }

    // C: if (L->status == LUA_YIELD) { if (counthook) L->hookcount = 1; ... luaD_throw(LUA_YIELD); }
    if state.status() == lua_types::status::LuaStatus::Yield {
        if counthook {
            state.set_hook_count(1);
        }
        // C: ci->callstatus |= CIST_HOOKYIELD
        state.get_ci_mut(ci_idx).callstatus |= CIST_HOOKYIELD;
        // C: luaD_throw(L, LUA_YIELD)
        // error_sites.tsv: luaD_throw(L, LUA_YIELD) → return Err(LuaError::with_status(LuaStatus::Yield))
        return Err(LuaError::Yield);
    }

    Ok(1)
}

// ─── File-local helpers referenced above but not directly translated ──────────

/// Gets the source line name (short, truncated) for error messages.
///
/// C: `void luaO_chunkid(char *out, const char *source, size_t srclen)` — delegates
/// to the real impl in `crate::object`. Handles `=name`, `@filename`, and
/// `[string "..."]` formatting so error prefixes are concise rather than dumping
/// the entire source verbatim.
fn chunk_id(out: &mut [u8; LUA_IDSIZE], source: &[u8], _srclen: usize) {
    out.fill(0);
    let n = crate::object::chunk_id(&mut out[..], source);
    if n < out.len() {
        out[n] = 0;
    }
}

/// Gets the local variable name for register `reg+1` at instruction `pc` in `p`.
/// Returns `None` if not found (variable is not live at `pc`).
///
/// C: `luaF_getlocalname(const Proto *p, int n, int pc)` from `lfunc.c`.
fn get_local_name(p: &LuaProto, n: i32, pc: i32) -> Option<&[u8]> {
    crate::func::get_local_name(p, n, pc)
}

/// Gets the n-th local name from a Lua closure (for non-active function query).
/// C: `luaF_getlocalname(cl->p, n, 0)`.
fn get_local_name_from_closure(cl: &LuaClosureLua, n: i32, pc: i32) -> Option<&[u8]> {
    get_local_name(&cl.proto, n, pc)
}

/// Retrieves the LuaProto for the Lua closure at `ci.func` from the stack.
///
/// C: `ci_func(ci)->p`  — the proto pointer of the Lua closure for this frame.
/// macros.tsv: ci_func → ci.lua_closure() returning &GcRef<LuaClosure::Lua>
///
/// PORT NOTE: The C version returns a raw pointer and is a macro. Here we
/// navigate through the LuaState stack. Returns a reference with the
/// lifetime of the proto inside the GcRef (Rc), which must remain valid.
///
/// TODO(port): This returns a cloned Rc's inner reference; Phase B must verify
/// lifetimes are correct once all types are wired.
/// PORT NOTE: reshaped for borrowck — returns `GcRef<LuaProto>` (Rc clone) instead
/// of `&'a LuaProto` to avoid returning a reference to a temporary `LuaValue`
/// produced by `get_at`. Callers deref through `GcRef<T>: Deref<Target=T>`.
fn ci_lua_proto(ci: &CallInfo, state: &LuaState) -> GcRef<LuaProto> {
    match state.get_at(ci.func) {
        LuaValue::Function(LuaClosure::Lua(cl)) => cl.proto.clone(),
        _ => panic!("ci_lua_proto: call frame does not hold a Lua closure"),
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        src/ldebug.c  (962 lines, 30 functions)
//   target_crate:  lua-vm
//   confidence:    medium
//   todos:         44
//   port_notes:    15
//   unsafe_blocks: 0
//   notes:         Logic faithful to C; cross-crate imports (luaF_*, luaT_*,
//                  luaD_*, luaO_chunkid, opcode accessors) are stubbed with
//                  TODO(port) markers. LuaState accessor methods (call_stack_mut,
//                  get_ci, set_trap, saved_pc, hook_mask, etc.) are called as if
//                  defined in state.rs — Phase B must implement them. The
//                  pointer-identity comparisons in instack/getupvalname are
//                  translated to StackIdx comparisons (a structural change).
//                  `lua_gethook` returns a bool instead of a fn pointer because
//                  Box<dyn FnMut> cannot be returned by value without restructuring.
//                  rustc check: zero real syntax errors; all 67 diagnostics are
//                  expected name-resolution errors (E0432/E0433/E0425/E0282).
// ──────────────────────────────────────────────────────────────────────────────
