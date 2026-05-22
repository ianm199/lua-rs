//! Base library — Lua's built-in functions (`print`, `type`, `pairs`, `pcall`, …).
//!
//! Translated from: `reference/lua-5.4.7/src/lbaselib.c` (549 lines, 32 functions)
//! Target crate: `lua-stdlib`

// TODO(port): LuaState and related types live in lua-vm; imports resolved in Phase B.
use lua_types::{
    closure::LuaClosure,
    error::LuaError,
    value::LuaValue,
    LuaType,
    LuaStatus,
    arith::ArithOp,
    gc::GcRef,
};
use crate::state_stub::{LuaState, LuaStateStubExt as _, lua_CFunction, upvalue_index, CompareOp, LuaDebug};

// ── Module-level constants ────────────────────────────────────────────────────

/// ASCII whitespace characters used by `b_str2int` for strspn-style skipping.
/// C: `#define SPACECHARS " \f\n\r\t\v"`
const SPACECHARS: &[u8] = b" \x0c\n\r\t\x0b";

/// Reserved stack slot used by `generic_reader` to anchor the current chunk
/// string so it is not collected while `lua_load` is running.
/// C: `#define RESERVEDSLOT 5`
const RESERVED_SLOT: i32 = 5;

/// Lua version string pushed as `_VERSION` in the global table.
/// C: `LUA_VERSION` from `lua.h`
const LUA_VERSION_STR: &[u8] = b"Lua 5.4";

/// Name of the global environment table stored as a global itself.
/// C: `LUA_GNAME` from `lua.h`
const LUA_GNAME: &[u8] = b"_G";

/// Sentinel indicating "all return values" for call/pcall helpers.
/// C: `LUA_MULTRET = -1`
const LUA_MULTRET: i32 = -1;

// ── GC operation codes ────────────────────────────────────────────────────────

/// Identifies a GC control operation passed to the `collectgarbage` built-in.
/// Mirrors the `LUA_GC*` integer constants from `lua.h`.
/// TODO(port): define as a proper type in lua-types once the GC API is finalised.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GcOp {
    Stop       = 0,
    Restart    = 1,
    Collect    = 2,
    Count      = 3,
    CountB     = 4,
    Step       = 5,
    SetPause   = 6,
    SetStepMul = 7,
    IsRunning  = 9,
    Gen        = 10,
    Inc        = 11,
}

// ── LuaState forward declaration ─────────────────────────────────────────────

// LuaState is provided by crate::state_stub.

// ── Type alias for standard Lua-callable functions ────────────────────────────

/// Rust equivalent of `lua_CFunction`: a bare function that receives the
/// interpreter state and returns a count of pushed results.
/// C: `typedef int (*lua_CFunction)(lua_State *L)`
pub(crate) type LuaLibFn = fn(&mut LuaState) -> Result<usize, LuaError>;

// ── Helper: push_mode ─────────────────────────────────────────────────────────

/// Push the GC mode string ("incremental" or "generational") onto the stack,
/// or push `nil` (fail) when `oldmode == -1` (invalid call inside a finalizer).
///
/// C: `static int pushmode(lua_State *L, int oldmode)`
fn push_mode(state: &mut LuaState, oldmode: i32) -> Result<usize, LuaError> {
    if oldmode == -1 {
        // C: luaL_pushfail(L);
        state.push(LuaValue::Nil);
    } else {
        // C: lua_pushstring(L, (oldmode == LUA_GCINC) ? "incremental" : "generational");
        let s: &[u8] = if oldmode == GcOp::Inc as i32 {
            b"incremental"
        } else {
            b"generational"
        };
        state.push_string(s)?;
    }
    Ok(1)
}

// ── Helper: finish_pcall ──────────────────────────────────────────────────────

/// Shared result-adjustment logic for `pcall` and `xpcall`.
///
/// On success: returns the count of values already on the stack minus `extra`
/// skipped sentinel values.  On failure: replaces whatever is on the stack
/// with `[false, error_message]` and returns 2.
///
/// C: `static int finishpcall(lua_State *L, int status, lua_KContext extra)`
fn finish_pcall(state: &mut LuaState, ok: bool, extra: i32) -> Result<usize, LuaError> {
    // C: if (l_unlikely(status != LUA_OK && status != LUA_YIELD))
    if !ok {
        // C: lua_pushboolean(L, 0); lua_pushvalue(L, -2); return 2;
        state.push(LuaValue::Bool(false));
        state.push_copy(-2)?;
        return Ok(2);
    }
    // C: return lua_gettop(L) - (int)extra;
    Ok((state.top() as i32 - extra) as usize)
}

// ── Helper: b_str2int ─────────────────────────────────────────────────────────

/// Parse an integer in an arbitrary base from the byte slice `s`.
///
/// Returns `Some((consumed, value))` on success, where `consumed` is the number
/// of bytes from the start of `s` that were processed (leading and trailing
/// ASCII whitespace included).  Returns `None` when the slice contains no valid
/// numeral in `base`.
///
/// The caller checks `consumed == s.len()` to verify the whole string was used.
///
/// C: `static const char *b_str2int(const char *s, int base, lua_Integer *pn)`
fn b_str2int(s: &[u8], base: u32) -> Option<(usize, i64)> {
    let mut pos = 0usize;
    // C: s += strspn(s, SPACECHARS); /* skip initial spaces */
    while pos < s.len() && SPACECHARS.contains(&s[pos]) {
        pos += 1;
    }
    // C: if (*s == '-') { s++; neg = 1; } else if (*s == '+') s++;
    let neg = if pos < s.len() && s[pos] == b'-' {
        pos += 1;
        true
    } else {
        if pos < s.len() && s[pos] == b'+' {
            pos += 1;
        }
        false
    };
    // C: if (!isalnum((unsigned char)*s)) return NULL; /* no digit? */
    if pos >= s.len() || !s[pos].is_ascii_alphanumeric() {
        return None;
    }
    let mut n: u64 = 0u64;
    // C: do { ... } while (isalnum((unsigned char)*s));
    loop {
        let byte = s[pos];
        let digit = if byte.is_ascii_digit() {
            (byte - b'0') as u32
        } else {
            // C: (toupper((unsigned char)*s) - 'A') + 10
            (byte.to_ascii_uppercase() - b'A') as u32 + 10
        };
        // C: if (digit >= base) return NULL; /* invalid numeral */
        if digit >= base {
            return None;
        }
        // C: n = n * base + digit;
        n = n.wrapping_mul(base as u64).wrapping_add(digit as u64);
        pos += 1;
        if pos >= s.len() || !s[pos].is_ascii_alphanumeric() {
            break;
        }
    }
    // C: s += strspn(s, SPACECHARS); /* skip trailing spaces */
    while pos < s.len() && SPACECHARS.contains(&s[pos]) {
        pos += 1;
    }
    // C: *pn = (lua_Integer)((neg) ? (0u - n) : n);
    let value: i64 = if neg {
        0u64.wrapping_sub(n) as i64
    } else {
        n as i64
    };
    Some((pos, value))
}

// ── Helper: load_aux ──────────────────────────────────────────────────────────

/// Shared post-load logic for `load` and `loadfile`.
///
/// On success (status_ok == true): optionally installs an environment upvalue,
/// then returns 1 (the chunk function is on the stack).
/// On failure: pushes nil then moves it before the error message, returns 2.
///
/// C: `static int load_aux(lua_State *L, int status, int envidx)`
fn load_aux(state: &mut LuaState, status_ok: bool, envidx: i32) -> Result<usize, LuaError> {
    if status_ok {
        // C: if (envidx != 0) { lua_pushvalue(L, envidx); if (!lua_setupvalue(L, -2, 1)) lua_pop(L, 1); }
        if envidx != 0 {
            state.push_copy(envidx)?;
            if state.set_upvalue(-2, 1)?.is_none() {
                state.pop_n(1);
            }
        }
        Ok(1)
    } else {
        // C: luaL_pushfail(L); lua_insert(L, -2); return 2;
        state.push(LuaValue::Nil);
        state.insert(-2);
        Ok(2)
    }
}

// ── print ─────────────────────────────────────────────────────────────────────

/// Converts each argument to a string with `tostring()` semantics, separates
/// them with tabs, writes them to standard output, and finishes with a newline.
///
/// C: `static int luaB_print(lua_State *L)`
pub(crate) fn print_fn(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: int n = lua_gettop(L);
    let n = state.top();
    for i in 1..=n {
        // C: const char *s = luaL_tolstring(L, i, &l);
        // luaL_tolstring converts via tostring() metamethod, pushes result,
        // returns a pointer. In Rust we get a GcRef and use its bytes.
        // TODO(port): to_display_string method needs implementing on LuaState.
        let display_ref = state.to_display_string(i)?;
        // C: if (i > 1) lua_writestring("\t", 1);
        if i > 1 {
            // TODO(port): I/O should go through the state's output abstraction.
            state.write_output(b"\t")?;
        }
        // C: lua_writestring(s, l);
        let bytes = display_ref.clone();
        state.write_output(&bytes)?;
        // C: lua_pop(L, 1);  /* pop result from luaL_tolstring */
        state.pop_n(1);
    }
    // C: lua_writeline();
    state.write_output(b"\n")?;
    Ok(0)
}

// ── warn ──────────────────────────────────────────────────────────────────────

/// Validates that every argument is a string, then forwards them as a
/// multi-part warning message via the state's warning hook.
///
/// C: `static int luaB_warn(lua_State *L)`
pub(crate) fn warn_fn(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: int n = lua_gettop(L);
    let n = state.top();
    // C: luaL_checkstring(L, 1);  /* at least one argument */
    state.check_arg_string(1)?;
    // C: for (i = 2; i <= n; i++) luaL_checkstring(L, i);
    for i in 2..=n {
        state.check_arg_string(i)?;
    }
    // C: for (i = 1; i < n; i++) lua_warning(L, lua_tostring(L, i), 1);
    for i in 1..n {
        // Clone bytes before further mutation to avoid borrow conflict.
        // PORTING.md §8: "No &LuaValue across a stack-mutating call."
        let s: Vec<u8> = state
            .to_lua_string_bytes(i)
            .map(|b| b.to_vec())
            .unwrap_or_default();
        // continue = true (1) — more parts follow
        state.warning(&s, true)?;
    }
    // C: lua_warning(L, lua_tostring(L, n), 0);  /* close warning */
    let s: Vec<u8> = state
        .to_lua_string_bytes(n)
        .map(|b| b.to_vec())
        .unwrap_or_default();
    state.warning(&s, false)?;
    Ok(0)
}

// ── tonumber ──────────────────────────────────────────────────────────────────

/// Converts a value to a number, optionally in a given numeric base (2–36).
///
/// C: `static int luaB_tonumber(lua_State *L)`
pub(crate) fn tonumber_fn(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: if (lua_isnoneornil(L, 2))  /* standard conversion? */
    if matches!(state.type_at(2), LuaType::None | LuaType::Nil) {
        // C: if (lua_type(L, 1) == LUA_TNUMBER) { lua_settop(L, 1); return 1; }
        if state.type_at(1) == LuaType::Number {
            lua_vm::api::set_top(state, 1)?;
            return Ok(1);
        }
        // C: const char *s = lua_tolstring(L, 1, &l);
        // C: if (s != NULL && lua_stringtonumber(L, s) == l + 1) return 1;
        // lua_stringtonumber returns bytes consumed including the NUL terminator,
        // so success iff consumed == string_length + 1.
        if let Some(len) = state.to_lua_string_len(1) {
            if let Some(consumed) = state.string_to_number(1) {
                if consumed == len + 1 {
                    return Ok(1);
                }
            }
        }
        // C: luaL_checkany(L, 1);  /* (but there must be some parameter) */
        state.check_arg_any(1)?;
    } else {
        // C: lua_Integer base = luaL_checkinteger(L, 2);
        let base = state.check_arg_integer(2)?;
        // C: luaL_checktype(L, 1, LUA_TSTRING);  /* no numbers as strings */
        state.check_arg_type(1, LuaType::String)?;
        // Clone before further state ops (PORTING.md §8).
        let bytes: Vec<u8> = state
            .to_lua_string_bytes(1)
            .map(|b| b.to_vec())
            .unwrap_or_default();
        // C: luaL_argcheck(L, 2 <= base && base <= 36, 2, "base out of range");
        if !(2..=36).contains(&base) {
            return Err(LuaError::arg_error(2, "base out of range"));
        }
        // C: if (b_str2int(s, (int)base, &n) == s + l) { lua_pushinteger(L, n); return 1; }
        if let Some((consumed, n)) = b_str2int(&bytes, base as u32) {
            if consumed == bytes.len() {
                state.push(LuaValue::Int(n));
                return Ok(1);
            }
        }
    }
    // C: luaL_pushfail(L);  /* not a number */
    state.push(LuaValue::Nil);
    Ok(1)
}

// ── error ─────────────────────────────────────────────────────────────────────

/// Raises the value at stack[1] as a Lua error, optionally prepending
/// source-location information for string errors when `level > 0`.
///
/// C: `static int luaB_error(lua_State *L)`
pub(crate) fn error_fn(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: int level = (int)luaL_optinteger(L, 2, 1);
    let level = state.opt_arg_integer(2, 1)? as i32;
    // C: lua_settop(L, 1);
    lua_vm::api::set_top(state, 1)?;
    // C: if (lua_type(L, 1) == LUA_TSTRING && level > 0)
    if state.type_at(1) == LuaType::String && level > 0 {
        // C: luaL_where(L, level); lua_pushvalue(L, 1); lua_concat(L, 2);
        state.push_where(level)?;
        state.push_copy(1)?;
        state.concat(2)?;
    }
    // C: return lua_error(L);
    Err(LuaError::from_value(state.pop()))
}

// ── getmetatable ──────────────────────────────────────────────────────────────

/// Returns the metatable of the first argument, or the `__metatable` field of
/// the metatable if that field exists (protecting the raw metatable).
///
/// C: `static int luaB_getmetatable(lua_State *L)`
pub(crate) fn getmetatable_fn(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: luaL_checkany(L, 1);
    state.check_arg_any(1)?;
    // C: if (!lua_getmetatable(L, 1)) { lua_pushnil(L); return 1; }
    if !state.get_metatable(1)? {
        state.push(LuaValue::Nil);
        return Ok(1);
    }
    // C: luaL_getmetafield(L, 1, "__metatable");
    // Returns LuaType::Nil if metatable has no __metatable; otherwise pushes it.
    state.get_metafield(1, b"__metatable")?;
    Ok(1)
}

// ── setmetatable ──────────────────────────────────────────────────────────────

/// Sets the metatable of the table at argument 1 to the value at argument 2
/// (nil clears it).  Raises an error if the current metatable is protected via
/// `__metatable`.
///
/// C: `static int luaB_setmetatable(lua_State *L)`
pub(crate) fn setmetatable_fn(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: int t = lua_type(L, 2);
    let t = state.type_at(2);
    // C: luaL_checktype(L, 1, LUA_TTABLE);
    state.check_arg_type(1, LuaType::Table)?;
    // C: luaL_argexpected(L, t == LUA_TNIL || t == LUA_TTABLE, 2, "nil or table");
    if !(t == LuaType::Nil || t == LuaType::Table) {
        let got = state.value_at(2);
        return Err(LuaError::type_arg_error(2, "nil or table", &got));
    }
    // C: if (l_unlikely(luaL_getmetafield(L, 1, "__metatable") != LUA_TNIL))
    if state.get_metafield(1, b"__metatable")? != LuaType::Nil {
        // C: return luaL_error(L, "cannot change a protected metatable");
        return Err(LuaError::runtime(format_args!(
            "cannot change a protected metatable"
        )));
    }
    // C: lua_settop(L, 2); lua_setmetatable(L, 1);
    lua_vm::api::set_top(state, 2)?;
    state.set_metatable(1)?;
    Ok(1)
}

// ── rawequal ──────────────────────────────────────────────────────────────────

/// Raw equality check (no metamethods).
///
/// C: `static int luaB_rawequal(lua_State *L)`
pub(crate) fn rawequal_fn(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: luaL_checkany(L, 1); luaL_checkany(L, 2);
    state.check_arg_any(1)?;
    state.check_arg_any(2)?;
    // C: lua_pushboolean(L, lua_rawequal(L, 1, 2));
    let eq = state.raw_equal(1, 2)?;
    state.push(LuaValue::Bool(eq));
    Ok(1)
}

// ── rawlen ────────────────────────────────────────────────────────────────────

/// Raw length (#) without metamethods; accepts tables and strings only.
///
/// C: `static int luaB_rawlen(lua_State *L)`
pub(crate) fn rawlen_fn(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: int t = lua_type(L, 1);
    let t = state.type_at(1);
    // C: luaL_argexpected(L, t == LUA_TTABLE || t == LUA_TSTRING, 1, "table or string");
    if !(t == LuaType::Table || t == LuaType::String) {
        let got = state.value_at(1);
        return Err(LuaError::type_arg_error(1, "table or string", &got));
    }
    // C: lua_pushinteger(L, lua_rawlen(L, 1));
    let len = state.raw_len(1);
    state.push(LuaValue::Int(len));
    Ok(1)
}

// ── rawget ────────────────────────────────────────────────────────────────────

/// Raw table read (no metamethods).
///
/// C: `static int luaB_rawget(lua_State *L)`
pub(crate) fn rawget_fn(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: luaL_checktype(L, 1, LUA_TTABLE); luaL_checkany(L, 2);
    state.check_arg_type(1, LuaType::Table)?;
    state.check_arg_any(2)?;
    // C: lua_settop(L, 2); lua_rawget(L, 1);
    lua_vm::api::set_top(state, 2)?;
    state.raw_get(1)?;
    Ok(1)
}

// ── rawset ────────────────────────────────────────────────────────────────────

/// Raw table write (no metamethods).
///
/// C: `static int luaB_rawset(lua_State *L)`
pub(crate) fn rawset_fn(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: luaL_checktype(L, 1, LUA_TTABLE); luaL_checkany(L, 2); luaL_checkany(L, 3);
    state.check_arg_type(1, LuaType::Table)?;
    state.check_arg_any(2)?;
    state.check_arg_any(3)?;
    // C: lua_settop(L, 3); lua_rawset(L, 1);
    lua_vm::api::set_top(state, 3)?;
    state.raw_set(1)?;
    Ok(1)
}

// ── collectgarbage ────────────────────────────────────────────────────────────

/// Expose GC control to Lua scripts.  The first argument selects the operation;
/// subsequent arguments are operation-specific parameters.
///
/// C: `static int luaB_collectgarbage(lua_State *L)`
/// C: `#define checkvalres(res) { if (res == -1) break; }`
///
/// PORT NOTE: C's `checkvalres(x)` macro breaks out of the `switch` to the
/// trailing `luaL_pushfail` when `x == -1` (called inside a finalizer).
/// In Rust we model this with an explicit early-return to the pushfail path
/// using a boolean flag, avoiding labeled blocks.
pub(crate) fn collectgarbage_fn(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: static const char *const opts[] = {"stop","restart",...,NULL};
    static OPTS: &[&[u8]] = &[
        b"stop", b"restart", b"collect",
        b"count", b"step", b"setpause", b"setstepmul",
        b"isrunning", b"generational", b"incremental",
    ];
    // C: static const int optsnum[] = {LUA_GCSTOP, LUA_GCRESTART, ...};
    static OPTS_NUM: &[GcOp] = &[
        GcOp::Stop, GcOp::Restart, GcOp::Collect,
        GcOp::Count, GcOp::Step, GcOp::SetPause, GcOp::SetStepMul,
        GcOp::IsRunning, GcOp::Gen, GcOp::Inc,
    ];
    // C: int o = optsnum[luaL_checkoption(L, 1, "collect", opts)];
    let idx = state.check_arg_option(1, Some(b"collect"), OPTS)?;
    let op = OPTS_NUM[idx];

    // Each arm either returns early on success, or evaluates to `false`
    // (meaning checkvalres fired — fall through to pushfail).
    let valid: bool = match op {
        GcOp::Count => {
            // C: int k = lua_gc(L, o); int b = lua_gc(L, LUA_GCCOUNTB); checkvalres(k);
            // C: lua_pushnumber(L, (lua_Number)k + ((lua_Number)b/1024));
            // TODO(port): gc_count / gc_count_b are stubs in Phase A.
            let k = state.gc_count()?;
            let b = state.gc_count_b()?;
            if k == -1 {
                false
            } else {
                state.push(LuaValue::Float(k as f64 + b as f64 / 1024.0));
                return Ok(1);
            }
        }
        GcOp::Step => {
            // C: int step = (int)luaL_optinteger(L, 2, 0); int res = lua_gc(L, o, step);
            // C: checkvalres(res); lua_pushboolean(L, res);
            let step = state.opt_arg_integer(2, 0)? as i32;
            // TODO(port): gc_step is a stub in Phase A.
            let res = state.gc_step(step)?;
            if res == -1 {
                false
            } else {
                state.push(LuaValue::Bool(res != 0));
                return Ok(1);
            }
        }
        GcOp::SetPause | GcOp::SetStepMul => {
            // C: int p = (int)luaL_optinteger(L, 2, 0); int previous = lua_gc(L, o, p);
            // C: checkvalres(previous); lua_pushinteger(L, previous);
            let p = state.opt_arg_integer(2, 0)? as i32;
            // TODO(port): gc_set_param is a stub in Phase A.
            let previous = state.gc_set_param(op as i32, p)?;
            if previous == -1 {
                false
            } else {
                state.push(LuaValue::Int(previous as i64));
                return Ok(1);
            }
        }
        GcOp::IsRunning => {
            let res = state.gc_is_running()?;
            state.push(LuaValue::Bool(res));
            return Ok(1);
        }
        GcOp::Gen => {
            // C: int minormul = luaL_optinteger(L, 2, 0);
            // C: int majormul = luaL_optinteger(L, 3, 0);
            // C: return pushmode(L, lua_gc(L, o, minormul, majormul));
            let minormul = state.opt_arg_integer(2, 0)? as i32;
            let majormul = state.opt_arg_integer(3, 0)? as i32;
            // TODO(port): gc_gen is a stub in Phase A.
            let oldmode = state.gc_gen(minormul, majormul)?;
            return push_mode(state, oldmode);
        }
        GcOp::Inc => {
            // C: int pause = ...; int stepmul = ...; int stepsize = ...;
            // C: return pushmode(L, lua_gc(L, o, pause, stepmul, stepsize));
            let pause    = state.opt_arg_integer(2, 0)? as i32;
            let stepmul  = state.opt_arg_integer(3, 0)? as i32;
            let stepsize = state.opt_arg_integer(4, 0)? as i32;
            // TODO(port): gc_inc is a stub in Phase A.
            let oldmode = state.gc_inc(pause, stepmul, stepsize)?;
            return push_mode(state, oldmode);
        }
        _ => {
            // C: default: int res = lua_gc(L, o); checkvalres(res); lua_pushinteger(L, res);
            // TODO(port): gc_control_simple is a stub in Phase A.
            let res = state.gc_control_simple(op as i32)?;
            if res == -1 {
                false
            } else {
                state.push(LuaValue::Int(res as i64));
                return Ok(1);
            }
        }
    };
    debug_assert!(!valid, "valid arms return early; reaching here means checkvalres fired");
    // C: luaL_pushfail(L);  /* invalid call (inside a finalizer) */
    state.push(LuaValue::Nil);
    Ok(1)
}

// ── type ──────────────────────────────────────────────────────────────────────

/// Returns the type name of its argument as a string.
///
/// C: `static int luaB_type(lua_State *L)`
pub(crate) fn type_fn(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: int t = lua_type(L, 1);
    let t = state.type_at(1);
    // C: luaL_argcheck(L, t != LUA_TNONE, 1, "value expected");
    if t == LuaType::None {
        return Err(LuaError::arg_error(1, "value expected"));
    }
    // C: lua_pushstring(L, lua_typename(L, t));
    // Clone the bytes before the push to avoid borrow conflict with state.
    let name: Vec<u8> = state.type_name(t).to_vec();
    state.push_string(&name)?;
    Ok(1)
}

// ── next ──────────────────────────────────────────────────────────────────────

/// Table traversal iterator: given a table and a key, pushes the next key-value
/// pair.  Pushes nil and returns 1 when the traversal is exhausted.
///
/// C: `static int luaB_next(lua_State *L)`
pub(crate) fn next_fn(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: luaL_checktype(L, 1, LUA_TTABLE);
    state.check_arg_type(1, LuaType::Table)?;
    // C: lua_settop(L, 2);  /* create a 2nd argument if there isn't one */
    lua_vm::api::set_top(state, 2)?;
    // C: if (lua_next(L, 1)) return 2; else { lua_pushnil(L); return 1; }
    if state.table_next(1)? {
        Ok(2)
    } else {
        state.push(LuaValue::Nil);
        Ok(1)
    }
}

// ── pairs continuation (coroutine stub) ───────────────────────────────────────

/// Continuation for `pairs` when the `__pairs` metamethod yields.
/// In C this just returns 3; yields are stubbed in Phases A–D.
///
/// C: `static int pairscont(lua_State *L, int status, lua_KContext k)`
fn pairs_cont(_state: &mut LuaState) -> Result<usize, LuaError> {
    // C: (void)L; (void)status; (void)k; return 3;
    Ok(3)
}

// ── pairs ─────────────────────────────────────────────────────────────────────

/// Returns the `next` function, the table, and nil (or invokes a `__pairs`
/// metamethod).
///
/// C: `static int luaB_pairs(lua_State *L)`
pub(crate) fn pairs_fn(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: luaL_checkany(L, 1);
    state.check_arg_any(1)?;
    // C: if (luaL_getmetafield(L, 1, "__pairs") == LUA_TNIL)
    if state.get_metafield(1, b"__pairs")? == LuaType::Nil {
        // C: lua_pushcfunction(L, luaB_next); lua_pushvalue(L, 1); lua_pushnil(L);
        state.push_c_function(next_fn)?;
        state.push_copy(1)?;
        state.push(LuaValue::Nil);
    } else {
        // C: lua_pushvalue(L, 1); lua_callk(L, 1, 3, 0, pairscont);
        state.push_copy(1)?;
        // TODO(port): lua_callk continuation (pairscont) stubbed — coroutines Phase E.
        state.call(1, 3)?;
    }
    Ok(3)
}

// ── ipairs auxiliary ──────────────────────────────────────────────────────────

/// Iterator step function for `ipairs`: increments the counter and fetches
/// the next array element.  Returns the index + value, or just the index when
/// the value is nil (signalling end-of-iteration).
///
/// C: `static int ipairsaux(lua_State *L)`
fn ipairs_aux(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: lua_Integer i = luaL_checkinteger(L, 2);
    let i = state.check_arg_integer(2)?;
    // C: i = luaL_intop(+, i, 1);
    // luaL_intop(+, a, b) → wrapping integer addition (PORTING.md §9 / macros.tsv `intop`)
    let i = (i as u64).wrapping_add(1u64) as i64;
    // C: lua_pushinteger(L, i);
    state.push(LuaValue::Int(i));
    // C: return (lua_geti(L, 1, i) == LUA_TNIL) ? 1 : 2;
    let t = state.get_i(1, i)?;
    if t == LuaType::Nil {
        Ok(1)
    } else {
        Ok(2)
    }
}

// ── ipairs ────────────────────────────────────────────────────────────────────

/// Returns the `ipairsaux` iterator, the table, and 0 as the initial counter.
///
/// C: `static int luaB_ipairs(lua_State *L)`
pub(crate) fn ipairs_fn(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: luaL_checkany(L, 1);
    state.check_arg_any(1)?;
    // C: lua_pushcfunction(L, ipairsaux); lua_pushvalue(L, 1); lua_pushinteger(L, 0);
    state.push_c_function(ipairs_aux)?;
    state.push_copy(1)?;
    state.push(LuaValue::Int(0));
    Ok(3)
}

// ── loadfile ──────────────────────────────────────────────────────────────────

/// Loads a Lua chunk from a file.
///
/// C: `static int luaB_loadfile(lua_State *L)`
pub(crate) fn loadfile_fn(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: const char *fname = luaL_optstring(L, 1, NULL);
    // Clone to avoid borrow conflict with later state calls.
    let fname: Option<Vec<u8>> = state.opt_arg_string_bytes(1).ok();
    // C: const char *mode = luaL_optstring(L, 2, NULL);
    let mode: Option<Vec<u8>> = state.opt_arg_string_bytes(2).ok();
    // C: int env = (!lua_isnone(L, 3) ? 3 : 0);
    let env = if state.type_at(3) != LuaType::None { 3 } else { 0 };
    // C: int status = luaL_loadfilex(L, fname, mode);
    // TODO(port): File I/O must go through state's IO abstraction; std::fs banned outside lua-cli.
    let status_ok = state.load_file_ex(fname.as_deref(), mode.as_deref())?;
    load_aux(state, status_ok, env)
}

// ── generic_reader ────────────────────────────────────────────────────────────

/// Reader callback for `luaB_load` when the chunk source is a Lua function.
/// Calls the function at stack[1] repeatedly to obtain successive chunks.
///
/// C: `static const char *generic_reader(lua_State *L, void *ud, size_t *size)`
///
/// PORT NOTE: In C this is a `lua_Reader` function pointer passed to
/// `lua_load`. In Rust, readers are closures — but `generic_reader` itself
/// needs `&mut LuaState`, which conflicts with `state.load_with_reader`'s
/// own borrow.  The current translation materialises the reader as a free
/// function for documentation purposes; Phase B must resolve the design
/// (e.g., a separate reader-context type, or a split between "advance reader"
/// and "run Lua call" phases).
/// TODO(port): generic_reader — self-referential &mut borrow when used as lua_load callback.
fn generic_reader(state: &mut LuaState) -> Result<Option<Vec<u8>>, LuaError> {
    // C: luaL_checkstack(L, 2, "too many nested functions");
    state.ensure_stack(2, b"too many nested functions")?;
    // C: lua_pushvalue(L, 1); /* get function */ lua_call(L, 0, 1);
    state.push_copy(1)?;
    state.call(0, 1)?;
    // C: if (lua_isnil(L, -1)) { lua_pop(L, 1); *size = 0; return NULL; }
    if state.type_at(-1) == LuaType::Nil {
        state.pop_n(1);
        return Ok(None);
    }
    // C: else if (l_unlikely(!lua_isstring(L, -1)))
    //      luaL_error(L, "reader function must return a string");
    // lua_isstring in C is true for strings AND coercible numbers.
    if !matches!(state.type_at(-1), LuaType::String | LuaType::Number) {
        return Err(LuaError::runtime(format_args!(
            "reader function must return a string"
        )));
    }
    // C: lua_replace(L, RESERVEDSLOT); return lua_tolstring(L, RESERVEDSLOT, size);
    state.replace(RESERVED_SLOT)?;
    let bytes = state
        .to_lua_string_bytes(RESERVED_SLOT)
        .map(|b| b.to_vec());
    Ok(bytes)
}

// ── load ──────────────────────────────────────────────────────────────────────

/// Loads a Lua chunk from a string or a reader function.
///
/// C: `static int luaB_load(lua_State *L)`
pub(crate) fn load_fn(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: const char *s = lua_tolstring(L, 1, &l);
    // Determine whether argument 1 is a string (load from buffer) or a
    // function (load from reader).
    let is_string = matches!(state.type_at(1), LuaType::String | LuaType::Number);
    // C: const char *mode = luaL_optstring(L, 3, "bt");
    let mode: Vec<u8> = state
        .opt_arg_string_bytes(3)
        .map(|b| b.to_vec())
        .unwrap_or_else(|_| b"bt".to_vec());
    // C: int env = (!lua_isnone(L, 4) ? 4 : 0);
    let env = if state.type_at(4) != LuaType::None { 4 } else { 0 };
    let status_ok = if is_string {
        // C: const char *chunkname = luaL_optstring(L, 2, s);
        // C: status = luaL_loadbufferx(L, s, l, chunkname, mode);
        let chunk: Vec<u8> = state.to_lua_string_bytes(1).unwrap_or_default();
        let chunkname: Vec<u8> = state
            .opt_arg_string_bytes(2)
            .unwrap_or_else(|_| chunk.clone());
        state.load_buffer_ex(&chunk, &chunkname, &mode)?
    } else {
        let chunkname: Vec<u8> = state
            .opt_arg_string_bytes(2)
            .unwrap_or_else(|_| b"=(load)".to_vec());
        state.check_arg_type(1, LuaType::Function)?;
        lua_vm::api::set_top(state, RESERVED_SLOT)?;
        // TODO(port): generic_reader cannot be passed directly due to self-referential
        // &mut borrow — see generic_reader's PORT NOTE. Phase B resolves this.
        state.load_with_reader(generic_reader, &chunkname, &mode)?
    };
    load_aux(state, status_ok, env)
}

// ── dofile ────────────────────────────────────────────────────────────────────

/// Loads and runs a Lua file, forwarding all return values.
///
/// C: `static int dofilecont(lua_State *L, int d1, lua_KContext d2)`
/// C: `static int luaB_dofile(lua_State *L)`
fn dofile_cont(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: (void)d1; (void)d2; return lua_gettop(L) - 1;
    Ok((state.top() as i32 - 1) as usize)
}

pub(crate) fn dofile_fn(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: const char *fname = luaL_optstring(L, 1, NULL);
    let fname: Option<Vec<u8>> = state.opt_arg_string_bytes(1).ok();
    // C: lua_settop(L, 1);
    lua_vm::api::set_top(state, 1)?;
    // C: if (l_unlikely(luaL_loadfile(L, fname) != LUA_OK)) return lua_error(L);
    // TODO(port): File I/O must go through state's IO abstraction; std::fs banned outside lua-cli.
    if !state.load_file(fname.as_deref())? {
        return Err(LuaError::from_value(state.pop()));
    }
    // C: lua_callk(L, 0, LUA_MULTRET, 0, dofilecont);
    // TODO(port): lua_callk continuation (dofilecont) stubbed — coroutines Phase E.
    state.call(0, LUA_MULTRET)?;
    // C: return dofilecont(L, 0, 0);
    dofile_cont(state)
}

// ── assert ────────────────────────────────────────────────────────────────────

/// Raises an error if the first argument is falsy, otherwise passes all
/// arguments through as return values.
///
/// C: `static int luaB_assert(lua_State *L)`
pub(crate) fn assert_fn(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: if (l_likely(lua_toboolean(L, 1))) return lua_gettop(L);
    if state.to_boolean(1) {
        return Ok(state.top() as usize);
    }
    // C: luaL_checkany(L, 1); lua_remove(L, 1);
    state.check_arg_any(1)?;
    state.remove(1);
    // C: lua_pushliteral(L, "assertion failed!"); lua_settop(L, 1);
    state.push_string(b"assertion failed!")?;
    lua_vm::api::set_top(state, 1)?;
    // C: return luaB_error(L);
    error_fn(state)
}

// ── select ────────────────────────────────────────────────────────────────────

/// Returns a slice of its arguments starting at the given index, or returns
/// the count of arguments when called with `"#"`.
///
/// C: `static int luaB_select(lua_State *L)`
pub(crate) fn select_fn(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: int n = lua_gettop(L);
    let n = state.top() as i64;
    // C: if (lua_type(L, 1) == LUA_TSTRING && *lua_tostring(L, 1) == '#')
    // Check for '#' first byte without holding a borrow across subsequent ops.
    let first_is_hash = state.type_at(1) == LuaType::String && {
        state
            .to_lua_string_bytes(1)
            .and_then(|b| b.first().copied())
            == Some(b'#')
    };
    if first_is_hash {
        // C: lua_pushinteger(L, n-1);
        state.push(LuaValue::Int(n - 1));
        return Ok(1);
    }
    // C: lua_Integer i = luaL_checkinteger(L, 1);
    let mut i = state.check_arg_integer(1)?;
    // C: if (i < 0) i = n + i; else if (i > n) i = n;
    if i < 0 {
        i = n + i;
    } else if i > n {
        i = n;
    }
    // C: luaL_argcheck(L, 1 <= i, 1, "index out of range");
    if i < 1 {
        return Err(LuaError::arg_error(1, "index out of range"));
    }
    // C: return n - (int)i;
    // The values at stack positions [i+1 .. n] are already in place; the
    // runtime picks up the top (n - i) of them as results.
    Ok((n - i) as usize)
}

// ── pcall ─────────────────────────────────────────────────────────────────────

/// Protected call: returns true + results on success, or false + error on
/// failure.
///
/// C: `static int luaB_pcall(lua_State *L)`
pub(crate) fn pcall_fn(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: luaL_checkany(L, 1);
    state.check_arg_any(1)?;
    // C: lua_pushboolean(L, 1); lua_insert(L, 1);
    // Stack before: [f, a1, …, aN]
    // Stack after:  [true, f, a1, …, aN]
    state.push(LuaValue::Bool(true));
    state.insert(1);
    // C: status = lua_pcallk(L, lua_gettop(L) - 2, LUA_MULTRET, 0, 0, finishpcall);
    // nargs = gettop - 2 (subtract the sentinel `true` and the function).
    let nargs = state.top() as i32 - 2;
    // TODO(port): lua_pcallk continuation (finishpcall) stubbed — coroutines Phase E.
    let ok = match state.protected_call(nargs, LUA_MULTRET, 0) {
        Ok(()) => true,
        Err(e) => {
            state.push(e.into_value());
            false
        }
    };
    // C: return finishpcall(L, status, 0);
    finish_pcall(state, ok, 0)
}

// ── xpcall ────────────────────────────────────────────────────────────────────

/// Protected call with a separate error-handler function.
///
/// C: `static int luaB_xpcall(lua_State *L)`
pub(crate) fn xpcall_fn(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: int n = lua_gettop(L);
    let n = state.top() as i32;
    // C: luaL_checktype(L, 2, LUA_TFUNCTION);  /* check error function */
    state.check_arg_type(2, LuaType::Function)?;
    // C: lua_pushboolean(L, 1); lua_pushvalue(L, 1); lua_rotate(L, 3, 2);
    // Stack before rotate: [f, err, a1, …, aN, true, f]
    // Stack after rotate:  [f, err, true, f, a1, …, aN]
    state.push(LuaValue::Bool(true));
    state.push_copy(1)?;
    state.rotate(3, 2);
    // C: status = lua_pcallk(L, n - 2, LUA_MULTRET, 2, 2, finishpcall);
    // errfunc is at stack index 2; extra=2 means finishpcall skips 2 values.
    // TODO(port): lua_pcallk continuation (finishpcall) stubbed — coroutines Phase E.
    let ok = match state.protected_call(n - 2, LUA_MULTRET, 2) {
        Ok(()) => true,
        Err(e) => {
            state.push(e.into_value());
            false
        }
    };
    // C: return finishpcall(L, status, 2);
    finish_pcall(state, ok, 2)
}

// ── tostring ──────────────────────────────────────────────────────────────────

/// Converts any value to its string representation (calls `__tostring` if
/// present).
///
/// C: `static int luaB_tostring(lua_State *L)`
pub(crate) fn tostring_fn(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: luaL_checkany(L, 1); luaL_tolstring(L, 1, NULL);
    state.check_arg_any(1)?;
    // to_display_string pushes the converted string and returns a handle to it.
    // TODO(port): to_display_string method needs implementing on LuaState.
    state.to_display_string(1)?;
    Ok(1)
}

// ── Registration table ────────────────────────────────────────────────────────

/// All base-library functions registered into the global table by `open`.
///
/// C: `static const luaL_Reg base_funcs[]`
///
/// PORT NOTE: The C table includes placeholder entries
/// `{LUA_GNAME, NULL}` and `{"_VERSION", NULL}` that `luaopen_base` fills in
/// separately.  Those are omitted here; `open()` sets them explicitly.
pub(crate) const BASE_FUNCS: &[(&[u8], LuaLibFn)] = &[
    (b"assert",         assert_fn),
    (b"collectgarbage", collectgarbage_fn),
    (b"dofile",         dofile_fn),
    (b"error",          error_fn),
    (b"getmetatable",   getmetatable_fn),
    (b"ipairs",         ipairs_fn),
    (b"loadfile",       loadfile_fn),
    (b"load",           load_fn),
    (b"next",           next_fn),
    (b"pairs",          pairs_fn),
    (b"pcall",          pcall_fn),
    (b"print",          print_fn),
    (b"warn",           warn_fn),
    (b"rawequal",       rawequal_fn),
    (b"rawlen",         rawlen_fn),
    (b"rawget",         rawget_fn),
    (b"rawset",         rawset_fn),
    (b"select",         select_fn),
    (b"setmetatable",   setmetatable_fn),
    (b"tonumber",       tonumber_fn),
    (b"tostring",       tostring_fn),
    (b"type",           type_fn),
    (b"xpcall",         xpcall_fn),
];

// ── Module opener ─────────────────────────────────────────────────────────────

/// Open the base library: register all base functions into the global table,
/// then set `_G` (a self-reference) and `_VERSION`.
///
/// C: `LUAMOD_API int luaopen_base(lua_State *L)`
pub fn open(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: lua_pushglobaltable(L);
    state.push_globals()?;
    // C: luaL_setfuncs(L, base_funcs, 0);
    state.set_funcs(BASE_FUNCS, 0)?;
    // C: lua_pushvalue(L, -1); lua_setfield(L, -2, LUA_GNAME);
    state.push_copy(-1)?;
    state.set_field(-2, LUA_GNAME)?;
    // C: lua_pushliteral(L, LUA_VERSION); lua_setfield(L, -2, "_VERSION");
    state.push_string(LUA_VERSION_STR)?;
    state.set_field(-2, b"_VERSION")?;
    Ok(1)
}

// ──────────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        src/lbaselib.c  (549 lines, 32 functions)
//   target_crate:  lua-stdlib
//   confidence:    medium
//   todos:         21
//   port_notes:    5
//   unsafe_blocks: 0
//   notes:         All 32 C functions translated.  Main uncertainties are (1)
//                  LuaState method signatures (top/type_at/push/… — resolved
//                  in Phase B when lua-vm is compiled), (2) generic_reader's
//                  self-referential &mut borrow needs architectural resolution,
//                  (3) GC API stubs (gc_count, gc_step, …) need Phase D
//                  implementations, (4) I/O (write_output, load_file*) must be
//                  routed through a state abstraction rather than std::fs/stdout
//                  directly (Phase B), (5) pcallk / callk continuations are
//                  stubbed pending coroutine support in Phase E.  The fake
//                  `struct LuaState;` placeholder here avoids duplicate-definition
//                  errors while keeping the file self-contained; Phase B removes it.
// ──────────────────────────────────────────────────────────────────────────────
