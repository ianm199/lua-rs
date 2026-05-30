//! Embedding helper for lua-rs.
//!
//! This crate sits above `lua-vm`, `lua-stdlib`, and `lua-parse`, so it can
//! provide the common setup sequence without creating dependency cycles:
//! create a state, install the parser hook, install host hooks, open stdlib,
//! and run chunks.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::fmt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::rc::Rc;

use lua_stdlib::auxlib::load_buffer;
use lua_stdlib::init::open_libs;
use lua_types::closure::LuaLClosure;
use lua_types::gc::GcRef;
use lua_types::string::LuaString as RawLuaString;
use lua_types::upval::UpVal;
use lua_types::value::{LuaTable as RawLuaTable, LuaValue as RawLuaValue};
use lua_vm::state::{
    new_state, CpuClockHook, DynLibLoadHook, DynLibSymbolHook, DynLibUnloadHook, EntropyHook,
    EnvHook, ExternalRootKey, FileLoaderHook, FileOpenHook, FileRemoveHook, FileRenameHook,
    InputHook, LuaCallable, LuaRustFunction, LuaState, OsExecuteHook, OutputHook, PopenHook,
    TempNameHook, UnixTimeHook,
};

pub use lua_types::{LuaError, LuaFileHandle};
pub use lua_vm::state::{DynLibId, DynamicSymbol, OsExecuteReason, OsExecuteResult};

pub type Result<T> = std::result::Result<T, LuaError>;

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
    pub cpu_clock_hook: Option<CpuClockHook>,
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

    pub fn cpu_clock(mut self, hook: CpuClockHook) -> Self {
        self.cpu_clock_hook = Some(hook);
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
        global.cpu_clock_hook = self.cpu_clock_hook;
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

/// Primary owned embedding handle.
///
/// `Lua` is intentionally cheap to clone and single-threaded. State access is
/// borrowed at the embedding boundary only; opcode dispatch still runs with
/// direct `&mut LuaState` access. Captured Rust callbacks will need a call-path
/// adapter that releases this boundary borrow before invoking user code.
#[derive(Clone)]
pub struct Lua {
    inner: Rc<LuaInner>,
}

struct LuaInner {
    state: RefCell<LuaState>,
    active_state: Cell<*mut LuaState>,
}

struct ActiveStateGuard<'a> {
    inner: &'a LuaInner,
    previous: *mut LuaState,
}

impl Drop for ActiveStateGuard<'_> {
    fn drop(&mut self) {
        self.inner.active_state.set(self.previous);
    }
}

impl LuaInner {
    fn enter_active(&self, state: *mut LuaState) -> ActiveStateGuard<'_> {
        let previous = self.active_state.replace(state);
        ActiveStateGuard {
            inner: self,
            previous,
        }
    }
}

impl fmt::Debug for Lua {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Lua").finish_non_exhaustive()
    }
}

impl Lua {
    /// Create a Lua runtime with parser and standard libraries installed.
    pub fn new() -> Result<Self> {
        Self::with_hooks(HostHooks::default())
    }

    /// Create a Lua runtime with the supplied host capabilities.
    pub fn with_hooks(hooks: HostHooks) -> Result<Self> {
        let mut state = new_state().ok_or(LuaError::Memory)?;
        install_parser_hook(&mut state);
        hooks.install(&mut state);
        open_libs(&mut state)?;
        Ok(Self::from_initialized_state(state))
    }

    fn from_initialized_state(state: LuaState) -> Self {
        Lua {
            inner: Rc::new(LuaInner {
                state: RefCell::new(state),
                active_state: Cell::new(std::ptr::null_mut()),
            }),
        }
    }

    fn with_state<R>(&self, f: impl FnOnce(&mut LuaState) -> R) -> R {
        if let Ok(mut state) = self.inner.state.try_borrow_mut() {
            let _active = self.inner.enter_active(&mut *state);
            return f(&mut state);
        }

        let state = self.inner.active_state.get();
        assert!(
            !state.is_null(),
            "re-entrant Lua access without an active state"
        );

        // SAFETY: `active_state` is set only while this `Lua` owns the outer
        // `RefCell` borrow and is executing VM code. Re-entrant access can only
        // happen when that VM frame has synchronously transferred control to a
        // Rust callback and is suspended. The callback path does not touch the
        // suspended `&mut LuaState` while user code re-enters through `Lua`.
        unsafe { f(&mut *state) }
    }

    fn root_raw(&self, value: RawLuaValue) -> RootedValue {
        let key = self.with_state(|state| state.external_root_value(value));
        RootedValue {
            lua: self.clone(),
            key,
        }
    }

    fn root_raw_in_state(&self, state: &mut LuaState, value: RawLuaValue) -> RootedValue {
        let key = state.external_root_value(value);
        RootedValue {
            lua: self.clone(),
            key,
        }
    }

    /// Load a Lua source chunk.
    pub fn load(&self, source: impl AsRef<[u8]>) -> Chunk {
        Chunk {
            lua: self.clone(),
            source: source.as_ref().to_vec(),
            name: b"chunk".to_vec(),
        }
    }

    /// Return the global environment table.
    pub fn globals(&self) -> Table {
        let raw = self.with_state(|state| state.global().globals.clone());
        Table {
            root: self.root_raw(raw),
        }
    }

    /// Create a new empty table.
    pub fn create_table(&self) -> Result<Table> {
        let root = self.with_state(|state| {
            let table = state.new_table();
            let raw = RawLuaValue::Table(table);
            let key = state.external_root_value(raw);
            state.gc().check_step();
            RootedValue {
                lua: self.clone(),
                key,
            }
        });
        Ok(Table { root })
    }

    /// Create a new Lua string from bytes.
    pub fn create_string(&self, bytes: impl AsRef<[u8]>) -> Result<LuaString> {
        let bytes = bytes.as_ref();
        let root = self.with_state(|state| {
            let string = state.new_string(bytes)?;
            let raw = RawLuaValue::Str(string);
            let key = state.external_root_value(raw);
            state.gc().check_step();
            Ok::<_, LuaError>(RootedValue {
                lua: self.clone(),
                key,
            })
        })?;
        Ok(LuaString { root })
    }

    pub fn create_function<A, R, F>(&self, func: F) -> Result<Function>
    where
        A: FromLuaMulti + 'static,
        R: IntoLuaMulti + 'static,
        F: Fn(&Lua, A) -> Result<R> + 'static,
    {
        let lua = self.clone();
        let callable: LuaRustFunction = Rc::new(move |state| {
            match catch_unwind(AssertUnwindSafe(|| {
                let args = callback_args(state, &lua)?;
                let args = A::from_lua_multi(args, &lua)?;
                let returns = func(&lua, args)?;
                let returns = returns.into_lua_multi(&lua)?;
                push_callback_returns(state, &lua, returns)
            })) {
                Ok(result) => result,
                Err(_) => Err(LuaError::runtime(format_args!("Rust callback panicked"))),
            }
        });
        self.create_registered_function(callable)
    }

    pub fn create_function_mut<A, R, F>(&self, func: F) -> Result<Function>
    where
        A: FromLuaMulti + 'static,
        R: IntoLuaMulti + 'static,
        F: FnMut(&Lua, A) -> Result<R> + 'static,
    {
        let func = RefCell::new(func);
        self.create_function(move |lua, args| {
            let mut func = func.try_borrow_mut().map_err(|_| {
                LuaError::runtime(format_args!("mutable Rust callback is already borrowed"))
            })?;
            func(lua, args)
        })
    }

    fn create_registered_function(&self, callable: LuaRustFunction) -> Result<Function> {
        let root = self.with_state(|state| {
            let idx = {
                let mut global = state.global_mut();
                let idx = global.c_functions.len();
                global.c_functions.push(LuaCallable::rust(callable));
                idx
            };
            let raw = RawLuaValue::Function(lua_types::closure::LuaClosure::LightC(idx));
            let key = state.external_root_value(raw);
            RootedValue {
                lua: self.clone(),
                key,
            }
        });
        Ok(Function { root })
    }

    /// Run a full garbage-collection cycle.
    pub fn gc_collect(&self) {
        self.with_state(|state| state.gc().full_collect());
    }
}

pub struct Chunk {
    lua: Lua,
    source: Vec<u8>,
    name: Vec<u8>,
}

impl Chunk {
    pub fn set_name(mut self, name: impl AsRef<[u8]>) -> Self {
        self.name = name.as_ref().to_vec();
        self
    }

    pub fn exec(self) -> Result<()> {
        self.lua
            .with_state(|state| exec_state(state, &self.source, &self.name))
    }

    pub fn eval<T: FromLuaMulti>(self) -> Result<T> {
        let raws = self.lua.with_state(|state| {
            let saved_top = state.top_idx();
            let status = load_buffer(state, &self.source, &self.name)?;
            if status != 0 {
                let err = state.pop();
                state.set_top_idx(saved_top);
                return Err(LuaError::from_value(err));
            }
            match lua_vm::api::pcall_k(state, 0, T::NRESULTS, 0, 0, None) {
                Ok(_) => {
                    let mut values = Vec::with_capacity(T::NRESULTS.max(0) as usize);
                    for _ in 0..T::NRESULTS.max(0) {
                        values.push(state.pop());
                    }
                    values.reverse();
                    state.set_top_idx(saved_top);
                    Ok(values)
                }
                Err(err) => {
                    state.set_top_idx(saved_top);
                    Err(err)
                }
            }
        })?;
        let values = raws
            .into_iter()
            .map(|raw| Value::from_raw(&self.lua, raw))
            .collect::<Result<Vec<_>>>()?;
        T::from_lua_multi(values, &self.lua)
    }
}

#[derive(Debug)]
struct RootedValue {
    lua: Lua,
    key: ExternalRootKey,
}

impl RootedValue {
    fn raw(&self) -> Result<RawLuaValue> {
        self.lua
            .with_state(|state| state.external_rooted_value(self.key))
            .ok_or_else(stale_handle_error)
    }

    fn raw_for_lua(&self, lua: &Lua, state: &LuaState) -> Result<RawLuaValue> {
        if !Rc::ptr_eq(&self.lua.inner, &lua.inner) {
            return Err(LuaError::runtime(format_args!(
                "Lua handle belongs to a different state"
            )));
        }
        state
            .external_rooted_value(self.key)
            .ok_or_else(stale_handle_error)
    }
}

impl Clone for RootedValue {
    fn clone(&self) -> Self {
        let raw = self.raw().expect("rooted Lua handle should not be stale");
        self.lua.root_raw(raw)
    }
}

impl Drop for RootedValue {
    fn drop(&mut self) {
        let _ = self
            .lua
            .with_state(|state| state.external_unroot_value(self.key));
    }
}

/// Dynamically typed owned Lua value.
#[derive(Debug, Clone)]
pub enum Value {
    Nil,
    Boolean(bool),
    Integer(i64),
    Number(f64),
    String(LuaString),
    Table(Table),
    Function(Function),
    UserData(AnyUserData),
    LightUserData(*mut c_void),
    Thread(Thread),
}

impl Value {
    fn from_raw(lua: &Lua, raw: RawLuaValue) -> Result<Self> {
        lua.with_state(|state| Self::from_raw_in_state(lua, state, raw))
    }

    fn from_raw_in_state(lua: &Lua, state: &mut LuaState, raw: RawLuaValue) -> Result<Self> {
        Ok(match raw {
            RawLuaValue::Nil => Value::Nil,
            RawLuaValue::Bool(v) => Value::Boolean(v),
            RawLuaValue::Int(v) => Value::Integer(v),
            RawLuaValue::Float(v) => Value::Number(v),
            RawLuaValue::Str(v) => Value::String(LuaString {
                root: lua.root_raw_in_state(state, RawLuaValue::Str(v)),
            }),
            RawLuaValue::Table(v) => Value::Table(Table {
                root: lua.root_raw_in_state(state, RawLuaValue::Table(v)),
            }),
            RawLuaValue::Function(v) => Value::Function(Function {
                root: lua.root_raw_in_state(state, RawLuaValue::Function(v)),
            }),
            RawLuaValue::UserData(v) => Value::UserData(AnyUserData {
                root: lua.root_raw_in_state(state, RawLuaValue::UserData(v)),
            }),
            RawLuaValue::LightUserData(v) => Value::LightUserData(v),
            RawLuaValue::Thread(v) => Value::Thread(Thread {
                root: lua.root_raw_in_state(state, RawLuaValue::Thread(v)),
            }),
        })
    }

    fn to_raw_for_lua(&self, lua: &Lua, state: &LuaState) -> Result<RawLuaValue> {
        match self {
            Value::Nil => Ok(RawLuaValue::Nil),
            Value::Boolean(v) => Ok(RawLuaValue::Bool(*v)),
            Value::Integer(v) => Ok(RawLuaValue::Int(*v)),
            Value::Number(v) => Ok(RawLuaValue::Float(*v)),
            Value::String(v) => v.root.raw_for_lua(lua, state),
            Value::Table(v) => v.root.raw_for_lua(lua, state),
            Value::Function(v) => v.root.raw_for_lua(lua, state),
            Value::UserData(v) => v.root.raw_for_lua(lua, state),
            Value::LightUserData(v) => Ok(RawLuaValue::LightUserData(*v)),
            Value::Thread(v) => v.root.raw_for_lua(lua, state),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Table {
    root: RootedValue,
}

impl Table {
    fn raw_table(&self) -> Result<GcRef<RawLuaTable>> {
        match self.root.raw()? {
            RawLuaValue::Table(table) => Ok(table),
            other => Err(type_error_raw(&other, "table")),
        }
    }

    pub fn get<K, V>(&self, key: K) -> Result<V>
    where
        K: IntoLua,
        V: FromLua,
    {
        let lua = self.root.lua.clone();
        let key = key.into_lua(&lua)?;
        let value_raw = lua.with_state(|state| {
            let key_raw = key.to_raw_for_lua(&lua, state)?;
            let table_raw = self.root.raw_for_lua(&lua, state)?;
            state.table_get_with_tm(&table_raw, &key_raw)
        })?;
        let value = Value::from_raw(&lua, value_raw)?;
        V::from_lua(value, &lua)
    }

    pub fn set<K, V>(&self, key: K, value: V) -> Result<()>
    where
        K: IntoLua,
        V: IntoLua,
    {
        let lua = self.root.lua.clone();
        let key = key.into_lua(&lua)?;
        let value = value.into_lua(&lua)?;
        lua.with_state(|state| {
            let key_raw = key.to_raw_for_lua(&lua, state)?;
            let value_raw = value.to_raw_for_lua(&lua, state)?;
            let table_raw = self.root.raw_for_lua(&lua, state)?;
            state.table_set_with_tm(&table_raw, key_raw, value_raw)
        })
    }

    pub fn len(&self) -> Result<u64> {
        Ok(self.raw_table()?.getn())
    }
}

#[derive(Debug, Clone)]
pub struct Function {
    root: RootedValue,
}

impl Function {
    pub fn call<A, R>(&self, args: A) -> Result<R>
    where
        A: IntoLuaMulti,
        R: FromLuaMulti,
    {
        let lua = self.root.lua.clone();
        let args = args.into_lua_multi(&lua)?;
        let result_raws = lua.with_state(|state| {
            let arg_raws = args
                .iter()
                .map(|value| value.to_raw_for_lua(&lua, state))
                .collect::<Result<Vec<_>>>()?;
            let function_raw = self.root.raw_for_lua(&lua, state)?;
            let saved_top = state.top_idx();
            state.push(function_raw);
            for arg in &arg_raws {
                state.push(*arg);
            }
            match lua_vm::api::pcall_k(state, arg_raws.len() as i32, R::NRESULTS, 0, 0, None) {
                Ok(_) => {
                    let mut results = Vec::with_capacity(R::NRESULTS.max(0) as usize);
                    for _ in 0..R::NRESULTS.max(0) {
                        results.push(state.pop());
                    }
                    results.reverse();
                    state.set_top_idx(saved_top);
                    Ok(results)
                }
                Err(err) => {
                    state.set_top_idx(saved_top);
                    Err(err)
                }
            }
        })?;
        let values = result_raws
            .into_iter()
            .map(|raw| Value::from_raw(&lua, raw))
            .collect::<Result<Vec<_>>>()?;
        R::from_lua_multi(values, &lua)
    }
}

#[derive(Debug, Clone)]
pub struct LuaString {
    root: RootedValue,
}

impl LuaString {
    fn raw_string(&self) -> Result<GcRef<RawLuaString>> {
        match self.root.raw()? {
            RawLuaValue::Str(string) => Ok(string),
            other => Err(type_error_raw(&other, "string")),
        }
    }

    pub fn as_bytes(&self) -> Result<Vec<u8>> {
        Ok(self.raw_string()?.as_bytes().to_vec())
    }

    pub fn to_str(&self) -> Result<String> {
        let bytes = self.as_bytes()?;
        String::from_utf8(bytes)
            .map_err(|_| LuaError::runtime(format_args!("string is not valid UTF-8")))
    }
}

#[derive(Debug, Clone)]
pub struct AnyUserData {
    root: RootedValue,
}

#[derive(Debug, Clone)]
pub struct Thread {
    root: RootedValue,
}

pub trait IntoLua {
    fn into_lua(self, lua: &Lua) -> Result<Value>;
}

pub trait FromLua: Sized {
    fn from_lua(value: Value, lua: &Lua) -> Result<Self>;
}

pub trait IntoLuaMulti {
    fn into_lua_multi(self, lua: &Lua) -> Result<Vec<Value>>;
}

pub trait FromLuaMulti: Sized {
    const NRESULTS: i32;

    fn from_lua_multi(values: Vec<Value>, lua: &Lua) -> Result<Self>;
}

impl IntoLua for Value {
    fn into_lua(self, _lua: &Lua) -> Result<Value> {
        Ok(self)
    }
}

impl IntoLua for &Value {
    fn into_lua(self, _lua: &Lua) -> Result<Value> {
        Ok(self.clone())
    }
}

impl FromLua for Value {
    fn from_lua(value: Value, _lua: &Lua) -> Result<Self> {
        Ok(value)
    }
}

impl IntoLua for bool {
    fn into_lua(self, _lua: &Lua) -> Result<Value> {
        Ok(Value::Boolean(self))
    }
}

impl FromLua for bool {
    fn from_lua(value: Value, _lua: &Lua) -> Result<Self> {
        match value {
            Value::Boolean(v) => Ok(v),
            other => Err(type_error_value(&other, "boolean")),
        }
    }
}

impl IntoLua for i64 {
    fn into_lua(self, _lua: &Lua) -> Result<Value> {
        Ok(Value::Integer(self))
    }
}

impl FromLua for i64 {
    fn from_lua(value: Value, _lua: &Lua) -> Result<Self> {
        match value {
            Value::Integer(v) => Ok(v),
            Value::Number(v) if v.fract() == 0.0 && v.is_finite() => Ok(v as i64),
            other => Err(type_error_value(&other, "integer")),
        }
    }
}

impl IntoLua for i32 {
    fn into_lua(self, lua: &Lua) -> Result<Value> {
        i64::from(self).into_lua(lua)
    }
}

impl FromLua for i32 {
    fn from_lua(value: Value, lua: &Lua) -> Result<Self> {
        let v = i64::from_lua(value, lua)?;
        i32::try_from(v).map_err(|_| LuaError::runtime(format_args!("integer out of range")))
    }
}

impl IntoLua for usize {
    fn into_lua(self, lua: &Lua) -> Result<Value> {
        let v = i64::try_from(self)
            .map_err(|_| LuaError::runtime(format_args!("integer out of range")))?;
        v.into_lua(lua)
    }
}

impl IntoLua for f64 {
    fn into_lua(self, _lua: &Lua) -> Result<Value> {
        Ok(Value::Number(self))
    }
}

impl FromLua for f64 {
    fn from_lua(value: Value, _lua: &Lua) -> Result<Self> {
        match value {
            Value::Integer(v) => Ok(v as f64),
            Value::Number(v) => Ok(v),
            other => Err(type_error_value(&other, "number")),
        }
    }
}

impl IntoLua for &str {
    fn into_lua(self, lua: &Lua) -> Result<Value> {
        Ok(Value::String(lua.create_string(self.as_bytes())?))
    }
}

impl IntoLua for String {
    fn into_lua(self, lua: &Lua) -> Result<Value> {
        Ok(Value::String(lua.create_string(self.into_bytes())?))
    }
}

impl FromLua for String {
    fn from_lua(value: Value, _lua: &Lua) -> Result<Self> {
        match value {
            Value::String(s) => s.to_str(),
            other => Err(type_error_value(&other, "string")),
        }
    }
}

impl IntoLua for &[u8] {
    fn into_lua(self, lua: &Lua) -> Result<Value> {
        Ok(Value::String(lua.create_string(self)?))
    }
}

impl IntoLua for Vec<u8> {
    fn into_lua(self, lua: &Lua) -> Result<Value> {
        Ok(Value::String(lua.create_string(self)?))
    }
}

impl IntoLua for LuaString {
    fn into_lua(self, _lua: &Lua) -> Result<Value> {
        Ok(Value::String(self))
    }
}

impl IntoLua for &LuaString {
    fn into_lua(self, _lua: &Lua) -> Result<Value> {
        Ok(Value::String(self.clone()))
    }
}

impl FromLua for LuaString {
    fn from_lua(value: Value, _lua: &Lua) -> Result<Self> {
        match value {
            Value::String(v) => Ok(v),
            other => Err(type_error_value(&other, "string")),
        }
    }
}

impl IntoLua for Table {
    fn into_lua(self, _lua: &Lua) -> Result<Value> {
        Ok(Value::Table(self))
    }
}

impl IntoLua for &Table {
    fn into_lua(self, _lua: &Lua) -> Result<Value> {
        Ok(Value::Table(self.clone()))
    }
}

impl FromLua for Table {
    fn from_lua(value: Value, _lua: &Lua) -> Result<Self> {
        match value {
            Value::Table(v) => Ok(v),
            other => Err(type_error_value(&other, "table")),
        }
    }
}

impl IntoLua for Function {
    fn into_lua(self, _lua: &Lua) -> Result<Value> {
        Ok(Value::Function(self))
    }
}

impl IntoLua for &Function {
    fn into_lua(self, _lua: &Lua) -> Result<Value> {
        Ok(Value::Function(self.clone()))
    }
}

impl FromLua for Function {
    fn from_lua(value: Value, _lua: &Lua) -> Result<Self> {
        match value {
            Value::Function(v) => Ok(v),
            other => Err(type_error_value(&other, "function")),
        }
    }
}

impl<T> IntoLua for Option<T>
where
    T: IntoLua,
{
    fn into_lua(self, lua: &Lua) -> Result<Value> {
        match self {
            Some(value) => value.into_lua(lua),
            None => Ok(Value::Nil),
        }
    }
}

impl<T> FromLua for Option<T>
where
    T: FromLua,
{
    fn from_lua(value: Value, lua: &Lua) -> Result<Self> {
        match value {
            Value::Nil => Ok(None),
            other => T::from_lua(other, lua).map(Some),
        }
    }
}

impl IntoLuaMulti for () {
    fn into_lua_multi(self, _lua: &Lua) -> Result<Vec<Value>> {
        Ok(Vec::new())
    }
}

impl<T> IntoLuaMulti for T
where
    T: IntoLua,
{
    fn into_lua_multi(self, lua: &Lua) -> Result<Vec<Value>> {
        Ok(vec![self.into_lua(lua)?])
    }
}

impl<A, B> IntoLuaMulti for (A, B)
where
    A: IntoLua,
    B: IntoLua,
{
    fn into_lua_multi(self, lua: &Lua) -> Result<Vec<Value>> {
        Ok(vec![self.0.into_lua(lua)?, self.1.into_lua(lua)?])
    }
}

impl FromLuaMulti for () {
    const NRESULTS: i32 = 0;

    fn from_lua_multi(_values: Vec<Value>, _lua: &Lua) -> Result<Self> {
        Ok(())
    }
}

impl<T> FromLuaMulti for T
where
    T: FromLua,
{
    const NRESULTS: i32 = 1;

    fn from_lua_multi(mut values: Vec<Value>, lua: &Lua) -> Result<Self> {
        let value = if values.is_empty() {
            Value::Nil
        } else {
            values.remove(0)
        };
        T::from_lua(value, lua)
    }
}

impl<A, B> FromLuaMulti for (A, B)
where
    A: FromLua,
    B: FromLua,
{
    const NRESULTS: i32 = 2;

    fn from_lua_multi(mut values: Vec<Value>, lua: &Lua) -> Result<Self> {
        let first = if values.is_empty() {
            Value::Nil
        } else {
            values.remove(0)
        };
        let second = if values.is_empty() {
            Value::Nil
        } else {
            values.remove(0)
        };
        Ok((A::from_lua(first, lua)?, B::from_lua(second, lua)?))
    }
}

fn callback_args(state: &mut LuaState, lua: &Lua) -> Result<Vec<Value>> {
    let func_idx = state.current_call_info().func;
    let nargs = state.top_idx().0.saturating_sub(func_idx.0 + 1);
    let mut args = Vec::with_capacity(nargs as usize);
    for i in 0..nargs {
        let raw = state.get_at(func_idx + 1 + i as i32);
        args.push(Value::from_raw_in_state(lua, state, raw)?);
    }
    Ok(args)
}

fn push_callback_returns(state: &mut LuaState, lua: &Lua, returns: Vec<Value>) -> Result<usize> {
    let mut count = 0usize;
    for value in returns {
        let raw = value.to_raw_for_lua(lua, state)?;
        state.push(raw);
        count += 1;
    }
    Ok(count)
}

fn stale_handle_error() -> LuaError {
    LuaError::runtime(format_args!("stale Lua handle"))
}

fn type_error_raw(value: &RawLuaValue, expected: &str) -> LuaError {
    LuaError::runtime(format_args!(
        "{} expected, got {}",
        expected,
        value.type_name()
    ))
}

fn type_error_value(value: &Value, expected: &str) -> LuaError {
    let got = match value {
        Value::Nil => "nil",
        Value::Boolean(_) => "boolean",
        Value::Integer(_) | Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Table(_) => "table",
        Value::Function(_) => "function",
        Value::UserData(_) | Value::LightUserData(_) => "userdata",
        Value::Thread(_) => "thread",
    };
    LuaError::runtime(format_args!("{} expected, got {}", expected, got))
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
    pub fn new() -> Result<Self> {
        Self::with_hooks(HostHooks::default())
    }

    /// Create a Lua runtime with the supplied host capabilities.
    pub fn with_hooks(hooks: HostHooks) -> Result<Self> {
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

    pub fn into_lua(self) -> Lua {
        Lua::from_initialized_state(self.state)
    }

    /// Load and execute a Lua source chunk.
    pub fn exec(&mut self, source: &[u8], name: &[u8]) -> Result<()> {
        exec_state(&mut self.state, source, name)
    }
}

fn exec_state(state: &mut LuaState, source: &[u8], name: &[u8]) -> Result<()> {
    let status = load_buffer(state, source, name)?;
    if status != 0 {
        let err = state.pop();
        return Err(LuaError::from_value(err));
    }
    lua_vm::api::pcall_k(state, 0, 0, 0, 0, None)?;
    Ok(())
}

pub fn install_parser_hook(state: &mut LuaState) {
    state.global_mut().parser_hook = Some(parser_hook);
}

fn parser_hook(
    state: &mut LuaState,
    source: &[u8],
    name: &[u8],
    firstchar: i32,
) -> Result<GcRef<LuaLClosure>> {
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
            RawLuaValue::Nil,
        ))));
    }
    Ok(GcRef::new(LuaLClosure {
        proto: GcRef::new(*proto),
        upvals,
    }))
}

// ────────────────────────────── Sandboxing ──────────────────────────────
//
// Bounded, untrusted execution for embedders. Three independent controls:
//
//   1. Instruction budget   — abort after N executed VM instructions.
//   2. Memory ceiling        — abort once GC-tracked bytes exceed a limit.
//   3. Capability stripping   — remove dangerous globals (`os.execute`, `io`,
//                               `load`, `require`, …) from the environment.
//
// (1) and (2) are enforced by a runtime-wide budget stored in the shared
// `GlobalState` (`SandboxLimits`). `install_sandbox_limits` arms the VM
// count-hook mask on every thread — including coroutines, via `preinit_thread`
// — and the VM charges the shared budget once per `check_interval` instructions
// directly in `trace_exec`. When a limit is crossed the VM returns a `LuaError`
// that unwinds the dispatch loop and surfaces to the embedder as an ordinary
// runtime error from `exec`/`eval`/`call`. Because the budget is shared and
// every thread is armed, code inside `coroutine.wrap(...)` is metered too.
//
// Cost: when no sandbox is active the count mask is unset, `trap` stays false,
// and the dispatch loop is byte-for-byte unchanged — zero overhead. Inside a
// sandbox, the VM pays the standard count-hook cost (a per-instruction trap
// dispatch); `check_interval` trades enforcement precision, not throughput.
//
// Enforcement granularity is `check_interval` instructions: a budget trips
// within `check_interval` of the true limit, and memory is sampled at the same
// cadence — so a single allocation between two samples (e.g. `string.rep` with
// a huge count) can momentarily exceed the ceiling before the next check sees
// it. A hard, per-allocation memory cap would require enforcement inside
// `Heap::allocate`; that is the natural next step.

/// Why a sandboxed run was aborted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TripReason {
    /// The instruction budget reached zero.
    Instructions,
    /// GC-tracked memory exceeded the configured ceiling.
    Memory,
}

/// A live handle to a sandbox's budget. The budget itself lives in the
/// runtime's shared `GlobalState`, so it spans every thread (main and
/// coroutines); this handle just reads and resets it through the `Lua`.
#[derive(Clone)]
pub struct Sandbox {
    lua: Lua,
}

impl Sandbox {
    /// Instructions left before the budget trips, or `None` if no instruction
    /// limit was configured.
    pub fn instructions_remaining(&self) -> Option<u64> {
        self.lua.with_state(|state| {
            if state.sandbox_instr_limited() {
                Some(state.sandbox_instr_remaining())
            } else {
                None
            }
        })
    }

    /// Instructions consumed so far (rounded to the check interval), or `None`
    /// if no instruction limit was configured.
    pub fn instructions_used(&self) -> Option<u64> {
        self.lua.with_state(|state| {
            if state.sandbox_instr_limited() {
                Some(state.sandbox_instr_limit() - state.sandbox_instr_remaining())
            } else {
                None
            }
        })
    }

    /// Why the last run aborted, if it was the sandbox that stopped it.
    pub fn tripped(&self) -> Option<TripReason> {
        self.lua.with_state(|state| match state.sandbox_tripped_code() {
            lua_vm::state::SANDBOX_TRIP_INSTRUCTIONS => Some(TripReason::Instructions),
            lua_vm::state::SANDBOX_TRIP_MEMORY => Some(TripReason::Memory),
            _ => None,
        })
    }

    /// Refill the instruction budget to its configured limit and clear the
    /// tripped flag. Call before re-running a chunk in the same `Lua` state.
    pub fn reset(&self) {
        self.lua.with_state(|state| state.sandbox_reset());
    }
}

/// Configuration for [`Lua::sandboxed`].
#[derive(Debug, Clone)]
pub struct SandboxConfig {
    /// Maximum VM instructions a run may execute. `None` = unlimited.
    pub instruction_limit: Option<u64>,
    /// Maximum GC-tracked bytes. `None` = unlimited.
    pub memory_limit_bytes: Option<usize>,
    /// Instructions between budget/memory checks. Lower = tighter enforcement,
    /// higher hook overhead. Clamped to at least 1.
    pub check_interval: u32,
    /// Global paths to delete before running, e.g. `b"os.execute"` or `b"io"`.
    /// A `.`-separated path nils a field of a sub-table; a bare name nils a
    /// top-level global.
    pub remove_globals: Vec<Vec<u8>>,
}

impl SandboxConfig {
    /// A strict default: 10M instructions, 64 MiB, and removal of the
    /// code-loading and host-access globals. Tune fields as needed.
    pub fn strict() -> Self {
        let removed: &[&[u8]] = &[
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
        Self {
            instruction_limit: Some(10_000_000),
            memory_limit_bytes: Some(64 * 1024 * 1024),
            check_interval: 1000,
            remove_globals: removed.iter().map(|s| s.to_vec()).collect(),
        }
    }
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self::strict()
    }
}

fn strip_globals(state: &mut LuaState, names: &[Vec<u8>]) -> Result<()> {
    let globals = match state.global().globals.clone() {
        RawLuaValue::Table(t) => t,
        _ => return Ok(()),
    };
    for name in names {
        match name.iter().position(|&b| b == b'.') {
            Some(dot) => {
                let head = &name[..dot];
                let tail = &name[dot + 1..];
                if let RawLuaValue::Table(sub) = globals.get_str_bytes(head) {
                    let key = RawLuaValue::Str(state.new_string(tail)?);
                    sub.raw_set(key, RawLuaValue::Nil);
                }
            }
            None => {
                let key = RawLuaValue::Str(state.new_string(name)?);
                globals.raw_set(key, RawLuaValue::Nil);
            }
        }
    }
    Ok(())
}

impl Lua {
    /// Create a Lua runtime with no host capabilities (no file, process, or
    /// dynamic-library hooks), the configured globals stripped, and an
    /// instruction/memory budget installed. Returns the runtime and a
    /// [`Sandbox`] handle for inspecting and resetting the budget.
    pub fn sandboxed(config: SandboxConfig) -> Result<(Self, Sandbox)> {
        let lua = Self::with_hooks(HostHooks::default())?;
        let sandbox = lua.install_sandbox(config)?;
        Ok((lua, sandbox))
    }

    /// Apply sandbox limits to this runtime: strip the configured globals and,
    /// if any runtime limit is set, install the runtime-wide budget. The budget
    /// lives in the shared `GlobalState` and is enforced natively in the VM on
    /// every thread, so code inside coroutines is metered too. Use this when
    /// you want to grant *some* host capabilities (build the `Lua` with selected
    /// [`HostHooks`]) but still bound execution.
    pub fn install_sandbox(&self, config: SandboxConfig) -> Result<Sandbox> {
        let interval = config.check_interval.max(1) as i32;
        self.with_state(|state| -> Result<()> {
            strip_globals(state, &config.remove_globals)?;
            if config.instruction_limit.is_some() || config.memory_limit_bytes.is_some() {
                state.install_sandbox_limits(
                    interval,
                    config.instruction_limit,
                    config.memory_limit_bytes,
                );
            }
            Ok(())
        })?;
        Ok(Sandbox { lua: self.clone() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn external_root_count(lua: &Lua) -> usize {
        lua.with_state(|state| state.global().external_roots.len())
    }

    #[test]
    fn rooted_table_clone_and_drop_manage_root_slots() {
        let lua = Lua::new().expect("lua should initialize");
        assert_eq!(external_root_count(&lua), 0);

        let table = lua.create_table().expect("table should allocate");
        assert_eq!(external_root_count(&lua), 1);

        let cloned = table.clone();
        assert_eq!(external_root_count(&lua), 2);

        drop(table);
        assert_eq!(external_root_count(&lua), 1);

        cloned.set("answer", 42_i64).expect("set should succeed");
        lua.gc_collect();
        assert_eq!(
            cloned.get::<_, i64>("answer").expect("get should succeed"),
            42
        );

        drop(cloned);
        assert_eq!(external_root_count(&lua), 0);
    }

    #[test]
    fn table_values_survive_forced_collection_between_operations() {
        let lua = Lua::new().expect("lua should initialize");
        let table = lua.create_table().expect("table should allocate");

        lua.gc_collect();
        table.set("k", "v").expect("set should succeed");
        table.set(1_i64, "array").expect("array set should succeed");
        lua.gc_collect();

        let value: String = table.get("k").expect("get should succeed");
        assert_eq!(value, "v");
        assert_eq!(table.len().expect("len should succeed"), 1);
    }

    #[test]
    fn chunk_exec_eval_and_function_call_use_rooted_handles() {
        let lua = Lua::new().expect("lua should initialize");
        lua.load("function add(a, b) return a + b end")
            .set_name("test")
            .exec()
            .expect("chunk should execute");

        let globals = lua.globals();
        let add: Function = globals.get("add").expect("function should exist");
        let result: i64 = add.call((20_i64, 22_i64)).expect("call should work");
        assert_eq!(result, 42);

        let eval_result: i64 = lua
            .load("return add(1, 2)")
            .eval()
            .expect("eval should work");
        assert_eq!(eval_result, 3);
    }

    #[test]
    fn rust_callback_captures_state_and_reenters_lua() {
        let lua = Lua::new().expect("lua should initialize");
        lua.load("function twice(v) return v * 2 end")
            .exec()
            .expect("chunk should execute");

        let globals = lua.globals();
        let twice: Function = globals.get("twice").expect("function should exist");
        let calls = Rc::new(Cell::new(0));
        let calls_for_callback = calls.clone();

        let callback = lua
            .create_function(move |_lua, value: i64| {
                calls_for_callback.set(calls_for_callback.get() + 1);
                let doubled: i64 = twice.call(value)?;
                Ok(doubled + 1)
            })
            .expect("callback should create");
        globals
            .set("from_rust", callback)
            .expect("callback should register");

        let result: i64 = lua
            .load("return from_rust(20)")
            .eval()
            .expect("callback should run");
        assert_eq!(result, 41);
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn rust_callback_accepts_and_returns_collectable_values() {
        let lua = Lua::new().expect("lua should initialize");
        let globals = lua.globals();
        let callback = lua
            .create_function(|lua, name: String| {
                let table = lua.create_table()?;
                table.set("name", name)?;
                Ok(table)
            })
            .expect("callback should create");
        globals
            .set("make_record", callback)
            .expect("callback should register");

        let result: String = lua
            .load("return make_record('lua-rs').name")
            .eval()
            .expect("callback should return table");
        assert_eq!(result, "lua-rs");
    }

    #[test]
    fn rust_callback_mut_tracks_state() {
        let lua = Lua::new().expect("lua should initialize");
        let globals = lua.globals();
        let mut next = 0_i64;
        let callback = lua
            .create_function_mut(move |_lua, delta: i64| {
                next += delta;
                Ok(next)
            })
            .expect("callback should create");
        globals
            .set("next", callback)
            .expect("callback should register");

        let result: (i64, i64) = lua
            .load("return next(2), next(5)")
            .eval()
            .expect("callback should run");
        assert_eq!(result, (2, 7));
    }
}
