//! Fast in-memory kit for issue #301 (`io.open`/`os.remove`/`os.rename`/
//! `io.input` errno + message fidelity).
//!
//! The end-to-end oracle (`specs/oracle/diff_one.sh` against the reference
//! binaries) is the truth-teller for the exact triple shape, but it needs a
//! real failing filesystem path and a subprocess per check. This kit installs
//! `HostHooks` that fail deterministically with a chosen `io::Error` (no real
//! disk I/O, no process spawn) and drives the io/os entry points straight
//! through the SAME mapping path the fix touches: hook return ->
//! `lua-stdlib`'s `file_result`/`os_remove`/`os_rename`/`opencheck` -> the Lua
//! failure result. Milliseconds per test, 100% reproducible.
//!
//! Two axes are covered:
//!   * **raw-OS failures** (`from_raw_os_error`) — the faithful path. Their
//!     errno/message expectations are `strerror`-shaped and therefore
//!     Unix-specific, so those tests are `#[cfg(unix)]`. Every expected string
//!     was cross-checked against the version-suffixed reference binaries
//!     (`/tmp/lua-refs/bin/lua5.x`) for the same simulated errno.
//!   * **non-OS failures** (a `raw_os_error`-less `io::Error`) and **success** —
//!     platform-independent structural checks (arity, "not errno 0", the
//!     success value). These guard the #301 no-fallback rule: a non-OS error
//!     must NOT be coerced to errno 0.

use std::cell::{Cell, RefCell};
use std::io;
use std::io::SeekFrom;

use omnilua::{HostHooks, Lua, LuaFileHandle, LuaVersion};

/// How the installed hooks should fail (or succeed) for the current test.
#[derive(Clone, Copy)]
enum FailMode {
    /// Fail with `io::Error::from_raw_os_error(code)` — carries a real errno.
    RawErrno(i32),
    /// Fail with a `raw_os_error`-less error — no errno to report.
    NonRaw,
    /// Succeed (`Ok`).
    Success,
}

thread_local! {
    /// The failure mode every installed hook in this file honours. A
    /// thread-local, not a closure capture, because the hook types are bare
    /// `fn` pointers (mirrors `io_strengthen.rs`'s `SCRATCH_PATH` pattern).
    /// Each test runs on its own thread under the cargo test harness.
    static MODE: Cell<FailMode> = const { Cell::new(FailMode::RawErrno(0)) };
}

fn set_mode(mode: FailMode) {
    MODE.with(|c| c.set(mode));
}

fn current_err() -> io::Result<()> {
    match MODE.with(Cell::get) {
        FailMode::RawErrno(code) => Err(io::Error::from_raw_os_error(code)),
        FailMode::NonRaw => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "synthetic capability failure",
        )),
        FailMode::Success => Ok(()),
    }
}

fn failing_open(_filename: &[u8], _mode: &[u8]) -> io::Result<Box<dyn LuaFileHandle>> {
    current_err()?;
    unreachable!("Success mode is not exercised on the open path (needs a real handle)")
}

fn failing_remove(_filename: &[u8]) -> io::Result<()> {
    current_err()
}

fn failing_rename(_from: &[u8], _to: &[u8]) -> io::Result<()> {
    current_err()
}

fn lua_with_failing_fs(version: LuaVersion) -> Lua {
    let hooks = HostHooks::new()
        .file_open(failing_open)
        .file_remove(failing_remove)
        .file_rename(failing_rename);
    Lua::with_hooks_versioned(hooks, version).expect("init lua with failing fs hooks")
}

/// Run `code` under `version` and return the single string it evaluates to.
/// The Lua program itself renders whatever it observes to a string, matching
/// the pattern established by `io_strengthen.rs::eval_str`.
fn eval_str(version: LuaVersion, code: &str) -> String {
    let lua = lua_with_failing_fs(version);
    lua.load(code)
        .set_name(b"=errno_kit")
        .eval()
        .unwrap_or_else(|e| panic!("eval of `{code}` failed under {version:?}: {e:?}"))
}

const ALL: [LuaVersion; 5] = [
    LuaVersion::V51,
    LuaVersion::V52,
    LuaVersion::V53,
    LuaVersion::V54,
    LuaVersion::V55,
];

#[cfg(unix)]
const ENOENT: i32 = 2;
#[cfg(unix)]
const EACCES: i32 = 13;

/// Render `(ok, msg, errno, count)` so a test can assert on the exact values
/// AND the arity — the arity distinguishes the faithful 3-value triple from
/// the honest 2-value (nil, msg) result when there is no errno.
fn triple_code(call: &str) -> String {
    format!(
        "local ok, msg, errno = {call}; \
         return tostring(ok) .. '|' .. tostring(msg) .. '|' .. tostring(errno) \
         .. '|' .. tostring(select('#', {call}))"
    )
}

// ── raw-OS failures: faithful errno + clean strerror message (Unix) ──────────

#[cfg(unix)]
#[test]
fn io_open_reports_enoent_errno_and_clean_message() {
    set_mode(FailMode::RawErrno(ENOENT));
    let code = triple_code("io.open('/nonexistent/x')");
    for v in ALL {
        let got = eval_str(v, &code);
        assert_eq!(
            got, "nil|/nonexistent/x: No such file or directory|2|3",
            "{v:?}: io.open ENOENT should be the faithful 3-value triple with errno 2 and a \
             clean strerror message, got `{got}`"
        );
    }
}

#[cfg(unix)]
#[test]
fn io_open_reports_eacces_errno_and_clean_message() {
    set_mode(FailMode::RawErrno(EACCES));
    let code = triple_code("io.open('/root/protected')");
    for v in ALL {
        let got = eval_str(v, &code);
        assert_eq!(
            got, "nil|/root/protected: Permission denied|13|3",
            "{v:?}: io.open EACCES should carry errno 13 and a clean strerror message, got `{got}`"
        );
    }
}

#[cfg(unix)]
#[test]
fn os_remove_reports_enoent_errno_and_clean_message() {
    set_mode(FailMode::RawErrno(ENOENT));
    let code = triple_code("os.remove('/nonexistent/x')");
    for v in ALL {
        let got = eval_str(v, &code);
        assert_eq!(
            got, "nil|/nonexistent/x: No such file or directory|2|3",
            "{v:?}: os.remove ENOENT should carry errno 2 and a clean strerror message, got `{got}`"
        );
    }
}

/// Verified against every reference binary (5.1-5.5): only Lua 5.1 prefixes
/// the failure message with the source filename; 5.2 onward report bare
/// `strerror` text with no prefix.
#[cfg(unix)]
#[test]
fn os_rename_reports_enoent_errno_and_version_gated_message() {
    set_mode(FailMode::RawErrno(ENOENT));
    let code = triple_code("os.rename('/nonexistent/x', 'y')");
    for v in ALL {
        let got = eval_str(v, &code);
        let expected = match v {
            LuaVersion::V51 => "nil|/nonexistent/x: No such file or directory|2|3",
            _ => "nil|No such file or directory|2|3",
        };
        assert_eq!(
            got, expected,
            "{v:?}: os.rename ENOENT should carry errno 2, got `{got}`"
        );
    }
}

/// `io.input` on a bad file raises (it does not return a triple). 5.1 uses
/// `fileerror`/`luaL_argerror` (`bad argument #1 to '?' (<file>: <strerror>)`
/// under `pcall`), 5.2+ use `opencheck`'s `luaL_error`
/// (`cannot open file '<file>' (<strerror>)`). Pinned against every reference
/// binary.
#[cfg(unix)]
#[test]
fn io_input_open_failure_is_version_gated() {
    set_mode(FailMode::RawErrno(ENOENT));
    let code = "local ok, err = pcall(io.input, '/nonexistent/x'); return tostring(err)";
    for v in ALL {
        let got = eval_str(v, code);
        let expected = match v {
            LuaVersion::V51 => "bad argument #1 to '?' (/nonexistent/x: No such file or directory)",
            _ => "cannot open file '/nonexistent/x' (No such file or directory)",
        };
        assert_eq!(
            got, expected,
            "{v:?}: io.input open-failure message was `{got}`"
        );
    }
}

// ── non-OS failure: must NOT fabricate errno 0 (#301 no-fallback rule) ────────

/// A host hook that returns a `raw_os_error`-less `io::Error` has no OS errno.
/// The removed `unwrap_or(0)` fallback would have reported errno **0** here;
/// the fixed `file_result` instead returns the honest 2-value `(nil, msg)` —
/// so the third value is `nil` (absent), never a fabricated `0`. This is the
/// literal thing #301 is about, on the non-raw path. Platform-independent.
#[test]
fn io_open_non_os_error_does_not_report_errno_zero() {
    set_mode(FailMode::NonRaw);
    let code = triple_code("io.open('/whatever')");
    for v in ALL {
        let got = eval_str(v, &code);
        let (_ok_msg, tail) = got.rsplit_once('|').unwrap();
        assert!(
            tail == "2",
            "{v:?}: a non-OS io.open failure must be the 2-value (nil, msg) result, got `{got}`"
        );
        let errno_field = got.split('|').nth(2).unwrap();
        assert_eq!(
            errno_field, "nil",
            "{v:?}: a non-OS failure must NOT fabricate errno 0 — third value must be nil, \
             got `{got}`"
        );
    }
}

/// The same guard on `os.remove`: a non-OS failure is 2-value, no errno 0.
#[test]
fn os_remove_non_os_error_does_not_report_errno_zero() {
    set_mode(FailMode::NonRaw);
    let code = triple_code("os.remove('/whatever')");
    for v in ALL {
        let got = eval_str(v, &code);
        let errno_field = got.split('|').nth(2).unwrap();
        let count_field = got.rsplit('|').next().unwrap();
        assert_eq!(
            (errno_field, count_field),
            ("nil", "2"),
            "{v:?}: a non-OS os.remove failure must be (nil, msg) with no fabricated errno, \
             got `{got}`"
        );
    }
}

// ── short writes: errno + tuple fidelity through g_write (#305) ──────────────

/// Construction parameters for the next [`ScriptedWriteFile`]: a total byte
/// `budget` after which `write_bytes` fails with `errno`, and a `per_call_cap`
/// forcing partial (short) `Ok(n)` progress per call. A `per_call_cap` of 0
/// scripts the pathological zero-progress `Ok(0)` handle.
#[derive(Clone, Copy)]
struct WriteScript {
    budget: usize,
    per_call_cap: usize,
    errno: i32,
}

thread_local! {
    /// Parameters the next scripted write handle is constructed with.
    static WRITE_SCRIPT: Cell<WriteScript> = const {
        Cell::new(WriteScript { budget: usize::MAX, per_call_cap: usize::MAX, errno: 0 })
    };
    /// Every byte the scripted handle accepted, for content assertions.
    static WRITE_SINK: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// A write-only handle that accepts at most `per_call_cap` bytes per call
/// (forcing the fwrite-style continuation loop in `g_write` to iterate) and
/// fails with `io::Error::from_raw_os_error(errno)` once `remaining` is
/// exhausted — a deterministic stand-in for a device that fills up mid-write.
struct ScriptedWriteFile {
    remaining: usize,
    per_call_cap: usize,
    errno: i32,
}

impl LuaFileHandle for ScriptedWriteFile {
    fn read_byte(&mut self) -> i32 {
        -1
    }

    fn unread_byte(&mut self, _byte: i32) {}

    fn write_bytes(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.per_call_cap == 0 {
            return Ok(0);
        }
        if self.remaining == 0 {
            return Err(io::Error::from_raw_os_error(self.errno));
        }
        let n = data.len().min(self.per_call_cap).min(self.remaining);
        WRITE_SINK.with(|s| s.borrow_mut().extend_from_slice(&data[..n]));
        self.remaining -= n;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn seek(&mut self, _pos: SeekFrom) -> io::Result<u64> {
        Ok(0)
    }

    fn tell(&mut self) -> io::Result<u64> {
        Ok(0)
    }

    fn clear_error(&mut self) {}

    fn has_error(&self) -> bool {
        false
    }
}

fn scripted_write_open(_filename: &[u8], _mode: &[u8]) -> io::Result<Box<dyn LuaFileHandle>> {
    let script = WRITE_SCRIPT.with(Cell::get);
    Ok(Box::new(ScriptedWriteFile {
        remaining: script.budget,
        per_call_cap: script.per_call_cap,
        errno: script.errno,
    }))
}

fn lua_with_scripted_write(version: LuaVersion) -> Lua {
    let hooks = HostHooks::new().file_open(scripted_write_open);
    Lua::with_hooks_versioned(hooks, version).expect("init lua with scripted write hooks")
}

/// `f:write('abcdefgh')` rendered as `v1|v2|v3|v4|arity` from a SINGLE call
/// (packing varargs; calling twice would write twice).
const WRITE_PROBE: &str = "local f = assert(io.open('x', 'w')); \
     local function pack(...) return select('#', ...), {...} end; \
     local n, t = pack(f:write('abcdefgh')); \
     return tostring(t[1]) .. '|' .. tostring(t[2]) .. '|' .. tostring(t[3]) \
     .. '|' .. tostring(t[4]) .. '|' .. n";

/// `f:write('ab', 'cde')` — multi-argument variant of [`WRITE_PROBE`], so the
/// 5.5 counter has to accumulate ACROSS chunks and WITHIN the failing chunk
/// (5.5 `liolib.c` `totalbytes += numbytes` runs before the `numbytes < len`
/// check).
const MULTI_WRITE_PROBE: &str = "local f = assert(io.open('x', 'w')); \
     local function pack(...) return select('#', ...), {...} end; \
     local n, t = pack(f:write('ab', 'cde')); \
     return tostring(t[1]) .. '|' .. tostring(t[2]) .. '|' .. tostring(t[3]) \
     .. '|' .. tostring(t[4]) .. '|' .. n";

fn eval_probe_with(version: LuaVersion, script: WriteScript, probe: &str) -> (String, Vec<u8>) {
    WRITE_SCRIPT.with(|c| c.set(script));
    WRITE_SINK.with(|s| s.borrow_mut().clear());
    let lua = lua_with_scripted_write(version);
    let got = lua
        .load(probe)
        .set_name(b"=errno_kit")
        .eval()
        .unwrap_or_else(|e| panic!("write probe failed under {version:?}: {e:?}"));
    (got, WRITE_SINK.with(|s| s.borrow().clone()))
}

fn eval_write_probe(version: LuaVersion, script: WriteScript) -> (String, Vec<u8>) {
    eval_probe_with(version, script, WRITE_PROBE)
}

/// A write that makes partial progress (2 bytes per call) and then fails with
/// ENOSPC must report the reference `luaL_fileresult` failure tuple with the
/// REAL errno — `(nil, strerror(errno), errno)` on 5.1-5.4 — not the 2-value
/// errno-less result the fabricated "short write" error used to produce. On
/// 5.5 the reference `g_write` additionally pushes the total bytes written
/// (completed chunks + the failing chunk's partial progress) as a 4th value.
/// Expected strings pinned against `/tmp/lua-refs/bin/lua5.x` semantics for
/// ENOSPC (errno 28, "No space left on device").
#[cfg(unix)]
#[test]
fn short_write_reports_real_errno_and_reference_tuple() {
    for v in ALL {
        let (got, sink) = eval_write_probe(
            v,
            WriteScript {
                budget: 4,
                per_call_cap: 2,
                errno: 28,
            },
        );
        let expected = match v {
            LuaVersion::V55 => "nil|No space left on device|28|4|4",
            _ => "nil|No space left on device|28|nil|3",
        };
        assert_eq!(
            got, expected,
            "{v:?}: short write must surface the real errno in the reference tuple, got `{got}`"
        );
        assert_eq!(
            sink, b"abcd",
            "{v:?}: the handle must have accepted exactly the budgeted partial progress"
        );
    }
}

/// Partial `Ok(n)` progress with no error is NOT a failure: `g_write` must
/// continue writing the remainder (the way `fwrite` loops over `write(2)`)
/// until the chunk completes, then return the single success value.
#[test]
fn partial_progress_write_continues_to_completion() {
    for v in ALL {
        let (got, sink) = eval_write_probe(
            v,
            WriteScript {
                budget: usize::MAX,
                per_call_cap: 3,
                errno: 0,
            },
        );
        let arity = got.rsplit('|').next().unwrap();
        assert_eq!(
            arity, "1",
            "{v:?}: a completed write must return exactly one success value, got `{got}`"
        );
        assert!(
            got.starts_with("file ("),
            "{v:?}: the success value must be the file handle itself (C's g_write \
             returns with the handle on the stack top), got `{got}`"
        );
        assert_eq!(
            sink, b"abcdefgh",
            "{v:?}: every byte must reach the handle across the continuation loop"
        );
    }
}

/// A zero-progress `Ok(0)` handle can never complete the chunk. The guard must
/// fail the write rather than loop forever, and — carrying no OS errno — must
/// report the honest errno-less 2-value result (#301: never fabricate an
/// errno), plus the 5.5 counter of 0 bytes.
#[test]
fn zero_progress_write_fails_without_fabricated_errno() {
    for v in ALL {
        let (got, sink) = eval_write_probe(
            v,
            WriteScript {
                budget: usize::MAX,
                per_call_cap: 0,
                errno: 0,
            },
        );
        let expected = match v {
            LuaVersion::V55 => "nil|write error|0|nil|3",
            _ => "nil|write error|nil|nil|2",
        };
        assert_eq!(
            got, expected,
            "{v:?}: a zero-progress write must fail errno-less, got `{got}`"
        );
        assert!(
            sink.is_empty(),
            "{v:?}: a zero-progress handle accepted no bytes"
        );
    }
}

/// Failure on the SECOND argument: `f:write('ab', 'cde')` with a 3-byte budget
/// completes `'ab'`, writes 1 byte of `'cde'`, then fails. The 5.5 counter
/// must be 3 (completed chunk + partial progress in the failing chunk), and
/// 5.1-5.4 must report the plain errno triple.
#[cfg(unix)]
#[test]
fn multi_argument_failure_counts_across_chunks() {
    for v in ALL {
        let (got, sink) = eval_probe_with(
            v,
            WriteScript {
                budget: 3,
                per_call_cap: usize::MAX,
                errno: 28,
            },
            MULTI_WRITE_PROBE,
        );
        let expected = match v {
            LuaVersion::V55 => "nil|No space left on device|28|3|4",
            _ => "nil|No space left on device|28|nil|3",
        };
        assert_eq!(
            got, expected,
            "{v:?}: multi-argument short write must count across chunks, got `{got}`"
        );
        assert_eq!(
            sink, b"abc",
            "{v:?}: the handle must have accepted the budgeted 3 bytes across chunks"
        );
    }
}

/// `EINTR` (errno 4) is a terminal failure, not a retry: glibc's `fwrite` does
/// not restart on `EINTR` — it returns a short count with `errno` set — so the
/// tuple must carry errno 4, and the 5.5 counter only the pre-interrupt bytes.
#[cfg(unix)]
#[test]
fn eintr_is_reported_not_retried() {
    for v in ALL {
        let (got, sink) = eval_write_probe(
            v,
            WriteScript {
                budget: 4,
                per_call_cap: 2,
                errno: 4,
            },
        );
        let expected = match v {
            LuaVersion::V55 => "nil|Interrupted system call|4|4|4",
            _ => "nil|Interrupted system call|4|nil|3",
        };
        assert_eq!(
            got, expected,
            "{v:?}: EINTR must surface as the tuple's errno, got `{got}`"
        );
        assert_eq!(
            sink, b"abcd",
            "{v:?}: only the pre-EINTR bytes reach the handle"
        );
    }
}

// ── success path ─────────────────────────────────────────────────────────────

/// A hook that succeeds makes `os.remove` return exactly `true` (1 value), per
/// `luaL_fileresult`'s `stat != 0` branch. Platform-independent.
#[test]
fn os_remove_success_returns_true() {
    set_mode(FailMode::Success);
    let code = "return tostring(os.remove('/whatever')) .. '|' .. \
                tostring(select('#', os.remove('/whatever')))";
    for v in ALL {
        let got = eval_str(v, code);
        assert_eq!(
            got, "true|1",
            "{v:?}: a successful os.remove must return exactly `true`, got `{got}`"
        );
    }
}
