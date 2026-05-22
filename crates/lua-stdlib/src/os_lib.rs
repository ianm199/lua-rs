//! Lua `os` standard library.
//!
//! Ports `src/loslib.c` (430 lines, 12 functions) to Rust.
//!
//! ## Platform access limitations
//!
//! Several `os.*` functions require OS-level capabilities that are restricted to
//! `lua-cli` per PORTING.md (no `std::fs`, no `std::process` in `lua-stdlib`).
//! Those functions are stubbed with `TODO(port)` markers and return nil / error
//! until a capability-injection layer is designed in Phase B.
//!
//! Time decomposition (`os.date`, `os.time`) requires C-library functions
//! (`gmtime_r`, `localtime_r`, `mktime`, `strftime`).  Those call sites are
//! flagged with `TODO(port)` and the stubs use a zero-initialised `TmFields`.

// C: #include "lua.h" / "lauxlib.h" / "lualib.h"
use lua_types::{LuaError, LuaState, LuaType, LuaValue};

// ── Constants ────────────────────────────────────────────────────────────────

// C: #define LUA_STRFTIMEOPTIONS  "aAbBcCdDeFgGhHIjmMnprRStTuUVwWxXyYzZ%" \
// C:     "||" "EcECExEXEyEY" "OdOeOHOIOmOMOSOuOUOVOwOWOy"
//
// Valid `strftime` conversion specifiers — C99 / POSIX variant.
// Single-char specifiers appear first; the `||` sentinel signals the start
// of 2-char specifiers (e.g. `%EC`, `%Oy`).  See `check_strftime_option`.
const STRFTIME_OPTIONS: &[u8] =
    b"aAbBcCdDeFgGhHIjmMnprRStTuUVwWxXyYzZ%||EcECExEXEyEYOdOeOHOIOmOMOSOuOUOVOwOWOy";

// C: #define SIZETIMEFMT 250
const SIZE_TIME_FMT: usize = 250;

// ── TmFields ─────────────────────────────────────────────────────────────────

/// Local mirror of C's `struct tm`.
///
/// Field conventions follow the C standard: `tm_year` is years since 1900,
/// `tm_mon` ∈ [0, 11], `tm_wday` ∈ [0, 6] (Sunday = 0), `tm_isdst` is −1 when
/// DST status is unknown.
///
/// TODO(port): In Phase B, replace with the `libc::tm` type (via the `libc` crate)
/// or an equivalent from `chrono` / `time`.  Conversion from / to Unix timestamps
/// is not implemented in Phase A — stubs that need a broken-down time use
/// `TmFields::default()` (all zeros).
#[derive(Debug, Default, Clone)]
pub struct TmFields {
    pub tm_sec: i32,
    pub tm_min: i32,
    pub tm_hour: i32,
    pub tm_mday: i32,
    pub tm_mon: i32,
    pub tm_year: i32,
    pub tm_wday: i32,
    pub tm_yday: i32,
    pub tm_isdst: i32,
}

// ── ByteDisplay ──────────────────────────────────────────────────────────────

/// `Display` adapter for `&[u8]` slices known to contain ASCII bytes.
///
/// Used only for formatting Lua table field names (always ASCII identifiers such
/// as `"year"`, `"month"`) inside error messages, without allocating a `String`.
struct ByteDisplay<'a>(&'a [u8]);

impl std::fmt::Display for ByteDisplay<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for &b in self.0 {
            write!(f, "{}", b as char)?;
        }
        Ok(())
    }
}

// ── Private stack-manipulation helpers ───────────────────────────────────────

/// C: `static void setfield(lua_State *L, const char *key, int value, int delta)`
///
/// Pushes `(value as i64) + (delta as i64)` as a Lua integer, then stores it
/// in the table currently on top of the stack at field `key`.
fn set_field(state: &mut LuaState, key: &[u8], value: i32, delta: i32) -> Result<(), LuaError> {
    // C: lua_pushinteger(L, (lua_Integer)value + delta);
    state.push(LuaValue::Int((value as i64) + (delta as i64)));
    // C: lua_setfield(L, -2, key);
    state.set_field(-2, key)?;
    Ok(())
}

/// C: `static void setboolfield(lua_State *L, const char *key, int value)`
///
/// Stores a boolean at field `key` in the table on top of the stack.
/// A negative `value` means "undefined" — the field is silently skipped.
fn set_bool_field(state: &mut LuaState, key: &[u8], value: i32) -> Result<(), LuaError> {
    // C: if (value < 0) return;  /* undefined? */
    if value < 0 {
        return Ok(());
    }
    // C: lua_pushboolean(L, value);
    state.push(LuaValue::Bool(value != 0));
    // C: lua_setfield(L, -2, key);
    state.set_field(-2, key)?;
    Ok(())
}

/// C: `static void setallfields(lua_State *L, struct tm *stm)`
///
/// Writes every field of `stm` into the table on top of the stack, applying the
/// offsets that convert from C-library conventions to Lua conventions:
/// year+1900, month+1, wday+1, yday+1.
fn set_all_fields(state: &mut LuaState, stm: &TmFields) -> Result<(), LuaError> {
    // C: setfield(L, "year",  stm->tm_year, 1900);
    set_field(state, b"year",  stm.tm_year, 1900)?;
    // C: setfield(L, "month", stm->tm_mon,  1);
    set_field(state, b"month", stm.tm_mon,  1)?;
    // C: setfield(L, "day",   stm->tm_mday, 0);
    set_field(state, b"day",   stm.tm_mday, 0)?;
    // C: setfield(L, "hour",  stm->tm_hour, 0);
    set_field(state, b"hour",  stm.tm_hour, 0)?;
    // C: setfield(L, "min",   stm->tm_min,  0);
    set_field(state, b"min",   stm.tm_min,  0)?;
    // C: setfield(L, "sec",   stm->tm_sec,  0);
    set_field(state, b"sec",   stm.tm_sec,  0)?;
    // C: setfield(L, "yday",  stm->tm_yday, 1);
    set_field(state, b"yday",  stm.tm_yday, 1)?;
    // C: setfield(L, "wday",  stm->tm_wday, 1);
    set_field(state, b"wday",  stm.tm_wday, 1)?;
    // C: setboolfield(L, "isdst", stm->tm_isdst);
    set_bool_field(state, b"isdst", stm.tm_isdst)?;
    Ok(())
}

/// C: `static int getboolfield(lua_State *L, const char *key)`
///
/// Reads a boolean field from the table on top of the stack.
/// Returns `-1` when the field is absent (nil), or `0` / `1` for false / true.
fn get_bool_field(state: &mut LuaState, key: &[u8]) -> Result<i32, LuaError> {
    // C: res = (lua_getfield(L, -1, key) == LUA_TNIL) ? -1 : lua_toboolean(L, -1);
    let ty = state.get_field(-1, key)?;
    let res = if matches!(ty, LuaType::Nil) {
        -1i32
    } else {
        // C: lua_toboolean(L, -1)
        state.to_boolean(-1) as i32
    };
    // C: lua_pop(L, 1);
    state.pop_n(1);
    Ok(res)
}

/// C: `static int getfield(lua_State *L, const char *key, int d, int delta)`
///
/// Reads an integer field from the table on top of the stack.
///
/// * `d` — default when the field is absent; pass `d < 0` to make absence an
///   error.
/// * `delta` — subtracted from the read value to convert from Lua's offset
///   representation back to C-library conventions (e.g. month−1, year−1900).
///
/// PORT NOTE: Stack cleanup on error paths (pop before returning Err) is added
/// vs. the C version where `luaL_error` never returns (longjmp).
fn get_field(
    state: &mut LuaState,
    key: &[u8],
    d: i32,
    delta: i32,
) -> Result<i32, LuaError> {
    // C: int t = lua_getfield(L, -1, key);
    let ty = state.get_field(-1, key)?;
    // C: lua_Integer res = lua_tointegerx(L, -1, &isnum);
    let maybe_int = state.to_integer_x(-1);
    let res: i32 = match maybe_int {
        Some(res) => {
            // C: if (!(res >= 0 ? res - delta <= INT_MAX : INT_MIN + delta <= res))
            //        return luaL_error(L, "field '%s' is out-of-bound", key);
            let in_bounds = if res >= 0 {
                res.saturating_sub(delta as i64) <= (i32::MAX as i64)
            } else {
                (i32::MIN as i64).saturating_add(delta as i64) <= res
            };
            if !in_bounds {
                state.pop_n(1);
                return Err(LuaError::runtime(format_args!(
                    "field '{}' is out-of-bound",
                    ByteDisplay(key),
                )));
            }
            // C: res -= delta;
            (res - delta as i64) as i32
        }
        None => {
            // C: if (l_unlikely(t != LUA_TNIL))
            if !matches!(ty, LuaType::Nil) {
                state.pop_n(1);
                // C: return luaL_error(L, "field '%s' is not an integer", key);
                return Err(LuaError::runtime(format_args!(
                    "field '{}' is not an integer",
                    ByteDisplay(key),
                )));
            } else if d < 0 {
                state.pop_n(1);
                // C: return luaL_error(L, "field '%s' missing in date table", key);
                return Err(LuaError::runtime(format_args!(
                    "field '{}' missing in date table",
                    ByteDisplay(key),
                )));
            }
            d
        }
    };
    // C: lua_pop(L, 1);
    state.pop_n(1);
    Ok(res)
}

/// C: `static const char *checkoption(lua_State *L, const char *conv,
///                                     ptrdiff_t convlen, char *buff)`
///
/// Validates the `strftime` conversion specifier at the start of `conv` against
/// `STRFTIME_OPTIONS`.
///
/// `cc` must have `cc[0] == b'%'` on entry (set by the caller).  On success the
/// matched specifier bytes are written into `cc[1..=oplen]`, a null terminator is
/// written at `cc[oplen+1]`, and the sub-slice of `conv` after the consumed
/// specifier is returned.
///
/// On failure a `LuaError::arg_error` describing the invalid specifier is
/// returned.
///
/// The options table uses `|` characters as length-transition markers: one `|`
/// increments `oplen` from 1 to 2 (and the following advance jumps past the `||`
/// sentinel), enabling 2-char specifiers like `%EC`.
fn check_strftime_option<'a>(
    state: &mut LuaState,
    conv: &'a [u8],
    cc: &mut [u8; 4],
) -> Result<&'a [u8], LuaError> {
    let options = STRFTIME_OPTIONS;
    let mut oplen: usize = 1;
    let mut i: usize = 0;

    // C: for (; *option != '\0' && oplen <= convlen; option += oplen)
    while i < options.len() && oplen <= conv.len() {
        if options[i] == b'|' {
            // C: if (*option == '|') oplen++;  then option advances by new oplen
            // Increment first so the subsequent `i += oplen` uses the new value,
            // which jumps from the first `|` past the entire `||` separator block.
            oplen += 1;
            i += oplen;
        } else if i + oplen <= options.len() && conv[..oplen] == options[i..i + oplen] {
            // C: memcpy(buff, conv, oplen); buff[oplen] = '\0';
            // cc[0] = b'%' is pre-filled; write specifier bytes into cc[1..=oplen].
            debug_assert!(oplen <= 2, "STRFTIME_OPTIONS only has 1- and 2-char specifiers");
            cc[1..=oplen].copy_from_slice(&conv[..oplen]);
            cc[oplen + 1] = 0;
            // C: return conv + oplen;
            return Ok(&conv[oplen..]);
        } else {
            // C: option += oplen  (advance to the next entry in the options list)
            i += oplen;
        }
    }
    // C: luaL_argerror(L, 1, lua_pushfstring(L, "invalid conversion specifier '%%%s'", conv));
    Err(LuaError::arg_error(
        state,
        1,
        format_args!("invalid conversion specifier '%{}'", ByteDisplay(conv)),
    ))
}

/// C: `static time_t l_checktime(lua_State *L, int arg)`
///
/// Reads argument `arg` as a Lua integer and returns it as a Unix timestamp.
///
/// PORT NOTE: On 64-bit targets `time_t == i64 == lua_Integer`, so the range
/// check in the C original (`(time_t)t == t`) is always satisfied.
/// TODO(port): On hypothetical 32-bit `time_t` platforms the check would need
/// to narrow `t` to `i32` and verify no truncation; flag for Phase B.
fn check_time(state: &mut LuaState, arg: i32) -> Result<i64, LuaError> {
    // C: l_timet t = l_gettime(L, arg);  where l_timet = lua_Integer = i64
    let t = state.check_arg_integer(arg)?;
    // C: luaL_argcheck(L, (time_t)t == t, arg, "time out-of-bounds");
    Ok(t)
}

// ── Library functions ─────────────────────────────────────────────────────────

/// C: `static int os_execute(lua_State *L)`
///
/// Executes a shell command via the system shell.
/// Without arguments: returns a boolean indicating whether a shell is available.
/// With a command string: returns `true` on success, or `nil, errmsg, exitcode`
/// on failure.
pub(crate) fn os_execute(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: const char *cmd = luaL_optstring(L, 1, NULL);
    let cmd = state.opt_arg_string(1)?;
    // TODO(port): `system()` / `l_system()` requires `std::process::Command` which
    // is banned in lua-stdlib per PORTING.md.  Route through an OS-capability
    // abstraction layer provided by lua-cli or a platform-hook in Phase B.
    match cmd {
        None => {
            // C: lua_pushboolean(L, stat);  /* true if there is a shell */
            // Stub: report that no shell is available until capability is wired.
            state.push(LuaValue::Bool(false));
            Ok(1)
        }
        Some(_cmd_bytes) => {
            // C: return luaL_execresult(L, stat);
            state.push(LuaValue::Nil);
            state.push_string(b"os.execute: not implemented in lua-stdlib")?;
            state.push(LuaValue::Int(-1));
            Ok(3)
        }
    }
}

/// C: `static int os_remove(lua_State *L)`
///
/// Removes the file or empty directory at the given path.
/// Returns `true` on success, or `nil, errmsg` on failure.
pub(crate) fn os_remove(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: const char *filename = luaL_checkstring(L, 1);
    let _filename: Vec<u8> = state.check_arg_string(1)?.to_vec();
    // TODO(port): C `remove()` maps to `std::fs::remove_file` / `remove_dir`,
    // both banned in lua-stdlib.  Phase B needs an OS-capability abstraction.
    state.push(LuaValue::Nil);
    state.push_string(b"os.remove: not implemented in lua-stdlib")?;
    Ok(2)
}

/// C: `static int os_rename(lua_State *L)`
///
/// Renames (moves) a file from the first path to the second.
/// Returns `true` on success, or `nil, errmsg` on failure.
pub(crate) fn os_rename(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: const char *fromname = luaL_checkstring(L, 1);
    let _fromname: Vec<u8> = state.check_arg_string(1)?.to_vec();
    // C: const char *toname = luaL_checkstring(L, 2);
    let _toname: Vec<u8> = state.check_arg_string(2)?.to_vec();
    // TODO(port): C `rename()` maps to `std::fs::rename`, banned in lua-stdlib.
    // Phase B needs an OS-capability abstraction.
    state.push(LuaValue::Nil);
    state.push_string(b"os.rename: not implemented in lua-stdlib")?;
    Ok(2)
}

/// C: `static int os_tmpname(lua_State *L)`
///
/// Generates a unique temporary file name and pushes it as a string.
/// Raises a runtime error if generation fails.
pub(crate) fn os_tmpname(_state: &mut LuaState) -> Result<usize, LuaError> {
    // C: char buff[LUA_TMPNAMBUFSIZE]; int err; lua_tmpnam(buff, err);
    // TODO(port): `tmpnam` / `mkstemp` require OS file-system access banned from
    // lua-stdlib.  Phase B should implement via a capability hook supplied by
    // the host (e.g. lua-cli registers a tmpnam provider).
    Err(LuaError::runtime(format_args!(
        "unable to generate a unique filename"
    )))
}

/// C: `static int os_getenv(lua_State *L)`
///
/// Reads the environment variable named by the first argument and pushes its
/// value as a string, or `nil` if the variable is not set.
pub(crate) fn os_getenv(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: lua_pushstring(L, getenv(luaL_checkstring(L, 1)));  /* if NULL push nil */
    let name_bytes: Vec<u8> = state.check_arg_string(1)?.to_vec();

    // PORT NOTE: On Unix, environment variable names are arbitrary byte sequences
    // and `OsStr::from_bytes` (from `std::os::unix::ffi::OsStrExt`) is the
    // byte-exact approach.  On Windows they must be valid UTF-16.  Phase B should
    // use the platform-appropriate OsStr API.
    //
    // TODO(port): Replace the cfg-guarded from_utf8 fallback on non-Unix targets
    // with proper wide-string handling.
    #[cfg(unix)]
    let result: Option<Vec<u8>> = {
        use std::ffi::OsStr;
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let os_name = OsStr::from_bytes(&name_bytes);
        std::env::var_os(os_name).map(|v| v.into_vec())
    };

    #[cfg(not(unix))]
    let result: Option<Vec<u8>> = {
        // TODO(port): from_utf8 used on Lua string data for OS API interop on
        // non-Unix platforms.  Ideally replaced with wide-string conversion.
        match std::str::from_utf8(&name_bytes) {
            Ok(name_str) => std::env::var(name_str).ok().map(|v| v.into_bytes()),
            Err(_) => None,
        }
    };

    match result {
        Some(val) => {
            // C: lua_pushstring(L, ...) — push the value as a Lua string
            state.push_string(&val)?;
        }
        None => {
            // C: getenv returns NULL → lua_pushstring pushes nil
            state.push(LuaValue::Nil);
        }
    }
    Ok(1)
}

/// C: `static int os_clock(lua_State *L)`
///
/// Returns an approximation of the CPU time (in seconds) used by the program.
pub(crate) fn os_clock(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: lua_pushnumber(L, ((lua_Number)clock())/(lua_Number)CLOCKS_PER_SEC);
    // TODO(port): C's `clock()` returns process CPU time, not wall-clock time.
    // `std::time::Instant` provides wall time only.  On POSIX targets, use
    // `libc::clock()` / `libc::CLOCKS_PER_SEC` via the `libc` crate.
    // Phase A stub returns 0.0.
    state.push(LuaValue::Float(0.0));
    Ok(1)
}

/// C: `static int os_date(lua_State *L)`
///
/// Formats the current (or a specified) date/time.
///
/// * Format starting with `'!'` → use UTC; otherwise local time.
/// * Format `"*t"` → push a table with broken-down time fields.
/// * Other format → push a formatted string, expanding `%`-specifiers via
///   the C-library `strftime`.
pub(crate) fn os_date(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: size_t slen; const char *s = luaL_optlstring(L, 1, "%c", &slen);
    // Clone to Vec<u8> so that `s` does not borrow from `state`.
    let format: Vec<u8> = state.opt_arg_lstring(1, b"%c")?.to_vec();
    let s: &[u8] = &format[..];

    // C: time_t t = luaL_opt(L, l_checktime, 2, time(NULL));
    // TODO(port): replace stub `0` with `std::time::SystemTime::now()` converted
    // to a Unix timestamp (seconds since epoch) in Phase B.
    let _t: i64 = if matches!(state.type_at(2), LuaType::None | LuaType::Nil) {
        0i64 // Phase A stub for time(NULL)
    } else {
        check_time(state, 2)?
    };

    // C: if (*s == '!') { stm = l_gmtime(&t, &tmr); s++; }
    // C: else              stm = l_localtime(&t, &tmr);
    let (use_utc, s): (bool, &[u8]) = if s.first() == Some(&b'!') {
        (true, &s[1..])
    } else {
        (false, s)
    };

    // TODO(port): decompose Unix timestamp `_t` into TmFields using `gmtime_r`
    // (when use_utc) or `localtime_r`.  Requires `libc` or `chrono`.
    // Phase A stub: zero-initialised TmFields.
    let _ = use_utc;
    let stm = TmFields::default();

    // C: if (stm == NULL)
    //      return luaL_error(L, "date result cannot be represented in this installation");
    // (Phase A stub is always valid — no null check needed.)

    if s == b"*t" {
        // C: lua_createtable(L, 0, 9);  /* 9 = number of fields */
        state.create_table(0, 9)?;
        // C: setallfields(L, stm);
        set_all_fields(state, &stm)?;
    } else {
        // C: builds formatted string using luaL_Buffer and per-specifier strftime calls
        let mut result: Vec<u8> = Vec::new();
        let mut pos: usize = 0;

        // C: while (s < se)
        while pos < s.len() {
            if s[pos] != b'%' {
                // C: luaL_addchar(&b, *s++);
                result.push(s[pos]);
                pos += 1;
            } else {
                // C: s++;  /* skip '%' */
                pos += 1;
                if pos >= s.len() {
                    break;
                }
                // C: char cc[4]; cc[0] = '%';
                let mut cc = [0u8; 4];
                cc[0] = b'%';
                let conv = &s[pos..];
                // C: s = checkoption(L, s, se - s, cc + 1);
                let after = check_strftime_option(state, conv, &mut cc)?;
                let oplen = conv.len() - after.len();
                pos += oplen;
                // C: reslen = strftime(buff, SIZETIMEFMT, cc, stm);
                // C: luaL_addsize(&b, reslen);
                // TODO(port): call platform `strftime` with `cc` as format string
                // and `stm` fields.  Phase A stub emits nothing for each specifier.
                let _ = SIZE_TIME_FMT;
            }
        }
        // C: luaL_pushresult(&b);
        state.push_string(&result)?;
    }
    Ok(1)
}

/// C: `static int os_time(lua_State *L)`
///
/// Without arguments: returns the current time as a Unix timestamp (integer).
/// With a table argument: interprets the table as broken-down local time,
/// normalises the fields via `mktime`, updates the table in place, and returns
/// the resulting timestamp.
pub(crate) fn os_time(state: &mut LuaState) -> Result<usize, LuaError> {
    let t: i64;

    // C: if (lua_isnoneornil(L, 1)) { t = time(NULL); }
    if matches!(state.type_at(1), LuaType::None | LuaType::Nil) {
        // TODO(port): use `std::time::SystemTime::now()` to obtain a Unix timestamp.
        t = 0; // Phase A stub
    } else {
        // C: luaL_checktype(L, 1, LUA_TTABLE);
        state.check_arg_type(1, LuaType::Table)?;
        // C: lua_settop(L, 1);  /* make sure table is at the top */
        state.set_top(1);

        // C: ts.tm_year = getfield(L, "year",  -1, 1900);
        let tm_year  = get_field(state, b"year",  -1, 1900)?;
        // C: ts.tm_mon  = getfield(L, "month", -1, 1);
        let tm_mon   = get_field(state, b"month", -1, 1)?;
        // C: ts.tm_mday = getfield(L, "day",   -1, 0);
        let tm_mday  = get_field(state, b"day",   -1, 0)?;
        // C: ts.tm_hour = getfield(L, "hour",  12, 0);
        let tm_hour  = get_field(state, b"hour",  12, 0)?;
        // C: ts.tm_min  = getfield(L, "min",   0,  0);
        let tm_min   = get_field(state, b"min",   0,  0)?;
        // C: ts.tm_sec  = getfield(L, "sec",   0,  0);
        let tm_sec   = get_field(state, b"sec",   0,  0)?;
        // C: ts.tm_isdst = getboolfield(L, "isdst");
        let tm_isdst = get_bool_field(state, b"isdst")?;

        let stm = TmFields {
            tm_year,
            tm_mon,
            tm_mday,
            tm_hour,
            tm_min,
            tm_sec,
            tm_isdst,
            ..TmFields::default()
        };

        // C: t = mktime(&ts);
        // TODO(port): call `libc::mktime` to normalise stm fields and compute the
        // Unix timestamp.  Phase A stub uses 0 and skips normalisation.
        t = 0;

        // C: setallfields(L, &ts);  /* update fields with normalized values */
        // PORT NOTE: In the real implementation, mktime may alter stm (e.g.
        // normalise out-of-range values), so we update the table with the
        // post-normalisation stm.  Phase A stm is unmodified.
        set_all_fields(state, &stm)?;
    }

    // C: if (t != (time_t)(l_timet)t || t == (time_t)(-1))
    //        return luaL_error(L, "time result cannot be represented in this installation");
    // PORT NOTE: On 64-bit targets time_t == i64 == lua_Integer so the cast check
    // is a no-op.  We only guard against mktime's failure sentinel (−1).
    if t == -1 {
        return Err(LuaError::runtime(format_args!(
            "time result cannot be represented in this installation"
        )));
    }

    // C: l_pushtime(L, t);  where l_timet = lua_Integer → push as integer
    state.push(LuaValue::Int(t));
    Ok(1)
}

/// C: `static int os_difftime(lua_State *L)`
///
/// Returns the number of seconds between two time values as a float (`t1 − t2`).
///
/// PORT NOTE: C's `difftime(t1, t2)` returns `t1 − t2` as a `double`.  For
/// 64-bit `time_t` this is exact as `f64` up to approximately 2^53 seconds
/// (~285 million years), which is sufficient for all practical timestamps.
pub(crate) fn os_difftime(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: time_t t1 = l_checktime(L, 1);
    let t1 = check_time(state, 1)?;
    // C: time_t t2 = l_checktime(L, 2);
    let t2 = check_time(state, 2)?;
    // C: lua_pushnumber(L, (lua_Number)difftime(t1, t2));
    state.push(LuaValue::Float((t1 - t2) as f64));
    Ok(1)
}

/// C: `static int os_setlocale(lua_State *L)`
///
/// Sets the locale for the given category and pushes the resulting locale name
/// as a string, or `nil` on failure.
pub(crate) fn os_setlocale(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: static const char *const catnames[] = {"all","collate","ctype","monetary","numeric","time",NULL};
    const CAT_NAMES: &[&[u8]] = &[
        b"all", b"collate", b"ctype", b"monetary", b"numeric", b"time",
    ];

    // C: const char *l = luaL_optstring(L, 1, NULL);
    let _locale: Option<Vec<u8>> = state.opt_arg_string(1)?;

    // C: int op = luaL_checkoption(L, 2, "all", catnames);
    let _op: usize = state.check_arg_option(2, Some(b"all"), CAT_NAMES)?;

    // C: lua_pushstring(L, setlocale(cat[op], l));
    // TODO(port): call `libc::setlocale(cat[op], locale_ptr)` using the `libc` crate.
    // The LC_* constants and `setlocale` function are not available in Rust std.
    // Phase B should link against libc and implement this properly.
    state.push(LuaValue::Nil);
    Ok(1)
}

/// C: `static int os_exit(lua_State *L)`
///
/// Exits the host process with the given status code (default `EXIT_SUCCESS = 0`).
/// If the second argument is true, also closes the Lua state before exiting.
///
/// This function is expected to terminate the process and never return normally.
pub(crate) fn os_exit(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: if (lua_isboolean(L, 1))
    //      status = lua_toboolean(L, 1) ? EXIT_SUCCESS : EXIT_FAILURE;
    //    else
    //      status = (int)luaL_optinteger(L, 1, EXIT_SUCCESS);
    let exit_code: i32 = if matches!(state.type_at(1), LuaType::Boolean) {
        if state.to_boolean(1) { 0 } else { 1 } // EXIT_SUCCESS = 0, EXIT_FAILURE = 1
    } else {
        state.opt_arg_integer(1, 0)? as i32
    };

    // C: if (lua_toboolean(L, 2)) lua_close(L);
    if state.to_boolean(2) {
        // C: lua_close(L) — tear down the Lua state before exiting
        state.close();
    }

    // C: if (L) exit(status);
    // TODO(port): `std::process::exit(exit_code)` is banned in lua-stdlib per
    // PORTING.md.  The correct design is a `LuaError::Exit(i32)` variant (not yet
    // defined in lua-types) that `lua-cli`'s main loop intercepts and converts to
    // `std::process::exit`.  For Phase A, propagate as a status-coded error so the
    // call stack unwinds — the exit code is passed via `with_status` as a
    // placeholder (semantic mismatch intentional; flagged for Phase B).
    Err(LuaError::with_status(exit_code))
}

// ── Registration table and entry point ───────────────────────────────────────

/// Type alias for a Lua native function implementation in Rust.
///
/// TODO(port): align with the canonical `lua_CFunction` / `NativeFn` type defined
/// in `lua-types` once that crate stabilises.
pub type NativeFn = fn(&mut LuaState) -> Result<usize, LuaError>;

/// C: `static const luaL_Reg syslib[]`
///
/// Mapping from Lua-visible names to the Rust implementations of each `os.*`
/// function.
pub const OS_LIB: &[(&[u8], NativeFn)] = &[
    (b"clock",     os_clock),
    (b"date",      os_date),
    (b"difftime",  os_difftime),
    (b"execute",   os_execute),
    (b"exit",      os_exit),
    (b"getenv",    os_getenv),
    (b"remove",    os_remove),
    (b"rename",    os_rename),
    (b"setlocale", os_setlocale),
    (b"time",      os_time),
    (b"tmpname",   os_tmpname),
];

/// C: `LUAMOD_API int luaopen_os(lua_State *L)`
///
/// Opens the `os` library: creates a new table populated with `OS_LIB` and
/// leaves it on the stack.
///
/// PORT NOTE: `register_lib` is the Rust equivalent of `luaL_newlib`; it creates
/// a fresh table, fills it from the `(name, fn)` pair slice, and pushes it.
pub fn open_os(state: &mut LuaState) -> Result<usize, LuaError> {
    // C: luaL_newlib(L, syslib);
    state.register_lib(b"os", OS_LIB)?;
    Ok(1)
}

// ──────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        src/loslib.c  (430 lines, 12 functions)
//   target_crate:  lua-stdlib
//   confidence:    medium
//   todos:         15
//   port_notes:    4
//   unsafe_blocks: 0
//   notes:         Logic structure faithful; all OS calls that require banned
//                  imports (std::fs, std::process) are stubbed with TODO(port).
//                  Time formatting (os.date, os.time, os.clock) needs libc or
//                  chrono in Phase B.  os.exit needs a LuaError::Exit(i32)
//                  variant.  check_strftime_option logic is fully translated.
//                  os_getenv uses OsStr::from_bytes on Unix (no from_utf8).
// ──────────────────────────────────────────────────────────────────────────
