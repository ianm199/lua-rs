//! Embedding helper for lua-rs.
//!
//! This crate sits above `lua-vm`, `lua-stdlib`, and `lua-parse`, so it can
//! provide the common setup sequence without creating dependency cycles:
//! create a state, install the parser hook, install host hooks, open stdlib,
//! and run chunks.

use std::cell::RefCell;
use std::ffi::c_void;
use std::fmt;
use std::rc::Rc;

use lua_stdlib::auxlib::load_buffer;
use lua_stdlib::init::open_libs;
use lua_types::closure::LuaLClosure;
use lua_types::gc::GcRef;
use lua_types::string::LuaString as RawLuaString;
use lua_types::upval::UpVal;
use lua_types::value::{LuaTable as RawLuaTable, LuaValue as RawLuaValue};
use lua_vm::state::{
    new_state, DynLibLoadHook, DynLibSymbolHook, DynLibUnloadHook, EntropyHook, EnvHook,
    ExternalRootKey, FileLoaderHook, FileOpenHook, FileRemoveHook, FileRenameHook, InputHook,
    LuaState, OsExecuteHook, OutputHook, PopenHook, TempNameHook, UnixTimeHook,
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
            }),
        }
    }

    fn with_state<R>(&self, f: impl FnOnce(&mut LuaState) -> R) -> R {
        let mut state = self.inner.state.borrow_mut();
        f(&mut state)
    }

    fn root_raw(&self, value: RawLuaValue) -> RootedValue {
        let key = self.with_state(|state| state.external_root_value(value));
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

    pub fn eval<T: FromLua>(self) -> Result<T> {
        let raw = self.lua.with_state(|state| {
            let saved_top = state.top_idx();
            let status = load_buffer(state, &self.source, &self.name)?;
            if status != 0 {
                let err = state.pop();
                state.set_top_idx(saved_top);
                return Err(LuaError::from_value(err));
            }
            match lua_vm::api::pcall_k(state, 0, 1, 0, 0, None) {
                Ok(_) => {
                    let value = state.pop();
                    state.set_top_idx(saved_top);
                    Ok(value)
                }
                Err(err) => {
                    state.set_top_idx(saved_top);
                    Err(err)
                }
            }
        })?;
        let value = Value::from_raw(&self.lua, raw)?;
        T::from_lua(value, &self.lua)
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
        Ok(match raw {
            RawLuaValue::Nil => Value::Nil,
            RawLuaValue::Bool(v) => Value::Boolean(v),
            RawLuaValue::Int(v) => Value::Integer(v),
            RawLuaValue::Float(v) => Value::Number(v),
            RawLuaValue::Str(v) => Value::String(LuaString {
                root: lua.root_raw(RawLuaValue::Str(v)),
            }),
            RawLuaValue::Table(v) => Value::Table(Table {
                root: lua.root_raw(RawLuaValue::Table(v)),
            }),
            RawLuaValue::Function(v) => Value::Function(Function {
                root: lua.root_raw(RawLuaValue::Function(v)),
            }),
            RawLuaValue::UserData(v) => Value::UserData(AnyUserData {
                root: lua.root_raw(RawLuaValue::UserData(v)),
            }),
            RawLuaValue::LightUserData(v) => Value::LightUserData(v),
            RawLuaValue::Thread(v) => Value::Thread(Thread {
                root: lua.root_raw(RawLuaValue::Thread(v)),
            }),
        })
    }

    fn to_raw(&self) -> Result<RawLuaValue> {
        match self {
            Value::Nil => Ok(RawLuaValue::Nil),
            Value::Boolean(v) => Ok(RawLuaValue::Bool(*v)),
            Value::Integer(v) => Ok(RawLuaValue::Int(*v)),
            Value::Number(v) => Ok(RawLuaValue::Float(*v)),
            Value::String(v) => v.root.raw(),
            Value::Table(v) => v.root.raw(),
            Value::Function(v) => v.root.raw(),
            Value::UserData(v) => v.root.raw(),
            Value::LightUserData(v) => Ok(RawLuaValue::LightUserData(*v)),
            Value::Thread(v) => v.root.raw(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Table {
    root: RootedValue,
}

impl Table {
    fn raw_value(&self) -> Result<RawLuaValue> {
        self.root.raw()
    }

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
        let key_raw = key.to_raw()?;
        let table_raw = self.raw_value()?;
        let value_raw = lua.with_state(|state| state.table_get_with_tm(&table_raw, &key_raw))?;
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
        let key_raw = key.to_raw()?;
        let value_raw = value.to_raw()?;
        let table_raw = self.raw_value()?;
        lua.with_state(|state| state.table_set_with_tm(&table_raw, key_raw, value_raw))
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
        let arg_raws = args.iter().map(Value::to_raw).collect::<Result<Vec<_>>>()?;
        let function_raw = self.root.raw()?;
        let result_raws = lua.with_state(|state| {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
