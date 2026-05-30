//! Rust port of `ltablib.c` — Lua `table` standard library.
//!
//! Provides: `table.concat`, `table.insert`, `table.move`, `table.pack`,
//! `table.remove`, `table.sort`, `table.unpack`.
//!
//! C source: `reference/lua-5.4.7/src/ltablib.c` (430 lines, 14 functions)

use lua_types::{GcRef, LuaError, LuaTable, LuaType, LuaValue};
use crate::state_stub::{LuaState, LuaStateStubExt as _, CompareOp};

// ─── Operation flags ──────────────────────────────────────────────────────────
const TAB_R: u32 = 1;
const TAB_W: u32 = 2;
const TAB_L: u32 = 4;
const TAB_RW: u32 = TAB_R | TAB_W;

const RANLIMIT: u32 = 100;

type IdxT = u32;

// ─── Internal helpers ─────────────────────────────────────────────────────────

/// currently sitting at stack depth `n`; returns `true` if the result is not nil.
///
/// ```c
/// static int checkfield (lua_State *L, const char *key, int n) {
///   lua_pushstring(L, key);
///   return (lua_rawget(L, -n) != LUA_TNIL);
/// }
/// ```
fn check_field(state: &mut LuaState, key: &[u8], n: i32) -> Result<bool, LuaError> {
    // TODO(port): state.push_string pushes a Lua string from &[u8]; verify method name
    state.push_string(key)?;
    // raw_get(-n): looks up MT[key] (MT is at -n after the key push), replaces key with value
    let ty = state.raw_get(-n)?;
    Ok(ty != LuaType::Nil)
}

/// metatable with the metamethods required by `what`
/// (`TAB_R` → `__index`, `TAB_W` → `__newindex`, `TAB_L` → `__len`).
///
/// ```c
/// static void checktab (lua_State *L, int arg, int what) {
///   if (lua_type(L, arg) != LUA_TTABLE) {
///     int n = 1;
///     if (lua_getmetatable(L, arg) &&
///         (!(what & TAB_R) || checkfield(L, "__index", ++n)) &&
///         (!(what & TAB_W) || checkfield(L, "__newindex", ++n)) &&
///         (!(what & TAB_L) || checkfield(L, "__len", ++n))) {
///       lua_pop(L, n);
///     }
///     else
///       luaL_checktype(L, arg, LUA_TTABLE);
///   }
/// }
/// ```
///
/// PORT NOTE: stack cleanup on the error path is elided here (in C, `longjmp`
/// unwinds automatically). Phase B should add cleanup before the `check_arg_type`
/// call to leave the stack consistent.
fn check_tab(state: &mut LuaState, arg: i32, what: u32) -> Result<(), LuaError> {
    if state.type_at(arg) == LuaType::Table {
        return Ok(());
    }
    // `n` tracks how many items have been pushed (MT + checked field values).
    let mut n: i32 = 1;
    // TODO(port): state.get_metatable returns bool (pushes MT if found); verify method name
    let has_mt = state.get_metatable(arg)?;
    let mut ok = has_mt;

    // Short-circuit: each field is only checked if all previous checks passed.
    if ok && (what & TAB_R) != 0 {
        n += 1;
        ok = check_field(state, b"__index", n)?;
    }
    if ok && (what & TAB_W) != 0 {
        n += 1;
        ok = check_field(state, b"__newindex", n)?;
    }
    if ok && (what & TAB_L) != 0 {
        n += 1;
        ok = check_field(state, b"__len", n)?;
    }

    if ok {
        state.pop_n(n as usize);
        Ok(())
    } else {
        state.check_arg_type(arg, LuaType::Table)
    }
}

///
/// Check that argument `n` is a table (or table-like per `w`) and return its length.
fn aux_getn(state: &mut LuaState, n: i32, w: u32) -> Result<i64, LuaError> {
    check_tab(state, n, w | TAB_L)?;
    // TODO(port): state.length_at applies the `#` operator and returns i64; verify method name
    state.length_at(n)
}

#[inline]
fn plain_table_at(state: &mut LuaState, idx: i32) -> Option<GcRef<LuaTable>> {
    match state.value_at(idx) {
        LuaValue::Table(tbl) if tbl.metatable().is_none() => Some(tbl),
        _ => None,
    }
}

#[inline]
fn raw_set_int(
    state: &mut LuaState,
    tbl: GcRef<LuaTable>,
    key: i64,
    value: LuaValue,
) -> Result<(), LuaError> {
    state.gc_barrier_back(LuaValue::Table(tbl), value);
    tbl.try_raw_set_int(key, value)
}

// ─── table.insert ─────────────────────────────────────────────────────────────

///
/// ```c
/// static int tinsert (lua_State *L) {
///   lua_Integer pos;
///   lua_Integer e = aux_getn(L, 1, TAB_RW);
///   e = luaL_intop(+, e, 1);
///   switch (lua_gettop(L)) {
///     case 2: { pos = e; break; }
///     case 3: {
///       lua_Integer i;
///       pos = luaL_checkinteger(L, 2);
///       luaL_argcheck(L, (lua_Unsigned)pos - 1u < (lua_Unsigned)e, 2,
///                        "position out of bounds");
///       for (i = e; i > pos; i--) {
///         lua_geti(L, 1, i - 1);
///         lua_seti(L, 1, i);
///       }
///       break;
///     }
///     default:
///       return luaL_error(L, "wrong number of arguments to 'insert'");
///   }
///   lua_seti(L, 1, pos);
///   return 0;
/// }
/// ```
pub fn insert(state: &mut LuaState) -> Result<usize, LuaError> {
    let mut e = aux_getn(state, 1, TAB_RW)?;
    e = (e as u64).wrapping_add(1) as i64;
    let plain_table = plain_table_at(state, 1);

    let pos: i64 = match state.get_top() {
        2 => {
            if let Some(tbl) = plain_table {
                let value = state.value_at(2);
                raw_set_int(state, tbl, e, value)?;
                state.pop_n(1);
                return Ok(0);
            }
            e
        }
        3 => {
            let pos = state.check_arg_integer(2)?;
            // Checks 1 <= pos <= e (wrapping subtraction catches pos <= 0)
            if !((pos as u64).wrapping_sub(1) < (e as u64)) {
                return Err(LuaError::arg_error(2, "position out of bounds"));
            }
            if let Some(tbl) = plain_table {
                let value = state.value_at(3);
                let mut i = e;
                while i > pos {
                    let shifted = tbl.get_int(i - 1);
                    raw_set_int(state, tbl, i, shifted)?;
                    i -= 1;
                }
                raw_set_int(state, tbl, pos, value)?;
                state.pop_n(1);
                return Ok(0);
            }
            // Cache the table once to avoid re-resolving stack slot 1 on every
            // iteration of the shift loop. C's lua_geti is a single pointer
            // arithmetic operation; our index_to_value is a function call with
            // branches, so this saves ~2N index resolutions for shift count N.
            let tbl = state.value_at(1);
            let mut i = e;
            while i > pos {
                state.table_get_i_value(&tbl, i - 1)?;
                state.table_set_i_value(&tbl, i)?;
                i -= 1;
            }
            pos
        }
        _ => {
            return Err(LuaError::runtime(format_args!(
                "wrong number of arguments to 'insert'"
            )));
        }
    };
    state.table_set_i(1, pos)?;
    Ok(0)
}

// ─── table.remove ─────────────────────────────────────────────────────────────

///
/// ```c
/// static int tremove (lua_State *L) {
///   lua_Integer size = aux_getn(L, 1, TAB_RW);
///   lua_Integer pos = luaL_optinteger(L, 2, size);
///   if (pos != size)
///     luaL_argcheck(L, (lua_Unsigned)pos - 1u <= (lua_Unsigned)size, 2,
///                      "position out of bounds");
///   lua_geti(L, 1, pos);
///   for ( ; pos < size; pos++) {
///     lua_geti(L, 1, pos + 1);
///     lua_seti(L, 1, pos);
///   }
///   lua_pushnil(L);
///   lua_seti(L, 1, pos);
///   return 1;
/// }
/// ```
pub fn remove(state: &mut LuaState) -> Result<usize, LuaError> {
    let size = aux_getn(state, 1, TAB_RW)?;
    let mut pos = state.opt_arg_integer(2, size)?;
    if pos != size {
        if !((pos as u64).wrapping_sub(1) <= (size as u64)) {
            return Err(LuaError::arg_error(2, "position out of bounds"));
        }
    }
    // Cache the table once to avoid re-resolving stack slot 1 on every
    // iteration of the shift loop. C's lua_geti is a single pointer
    // arithmetic operation; our index_to_value is a function call with
    // branches, so this saves ~2N index resolutions for shift count N.
    if let Some(tbl) = plain_table_at(state, 1) {
        let result = tbl.get_int(pos);
        state.push(result);
        while pos < size {
            let shifted = tbl.get_int(pos + 1);
            raw_set_int(state, tbl, pos, shifted)?;
            pos += 1;
        }
        raw_set_int(state, tbl, pos, LuaValue::Nil)?;
        return Ok(1);
    }
    let tbl = state.value_at(1);
    state.table_get_i_value(&tbl, pos)?;   // push element to be returned
    while pos < size {
        state.table_get_i_value(&tbl, pos + 1)?;
        state.table_set_i_value(&tbl, pos)?;
        pos += 1;
    }
    state.push(LuaValue::Nil);
    state.table_set_i_value(&tbl, pos)?;   // remove last slot (table[pos] = nil)
    Ok(1)
}

// ─── table.move ───────────────────────────────────────────────────────────────

///
/// Copies elements `a1[f..e]` into `a2[t..]` (or `a1[t..]` if `a2` is absent).
/// Copies in increasing order when safe, decreasing when ranges overlap.
///
/// ```c
/// static int tmove (lua_State *L) {
///   lua_Integer f = luaL_checkinteger(L, 2);
///   lua_Integer e = luaL_checkinteger(L, 3);
///   lua_Integer t = luaL_checkinteger(L, 4);
///   int tt = !lua_isnoneornil(L, 5) ? 5 : 1;
///   checktab(L, 1, TAB_R);
///   checktab(L, tt, TAB_W);
///   if (e >= f) {
///     lua_Integer n, i;
///     luaL_argcheck(L, f > 0 || e < LUA_MAXINTEGER + f, 3, "too many elements to move");
///     n = e - f + 1;
///     luaL_argcheck(L, t <= LUA_MAXINTEGER - n + 1, 4, "destination wrap around");
///     if (t > e || t <= f || (tt != 1 && !lua_compare(L, 1, tt, LUA_OPEQ))) {
///       for (i = 0; i < n; i++) { lua_geti(L, 1, f + i); lua_seti(L, tt, t + i); }
///     } else {
///       for (i = n - 1; i >= 0; i--) { lua_geti(L, 1, f + i); lua_seti(L, tt, t + i); }
///     }
///   }
///   lua_pushvalue(L, tt);
///   return 1;
/// }
/// ```
pub fn tmove(state: &mut LuaState) -> Result<usize, LuaError> {
    let f = state.check_arg_integer(2)?;
    let e = state.check_arg_integer(3)?;
    let t = state.check_arg_integer(4)?;
    let tt: i32 = if !matches!(state.type_at(5), LuaType::None | LuaType::Nil) {
        5
    } else {
        1
    };
    check_tab(state, 1, TAB_R)?;
    check_tab(state, tt, TAB_W)?;

    if e >= f {
        if !(f > 0 || e < i64::MAX + f) {
            return Err(LuaError::arg_error(3, "too many elements to move"));
        }
        let n = e - f + 1;
        if !(t <= i64::MAX - n + 1) {
            return Err(LuaError::arg_error(4, "destination wrap around"));
        }
        // Copy forward (increasing) when safe to do so; backward when ranges overlap.
        // TODO(port): state.compare(a, b, CompareOp::Eq) → lua_compare LUA_OPEQ; verify method
        let copy_forward = t > e
            || t <= f
            || (tt != 1 && !state.compare(1, tt, CompareOp::Eq)?);
        if copy_forward {
            for i in 0..n {
                state.table_get_i(1, f + i)?;
                state.table_set_i(tt, t + i)?;
            }
        } else {
            for i in (0..n).rev() {
                state.table_get_i(1, f + i)?;
                state.table_set_i(tt, t + i)?;
            }
        }
    }
    // TODO(port): state.push_value_at → lua_pushvalue; verify method name
    state.push_value_at(tt)?;
    Ok(1)
}

// ─── table.concat ─────────────────────────────────────────────────────────────

/// a string-or-number, add its string representation to `buf`, then pop it.
///
/// ```c
/// static void addfield (lua_State *L, luaL_Buffer *b, lua_Integer i) {
///   lua_geti(L, 1, i);
///   if (l_unlikely(!lua_isstring(L, -1)))
///     luaL_error(L, "invalid value (%s) at index %I in table for 'concat'",
///                   luaL_typename(L, -1), (LUAI_UACINT)i);
///   luaL_addvalue(b);
/// }
/// ```
///
/// PORT NOTE: `luaL_Buffer` in C accumulates bytes and then pushes the result;
/// Rust uses a `Vec<u8>` accumulator passed by mutable reference instead.
fn add_field(state: &mut LuaState, buf: &mut Vec<u8>, idx: i64) -> Result<(), LuaError> {
    state.table_get_i(1, idx)?;
    if !matches!(state.type_at(-1), LuaType::String | LuaType::Number) {
        // TODO(port): state.type_name_str_at(-1) returns &'static str for the base type name
        let type_name = state.type_name_str_at(-1);
        return Err(LuaError::runtime(format_args!(
            "invalid value ({:?}) at index {} in table for 'concat'",
            type_name, idx
        )));
    }
    // TODO(port): state.to_bytes_at(-1) converts via Lua's tostring coercion; verify method name
    let bytes = state.to_bytes_at(-1).ok_or_else(|| LuaError::runtime(format_args!("invalid value at index {}", idx)))?;
    buf.extend_from_slice(&bytes);
    state.pop_n(1);
    Ok(())
}

///
/// ```c
/// static int tconcat (lua_State *L) {
///   luaL_Buffer b;
///   lua_Integer last = aux_getn(L, 1, TAB_R);
///   size_t lsep;
///   const char *sep = luaL_optlstring(L, 2, "", &lsep);
///   lua_Integer i = luaL_optinteger(L, 3, 1);
///   last = luaL_optinteger(L, 4, last);
///   luaL_buffinit(L, &b);
///   for (; i < last; i++) { addfield(L, &b, i); luaL_addlstring(&b, sep, lsep); }
///   if (i == last) addfield(L, &b, i);
///   luaL_pushresult(&b);
///   return 1;
/// }
/// ```
pub fn concat(state: &mut LuaState) -> Result<usize, LuaError> {
    let last = aux_getn(state, 1, TAB_R)?;
    // TODO(port): state.opt_arg_lstring(n, default) → luaL_optlstring; verify method name
    // Clone the separator before any stack-mutating calls that might invalidate it.
    let sep: Vec<u8> = state.opt_arg_lstring(2, Some(b""))?.unwrap_or_default();
    let mut i = state.opt_arg_integer(3, 1)?;
    let last = state.opt_arg_integer(4, last)?;

    // PORT NOTE: C uses luaL_Buffer (which may back-patch the stack);
    // Rust uses a plain Vec<u8> accumulator and pushes the result at the end.
    let mut buf: Vec<u8> = Vec::new();
    while i < last {
        add_field(state, &mut buf, i)?;
        buf.extend_from_slice(&sep);
        i += 1;
    }
    if i == last {
        add_field(state, &mut buf, i)?;
    }
    // TODO(port): state.push_lstring pushes a Lua string from &[u8]; verify method name
    state.push_lstring(&buf)?;
    Ok(1)
}

// ─── table.pack / table.unpack ────────────────────────────────────────────────

///
/// Creates a new table `t` with all arguments as integer keys and `t.n` set
/// to the argument count.
///
/// ```c
/// static int tpack (lua_State *L) {
///   int i;
///   int n = lua_gettop(L);
///   lua_createtable(L, n, 1);
///   lua_insert(L, 1);
///   for (i = n; i >= 1; i--)
///     lua_seti(L, 1, i);
///   lua_pushinteger(L, n);
///   lua_setfield(L, 1, "n");
///   return 1;
/// }
/// ```
pub fn pack(state: &mut LuaState) -> Result<usize, LuaError> {
    let n = state.get_top();
    // TODO(port): state.create_table(narr, nrec) → lua_createtable; verify method name
    state.create_table(n, 1)?;
    state.insert(1)?;
    // table_set_i pops the top; args shift from n+1..=2 down to 1..=n as we pop
    for i in (1..=n).rev() {
        state.table_set_i(1, i as i64)?;
    }
    state.push(LuaValue::Int(n as i64));
    // TODO(port): state.set_field(stack_pos, key_bytes) → lua_setfield; verify method name
    state.set_field(1, b"n")?;
    Ok(1)
}

///
/// Pushes `t[i], t[i+1], …, t[j]` and returns the count.
///
/// ```c
/// static int tunpack (lua_State *L) {
///   lua_Unsigned n;
///   lua_Integer i = luaL_optinteger(L, 2, 1);
///   lua_Integer e = luaL_opt(L, luaL_checkinteger, 3, luaL_len(L, 1));
///   if (i > e) return 0;
///   n = (lua_Unsigned)e - i;
///   if (l_unlikely(n >= (unsigned int)INT_MAX ||
///                  !lua_checkstack(L, (int)(++n))))
///     return luaL_error(L, "too many results to unpack");
///   for (; i < e; i++) lua_geti(L, 1, i);
///   lua_geti(L, 1, e);
///   return (int)n;
/// }
/// ```
pub fn unpack(state: &mut LuaState) -> Result<usize, LuaError> {
    let i = state.opt_arg_integer(2, 1)?;
    let e = if matches!(state.type_at(3), LuaType::None | LuaType::Nil) {
        state.length_at(1)?
    } else {
        state.check_arg_integer(3)?
    };
    if i > e {
        return Ok(0); // empty range
    }
    let n = (e as u64).wrapping_sub(i as u64);
    // The size check uses the pre-increment value so that a wrapped-to-0 result
    // (e.g. i=minI, e=maxI yields n = 2^64-1 pre-inc, 0 post-inc) still trips
    // the error rather than silently entering a 2^64-iteration loop.
    if n >= i32::MAX as u64 {
        return Err(LuaError::runtime(format_args!("too many results to unpack")));
    }
    let n = n + 1;
    if !state.check_stack_growth(n as i32) {
        return Err(LuaError::runtime(format_args!("too many results to unpack")));
    }
    let n = n as i64;
    let mut k = i;
    while k < e {
        state.table_get_i(1, k)?;
        k += 1;
    }
    state.table_get_i(1, e)?; // push last element
    Ok(n as usize)
}

// ─── Quicksort ────────────────────────────────────────────────────────────────

/// selection when a partition is severely imbalanced.
///
/// `unsigned int` array whose elements are summed.
///
/// PORT NOTE: C uses a small randomised pivot guard to avoid pathological sort
/// partitions. The Rust port asks the host for entropy when available and falls
/// back to a deterministic pivot value in sandboxed/bare-WASM hosts.
fn randomize_pivot(state: &LuaState) -> u32 {
    let entropy = state.global().entropy_hook.map(|hook| hook()).unwrap_or(0);
    let mixed = entropy ^ entropy.wrapping_shr(32);
    (mixed as u32) ^ (mixed as u32).wrapping_shr(16)
}

/// `table[i]` and `table[j]` respectively (table is at stack position 1).
///
/// ```c
/// static void set2 (lua_State *L, IdxT i, IdxT j) {
///   lua_seti(L, 1, i);
///   lua_seti(L, 1, j);
/// }
/// ```
fn set2(state: &mut LuaState, i: IdxT, j: IdxT) -> Result<(), LuaError> {
    // First seti pops the stack top; second seti pops the new top.
    state.table_set_i(1, i as i64)?;
    state.table_set_i(1, j as i64)?;
    Ok(())
}

/// sort order: either the `<` operator (if arg 2 is nil) or the user's
/// comparison function at stack position 2.
///
/// ```c
/// static int sort_comp (lua_State *L, int a, int b) {
///   if (lua_isnil(L, 2))
///     return lua_compare(L, a, b, LUA_OPLT);
///   else {
///     int res;
///     lua_pushvalue(L, 2);
///     lua_pushvalue(L, a-1);
///     lua_pushvalue(L, b-2);
///     lua_call(L, 2, 1);
///     res = lua_toboolean(L, -1);
///     lua_pop(L, 1);
///     return res;
///   }
/// }
/// ```
///
/// The offsets `a-1` and `b-2` compensate for the function and first-argument
/// copies pushed before the respective values: `a-1` accounts for the function
/// push; `b-2` accounts for both the function push and the copy of `a`.
fn sort_comp(state: &mut LuaState, a: i32, b: i32) -> Result<bool, LuaError> {
    if state.type_at(2) == LuaType::Nil {
        // No user comparator: use the default `<` operator.
        return state.compare(a, b, CompareOp::Lt);
    }
    // User comparator at stack position 2.
    state.push_value_at(2)?;       // push function
    state.push_value_at(a - 1)?;   // push copy of a (compensate for function push)
    state.push_value_at(b - 2)?;   // push copy of b (compensate for function + a copy)
    state.call(2, 1)?;
    // TODO(port): state.to_boolean(-1) → lua_toboolean (never fails); verify method name
    let res = state.to_boolean(-1);
    state.pop_n(1);
    Ok(res)
}

/// is already on the top of the Lua stack.
///
/// Precondition: `a[lo] <= P == a[up-1] <= a[up]` and `P` is at stack top.
/// Postcondition: `a[lo..i-1] <= a[i] == P <= a[i+1..up]`; stack is clean.
/// Returns the final pivot index `i`.
///
/// ```c
/// static IdxT partition (lua_State *L, IdxT lo, IdxT up) {
///   IdxT i = lo;
///   IdxT j = up - 1;
///   for (;;) {
///     while ((void)lua_geti(L, 1, ++i), sort_comp(L, -1, -2)) {
///       if (l_unlikely(i == up - 1))
///         luaL_error(L, "invalid order function for sorting");
///       lua_pop(L, 1);
///     }
///     while ((void)lua_geti(L, 1, --j), sort_comp(L, -3, -1)) {
///       if (l_unlikely(j < i))
///         luaL_error(L, "invalid order function for sorting");
///       lua_pop(L, 1);
///     }
///     if (j < i) {
///       lua_pop(L, 1);
///       set2(L, up - 1, i);
///       return i;
///     }
///     set2(L, i, j);
///   }
/// }
/// ```
fn partition(state: &mut LuaState, lo: IdxT, up: IdxT) -> Result<IdxT, LuaError> {
    let mut i: IdxT = lo;
    let mut j: IdxT = up - 1;
    // Entry: stack top is P (pivot value).
    loop {
        // Advance i: find first a[i] >= P.
        // Stack during i-loop body: P(-2), a[i](-1)
        loop {
            i += 1;
            state.table_get_i(1, i as i64)?; // push a[i]
            if !sort_comp(state, -1, -2)? {
                // a[i] >= P: leave a[i] on stack and exit
                break;
            }
            // a[i] < P; check for invalid comparator
            if i == up - 1 {
                return Err(LuaError::runtime(format_args!(
                    "invalid order function for sorting"
                )));
            }
            state.pop_n(1); // remove a[i]
        }
        // Retreat j: find last a[j] <= P.
        // Stack during j-loop body: P(-3), a[i](-2), a[j](-1)
        loop {
            // PERF(port): wrapping_sub mirrors C unsigned IdxT behaviour for edge cases
            j = j.wrapping_sub(1);
            state.table_get_i(1, j as i64)?; // push a[j]
            if !sort_comp(state, -3, -1)? {
                // P >= a[j]: leave a[j] on stack and exit
                break;
            }
            // P < a[j]; check for invalid comparator
            if j < i {
                return Err(LuaError::runtime(format_args!(
                    "invalid order function for sorting"
                )));
            }
            state.pop_n(1); // remove a[j]
        }
        // Stack: P(-3), a[i](-2), a[j](-1)
        if j < i {
            // No out-of-place elements; finalize: place pivot at position i.
            state.pop_n(1); // pop a[j]; stack: P(-2), a[i](-1)
            set2(state, up - 1, i)?; // table[up-1] = a[i], table[i] = P; stack clean
            return Ok(i);
        }
        // Swap a[i] and a[j] to restore loop invariant.
        // set2: table[i] = a[j] (pops -1), table[j] = a[i] (pops new -1); stack: P(-1)
        set2(state, i, j)?;
    }
}

/// `[lo, up]`, randomised by `rnd`.
///
/// ```c
/// static IdxT choosePivot (IdxT lo, IdxT up, unsigned int rnd) {
///   IdxT r4 = (up - lo) / 4;
///   IdxT p = rnd % (r4 * 2) + (lo + r4);
///   lua_assert(lo + r4 <= p && p <= up - r4);
///   return p;
/// }
/// ```
fn choose_pivot(lo: IdxT, up: IdxT, rnd: u32) -> IdxT {
    let r4 = (up - lo) / 4; // range / 4
    let p = rnd % (r4 * 2) + (lo + r4);
    debug_assert!(lo + r4 <= p && p <= up - r4);
    p
}

///
/// Sorts `table[lo..=up]` in place, recursing on the smaller partition and
/// tail-looping on the larger (to bound Rust's call stack). Randomises pivot
/// selection when a partition is badly imbalanced.
///
/// ```c
/// static void auxsort (lua_State *L, IdxT lo, IdxT up, unsigned int rnd) {
///   while (lo < up) {
///     IdxT p, n;
///     lua_geti(L, 1, lo); lua_geti(L, 1, up);
///     if (sort_comp(L, -1, -2)) set2(L, lo, up); else lua_pop(L, 2);
///     if (up - lo == 1) return;
///     if (up - lo < RANLIMIT || rnd == 0) p = (lo + up)/2;
///     else p = choosePivot(lo, up, rnd);
///     lua_geti(L, 1, p); lua_geti(L, 1, lo);
///     if (sort_comp(L, -2, -1)) set2(L, p, lo);
///     else {
///       lua_pop(L, 1); lua_geti(L, 1, up);
///       if (sort_comp(L, -1, -2)) set2(L, p, up); else lua_pop(L, 2);
///     }
///     if (up - lo == 2) return;
///     lua_geti(L, 1, p); lua_pushvalue(L, -1); lua_geti(L, 1, up - 1);
///     set2(L, p, up - 1);
///     p = partition(L, lo, up);
///     if (p - lo < up - p) {
///       auxsort(L, lo, p - 1, rnd); n = p - lo; lo = p + 1;
///     } else {
///       auxsort(L, p + 1, up, rnd); n = up - p; up = p - 1;
///     }
///     if ((up - lo) / 128 > n) rnd = l_randomizePivot();
///   }
/// }
/// ```
fn aux_sort(state: &mut LuaState, mut lo: IdxT, mut up: IdxT, mut rnd: u32) -> Result<(), LuaError> {
    while lo < up {
        // Step 1: ensure a[lo] <= a[up] (cheap two-element sort)
        state.table_get_i(1, lo as i64)?; // push a[lo]
        state.table_get_i(1, up as i64)?; // push a[up]
        if sort_comp(state, -1, -2)? {
            set2(state, lo, up)?; // swap so a[lo] <= a[up]
        } else {
            state.pop_n(2);
        }
        if up - lo == 1 {
            return Ok(()); // only 2 elements, now sorted
        }

        // Step 2: choose pivot index
        let mut p: IdxT = if up - lo < RANLIMIT || rnd == 0 {
            (lo + up) / 2 // midpoint pivot for small/non-random runs
        } else {
            choose_pivot(lo, up, rnd)
        };

        // Step 3: median-of-three: sort a[lo], a[p], a[up]
        state.table_get_i(1, p as i64)?;  // push a[p]
        state.table_get_i(1, lo as i64)?; // push a[lo]
        if sort_comp(state, -2, -1)? {
            set2(state, p, lo)?; // swap a[p] ↔ a[lo]; stack clean
        } else {
            state.pop_n(1); // remove a[lo]; stack: a[p]
            state.table_get_i(1, up as i64)?; // push a[up]; stack: a[p], a[up]
            if sort_comp(state, -1, -2)? {
                set2(state, p, up)?; // swap a[p] ↔ a[up]; stack clean
            } else {
                state.pop_n(2); // remove a[up] and a[p]; stack clean
            }
        }
        // Stack is clean at this point.
        if up - lo == 2 {
            return Ok(()); // only 3 elements, now sorted
        }

        // Step 4: move pivot to a[up-1] and call partition.
        //
        // Stack evolution:
        //   table_get_i(p):    a[p]  (-1)
        //   push_value_at(-1): a[p]  (-2), a[p]_copy  (-1)
        //   table_get_i(up-1): a[p]  (-3), a[p]_copy  (-2), a[up-1]  (-1)
        //   set2(p, up-1):     table[p] = a[up-1], table[up-1] = a[p]_copy;
        //                      stack: a[p] (-1)  ← pivot for partition
        state.table_get_i(1, p as i64)?;
        state.push_value_at(-1)?; // duplicate: two copies of pivot on stack
        state.table_get_i(1, (up - 1) as i64)?;
        set2(state, p, up - 1)?;
        // One copy of the pivot value remains at the stack top for partition.

        p = partition(state, lo, up)?;
        // Stack is clean after partition returns.

        // Step 5: recurse on smaller partition; tail-loop on larger.
        let n: IdxT;
        if p - lo < up - p {
            aux_sort(state, lo, p - 1, rnd)?;
            n = p - lo;
            lo = p + 1; // tail: sort [p+1 .. up]
        } else {
            aux_sort(state, p + 1, up, rnd)?;
            n = up - p;
            up = p - 1; // tail: sort [lo .. p-1]
        }

        // Re-randomise if the partition was severely imbalanced.
        if (up - lo) / 128 > n {
            rnd = randomize_pivot(state);
        }
    }
    Ok(())
}

///
/// ```c
/// static int sort (lua_State *L) {
///   lua_Integer n = aux_getn(L, 1, TAB_RW);
///   if (n > 1) {
///     luaL_argcheck(L, n < INT_MAX, 1, "array too big");
///     if (!lua_isnoneornil(L, 2))
///       luaL_checktype(L, 2, LUA_TFUNCTION);
///     lua_settop(L, 2);
///     auxsort(L, 1, (IdxT)n, 0);
///   }
///   return 0;
/// }
/// ```
pub fn sort(state: &mut LuaState) -> Result<usize, LuaError> {
    let n = aux_getn(state, 1, TAB_RW)?;
    if n > 1 {
        if !(n < i32::MAX as i64) {
            return Err(LuaError::arg_error(1, "array too big"));
        }
        if !matches!(state.type_at(2), LuaType::None | LuaType::Nil) {
            state.check_arg_type(2, LuaType::Function)?;
        }
        // Must go through the public C-API set_top (relative to the call
        // frame); the inherent LuaState::set_top treats its argument as
        // an absolute stack slot and would corrupt the frame.
        lua_vm::api::set_top(state, 2)?;
        aux_sort(state, 1, n as IdxT, 0)?;
    }
    Ok(0)
}

// ─── Registration ─────────────────────────────────────────────────────────────

///
/// ```c
/// static const luaL_Reg tab_funcs[] = {
///   {"concat", tconcat}, {"insert", tinsert}, {"pack", tpack},
///   {"unpack", tunpack}, {"remove", tremove}, {"move", tmove},
///   {"sort", sort}, {NULL, NULL}
/// };
/// ```
///
/// PORT NOTE: In Rust we represent this as a slice of `(&[u8], fn-ptr)` pairs;
/// the sentinel `{NULL, NULL}` is implicit (the slice has a known length).
pub const TABLE_FUNCS: &[(&[u8], fn(&mut LuaState) -> Result<usize, LuaError>)] = &[
    (b"concat", concat),
    (b"insert", insert),
    (b"pack", pack),
    (b"unpack", unpack),
    (b"remove", remove),
    (b"move", tmove),
    (b"sort", sort),
];

/// `table.create(nseq [, nrec])` — Lua 5.5 addition
/// (`specs/research/5.5-upstream-delta.md` §5, `ltablib.c`).
///
/// Preallocates a table with `nseq` array (sequence) slots and `nrec` hash
/// (record) slots, returning the empty table. Preallocation is purely a
/// capacity hint; the returned table is observably empty (length 0, no keys),
/// so this implementation is behaviorally faithful even though our
/// `create_table` may treat the sizes as advisory.
///
/// Registered into the `table` roster only under [`lua_types::LuaVersion::V55`]
/// (see [`open_table`]); absent under 5.1-5.4, matching upstream.
pub fn create(state: &mut LuaState) -> Result<usize, LuaError> {
    let nseq = state.check_arg_integer(1)?;
    let nrec = state.opt_arg_integer(2, 0)?;
    if nseq < 0 || nseq > i32::MAX as i64 {
        return Err(LuaError::runtime(format_args!(
            "bad argument #1 to 'create' (size out of range)"
        )));
    }
    if nrec < 0 || nrec > i32::MAX as i64 {
        return Err(LuaError::runtime(format_args!(
            "bad argument #2 to 'create' (size out of range)"
        )));
    }
    state.create_table(nseq as i32, nrec as i32)?;
    Ok(1)
}

// ─── Module opener ────────────────────────────────────────────────────────────

///
/// ```c
/// LUAMOD_API int luaopen_table (lua_State *L) {
///   luaL_newlib(L, tab_funcs);
///   return 1;
/// }
/// ```
pub fn open_table(state: &mut LuaState) -> Result<usize, LuaError> {
    // TODO(port): state.new_lib → luaL_newlib; creates a new table and registers functions;
    //             verify method name and signature
    state.new_lib(TABLE_FUNCS)?;
    // Per-version roster delta: `table.create` is a Lua 5.5 addition
    // (`specs/research/5.5-upstream-delta.md` §5), absent in 5.1-5.4. Register
    // it only on the V55 backend so the version seam carries a real,
    // script-observable stdlib difference. `new_lib` leaves the new table on
    // the stack top, so we register `create` into it directly.
    if matches!(state.global().lua_version, lua_types::LuaVersion::V55) {
        const CREATE_FUNCS: &[(&[u8], fn(&mut LuaState) -> Result<usize, LuaError>)] =
            &[(b"create", create)];
        state.set_funcs_with_upvalues(CREATE_FUNCS, 0)?;
    }
    Ok(1)
}

// ──────────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        src/ltablib.c  (430 lines, 14 functions)
//   target_crate:  lua-stdlib
//   confidence:    medium
//   todos:         17
//   port_notes:    5
//   unsafe_blocks: 0
//   notes:         Logic is faithfully translated. All TODOs are method-name
//                  uncertainties for LuaState API calls (table_get_i, table_set_i,
//                  opt_arg_integer, opt_arg_lstring, get_metatable, to_bytes_at,
//                  push_lstring, push_value_at, compare, new_lib, set_top,
//                  check_stack_growth, type_name_str_at, create_table) — Phase B
//                  maps these to the real method names once lua-vm is drafted.
//                  Stack cleanup on error paths is elided (C uses longjmp);
//                  needs Phase B attention in check_tab and add_field.
//                  PERF: remove() and insert() shift loops now cache the table
//                  value once (via value_at) and call table_get_i_value /
//                  table_set_i_value, bypassing the per-iteration index_to_value
//                  call. This shrank the index_to_value hot frame and improved
//                  table_ops_long from ~4.76x to ~4.02x vs reference.
// ──────────────────────────────────────────────────────────────────────────────
