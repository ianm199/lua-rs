//! Embedding helper for lua-rs.
//!
//! This crate sits above `lua-vm`, `lua-stdlib`, and `lua-parse`, so it can
//! provide the common setup sequence without creating dependency cycles:
//! create a state, install the parser hook, install host hooks, open stdlib,
//! and run chunks.

use std::any::Any;
use std::cell::{Cell, Ref, RefCell, RefMut};
use std::collections::HashMap;
use std::ffi::c_void;
use std::fmt;
use std::hash::Hash;
use std::ops::{Deref, DerefMut};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::rc::Rc;

use lua_stdlib::auxlib::load_buffer;
use lua_stdlib::init::open_libs;
use lua_types::closure::{LuaCClosure as RawLuaCClosure, LuaClosure as RawLuaClosure, LuaLClosure};
use lua_types::gc::GcRef;
use lua_types::string::LuaString as RawLuaString;
use lua_types::upval::UpVal;
use lua_types::userdata::LuaUserData as RawLuaUserData;
use lua_types::value::{LuaTable as RawLuaTable, LuaValue as RawLuaValue};
use lua_vm::state::{
    new_state, CpuClockHook, DynLibLoadHook, DynLibSymbolHook, DynLibUnloadHook, EntropyHook,
    EnvHook, ExternalRootKey, FileLoaderHook, FileOpenHook, FileRemoveHook, FileRenameHook,
    InputHook, LuaCallable, LuaRustFunction, LuaState, OsExecuteHook, OutputHook, PopenHook,
    TempNameHook, UnixTimeHook,
};

pub use lua_types::{LuaError, LuaFileHandle};
pub use lua_vm::state::{DynLibId, DynamicSymbol, OsExecuteReason, OsExecuteResult};

pub type Error = LuaError;
pub type Result<T> = std::result::Result<T, Error>;

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
    pending_external_unroots: RefCell<Vec<ExternalRootKey>>,
}

struct UserDataCell<T> {
    value: RefCell<T>,
}

struct RustCallbackCell {
    function: LuaRustFunction,
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

    fn flush_pending_external_unroots(&self, state: &mut LuaState) {
        let pending = self.pending_external_unroots.replace(Vec::new());
        if pending.is_empty() {
            return;
        }

        let mut still_pending = Vec::new();
        for key in pending {
            if state.try_external_unroot_value(key).is_err() {
                still_pending.push(key);
            }
        }

        if !still_pending.is_empty() {
            self.pending_external_unroots
                .borrow_mut()
                .extend(still_pending);
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
    pub fn new() -> Self {
        Self::try_new().expect("Lua runtime should initialize")
    }

    /// Fallible variant of [`Lua::new`].
    pub fn try_new() -> Result<Self> {
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
                pending_external_unroots: RefCell::new(Vec::new()),
            }),
        }
    }

    fn with_state<R>(&self, f: impl FnOnce(&mut LuaState) -> R) -> R {
        if let Ok(mut state) = self.inner.state.try_borrow_mut() {
            let _active = self.inner.enter_active(&mut *state);
            self.inner.flush_pending_external_unroots(&mut state);
            let result = f(&mut state);
            self.inner.flush_pending_external_unroots(&mut state);
            return result;
        }

        let state = self
            .active_state_mut()
            .expect("re-entrant Lua access without an active state");
        let result = f(state);
        self.inner.flush_pending_external_unroots(state);
        result
    }

    fn active_state_mut(&self) -> Option<&mut LuaState> {
        let state = self.inner.active_state.get();
        if state.is_null() {
            return None;
        }

        // SAFETY: `active_state` is set only while this `Lua` owns the outer
        // `RefCell` borrow and is executing VM code. Re-entrant access can only
        // happen when that VM frame has synchronously transferred control to a
        // Rust callback and is suspended. The callback path does not touch the
        // suspended `&mut LuaState` while user code re-enters through `Lua`.
        Some(unsafe { &mut *state })
    }

    fn unroot_external_key(&self, key: ExternalRootKey) {
        let removed = if let Ok(mut state) = self.inner.state.try_borrow_mut() {
            let _active = self.inner.enter_active(&mut *state);
            self.inner.flush_pending_external_unroots(&mut state);
            let removed = state.try_external_unroot_value(key).is_ok();
            self.inner.flush_pending_external_unroots(&mut state);
            removed
        } else {
            if let Some(state) = self.active_state_mut() {
                let removed = state.try_external_unroot_value(key).is_ok();
                self.inner.flush_pending_external_unroots(state);
                removed
            } else {
                false
            }
        };

        if !removed {
            self.inner.pending_external_unroots.borrow_mut().push(key);
        }
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

    fn userdata_cell<'a, T: 'static>(
        &self,
        userdata: &'a AnyUserData,
    ) -> Result<&'a UserDataCell<T>> {
        if !Rc::ptr_eq(&self.inner, &userdata.root.lua.inner) {
            return Err(LuaError::runtime(format_args!(
                "Lua userdata belongs to a different state"
            )));
        }
        userdata.host_cell()
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
            let _heap_guard = heap_guard(state);
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
            let _heap_guard = heap_guard(state);
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
            let trampoline = rust_callback_trampoline as lua_vm::state::LuaCFunction;
            let idx = {
                let mut global = state.global_mut();
                match global.c_functions.iter().position(|existing| {
                    existing
                        .as_bare()
                        .is_some_and(|existing| std::ptr::fn_addr_eq(existing, trampoline))
                }) {
                    Some(idx) => idx,
                    None => {
                        let idx = global.c_functions.len();
                        global.c_functions.push(LuaCallable::bare(trampoline));
                        idx
                    }
                }
            };
            let raw = with_heap_guard(state, || {
                let callback_payload = GcRef::new(RawLuaUserData {
                    data: Box::new([]),
                    uv: Vec::new(),
                    metatable: RefCell::new(None),
                    host_value: RefCell::new(Some(
                        Rc::new(RustCallbackCell { function: callable }) as Rc<dyn Any>,
                    )),
                });
                RawLuaValue::Function(RawLuaClosure::C(GcRef::new(RawLuaCClosure {
                    func: idx,
                    upvalues: vec![RawLuaValue::UserData(callback_payload)],
                })))
            });
            let key = state.external_root_value(raw);
            state.gc().check_step();
            RootedValue {
                lua: self.clone(),
                key,
            }
        });
        Ok(Function { root })
    }

    fn create_userdata_method<T, A, R, F>(&self, method: F) -> Result<Function>
    where
        T: UserData,
        A: FromLuaMulti + 'static,
        R: IntoLuaMulti + 'static,
        F: Fn(&Lua, &T, A) -> Result<R> + 'static,
    {
        let lua = self.clone();
        let callable: LuaRustFunction = Rc::new(move |state| {
            match catch_unwind(AssertUnwindSafe(|| {
                let (userdata, args) = callback_userdata_args(state, &lua)?;
                let args = A::from_lua_multi(args, &lua)?;
                let cell = lua.userdata_cell::<T>(&userdata)?;
                let value = cell.value.try_borrow().map_err(|_| {
                    LuaError::runtime(format_args!("userdata is already mutably borrowed"))
                })?;
                let returns = method(&lua, &value, args)?;
                let returns = returns.into_lua_multi(&lua)?;
                push_callback_returns(state, &lua, returns)
            })) {
                Ok(result) => result,
                Err(_) => Err(LuaError::runtime(format_args!(
                    "Rust userdata method panicked"
                ))),
            }
        });
        self.create_registered_function(callable)
    }

    fn create_userdata_method_mut<T, A, R, F>(&self, method: F) -> Result<Function>
    where
        T: UserData,
        A: FromLuaMulti + 'static,
        R: IntoLuaMulti + 'static,
        F: Fn(&Lua, &mut T, A) -> Result<R> + 'static,
    {
        let lua = self.clone();
        let callable: LuaRustFunction = Rc::new(move |state| {
            match catch_unwind(AssertUnwindSafe(|| {
                let (userdata, args) = callback_userdata_args(state, &lua)?;
                let args = A::from_lua_multi(args, &lua)?;
                let cell = lua.userdata_cell::<T>(&userdata)?;
                let mut value = cell
                    .value
                    .try_borrow_mut()
                    .map_err(|_| LuaError::runtime(format_args!("userdata is already borrowed")))?;
                let returns = method(&lua, &mut value, args)?;
                let returns = returns.into_lua_multi(&lua)?;
                push_callback_returns(state, &lua, returns)
            })) {
                Ok(result) => result,
                Err(_) => Err(LuaError::runtime(format_args!(
                    "Rust userdata method panicked"
                ))),
            }
        });
        self.create_registered_function(callable)
    }

    pub fn create_userdata<T>(&self, data: T) -> Result<AnyUserData>
    where
        T: UserData,
    {
        let mut methods = UserDataMethodRegistry::<T>::new(self);
        T::add_methods(&mut methods);
        T::add_meta_methods(&mut methods);
        methods.finish(data)
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
                    let nresults = if T::NRESULTS < 0 {
                        state.top_idx().0.saturating_sub(saved_top.0) as i32
                    } else {
                        T::NRESULTS
                    };
                    let mut values = Vec::with_capacity(nresults as usize);
                    for _ in 0..nresults {
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
        self.lua.unroot_external_key(self.key);
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
            RawLuaValue::UserData(v) => {
                let host_value = v.host_value();
                Value::UserData(AnyUserData {
                    root: lua.root_raw_in_state(state, RawLuaValue::UserData(v)),
                    host_value,
                })
            }
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
                    let nresults = if R::NRESULTS < 0 {
                        state.top_idx().0.saturating_sub(saved_top.0) as i32
                    } else {
                        R::NRESULTS
                    };
                    let mut results = Vec::with_capacity(nresults as usize);
                    for _ in 0..nresults {
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

#[derive(Clone)]
pub struct AnyUserData {
    root: RootedValue,
    host_value: Option<Rc<dyn Any>>,
}

impl fmt::Debug for AnyUserData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnyUserData")
            .field("root", &self.root)
            .field("has_host_value", &self.host_value.is_some())
            .finish()
    }
}

impl AnyUserData {
    fn host_cell<T: 'static>(&self) -> Result<&UserDataCell<T>> {
        let host = self
            .host_value
            .as_deref()
            .ok_or_else(|| LuaError::runtime(format_args!("missing Rust userdata payload")))?;
        host.downcast_ref::<UserDataCell<T>>()
            .ok_or_else(|| LuaError::runtime(format_args!("userdata type mismatch")))
    }

    pub fn borrow<T>(&self) -> Result<Ref<'_, T>>
    where
        T: 'static,
    {
        self.host_cell::<T>()?
            .value
            .try_borrow()
            .map_err(|_| LuaError::runtime(format_args!("userdata is already mutably borrowed")))
    }

    pub fn borrow_mut<T>(&self) -> Result<RefMut<'_, T>>
    where
        T: 'static,
    {
        self.host_cell::<T>()?
            .value
            .try_borrow_mut()
            .map_err(|_| LuaError::runtime(format_args!("userdata is already borrowed")))
    }

    pub fn with_borrow<T, R>(&self, f: impl FnOnce(&T) -> R) -> Result<R>
    where
        T: 'static,
    {
        let value = self.borrow::<T>()?;
        Ok(f(&value))
    }

    pub fn with_borrow_mut<T, R>(&self, f: impl FnOnce(&mut T) -> R) -> Result<R>
    where
        T: 'static,
    {
        let mut value = self.borrow_mut::<T>()?;
        Ok(f(&mut value))
    }
}

#[derive(Debug, Clone)]
pub struct Thread {
    root: RootedValue,
}

/// Variable argument or return list converted element-by-element.
///
/// This mirrors mlua's `Variadic<T>` enough for dynamic callback bridges:
/// `create_function(|_, args: Variadic<Value>| ...)` receives all Lua
/// arguments, and returning `Variadic<T>` pushes all contained values.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Variadic<T>(Vec<T>);

impl<T> Variadic<T> {
    pub const fn new() -> Self {
        Self(Vec::new())
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self(Vec::with_capacity(capacity))
    }

    pub fn into_vec(self) -> Vec<T> {
        self.0
    }
}

impl<T> Deref for Variadic<T> {
    type Target = Vec<T>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> DerefMut for Variadic<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T> From<Vec<T>> for Variadic<T> {
    fn from(value: Vec<T>) -> Self {
        Self(value)
    }
}

impl<T> From<Variadic<T>> for Vec<T> {
    fn from(value: Variadic<T>) -> Self {
        value.0
    }
}

impl<T> FromIterator<T> for Variadic<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        Self(Vec::from_iter(iter))
    }
}

impl<T> IntoIterator for Variadic<T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

pub trait UserData: 'static {
    fn add_methods<M: UserDataMethods<Self>>(_methods: &mut M)
    where
        Self: Sized,
    {
    }

    fn add_meta_methods<M: UserDataMethods<Self>>(_methods: &mut M)
    where
        Self: Sized,
    {
    }
}

pub trait UserDataMethods<T: UserData> {
    fn add_method<A, R, F>(&mut self, name: &str, method: F)
    where
        A: FromLuaMulti + 'static,
        R: IntoLuaMulti + 'static,
        F: Fn(&Lua, &T, A) -> Result<R> + 'static;

    fn add_method_mut<A, R, F>(&mut self, name: &str, method: F)
    where
        A: FromLuaMulti + 'static,
        R: IntoLuaMulti + 'static,
        F: Fn(&Lua, &mut T, A) -> Result<R> + 'static;

    fn add_meta_method<A, R, F>(&mut self, metamethod: MetaMethod, method: F)
    where
        A: FromLuaMulti + 'static,
        R: IntoLuaMulti + 'static,
        F: Fn(&Lua, &T, A) -> Result<R> + 'static;

    fn add_meta_method_mut<A, R, F>(&mut self, metamethod: MetaMethod, method: F)
    where
        A: FromLuaMulti + 'static,
        R: IntoLuaMulti + 'static,
        F: Fn(&Lua, &mut T, A) -> Result<R> + 'static;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MetaMethod {
    Index,
    NewIndex,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    Unm,
    Len,
    Eq,
    Lt,
    Le,
    Concat,
    Call,
    ToString,
    Pairs,
}

impl MetaMethod {
    fn name(self) -> &'static str {
        match self {
            MetaMethod::Index => "__index",
            MetaMethod::NewIndex => "__newindex",
            MetaMethod::Add => "__add",
            MetaMethod::Sub => "__sub",
            MetaMethod::Mul => "__mul",
            MetaMethod::Div => "__div",
            MetaMethod::Mod => "__mod",
            MetaMethod::Pow => "__pow",
            MetaMethod::Unm => "__unm",
            MetaMethod::Len => "__len",
            MetaMethod::Eq => "__eq",
            MetaMethod::Lt => "__lt",
            MetaMethod::Le => "__le",
            MetaMethod::Concat => "__concat",
            MetaMethod::Call => "__call",
            MetaMethod::ToString => "__tostring",
            MetaMethod::Pairs => "__pairs",
        }
    }
}

struct UserDataMethodRegistry<'lua, T: UserData> {
    lua: &'lua Lua,
    methods: Vec<(String, Function)>,
    meta_methods: Vec<(MetaMethod, Function)>,
    error: Option<LuaError>,
    _marker: std::marker::PhantomData<T>,
}

impl<'lua, T: UserData> UserDataMethodRegistry<'lua, T> {
    fn new(lua: &'lua Lua) -> Self {
        Self {
            lua,
            methods: Vec::new(),
            meta_methods: Vec::new(),
            error: None,
            _marker: std::marker::PhantomData,
        }
    }

    fn record(&mut self, result: Result<Function>, insert: impl FnOnce(&mut Self, Function)) {
        if self.error.is_some() {
            return;
        }
        match result {
            Ok(function) => insert(self, function),
            Err(err) => self.error = Some(err),
        }
    }

    fn finish(mut self, data: T) -> Result<AnyUserData> {
        if let Some(err) = self.error.take() {
            return Err(err);
        }

        let method_table = self.lua.create_table()?;
        for (name, function) in &self.methods {
            method_table.set(name.as_str(), function)?;
        }

        let metatable = self.lua.create_table()?;
        let mut has_index = false;
        for (metamethod, function) in &self.meta_methods {
            if *metamethod == MetaMethod::Index {
                has_index = true;
            }
            metatable.set(metamethod.name(), function)?;
        }
        if !has_index {
            metatable.set(MetaMethod::Index.name(), &method_table)?;
        }

        let cell: Rc<dyn Any> = Rc::new(UserDataCell {
            value: RefCell::new(data),
        });
        let host_value = cell.clone();
        let root = self.lua.with_state(|state| {
            let userdata = with_heap_guard(state, || {
                GcRef::new(RawLuaUserData {
                    data: Box::new([]),
                    uv: Vec::new(),
                    metatable: RefCell::new(None),
                    host_value: RefCell::new(None),
                })
            });
            let metatable_raw = metatable.root.raw_for_lua(self.lua, state)?;
            let RawLuaValue::Table(metatable) = metatable_raw else {
                return Err(type_error_raw(&metatable_raw, "table"));
            };
            userdata.set_metatable(Some(metatable));
            userdata.set_host_value(Some(cell));
            let key = state.external_root_value(RawLuaValue::UserData(userdata));
            Ok::<_, LuaError>(RootedValue {
                lua: self.lua.clone(),
                key,
            })
        })?;
        Ok(AnyUserData {
            root,
            host_value: Some(host_value),
        })
    }
}

impl<T: UserData> UserDataMethods<T> for UserDataMethodRegistry<'_, T> {
    fn add_method<A, R, F>(&mut self, name: &str, method: F)
    where
        A: FromLuaMulti + 'static,
        R: IntoLuaMulti + 'static,
        F: Fn(&Lua, &T, A) -> Result<R> + 'static,
    {
        let name = name.to_string();
        let result = self.lua.create_userdata_method(method);
        self.record(result, move |this, function| {
            this.methods.push((name, function));
        });
    }

    fn add_method_mut<A, R, F>(&mut self, name: &str, method: F)
    where
        A: FromLuaMulti + 'static,
        R: IntoLuaMulti + 'static,
        F: Fn(&Lua, &mut T, A) -> Result<R> + 'static,
    {
        let name = name.to_string();
        let result = self.lua.create_userdata_method_mut(method);
        self.record(result, move |this, function| {
            this.methods.push((name, function));
        });
    }

    fn add_meta_method<A, R, F>(&mut self, metamethod: MetaMethod, method: F)
    where
        A: FromLuaMulti + 'static,
        R: IntoLuaMulti + 'static,
        F: Fn(&Lua, &T, A) -> Result<R> + 'static,
    {
        let result = self.lua.create_userdata_method(method);
        self.record(result, move |this, function| {
            this.meta_methods.push((metamethod, function));
        });
    }

    fn add_meta_method_mut<A, R, F>(&mut self, metamethod: MetaMethod, method: F)
    where
        A: FromLuaMulti + 'static,
        R: IntoLuaMulti + 'static,
        F: Fn(&Lua, &mut T, A) -> Result<R> + 'static,
    {
        let result = self.lua.create_userdata_method_mut(method);
        self.record(result, move |this, function| {
            this.meta_methods.push((metamethod, function));
        });
    }
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

impl FromLua for usize {
    fn from_lua(value: Value, lua: &Lua) -> Result<Self> {
        let v = i64::from_lua(value, lua)?;
        usize::try_from(v).map_err(|_| LuaError::runtime(format_args!("integer out of range")))
    }
}

impl IntoLua for u64 {
    fn into_lua(self, lua: &Lua) -> Result<Value> {
        let v = i64::try_from(self)
            .map_err(|_| LuaError::runtime(format_args!("integer out of range")))?;
        v.into_lua(lua)
    }
}

impl FromLua for u64 {
    fn from_lua(value: Value, lua: &Lua) -> Result<Self> {
        let v = i64::from_lua(value, lua)?;
        u64::try_from(v).map_err(|_| LuaError::runtime(format_args!("integer out of range")))
    }
}

impl IntoLua for u32 {
    fn into_lua(self, lua: &Lua) -> Result<Value> {
        u64::from(self).into_lua(lua)
    }
}

impl FromLua for u32 {
    fn from_lua(value: Value, lua: &Lua) -> Result<Self> {
        let v = u64::from_lua(value, lua)?;
        u32::try_from(v).map_err(|_| LuaError::runtime(format_args!("integer out of range")))
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

impl IntoLua for AnyUserData {
    fn into_lua(self, _lua: &Lua) -> Result<Value> {
        Ok(Value::UserData(self))
    }
}

impl IntoLua for &AnyUserData {
    fn into_lua(self, _lua: &Lua) -> Result<Value> {
        Ok(Value::UserData(self.clone()))
    }
}

impl FromLua for AnyUserData {
    fn from_lua(value: Value, _lua: &Lua) -> Result<Self> {
        match value {
            Value::UserData(v) => Ok(v),
            other => Err(type_error_value(&other, "userdata")),
        }
    }
}

impl<T> IntoLua for T
where
    T: UserData,
{
    fn into_lua(self, lua: &Lua) -> Result<Value> {
        Ok(Value::UserData(lua.create_userdata(self)?))
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

impl<T> IntoLua for Vec<T>
where
    T: IntoLua,
{
    fn into_lua(self, lua: &Lua) -> Result<Value> {
        let table = lua.create_table()?;
        for (idx, value) in self.into_iter().enumerate() {
            table.set((idx + 1) as i64, value)?;
        }
        Ok(Value::Table(table))
    }
}

impl<T> FromLua for Vec<T>
where
    T: FromLua,
{
    fn from_lua(value: Value, lua: &Lua) -> Result<Self> {
        let table = Table::from_lua(value, lua)?;
        let raw = table.raw_table()?;
        let len = raw.getn();
        let mut out = Vec::with_capacity(len as usize);
        for idx in 1..=len {
            let value = Value::from_raw(lua, raw.get_int(idx as i64))?;
            out.push(T::from_lua(value, lua)?);
        }
        Ok(out)
    }
}

impl<K, V> IntoLua for HashMap<K, V>
where
    K: IntoLua,
    V: IntoLua,
{
    fn into_lua(self, lua: &Lua) -> Result<Value> {
        let table = lua.create_table()?;
        for (key, value) in self {
            table.set(key, value)?;
        }
        Ok(Value::Table(table))
    }
}

impl<K, V> FromLua for HashMap<K, V>
where
    K: FromLua + Eq + Hash,
    V: FromLua,
{
    fn from_lua(value: Value, lua: &Lua) -> Result<Self> {
        let table = Table::from_lua(value, lua)?;
        let raw = table.raw_table()?;
        let mut out = HashMap::new();
        let mut result = Ok(());
        raw.for_each_entry(|key, value| {
            if result.is_err() {
                return;
            }
            result = (|| {
                let key = Value::from_raw(lua, *key)?;
                let value = Value::from_raw(lua, *value)?;
                out.insert(K::from_lua(key, lua)?, V::from_lua(value, lua)?);
                Ok(())
            })();
        });
        result?;
        Ok(out)
    }
}

impl<T> IntoLuaMulti for Variadic<T>
where
    T: IntoLua,
{
    fn into_lua_multi(self, lua: &Lua) -> Result<Vec<Value>> {
        self.into_iter().map(|value| value.into_lua(lua)).collect()
    }
}

impl<T> FromLuaMulti for Variadic<T>
where
    T: FromLua,
{
    const NRESULTS: i32 = -1;

    fn from_lua_multi(values: Vec<Value>, lua: &Lua) -> Result<Self> {
        values
            .into_iter()
            .map(|value| T::from_lua(value, lua))
            .collect()
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

impl<A, T> IntoLuaMulti for (A, Variadic<T>)
where
    A: IntoLua,
    T: IntoLua,
{
    fn into_lua_multi(self, lua: &Lua) -> Result<Vec<Value>> {
        let mut values = vec![self.0.into_lua(lua)?];
        values.extend(self.1.into_lua_multi(lua)?);
        Ok(values)
    }
}

impl<A, B, C> IntoLuaMulti for (A, B, C)
where
    A: IntoLua,
    B: IntoLua,
    C: IntoLua,
{
    fn into_lua_multi(self, lua: &Lua) -> Result<Vec<Value>> {
        Ok(vec![
            self.0.into_lua(lua)?,
            self.1.into_lua(lua)?,
            self.2.into_lua(lua)?,
        ])
    }
}

impl<A, B, T> IntoLuaMulti for (A, B, Variadic<T>)
where
    A: IntoLua,
    B: IntoLua,
    T: IntoLua,
{
    fn into_lua_multi(self, lua: &Lua) -> Result<Vec<Value>> {
        let mut values = vec![self.0.into_lua(lua)?, self.1.into_lua(lua)?];
        values.extend(self.2.into_lua_multi(lua)?);
        Ok(values)
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

impl<A, T> FromLuaMulti for (A, Variadic<T>)
where
    A: FromLua,
    T: FromLua,
{
    const NRESULTS: i32 = -1;

    fn from_lua_multi(mut values: Vec<Value>, lua: &Lua) -> Result<Self> {
        let first = if values.is_empty() {
            Value::Nil
        } else {
            values.remove(0)
        };
        Ok((
            A::from_lua(first, lua)?,
            Variadic::from_lua_multi(values, lua)?,
        ))
    }
}

impl<A, B, C> FromLuaMulti for (A, B, C)
where
    A: FromLua,
    B: FromLua,
    C: FromLua,
{
    const NRESULTS: i32 = 3;

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
        let third = if values.is_empty() {
            Value::Nil
        } else {
            values.remove(0)
        };
        Ok((
            A::from_lua(first, lua)?,
            B::from_lua(second, lua)?,
            C::from_lua(third, lua)?,
        ))
    }
}

impl<A, B, T> FromLuaMulti for (A, B, Variadic<T>)
where
    A: FromLua,
    B: FromLua,
    T: FromLua,
{
    const NRESULTS: i32 = -1;

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
        Ok((
            A::from_lua(first, lua)?,
            B::from_lua(second, lua)?,
            Variadic::from_lua_multi(values, lua)?,
        ))
    }
}

fn rust_callback_trampoline(state: &mut LuaState) -> Result<usize> {
    let func_idx = state.current_call_info().func;
    let callback = match state.get_at(func_idx) {
        RawLuaValue::Function(RawLuaClosure::C(closure)) => {
            let Some(RawLuaValue::UserData(userdata)) = closure.upvalues.first() else {
                return Err(LuaError::runtime(format_args!(
                    "missing Rust callback payload"
                )));
            };
            let host = userdata
                .host_value()
                .ok_or_else(|| LuaError::runtime(format_args!("missing Rust callback payload")))?;
            host.downcast::<RustCallbackCell>().map_err(|_| {
                LuaError::runtime(format_args!("Rust callback payload type mismatch"))
            })?
        }
        _ => {
            return Err(LuaError::runtime(format_args!(
                "Rust callback trampoline called without C closure"
            )));
        }
    };
    (callback.function)(state)
}

fn with_heap_guard<R>(state: &LuaState, f: impl FnOnce() -> R) -> R {
    let _heap_guard = heap_guard(state);
    f()
}

fn heap_guard(state: &LuaState) -> lua_gc::HeapGuard {
    let global = state.global();
    lua_gc::HeapGuard::push(&global.heap)
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

fn callback_userdata_args(state: &mut LuaState, lua: &Lua) -> Result<(AnyUserData, Vec<Value>)> {
    let mut args = callback_args(state, lua)?;
    if args.is_empty() {
        return Err(LuaError::runtime(format_args!(
            "userdata method missing self argument"
        )));
    }
    let userdata = AnyUserData::from_lua(args.remove(0), lua)?;
    Ok((userdata, args))
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
    let _heap_guard = heap_guard(state);
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
    use std::cell::Cell;

    fn external_root_count(lua: &Lua) -> usize {
        lua.with_state(|state| state.global().external_roots.len())
    }

    struct Counter {
        value: i64,
    }

    impl UserData for Counter {
        fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
            methods.add_method("get", |_lua, this, ()| Ok(this.value));
            methods.add_method_mut("inc", |_lua, this, delta: i64| {
                this.value += delta;
                Ok(this.value)
            });
        }
    }

    struct PropertyBag {
        value: i64,
    }

    impl UserData for PropertyBag {
        fn add_meta_methods<M: UserDataMethods<Self>>(methods: &mut M) {
            methods.add_meta_method(MetaMethod::Index, |_lua, this, key: String| {
                if key == "value" {
                    Ok(Value::Integer(this.value))
                } else {
                    Ok(Value::Nil)
                }
            });
            methods.add_meta_method_mut(
                MetaMethod::NewIndex,
                |_lua, this, (key, value): (String, i64)| {
                    if key != "value" {
                        return Err(LuaError::runtime(format_args!("unknown property")));
                    }
                    this.value = value;
                    Ok(())
                },
            );
        }
    }

    #[test]
    fn rooted_table_clone_and_drop_manage_root_slots() {
        let lua = Lua::new();
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
        let lua = Lua::new();
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
        let lua = Lua::new();
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
        let lua = Lua::new();
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
        let lua = Lua::new();
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
        let lua = Lua::new();
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

    #[test]
    fn dropped_rust_callback_releases_captured_handles_after_gc() {
        let lua = Lua::new();
        let table = lua.create_table().expect("table should allocate");
        table.set("value", 42_i64).expect("set should succeed");
        assert_eq!(external_root_count(&lua), 1);

        let callback = {
            let captured = table.clone();
            lua.create_function(move |_lua, ()| captured.get::<_, i64>("value"))
                .expect("callback should create")
        };
        assert_eq!(external_root_count(&lua), 3);

        drop(callback);
        lua.gc_collect();
        assert_eq!(external_root_count(&lua), 1);
        assert_eq!(table.get::<_, i64>("value").expect("table should live"), 42);
    }

    #[test]
    fn userdata_methods_dispatch_and_track_borrows() {
        let lua = Lua::new();
        let globals = lua.globals();
        let counter = lua
            .create_userdata(Counter { value: 1 })
            .expect("userdata should create");
        globals
            .set("counter", &counter)
            .expect("userdata should register");

        let result: i64 = lua
            .load("counter:inc(5); return counter:get()")
            .eval()
            .expect("methods should dispatch");
        assert_eq!(result, 6);
        assert_eq!(
            counter
                .with_borrow::<Counter, _>(|counter| counter.value)
                .expect("borrow should work"),
            6
        );

        {
            let borrowed = counter
                .borrow::<Counter>()
                .expect("borrow guard should work");
            assert_eq!(borrowed.value, 6);
        }

        {
            let mut borrowed = counter
                .borrow_mut::<Counter>()
                .expect("mutable borrow guard should work");
            borrowed.value = 9;
        }

        assert_eq!(
            lua.load("return counter:get()")
                .eval::<i64>()
                .expect("method should see guard mutation"),
            9
        );
    }

    #[test]
    fn userdata_payload_survives_gc_while_lua_holds_userdata() {
        let lua = Lua::new();
        let globals = lua.globals();
        let counter = lua
            .create_userdata(Counter { value: 10 })
            .expect("userdata should create");
        globals
            .set("counter", counter)
            .expect("userdata should register");

        lua.gc_collect();
        let result: i64 = lua
            .load("counter:inc(2); collectgarbage('collect'); return counter:get()")
            .eval()
            .expect("userdata should survive collection");
        assert_eq!(result, 12);
    }

    #[test]
    fn userdata_runtime_borrow_conflict_returns_lua_error() {
        let lua = Lua::new();
        let globals = lua.globals();
        let counter = lua
            .create_userdata(Counter { value: 1 })
            .expect("userdata should create");
        globals
            .set("counter", &counter)
            .expect("userdata should register");

        let failed = counter
            .with_borrow::<Counter, _>(|_| lua.load("return counter:inc(1)").eval::<i64>().is_err())
            .expect("outer borrow should succeed");
        assert!(
            failed,
            "mutable method should fail while immutable borrow is held"
        );
        assert_eq!(
            counter
                .with_borrow::<Counter, _>(|counter| counter.value)
                .expect("borrow should work"),
            1
        );
    }

    #[test]
    fn userdata_index_and_newindex_metamethods_dispatch() {
        let lua = Lua::new();
        let globals = lua.globals();
        let bag = lua
            .create_userdata(PropertyBag { value: 7 })
            .expect("userdata should create");
        globals.set("bag", &bag).expect("userdata should register");

        let result: i64 = lua
            .load("bag.value = 42; return bag.value")
            .eval()
            .expect("metamethods should dispatch");
        assert_eq!(result, 42);
        assert_eq!(
            bag.with_borrow::<PropertyBag, _>(|bag| bag.value)
                .expect("borrow should work"),
            42
        );
    }

    #[test]
    fn userdata_values_convert_directly_with_into_lua() {
        let lua = Lua::new();
        let globals = lua.globals();
        globals
            .set("counter", Counter { value: 3 })
            .expect("userdata should convert through IntoLua");

        let result: i64 = lua
            .load("counter:inc(4); return counter:get()")
            .eval()
            .expect("converted userdata should dispatch methods");
        assert_eq!(result, 7);
    }

    #[test]
    fn variadic_args_and_returns_convert_all_values() {
        let lua = Lua::new();
        let globals = lua.globals();

        let sum = lua
            .create_function(|_lua, values: Variadic<i64>| Ok(values.iter().sum::<i64>()))
            .expect("variadic callback should create");
        globals.set("sum", sum).expect("callback should register");
        let result: i64 = lua
            .load("return sum(3, 2, 5)")
            .eval()
            .expect("variadic callback should run");
        assert_eq!(result, 10);

        let echo = lua
            .create_function(|_lua, values: Variadic<Value>| Ok(values))
            .expect("variadic return callback should create");
        globals.set("echo", echo).expect("callback should register");
        let result: (i64, i64, i64) = lua
            .load("return echo(1, 2, 3)")
            .eval()
            .expect("variadic returns should stay separate");
        assert_eq!(result, (1, 2, 3));

        let values: Variadic<i64> = lua
            .load("return 4, 5, 6")
            .eval()
            .expect("variadic eval should collect all returns");
        assert_eq!(values.into_vec(), vec![4, 5, 6]);
    }

    #[test]
    fn vectors_maps_and_triple_returns_convert_through_tables() {
        let lua = Lua::new();
        let globals = lua.globals();

        globals
            .set("list", vec![1_i64, 2, 3])
            .expect("vector should convert to table");
        let second: i64 = lua
            .load("return list[2]")
            .eval()
            .expect("table should be readable from Lua");
        assert_eq!(second, 2);

        let list: Vec<i64> = lua
            .load("return {4, 5, 6}")
            .eval()
            .expect("table should convert to vector");
        assert_eq!(list, vec![4, 5, 6]);

        let mut map = HashMap::new();
        map.insert("left".to_string(), 10_i64);
        map.insert("right".to_string(), 20_i64);
        globals
            .set("map", map)
            .expect("map should convert to table");
        let sum: i64 = lua
            .load("return map.left + map.right")
            .eval()
            .expect("map table should be readable from Lua");
        assert_eq!(sum, 30);

        let map: HashMap<String, i64> = lua
            .load("return {alpha = 3, beta = 9}")
            .eval()
            .expect("table should convert to map");
        assert_eq!(map.get("alpha"), Some(&3));
        assert_eq!(map.get("beta"), Some(&9));

        let triple: (i64, i64, i64) = lua
            .load("return 1, 2, 3")
            .eval()
            .expect("triple returns should convert");
        assert_eq!(triple, (1, 2, 3));
    }
}
