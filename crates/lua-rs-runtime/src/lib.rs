//! Embedding helper for lua-rs.
//!
//! This crate sits above `lua-vm`, `lua-stdlib`, and `lua-parse`, so it can
//! provide the common setup sequence without creating dependency cycles:
//! create a state, install the parser hook, install host hooks, open stdlib,
//! and run chunks.

use lua_stdlib::auxlib::load_buffer;
use lua_stdlib::init::open_libs;
use lua_types::closure::LuaLClosure;
use lua_types::gc::GcRef;
use lua_types::upval::UpVal;
use lua_types::value::LuaValue;
use lua_vm::state::{
    new_state, DynLibLoadHook, DynLibSymbolHook, DynLibUnloadHook, EntropyHook, EnvHook,
    FileLoaderHook, FileOpenHook, FileRemoveHook, FileRenameHook, InputHook, LuaState,
    OsExecuteHook, OutputHook, PopenHook, TempNameHook, UnixTimeHook,
};

pub use lua_types::{LuaError, LuaFileHandle};
pub use lua_vm::state::{DynLibId, DynamicSymbol, OsExecuteReason, OsExecuteResult};

/// Host capabilities exposed to Lua stdlib.
///
/// Every field is optional. Missing file, process, and dynamic-loading hooks
/// produce Lua errors or Lua failure tuples. On bare `wasm32-unknown-unknown`,
/// missing stdio/time/env/temp hooks avoid unsupported Rust `std` stubs and fail
/// at the Lua boundary. Native builds may still use compatibility fallbacks for
/// some stdio and OS functions when hooks are absent.
#[derive(Clone, Copy, Default)]
pub struct HostHooks {
    pub file_loader_hook: Option<FileLoaderHook>,
    pub file_open_hook: Option<FileOpenHook>,
    pub stdin_hook: Option<InputHook>,
    pub stdout_hook: Option<OutputHook>,
    pub stderr_hook: Option<OutputHook>,
    pub env_hook: Option<EnvHook>,
    pub unix_time_hook: Option<UnixTimeHook>,
    pub entropy_hook: Option<EntropyHook>,
    pub temp_name_hook: Option<TempNameHook>,
    pub popen_hook: Option<PopenHook>,
    pub file_remove_hook: Option<FileRemoveHook>,
    pub file_rename_hook: Option<FileRenameHook>,
    pub os_execute_hook: Option<OsExecuteHook>,
    pub dynlib_load_hook: Option<DynLibLoadHook>,
    pub dynlib_symbol_hook: Option<DynLibSymbolHook>,
    pub dynlib_unload_hook: Option<DynLibUnloadHook>,
}

impl HostHooks {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn file_loader(mut self, hook: FileLoaderHook) -> Self {
        self.file_loader_hook = Some(hook);
        self
    }

    pub fn file_open(mut self, hook: FileOpenHook) -> Self {
        self.file_open_hook = Some(hook);
        self
    }

    pub fn stdin(mut self, hook: InputHook) -> Self {
        self.stdin_hook = Some(hook);
        self
    }

    pub fn stdout(mut self, hook: OutputHook) -> Self {
        self.stdout_hook = Some(hook);
        self
    }

    pub fn stderr(mut self, hook: OutputHook) -> Self {
        self.stderr_hook = Some(hook);
        self
    }

    pub fn env(mut self, hook: EnvHook) -> Self {
        self.env_hook = Some(hook);
        self
    }

    pub fn unix_time(mut self, hook: UnixTimeHook) -> Self {
        self.unix_time_hook = Some(hook);
        self
    }

    pub fn entropy(mut self, hook: EntropyHook) -> Self {
        self.entropy_hook = Some(hook);
        self
    }

    pub fn temp_name(mut self, hook: TempNameHook) -> Self {
        self.temp_name_hook = Some(hook);
        self
    }

    pub fn popen(mut self, hook: PopenHook) -> Self {
        self.popen_hook = Some(hook);
        self
    }

    pub fn file_remove(mut self, hook: FileRemoveHook) -> Self {
        self.file_remove_hook = Some(hook);
        self
    }

    pub fn file_rename(mut self, hook: FileRenameHook) -> Self {
        self.file_rename_hook = Some(hook);
        self
    }

    pub fn os_execute(mut self, hook: OsExecuteHook) -> Self {
        self.os_execute_hook = Some(hook);
        self
    }

    pub fn dynlib_load(mut self, hook: DynLibLoadHook) -> Self {
        self.dynlib_load_hook = Some(hook);
        self
    }

    pub fn dynlib_symbol(mut self, hook: DynLibSymbolHook) -> Self {
        self.dynlib_symbol_hook = Some(hook);
        self
    }

    pub fn dynlib_unload(mut self, hook: DynLibUnloadHook) -> Self {
        self.dynlib_unload_hook = Some(hook);
        self
    }

    pub fn install(self, state: &mut LuaState) {
        let global = &mut *state.global_mut();
        global.file_loader_hook = self.file_loader_hook;
        global.file_open_hook = self.file_open_hook;
        global.stdin_hook = self.stdin_hook;
        global.stdout_hook = self.stdout_hook;
        global.stderr_hook = self.stderr_hook;
        global.env_hook = self.env_hook;
        global.unix_time_hook = self.unix_time_hook;
        global.entropy_hook = self.entropy_hook;
        global.temp_name_hook = self.temp_name_hook;
        global.popen_hook = self.popen_hook;
        global.file_remove_hook = self.file_remove_hook;
        global.file_rename_hook = self.file_rename_hook;
        global.os_execute_hook = self.os_execute_hook;
        global.dynlib_load_hook = self.dynlib_load_hook;
        global.dynlib_symbol_hook = self.dynlib_symbol_hook;
        global.dynlib_unload_hook = self.dynlib_unload_hook;
    }
}

/// A Lua state with parser and standard libraries installed.
pub struct LuaRuntime {
    state: LuaState,
}

impl LuaRuntime {
    /// Create a Lua runtime with parser and standard libraries installed.
    ///
    /// This installs no explicit host hooks. For a strict sandbox, construct
    /// with [`LuaRuntime::with_hooks`] and audit the native compatibility
    /// fallbacks in `lua-stdlib`.
    pub fn new() -> Result<Self, LuaError> {
        Self::with_hooks(HostHooks::default())
    }

    /// Create a Lua runtime with the supplied host capabilities.
    pub fn with_hooks(hooks: HostHooks) -> Result<Self, LuaError> {
        let mut state = new_state().ok_or(LuaError::Memory)?;
        install_parser_hook(&mut state);
        hooks.install(&mut state);
        open_libs(&mut state)?;
        Ok(Self { state })
    }

    pub fn state(&self) -> &LuaState {
        &self.state
    }

    pub fn state_mut(&mut self) -> &mut LuaState {
        &mut self.state
    }

    pub fn into_state(self) -> LuaState {
        self.state
    }

    /// Load and execute a Lua source chunk.
    pub fn exec(&mut self, source: &[u8], name: &[u8]) -> Result<(), LuaError> {
        let status = load_buffer(&mut self.state, source, name)?;
        if status != 0 {
            let err = self.state.pop();
            return Err(LuaError::from_value(err));
        }
        lua_vm::api::pcall_k(&mut self.state, 0, 0, 0, 0, None)?;
        Ok(())
    }
}

pub fn install_parser_hook(state: &mut LuaState) {
    state.global_mut().parser_hook = Some(parser_hook);
}

fn parser_hook(
    state: &mut LuaState,
    source: &[u8],
    name: &[u8],
    firstchar: i32,
) -> Result<GcRef<LuaLClosure>, LuaError> {
    let proto = lua_parse::parse(
        state,
        lua_parse::DynData::default(),
        source,
        name,
        firstchar,
    )?;
    let nupvals = proto.upvalues.len();
    let mut upvals = Vec::with_capacity(nupvals);
    for _ in 0..nupvals {
        upvals.push(std::cell::Cell::new(GcRef::new(UpVal::closed(
            LuaValue::Nil,
        ))));
    }
    Ok(GcRef::new(LuaLClosure {
        proto: GcRef::new(*proto),
        upvals,
    }))
}
