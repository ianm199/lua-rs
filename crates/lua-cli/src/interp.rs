//! Faithful Rust port of `reference/lua-5.4.7/src/lua.c` — the standalone
//! interpreter's option handling and chunk-running logic.
//!
//! The read-eval-print loop (`doREPL` and friends) lives in [`crate::repl`].
//! Platform hooks (file/io/dynlib/popen) and the `main` entry point live in
//! `main.rs`; this module owns everything between argument collection and the
//! decision to enter the REPL.
//!
//! Structure mirrors `lua.c` section-for-section: `collectargs`, `runargs`,
//! `createargtable`, `handle_script`, `dolibrary`, `handle_luainit`, the
//! `docall`/`msghandler`/`report` error path, and the `pmain` orchestrator
//! ([`run`]).

use std::io::{IsTerminal, Read, Write};

use lua_stdlib::auxlib::{self, load_buffer};
use lua_stdlib::init::open_libs;
use lua_types::error::LuaError;
use lua_types::value::LuaValue;
use lua_types::LuaType;
use lua_vm::api;
use lua_vm::state::LuaState;

/// `LUA_MULTRET`.
const MULTRET: i32 = -1;

/// `luaconf.h`: `LUA_PROGNAME`.
const PROGNAME_DEFAULT: &[u8] = b"lua";

/// `lua.c`: `LUA_COPYRIGHT`, printed by `-v` and on entry to an interactive
/// session. The language level implemented is Lua 5.4.7, so the banner names
/// that release.
//
// PORT NOTE: lua-rs is not C-Lua, but the REPL/`-v` banner mirrors the
// upstream copyright line so tooling that greps `lua -v` behaves unchanged.
const LUA_COPYRIGHT: &[u8] = b"Lua 5.4.7  Copyright (C) 1994-2024 Lua.org, PUC-Rio";

/// `LUA_REGISTRYINDEX` pseudo-index. Used to plant `LUA_NOENV` for `-E`.
const LUA_REGISTRYINDEX: i32 = -1_001_000;

/// `lua.c`: bits returned by `collectargs`.
const HAS_ERROR: i32 = 1;
const HAS_I: i32 = 2;
const HAS_V: i32 = 4;
const HAS_E: i32 = 8;
const HAS_BIG_E: i32 = 16;

/// Carries the program name across the option/chunk helpers, mirroring the
/// file-scope `progname` in `lua.c`. The REPL clears it (`None`) so errors at
/// the prompt are printed without a `lua:` prefix.
pub(crate) struct Cli {
    pub progname: Option<Vec<u8>>,
}

/// `lua.c`: `l_message` — print a message to stderr, prefixed with the program
/// name when one is set.
pub(crate) fn l_message(progname: Option<&[u8]>, msg: &[u8]) {
    let mut err = std::io::stderr();
    if let Some(p) = progname {
        let _ = err.write_all(p);
        let _ = err.write_all(b": ");
    }
    let _ = err.write_all(msg);
    let _ = err.write_all(b"\n");
    let _ = err.flush();
}

/// Extract the printable bytes from an error object. After `docall`, the
/// message handler has already replaced the error with a traceback string, so
/// the common case is `LuaError::Runtime(LuaValue::Str)`.
pub(crate) fn error_bytes(e: &LuaError) -> Vec<u8> {
    match e {
        LuaError::Runtime(v) | LuaError::Syntax(v) => match v {
            LuaValue::Str(s) => s.as_bytes().to_vec(),
            _ => b"(error object is not a string)".to_vec(),
        },
        _ => b"(error message not a string)".to_vec(),
    }
}

/// `lua.c`: `lua_remove` — drop the stack slot at `idx`, shifting the elements
/// above it down. Expressed via the public `rotate` + `set_top` primitives.
fn lua_remove(state: &mut LuaState, idx: i32) -> Result<(), LuaError> {
    api::rotate(state, idx, -1);
    api::set_top(state, -2)
}

/// `lua.c`: `msghandler` — the error-message handler installed by `docall`.
/// Turns the error object into a string (directly, via `__tostring`, or a
/// typed placeholder) and appends a standard traceback.
fn msghandler(state: &mut LuaState) -> Result<usize, LuaError> {
    let direct = match api::to_lua_string(state, 1) {
        Ok(Some(s)) => Some(s.as_bytes().to_vec()),
        _ => None,
    };
    let msg = match direct {
        Some(m) => m,
        None => {
            if auxlib::call_meta(state, 1, b"__tostring")?
                && api::lua_type_at(state, -1) == LuaType::String
            {
                return Ok(1);
            }
            let tn = api::type_name(state, api::lua_type_at(state, 1));
            format!(
                "(error object is a {} value)",
                String::from_utf8_lossy(tn)
            )
            .into_bytes()
        }
    };
    auxlib::traceback(state, None, Some(&msg), 1)?;
    Ok(1)
}

/// `lua.c`: `print_version` — write the copyright banner to stdout.
fn print_version() {
    let mut out = std::io::stdout();
    let _ = out.write_all(LUA_COPYRIGHT);
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

/// `lua.c`: `lua_stdin_is_tty` — whether we are attached to an interactive
/// terminal.
fn stdin_is_tty() -> bool {
    std::io::stdin().is_terminal()
}

/// `lua_readline`'s loader counterpart from `lauxlib.c`: skip a UTF-8 BOM and a
/// leading `#...` line so executable scripts with a shebang load. A newline is
/// preserved in place of the stripped line to keep reported line numbers
/// aligned with the source file.
//
// PORT NOTE: C's luaL_loadfilex skips the shebang char-by-char in the reader;
// we strip it from the buffer before `load_buffer`, keeping the terminating
// newline so line counts match.
fn strip_shebang(mut data: Vec<u8>) -> Vec<u8> {
    if data.starts_with(b"\xEF\xBB\xBF") {
        data.drain(0..3);
    }
    if data.first() == Some(&b'#') {
        match data.iter().position(|&b| b == b'\n') {
            Some(pos) => {
                data.drain(0..pos);
            }
            None => data.clear(),
        }
    }
    data
}

/// Render an I/O error the way C's `strerror` does — Rust's `Display` appends
/// a ` (os error N)` suffix that C-Lua never prints, so strip it.
fn io_error_message(e: &std::io::Error) -> String {
    let s = e.to_string();
    match s.rfind(" (os error ") {
        Some(idx) => s[..idx].to_string(),
        None => s,
    }
}

#[cfg(unix)]
fn path_from_bytes(b: &[u8]) -> std::path::PathBuf {
    use std::os::unix::ffi::OsStrExt;
    std::path::PathBuf::from(std::ffi::OsStr::from_bytes(b))
}
#[cfg(not(unix))]
fn path_from_bytes(b: &[u8]) -> std::path::PathBuf {
    std::path::PathBuf::from(String::from_utf8_lossy(b).into_owned())
}

#[cfg(unix)]
fn env_bytes(key: &str) -> Option<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;
    std::env::var_os(key).map(|v| v.as_bytes().to_vec())
}
#[cfg(not(unix))]
fn env_bytes(key: &str) -> Option<Vec<u8>> {
    std::env::var_os(key).map(|v| v.to_string_lossy().into_owned().into_bytes())
}

/// `lua.c`: `createargtable` — build the global `arg` table. `argv[script]`
/// lands at index 0; arguments after the script take positive indices; options
/// before it take negative indices.
fn createargtable(state: &mut LuaState, argv: &[Vec<u8>], script: i32) -> Result<(), LuaError> {
    let argc = argv.len() as i32;
    let narg = argc - (script + 1);
    api::create_table(state, narg.max(0), script + 1)?;
    for i in 0..argc {
        api::push_lstring(state, &argv[i as usize])?;
        api::raw_set_i(state, -2, (i - script) as i64)?;
    }
    api::set_global(state, b"arg")
}

/// `lua.c`: `pushargs` — push `arg[1..#arg]` for the script call and return the
/// count.
fn pushargs(state: &mut LuaState) -> Result<i32, LuaError> {
    if api::get_global(state, b"arg")? != LuaType::Table {
        return Err(LuaError::runtime(format_args!("'arg' is not a table")));
    }
    let n = auxlib::lua_len(state, -1)? as i32;
    if !api::check_stack(state, n + 3) {
        return Err(LuaError::runtime(format_args!(
            "too many arguments to script"
        )));
    }
    for i in 1..=n {
        api::raw_get_i(state, -i, i as i64);
    }
    lua_remove(state, -(n + 1))?;
    Ok(n)
}

/// `lua.c`: `collectargs` — scan options, returning `(mask, first)`. `first` is
/// the index of the script name, `0` for no script, or the index of the bad
/// option when `mask == HAS_ERROR`.
/// Sandbox options collected from the command line: `--sandbox`,
/// `--max-instructions=N`, `--max-memory=N[K|M|G]`.
#[derive(Default)]
struct SandboxCliOpts {
    strict: bool,
    max_instructions: Option<u64>,
    max_memory: Option<usize>,
}

impl SandboxCliOpts {
    fn active(&self) -> bool {
        self.strict || self.max_instructions.is_some() || self.max_memory.is_some()
    }

    /// Resolve the effective instruction limit: an explicit `--max-instructions`
    /// wins, otherwise the strict preset's 10M when `--sandbox` is set.
    fn instruction_limit(&self) -> Option<u64> {
        self.max_instructions.or(if self.strict {
            Some(10_000_000)
        } else {
            None
        })
    }

    /// Resolve the effective memory limit: explicit `--max-memory`, otherwise
    /// the strict preset's 64 MiB when `--sandbox` is set.
    fn memory_limit(&self) -> Option<usize> {
        self.max_memory.or(if self.strict {
            Some(64 * 1024 * 1024)
        } else {
            None
        })
    }
}

/// Whether `a` is one of the sandbox long-options (so `collectargs` accepts it
/// rather than rejecting it as an unknown option).
fn is_sandbox_opt(a: &[u8]) -> bool {
    a == b"--sandbox"
        || a.starts_with(b"--max-instructions=")
        || a.starts_with(b"--max-memory=")
}

/// Parse a byte count with an optional `K`/`M`/`G` (1024-based) suffix.
fn parse_byte_count(v: &[u8]) -> Option<usize> {
    let (digits, mult) = match v.last() {
        Some(b'K') | Some(b'k') => (&v[..v.len() - 1], 1024),
        Some(b'M') | Some(b'm') => (&v[..v.len() - 1], 1024 * 1024),
        Some(b'G') | Some(b'g') => (&v[..v.len() - 1], 1024 * 1024 * 1024),
        _ => (v, 1),
    };
    let text = std::str::from_utf8(digits).ok()?;
    let n: usize = text.parse().ok()?;
    n.checked_mul(mult)
}

/// Scan the option prefix of `argv` for sandbox flags. Stops at the first
/// non-option / `--`, matching `collectargs`' option region.
fn parse_sandbox_opts(argv: &[Vec<u8>]) -> SandboxCliOpts {
    let mut opts = SandboxCliOpts::default();
    let mut i = 1usize;
    while i < argv.len() {
        let a = &argv[i];
        if a.is_empty() || a[0] != b'-' {
            break;
        }
        if a.as_slice() == b"--" {
            break;
        }
        if a.as_slice() == b"--sandbox" {
            opts.strict = true;
        } else if let Some(v) = a.strip_prefix(b"--max-instructions=") {
            if let Ok(text) = std::str::from_utf8(v) {
                opts.max_instructions = text.parse().ok();
            }
        } else if let Some(v) = a.strip_prefix(b"--max-memory=") {
            opts.max_memory = parse_byte_count(v);
        }
        i += 1;
    }
    opts
}

fn collectargs(argv: &[Vec<u8>]) -> (i32, i32) {
    let mut args = 0;
    let mut first;
    let mut i = 1usize;
    while i < argv.len() {
        first = i as i32;
        let a = &argv[i];
        if a.is_empty() || a[0] != b'-' {
            return (args, first);
        }
        match a.get(1).copied() {
            None => return (args, first),
            Some(b'-') => {
                if a.len() == 2 {
                    return (args, (i + 1) as i32);
                }
                if !is_sandbox_opt(a) {
                    return (HAS_ERROR, first);
                }
            }
            Some(b'E') => {
                if a.len() > 2 {
                    return (HAS_ERROR, first);
                }
                args |= HAS_BIG_E;
            }
            Some(b'W') => {
                if a.len() > 2 {
                    return (HAS_ERROR, first);
                }
            }
            Some(b'i') => {
                if a.len() > 2 {
                    return (HAS_ERROR, first);
                }
                args |= HAS_I | HAS_V;
            }
            Some(b'v') => {
                if a.len() > 2 {
                    return (HAS_ERROR, first);
                }
                args |= HAS_V;
            }
            Some(b'e') => {
                args |= HAS_E;
                if a.len() == 2 {
                    i += 1;
                    if i >= argv.len() || argv[i].first() == Some(&b'-') {
                        return (HAS_ERROR, first);
                    }
                }
            }
            Some(b'l') => {
                if a.len() == 2 {
                    i += 1;
                    if i >= argv.len() || argv[i].first() == Some(&b'-') {
                        return (HAS_ERROR, first);
                    }
                }
            }
            _ => return (HAS_ERROR, first),
        }
        i += 1;
    }
    (args, 0)
}

impl Cli {
    pub(crate) fn report(&self, r: Result<(), LuaError>) -> bool {
        match r {
            Ok(()) => true,
            Err(e) => {
                l_message(self.progname.as_deref(), &error_bytes(&e));
                false
            }
        }
    }

    /// Report the error string a failed `load` left on the stack, then pop it.
    pub(crate) fn report_stack_error(&self, state: &mut LuaState) {
        let bytes = match api::to_lua_string(state, -1) {
            Ok(Some(s)) => s.as_bytes().to_vec(),
            _ => b"(error message not a string)".to_vec(),
        };
        state.pop_n(1);
        l_message(self.progname.as_deref(), &bytes);
    }

    /// `lua.c`: `docall` — call the function on the stack under a freshly
    /// installed `msghandler`, so any error comes back as a traceback string.
    /// C-Lua's SIGINT-driven interrupt of a running chunk (`laction`/`lstop`)
    /// is not yet ported; see the `repl.rs` PORT STATUS for why.
    pub(crate) fn docall(&self, state: &mut LuaState, nargs: i32, nres: i32) -> Result<(), LuaError> {
        let base = api::get_top(state) - nargs;
        api::push_cclosure(state, msghandler, 0)?;
        state.insert(base)?;
        let r = api::pcall_k(state, nargs, nres, base, 0, None);
        let _ = lua_remove(state, base);
        r.map(|_| ())
    }

    /// `lua.c`: `dostring` — load a string and run it, reporting any failure.
    fn dostring(&self, state: &mut LuaState, s: &[u8], name: &[u8]) -> bool {
        match load_buffer(state, s, name) {
            Ok(0) => self.report(self.docall(state, 0, 0)),
            Ok(_) => {
                self.report_stack_error(state);
                false
            }
            Err(e) => self.report(Err(e)),
        }
    }

    /// `lua.c`: `dofile` — load a file (or stdin when `name` is `None`) and run
    /// it.
    fn dofile(&self, state: &mut LuaState, name: Option<&[u8]>) -> bool {
        match self.load_file(state, name) {
            Ok(0) => self.report(self.docall(state, 0, 0)),
            Ok(_) => {
                self.report_stack_error(state);
                false
            }
            Err(e) => self.report(Err(e)),
        }
    }

    /// `lauxlib.c`: `luaL_loadfile` — read a file (or stdin) and compile it.
    /// Returns the loader status (`0` = ok, chunk on stack; non-zero = error
    /// message on stack).
    fn load_file(&self, state: &mut LuaState, name: Option<&[u8]>) -> Result<i32, LuaError> {
        let (data, chunkname) = match name {
            Some(path) => {
                let raw = std::fs::read(path_from_bytes(path)).map_err(|e| {
                    LuaError::runtime(format_args!(
                        "cannot open {}: {}",
                        String::from_utf8_lossy(path),
                        io_error_message(&e)
                    ))
                })?;
                let mut cn = vec![b'@'];
                cn.extend_from_slice(path);
                (strip_shebang(raw), cn)
            }
            None => {
                let mut raw = Vec::new();
                std::io::stdin()
                    .lock()
                    .read_to_end(&mut raw)
                    .map_err(|e| LuaError::runtime(format_args!("stdin read error: {}", e)))?;
                (strip_shebang(raw), b"=stdin".to_vec())
            }
        };
        load_buffer(state, &data, &chunkname)
    }

    /// `lua.c`: `dolibrary` — run `globname = require(modname)` for `-l`,
    /// handling the `g=mod` and version-suffix forms.
    fn dolibrary(&self, state: &mut LuaState, globname: &[u8]) -> bool {
        let eq = globname.iter().position(|&b| b == b'=');
        let (glob_full, modname): (&[u8], &[u8]) = match eq {
            Some(p) => (&globname[..p], &globname[p + 1..]),
            None => (globname, globname),
        };
        let effective_glob: &[u8] = if eq.is_none() {
            match glob_full.iter().position(|&b| b == b'-') {
                Some(p) => &glob_full[..p],
                None => glob_full,
            }
        } else {
            glob_full
        };

        let load = (|| {
            if api::get_global(state, b"require")? != LuaType::Function {
                return Err(LuaError::runtime(format_args!("'require' is not a function")));
            }
            api::push_lstring(state, modname)?;
            Ok(())
        })();
        if let Err(e) = load {
            return self.report(Err(e));
        }
        match self.docall(state, 1, 1) {
            Ok(()) => match api::set_global(state, effective_glob) {
                Ok(()) => true,
                Err(e) => self.report(Err(e)),
            },
            Err(e) => self.report(Err(e)),
        }
    }

    /// `lua.c`: `handle_luainit` — run `$LUA_INIT_5_4` or `$LUA_INIT` (a file
    /// when it starts with `@`, otherwise a chunk).
    fn handle_luainit(&self, state: &mut LuaState) -> bool {
        let (init, name): (Vec<u8>, &[u8]) = match env_bytes("LUA_INIT_5_4") {
            Some(v) => (v, b"=LUA_INIT_5_4" as &[u8]),
            None => match env_bytes("LUA_INIT") {
                Some(v) => (v, b"=LUA_INIT" as &[u8]),
                None => return true,
            },
        };
        if init.first() == Some(&b'@') {
            self.dofile(state, Some(&init[1..]))
        } else {
            self.dostring(state, &init, name)
        }
    }

    /// `lua.c`: `handle_script` — load and run the main script, passing the
    /// positive entries of `arg`. `argv` is sliced so index 0 is the script.
    fn handle_script(&self, state: &mut LuaState, argv: &[Vec<u8>], script: i32) -> bool {
        let fname = &argv[script as usize];
        let prev = argv.get((script - 1) as usize).map(|v| v.as_slice());
        let use_stdin = fname.as_slice() == b"-" && prev != Some(b"--");
        let loaded = if use_stdin {
            self.load_file(state, None)
        } else {
            self.load_file(state, Some(fname))
        };
        match loaded {
            Ok(0) => {
                let n = match pushargs(state) {
                    Ok(n) => n,
                    Err(e) => return self.report(Err(e)),
                };
                self.report(self.docall(state, n, MULTRET))
            }
            Ok(_) => {
                self.report_stack_error(state);
                false
            }
            Err(e) => self.report(Err(e)),
        }
    }

    /// `lua.c`: `runargs` — execute the `-e`, `-l`, and `-W` options in order,
    /// up to `optlim`. Returns `false` if any chunk failed.
    fn runargs(&self, state: &mut LuaState, argv: &[Vec<u8>], n: i32) -> bool {
        let mut i = 1usize;
        while (i as i32) < n {
            let a = &argv[i];
            let option = a.get(1).copied();
            match option {
                Some(opt @ b'e') | Some(opt @ b'l') => {
                    let extra: Vec<u8> = if a.len() > 2 {
                        a[2..].to_vec()
                    } else {
                        i += 1;
                        argv[i].clone()
                    };
                    let ok = if opt == b'e' {
                        self.dostring(state, &extra, b"=(command line)")
                    } else {
                        self.dolibrary(state, &extra)
                    };
                    if !ok {
                        return false;
                    }
                }
                Some(b'W') => api::warning(state, b"@on", false),
                _ => {}
            }
            i += 1;
        }
        true
    }

    /// `lua.c`: `print_usage` — emit the usage block, tailored to the offending
    /// option.
    fn print_usage(&self, badoption: &[u8]) {
        let mut err = std::io::stderr();
        let prog = self.progname.as_deref().unwrap_or(PROGNAME_DEFAULT);
        let _ = err.write_all(prog);
        let _ = err.write_all(b": ");
        if badoption.get(1) == Some(&b'e') || badoption.get(1) == Some(&b'l') {
            let _ = writeln!(err, "'{}' needs argument", String::from_utf8_lossy(badoption));
        } else {
            let _ = writeln!(
                err,
                "unrecognized option '{}'",
                String::from_utf8_lossy(badoption)
            );
        }
        let _ = err.write_all(b"usage: ");
        let _ = err.write_all(prog);
        let _ = err.write_all(
            b" [options] [script [args]]\n\
              Available options are:\n\
              \x20 -e stat   execute string 'stat'\n\
              \x20 -i        enter interactive mode after executing 'script'\n\
              \x20 -l mod    require library 'mod' into global 'mod'\n\
              \x20 -l g=mod  require library 'mod' into global 'g'\n\
              \x20 -v        show version information\n\
              \x20 -E        ignore environment variables\n\
              \x20 -W        turn warnings on\n\
              \x20 --        stop handling options\n\
              \x20 -         stop handling options and execute stdin\n\
              \x20 --sandbox            run untrusted code: strip host-access\n\
              \x20                      globals and apply default CPU/memory caps\n\
              \x20 --max-instructions=N abort after N executed VM instructions\n\
              \x20 --max-memory=N[K|M|G] abort when GC memory exceeds N\n",
        );
        let _ = err.flush();
    }
}

/// `lua.c`: `pmain` — the body of the interpreter. Assumes `state` already has
/// its platform hooks installed; opens the standard libraries, installs
/// `preload`-registered native modules, builds `arg`, runs `LUA_INIT`, then the
/// `-e`/`-l` options, then the script, and finally enters the REPL when
/// appropriate. Returns the process exit code.
pub(crate) fn run(
    state: &mut LuaState,
    argv: &[Vec<u8>],
    preload: fn(&mut LuaState) -> Result<(), LuaError>,
) -> i32 {
    let (args, script) = collectargs(argv);

    let mut cli = Cli {
        progname: argv
            .first()
            .filter(|p| !p.is_empty())
            .map(|p| p.to_vec()),
    };

    if args == HAS_ERROR {
        let bad = argv.get(script as usize).map(|v| v.as_slice()).unwrap_or(b"");
        cli.print_usage(bad);
        return 1;
    }

    if args & HAS_V != 0 {
        print_version();
    }

    if args & HAS_BIG_E != 0 {
        api::push_boolean(state, true);
        if let Err(e) = api::set_field(state, LUA_REGISTRYINDEX, b"LUA_NOENV") {
            return if cli.report(Err(e)) { 0 } else { 1 };
        }
    }

    if script > 0 {
        if let Some(dir) = path_from_bytes(&argv[script as usize])
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
        {
            crate::prepend_lua_path(dir);
        }
    }

    if let Err(e) = open_libs(state) {
        cli.report(Err(e));
        return 1;
    }
    if let Err(e) = preload(state) {
        cli.report(Err(e));
        return 1;
    }

    let sandbox_opts = parse_sandbox_opts(argv);
    if sandbox_opts.active() {
        if sandbox_opts.strict {
            if let Err(e) =
                lua_stdlib::sandbox::strip_globals(state, lua_stdlib::sandbox::STRICT_REMOVED_GLOBALS)
            {
                cli.report(Err(e));
                return 1;
            }
        }
        let instr = sandbox_opts.instruction_limit();
        let mem = sandbox_opts.memory_limit();
        if instr.is_some() || mem.is_some() {
            state.install_sandbox_limits(1000, instr, mem);
        }
    }

    if let Err(e) = createargtable(state, argv, script) {
        cli.report(Err(e));
        return 1;
    }

    if args & HAS_BIG_E == 0 && !cli.handle_luainit(state) {
        return 1;
    }

    let optlim = if script > 0 { script } else { argv.len() as i32 };
    if !cli.runargs(state, argv, optlim) {
        return 1;
    }

    if script > 0 && !cli.handle_script(state, argv, script) {
        return 1;
    }

    if args & HAS_I != 0 {
        crate::repl::do_repl(state, &mut cli);
    } else if script < 1 && args & (HAS_E | HAS_V) == 0 {
        if stdin_is_tty() {
            print_version();
            crate::repl::do_repl(state, &mut cli);
        } else {
            cli.dofile(state, None);
        }
    }

    // C-Lua's `main` calls `lua_close(L)` after `pmain`, which runs every
    // remaining `__gc` finalizer (`luaC_freeallobjects`). Drive the same
    // close-time finalizer pass before the `LuaState` is dropped so programs
    // that keep a finalizable object alive to program end still observe their
    // `__gc` side effects (e.g. `gc.lua`'s `>>> closing state <<<`).
    api::run_close_finalizers(state);

    0
}

// ──────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        reference/lua-5.4.7/src/lua.c (option handling + pmain)
//   target_crate:  lua-cli
//   confidence:    medium
//   todos:         1  (os.exit exit-code propagation is a pre-existing
//                      lua-stdlib gap: os_lib.rs returns a placeholder
//                      with_status error; a LuaError::Exit(i32) variant is
//                      needed for faithful behaviour — out of scope here)
//   port_notes:    3  (LUA_COPYRIGHT banner reused; shebang stripped in
//                      buffer not reader; script-dir prepended to LUA_PATH —
//                      a lua-rs extension preserved from the prior CLI)
//   unsafe_blocks: 0
//   notes:         docall installs msghandler as the pcall errfunc; the VM
//                  invokes it during error synthesis (do_.rs pcall), so the
//                  returned LuaError carries the traceback string. SIGINT
//                  interruption of a running chunk is wired in repl.rs.
// ──────────────────────────────────────────────────────────────────────────
