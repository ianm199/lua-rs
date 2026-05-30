//! Lua `os` standard library.
//!
//! Ports `src/loslib.c` (430 lines, 12 functions) to Rust.
//!
//! ## Platform access limitations
//!
//! Several `os.*` functions require OS-level capabilities. File removal,
//! rename, command execution, environment lookup, temporary-name generation,
//! and wall-clock access route through `GlobalState` hooks supplied by the
//! embedder where needed for sandboxed/WASM hosts.
//!
//! Time decomposition (`os.date`, `os.time`) requires C-library functions
//! (`gmtime_r`, `localtime_r`, `mktime`, `strftime`).  Those call sites are
//! flagged with `TODO(port)` and the stubs use a zero-initialised `TmFields`.

use lua_types::{LuaError, LuaExit, LuaType, LuaValue};
use crate::state_stub::{LuaState, LuaStateStubExt as _};
use lua_vm::state::OsExecuteReason;

// ── Constants ────────────────────────────────────────────────────────────────

//
// Valid `strftime` conversion specifiers — C99 / POSIX variant.
// Single-char specifiers appear first; the `||` sentinel signals the start
// of 2-char specifiers (e.g. `%EC`, `%Oy`).  See `check_strftime_option`.
const STRFTIME_OPTIONS: &[u8] =
    b"aAbBcCdDeFgGhHIjmMnprRStTuUVwWxXyYzZ%||EcECExEXEyEYOdOeOHOIOmOMOSOuOUOVOwOWOy";

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

///
/// Pushes `(value as i64) + (delta as i64)` as a Lua integer, then stores it
/// in the table currently on top of the stack at field `key`.
fn set_field(state: &mut LuaState, key: &[u8], value: i32, delta: i32) -> Result<(), LuaError> {
    state.push(LuaValue::Int((value as i64) + (delta as i64)));
    state.set_field(-2, key)?;
    Ok(())
}

///
/// Stores a boolean at field `key` in the table on top of the stack.
/// A negative `value` means "undefined" — the field is silently skipped.
fn set_bool_field(state: &mut LuaState, key: &[u8], value: i32) -> Result<(), LuaError> {
    if value < 0 {
        return Ok(());
    }
    state.push(LuaValue::Bool(value != 0));
    state.set_field(-2, key)?;
    Ok(())
}

///
/// Writes every field of `stm` into the table on top of the stack, applying the
/// offsets that convert from C-library conventions to Lua conventions:
/// year+1900, month+1, wday+1, yday+1.
fn set_all_fields(state: &mut LuaState, stm: &TmFields) -> Result<(), LuaError> {
    set_field(state, b"year",  stm.tm_year, 1900)?;
    set_field(state, b"month", stm.tm_mon,  1)?;
    set_field(state, b"day",   stm.tm_mday, 0)?;
    set_field(state, b"hour",  stm.tm_hour, 0)?;
    set_field(state, b"min",   stm.tm_min,  0)?;
    set_field(state, b"sec",   stm.tm_sec,  0)?;
    set_field(state, b"yday",  stm.tm_yday, 1)?;
    set_field(state, b"wday",  stm.tm_wday, 1)?;
    set_bool_field(state, b"isdst", stm.tm_isdst)?;
    Ok(())
}

///
/// Reads a boolean field from the table on top of the stack.
/// Returns `-1` when the field is absent (nil), or `0` / `1` for false / true.
fn get_bool_field(state: &mut LuaState, key: &[u8]) -> Result<i32, LuaError> {
    let ty = state.get_field(-1, key)?;
    let res = if matches!(ty, LuaType::Nil) {
        -1i32
    } else {
        state.to_boolean(-1) as i32
    };
    state.pop_n(1);
    Ok(res)
}

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
    let ty = state.get_field(-1, key)?;
    let maybe_int = state.to_integer_x(-1);
    let res: i32 = match maybe_int {
        Some(res) => {
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
            (res - delta as i64) as i32
        }
        None => {
            if !matches!(ty, LuaType::Nil) {
                state.pop_n(1);
                return Err(LuaError::runtime(format_args!(
                    "field '{}' is not an integer",
                    ByteDisplay(key),
                )));
            } else if d < 0 {
                state.pop_n(1);
                return Err(LuaError::runtime(format_args!(
                    "field '{}' missing in date table",
                    ByteDisplay(key),
                )));
            }
            d
        }
    };
    state.pop_n(1);
    Ok(res)
}

/// ptrdiff_t convlen, char *buff)`
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
    _state: &mut LuaState,
    conv: &'a [u8],
    cc: &mut [u8; 4],
) -> Result<&'a [u8], LuaError> {
    let options = STRFTIME_OPTIONS;
    let mut oplen: usize = 1;
    let mut i: usize = 0;

    while i < options.len() && oplen <= conv.len() {
        if options[i] == b'|' {
            // Increment first so the subsequent `i += oplen` uses the new value,
            // which jumps from the first `|` past the entire `||` separator block.
            oplen += 1;
            i += oplen;
        } else if i + oplen <= options.len() && conv[..oplen] == options[i..i + oplen] {
            // cc[0] = b'%' is pre-filled; write specifier bytes into cc[1..=oplen].
            debug_assert!(oplen <= 2, "STRFTIME_OPTIONS only has 1- and 2-char specifiers");
            cc[1..=oplen].copy_from_slice(&conv[..oplen]);
            cc[oplen + 1] = 0;
            return Ok(&conv[oplen..]);
        } else {
            i += oplen;
        }
    }
    Err(LuaError::arg_error(
        1,
        "invalid conversion specifier",
    ))
}

///
/// Reads argument `arg` as a Lua integer and returns it as a Unix timestamp.
///
/// PORT NOTE: On 64-bit targets `time_t == i64 == lua_Integer`, so the range
/// check in the C original (`(time_t)t == t`) is always satisfied.
/// TODO(port): On hypothetical 32-bit `time_t` platforms the check would need
/// to narrow `t` to `i32` and verify no truncation; flag for Phase B.
fn check_time(state: &mut LuaState, arg: i32) -> Result<i64, LuaError> {
    let t = state.check_arg_integer(arg)?;
    Ok(t)
}

/// Returns the current Unix timestamp (seconds since 1970-01-01 UTC).
fn unix_now(state: &LuaState) -> Result<i64, LuaError> {
    if let Some(now_fn) = state.global().unix_time_hook {
        return Ok(now_fn());
    }

    #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
    {
        let _ = state;
        return Err(LuaError::runtime(format_args!(
            "current time not available in this host"
        )));
    }

    #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
    {
        use std::time::{SystemTime, UNIX_EPOCH};
        Ok(SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0))
    }
}

/// Returns the host's local timezone offset (seconds) at instant `t`, such that
/// the local broken-down time equals `decompose_utc(t + offset)`.
///
/// Routes through `GlobalState::local_offset_hook` when the host installs one
/// (lua-cli does, via `localtime_r`). Absent a hook the offset is 0, so
/// `os.date`/`os.time` fall back to UTC — matching the prior behaviour and
/// keeping the round-trip exact under bare WASM.
fn local_offset(state: &LuaState, t: i64) -> i64 {
    match state.global().local_offset_hook {
        Some(off_fn) => off_fn(t),
        None => 0,
    }
}

fn native_temp_name() -> Result<Vec<u8>, LuaError> {
    #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
    {
        return Err(LuaError::runtime(format_args!(
            "temporary filenames not available in this host"
        )));
    }

    #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
    {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::{SystemTime, UNIX_EPOCH};

        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let mut dir: Vec<u8> = {
            let path = std::env::temp_dir();
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStrExt;
                path.as_os_str().as_bytes().to_vec()
            }
            #[cfg(not(unix))]
            {
                path.to_string_lossy().as_bytes().to_vec()
            }
        };
        if dir.last().copied() != Some(b'/') && dir.last().copied() != Some(b'\\') {
            dir.push(b'/');
        }

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);

        let suffix = format!("lua_{:x}_{:x}_{:x}", std::process::id(), nanos, n);
        dir.extend_from_slice(suffix.as_bytes());
        Ok(dir)
    }
}

fn host_temp_name(state: &LuaState) -> Result<Vec<u8>, LuaError> {
    match state.global().temp_name_hook {
        Some(temp_fn) => temp_fn(),
        None => native_temp_name(),
    }
}

/// Decompose a Unix timestamp (UTC) into broken-down time fields.
///
/// Uses Howard Hinnant's `civil_from_days` algorithm (public domain, see
/// <http://howardhinnant.github.io/date_algorithms.html#civil_from_days>),
/// which is exact for all `i64` inputs across the proleptic Gregorian calendar.
///
/// PORT NOTE: C uses `gmtime_r(&t, &tmr)`.  Pure-Rust replacement because the
/// crate forbids `unsafe` (required for libc FFI).  `tm_isdst` is always 0 for
/// UTC.  `tm_wday` is 0-based with Sunday = 0 (matches POSIX).  `tm_yday` is
/// 0-based (matches POSIX; `set_all_fields` adds 1 for the Lua-visible table).
fn decompose_utc(t: i64) -> TmFields {
    let days = t.div_euclid(86_400);
    let sod = t.rem_euclid(86_400) as i32;

    let tm_hour = sod / 3600;
    let tm_min = (sod / 60) % 60;
    let tm_sec = sod % 60;

    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }).div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy_mar = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy_mar + 2) / 153;
    let day = (doy_mar - (153 * mp + 2) / 5 + 1) as i32;
    let month: i32 = if mp < 10 { (mp + 3) as i32 } else { (mp - 9) as i32 };
    let year = y + if month <= 2 { 1 } else { 0 };

    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    const DAYS_BEFORE_MONTH: [i32; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    let tm_yday = DAYS_BEFORE_MONTH[(month - 1) as usize]
        + (day - 1)
        + if leap && month > 2 { 1 } else { 0 };

    let tm_wday = (days + 4).rem_euclid(7) as i32;

    TmFields {
        tm_sec,
        tm_min,
        tm_hour,
        tm_mday: day,
        tm_mon: month - 1,
        tm_year: (year - 1900) as i32,
        tm_wday,
        tm_yday,
        tm_isdst: 0,
    }
}

/// Compose a UTC Unix timestamp from broken-down time fields.
///
/// Inverse of `decompose_utc`.  Uses Howard Hinnant's `days_from_civil` and
/// normalises month overflow into the year (matching `mktime`'s behaviour for
/// the year/month axes).  Day-of-month, hour, minute, and second components
/// are added linearly so out-of-range values normalise carry into the larger
/// units exactly as `mktime` would for UTC.
fn compose_utc(tm: &TmFields) -> i64 {
    let mut y: i64 = (tm.tm_year as i64) + 1900;
    let mut m: i64 = (tm.tm_mon as i64) + 1;
    let dy = (m - 1).div_euclid(12);
    y += dy;
    m -= dy * 12;
    let y_adj = if m <= 2 { y - 1 } else { y };
    let era = (if y_adj >= 0 { y_adj } else { y_adj - 399 }).div_euclid(400);
    let yoe = y_adj - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + (tm.tm_mday as i64) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    days * 86_400 + (tm.tm_hour as i64) * 3600 + (tm.tm_min as i64) * 60 + (tm.tm_sec as i64)
}

/// Append the formatted result of a single `strftime` conversion specifier.
///
/// `cc` holds the canonical specifier bytes filled in by `check_strftime_option`:
/// `cc[0] == b'%'`, `cc[1]` is the leading specifier char, and for 2-char
/// specifiers `cc[2]` is the second char (an E/O modifier comes first in C, e.g.
/// `%Ex` → `cc = "%Ex\0"`).  `oplen` is 1 or 2.
///
/// PORT NOTE: C delegates to the platform `strftime`.  Pure-Rust replacement for
/// the same reason as `decompose_utc`.  The E/O modifiers are stripped (POSIX
/// allows the implementation to ignore them and fall back to the unmodified
/// form) — the test suite only requires that they not error.
fn strftime_one(buf: &mut Vec<u8>, cc: &[u8; 4], oplen: usize, tm: &TmFields) {
    use std::io::Write as _;
    let spec = if oplen == 2 { cc[2] } else { cc[1] };
    let year_full = (tm.tm_year as i64) + 1900;
    let hour12 = {
        let h = tm.tm_hour.rem_euclid(12);
        if h == 0 { 12 } else { h }
    };
    const DAY_SHORT: [&[u8]; 7] = [b"Sun", b"Mon", b"Tue", b"Wed", b"Thu", b"Fri", b"Sat"];
    const DAY_LONG: [&[u8]; 7] = [
        b"Sunday", b"Monday", b"Tuesday", b"Wednesday", b"Thursday", b"Friday", b"Saturday",
    ];
    const MON_SHORT: [&[u8]; 12] = [
        b"Jan", b"Feb", b"Mar", b"Apr", b"May", b"Jun", b"Jul", b"Aug", b"Sep", b"Oct", b"Nov",
        b"Dec",
    ];
    const MON_LONG: [&[u8]; 12] = [
        b"January", b"February", b"March", b"April", b"May", b"June", b"July", b"August",
        b"September", b"October", b"November", b"December",
    ];
    let wday_idx = tm.tm_wday.rem_euclid(7) as usize;
    let mon_idx = tm.tm_mon.rem_euclid(12) as usize;
    match spec {
        b'Y' => { let _ = write!(buf, "{}", year_full); }
        b'y' => { let _ = write!(buf, "{:02}", year_full.rem_euclid(100)); }
        b'C' => { let _ = write!(buf, "{:02}", year_full.div_euclid(100)); }
        b'm' => { let _ = write!(buf, "{:02}", tm.tm_mon + 1); }
        b'd' => { let _ = write!(buf, "{:02}", tm.tm_mday); }
        b'e' => { let _ = write!(buf, "{:2}", tm.tm_mday); }
        b'H' => { let _ = write!(buf, "{:02}", tm.tm_hour); }
        b'I' => { let _ = write!(buf, "{:02}", hour12); }
        b'k' => { let _ = write!(buf, "{:2}", tm.tm_hour); }
        b'l' => { let _ = write!(buf, "{:2}", hour12); }
        b'M' => { let _ = write!(buf, "{:02}", tm.tm_min); }
        b'S' => { let _ = write!(buf, "{:02}", tm.tm_sec); }
        b'w' => { let _ = write!(buf, "{}", tm.tm_wday); }
        b'u' => {
            let u = if tm.tm_wday == 0 { 7 } else { tm.tm_wday };
            let _ = write!(buf, "{}", u);
        }
        b'j' => { let _ = write!(buf, "{:03}", tm.tm_yday + 1); }
        b'a' => buf.extend_from_slice(DAY_SHORT[wday_idx]),
        b'A' => buf.extend_from_slice(DAY_LONG[wday_idx]),
        b'b' | b'h' => buf.extend_from_slice(MON_SHORT[mon_idx]),
        b'B' => buf.extend_from_slice(MON_LONG[mon_idx]),
        b'p' => buf.extend_from_slice(if tm.tm_hour < 12 { b"AM" } else { b"PM" }),
        b'P' => buf.extend_from_slice(if tm.tm_hour < 12 { b"am" } else { b"pm" }),
        b'D' | b'x' => {
            let _ = write!(buf, "{:02}/{:02}/{:02}", tm.tm_mon + 1, tm.tm_mday, year_full.rem_euclid(100));
        }
        b'F' => {
            let _ = write!(buf, "{}-{:02}-{:02}", year_full, tm.tm_mon + 1, tm.tm_mday);
        }
        b'T' | b'X' => {
            let _ = write!(buf, "{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec);
        }
        b'R' => { let _ = write!(buf, "{:02}:{:02}", tm.tm_hour, tm.tm_min); }
        b'r' => {
            let ampm: &[u8] = if tm.tm_hour < 12 { b"AM" } else { b"PM" };
            let _ = write!(buf, "{:02}:{:02}:{:02} ", hour12, tm.tm_min, tm.tm_sec);
            buf.extend_from_slice(ampm);
        }
        b'c' => {
            let _ = write!(
                buf,
                "{} {} {:2} {:02}:{:02}:{:02} {}",
                std::str::from_utf8(DAY_SHORT[wday_idx]).unwrap_or(""),
                std::str::from_utf8(MON_SHORT[mon_idx]).unwrap_or(""),
                tm.tm_mday,
                tm.tm_hour,
                tm.tm_min,
                tm.tm_sec,
                year_full,
            );
        }
        b'n' => buf.push(b'\n'),
        b't' => buf.push(b'\t'),
        b'%' => buf.push(b'%'),
        b'z' => buf.extend_from_slice(b"+0000"),
        b'Z' => buf.extend_from_slice(b"UTC"),
        b's' => { let _ = write!(buf, "{}", compose_utc(tm)); }
        b'U' => {
            let week = (tm.tm_yday + 7 - tm.tm_wday) / 7;
            let _ = write!(buf, "{:02}", week);
        }
        b'W' => {
            let mwday = if tm.tm_wday == 0 { 6 } else { tm.tm_wday - 1 };
            let week = (tm.tm_yday + 7 - mwday) / 7;
            let _ = write!(buf, "{:02}", week);
        }
        b'V' | b'g' | b'G' => {
            let _ = write!(buf, "{:02}", 1);
        }
        _ => {}
    }
}

// ── Library functions ─────────────────────────────────────────────────────────

///
/// Executes a shell command via the system shell.
///
/// Without arguments: tests whether a shell is available — returns `true`
/// when an `os_execute_hook` is installed (we always have `sh` in that case),
/// `false` otherwise.
///
/// With a command string: dispatches through `os_execute_hook` and pushes the
/// three C-Lua return values `(boolean|nil, "exit"|"signal", int)` as defined
/// by `luaL_execresult`.  Returns the stub `nil, errmsg, -1` triple when no
/// hook is installed.
pub(crate) fn os_execute(state: &mut LuaState) -> Result<usize, LuaError> {
    let cmd = state.opt_arg_lstring(1, None)?;
    match cmd {
        None => {
            // We have a shell if and only if the embedder installed a hook.
            let has_shell = state.global().os_execute_hook.is_some();
            state.push(LuaValue::Bool(has_shell));
            Ok(1)
        }
        Some(cmd_bytes) => {
            let hook = state.global().os_execute_hook;
            match hook {
                Some(execute_fn) => {
                    // Clone to avoid holding a borrow across the hook call.
                    let cmd_owned: Vec<u8> = cmd_bytes.to_vec();
                    match execute_fn(&cmd_owned) {
                        Ok(result) => {
                            if result.success {
                                state.push(LuaValue::Bool(true));
                            } else {
                                state.push(LuaValue::Nil);
                            }
                            let reason_str: &[u8] = match result.reason {
                                OsExecuteReason::Exit => b"exit",
                                OsExecuteReason::Signal => b"signal",
                            };
                            state.push_string(reason_str)?;
                            state.push(LuaValue::Int(result.code as i64));
                            Ok(3)
                        }
                        Err(e) => {
                            state.push(LuaValue::Nil);
                            let msg = match &e {
                                LuaError::Runtime(LuaValue::Str(s)) => s.as_bytes().to_vec(),
                                other => format!("{:?}", other).into_bytes(),
                            };
                            let s = state.intern_str(&msg)?;
                            state.push(LuaValue::Str(s));
                            state.push(LuaValue::Int(-1));
                            Ok(3)
                        }
                    }
                }
                None => {
                    state.push(LuaValue::Nil);
                    state.push_string(b"os.execute: not implemented in lua-stdlib")?;
                    state.push(LuaValue::Int(-1));
                    Ok(3)
                }
            }
        }
    }
}

///
/// Removes the file or empty directory at the given path.
/// Returns `true` on success, or `nil, errmsg` on failure.
pub(crate) fn os_remove(state: &mut LuaState) -> Result<usize, LuaError> {
    let filename: Vec<u8> = state.check_arg_string(1)?.to_vec();
    // `std::fs` is banned in lua-stdlib; delegate to the embedder hook.
    let hook = state.global().file_remove_hook;
    match hook {
        Some(remove_fn) => match remove_fn(&filename) {
            Ok(()) => {
                state.push(LuaValue::Bool(true));
                Ok(1)
            }
            Err(e) => {
                state.push(LuaValue::Nil);
                let msg = match &e {
                    LuaError::Runtime(LuaValue::Str(s)) => s.as_bytes().to_vec(),
                    other => format!("{:?}", other).into_bytes(),
                };
                let s = state.intern_str(&msg)?;
                state.push(LuaValue::Str(s));
                Ok(2)
            }
        },
        None => {
            state.push(LuaValue::Nil);
            state.push_string(b"os.remove: no filesystem hook registered")?;
            Ok(2)
        }
    }
}

///
/// Renames (moves) a file from the first path to the second.
/// Returns `true` on success, or `nil, errmsg` on failure.
pub(crate) fn os_rename(state: &mut LuaState) -> Result<usize, LuaError> {
    let fromname: Vec<u8> = state.check_arg_string(1)?.to_vec();
    let toname: Vec<u8> = state.check_arg_string(2)?.to_vec();
    // `std::fs` is banned in lua-stdlib; delegate to the embedder hook.
    let hook = state.global().file_rename_hook;
    match hook {
        Some(rename_fn) => match rename_fn(&fromname, &toname) {
            Ok(()) => {
                state.push(LuaValue::Bool(true));
                return Ok(1);
            }
            Err(e) => {
                state.push(LuaValue::Nil);
                let msg = match &e {
                    LuaError::Runtime(LuaValue::Str(s)) => s.as_bytes().to_vec(),
                    other => format!("{:?}", other).into_bytes(),
                };
                let s = state.intern_str(&msg)?;
                state.push(LuaValue::Str(s));
                return Ok(2);
            }
        },
        None => {}
    }
    state.push(LuaValue::Nil);
    state.push_string(b"os.rename: no filesystem hook registered")?;
    Ok(2)
}

///
/// Generates a unique temporary file name and pushes it as a string.
/// Raises a runtime error if generation fails.
///
/// PORT NOTE: Temporary names are host capability. Native hosts can install
/// `GlobalState::temp_name_hook`; bare WASM without that hook raises a Lua
/// error instead of touching `std::env` / `std::time` stubs.
pub(crate) fn os_tmpname(state: &mut LuaState) -> Result<usize, LuaError> {
    let dir = host_temp_name(state)?;
    state.push_string(&dir)?;
    Ok(1)
}

///
/// Reads the environment variable named by the first argument and pushes its
/// value as a string, or `nil` if the variable is not set.
pub(crate) fn os_getenv(state: &mut LuaState) -> Result<usize, LuaError> {
    let name_bytes: Vec<u8> = state.check_arg_string(1)?.to_vec();

    let result: Option<Vec<u8>> = match state.global().env_hook {
        Some(env_fn) => env_fn(&name_bytes),
        None => {
            #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
            {
                None
            }

            #[cfg(all(unix, not(all(target_arch = "wasm32", target_os = "unknown"))))]
            {
                use std::ffi::OsStr;
                use std::os::unix::ffi::{OsStrExt, OsStringExt};
                let os_name = OsStr::from_bytes(&name_bytes);
                std::env::var_os(os_name).map(|v| v.into_vec())
            }

            #[cfg(all(not(unix), not(all(target_arch = "wasm32", target_os = "unknown"))))]
            {
                // TODO(port): from_utf8 used on Lua string data for OS API interop on
                // non-Unix platforms.  Ideally replaced with wide-string conversion.
                match std::str::from_utf8(&name_bytes) {
                    Ok(name_str) => std::env::var(name_str).ok().map(|v| v.into_bytes()),
                    Err(_) => None,
                }
            }
        }
    };

    match result {
        Some(val) => {
            state.push_string(&val)?;
        }
        None => {
            state.push(LuaValue::Nil);
        }
    }
    Ok(1)
}

///
/// Returns an approximation of the CPU time (in seconds) used by the program.
pub(crate) fn os_clock(state: &mut LuaState) -> Result<usize, LuaError> {
    let seconds = cpu_seconds(state)?;
    state.push(LuaValue::Float(seconds));
    Ok(1)
}

/// Returns program CPU time in seconds, as consumed by `os.clock`.
///
/// C's `clock()` reads `CLOCK_PROCESS_CPUTIME_ID`, which has no portable `std`
/// equivalent. We route through `cpu_clock_hook` when the host installs one;
/// otherwise native builds report monotonic wall time elapsed since the first
/// call (the substitution wasi-libc and Emscripten make for `clock()`), and bare
/// WASM reports the clock as unavailable rather than touching a stubbed source.
fn cpu_seconds(state: &LuaState) -> Result<f64, LuaError> {
    if let Some(clock_fn) = state.global().cpu_clock_hook {
        return Ok(clock_fn());
    }

    #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
    {
        let _ = state;
        Err(LuaError::runtime(format_args!(
            "CPU clock not available in this host"
        )))
    }

    #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
    {
        let _ = state;
        use std::sync::OnceLock;
        use std::time::Instant;
        static START: OnceLock<Instant> = OnceLock::new();
        Ok(START.get_or_init(Instant::now).elapsed().as_secs_f64())
    }
}

///
/// Formats the current (or a specified) date/time.
///
/// * Format starting with `'!'` → use UTC; otherwise local time.
/// * Format `"*t"` → push a table with broken-down time fields.
/// * Other format → push a formatted string, expanding `%`-specifiers via
///   the C-library `strftime`.
pub(crate) fn os_date(state: &mut LuaState) -> Result<usize, LuaError> {
    // Clone to Vec<u8> so that `s` does not borrow from `state`.
    let format: Vec<u8> = state.opt_arg_lstring(1, Some(b"%c"))?.unwrap_or_default();
    let s: &[u8] = &format[..];

    let t: i64 = if matches!(state.type_at(2), LuaType::None | LuaType::Nil) {
        unix_now(state)?
    } else {
        check_time(state, 2)?
    };

    let (_use_utc, s): (bool, &[u8]) = if s.first() == Some(&b'!') {
        (true, &s[1..])
    } else {
        (false, s)
    };

    // PORT NOTE: C distinguishes UTC (`gmtime_r`) from local time (`localtime_r`).
    // The Rust port reproduces `localtime_r` by decomposing `t + offset`, where
    // `offset` is the host timezone offset at `t` supplied by the
    // `local_offset_hook` (lua-cli installs one via `localtime_r`; reading the
    // timezone database needs `libc` FFI, banned in `lua-stdlib`). Without a hook
    // the offset is 0 and local time degrades to UTC, keeping the
    // `os.date`/`os.time` round-trip exact under bare WASM. `'!'`-prefixed formats
    // request UTC explicitly and skip the offset.
    let offset = if _use_utc { 0 } else { local_offset(state, t) };
    let stm = decompose_utc(t + offset);

    //      return luaL_error(L, "date result cannot be represented in this installation");
    // (Phase A stub is always valid — no null check needed.)

    if s == b"*t" {
        state.create_table(0, 9)?;
        set_all_fields(state, &stm)?;
    } else {
        let mut result: Vec<u8> = Vec::new();
        let mut pos: usize = 0;

        while pos < s.len() {
            if s[pos] != b'%' {
                result.push(s[pos]);
                pos += 1;
            } else {
                pos += 1;
                let mut cc = [0u8; 4];
                cc[0] = b'%';
                // Pass the remaining slice even if empty: checkoption's loop
                // condition (oplen <= convlen) fails immediately on an empty
                // slice, which causes it to raise "invalid conversion specifier"
                // matching C behaviour for a trailing bare '%'.
                let conv = &s[pos..];
                let after = check_strftime_option(state, conv, &mut cc)?;
                let oplen = conv.len() - after.len();
                pos += oplen;
                // The `%%` specifier is data-independent: strftime emits a literal
                // `%` byte regardless of the broken-down time, so it is correct to
                // handle here even while the rest of strftime is stubbed.
                strftime_one(&mut result, &cc, oplen, &stm);
                let _ = SIZE_TIME_FMT;
            }
        }
        state.push_string(&result)?;
    }
    Ok(1)
}

///
/// Without arguments: returns the current time as a Unix timestamp (integer).
/// With a table argument: interprets the table as broken-down local time,
/// normalises the fields via `mktime`, updates the table in place, and returns
/// the resulting timestamp.
pub(crate) fn os_time(state: &mut LuaState) -> Result<usize, LuaError> {
    let t: i64;

    if matches!(state.type_at(1), LuaType::None | LuaType::Nil) {
        t = unix_now(state)?;
    } else {
        state.check_arg_type(1, LuaType::Table)?;
        // PORT NOTE: must use the public-API `set_top` (relative to the current
        // C-frame's `func`), not `LuaState::set_top` which is an inherent that
        // sets an absolute stack index and would truncate the entire stack.
        lua_vm::api::set_top(state, 1)?;

        let tm_year  = get_field(state, b"year",  -1, 1900)?;
        let tm_mon   = get_field(state, b"month", -1, 1)?;
        let tm_mday  = get_field(state, b"day",   -1, 0)?;
        let tm_hour  = get_field(state, b"hour",  12, 0)?;
        let tm_min   = get_field(state, b"min",   0,  0)?;
        let tm_sec   = get_field(state, b"sec",   0,  0)?;
        let tm_isdst = get_bool_field(state, b"isdst")?;

        let raw = TmFields {
            tm_year,
            tm_mon,
            tm_mday,
            tm_hour,
            tm_min,
            tm_sec,
            tm_isdst,
            ..TmFields::default()
        };

        // PORT NOTE: C `mktime` interprets the broken-down time as LOCAL and
        // returns the corresponding UTC timestamp. We reproduce it: treat the
        // fields as UTC to get a provisional `t_utc` (this also normalises the
        // month axis), then subtract the host timezone offset to recover the true
        // UTC instant. The offset is sampled at `t_utc` then re-sampled at the
        // corrected instant — the standard `mktime` fixed-point step — so the
        // result is correct except across a DST transition inside the offset
        // window, which `os.time`'s test inputs do not exercise. Without a hook
        // the offset is 0 and this is the exact inverse of `os.date`'s local
        // decomposition, so the `os.time(os.date("*t")) == t` round-trip holds.
        let t_utc = compose_utc(&raw);
        let off0 = local_offset(state, t_utc);
        let off = local_offset(state, t_utc - off0);
        t = t_utc - off;
        let stm = decompose_utc(t + off);

        set_all_fields(state, &stm)?;
    }

    //        return luaL_error(L, "time result cannot be represented in this installation");
    // PORT NOTE: On 64-bit targets time_t == i64 == lua_Integer so the cast check
    // is a no-op.  We only guard against mktime's failure sentinel (−1).
    if t == -1 {
        return Err(LuaError::runtime(format_args!(
            "time result cannot be represented in this installation"
        )));
    }

    state.push(LuaValue::Int(t));
    Ok(1)
}

///
/// Returns the number of seconds between two time values as a float (`t1 − t2`).
///
/// PORT NOTE: C's `difftime(t1, t2)` returns `t1 − t2` as a `double`.  For
/// 64-bit `time_t` this is exact as `f64` up to approximately 2^53 seconds
/// (~285 million years), which is sufficient for all practical timestamps.
pub(crate) fn os_difftime(state: &mut LuaState) -> Result<usize, LuaError> {
    let t1 = check_time(state, 1)?;
    let t2 = check_time(state, 2)?;
    state.push(LuaValue::Float((t1 - t2) as f64));
    Ok(1)
}

///
/// Sets the locale for the given category and pushes the resulting locale name
/// as a string, or `nil` on failure.
pub(crate) fn os_setlocale(state: &mut LuaState) -> Result<usize, LuaError> {
    const CAT_NAMES: &[&[u8]] = &[
        b"all", b"collate", b"ctype", b"monetary", b"numeric", b"time",
    ];

    let locale: Option<Vec<u8>> = state.opt_arg_lstring(1, None)?;

    let _op: usize = state.check_arg_option(2, Some(b"all"), CAT_NAMES)?;

    // PORT NOTE: calling libc::setlocale requires unsafe (banned in lua-stdlib, budget=0).
    // Rust programs inherit the "C" locale by default and never change it, so returning
    // "C" for the C locale (and nil for anything else) is faithful for this build:
    // "C" is the only locale guaranteed available on every POSIX system.
    let result_locale: Option<&[u8]> = match locale.as_deref() {
        None => Some(b"C"),          // query: return current locale (always "C" here)
        Some(b"C") | Some(b"POSIX") => Some(b"C"),  // setting to "C"/"POSIX" always succeeds
        Some(_) => None,             // any other locale: unsupported in this build
    };
    match result_locale {
        Some(s) => { state.push_string(s)?; }
        None => state.push(LuaValue::Nil),
    }
    Ok(1)
}

///
/// Exits the host process with the given status code (default `EXIT_SUCCESS = 0`).
/// If the second argument is true, also closes the Lua state before exiting.
///
/// This function is expected to terminate the process and never return normally.
pub(crate) fn os_exit(state: &mut LuaState) -> Result<usize, LuaError> {
    //      status = lua_toboolean(L, 1) ? EXIT_SUCCESS : EXIT_FAILURE;
    //    else
    //      status = (int)luaL_optinteger(L, 1, EXIT_SUCCESS);
    let exit_code: i32 = if matches!(state.type_at(1), LuaType::Boolean) {
        if state.to_boolean(1) { 0 } else { 1 } // EXIT_SUCCESS = 0, EXIT_FAILURE = 1
    } else {
        state.opt_arg_integer(1, 0)? as i32
    };

    if state.to_boolean(2) {
        state.close();
    }

    //
    // `std::process::exit` remains restricted to `lua-cli`. A regular
    // `LuaError` is also wrong here: Lua `pcall` must not catch `os.exit`.
    // Use a typed panic payload as internal non-local control flow; the CLI
    // catches it at the process boundary and converts it to an `ExitCode`.
    std::panic::panic_any(LuaExit(exit_code));
}

// ── Registration table and entry point ───────────────────────────────────────

/// Type alias for a Lua native function implementation in Rust.
///
/// TODO(port): align with the canonical `lua_CFunction` / `NativeFn` type defined
/// in `lua-types` once that crate stabilises.
pub type NativeFn = fn(&mut LuaState) -> Result<usize, LuaError>;

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

///
/// Opens the `os` library: creates a new table populated with `OS_LIB` and
/// leaves it on the stack.
///
/// PORT NOTE: `register_lib` is the Rust equivalent of `luaL_newlib`; it creates
/// a fresh table, fills it from the `(name, fn)` pair slice, and pushes it.
pub fn open_os(state: &mut LuaState) -> Result<usize, LuaError> {
    state.register_lib(b"os", OS_LIB)?;
    Ok(1)
}

// ──────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        src/loslib.c  (430 lines, 12 functions)
//   target_crate:  lua-stdlib
//   confidence:    medium
//   todos:         18
//   port_notes:    4
//   unsafe_blocks: 0
//   notes:         Logic structure faithful. File/process/env/temp/time
//                  operations route through host hooks where they need OS
//                  capabilities for sandboxed and bare-WASM hosts.
//                  Time formatting (os.date, os.time) needs libc or chrono in
//                  Phase B.  os.clock routes through cpu_clock_hook with a
//                  monotonic-wall fallback (no std CPU-time source).
//                  os.exit needs a LuaError::Exit(i32)
//                  variant.  check_strftime_option logic is fully translated.
//                  os_getenv uses OsStr::from_bytes on Unix (no from_utf8).
// ──────────────────────────────────────────────────────────────────────────
