//! Faithful port of `lua.c`'s read-eval-print loop (`doREPL`, `loadline`,
//! `addreturn`, `multiline`, `incomplete`, `l_print`, `get_prompt`), with the
//! line-editing layer delegated to `rustyline` (Tier 3: history, key bindings,
//! syntax highlighting, and `_G`/field completion).
//!
//! The Lua semantics stay here and stay ours: continuation detection is the
//! upstream `<eof>` mechanism (try-compile each accumulated buffer; a syntax
//! error whose message ends in `<eof>` means "ask for another line"), and bare
//! expressions auto-print via the `return <expr>;` trick. `rustyline` only
//! supplies one edited physical line per `readline`, exactly as C-Lua linked
//! against readline does — which is why continuation is handled in our loop and
//! not in a `rustyline::Validator`.

use std::borrow::Cow;
use std::cell::RefCell;
use std::io::Write;
use std::path::PathBuf;
use std::rc::Rc;

use rustyline::completion::Completer;
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use rustyline::history::FileHistory;
use rustyline::{CompletionType, Config, Context, Editor, Helper};

use lua_stdlib::auxlib::load_buffer;
use lua_types::value::LuaValue;
use lua_types::LuaType;
use lua_vm::api;
use lua_vm::state::LuaState;

use crate::interp::{self, Cli};

/// `LUA_MULTRET`.
const MULTRET: i32 = -1;

/// Lua 5.4 reserved words — seed the completion set and drive keyword
/// highlighting.
const KEYWORDS: &[&str] = &[
    "and", "break", "do", "else", "elseif", "end", "false", "for", "function",
    "goto", "if", "in", "local", "nil", "not", "or", "repeat", "return", "then",
    "true", "until", "while",
];

/// Outcome of reading and compiling one logical line (a primary line plus any
/// continuation lines).
enum Line {
    /// Ctrl-D at the primary prompt: leave the REPL.
    Eof,
    /// Ctrl-C, or Ctrl-D mid-continuation: discard and re-prompt.
    Aborted,
    /// A compiled chunk is on the stack, ready to call.
    Compiled,
    /// A (complete) syntax error message is on the stack.
    CompileError,
}

/// Result of prompting for and reading a single physical line.
enum Read {
    Eof,
    Aborted,
    Line(Vec<u8>),
}

/// `~/.lua_history`.
fn history_path() -> Option<PathBuf> {
    #[cfg(unix)]
    let home = std::env::var_os("HOME");
    #[cfg(not(unix))]
    let home = std::env::var_os("USERPROFILE");
    home.map(|h| PathBuf::from(h).join(".lua_history"))
}

/// `lua.c`: `get_prompt` — `_PROMPT`/`_PROMPT2` if set, else the defaults.
fn get_prompt(state: &mut LuaState, firstline: bool) -> Vec<u8> {
    let name: &[u8] = if firstline { b"_PROMPT" } else { b"_PROMPT2" };
    let default: Vec<u8> = if firstline { b"> ".to_vec() } else { b">> ".to_vec() };
    let ty = api::get_global(state, name).unwrap_or(LuaType::Nil);
    let prompt = if ty == LuaType::Nil {
        default
    } else {
        match api::to_lua_string(state, -1) {
            Ok(Some(s)) => s.as_bytes().to_vec(),
            _ => default,
        }
    };
    state.pop_n(1);
    prompt
}

/// `lua.c`: `pushline` — show the prompt and read one line. Applies the Lua 5.2
/// `=expr` → `return expr` compatibility shim on the primary line.
fn read_line(
    state: &mut LuaState,
    editor: &mut Editor<LuaHelper, FileHistory>,
    firstline: bool,
) -> Read {
    let prompt = get_prompt(state, firstline);
    let prompt = String::from_utf8_lossy(&prompt).into_owned();
    match editor.readline(&prompt) {
        Ok(line) => {
            let bytes = line.into_bytes();
            if firstline && bytes.first() == Some(&b'=') {
                let mut r = b"return ".to_vec();
                r.extend_from_slice(&bytes[1..]);
                Read::Line(r)
            } else {
                Read::Line(bytes)
            }
        }
        Err(ReadlineError::Eof) => Read::Eof,
        Err(ReadlineError::Interrupted) => Read::Aborted,
        Err(_) => Read::Eof,
    }
}

/// `lua.c`: `incomplete` — true if the failed-load error on the stack ends with
/// the `<eof>` marker (an incomplete statement). Pops the error in that case.
fn incomplete_pop(state: &mut LuaState) -> bool {
    if let Ok(Some(s)) = api::to_lua_string(state, -1) {
        if s.as_bytes().ends_with(b"<eof>") {
            state.pop_n(1);
            return true;
        }
    }
    false
}

/// `lua.c`: `loadline` — read a line, try it as `return <expr>;` (so bare
/// expressions print), then as a statement with continuation lines until the
/// buffer compiles or fails for real.
fn loadline(state: &mut LuaState, editor: &mut Editor<LuaHelper, FileHistory>) -> Line {
    let _ = api::set_top(state, 0);
    let first = match read_line(state, editor, true) {
        Read::Eof => return Line::Eof,
        Read::Aborted => return Line::Aborted,
        Read::Line(s) => s,
    };

    let mut retline = b"return ".to_vec();
    retline.extend_from_slice(&first);
    retline.extend_from_slice(b";");
    match load_buffer(state, &retline, b"=stdin") {
        Ok(0) => {
            save_history(editor, &first);
            return Line::Compiled;
        }
        Ok(_) => {
            state.pop_n(1);
        }
        Err(_) => {}
    }

    let mut buffer = first;
    loop {
        match load_buffer(state, &buffer, b"=stdin") {
            Ok(0) => {
                save_history(editor, &buffer);
                return Line::Compiled;
            }
            Ok(_) => {
                if incomplete_pop(state) {
                    match read_line(state, editor, false) {
                        Read::Line(cont) => {
                            buffer.push(b'\n');
                            buffer.extend_from_slice(&cont);
                        }
                        Read::Aborted => return Line::Aborted,
                        Read::Eof => {
                            let _ = load_buffer(state, &buffer, b"=stdin");
                            save_history(editor, &buffer);
                            return Line::CompileError;
                        }
                    }
                } else {
                    save_history(editor, &buffer);
                    return Line::CompileError;
                }
            }
            Err(_) => {
                save_history(editor, &buffer);
                return Line::CompileError;
            }
        }
    }
}

fn save_history(editor: &mut Editor<LuaHelper, FileHistory>, line: &[u8]) {
    if line.is_empty() {
        return;
    }
    let _ = editor.add_history_entry(String::from_utf8_lossy(line).into_owned());
}

/// `lua.c`: `l_print` — print whatever the chunk returned by calling the global
/// `print` on the results.
fn l_print(state: &mut LuaState) {
    let n = api::get_top(state);
    if n == 0 {
        return;
    }
    let _ = api::check_stack(state, 20);
    if api::get_global(state, b"print").is_err() {
        return;
    }
    if state.insert(1).is_err() {
        return;
    }
    if let Err(e) = api::pcall_k(state, n, 0, 0, 0, None) {
        let msg = format!(
            "error calling 'print' ({})",
            String::from_utf8_lossy(&interp::error_bytes(&e))
        );
        interp::l_message(None, msg.as_bytes());
    }
}

/// `lua.c`: `doREPL` — read, evaluate, print, repeat. `progname` is cleared for
/// the duration so prompt-time errors are not prefixed.
pub(crate) fn do_repl(state: &mut LuaState, cli: &mut Cli) {
    let saved_progname = cli.progname.take();

    let completions: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let mut editor = match build_editor(completions.clone()) {
        Ok(e) => e,
        Err(e) => {
            interp::l_message(Some(b"lua"), format!("cannot start REPL: {}", e).as_bytes());
            cli.progname = saved_progname;
            return;
        }
    };
    let hist = history_path();
    if let Some(ref p) = hist {
        let _ = editor.load_history(p);
    }

    loop {
        refresh_completions(state, &completions);
        match loadline(state, &mut editor) {
            Line::Eof => break,
            Line::Aborted => continue,
            Line::Compiled => {
                let r = cli.docall(state, 0, MULTRET);
                if r.is_ok() {
                    l_print(state);
                } else {
                    cli.report(r);
                }
            }
            Line::CompileError => {
                cli.report_stack_error(state);
            }
        }
    }

    if let Some(ref p) = hist {
        let _ = editor.save_history(p);
    }
    let _ = api::set_top(state, 0);
    let _ = writeln!(std::io::stdout());
    cli.progname = saved_progname;
}

fn build_editor(
    completions: Rc<RefCell<Vec<String>>>,
) -> rustyline::Result<Editor<LuaHelper, FileHistory>> {
    let config = Config::builder()
        .auto_add_history(false)
        .completion_type(CompletionType::List)
        .build();
    let mut editor: Editor<LuaHelper, FileHistory> = Editor::with_config(config)?;
    editor.set_helper(Some(LuaHelper { completions }));
    Ok(editor)
}

/// Rebuild the completion set from the live globals: every string key of `_G`,
/// plus one level of `name.field` for table-valued globals. Refreshed before
/// each prompt so user-defined globals appear.
fn refresh_completions(state: &mut LuaState, completions: &Rc<RefCell<Vec<String>>>) {
    let mut names: Vec<String> = KEYWORDS.iter().map(|s| s.to_string()).collect();

    if api::get_global(state, b"_G").unwrap_or(LuaType::Nil) == LuaType::Table {
        let top_level = iter_string_keys(state);
        state.pop_n(1);
        for (name, _) in &top_level {
            names.push(name.clone());
        }
        for (name, is_table) in &top_level {
            if *is_table && name != "_G" && name != "package" {
                if api::get_global(state, name.as_bytes()).unwrap_or(LuaType::Nil)
                    == LuaType::Table
                {
                    for (field, _) in iter_string_keys(state) {
                        names.push(format!("{}.{}", name, field));
                    }
                }
                state.pop_n(1);
            }
        }
    } else {
        state.pop_n(1);
    }

    names.sort();
    names.dedup();
    *completions.borrow_mut() = names;
}

/// Iterate the table at the top of the stack, returning each string key and
/// whether its value is a table. Leaves the stack as it found it (table on
/// top).
fn iter_string_keys(state: &mut LuaState) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let t_idx = api::get_top(state);
    state.push(LuaValue::Nil);
    while api::next(state, t_idx).unwrap_or(false) {
        if api::lua_type_at(state, -2) == LuaType::String {
            if let Ok(Some(s)) = api::to_lua_string(state, -2) {
                if let Ok(name) = String::from_utf8(s.as_bytes().to_vec()) {
                    let is_table = api::lua_type_at(state, -1) == LuaType::Table;
                    out.push((name, is_table));
                }
            }
        }
        state.pop_n(1);
    }
    out
}

/// rustyline helper: keyword/string/comment/number highlighting and prefix
/// completion against the refreshed global/field set.
struct LuaHelper {
    completions: Rc<RefCell<Vec<String>>>,
}

impl Completer for LuaHelper {
    type Candidate = String;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<String>)> {
        let bytes = line.as_bytes();
        let mut start = pos;
        while start > 0 {
            let c = bytes[start - 1];
            if c.is_ascii_alphanumeric() || c == b'_' || c == b'.' {
                start -= 1;
            } else {
                break;
            }
        }
        let prefix = &line[start..pos];
        if prefix.is_empty() {
            return Ok((pos, Vec::new()));
        }
        let comps = self.completions.borrow();
        let matches: Vec<String> = comps
            .iter()
            .filter(|c| c.starts_with(prefix))
            .cloned()
            .collect();
        Ok((start, matches))
    }
}

impl Hinter for LuaHelper {
    type Hint = String;
}

impl Validator for LuaHelper {}

impl Highlighter for LuaHelper {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        if line.is_empty() {
            return Cow::Borrowed(line);
        }
        Cow::Owned(highlight_lua(line))
    }

    fn highlight_char(&self, _line: &str, _pos: usize, _forced: bool) -> bool {
        true
    }
}

impl Helper for LuaHelper {}

/// Colourise one line of Lua with ANSI escapes. Operates on bytes for token
/// classification but slices the original `&str` at ASCII token boundaries so
/// multibyte UTF-8 inside strings/comments is preserved.
fn highlight_lua(line: &str) -> String {
    const KW: &str = "\x1b[35m";
    const STR: &str = "\x1b[32m";
    const COM: &str = "\x1b[90m";
    const NUM: &str = "\x1b[36m";
    const RST: &str = "\x1b[0m";

    let b = line.as_bytes();
    let n = b.len();
    let mut out = String::with_capacity(n + 16);
    let mut i = 0;
    while i < n {
        let c = b[i];
        if c == b'-' && i + 1 < n && b[i + 1] == b'-' {
            out.push_str(COM);
            out.push_str(&line[i..]);
            out.push_str(RST);
            break;
        } else if c == b'"' || c == b'\'' {
            let quote = c;
            let start = i;
            i += 1;
            while i < n {
                if b[i] == b'\\' && i + 1 < n {
                    i += 2;
                    continue;
                }
                if b[i] == quote {
                    i += 1;
                    break;
                }
                i += 1;
            }
            out.push_str(STR);
            out.push_str(&line[start..i]);
            out.push_str(RST);
        } else if c.is_ascii_digit() {
            let start = i;
            while i < n && (b[i].is_ascii_alphanumeric() || b[i] == b'.') {
                i += 1;
            }
            out.push_str(NUM);
            out.push_str(&line[start..i]);
            out.push_str(RST);
        } else if c.is_ascii_alphabetic() || c == b'_' {
            let start = i;
            while i < n && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            let word = &line[start..i];
            if KEYWORDS.contains(&word) {
                out.push_str(KW);
                out.push_str(word);
                out.push_str(RST);
            } else {
                out.push_str(word);
            }
        } else {
            let ch = line[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

// ──────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        reference/lua-5.4.7/src/lua.c (doREPL and friends)
//   target_crate:  lua-cli
//   confidence:    medium
//   todos:         2  (in-execution Ctrl-C interrupt of a running chunk is
//                      deferred: the debug-hook closure returns () so a hook
//                      cannot raise "interrupted!" the way lua.c's lstop does;
//                      that needs a hook-ABI change in lua-vm. Ctrl-C at the
//                      prompt works via rustyline. field completion is one
//                      level of table fields deep)
//   port_notes:    2  (continuation handled in our loop, not a rustyline
//                      Validator, mirroring C readline's line-at-a-time model;
//                      highlighting uses a self-contained scanner because
//                      lua-lex requires &mut LuaState per token)
//   unsafe_blocks: 0
//   notes:         loadline/addreturn/multiline/incomplete reproduce the
//                  upstream <eof> continuation contract against load_buffer.
//                  rustyline supplies editing, history (~/.lua_history),
//                  highlighting, and completion over a per-prompt snapshot of
//                  _G plus one level of table fields.
// ──────────────────────────────────────────────────────────────────────────
