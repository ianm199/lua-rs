//! CLI-level oracle for issue #301: the io/os failure `(nil, msg, errno)`
//! triples produced by the REAL `lua-cli` filesystem hooks must byte-match the
//! reference standalone interpreter.
//!
//! This is a SPAWN-THE-BINARY test on purpose. The in-memory
//! `io_errno_fidelity_kit` drives fabricated `io::Error`s, so it can pin the
//! mapping layer but CANNOT catch a bug in the CLI hook that actually touches
//! the filesystem — e.g. HIGH-2, where `os.remove` on a symlink must call
//! `unlink` (reporting the real errno) rather than falling back to `rmdir` and
//! clobbering it. Only running the true hook against real paths, diffed against
//! the reference `remove(3)`, exercises that.
//!
//! Each scenario runs both binaries and compares. Where a scenario mutates the
//! filesystem (`os.remove`), omniLua and the reference get separate but
//! identical fixtures, and the fixture path is normalized to `<p>` before
//! comparison. Scenarios on a fixed nonexistent path need no normalization.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

const VERSIONS: &[&str] = &["5.1", "5.2", "5.3", "5.4", "5.5"];

fn full_version(version: &str) -> &'static str {
    match version {
        "5.1" => "5.1.5",
        "5.2" => "5.2.4",
        "5.3" => "5.3.6",
        "5.4" => "5.4.7",
        "5.5" => "5.5.0",
        other => panic!("unhandled version {other}"),
    }
}

fn reference_binary(version: &str) -> Option<PathBuf> {
    let p = PathBuf::from(format!("/tmp/lua-refs/bin/lua{}", full_version(version)));
    p.exists().then_some(p)
}

/// Unique temp path root for one scenario, pid + counter so parallel test
/// threads never collide (per the harness temp-file rule).
fn unique(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut p = std::env::temp_dir();
    p.push(format!("omnilua_io_errno_{}_{}_{}", tag, std::process::id(), n));
    p
}

fn run_omni(version: &str, code: &str) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_omnilua"))
        .env("OMNILUA_VERSION", version)
        .arg("-e")
        .arg(code)
        .output()
        .expect("spawn omnilua");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn run_ref(refbin: &PathBuf, code: &str) -> String {
    let out = Command::new(refbin)
        .arg("-e")
        .arg(code)
        .output()
        .expect("spawn reference");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// `print(...)` a failure triple for `call`, so stdout carries the tab-joined
/// `nil<TAB>msg<TAB>errno` exactly as the reference renders it.
fn print_triple(call: &str) -> String {
    format!("print({call})")
}

// ── io.open on a fixed missing path (no fixture, direct compare) ─────────────

#[test]
fn io_open_missing_matches_reference_all_versions() {
    let code = print_triple("io.open('/nonexistent/omnilua-301/x')");
    for v in VERSIONS {
        let Some(refbin) = reference_binary(v) else {
            continue;
        };
        let omni = run_omni(v, &code);
        let reference = run_ref(&refbin, &code);
        assert_eq!(
            omni, reference,
            "[{v}] io.open missing-file triple diverged from reference"
        );
    }
}

#[test]
fn os_rename_missing_matches_reference_all_versions() {
    let code = print_triple("os.rename('/nonexistent/omnilua-301/x', '/nonexistent/omnilua-301/y')");
    for v in VERSIONS {
        let Some(refbin) = reference_binary(v) else {
            continue;
        };
        let omni = run_omni(v, &code);
        let reference = run_ref(&refbin, &code);
        assert_eq!(
            omni, reference,
            "[{v}] os.rename missing-file triple diverged from reference"
        );
    }
}

/// MEDIUM-4: `io.input` on a bad file is version-gated (5.1 `argerror` vs 5.2+
/// `cannot open file`), through the real CLI open hook.
#[test]
fn io_input_open_failure_matches_reference_all_versions() {
    let code = "print(pcall(io.input, '/nonexistent/omnilua-301/x'))";
    for v in VERSIONS {
        let Some(refbin) = reference_binary(v) else {
            continue;
        };
        let omni = run_omni(v, code);
        let reference = run_ref(&refbin, code);
        assert_eq!(
            omni, reference,
            "[{v}] io.input open-failure message diverged from reference"
        );
    }
}

/// HIGH-2: writing to a bad descriptor (a file opened read-only) must RETURN
/// the `luaL_fileresult` failure tuple, not raise — through the real CLI hook,
/// for both `io.write` and `file:write`. 5.1-5.4 return `(nil, "Bad file
/// descriptor", 9)`; 5.5 additionally returns a 4th value, total bytes written
/// (0). The same read-only fixture serves both interpreters (a failed write
/// does not mutate it).
#[test]
fn write_to_bad_fd_returns_triple_all_versions() {
    for v in VERSIONS {
        let Some(refbin) = reference_binary(v) else {
            continue;
        };
        let path = unique("badfd");
        std::fs::write(&path, b"data").expect("create read-only-write fixture");
        let p = path.display();
        let f_code = format!(
            "local f = io.open('{p}', 'r'); \
             print(f:write('xuxu')); \
             print('n=' .. select('#', f:write('yy')))"
        );
        let io_code = format!(
            "local f = io.open('{p}', 'r'); io.output(f); \
             print(io.write('xuxu')); \
             print('n=' .. select('#', io.write('yy')))"
        );
        for (label, code) in [("file:write", &f_code), ("io.write", &io_code)] {
            let omni = run_omni(v, code);
            let reference = run_ref(&refbin, code);
            assert_eq!(
                omni, reference,
                "[{v}] {label} to a bad descriptor diverged from reference"
            );
        }
        let _ = std::fs::remove_file(&path);
    }
}

// ── os.remove on real fixtures (HIGH-2), fixture path normalized ─────────────

/// Run `os.remove(<path>)` under omniLua and the reference on separate but
/// identical fixtures built by `make_fixture`, and assert the triples match
/// after normalizing each fixture path to `<p>`.
#[cfg(unix)]
fn assert_remove_matches(tag: &str, make_fixture: impl Fn(&PathBuf)) {
    for v in VERSIONS {
        let Some(refbin) = reference_binary(v) else {
            continue;
        };
        let omni_path = unique(&format!("{tag}_omni"));
        let ref_path = unique(&format!("{tag}_ref"));
        make_fixture(&omni_path);
        make_fixture(&ref_path);

        let omni = run_omni(v, &print_triple(&format!("os.remove('{}')", omni_path.display())))
            .replace(&omni_path.display().to_string(), "<p>");
        let reference = run_ref(&refbin, &print_triple(&format!("os.remove('{}')", ref_path.display())))
            .replace(&ref_path.display().to_string(), "<p>");

        // Best-effort cleanup of anything the interpreters did not remove.
        let _ = std::fs::remove_dir_all(&omni_path);
        let _ = std::fs::remove_dir_all(&ref_path);
        let _ = std::fs::remove_file(&omni_path);
        let _ = std::fs::remove_file(&ref_path);

        assert_eq!(
            omni, reference,
            "[{v}] os.remove('{tag}') triple diverged from reference remove(3)"
        );
    }
}

/// HIGH-2: a symlink. C `remove(3)` `unlink`s it (reporting `unlink`'s errno if
/// it fails); the old `remove_file.or_else(remove_dir)` fallback reported
/// `rmdir`'s spurious `ENOTDIR` instead. Here the symlink target is a directory
/// the user cannot unlink through, exercising the errno path.
#[cfg(unix)]
#[test]
fn os_remove_symlink_matches_reference() {
    use std::os::unix::fs::symlink;
    // A symlink to `/` — `unlink` on the symlink itself succeeds (removes the
    // link), so both interpreters return `true`. This pins that omniLua unlinks
    // the symlink rather than following it to `rmdir` the target.
    assert_remove_matches("symlink", |p| {
        symlink("/", p).expect("create symlink fixture");
    });
}

/// A non-empty directory: `remove(3)` `rmdir`s it and reports `ENOTEMPTY`.
#[cfg(unix)]
#[test]
fn os_remove_nonempty_dir_matches_reference() {
    assert_remove_matches("nonempty_dir", |p| {
        std::fs::create_dir(p).expect("create dir fixture");
        std::fs::write(p.join("inner"), b"x").expect("populate dir fixture");
    });
}

/// A regular file: `remove(3)` `unlink`s it, returns `true`.
#[cfg(unix)]
#[test]
fn os_remove_regular_file_matches_reference() {
    assert_remove_matches("regfile", |p| {
        std::fs::write(p, b"data").expect("create file fixture");
    });
}

/// HIGH-2 regression: a symlink inside a **non-writable** parent directory.
/// `unlink(link)` fails `EACCES` (no write on the parent); `rmdir(link)` would
/// fail `ENOTDIR` (the link is not a directory). C `remove(3)` reports the
/// `unlink` error (`EACCES`, 13); the old `remove_file.or_else(remove_dir)`
/// hook reported the `rmdir` error (`ENOTDIR`, 20) instead. This is the exact
/// errno-clobber HIGH-2 describes, and the only fixture here that distinguishes
/// the fixed hook from the buggy one.
///
/// (When the tests run as root the parent's mode is bypassed and both `unlink`s
/// succeed; the case then still matches the reference — which also runs as root
/// — it just loses its distinguishing power.)
#[cfg(unix)]
#[test]
fn os_remove_symlink_in_readonly_dir_matches_reference() {
    use std::os::unix::fs::{symlink, PermissionsExt};

    for v in VERSIONS {
        let Some(refbin) = reference_binary(v) else {
            continue;
        };

        let build = |parent: &PathBuf| {
            std::fs::create_dir(parent).expect("create parent dir");
            symlink(parent.join("target"), parent.join("link")).expect("create symlink");
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o555))
                .expect("chmod parent read-only");
        };
        let teardown = |parent: &PathBuf| {
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o755));
            let _ = std::fs::remove_dir_all(parent);
        };

        let omni_parent = unique("rodir_omni");
        let ref_parent = unique("rodir_ref");
        build(&omni_parent);
        build(&ref_parent);

        let omni_link = omni_parent.join("link");
        let ref_link = ref_parent.join("link");
        let omni = run_omni(v, &print_triple(&format!("os.remove('{}')", omni_link.display())))
            .replace(&omni_link.display().to_string(), "<p>");
        let reference = run_ref(&refbin, &print_triple(&format!("os.remove('{}')", ref_link.display())))
            .replace(&ref_link.display().to_string(), "<p>");

        teardown(&omni_parent);
        teardown(&ref_parent);

        assert_eq!(
            omni, reference,
            "[{v}] os.remove(symlink in read-only dir) must report the unlink errno like \
             reference remove(3), not a spurious rmdir ENOTDIR"
        );
    }
}

// ── /dev/full: short-write tuples through stdio buffering modes (#305) ───────

/// The `/dev/full` scenarios only exist where the device does (Linux); the
/// stand-in in `file_open_hook` must then byte-match the reference through
/// every `setvbuf` mode. Handles are never printed (their `tostring` address
/// rendering differs); scenarios print only failure tuples and booleans.
#[cfg(unix)]
fn dev_full_available() -> bool {
    std::path::Path::new("/dev/full").exists()
}

/// glibc's line-buffered `fwrite` byte accounting is stateful, and 5.5's
/// `g_write` exposes it through the 4th totalbytes value: the FIRST write on a
/// fresh stream fails at the first newline having counted the bytes before it;
/// SUBSEQUENT writes consume through the LAST newline (`"abc","de\nf"` → 6)
/// unless the data ends at that newline, where `fwrite` reports 0
/// (`"abc","de\n"` → 3). A failed newline-flush discards the buffer, so a
/// later `close` succeeds. 5.1-5.4 print the same 3-value tuple without the
/// counter. Every snippet is diffed verbatim against the reference binary.
#[cfg(unix)]
#[test]
fn dev_full_write_tuples_match_reference_all_versions() {
    if !dev_full_available() {
        return;
    }
    const SNIPPETS: &[&str] = &[
        "local f=assert(io.open('/dev/full','w')); f:setvbuf('line'); print(f:write('abc\\n'))",
        "local f=assert(io.open('/dev/full','w')); f:setvbuf('line'); print(f:write('abc','de\\nf'))",
        "local f=assert(io.open('/dev/full','w')); f:setvbuf('line'); print(f:write('abc','de\\n'))",
        "local f=assert(io.open('/dev/full','w')); f:setvbuf('line'); print(f:write('\\n'))",
        "local f=assert(io.open('/dev/full','w')); f:setvbuf('line'); print(f:write('ab\\ncd\\nef'))",
        "local f=assert(io.open('/dev/full','w')); f:setvbuf('line'); assert(f:write('abc')); print(f:write('de\\nf'))",
        "local f=assert(io.open('/dev/full','w')); f:setvbuf('line'); f:write('abc\\n'); print((f:write('x')) ~= nil)",
        "local f=assert(io.open('/dev/full','w')); f:setvbuf('line'); f:write('abc\\n'); print(f:close())",
        "local f=assert(io.open('/dev/full','w')); f:setvbuf('line'); assert(f:write('abc')); print(f:flush())",
        "local f=assert(io.open('/dev/full','w')); f:setvbuf('no'); print(f:write('abc'))",
        "local f=assert(io.open('/dev/full','w')); f:setvbuf('no'); print(f:write(12345))",
        "local f=assert(io.open('/dev/full','w')); f:setvbuf('no'); print(f:flush())",
        "local f=assert(io.open('/dev/full','w')); f:setvbuf('no'); print(f:close())",
        "local f=assert(io.open('/dev/full','w')); print(f:flush())",
        "local f=assert(io.open('/dev/full','w')); assert(f:write('abc')); print(f:flush())",
    ];
    for v in VERSIONS {
        let Some(refbin) = reference_binary(v) else {
            continue;
        };
        for code in SNIPPETS {
            let omni = run_omni(v, code);
            let reference = run_ref(&refbin, code);
            assert_eq!(
                omni, reference,
                "[{v}] /dev/full tuple diverged from reference for `{code}`"
            );
        }
    }
}
