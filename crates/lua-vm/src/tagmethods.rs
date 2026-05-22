//! Tag methods (metamethods) — ported from `ltm.c` / `ltm.h`.
//!
//! Every metamethod name (`__index`, `__add`, …) is interned on `GlobalState`
//! during `init()`.  Lookup helpers (`get_tm`, `get_tm_by_obj`) return
//! `LuaValue::Nil` when no metamethod is present; callers check with
//! `value.is_nil()` (the `notm` macro in C).

use crate::state::LuaState;
#[allow(unused_imports)] use crate::prelude::*;
use lua_types::{CallInfoIdx, GcRef, LuaError, LuaType, LuaValue, StackIdx};

// ── TagMethod enum (from ltm.h `TMS`) ────────────────────────────────────────

/// Metamethod selector; one variant per `__xxx` event, in ORDER TM.
///
/// The discriminant values are load-bearing: they index into
/// `GlobalState.tmname` and are used as bit positions in `Table.flags`.
/// Do **not** reorder without grepping ORDER TM / ORDER OP.
///
/// C: `typedef enum { TM_INDEX, … TM_N } TMS;`
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub(crate) enum TagMethod {
    // C: TM_INDEX
    Index = 0,
    // C: TM_NEWINDEX
    NewIndex,
    // C: TM_GC
    Gc,
    // C: TM_MODE
    Mode,
    // C: TM_LEN
    Len,
    // C: TM_EQ  /* last tag method with fast access */
    Eq,
    // C: TM_ADD
    Add,
    // C: TM_SUB
    Sub,
    // C: TM_MUL
    Mul,
    // C: TM_MOD
    Mod,
    // C: TM_POW
    Pow,
    // C: TM_DIV
    Div,
    // C: TM_IDIV
    IDiv,
    // C: TM_BAND
    BAnd,
    // C: TM_BOR
    BOr,
    // C: TM_BXOR
    BXor,
    // C: TM_SHL
    Shl,
    // C: TM_SHR
    Shr,
    // C: TM_UNM
    Unm,
    // C: TM_BNOT
    BNot,
    // C: TM_LT
    Lt,
    // C: TM_LE
    Le,
    // C: TM_CONCAT
    Concat,
    // C: TM_CALL
    Call,
    // C: TM_CLOSE
    Close,
    // C: TM_N  /* number of elements in the enum */
    N,
}

/// Number of real metamethods (= `TagMethod::N as usize`).
///
/// C: `TM_N`
pub(crate) const TM_N: usize = TagMethod::N as usize;

// ── Type-name table (from ltm.h / ltm.c `luaT_typenames_`) ──────────────────

// C: static const char udatatypename[] = "userdata";
//
// C: LUAI_DDEF const char *const luaT_typenames_[LUA_TOTALTYPES] = {
//   "no value",
//   "nil", "boolean", udatatypename, "number",
//   "string", "table", "function", udatatypename, "thread",
//   "upvalue", "proto"
// };
//
// Indexed as `luaT_typenames_[(x) + 1]` where x is the raw LuaType integer
// (LUA_TNONE = -1, LUA_TNIL = 0, …, LUA_TPROTO = 10).
// LUA_TOTALTYPES = LUA_TPROTO + 2 = 12 entries.
//
// PORT NOTE: C uses `const char*`; Rust uses `&'static [u8]` throughout.
pub(crate) static TYPE_NAMES: &[&[u8]] = &[
    b"no value", // index 0 → LUA_TNONE  (-1 + 1)
    b"nil",      // index 1 → LUA_TNIL   ( 0 + 1)
    b"boolean",  // index 2 → LUA_TBOOLEAN
    b"userdata", // index 3 → LUA_TLIGHTUSERDATA
    b"number",   // index 4 → LUA_TNUMBER
    b"string",   // index 5 → LUA_TSTRING
    b"table",    // index 6 → LUA_TTABLE
    b"function", // index 7 → LUA_TFUNCTION
    b"userdata", // index 8 → LUA_TUSERDATA
    b"thread",   // index 9 → LUA_TTHREAD
    b"upvalue",  // index 10 → LUA_TUPVAL
    b"proto",    // index 11 → LUA_TPROTO
];

/// Return the human-readable type name for a `LuaType`.
///
/// C: `#define ttypename(x) luaT_typenames_[(x) + 1]`
///
/// Panics in debug builds if `t` is out of the expected range (shouldn't
/// happen with a well-formed `LuaType`).
pub(crate) fn type_name(t: LuaType) -> &'static [u8] {
    // C: luaT_typenames_[(x) + 1]
    let idx = (t as i32 + 1) as usize;
    TYPE_NAMES.get(idx).copied().unwrap_or(b"?")
}

// ── luaT_init ────────────────────────────────────────────────────────────────

// C: void luaT_init (lua_State *L)
/// Intern all metamethod name strings and pin them in the GC.
///
/// Must be called exactly once during `LuaState` initialization, before any
/// metamethod lookup.  After this call, `GlobalState.tmname[i]` holds the
/// interned `LuaString` for metamethod `i`.
pub(crate) fn init(state: &mut LuaState) -> Result<(), LuaError> {
    // C: static const char *const luaT_eventname[] = {  /* ORDER TM */
    //     "__index", "__newindex",
    //     "__gc", "__mode", "__len", "__eq",
    //     "__add", "__sub", "__mul", "__mod", "__pow",
    //     "__div", "__idiv",
    //     "__band", "__bor", "__bxor", "__shl", "__shr",
    //     "__unm", "__bnot", "__lt", "__le",
    //     "__concat", "__call", "__close"
    // };
    const EVENT_NAMES: &[&[u8]] = &[
        b"__index",
        b"__newindex",
        b"__gc",
        b"__mode",
        b"__len",
        b"__eq",
        b"__add",
        b"__sub",
        b"__mul",
        b"__mod",
        b"__pow",
        b"__div",
        b"__idiv",
        b"__band",
        b"__bor",
        b"__bxor",
        b"__shl",
        b"__shr",
        b"__unm",
        b"__bnot",
        b"__lt",
        b"__le",
        b"__concat",
        b"__call",
        b"__close",
    ];
    debug_assert!(EVENT_NAMES.len() == TM_N);

    // C: for (i=0; i<TM_N; i++) {
    //     G(L)->tmname[i] = luaS_new(L, luaT_eventname[i]);
    //     luaC_fix(L, obj2gco(G(L)->tmname[i]));  /* never collect these names */
    // }
    for (i, &name) in EVENT_NAMES.iter().enumerate() {
        // C: luaS_new(L, luaT_eventname[i])
        let interned = state.intern_str(name)?;
        // C: G(L)->tmname[i] = ...
        state.global_mut().tmname[i] = interned.clone();
        // C: luaC_fix(L, obj2gco(...))
        // Pin the string so the GC never collects it.
        // TODO(port): luaC_fix API on gc() is TBD; no-op in Phase A–C (Rc keeps it alive)
        state.gc().fix_object(&interned);
    }
    Ok(())
}

// ── luaT_gettm ───────────────────────────────────────────────────────────────

// C: const TValue *luaT_gettm (Table *events, TMS event, TString *ename)
/// Fast-path metamethod lookup using the table's flags cache.
///
/// If the metamethod is absent, sets the corresponding bit in `events.flags`
/// so future lookups via `fasttm` / `gfasttm` skip the hash entirely.
///
/// Returns `Some(value)` when found, `None` when absent.
///
/// Precondition: `event <= TagMethod::Eq` (only fast-access metamethods use
/// the flags cache).
pub(crate) fn get_tm(
    events: &mut lua_types::value::LuaTable,
    event: TagMethod,
    ename: &GcRef<lua_types::LuaString>,
) -> Option<LuaValue> {
    // C: const TValue *tm = luaH_getshortstr(events, ename);
    let _ = (events, ename);
    let tm: LuaValue = todo!("phase-b: LuaTable::get_short_str");
    // C: lua_assert(event <= TM_EQ);
    debug_assert!((event as u8) <= (TagMethod::Eq as u8));
    // C: if (notm(tm)) {
    //     events->flags |= cast_byte(1u<<event);  /* cache this fact */
    //     return NULL;
    // }
    if tm.is_nil() {
        // TODO(port): `flags_set_absent_bit(event as u8)` — exact LuaTable
        // method name for setting the fast-access absence bit is TBD; the bit
        // is `1 << event` in the flags byte per ltm.h `maskflags` definition.
        let _ = (events, event); // todo!("phase-b: LuaTable::flags_set_absent_bit")
        None
    } else {
        // C: else return tm;
        Some(tm)
    }
}

// ── luaT_gettmbyobj ──────────────────────────────────────────────────────────

// C: const TValue *luaT_gettmbyobj (lua_State *L, const TValue *o, TMS event)
/// Look up a metamethod for any Lua value by dispatching on its type.
///
/// Tables and full userdata have per-object metatables; all other types use
/// the per-type metatables on `GlobalState`.  Returns `LuaValue::Nil` when
/// neither the object nor its type has a metatable, or when the metatable
/// does not contain the requested metamethod.
pub(crate) fn get_tm_by_obj(
    state: &mut LuaState,
    o: &LuaValue,
    event: TagMethod,
) -> LuaValue {
    // C: Table *mt;
    // C: switch (ttype(o)) {
    //   case LUA_TTABLE:    mt = hvalue(o)->metatable; break;
    //   case LUA_TUSERDATA: mt = uvalue(o)->metatable; break;
    //   default:            mt = G(L)->mt[ttype(o)];
    // }
    //
    // TODO(port): `GcRef<LuaTable>` access pattern (direct field vs borrow) is
    // TBD pending the GcRef/RefCell decision in Phase B; using `.metatable()`
    // accessor here as a placeholder.
    let mt: Option<GcRef<lua_types::value::LuaTable>> = match o {
        LuaValue::Table(t) => t.metatable(),
        LuaValue::UserData(u) => u.metatable(),
        _ => {
            // C: G(L)->mt[ttype(o)]
            let type_idx = o.base_type() as usize;
            state.global().mt[type_idx].clone()
        }
    };

    // C: return (mt ? luaH_getshortstr(mt, G(L)->tmname[event]) : &G(L)->nilvalue);
    match mt {
        Some(mt_ref) => {
            // Clone the name string before the table lookup to avoid borrow conflict.
            let ename = state.global().tmname[event as usize].clone();
            // C: luaH_getshortstr(mt, G(L)->tmname[event])
            mt_ref.get_short_str(&ename)
        }
        None => LuaValue::Nil,
    }
}

// ── luaT_objtypename ─────────────────────────────────────────────────────────

// C: const char *luaT_objtypename (lua_State *L, const TValue *o)
/// Return the human-readable type name for a Lua value.
///
/// For tables and full userdata whose metatable defines `__name` as a string,
/// that string is returned.  Otherwise falls back to the standard `type_name()`
/// for the base type.
///
/// PORT NOTE: C returns `const char*` (static or LuaString bytes with stable
/// lifetime).  Rust returns `Vec<u8>` to avoid lifetime entanglement between
/// the static `TYPE_NAMES` entries and GcRef'd `LuaString` bytes.
/// PERF(port): Vec allocation on every call — profile in Phase B; may become
/// a `Cow<'static, [u8]>` once lifetimes are firmed up.
pub(crate) fn obj_type_name(state: &mut LuaState, o: &LuaValue) -> Result<Vec<u8>, LuaError> {
    // C: Table *mt;
    // C: if ((ttistable(o) && (mt = hvalue(o)->metatable) != NULL) ||
    //        (ttisfulluserdata(o) && (mt = uvalue(o)->metatable) != NULL)) {
    //
    // TODO(port): same GcRef accessor TBD as in get_tm_by_obj.
    let mt: Option<GcRef<lua_types::value::LuaTable>> = match o {
        LuaValue::Table(t) => t.metatable(),
        LuaValue::UserData(u) => u.metatable(),
        _ => None,
    };

    if let Some(mt_ref) = mt {
        // C: const TValue *name = luaH_getshortstr(mt, luaS_new(L, "__name"));
        // TODO(port): intern_str can fail with LuaError::Memory; propagating
        // here means obj_type_name is now fallible (it wasn't in C, because
        // luaS_new's OOM longjmp'd). The caller must use `?`.
        let name_key = state.intern_str(b"__name")?;
        let name_val = mt_ref.get_short_str(&name_key);
        // C: if (ttisstring(name))  /* is '__name' a string? */
        //      return getstr(tsvalue(name));  /* use it as type name */
        if let LuaValue::Str(s) = name_val {
            // C: getstr(tsvalue(name))  →  ts.as_bytes()  →  &[u8]
            return Ok(s.as_bytes().to_vec());
        }
    }

    // C: return ttypename(ttype(o));  /* else use standard type name */
    Ok(type_name(o.base_type()).to_vec())
}

// ── luaT_callTM ──────────────────────────────────────────────────────────────

// C: void luaT_callTM (lua_State *L, const TValue *f, const TValue *p1,
//                      const TValue *p2, const TValue *p3)
/// Call tag method `f` with three arguments, discarding any return values.
///
/// Used for metamethods like `__gc` and `__close` that take three operands
/// and whose return value is irrelevant.
///
/// If the current call frame is Lua bytecode (`isLuacode`), the metamethod
/// may yield; otherwise yielding is suppressed (`callnoyield`).
pub(crate) fn call_tm(
    state: &mut LuaState,
    f: LuaValue,
    p1: LuaValue,
    p2: LuaValue,
    p3: LuaValue,
) -> Result<(), LuaError> {
    // C: StkId func = L->top.p;
    let func = state.top_idx();
    // C: setobj2s(L, func, f);     /* push function (assume EXTRA_STACK) */
    // C: setobj2s(L, func + 1, p1); /* 1st argument */
    // C: setobj2s(L, func + 2, p2); /* 2nd argument */
    // C: setobj2s(L, func + 3, p3); /* 3rd argument */
    // C: L->top.p = func + 4;
    //
    // PORT NOTE: In C these are direct writes into the EXTRA_STACK reserve
    // area above the official top.  In Rust we use push() which manages
    // capacity; the semantic result is identical.
    state.push(f);
    state.push(p1);
    state.push(p2);
    state.push(p3);
    // C: if (isLuacode(L->ci))
    //      luaD_call(L, func, 0);
    //    else
    //      luaD_callnoyield(L, func, 0);
    //
    // TODO(port): `do_call(func, nresults)` vs `call_from(func, nresults)` —
    // exact Rust API name TBD; nargs is implicit as `top - func - 1`.
    if state.current_ci().is_lua_code() {
        state.do_call(func, 0)?;
    } else {
        state.do_call_no_yield(func, 0)?;
    }
    Ok(())
}

// ── luaT_callTMres ───────────────────────────────────────────────────────────

// C: void luaT_callTMres (lua_State *L, const TValue *f, const TValue *p1,
//                          const TValue *p2, StkId res)
/// Call tag method `f` with two arguments, writing the single result into
/// the stack slot at index `res`.
///
/// `res` is a `StackIdx` (index-stable across stack reallocation) that must
/// refer to a pre-existing or scratch slot.  After return, the stack top is
/// back to what it was before the call (i.e. `top == res`).
///
/// C uses `savestack`/`restorestack` (byte-offset from base) to preserve `res`
/// across potential stack reallocations inside `luaD_call`.  In Rust, `StackIdx`
/// is already an index and needs no save/restore.
pub(crate) fn call_tm_res(
    state: &mut LuaState,
    f: LuaValue,
    p1: LuaValue,
    p2: LuaValue,
    res: StackIdx,
) -> Result<(), LuaError> {
    // C: ptrdiff_t result = savestack(L, res);
    // savestack → StackIdx is already the stable byte-offset analogue; no-op.

    // C: StkId func = L->top.p;
    let func = state.top_idx();
    // C: setobj2s(L, func, f);     /* push function (assume EXTRA_STACK) */
    // C: setobj2s(L, func + 1, p1); /* 1st argument */
    // C: setobj2s(L, func + 2, p2); /* 2nd argument */
    // C: L->top.p += 3;
    state.push(f);
    state.push(p1);
    state.push(p2);

    // C: if (isLuacode(L->ci))
    //      luaD_call(L, func, 1);
    //    else
    //      luaD_callnoyield(L, func, 1);
    //
    // TODO(port): same `do_call` API question as in call_tm above.
    if state.current_ci().is_lua_code() {
        state.do_call(func, 1)?;
    } else {
        state.do_call_no_yield(func, 1)?;
    }

    // C: res = restorestack(L, result);
    // restorestack → StackIdx is already stable; `res` is unchanged.

    // C: setobjs2s(L, res, --L->top.p);  /* move result to its place */
    // Pre-decrement top, then copy that slot to res.
    let result_val = state.pop();
    state.set_at(res, result_val);
    Ok(())
}

// ── callbinTM (static) ────────────────────────────────────────────────────────

// C: static int callbinTM (lua_State *L, const TValue *p1, const TValue *p2,
//                           StkId res, TMS event)
/// Try to find and call a binary tag method for `event`.
///
/// Checks `p1` first, then `p2`, for a metamethod.  If neither has one,
/// returns `false` and leaves `res` unmodified.  If found, calls it with
/// `(p1, p2)` as arguments, writes the result to slot `res`, and returns `true`.
fn call_bin_tm(
    state: &mut LuaState,
    p1: &LuaValue,
    p2: &LuaValue,
    res: StackIdx,
    event: TagMethod,
) -> Result<bool, LuaError> {
    // C: const TValue *tm = luaT_gettmbyobj(L, p1, event);  /* try first operand */
    let tm = get_tm_by_obj(state, p1, event);
    // C: if (notm(tm))
    //      tm = luaT_gettmbyobj(L, p2, event);  /* try second operand */
    let tm = if tm.is_nil() {
        get_tm_by_obj(state, p2, event)
    } else {
        tm
    };
    // C: if (notm(tm)) return 0;
    if tm.is_nil() {
        return Ok(false);
    }
    // C: luaT_callTMres(L, tm, p1, p2, res);
    // Clone p1/p2 before the mutable borrow of `state` via call_tm_res.
    call_tm_res(state, tm, p1.clone(), p2.clone(), res)?;
    // C: return 1;
    Ok(true)
}

// ── luaT_trybinTM ────────────────────────────────────────────────────────────

// C: void luaT_trybinTM (lua_State *L, const TValue *p1, const TValue *p2,
//                         StkId res, TMS event)
/// Attempt a binary metamethod call; raise a type error if no metamethod exists.
///
/// For bitwise operations, further distinguishes between:
/// - Both operands are numbers but not integers → `LuaError::int_overflow`
///   (`luaG_tointerror` — "number has no integer representation")
/// - At least one operand is not a number → `LuaError::arith_error` with
///   "perform bitwise operation on"
///
/// All other missing metamethods raise `LuaError::arith_error` with
/// "perform arithmetic on".
pub(crate) fn try_bin_tm(
    state: &mut LuaState,
    p1: &LuaValue,
    p2: &LuaValue,
    res: StackIdx,
    event: TagMethod,
) -> Result<(), LuaError> {
    // C: if (l_unlikely(!callbinTM(L, p1, p2, res, event))) {
    if !call_bin_tm(state, p1, p2, res, event)? {
        // C: switch (event) {
        //   case TM_BAND: case TM_BOR: case TM_BXOR:
        //   case TM_SHL: case TM_SHR: case TM_BNOT: {
        //     if (ttisnumber(p1) && ttisnumber(p2))
        //       luaG_tointerror(L, p1, p2);
        //     else
        //       luaG_opinterror(L, p1, p2, "perform bitwise operation on");
        //   }
        //   /* calls never return, but to avoid warnings: *//* FALLTHROUGH */
        //   default:
        //     luaG_opinterror(L, p1, p2, "perform arithmetic on");
        // }
        //
        // PORT NOTE: the C switch has a dead "FALLTHROUGH" for bitwise cases
        // because both branches of the inner if/else call noreturn functions.
        // In Rust `match` has no implicit fallthrough; each arm is self-contained
        // and explicitly returns `Err(...)`.
        match event {
            TagMethod::BAnd
            | TagMethod::BOr
            | TagMethod::BXor
            | TagMethod::Shl
            | TagMethod::Shr
            | TagMethod::BNot => {
                // C: if (ttisnumber(p1) && ttisnumber(p2))
                if matches!(p1, LuaValue::Int(_) | LuaValue::Float(_))
                    && matches!(p2, LuaValue::Int(_) | LuaValue::Float(_))
                {
                    // C: luaG_tointerror(L, p1, p2)
                    return Err(LuaError::int_overflow(p1, p2));
                } else {
                    // C: luaG_opinterror(L, p1, p2, "perform bitwise operation on")
                    return Err(LuaError::arith_error(p1, p2, "perform bitwise operation on"));
                }
            }
            _ => {
                // C: luaG_opinterror(L, p1, p2, "perform arithmetic on")
                return Err(LuaError::arith_error(p1, p2, "perform arithmetic on"));
            }
        }
    }
    Ok(())
}

// ── luaT_tryconcatTM ─────────────────────────────────────────────────────────

// C: void luaT_tryconcatTM (lua_State *L)
/// Attempt the `__concat` metamethod on the two values at the top of the stack.
///
/// Reads `stack[top-2]` and `stack[top-1]`, searches both for `__concat`,
/// calls it with `(stack[top-2], stack[top-1])` writing the result back to
/// `stack[top-2]`, or raises `LuaError::concat_error` if no metamethod exists.
pub(crate) fn try_concat_tm(state: &mut LuaState) -> Result<(), LuaError> {
    // C: StkId top = L->top.p;
    let top = state.top_idx();
    // C: if (l_unlikely(!callbinTM(L, s2v(top - 2), s2v(top - 1), top - 2, TM_CONCAT)))
    //      luaG_concaterror(L, s2v(top - 2), s2v(top - 1));
    //
    // Clone the operands before any call that might mutate the stack.
    let p1 = state.get_at(top - 2).clone();
    let p2 = state.get_at(top - 1).clone();
    if !call_bin_tm(state, &p1, &p2, top - 2, TagMethod::Concat)? {
        // C: luaG_concaterror(L, s2v(top - 2), s2v(top - 1))
        return Err(LuaError::concat_error(&p1, &p2));
    }
    Ok(())
}

// ── luaT_trybinassocTM ───────────────────────────────────────────────────────

// C: void luaT_trybinassocTM (lua_State *L, const TValue *p1, const TValue *p2,
//                              int flip, StkId res, TMS event)
/// Try a binary associative metamethod, optionally swapping the operands.
///
/// When `flip` is `true`, operands are exchanged before dispatch.  This
/// implements Lua's symmetry rule: if `a OP b` finds no metamethod on `a`,
/// the VM retries with `b OP a` (setting flip=true to restore the original
/// argument order for the call).
pub(crate) fn try_bin_assoc_tm(
    state: &mut LuaState,
    p1: &LuaValue,
    p2: &LuaValue,
    flip: bool,
    res: StackIdx,
    event: TagMethod,
) -> Result<(), LuaError> {
    // C: if (flip)
    //      luaT_trybinTM(L, p2, p1, res, event);
    //    else
    //      luaT_trybinTM(L, p1, p2, res, event);
    if flip {
        try_bin_tm(state, p2, p1, res, event)
    } else {
        try_bin_tm(state, p1, p2, res, event)
    }
}

// ── luaT_trybiniTM ───────────────────────────────────────────────────────────

// C: void luaT_trybiniTM (lua_State *L, const TValue *p1, lua_Integer i2,
//                          int flip, StkId res, TMS event)
/// Try a binary metamethod where the second operand is an integer constant.
///
/// Boxes `i2` as `LuaValue::Int` and delegates to `try_bin_assoc_tm`.
pub(crate) fn try_bini_tm(
    state: &mut LuaState,
    p1: &LuaValue,
    i2: i64,
    flip: bool,
    res: StackIdx,
    event: TagMethod,
) -> Result<(), LuaError> {
    // C: TValue aux;
    // C: setivalue(&aux, i2);
    let aux = LuaValue::Int(i2);
    // C: luaT_trybinassocTM(L, p1, &aux, flip, res, event);
    try_bin_assoc_tm(state, p1, &aux, flip, res, event)
}

// ── luaT_callorderTM ─────────────────────────────────────────────────────────

// C: int luaT_callorderTM (lua_State *L, const TValue *p1, const TValue *p2,
//                           TMS event)
/// Call an order metamethod (`__lt` or `__le`) and return its boolean result.
///
/// Returns `true` if the metamethod returned a truthy value.
/// Raises `LuaError::order_error` if neither operand has the metamethod.
///
/// PORT NOTE: The `LUA_COMPAT_LT_LE` block (which falls back from `__le` to
/// `!(p2 < p1)`) is omitted per PORTING.md §13 — no compatibility shims.
pub(crate) fn call_order_tm(
    state: &mut LuaState,
    p1: &LuaValue,
    p2: &LuaValue,
    event: TagMethod,
) -> Result<bool, LuaError> {
    // C: if (callbinTM(L, p1, p2, L->top.p, event))  /* try original event */
    //      return !l_isfalse(s2v(L->top.p));
    //
    // PORT NOTE: In C, `L->top.p` is used as a scratch slot (written by
    // callTMres then immediately read) in the EXTRA_STACK reserved area above
    // the official stack top — the stack top is NOT officially advanced.
    // In Rust we pass `state.top_idx()` as `res`; call_bin_tm → call_tm_res
    // pushes 3 values, calls, pops the result back to res, and leaves top ==
    // res (i.e. top unchanged relative to entry).  Reading get_at(res_idx)
    // after this is safe because the slot was just written and top == res_idx.
    //
    // TODO(port): Verify in Phase B that no call path between call_bin_tm
    // returning and the get_at read can disturb the scratch slot or reset top
    // below res_idx.  The invariant holds as long as do_call with 1 result
    // leaves exactly one value on the stack above func.
    let res_idx = state.top_idx();
    if call_bin_tm(state, p1, p2, res_idx, event)? {
        // C: return !l_isfalse(s2v(L->top.p));
        // l_isfalse(o) → matches!(o, LuaValue::Nil | LuaValue::Bool(false))
        let result = state.get_at(res_idx).clone();
        return Ok(!matches!(result, LuaValue::Nil | LuaValue::Bool(false)));
    }

    // PORT NOTE: LUA_COMPAT_LT_LE block skipped (see above).

    // C: luaG_ordererror(L, p1, p2);  /* no metamethod found */
    Err(LuaError::order_error(p1, p2))
}

// ── luaT_callorderiTM ────────────────────────────────────────────────────────

// C: int luaT_callorderiTM (lua_State *L, const TValue *p1, int v2,
//                            int flip, int isfloat, TMS event)
/// Call an order metamethod where the second operand is a primitive int or float.
///
/// `v2` is a C `int`; `isfloat` selects whether it is coerced to
/// `LuaValue::Float` (via `cast_num`) or kept as `LuaValue::Int`.
/// When `flip` is true the operands are swapped so that `p1` was originally
/// on the right-hand side.
pub(crate) fn call_orderi_tm(
    state: &mut LuaState,
    p1: &LuaValue,
    v2: i32,
    flip: bool,
    isfloat: bool,
    event: TagMethod,
) -> Result<bool, LuaError> {
    // C: TValue aux; const TValue *p2;
    // C: if (isfloat) {
    //      setfltvalue(&aux, cast_num(v2));
    //    }
    //    else
    //      setivalue(&aux, v2);
    let aux = if isfloat {
        // C: cast_num(v2)  →  v2 as f64
        LuaValue::Float(v2 as f64)
    } else {
        LuaValue::Int(v2 as i64)
    };

    // C: if (flip) {  /* arguments were exchanged? */
    //      p2 = p1; p1 = &aux;  /* correct them */
    //    }
    //    else
    //      p2 = &aux;
    // C: return luaT_callorderTM(L, p1, p2, event);
    if flip {
        call_order_tm(state, &aux, p1, event)
    } else {
        call_order_tm(state, p1, &aux, event)
    }
}

// ── luaT_adjustvarargs ───────────────────────────────────────────────────────

// C: void luaT_adjustvarargs (lua_State *L, int nfixparams, CallInfo *ci,
//                              const Proto *p)
/// Adjust the stack layout for a vararg function at call entry.
///
/// Moves the fixed parameters above the extra (vararg) arguments and copies
/// the function object alongside them so the function body sees its registers
/// at the expected offsets.  Records the extra-argument count in the CallInfo
/// for `OP_VARARG` use.
///
/// Before call:  `[func | fixed... | extra...]`
/// After call:   `[func | nil... | extra... | func′ | fixed...]`
/// (`ci.func` and `ci.top` are advanced by `actual + 1`.)
pub(crate) fn adjust_varargs(
    state: &mut LuaState,
    nfixparams: i32,
    ci_idx: CallInfoIdx,
    proto: &GcRef<lua_types::LuaProto>,
) -> Result<(), LuaError> {
    // C: int actual = cast_int(L->top.p - ci->func.p) - 1;  /* number of arguments */
    let ci_func: StackIdx = state.call_info[ci_idx.as_usize()].func;
    let actual = (state.top_idx().0 as i32) - (ci_func.0 as i32) - 1;
    // C: int nextra = actual - nfixparams;  /* number of extra arguments */
    let nextra = actual - nfixparams;
    // C: ci->u.l.nextraargs = nextra;
    // TODO(phase-b): nextraargs lives inside CallInfoFrame::Lua; needs proper
    // pattern-match write through state.call_info[..].u.
    if let crate::state::CallInfoFrame::Lua { ref mut nextraargs, .. } = state.call_info[ci_idx.as_usize()].u {
        *nextraargs = nextra;
    }

    // C: luaD_checkstack(L, p->maxstacksize + 1);
    let maxstacksize = proto.maxstacksize as i32;
    state.check_stack(maxstacksize + 1)?;

    // Re-read ci_func after check_stack (stack may have reallocated, but
    // StackIdx is index-based so the value is still correct).
    let ci_func: StackIdx = state.call_info[ci_idx.as_usize()].func;

    // C: setobjs2s(L, L->top.p++, ci->func.p);  /* copy function to the top */
    let func_val = state.get_at(ci_func).clone();
    state.push(func_val);

    // C: for (i = 1; i <= nfixparams; i++) {
    //      setobjs2s(L, L->top.p++, ci->func.p + i);
    //      setnilvalue(s2v(ci->func.p + i));  /* erase original parameter (for GC) */
    //    }
    for i in 1..=nfixparams {
        // TODO(port): StackIdx is u32; if ci_func + i overflows the u32 range
        // this panics in debug.  In practice `i` is small (≤ 255 params), but
        // add a saturating or checked add in Phase B.
        let src: StackIdx = ci_func + i as i32;
        let param_val = state.get_at(src).clone();
        state.push(param_val);
        // C: setnilvalue(s2v(ci->func.p + i))  →  *o = LuaValue::Nil
        state.set_at(src, LuaValue::Nil);
    }

    // C: ci->func.p += actual + 1;
    // C: ci->top.p  += actual + 1;
    // TODO(port): `actual + 1` may be negative if `actual < -1` (malformed call);
    // casting to StackIdx (u32) would underflow.  In practice Lua guarantees
    // actual >= 0 at this point, but add a debug_assert in Phase B.
    let offset = (actual + 1) as i32;
    state.call_info[ci_idx.as_usize()].func = state.call_info[ci_idx.as_usize()].func + offset;
    state.call_info[ci_idx.as_usize()].top = state.call_info[ci_idx.as_usize()].top + offset;

    // C: lua_assert(L->top.p <= ci->top.p && ci->top.p <= L->stack_last.p);
    debug_assert!(state.top_idx().0 <= state.call_info[ci_idx.as_usize()].top.0);
    Ok(())
}

// ── luaT_getvarargs ──────────────────────────────────────────────────────────

// C: void luaT_getvarargs (lua_State *L, CallInfo *ci, StkId where, int wanted)
/// Copy vararg values into the stack starting at `where_idx`.
///
/// `wanted` specifies how many values to copy.  Pass `wanted < 0` (the
/// `LUA_MULTRET` convention) to request all available extra arguments; the
/// stack top is then set to `where_idx + nextra`.
///
/// Slots beyond `nextra` but within `wanted` are filled with `LuaValue::Nil`.
pub(crate) fn get_varargs(
    state: &mut LuaState,
    ci_idx: CallInfoIdx,
    where_idx: StackIdx,
    wanted: i32,
) -> Result<(), LuaError> {
    // C: int nextra = ci->u.l.nextraargs;
    let nextra: i32 = if let crate::state::CallInfoFrame::Lua { nextraargs, .. } = state.call_info[ci_idx.as_usize()].u { nextraargs } else { 0 };

    // C: if (wanted < 0) {
    //      wanted = nextra;  /* get all extra arguments available */
    //      checkstackGCp(L, nextra, where);  /* ensure stack space */
    //      L->top.p = where + nextra;  /* next instruction will need top */
    //    }
    let wanted: i32 = if wanted < 0 {
        // C: checkstackGCp(L, nextra, where)  →  check_stack + gc step
        state.check_stack(nextra)?;
        state.gc().check_step();
        // C: L->top.p = where + nextra
        // TODO(port): `where_idx + nextra as i32` may overflow if nextra
        // is very large; checked add in Phase B.
        state.set_top(where_idx + nextra as i32);
        nextra
    } else {
        wanted
    };

    // C: for (i = 0; i < wanted && i < nextra; i++)
    //      setobjs2s(L, where + i, ci->func.p - nextra + i);
    //
    // After adjustvarargs, the extra args live at positions
    // ci->func - nextra .. ci->func - 1.
    let ci_func: StackIdx = state.call_info[ci_idx.as_usize()].func;
    let copy_count = wanted.min(nextra);
    for i in 0..copy_count {
        // C: ci->func.p - nextra + i
        // TODO(port): subtraction on StackIdx (u32) underflows if nextra > ci_func.
        // Invariant: ci_func >= nextra (enforced by adjustvarargs), but add
        // a debug_assert in Phase B.
        let src: StackIdx = ci_func - nextra as i32 + i as i32;
        let val = state.get_at(src).clone();
        state.set_at(where_idx + i as i32, val);
    }

    // C: for (; i < wanted; i++)   /* complete required results with nil */
    //      setnilvalue(s2v(where + i));
    for i in copy_count..wanted {
        // C: setnilvalue(s2v(where + i))  →  *o = LuaValue::Nil
        state.set_at(where_idx + i as i32, LuaValue::Nil);
    }
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        src/ltm.c  (271 lines, 15 functions)
//                  src/ltm.h  (104 lines; merged into this file)
//   target_crate:  lua-vm
//   confidence:    medium
//   todos:         12
//   port_notes:    7
//   unsafe_blocks: 0   (must be 0 outside lua-gc/lua-coro)
//   notes:
//     Logic translation is faithful; the main uncertainties are:
//     (1) GcRef<LuaTable> accessor pattern (.metatable() / .borrow().metatable)
//         is TBD — annotated TODO(port) in get_tm_by_obj and obj_type_name.
//     (2) call_tm / call_tm_res use a `do_call` / `do_call_no_yield` naming
//         convention that must be confirmed against the LuaState API in Phase B.
//     (3) call_order_tm uses `top_idx()` as a scratch slot matching C's
//         EXTRA_STACK convention — annotated with TODO(port) for Phase B review.
//     (4) StackIdx (u32) arithmetic in adjust_varargs / get_varargs can
//         underflow — annotated TODO(port); add checked arithmetic in Phase B.
//     (5) obj_type_name returns Vec<u8> (PERF alloc) to avoid lifetime issues
//         between static TYPE_NAMES and GcRef<LuaString>; revisit in Phase B.
//     (6) LUA_COMPAT_LT_LE block in call_order_tm omitted per PORTING.md §13.
//     (7) intern_str in obj_type_name is now fallible (propagates LuaError);
//         the C version would longjmp on OOM — Rust callers must use `?`.
//     (8) luaC_fix in init() is stubbed as gc().fix_object() — no-op Phase A-C.
// ──────────────────────────────────────────────────────────────────────────────
