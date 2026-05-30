//! Shared sandbox helpers: the canonical strict capability-strip list and a
//! global-stripping routine, used by both the CLI `--sandbox` flag and
//! `lua-rs-runtime`'s `SandboxConfig::strict()` so the dangerous-globals list
//! has a single source of truth.
//!
//! The instruction and memory budget itself is installed separately via
//! [`lua_vm::state::LuaState::install_sandbox_limits`]; capability stripping is
//! defense-in-depth on top of the host-hook gating.

use lua_types::value::LuaValue;
use lua_types::LuaError;

use crate::state_stub::LuaState;

/// Globals removed by the strict sandbox preset: the code-loading and
/// host-access surfaces. A `.`-separated entry nils a field of a sub-table
/// (e.g. `os.execute`); a bare name nils a top-level global.
pub const STRICT_REMOVED_GLOBALS: &[&[u8]] = &[
    b"dofile",
    b"loadfile",
    b"load",
    b"loadstring",
    b"require",
    b"package",
    b"io",
    b"debug",
    b"os.execute",
    b"os.exit",
    b"os.remove",
    b"os.rename",
    b"os.tmpname",
    b"os.getenv",
    b"os.setlocale",
];

/// Delete the named globals from `_G`. Each entry is either a bare global name
/// or a `head.tail` path naming a field of a sub-table.
pub fn strip_globals(state: &mut LuaState, names: &[&[u8]]) -> Result<(), LuaError> {
    let globals = match state.global().globals.clone() {
        LuaValue::Table(t) => t,
        _ => return Ok(()),
    };
    for name in names {
        match name.iter().position(|&b| b == b'.') {
            Some(dot) => {
                let head = &name[..dot];
                let tail = &name[dot + 1..];
                if let LuaValue::Table(sub) = globals.get_str_bytes(head) {
                    let key = LuaValue::Str(state.new_string(tail)?);
                    sub.raw_set(key, LuaValue::Nil);
                }
            }
            None => {
                let key = LuaValue::Str(state.new_string(name)?);
                globals.raw_set(key, LuaValue::Nil);
            }
        }
    }
    Ok(())
}
