//! Embedding helper for lua-rs.
//!
//! This crate sits above `lua-vm`, `lua-stdlib`, and `lua-parse` and exposes a
//! handle-based embedding API: a [`Lua`] state, typed [`Value`] / [`Table`] /
//! [`Function`] handles that root themselves via RAII, [`UserData`] for binding
//! Rust types, and a typed [`LuaError`]. It also provides the common setup
//! sequence (state, parser hook, host hooks, stdlib).
//!
//! # Userdata model
//!
//! Userdata behavior in lua-rs runs through real Lua metatables, exactly as in
//! reference Lua 5.4. The runtime builds the metatable for a type once, on the
//! first [`Lua::create_userdata`] for that `TypeId`, permanently roots it on
//! the state, and shares it across every later value of the type. This keeps
//! `getmetatable`, `setmetatable`, `rawget`, `debug.setmetatable`, and every
//! other reflective Lua operation behaving as in C Lua, which is what lets
//! lua-rs pass the upstream 5.4 test suite and stand in for C Lua in real
//! embedders.
//!
//! Fields and methods both live on that single metatable. Register fields with
//! [`UserDataMethods::add_field_method_get`] / `add_field_method_set` and
//! methods with [`UserDataMethods::add_method`] / `add_method_mut`. The runtime
//! composes a single `__index` whose lookup order is field, then method, then
//! a raw `add_meta_method(MetaMethod::Index, ...)` if you registered one as an
//! escape hatch, with the symmetric composition on `__newindex`.
//!
//! # Derive
//!
//! Enable the `derive` feature for `#[derive(LuaUserData)]`, `#[lua_methods]`,
//! and `#[lua_impl(Display, PartialEq, PartialOrd)]`. The derive targets the
//! field API above; `#[lua_methods]` exposes each `pub fn(&self / &mut self,
//! ...)` as `obj:method(args)`; `#[lua_impl(...)]` wires `__tostring`, `__eq`,
//! `__lt`, and `__le` from the type's Rust trait impls.
//!
//! ```ignore
//! use lua_rs_runtime::{lua_methods, Lua, LuaUserData};
//!
//! #[derive(LuaUserData, PartialEq, PartialOrd)]
//! #[lua(methods)]
//! #[lua_impl(Display, PartialEq, PartialOrd)]
//! struct Vec2 { pub x: f64, pub y: f64 }
//!
//! #[lua_methods]
//! impl Vec2 {
//!     pub fn length(&self) -> f64 { (self.x * self.x + self.y * self.y).sqrt() }
//!     pub fn scale(&mut self, k: f64) { self.x *= k; self.y *= k; }
//! }
//! ```
//!
//! # Scope: lending non-`'static` borrows to Lua
//!
//! [`Lua::create_userdata`] takes its value by ownership, so the type must be
//! `'static`. When you instead want to lend Lua a value that lives on the Rust
//! stack for the duration of one call (typically a game engine's
//! `&mut World`), use [`Lua::scope`]. A scope hands Lua a borrow that is
//! invalidated the moment the scope closure returns: any Lua reference that
//! escaped the scope fails with a clean runtime error on next use instead of
//! touching freed memory.
//!
//! ```
//! use lua_rs_runtime::{Lua, UserData, UserDataMethods};
//!
//! struct Counter { value: i64 }
//! impl UserData for Counter {
//!     fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
//!         m.add_method_mut("inc", |_, this, by: i64| { this.value += by; Ok(this.value) });
//!     }
//! }
//!
//! let lua = Lua::new();
//! let mut counter = Counter { value: 0 };
//! lua.scope(|s| {
//!     let ud = s.create_userdata_ref_mut(&lua, &mut counter)?;
//!     lua.globals().set("c", &ud)?;
//!     lua.load("c:inc(5); c:inc(7)").exec()
//! }).unwrap();
//! assert_eq!(counter.value, 12);
//! ```
//!
//! [`Scope::create_function`] / [`Scope::create_function_mut`] do the same for
//! closures that capture non-`'static` borrows. And
//! [`AnyUserData::delegate`] builds a *sub-userdata* that re-borrows a field of
//! its parent on every call (`world:entity(id)` returning a live `&mut Entity`),
//! so an `App -> World -> Component` chain stays a chain of short borrows rather
//! than one long-held `&mut`. See [`Lua::scope`] for the full contract.
//!
//! With the `derive` feature, a `#[lua_methods]` method that returns a
//! reference is registered as a delegate automatically: `fn entity(&mut self,
//! id: u32) -> &mut Entity` becomes `world:entity(id)` with no hand-written
//! accessor. `&mut T` returns give a mutable delegate, `&T` a read-only one.
//!
//! # Known limitations and planned work
//!
//! - `#[lua_methods]` does not yet special-case methods that return
//!   `Result<T, E>`, associated functions and constructors (`Type::new`), or
//!   `Option<T>` parameters and returns.
//! - The derive does not yet handle enums (a `register_enum::<T>()` path) or
//!   the iteration, `__close`, and arithmetic metamethods. The runtime already
//!   supports adding these as ordinary `add_meta_method` registrations today.

use std::any::{Any, TypeId};
use std::cell::{Cell, Ref, RefCell, RefMut};
use std::collections::HashMap;
use std::ffi::c_void;
use std::fmt;
use std::hash::Hash;
use std::ops::{Deref, DerefMut};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr::NonNull;
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

#[cfg(feature = "derive")]
pub use lua_rs_derive::{lua_methods, LuaUserData};

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
    /// One metatable per `UserData` type, built on first `create_userdata::<T>`
    /// and reused for every later value of that type. Each entry is permanently
    /// rooted in the state's external-root set, so it survives even when no
    /// instance currently exists, and frees with the state.
    userdata_metatables: RefCell<HashMap<TypeId, GcRef<RawLuaTable>>>,
    /// Same shape as `userdata_metatables` but for the `Scope::create_userdata`
    /// path: the method closures here downcast `host_value` to
    /// `Rc<ScopedCell<T>>` and check the cell's validity flag before
    /// dereferencing the pointer it holds.
    userdata_scoped_metatables: RefCell<HashMap<TypeId, GcRef<RawLuaTable>>>,
}

struct UserDataCell<T> {
    value: RefCell<T>,
}

// ---------------------------------------------------------------------------
// Scope: pass non-`'static` borrows into Lua safely.
//
// `Scope::create_userdata::<T>(&mut data)` stores a raw pointer to `data` in a
// `ScopedCell<T>` and registers the cell with the scope. While the scope is
// alive the cell's pointer is dereferenced (validity-checked) on every method
// call from Lua. When the scope drops, every registered cell's pointer is set
// to `None`, so any leaked userdata calls return a clean Lua error instead of
// using-after-the-borrow-ended.
//
// Safety model:
// - The raw pointer's borrow originates from `&mut data`, whose lifetime is
//   tied to the scope's lifetime via `&'scope mut T`. The borrow checker holds
//   the borrow alive for the full scope body.
// - Re-entrant access (a Lua callback that fires another callback on the same
//   userdata) is rejected at runtime via `ScopedCell::borrow`'s shared/exclusive
//   counter, mirroring `RefCell`.
// - On scope drop, callbacks have already returned (they run synchronously
//   inside the scope body), so `invalidate` only nulls the pointer; no
//   concurrent dereference can be in progress.

/// Holder for a borrowed Rust value passed into Lua via [`Scope::create_userdata`].
///
/// Generic over `T: 'static` so it satisfies the existing `UserData: 'static`
/// requirement and `Any`-based downcast lookup; the actual borrow lifetime is
/// erased into a raw pointer and re-checked on every access.
struct ScopedCell<T: 'static> {
    ptr: Cell<Option<NonNull<T>>>,
    /// Same encoding as `RefCell`: positive = shared borrows, negative = one
    /// exclusive borrow, zero = unborrowed.
    borrow: Cell<isize>,
}

impl<T: 'static> ScopedCell<T> {
    fn new(data: &mut T) -> Self {
        Self {
            ptr: Cell::new(Some(NonNull::from(data))),
            borrow: Cell::new(0),
        }
    }

    fn try_borrow(&self) -> Result<ScopedRef<'_, T>> {
        let b = self.borrow.get();
        if b < 0 {
            return Err(LuaError::runtime(format_args!(
                "scoped userdata is already mutably borrowed"
            )));
        }
        let ptr = self.ptr.get().ok_or_else(|| {
            LuaError::runtime(format_args!(
                "scoped userdata is no longer valid (its scope has ended)"
            ))
        })?;
        self.borrow.set(b + 1);
        Ok(ScopedRef { cell: self, ptr })
    }

    fn try_borrow_mut(&self) -> Result<ScopedRefMut<'_, T>> {
        let b = self.borrow.get();
        if b != 0 {
            return Err(LuaError::runtime(format_args!(
                "scoped userdata is already borrowed"
            )));
        }
        let ptr = self.ptr.get().ok_or_else(|| {
            LuaError::runtime(format_args!(
                "scoped userdata is no longer valid (its scope has ended)"
            ))
        })?;
        self.borrow.set(-1);
        Ok(ScopedRefMut { cell: self, ptr })
    }
}

/// Trait-object handle a `Scope` uses to invalidate any cell type on drop
/// without knowing its `T`.
trait ScopeInvalidate {
    fn invalidate(&self);
}

impl<T: 'static> ScopeInvalidate for ScopedCell<T> {
    fn invalidate(&self) {
        // Safe only because callbacks have all returned by the time `Scope`
        // drops: they run synchronously inside the closure body. If a callback
        // is somehow mid-execution, its `ScopedRef`/`ScopedRefMut` guard still
        // has the raw pointer copied locally and dereferences it; the next
        // `try_borrow*` after invalidate sees `ptr = None` and errors cleanly.
        self.ptr.set(None);
    }
}

struct ScopedRef<'a, T: 'static> {
    cell: &'a ScopedCell<T>,
    ptr: NonNull<T>,
}

impl<'a, T: 'static> Drop for ScopedRef<'a, T> {
    fn drop(&mut self) {
        self.cell.borrow.set(self.cell.borrow.get() - 1);
    }
}

impl<'a, T: 'static> Deref for ScopedRef<'a, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: pointer was obtained from a live `&mut T` (or `&mut T`-derived)
        // value whose lifetime spans the scope. Re-entrant borrow conflicts are
        // rejected by `borrow` above. The pointer is set to `None` only when
        // `invalidate` runs, which can only happen after `Scope` drops; by then
        // no `ScopedRef` can exist because callbacks have returned.
        unsafe { self.ptr.as_ref() }
    }
}

struct ScopedRefMut<'a, T: 'static> {
    cell: &'a ScopedCell<T>,
    ptr: NonNull<T>,
}

impl<'a, T: 'static> Drop for ScopedRefMut<'a, T> {
    fn drop(&mut self) {
        self.cell.borrow.set(0);
    }
}

impl<'a, T: 'static> Deref for ScopedRefMut<'a, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: same as `ScopedRef::deref`.
        unsafe { self.ptr.as_ref() }
    }
}

impl<'a, T: 'static> DerefMut for ScopedRefMut<'a, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: same as `ScopedRef::deref`, plus the cell's `borrow == -1`
        // ensures no other shared or exclusive borrow is currently outstanding.
        unsafe { self.ptr.as_mut() }
    }
}

/// Handle passed to the closure body of [`Lua::scope`].
///
/// `Scope::create_userdata` produces an [`AnyUserData`] whose backing storage
/// is a borrow you provide; when the scope drops every cell it created is
/// invalidated. Any later Lua call that reaches one of those userdatas fails
/// with a clean error rather than touching freed memory.
pub struct Scope<'scope> {
    invalidators: RefCell<Vec<Rc<dyn ScopeInvalidate>>>,
    _phantom: std::marker::PhantomData<&'scope mut ()>,
}

impl<'scope> Scope<'scope> {
    fn new() -> Self {
        Self {
            invalidators: RefCell::new(Vec::new()),
            _phantom: std::marker::PhantomData,
        }
    }

    /// Wrap a `&mut T` borrow as a Lua userdata that lives for the duration of
    /// this scope. Any call from Lua to the returned userdata after the scope
    /// ends fails with a clean Lua runtime error instead of touching the
    /// freed borrow.
    ///
    /// Naming mirrors mlua's `Scope::create_userdata_ref_mut`. The bare
    /// `create_userdata` name on `Scope` is intentionally reserved for the
    /// future by-value, non-`'static` constructor (mlua's
    /// `Scope::create_userdata<T: UserData + 'env>(T)`), tracked as a
    /// follow-up to lua-rs#27.
    pub fn create_userdata_ref_mut<T>(&self, lua: &Lua, data: &'scope mut T) -> Result<AnyUserData>
    where
        T: UserData,
    {
        let cell = Rc::new(ScopedCell::<T>::new(data));
        self.invalidators
            .borrow_mut()
            .push(cell.clone() as Rc<dyn ScopeInvalidate>);
        lua.create_scoped_userdata::<T>(cell)
    }

    /// Build a Lua [`Function`] from a non-`'static` Rust closure. The closure
    /// is owned by a [`ScopedFnCell`] that the scope holds; once the scope
    /// drops, the cell drops the closure and any later Lua call that reaches
    /// the returned function fails cleanly with "no longer valid" instead of
    /// touching the released captures.
    ///
    /// This is the function counterpart to [`Self::create_userdata`] — pair
    /// them when you want to hand Lua a `&mut World` plus a few closures that
    /// also borrow from the same stack frame.
    pub fn create_function<A, R, F>(&self, lua: &Lua, func: F) -> Result<Function>
    where
        A: FromLuaMulti + 'static,
        R: IntoLuaMulti + 'static,
        F: Fn(&Lua, A) -> Result<R> + 'scope,
    {
        let adapter: Box<dyn Fn(&Lua, Vec<Value>) -> Result<Vec<Value>> + 'scope> =
            Box::new(move |lua, args| {
                let args = A::from_lua_multi(args, lua)?;
                let returns = func(lua, args)?;
                returns.into_lua_multi(lua)
            });
        self.install_function(lua, adapter)
    }

    /// Like [`Self::create_function`] but accepts an `FnMut`. Mirrors
    /// [`Lua::create_function_mut`]: re-entrant calls into the same closure
    /// are rejected with an "already borrowed" runtime error rather than
    /// producing aliasing `&mut` captures.
    pub fn create_function_mut<A, R, F>(&self, lua: &Lua, func: F) -> Result<Function>
    where
        A: FromLuaMulti + 'static,
        R: IntoLuaMulti + 'static,
        F: FnMut(&Lua, A) -> Result<R> + 'scope,
    {
        let func = RefCell::new(func);
        self.create_function(lua, move |lua, args| {
            let mut func = func.try_borrow_mut().map_err(|_| {
                LuaError::runtime(format_args!("mutable Rust callback is already borrowed"))
            })?;
            func(lua, args)
        })
    }

    /// Internal: launder the closure's `'scope` lifetime bound to `'static`
    /// so the resulting cell can be held by a `'static` Lua callback, park
    /// the box inside a [`ScopedFnCell`], and register that cell with the
    /// scope so its closure is dropped on scope end.
    fn install_function(
        &self,
        lua: &Lua,
        adapter: Box<dyn Fn(&Lua, Vec<Value>) -> Result<Vec<Value>> + 'scope>,
    ) -> Result<Function> {
        // SAFETY: extending the trait-object lifetime bound from `'scope` to
        // `'static` is sound here because the closure is owned by the
        // [`ScopedFnCell`] we are about to build, that cell is registered in
        // `self.invalidators`, and `Scope::drop` invokes `invalidate()` on
        // every registered cell. `invalidate()` calls `take()` on the box,
        // which drops the closure (and therefore its `'scope` captures)
        // while `'scope` is still alive (we are mid-drop of `Scope`). After
        // `invalidate()` the cell holds `None`, so any subsequent call sees
        // "no longer valid" before it can reach a dangling capture.
        let adapter_static: Box<dyn Fn(&Lua, Vec<Value>) -> Result<Vec<Value>>> =
            unsafe { std::mem::transmute(adapter) };
        let cell = Rc::new(ScopedFnCell {
            boxed: RefCell::new(Some(adapter_static)),
        });
        self.invalidators
            .borrow_mut()
            .push(cell.clone() as Rc<dyn ScopeInvalidate>);
        lua.create_scoped_function(cell)
    }
}

impl<'scope> Drop for Scope<'scope> {
    fn drop(&mut self) {
        for inv in self.invalidators.borrow().iter() {
            inv.invalidate();
        }
    }
}

/// Owns a scoped Rust closure on behalf of [`Scope`]. The closure is stored
/// as `Box<dyn Fn(...)>` (lifetime laundered to `'static`); on scope drop the
/// option is taken and the closure (with its `'scope` captures) is dropped.
/// All later calls see `None` and return a clean Lua runtime error.
struct ScopedFnCell {
    boxed: RefCell<Option<Box<dyn Fn(&Lua, Vec<Value>) -> Result<Vec<Value>>>>>,
}

impl ScopedFnCell {
    /// Dispatch the wrapped closure, or surface a clean error if the scope
    /// already ended.
    fn try_call(&self, lua: &Lua, args: Vec<Value>) -> Result<Vec<Value>> {
        let guard = self.boxed.borrow();
        let func = guard.as_deref().ok_or_else(|| {
            LuaError::runtime(format_args!(
                "scoped function is no longer valid (its scope has ended)"
            ))
        })?;
        func(lua, args)
    }
}

impl ScopeInvalidate for ScopedFnCell {
    fn invalidate(&self) {
        *self.boxed.borrow_mut() = None;
    }
}

// ---------------------------------------------------------------------------
// Delegated cell: a sub-userdata that re-acquires a fresh `&mut S` from a
// parent cell on every method call. Lives at the same scope as the parent.
//
// The cell stores no live borrow itself. Instead it holds a closure that
// knows how to enter the parent (`try_borrow_mut`), apply the user's
// accessor (`|p: &mut P| -> &mut S`), invoke a caller-supplied callback
// with the derived `&mut S`, then release the parent's borrow. Methods on
// the sub-userdata's metatable invoke `enter_mut` to do their work; if a
// nested Lua call tries to re-enter the parent during a delegate call, the
// inner `try_borrow_mut` surfaces the same "already borrowed" error path
// `ScopedCell` already uses.
//
// Invalidation: the cell holds an `Rc<dyn ScopeInvalidate>` for the parent
// so the scope drop chain still works. The cell's own `invalidate` also
// nulls the `enter_mut` closure to short-circuit any caller that managed
// to retain a `Rc<DelegatedCell<S>>` past scope end (the closure captures
// the parent cell's `Rc`, which we want to release).
//
// Generic over `S` only — the parent type `P` is type-erased inside the
// closure so that one `Rc<DelegatedCell<S>>` covers any chain of accessors
// regardless of where it bottomed out (`App -> World`, `World -> Inner`,
// etc.). Composition (`delegate` on a delegated userdata) builds a fresh
// closure that wraps the parent's `enter_mut` plus the new accessor.
/// How a delegated cell reaches its referent. `Mut` borrows the parent
/// exclusively and yields `&mut S` (from `delegate`); `Ref` borrows the
/// parent shared and yields `&S` (from `delegate_ref`). A `Ref` delegate is
/// read-only: a mutating child method on it fails cleanly.
enum DelegateEnter<S: 'static> {
    Mut(Box<dyn Fn(&mut dyn FnMut(&mut S)) -> Result<()>>),
    Ref(Box<dyn Fn(&mut dyn FnMut(&S)) -> Result<()>>),
}

struct DelegatedCell<S: 'static> {
    enter: RefCell<Option<DelegateEnter<S>>>,
}

impl<S: 'static> DelegatedCell<S> {
    fn invalid() -> LuaError {
        LuaError::runtime(format_args!(
            "scoped userdata is no longer valid (its scope has ended)"
        ))
    }

    /// Shared access. Works for both delegate kinds: a `Mut` cell yields
    /// `&mut S` which is downgraded to `&S`; a `Ref` cell yields `&S`.
    fn enter_ref(&self, f: &mut dyn FnMut(&S)) -> Result<()> {
        let guard = self.enter.borrow();
        match guard.as_ref().ok_or_else(Self::invalid)? {
            DelegateEnter::Mut(g) => g(&mut |t| f(&*t)),
            DelegateEnter::Ref(g) => g(f),
        }
    }

    /// Exclusive access. Only a `Mut` delegate can grant it; a `Ref` delegate
    /// is read-only and rejects mutating methods.
    fn enter_mut(&self, f: &mut dyn FnMut(&mut S)) -> Result<()> {
        let guard = self.enter.borrow();
        match guard.as_ref().ok_or_else(Self::invalid)? {
            DelegateEnter::Mut(g) => g(f),
            DelegateEnter::Ref(_) => Err(LuaError::runtime(format_args!(
                "cannot call a mutating method on a read-only delegated reference"
            ))),
        }
    }
}

impl<S: 'static> ScopeInvalidate for DelegatedCell<S> {
    fn invalidate(&self) {
        *self.enter.borrow_mut() = None;
    }
}

// ---------------------------------------------------------------------------

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
                userdata_metatables: RefCell::new(HashMap::new()),
                userdata_scoped_metatables: RefCell::new(HashMap::new()),
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
        let lua_weak = Rc::downgrade(&self.inner);
        let callable: LuaRustFunction = Rc::new(move |state| {
            let lua = match lua_weak.upgrade() {
                Some(inner) => Lua { inner },
                None => {
                    return Err(LuaError::runtime(format_args!(
                        "Lua callback fired after the state was dropped"
                    )))
                }
            };
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
        let lua_weak = Rc::downgrade(&self.inner);
        let callable: LuaRustFunction = Rc::new(move |state| {
            let lua = match lua_weak.upgrade() {
                Some(inner) => Lua { inner },
                None => {
                    return Err(LuaError::runtime(format_args!(
                        "Lua callback fired after the state was dropped"
                    )))
                }
            };
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
        let lua_weak = Rc::downgrade(&self.inner);
        let callable: LuaRustFunction = Rc::new(move |state| {
            let lua = match lua_weak.upgrade() {
                Some(inner) => Lua { inner },
                None => {
                    return Err(LuaError::runtime(format_args!(
                        "Lua callback fired after the state was dropped"
                    )))
                }
            };
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
        let type_id = TypeId::of::<T>();
        let cached = self
            .inner
            .userdata_metatables
            .borrow()
            .get(&type_id)
            .cloned();
        let metatable = match cached {
            Some(metatable) => metatable,
            None => {
                let mut methods = UserDataMethodRegistry::<T>::new(self);
                T::add_methods(&mut methods);
                T::add_meta_methods(&mut methods);
                let metatable = methods.build_metatable()?;
                self.inner
                    .userdata_metatables
                    .borrow_mut()
                    .insert(type_id, metatable.clone());
                metatable
            }
        };
        self.attach_userdata(data, metatable)
    }

    /// Wrap `data` in a fresh Lua userdata that shares `metatable` (built once per
    /// type by [`Lua::create_userdata`]). Only the per-value data cell is allocated
    /// here; the binding closures live on the shared, cached metatable.
    fn attach_userdata<T: UserData>(
        &self,
        data: T,
        metatable: GcRef<RawLuaTable>,
    ) -> Result<AnyUserData> {
        let cell: Rc<dyn Any> = Rc::new(UserDataCell {
            value: RefCell::new(data),
        });
        let host_value = cell.clone();
        let root = self.with_state(|state| {
            let userdata = with_heap_guard(state, || {
                GcRef::new(RawLuaUserData {
                    data: Box::new([]),
                    uv: Vec::new(),
                    metatable: RefCell::new(None),
                    host_value: RefCell::new(None),
                })
            });
            userdata.set_metatable(Some(metatable));
            userdata.set_host_value(Some(cell));
            let key = state.external_root_value(RawLuaValue::UserData(userdata));
            RootedValue {
                lua: self.clone(),
                key,
            }
        });
        Ok(AnyUserData {
            root,
            host_value: Some(host_value),
        })
    }

    /// Run `f` with a fresh [`Scope`]; any [`AnyUserData`] created via the
    /// scope is invalidated when `f` returns, so leaked references fail
    /// cleanly instead of using-after-the-borrow-ended.
    ///
    /// ```
    /// use lua_rs_runtime::{Lua, UserData, UserDataMethods};
    ///
    /// struct Counter { value: i64 }
    ///
    /// impl UserData for Counter {
    ///     fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
    ///         methods.add_method_mut("inc", |_lua, this, delta: i64| {
    ///             this.value += delta;
    ///             Ok(this.value)
    ///         });
    ///     }
    /// }
    ///
    /// let lua = Lua::new();
    /// let mut counter = Counter { value: 0 };
    ///
    /// lua.scope(|scope| {
    ///     let ud = scope.create_userdata_ref_mut(&lua, &mut counter)?;
    ///     lua.globals().set("c", &ud)?;
    ///     lua.load("c:inc(5); c:inc(7)").exec()
    /// }).unwrap();
    ///
    /// assert_eq!(counter.value, 12);
    ///
    /// // The script can stash the userdata on a global and try to use it
    /// // later, but the call cleanly errors instead of touching the
    /// // dropped `&mut counter`:
    /// lua.scope(|scope| {
    ///     let ud = scope.create_userdata_ref_mut(&lua, &mut counter)?;
    ///     lua.globals().set("leaked", &ud)
    /// }).unwrap();
    /// assert!(lua.load("leaked:inc(1)").exec().is_err());
    /// ```
    pub fn scope<F, R>(&self, f: F) -> Result<R>
    where
        F: for<'scope> FnOnce(&Scope<'scope>) -> Result<R>,
    {
        let scope = Scope::new();
        let result = f(&scope);
        // `scope` drops here, invalidating every cell it created. After this
        // point any Lua call that reaches a scoped userdata sees `ptr = None`
        // and errors.
        drop(scope);
        result
    }

    /// Build (or reuse) the per-`TypeId` *scoped* metatable for `T`. Same
    /// metatable serves both `Scope::create_userdata_ref_mut` userdata and
    /// `AnyUserData::delegate` sub-userdata of type `T`, because the
    /// dispatch closures are cell-variant-polymorphic via
    /// `dispatch_scoped_borrow*`.
    fn scoped_metatable_for<T>(&self) -> Result<GcRef<RawLuaTable>>
    where
        T: UserData,
    {
        let type_id = TypeId::of::<T>();
        let cached = self
            .inner
            .userdata_scoped_metatables
            .borrow()
            .get(&type_id)
            .cloned();
        if let Some(mt) = cached {
            return Ok(mt);
        }
        let mut methods = UserDataMethodRegistry::<T>::new_scoped(self);
        T::add_methods(&mut methods);
        T::add_meta_methods(&mut methods);
        let mt = methods.build_metatable()?;
        self.inner
            .userdata_scoped_metatables
            .borrow_mut()
            .insert(type_id, mt.clone());
        Ok(mt)
    }

    /// Attach the scoped metatable for `T` to a fresh userdata whose
    /// `host_value` is the given `ScopedCell<T>`.
    fn create_scoped_userdata<T>(&self, cell: Rc<ScopedCell<T>>) -> Result<AnyUserData>
    where
        T: UserData,
    {
        let metatable = self.scoped_metatable_for::<T>()?;
        self.attach_scoped_userdata::<T>(cell, metatable)
    }

    /// Same as `create_scoped_userdata` but the `host_value` is a
    /// `DelegatedCell<S>`. The metatable is the same per-`TypeId` cached
    /// metatable for `S`; dispatch handles both cell variants.
    fn create_delegated_userdata<S>(&self, cell: Rc<DelegatedCell<S>>) -> Result<AnyUserData>
    where
        S: UserData,
    {
        let metatable = self.scoped_metatable_for::<S>()?;
        let host_value: Rc<dyn Any> = cell;
        let root = self.with_state(|state| {
            let userdata = with_heap_guard(state, || {
                GcRef::new(RawLuaUserData {
                    data: Box::new([]),
                    uv: Vec::new(),
                    metatable: RefCell::new(None),
                    host_value: RefCell::new(None),
                })
            });
            userdata.set_metatable(Some(metatable));
            userdata.set_host_value(Some(host_value.clone()));
            let key = state.external_root_value(RawLuaValue::UserData(userdata));
            RootedValue {
                lua: self.clone(),
                key,
            }
        });
        Ok(AnyUserData {
            root,
            host_value: Some(host_value),
        })
    }

    /// Same shape as [`Self::attach_userdata`] but the `host_value` is the
    /// `ScopedCell` rather than a fresh `UserDataCell`.
    fn attach_scoped_userdata<T>(
        &self,
        cell: Rc<ScopedCell<T>>,
        metatable: GcRef<RawLuaTable>,
    ) -> Result<AnyUserData>
    where
        T: UserData,
    {
        let host_value: Rc<dyn Any> = cell;
        let root = self.with_state(|state| {
            let userdata = with_heap_guard(state, || {
                GcRef::new(RawLuaUserData {
                    data: Box::new([]),
                    uv: Vec::new(),
                    metatable: RefCell::new(None),
                    host_value: RefCell::new(None),
                })
            });
            userdata.set_metatable(Some(metatable));
            userdata.set_host_value(Some(host_value.clone()));
            let key = state.external_root_value(RawLuaValue::UserData(userdata));
            RootedValue {
                lua: self.clone(),
                key,
            }
        });
        Ok(AnyUserData {
            root,
            host_value: Some(host_value),
        })
    }

    /// Polymorphic borrow over the cell variants reachable by a scoped
    /// userdata: `Rc<ScopedCell<T>>` (created via
    /// `Scope::create_userdata_ref_mut`) and `Rc<DelegatedCell<T>>`
    /// (created via `AnyUserData::delegate`).
    ///
    /// Each variant has its own borrow protocol, but from the caller's
    /// perspective both produce a `&T` (or `&mut T`) for the duration of
    /// the closure. The result is threaded back out via an `Option` slot
    /// to satisfy `FnMut`'s constraint on the inner callback. The slot is
    /// always populated by the enter path before it returns.
    fn dispatch_scoped_borrow<T, F, R>(
        &self,
        userdata: &AnyUserData,
        f: F,
    ) -> Result<R>
    where
        T: 'static,
        F: FnOnce(&T) -> Result<R>,
    {
        let host = userdata
            .host_value
            .as_ref()
            .ok_or_else(|| LuaError::runtime(format_args!("missing Rust userdata payload")))?;

        if let Ok(scoped) = Rc::clone(host).downcast::<ScopedCell<T>>() {
            let borrow = scoped.try_borrow()?;
            return f(&*borrow);
        }

        if let Ok(delegated) = Rc::clone(host).downcast::<DelegatedCell<T>>() {
            let mut slot: Option<Result<R>> = None;
            let mut f_slot = Some(f);
            delegated.enter_ref(&mut |t| {
                if let Some(f) = f_slot.take() {
                    slot = Some(f(t));
                }
            })?;
            return slot.expect("delegated enter_ref must invoke its callback");
        }

        Err(LuaError::runtime(format_args!(
            "scoped userdata type mismatch"
        )))
    }

    fn dispatch_scoped_borrow_mut<T, F, R>(
        &self,
        userdata: &AnyUserData,
        f: F,
    ) -> Result<R>
    where
        T: 'static,
        F: FnOnce(&mut T) -> Result<R>,
    {
        let host = userdata
            .host_value
            .as_ref()
            .ok_or_else(|| LuaError::runtime(format_args!("missing Rust userdata payload")))?;

        if let Ok(scoped) = Rc::clone(host).downcast::<ScopedCell<T>>() {
            let mut borrow = scoped.try_borrow_mut()?;
            return f(&mut *borrow);
        }

        if let Ok(delegated) = Rc::clone(host).downcast::<DelegatedCell<T>>() {
            let mut slot: Option<Result<R>> = None;
            let mut f_slot = Some(f);
            delegated.enter_mut(&mut |t| {
                if let Some(f) = f_slot.take() {
                    slot = Some(f(t));
                }
            })?;
            return slot.expect("delegated enter_mut must invoke its callback");
        }

        Err(LuaError::runtime(format_args!(
            "scoped userdata type mismatch"
        )))
    }

    /// Scoped variants of the four `create_userdata_method*` constructors. Each
    /// uses `dispatch_scoped_borrow*` so the same registered metatable serves
    /// both `Scope::create_userdata_ref_mut` userdata and
    /// `AnyUserData::delegate` sub-userdata.
    fn create_scoped_userdata_method<T, A, R, F>(&self, method: F) -> Result<Function>
    where
        T: UserData,
        A: FromLuaMulti + 'static,
        R: IntoLuaMulti + 'static,
        F: Fn(&Lua, &T, A) -> Result<R> + 'static,
    {
        let lua_weak = Rc::downgrade(&self.inner);
        let callable: LuaRustFunction = Rc::new(move |state| {
            let lua = match lua_weak.upgrade() {
                Some(inner) => Lua { inner },
                None => {
                    return Err(LuaError::runtime(format_args!(
                        "Lua callback fired after the state was dropped"
                    )))
                }
            };
            match catch_unwind(AssertUnwindSafe(|| {
                let (userdata, args) = callback_userdata_args(state, &lua)?;
                let args = A::from_lua_multi(args, &lua)?;
                let returns = lua.dispatch_scoped_borrow::<T, _, _>(&userdata, |t| {
                    method(&lua, t, args)
                })?;
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

    fn create_scoped_userdata_method_mut<T, A, R, F>(&self, method: F) -> Result<Function>
    where
        T: UserData,
        A: FromLuaMulti + 'static,
        R: IntoLuaMulti + 'static,
        F: Fn(&Lua, &mut T, A) -> Result<R> + 'static,
    {
        let lua_weak = Rc::downgrade(&self.inner);
        let callable: LuaRustFunction = Rc::new(move |state| {
            let lua = match lua_weak.upgrade() {
                Some(inner) => Lua { inner },
                None => {
                    return Err(LuaError::runtime(format_args!(
                        "Lua callback fired after the state was dropped"
                    )))
                }
            };
            match catch_unwind(AssertUnwindSafe(|| {
                let (userdata, args) = callback_userdata_args(state, &lua)?;
                let args = A::from_lua_multi(args, &lua)?;
                let returns = lua.dispatch_scoped_borrow_mut::<T, _, _>(&userdata, |t| {
                    method(&lua, t, args)
                })?;
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

    /// Materialize a [`Function`] whose body dispatches through a
    /// [`ScopedFnCell`]. The cell is closed over by the `LuaRustFunction`
    /// trampoline; reads of `cell.ptr` are guarded inside `try_call`, so once
    /// the originating [`Scope`] drops, every subsequent invocation surfaces
    /// "no longer valid" instead of touching the released closure.
    fn create_scoped_function(&self, cell: Rc<ScopedFnCell>) -> Result<Function> {
        let lua_weak = Rc::downgrade(&self.inner);
        let callable: LuaRustFunction = Rc::new(move |state| {
            let lua = match lua_weak.upgrade() {
                Some(inner) => Lua { inner },
                None => {
                    return Err(LuaError::runtime(format_args!(
                        "Lua callback fired after the state was dropped"
                    )))
                }
            };
            match catch_unwind(AssertUnwindSafe(|| {
                let args = callback_args(state, &lua)?;
                let returns = cell.try_call(&lua, args)?;
                push_callback_returns(state, &lua, returns)
            })) {
                Ok(result) => result,
                Err(_) => Err(LuaError::runtime(format_args!(
                    "scoped Rust callback panicked"
                ))),
            }
        });
        self.create_registered_function(callable)
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

    /// Downcast `host_value` to a [`ScopedCell<T>`] reference. Mirrors
    /// [`Self::host_cell`] but for userdata created via [`Scope::create_userdata`].
    fn host_scoped_cell<T: 'static>(&self) -> Result<&ScopedCell<T>> {
        let host = self
            .host_value
            .as_deref()
            .ok_or_else(|| LuaError::runtime(format_args!("missing Rust userdata payload")))?;
        host.downcast_ref::<ScopedCell<T>>()
            .ok_or_else(|| LuaError::runtime(format_args!("scoped userdata type mismatch")))
    }

    /// Rust-side shared borrow of a [`Scope::create_userdata`] payload. Routes
    /// through the scoped cell, so calls after the scope has dropped fail with
    /// the same "no longer valid" error a Lua method call would see, instead
    /// of returning a stale reference.
    pub fn scoped_borrow<T, R>(&self, f: impl FnOnce(&T) -> R) -> Result<R>
    where
        T: 'static,
    {
        let cell = self.host_scoped_cell::<T>()?;
        let guard = cell.try_borrow()?;
        Ok(f(&*guard))
    }

    /// Rust-side exclusive borrow of a [`Scope::create_userdata`] payload. Same
    /// invalidation guarantees as [`Self::scoped_borrow`].
    pub fn scoped_borrow_mut<T, R>(&self, f: impl FnOnce(&mut T) -> R) -> Result<R>
    where
        T: 'static,
    {
        let cell = self.host_scoped_cell::<T>()?;
        let mut guard = cell.try_borrow_mut()?;
        Ok(f(&mut *guard))
    }

    /// Create a sub-userdata in the same scope that re-acquires `&mut S`
    /// from this userdata's payload via `accessor` on every method call.
    /// The sub-userdata holds no long-lived `&mut S`: every Lua method call
    /// borrows the parent (mut), applies `accessor`, runs the method,
    /// releases. If a script tries to call a parent method while inside a
    /// sub-userdata method body, the inner `try_borrow_mut` surfaces the
    /// same "already borrowed" error path scoped cells already use.
    ///
    /// Receiver must be a [`Scope::create_userdata_ref_mut`] userdata of
    /// type `P`, or another delegated userdata of type `P` (chains
    /// compose).
    ///
    /// Scope invalidation propagates: when the originating scope drops,
    /// both the parent and every delegated descendant become invalid.
    pub fn delegate<P, S, F>(&self, lua: &Lua, accessor: F) -> Result<AnyUserData>
    where
        P: UserData,
        S: UserData,
        F: Fn(&mut P) -> &mut S + 'static,
    {
        let host = self
            .host_value
            .as_ref()
            .ok_or_else(|| LuaError::runtime(format_args!("missing Rust userdata payload")))?;

        // Two parent variants are allowed: a direct `ScopedCell<P>` from
        // `Scope::create_userdata_ref_mut`, or another `DelegatedCell<P>`
        // for multi-level chains.
        if let Ok(parent_cell) = Rc::clone(host).downcast::<ScopedCell<P>>() {
            let parent_for_closure = Rc::clone(&parent_cell);
            let enter: Box<dyn Fn(&mut dyn FnMut(&mut S)) -> Result<()>> =
                Box::new(move |f| {
                    let mut guard = parent_for_closure.try_borrow_mut()?;
                    f(accessor(&mut *guard));
                    Ok(())
                });
            let cell = Rc::new(DelegatedCell::<S> {
                enter: RefCell::new(Some(DelegateEnter::Mut(enter))),
            });
            return lua.create_delegated_userdata::<S>(cell);
        }

        if let Ok(parent_cell) = Rc::clone(host).downcast::<DelegatedCell<P>>() {
            let parent_for_closure = Rc::clone(&parent_cell);
            let enter: Box<dyn Fn(&mut dyn FnMut(&mut S)) -> Result<()>> =
                Box::new(move |f| {
                    parent_for_closure.enter_mut(&mut |p| {
                        f(accessor(p));
                    })
                });
            let cell = Rc::new(DelegatedCell::<S> {
                enter: RefCell::new(Some(DelegateEnter::Mut(enter))),
            });
            return lua.create_delegated_userdata::<S>(cell);
        }

        Err(LuaError::runtime(format_args!(
            "delegate: receiver is not a scoped userdata of the expected type"
        )))
    }

    /// Shared counterpart to [`Self::delegate`]. The accessor takes `&P` and
    /// returns `&S`, the parent is borrowed shared per call, and the resulting
    /// sub-userdata is read-only: a mutating method on it fails with a clean
    /// runtime error. Used for `&self -> &S` accessors.
    pub fn delegate_ref<P, S, F>(&self, lua: &Lua, accessor: F) -> Result<AnyUserData>
    where
        P: UserData,
        S: UserData,
        F: Fn(&P) -> &S + 'static,
    {
        let host = self
            .host_value
            .as_ref()
            .ok_or_else(|| LuaError::runtime(format_args!("missing Rust userdata payload")))?;

        if let Ok(parent_cell) = Rc::clone(host).downcast::<ScopedCell<P>>() {
            let parent_for_closure = Rc::clone(&parent_cell);
            let enter: Box<dyn Fn(&mut dyn FnMut(&S)) -> Result<()>> = Box::new(move |f| {
                let guard = parent_for_closure.try_borrow()?;
                f(accessor(&*guard));
                Ok(())
            });
            let cell = Rc::new(DelegatedCell::<S> {
                enter: RefCell::new(Some(DelegateEnter::Ref(enter))),
            });
            return lua.create_delegated_userdata::<S>(cell);
        }

        if let Ok(parent_cell) = Rc::clone(host).downcast::<DelegatedCell<P>>() {
            let parent_for_closure = Rc::clone(&parent_cell);
            let enter: Box<dyn Fn(&mut dyn FnMut(&S)) -> Result<()>> = Box::new(move |f| {
                parent_for_closure.enter_ref(&mut |p| {
                    f(accessor(p));
                })
            });
            let cell = Rc::new(DelegatedCell::<S> {
                enter: RefCell::new(Some(DelegateEnter::Ref(enter))),
            });
            return lua.create_delegated_userdata::<S>(cell);
        }

        Err(LuaError::runtime(format_args!(
            "delegate_ref: receiver is not a scoped userdata of the expected type"
        )))
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

    /// Register a getter for `obj.name`. The runtime composes all field getters,
    /// the method table, and any raw `__index` into a single `__index` so fields
    /// and methods coexist (lookup order: field, then method, then raw `__index`).
    fn add_field_method_get<R, F>(&mut self, name: &str, getter: F)
    where
        R: IntoLuaMulti + 'static,
        F: Fn(&Lua, &T) -> Result<R> + 'static;

    /// Register a setter for `obj.name = value`. Assigning a field with no setter
    /// (or an unknown field) errors unless a raw `__newindex` handles it.
    fn add_field_method_set<A, F>(&mut self, name: &str, setter: F)
    where
        A: FromLuaMulti + 'static,
        F: Fn(&Lua, &mut T, A) -> Result<()> + 'static;

    /// Register a "function-shape" method whose callback does not extract the
    /// typed `&T` automatically. The userdata handle (and any other args) is
    /// passed to the closure as a regular [`FromLuaMulti`] tuple, so `A` is
    /// usually `(AnyUserData, X, Y, ...)`.
    ///
    /// Equivalent to mlua's `UserDataMethods::add_function`. The main reason
    /// to reach for this over [`Self::add_method`] is when the callback body
    /// needs the [`AnyUserData`] handle for the receiver — most commonly to
    /// build a sub-userdata via [`AnyUserData::delegate`].
    fn add_function<A, R, F>(&mut self, name: &str, function: F)
    where
        A: FromLuaMulti + 'static,
        R: IntoLuaMulti + 'static,
        F: Fn(&Lua, A) -> Result<R> + 'static;

    /// `FnMut` variant of [`Self::add_function`]. Re-entrant calls into the
    /// same closure are rejected with an "already borrowed" runtime error.
    fn add_function_mut<A, R, F>(&mut self, name: &str, function: F)
    where
        A: FromLuaMulti + 'static,
        R: IntoLuaMulti + 'static,
        F: FnMut(&Lua, A) -> Result<R> + 'static;
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

/// Root `value` on the state for as long as the state itself lives.
///
/// The returned [`ExternalRootKey`] is intentionally discarded: this helper is
/// the explicit name for the "cached per-type metadata" rooting pattern used by
/// [`UserDataMethodRegistry::build_metatable`] (the metatable itself, the
/// field-getter / method / field-setter tables, and any raw `__index`/`__newindex`
/// referenced by the composed dispatch closures). Those values must stay
/// reachable for the state's whole lifetime and only ever free together with the
/// state. Do not call this for any value you want the GC to be able to collect
/// later: it is by design an un-undoable root.
fn root_for_state_lifetime(state: &mut LuaState, value: RawLuaValue) {
    let _ = state.external_root_value(value);
}

/// Whether the registry wires methods through `create_userdata_method*` (owned
/// `T` in a `RefCell`) or through `create_scoped_userdata_method*`
/// (`Rc<ScopedCell<T>>` with a validity-checked pointer). The build_metatable
/// step is identical for both.
#[derive(Clone, Copy)]
enum RegistryMode {
    Owned,
    Scoped,
}

struct UserDataMethodRegistry<'lua, T: UserData> {
    lua: &'lua Lua,
    mode: RegistryMode,
    methods: Vec<(String, Function)>,
    meta_methods: Vec<(MetaMethod, Function)>,
    fields_get: Vec<(String, Function)>,
    fields_set: Vec<(String, Function)>,
    error: Option<LuaError>,
    _marker: std::marker::PhantomData<T>,
}

impl<'lua, T: UserData> UserDataMethodRegistry<'lua, T> {
    fn new(lua: &'lua Lua) -> Self {
        Self::with_mode(lua, RegistryMode::Owned)
    }

    fn new_scoped(lua: &'lua Lua) -> Self {
        Self::with_mode(lua, RegistryMode::Scoped)
    }

    fn with_mode(lua: &'lua Lua, mode: RegistryMode) -> Self {
        Self {
            lua,
            mode,
            methods: Vec::new(),
            meta_methods: Vec::new(),
            fields_get: Vec::new(),
            fields_set: Vec::new(),
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

    /// Build this type's metatable once: a method table plus any meta-methods,
    /// returning the raw table handle permanently rooted in the external-root set
    /// so it can be cached and shared by every value of the type.
    fn build_metatable(mut self) -> Result<GcRef<RawLuaTable>> {
        if let Some(err) = self.error.take() {
            return Err(err);
        }

        let lua = self.lua;

        let method_table = lua.create_table()?;
        for (name, function) in &self.methods {
            method_table.set(name.as_str(), function)?;
        }

        let field_getters = lua.create_table()?;
        for (name, function) in &self.fields_get {
            field_getters.set(name.as_str(), function)?;
        }
        let field_setters = lua.create_table()?;
        for (name, function) in &self.fields_set {
            field_setters.set(name.as_str(), function)?;
        }

        // Raw __index/__newindex are escape hatches that compose as the final
        // fallback; every other meta-method is set directly.
        let metatable = lua.create_table()?;
        let mut raw_index: Option<Function> = None;
        let mut raw_newindex: Option<Function> = None;
        for (metamethod, function) in &self.meta_methods {
            match metamethod {
                MetaMethod::Index => raw_index = Some(function.clone()),
                MetaMethod::NewIndex => raw_newindex = Some(function.clone()),
                other => {
                    metatable.set(other.name(), function)?;
                }
            }
        }

        // __index: field getter, then method, then raw __index.
        //
        // - fields → must compose (field → method → raw via a single closure)
        // - raw_index + methods (no fields) → must compose (method → raw)
        // - raw_index only (no fields, no methods) → set raw __index directly,
        //   skipping the composed closure entirely. This is the common shape
        //   for bridges that bind reflected state via a raw `add_meta_method`
        //   (e.g. bms-lua-rs's `LuaRef`) and the lookup is on the hot path.
        // - method-only → method_table as __index (existing fast path)
        //
        // The composed closure deliberately captures raw `GcRef`/`RawLuaValue`
        // handles, not high-level `Table`/`Function`: each high-level wrapper
        // holds a `RootedValue` with a strong `Rc<LuaInner>`, which would cycle
        // through the heap-resident closure back to the state and leak it on
        // drop. Raw handles are rooted permanently via
        // [`root_for_state_lifetime`], and `Table`/`Function` views are rebuilt
        // per call from the closure's `&lua`.
        let has_fields_get = !self.fields_get.is_empty();
        let has_methods = !self.methods.is_empty();
        let needs_index_composition = has_fields_get || (raw_index.is_some() && has_methods);

        if needs_index_composition {
            let (getters_raw, methods_raw, raw_index_raw) = lua.with_state(|state| {
                let g = match field_getters.root.raw_for_lua(lua, state)? {
                    RawLuaValue::Table(g) => g,
                    v => return Err(type_error_raw(&v, "table")),
                };
                root_for_state_lifetime(state, RawLuaValue::Table(g.clone()));
                let m = match method_table.root.raw_for_lua(lua, state)? {
                    RawLuaValue::Table(m) => m,
                    v => return Err(type_error_raw(&v, "table")),
                };
                root_for_state_lifetime(state, RawLuaValue::Table(m.clone()));
                let r = match &raw_index {
                    Some(f) => {
                        let rv = f.root.raw_for_lua(lua, state)?;
                        root_for_state_lifetime(state, rv.clone());
                        Some(rv)
                    }
                    None => None,
                };
                Ok::<_, LuaError>((g, m, r))
            })?;
            let index_fn = lua.create_function(move |lua, (ud, key): (Value, Value)| {
                let getters = Table {
                    root: lua.root_raw(RawLuaValue::Table(getters_raw.clone())),
                };
                let methods = Table {
                    root: lua.root_raw(RawLuaValue::Table(methods_raw.clone())),
                };
                if let Value::Function(getter) = getters.get::<_, Value>(key.clone())? {
                    return getter.call::<_, Value>(ud);
                }
                let method = methods.get::<_, Value>(key.clone())?;
                if !matches!(method, Value::Nil) {
                    return Ok(method);
                }
                if let Some(raw_idx) = &raw_index_raw {
                    let raw_fn = Function {
                        root: lua.root_raw(raw_idx.clone()),
                    };
                    return raw_fn.call::<_, Value>((ud, key));
                }
                Ok(Value::Nil)
            })?;
            metatable.set(MetaMethod::Index.name(), &index_fn)?;
        } else if let Some(raw) = raw_index.as_ref() {
            metatable.set(MetaMethod::Index.name(), raw)?;
        } else {
            metatable.set(MetaMethod::Index.name(), &method_table)?;
        }

        // __newindex: field setter, then raw __newindex, else error. Same
        // composed-vs-pass-through choice as __index above.
        let has_fields_set = !self.fields_set.is_empty();

        if has_fields_set {
            let (setters_raw, raw_newindex_raw) = lua.with_state(|state| {
                let s = match field_setters.root.raw_for_lua(lua, state)? {
                    RawLuaValue::Table(s) => s,
                    v => return Err(type_error_raw(&v, "table")),
                };
                root_for_state_lifetime(state, RawLuaValue::Table(s.clone()));
                let r = match &raw_newindex {
                    Some(f) => {
                        let rv = f.root.raw_for_lua(lua, state)?;
                        root_for_state_lifetime(state, rv.clone());
                        Some(rv)
                    }
                    None => None,
                };
                Ok::<_, LuaError>((s, r))
            })?;
            let newindex_fn =
                lua.create_function(move |lua, (ud, key, value): (Value, Value, Value)| {
                    let setters = Table {
                        root: lua.root_raw(RawLuaValue::Table(setters_raw.clone())),
                    };
                    if let Value::Function(setter) = setters.get::<_, Value>(key.clone())? {
                        return setter.call::<_, Value>((ud, value));
                    }
                    if let Some(raw) = &raw_newindex_raw {
                        let raw_fn = Function {
                            root: lua.root_raw(raw.clone()),
                        };
                        return raw_fn.call::<_, Value>((ud, key, value));
                    }
                    Err(LuaError::runtime(format_args!(
                        "cannot assign to unknown or read-only userdata field"
                    )))
                })?;
            metatable.set(MetaMethod::NewIndex.name(), &newindex_fn)?;
        } else if let Some(raw) = raw_newindex.as_ref() {
            metatable.set(MetaMethod::NewIndex.name(), raw)?;
        }

        self.lua.with_state(|state| {
            let metatable_raw = metatable.root.raw_for_lua(self.lua, state)?;
            let RawLuaValue::Table(metatable) = metatable_raw else {
                return Err(type_error_raw(&metatable_raw, "table"));
            };
            root_for_state_lifetime(state, RawLuaValue::Table(metatable.clone()));
            Ok(metatable)
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
        let result = match self.mode {
            RegistryMode::Owned => self.lua.create_userdata_method(method),
            RegistryMode::Scoped => self.lua.create_scoped_userdata_method(method),
        };
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
        let result = match self.mode {
            RegistryMode::Owned => self.lua.create_userdata_method_mut(method),
            RegistryMode::Scoped => self.lua.create_scoped_userdata_method_mut(method),
        };
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
        let result = match self.mode {
            RegistryMode::Owned => self.lua.create_userdata_method(method),
            RegistryMode::Scoped => self.lua.create_scoped_userdata_method(method),
        };
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
        let result = match self.mode {
            RegistryMode::Owned => self.lua.create_userdata_method_mut(method),
            RegistryMode::Scoped => self.lua.create_scoped_userdata_method_mut(method),
        };
        self.record(result, move |this, function| {
            this.meta_methods.push((metamethod, function));
        });
    }

    fn add_field_method_get<R, F>(&mut self, name: &str, getter: F)
    where
        R: IntoLuaMulti + 'static,
        F: Fn(&Lua, &T) -> Result<R> + 'static,
    {
        let name = name.to_string();
        let wrapped = move |lua: &Lua, this: &T, ()| getter(lua, this);
        let result = match self.mode {
            RegistryMode::Owned => self.lua.create_userdata_method(wrapped),
            RegistryMode::Scoped => self.lua.create_scoped_userdata_method(wrapped),
        };
        self.record(result, move |this, function| {
            this.fields_get.push((name, function));
        });
    }

    fn add_field_method_set<A, F>(&mut self, name: &str, setter: F)
    where
        A: FromLuaMulti + 'static,
        F: Fn(&Lua, &mut T, A) -> Result<()> + 'static,
    {
        let name = name.to_string();
        let wrapped = move |lua: &Lua, this: &mut T, arg: A| setter(lua, this, arg);
        let result = match self.mode {
            RegistryMode::Owned => self.lua.create_userdata_method_mut(wrapped),
            RegistryMode::Scoped => self.lua.create_scoped_userdata_method_mut(wrapped),
        };
        self.record(result, move |this, function| {
            this.fields_set.push((name, function));
        });
    }

    fn add_function<A, R, F>(&mut self, name: &str, function: F)
    where
        A: FromLuaMulti + 'static,
        R: IntoLuaMulti + 'static,
        F: Fn(&Lua, A) -> Result<R> + 'static,
    {
        let name = name.to_string();
        // Function-shape entries don't extract `&T` from the receiver, so
        // they reuse the existing top-level `Lua::create_function` directly
        // for both Owned and Scoped registry modes.
        let result = self.lua.create_function(function);
        self.record(result, move |this, function| {
            this.methods.push((name, function));
        });
    }

    fn add_function_mut<A, R, F>(&mut self, name: &str, function: F)
    where
        A: FromLuaMulti + 'static,
        R: IntoLuaMulti + 'static,
        F: FnMut(&Lua, A) -> Result<R> + 'static,
    {
        let name = name.to_string();
        let result = self.lua.create_function_mut(function);
        self.record(result, move |this, function| {
            this.methods.push((name, function));
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
        Self {
            instruction_limit: Some(10_000_000),
            memory_limit_bytes: Some(64 * 1024 * 1024),
            check_interval: 1000,
            remove_globals: lua_stdlib::sandbox::STRICT_REMOVED_GLOBALS
                .iter()
                .map(|s| s.to_vec())
                .collect(),
        }
    }
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self::strict()
    }
}

fn strip_globals(state: &mut LuaState, names: &[Vec<u8>]) -> Result<()> {
    let refs: Vec<&[u8]> = names.iter().map(|n| n.as_slice()).collect();
    lua_stdlib::sandbox::strip_globals(state, &refs)
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
    fn metatable_is_built_once_per_type() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static BUILDS: AtomicUsize = AtomicUsize::new(0);

        struct Widget {
            n: i64,
        }
        impl UserData for Widget {
            fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
                BUILDS.fetch_add(1, Ordering::SeqCst);
                methods.add_method("n", |_lua, this, ()| Ok(this.n));
            }
        }

        let lua = Lua::new();
        let a = lua.create_userdata(Widget { n: 1 }).expect("first");
        let b = lua.create_userdata(Widget { n: 2 }).expect("second");
        let c = lua.create_userdata(Widget { n: 3 }).expect("third");

        // Built exactly once despite three values of the same type.
        assert_eq!(BUILDS.load(Ordering::SeqCst), 1);

        // Each value still carries its own data and dispatches correctly.
        let globals = lua.globals();
        globals.set("a", &a).unwrap();
        globals.set("b", &b).unwrap();
        globals.set("c", &c).unwrap();
        let sum: i64 = lua.load("return a:n() + b:n() + c:n()").eval().unwrap();
        assert_eq!(sum, 6);
    }

    /// Reproducer for the callback-to-`Lua` reference cycle:
    /// `create_userdata_method` captures a strong `Lua` (`Rc<LuaInner>`) into each
    /// callback closure, the closure lives in a heap GC object owned by `LuaState`,
    /// and `LuaState` is owned by `LuaInner` — so dropping every external `Lua`
    /// handle still leaves the closures holding a strong `Rc<LuaInner>` to the
    /// state that owns them. Per-type metatable caching makes this permanent for
    /// any type a userdata is ever created for.
    ///
    /// This test holds a `Weak<LuaInner>`, drops every external `Lua`, and asserts
    /// the inner has actually been freed. It fails today and is what the
    /// `Weak`-capture fix in the callback constructors is meant to make pass.
    #[test]
    fn lua_state_frees_after_userdata_with_methods_is_dropped() {
        use std::rc::Rc;

        let weak_inner = {
            let lua = Lua::new();
            let weak = Rc::downgrade(&lua.inner);
            // Create + drop a userdata of a type that registers methods. This
            // primes the per-type metatable cache and installs method closures
            // that capture `Lua` strongly.
            let _ = lua
                .create_userdata(Counter { value: 1 })
                .expect("userdata should create");
            weak
        };

        assert!(
            weak_inner.upgrade().is_none(),
            "LuaInner is still alive after every external Lua handle dropped: \
             internal callback closures hold a strong Rc<LuaInner>, leaking the state"
        );
    }

    /// Same cycle issue as above, on the `create_function` path: the Rust
    /// callback closure used to capture a strong `Lua`, so a function that
    /// outlived all external handles would keep the state pinned.
    #[test]
    fn lua_state_frees_after_create_function_handle_drops() {
        use std::rc::Rc;

        let weak_inner = {
            let lua = Lua::new();
            let weak = Rc::downgrade(&lua.inner);
            let _f = lua
                .create_function(|_, ()| Ok(()))
                .expect("create_function should succeed");
            weak
        };

        assert!(
            weak_inner.upgrade().is_none(),
            "LuaInner is still alive after the only Lua handle dropped: \
             the create_function callback held a strong Rc<LuaInner>"
        );
    }

    /// Field-bearing types take the composed `__index` path in `build_metatable`,
    /// where the composing closure is itself passed to `create_function` and
    /// captures the field-getter table, method table, and optional raw
    /// `__index` function. Each of those is a `Table` or `Function` whose
    /// `RootedValue` holds a strong `Rc<LuaInner>`. Even with the outer
    /// `Weak` fix, that user closure still leaks the state.
    #[test]
    fn lua_state_frees_after_userdata_with_fields_drops() {
        use std::rc::Rc;

        struct Point {
            x: f64,
        }
        impl UserData for Point {
            fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
                m.add_field_method_get("x", |_, this| Ok(this.x));
                m.add_field_method_set("x", |_, this, v: f64| {
                    this.x = v;
                    Ok(())
                });
            }
        }

        let weak_inner = {
            let lua = Lua::new();
            let weak = Rc::downgrade(&lua.inner);
            let _ = lua
                .create_userdata(Point { x: 1.0 })
                .expect("userdata should create");
            weak
        };

        assert!(
            weak_inner.upgrade().is_none(),
            "LuaInner leaked via the composed __index/__newindex closures: \
             they capture Table/Function values whose RootedValue holds a \
             strong Rc<LuaInner>"
        );
    }

    /// Maximal mixed shape: field getter + field setter + regular method +
    /// raw `__index` + raw `__newindex` all on one type. Exercises every
    /// branch of the composed dispatch and every permanently rooted handle.
    /// If a future change reintroduces a captured wrapper anywhere in the
    /// composition path, this is the test most likely to catch it.
    #[test]
    fn lua_state_frees_with_fields_methods_and_raw_meta() {
        use std::rc::Rc;

        struct Mixed {
            x: f64,
            log: Vec<String>,
        }
        impl UserData for Mixed {
            fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
                m.add_field_method_get("x", |_, this| Ok(this.x));
                m.add_field_method_set("x", |_, this, v: f64| {
                    this.x = v;
                    Ok(())
                });
                m.add_method("log_len", |_, this, ()| Ok(this.log.len() as i64));
                m.add_method_mut("push_log", |_, this, s: String| {
                    this.log.push(s);
                    Ok(())
                });
                m.add_meta_method(MetaMethod::Index, |_, _this, key: String| {
                    Ok(::std::format!("dynamic:{key}"))
                });
                m.add_meta_method_mut(
                    MetaMethod::NewIndex,
                    |_, _this, (_k, _v): (String, Value)| Ok(()),
                );
            }
        }

        let weak_inner = {
            let lua = Lua::new();
            let weak = Rc::downgrade(&lua.inner);
            let _ = lua
                .create_userdata(Mixed {
                    x: 1.0,
                    log: Vec::new(),
                })
                .expect("create");
            weak
        };

        assert!(
            weak_inner.upgrade().is_none(),
            "maximal-composition userdata leaked LuaInner: \
             check the composed __index / __newindex captures"
        );
    }

    /// The composed `__index` allocates two or three temporary external roots
    /// per call (for the per-call `Table`/`Function` views) and relies on
    /// `pending_external_unroots` being flushed by the next `with_state`. If
    /// that plumbing ever breaks, every field read silently leaks a root. Hammer
    /// it in a loop and assert `external_roots.len()` returns to baseline.
    #[test]
    fn composed_dispatch_does_not_accumulate_external_roots() {
        struct Probe {
            x: i64,
        }
        impl UserData for Probe {
            fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
                m.add_field_method_get("x", |_, this| Ok(this.x));
            }
        }

        let lua = Lua::new();
        lua.globals()
            .set("v", lua.create_userdata(Probe { x: 1 }).unwrap())
            .unwrap();
        let baseline = external_root_count(&lua);

        for _ in 0..1000 {
            let _: i64 = lua.load("return v.x").eval().unwrap();
        }
        // The last iteration's temp roots queue for unroot on exit of its
        // outer with_state; force one more so the flush definitely runs.
        let after = external_root_count(&lua);

        assert!(
            after <= baseline + 2,
            "external roots grew under composed __index churn: baseline={baseline} after={after}"
        );
    }

    /// A Rust userdata method takes a Lua `Function` and calls it. Exercises
    /// the Weak<LuaInner> upgrade plus the `active_state` reentrancy pointer
    /// together. The bms-lua-rs reflection bridge hits this shape on every
    /// component access; an existing test covers `create_function` reentry but
    /// not the userdata-method path.
    #[test]
    fn userdata_method_can_reenter_lua_from_callback() {
        struct Calc;
        impl UserData for Calc {
            fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
                m.add_method("apply", |_lua, _this, f: Function| {
                    let r: i64 = f.call(7_i64)?;
                    Ok(r + 1)
                });
            }
        }

        let lua = Lua::new();
        lua.globals()
            .set("c", lua.create_userdata(Calc).unwrap())
            .unwrap();
        let r: i64 = lua
            .load("return c:apply(function(n) return n * 2 end)")
            .eval()
            .unwrap();
        assert_eq!(r, 15);
    }

    /// Two `Lua::new()` instances must each build their own metatable for the
    /// same Rust type. Counts calls to `add_methods` across both states and
    /// asserts each state builds independently while still de-duplicating
    /// within its own scope.
    #[test]
    fn metatable_cache_is_per_lua_state() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static BUILDS: AtomicUsize = AtomicUsize::new(0);

        struct Marker {
            v: i64,
        }
        impl UserData for Marker {
            fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
                BUILDS.fetch_add(1, Ordering::SeqCst);
                m.add_method("v", |_, this, ()| Ok(this.v));
            }
        }

        let start = BUILDS.load(Ordering::SeqCst);

        let lua_a = Lua::new();
        let _a1 = lua_a.create_userdata(Marker { v: 1 }).unwrap();
        assert_eq!(BUILDS.load(Ordering::SeqCst) - start, 1, "state A first build");
        let _a2 = lua_a.create_userdata(Marker { v: 2 }).unwrap();
        assert_eq!(BUILDS.load(Ordering::SeqCst) - start, 1, "state A reuses cache");

        let lua_b = Lua::new();
        let _b1 = lua_b.create_userdata(Marker { v: 3 }).unwrap();
        assert_eq!(BUILDS.load(Ordering::SeqCst) - start, 2, "state B is independent");

        let _a3 = lua_a.create_userdata(Marker { v: 4 }).unwrap();
        assert_eq!(BUILDS.load(Ordering::SeqCst) - start, 2, "state A still cached");
    }

    /// Field beats method when names collide. The composed `__index` looks up
    /// field getters before the method table; pin that order so a future
    /// refactor of the dispatch closure does not silently swap precedence.
    #[test]
    fn field_shadows_method_of_same_name() {
        struct Shadow {
            x: i64,
        }
        impl UserData for Shadow {
            fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
                m.add_field_method_get("x", |_, this| Ok(this.x));
                m.add_method("x", |_, _this, ()| Ok(999_i64));
            }
        }

        let lua = Lua::new();
        lua.globals()
            .set("v", lua.create_userdata(Shadow { x: 42 }).unwrap())
            .unwrap();

        let r: i64 = lua.load("return v.x").eval().unwrap();
        assert_eq!(r, 42, "the field getter should beat the method of the same name");
    }

    /// Direct Lua-side proof the cache is real: two userdata of the same type
    /// share the same metatable object as observed by `getmetatable`. If the
    /// cache regressed to per-value metatables this returns false.
    #[test]
    fn cached_metatable_is_shared_across_values_in_lua() {
        struct Twin;
        impl UserData for Twin {
            fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
                m.add_method("ping", |_, _this, ()| Ok(1_i64));
            }
        }

        let lua = Lua::new();
        lua.globals()
            .set("a", lua.create_userdata(Twin).unwrap())
            .unwrap();
        lua.globals()
            .set("b", lua.create_userdata(Twin).unwrap())
            .unwrap();

        let same: bool = lua
            .load("return getmetatable(a) == getmetatable(b)")
            .eval()
            .unwrap();
        assert!(same, "cached metatable must be shared across values of the same type");
    }

    #[test]
    fn fields_and_methods_coexist() {
        struct Vec2 {
            x: f64,
            y: f64,
        }
        impl UserData for Vec2 {
            fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
                m.add_field_method_get("x", |_, this| Ok(this.x));
                m.add_field_method_get("y", |_, this| Ok(this.y));
                m.add_field_method_set("x", |_, this, v: f64| {
                    this.x = v;
                    Ok(())
                });
                m.add_field_method_set("y", |_, this, v: f64| {
                    this.y = v;
                    Ok(())
                });
                m.add_method("length", |_, this, ()| {
                    Ok((this.x * this.x + this.y * this.y).sqrt())
                });
                m.add_method_mut("scale", |_, this, k: f64| {
                    this.x *= k;
                    this.y *= k;
                    Ok(())
                });
            }
        }

        let lua = Lua::new();
        let v = lua.create_userdata(Vec2 { x: 3.0, y: 4.0 }).unwrap();
        lua.globals().set("v", &v).unwrap();

        // method call and field reads on the same value
        assert_eq!(lua.load("return v:length()").eval::<f64>().unwrap(), 5.0);
        assert_eq!(lua.load("return v.x + v.y").eval::<f64>().unwrap(), 7.0);

        // field write
        lua.load("v.x = 6").exec().unwrap();
        assert_eq!(lua.load("return v.x").eval::<f64>().unwrap(), 6.0);

        // method mutation is visible through field reads
        lua.load("v:scale(2)").exec().unwrap();
        assert_eq!(lua.load("return v.x").eval::<f64>().unwrap(), 12.0);
        assert_eq!(lua.load("return v.y").eval::<f64>().unwrap(), 8.0);

        // unknown field assignment errors
        assert!(lua.load("v.z = 1").exec().is_err());
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

    /// Pull the human-readable message out of a `LuaError::Runtime(LuaValue::Str)`.
    /// The default `Display` for `LuaError` just defers to `Debug`, which prints
    /// `Runtime(Str(GcRef(Gc(0x…))))` for runtime errors that were raised through
    /// Lua. The actual string lives behind the GcRef; this helper digs it out so
    /// assertions can check the message text directly.
    fn runtime_error_message(err: &LuaError) -> String {
        match err {
            LuaError::Runtime(v) | LuaError::Syntax(v) => match v {
                RawLuaValue::Str(s) => String::from_utf8_lossy(s.as_bytes()).into_owned(),
                other => format!("{other:?}"),
            },
            other => format!("{other:?}"),
        }
    }

    /// Helper userdata for scope tests: carries a single mutable field so a
    /// `&mut Counter` borrow handed to Lua can be observed from Rust after the
    /// scope ends. Distinct from the module-level `Counter` only to keep the
    /// owned-vs-scoped paths from sharing fixtures.
    struct ScopedCounter {
        value: i64,
        calls: Cell<u32>,
    }

    impl UserData for ScopedCounter {
        fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
            methods.add_method("get", |_lua, this, ()| {
                this.calls.set(this.calls.get() + 1);
                Ok(this.value)
            });
            methods.add_method_mut("inc", |_lua, this, delta: i64| {
                this.value += delta;
                Ok(this.value)
            });
            methods.add_method("calls", |_lua, this, ()| Ok(this.calls.get() as i64));
            methods.add_method("call_get_via_global", |lua, _this, ()| {
                lua.load("return c:get()").eval::<i64>()
            });
            methods.add_method_mut("inc_via_global", |lua, this, ()| {
                this.value += 1;
                lua.load("return c:get()").eval::<i64>()
            });
        }
    }

    struct ScopedBag {
        value: i64,
    }

    impl UserData for ScopedBag {
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

    struct ScopedFielded {
        n: i64,
    }

    impl UserData for ScopedFielded {
        fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
            methods.add_field_method_get("n", |_lua, this| Ok(this.n));
            methods.add_field_method_set("n", |_lua, this, new: i64| {
                this.n = new;
                Ok(())
            });
        }
    }

    /// Smoke test for [`Lua::scope`]: a `&mut ScopedCounter` borrow lives on
    /// the Rust stack, gets handed to Lua as a userdata for the duration of a
    /// scope body, and the original is mutated through it.
    #[test]
    fn scope_userdata_dispatches_method_calls_against_borrow() {
        let lua = Lua::new();
        let mut counter = ScopedCounter {
            value: 10,
            calls: Cell::new(0),
        };

        let observed: i64 = lua
            .scope(|scope| {
                let ud = scope.create_userdata_ref_mut(&lua, &mut counter)?;
                lua.globals().set("c", &ud)?;
                lua.load("return c:get()").eval::<i64>()
            })
            .expect("scope body should succeed");
        assert_eq!(observed, 10);
        assert_eq!(counter.value, 10);
        assert_eq!(counter.calls.get(), 1);
    }

    /// Mutations through a scoped `&mut T` method must be visible to the Rust
    /// owner after the scope returns. This is the central reason for the API:
    /// `&mut World` etc. need to round-trip cleanly.
    #[test]
    fn scope_userdata_mut_method_propagates_to_external_borrow() {
        let lua = Lua::new();
        let mut counter = ScopedCounter {
            value: 0,
            calls: Cell::new(0),
        };

        lua.scope(|scope| {
            let ud = scope.create_userdata_ref_mut(&lua, &mut counter)?;
            lua.globals().set("c", &ud)?;
            lua.load("c:inc(5); c:inc(7)").exec()
        })
        .expect("scope body should succeed");
        assert_eq!(counter.value, 12);
    }

    /// Headline safety property: any AnyUserData that leaks past its scope
    /// must fail cleanly (Lua runtime error), not touch the freed `&mut` slot.
    /// We persist the leaked userdata on `globals` precisely to model the
    /// adversarial case from the issue: a script squirrels away a `&mut World`
    /// and tries to use it later.
    #[test]
    fn scope_userdata_invalidated_after_scope_returns_runtime_error() {
        let lua = Lua::new();
        let mut counter = ScopedCounter {
            value: 99,
            calls: Cell::new(0),
        };

        lua.scope(|scope| {
            let ud = scope.create_userdata_ref_mut(&lua, &mut counter)?;
            lua.globals().set("leaked", &ud)?;
            Ok(())
        })
        .expect("scope body should succeed");

        let err = lua
            .load("return leaked:get()")
            .eval::<i64>()
            .expect_err("scoped userdata must be unusable after scope ends");
        let msg = runtime_error_message(&err);
        assert!(
            msg.contains("no longer valid") || msg.contains("scope has ended"),
            "expected invalidation error, got: {msg}"
        );
    }

    /// Even a `pcall`-wrapped post-scope invocation must surface a Lua-level
    /// error rather than crashing. Models the case where the script tries to
    /// recover from the failure.
    #[test]
    fn scope_userdata_invalidated_is_recoverable_via_pcall() {
        let lua = Lua::new();
        let mut counter = ScopedCounter {
            value: 5,
            calls: Cell::new(0),
        };

        lua.scope(|scope| {
            let ud = scope.create_userdata_ref_mut(&lua, &mut counter)?;
            lua.globals().set("leaked", &ud)?;
            Ok(())
        })
        .expect("scope body should succeed");

        let (ok, _err_msg): (bool, String) = lua
            .load("local ok, e = pcall(function() return leaked:get() end); return ok, tostring(e)")
            .eval()
            .expect("pcall harness should produce two values");
        assert!(!ok, "post-scope call must fail");
    }

    /// Re-entry from inside a `&mut` method body into Lua that calls another
    /// method on the *same* scoped userdata must be rejected at the second
    /// borrow attempt, not produce aliasing `&mut`s. This is the aliasing
    /// concern called out in the design.
    #[test]
    fn scope_userdata_reentrant_borrow_during_mut_method_returns_error() {
        let lua = Lua::new();
        let mut counter = ScopedCounter {
            value: 0,
            calls: Cell::new(0),
        };

        let err = lua
            .scope(|scope| {
                let ud = scope.create_userdata_ref_mut(&lua, &mut counter)?;
                lua.globals().set("c", &ud)?;
                lua.load("return c:inc_via_global()").eval::<i64>()
            })
            .expect_err("re-entry while mut-borrowed must fail");
        let msg = runtime_error_message(&err);
        assert!(
            msg.contains("already") && msg.contains("borrowed"),
            "expected borrow-conflict error, got: {msg}"
        );
        assert_eq!(counter.value, 1, "outer mutation persists despite inner failure");
    }

    /// Two shared borrows of the same scoped cell must be compatible: a
    /// `:get()` re-entering Lua to call `:get()` again should succeed.
    #[test]
    fn scope_userdata_reentrant_shared_borrows_are_compatible() {
        let lua = Lua::new();
        let mut counter = ScopedCounter {
            value: 17,
            calls: Cell::new(0),
        };

        let observed: i64 = lua
            .scope(|scope| {
                let ud = scope.create_userdata_ref_mut(&lua, &mut counter)?;
                lua.globals().set("c", &ud)?;
                lua.load("return c:call_get_via_global()").eval::<i64>()
            })
            .expect("nested shared borrows should succeed");
        assert_eq!(observed, 17);
        assert_eq!(counter.calls.get(), 1);
    }

    /// Field methods route through `create_scoped_userdata_method`/`_mut` via
    /// the registry's `RegistryMode::Scoped` branch. Verifies that path is
    /// wired correctly for both get and set.
    #[test]
    fn scope_userdata_field_methods_get_and_set() {
        let lua = Lua::new();
        let mut bag = ScopedFielded { n: 3 };

        let read_back: i64 = lua
            .scope(|scope| {
                let ud = scope.create_userdata_ref_mut(&lua, &mut bag)?;
                lua.globals().set("f", &ud)?;
                lua.load("f.n = f.n + 39; return f.n").eval::<i64>()
            })
            .expect("field methods should dispatch");
        assert_eq!(read_back, 42);
        assert_eq!(bag.n, 42);
    }

    /// Meta-methods (`__index`/`__newindex` written by hand on a type) must
    /// also route through the scoped path.
    #[test]
    fn scope_userdata_meta_methods_dispatch() {
        let lua = Lua::new();
        let mut bag = ScopedBag { value: 100 };

        let read: i64 = lua
            .scope(|scope| {
                let ud = scope.create_userdata_ref_mut(&lua, &mut bag)?;
                lua.globals().set("b", &ud)?;
                lua.load("b.value = 200; return b.value").eval::<i64>()
            })
            .expect("scoped meta-methods should dispatch");
        assert_eq!(read, 200);
        assert_eq!(bag.value, 200);
    }

    /// Multiple scoped userdatas of the *same* type in one scope are
    /// independent: each call routes to the correct cell.
    #[test]
    fn scope_userdata_multiple_borrows_same_type_in_one_scope() {
        let lua = Lua::new();
        let mut a = ScopedCounter {
            value: 1,
            calls: Cell::new(0),
        };
        let mut b = ScopedCounter {
            value: 100,
            calls: Cell::new(0),
        };

        lua.scope(|scope| {
            let ua = scope.create_userdata_ref_mut(&lua, &mut a)?;
            let ub = scope.create_userdata_ref_mut(&lua, &mut b)?;
            lua.globals().set("a", &ua)?;
            lua.globals().set("b", &ub)?;
            lua.load("a:inc(10); b:inc(1)").exec()
        })
        .expect("scope body should succeed");
        assert_eq!(a.value, 11);
        assert_eq!(b.value, 101);
    }

    /// Different types in one scope share the scope's invalidation but live
    /// in independent metatables; both must work.
    #[test]
    fn scope_userdata_different_types_coexist_in_one_scope() {
        let lua = Lua::new();
        let mut counter = ScopedCounter {
            value: 0,
            calls: Cell::new(0),
        };
        let mut bag = ScopedBag { value: 0 };

        lua.scope(|scope| {
            let uc = scope.create_userdata_ref_mut(&lua, &mut counter)?;
            let ub = scope.create_userdata_ref_mut(&lua, &mut bag)?;
            lua.globals().set("c", &uc)?;
            lua.globals().set("b", &ub)?;
            lua.load("c:inc(7); b.value = 13").exec()
        })
        .expect("scope body should succeed");
        assert_eq!(counter.value, 7);
        assert_eq!(bag.value, 13);
    }

    /// `Lua::scope` threads its closure's return value out — used for
    /// extracting Lua results without leaking them through globals.
    #[test]
    fn scope_userdata_scope_returns_closure_value() {
        let lua = Lua::new();
        let mut counter = ScopedCounter {
            value: 4,
            calls: Cell::new(0),
        };

        let doubled: i64 = lua
            .scope(|scope| {
                let ud = scope.create_userdata_ref_mut(&lua, &mut counter)?;
                lua.globals().set("c", &ud)?;
                lua.load("return c:inc(c:get())").eval::<i64>()
            })
            .expect("scope body should succeed");
        assert_eq!(doubled, 8);
        assert_eq!(counter.value, 8);
    }

    /// A scoped userdata invalidated by its scope still keeps the
    /// `host_value` Rc alive on the userdata; calling it from a *different*
    /// `Lua` instance (which doesn't own this cell) is independently rejected
    /// by `scoped_userdata_cell`'s state check. We cannot fully test the
    /// cross-state case because `globals().set` requires the same Lua, but we
    /// can verify the cached scoped metatable is per-state: building a fresh
    /// `Lua` doesn't see the prior state's metatable cache.
    #[test]
    fn scope_userdata_metatable_cache_is_per_state() {
        let lua_a = Lua::new();
        let lua_b = Lua::new();
        let mut a = ScopedCounter {
            value: 1,
            calls: Cell::new(0),
        };
        let mut b = ScopedCounter {
            value: 2,
            calls: Cell::new(0),
        };

        lua_a
            .scope(|scope| {
                let _ud = scope.create_userdata_ref_mut(&lua_a, &mut a)?;
                Ok(())
            })
            .expect("scope on A should succeed");
        lua_b
            .scope(|scope| {
                let _ud = scope.create_userdata_ref_mut(&lua_b, &mut b)?;
                Ok(())
            })
            .expect("scope on B should succeed");

        let cache_a_len = lua_a.inner.userdata_scoped_metatables.borrow().len();
        let cache_b_len = lua_b.inner.userdata_scoped_metatables.borrow().len();
        assert_eq!(cache_a_len, 1);
        assert_eq!(cache_b_len, 1);
    }

    /// The scoped-metatable cache must not be repopulated on every scope:
    /// a second scope of the same type re-uses the metatable built by the
    /// first. Confirms the `match cached { Some(mt) => mt, None => ... }`
    /// branch in `create_scoped_userdata`.
    #[test]
    fn scope_userdata_metatable_is_built_once_per_type() {
        let lua = Lua::new();
        let mut a = ScopedCounter {
            value: 0,
            calls: Cell::new(0),
        };
        let mut b = ScopedCounter {
            value: 0,
            calls: Cell::new(0),
        };

        lua.scope(|scope| {
            let _ud = scope.create_userdata_ref_mut(&lua, &mut a)?;
            Ok(())
        })
        .expect("first scope should succeed");
        let after_first = lua.inner.userdata_scoped_metatables.borrow().len();

        lua.scope(|scope| {
            let _ud = scope.create_userdata_ref_mut(&lua, &mut b)?;
            Ok(())
        })
        .expect("second scope should succeed");
        let after_second = lua.inner.userdata_scoped_metatables.borrow().len();

        assert_eq!(after_first, 1);
        assert_eq!(after_second, 1);
    }

    /// Rust-side shared borrow of a scoped userdata works inside the scope.
    #[test]
    fn scope_userdata_rust_side_scoped_borrow_inside_scope() {
        let lua = Lua::new();
        let mut counter = ScopedCounter {
            value: 21,
            calls: Cell::new(0),
        };

        let observed = lua
            .scope(|scope| {
                let ud = scope.create_userdata_ref_mut(&lua, &mut counter)?;
                ud.scoped_borrow::<ScopedCounter, _>(|c| c.value)
            })
            .expect("scoped_borrow should succeed inside scope");
        assert_eq!(observed, 21);
    }

    /// Rust-side mut borrow of a scoped userdata mutates the source.
    #[test]
    fn scope_userdata_rust_side_scoped_borrow_mut_inside_scope() {
        let lua = Lua::new();
        let mut counter = ScopedCounter {
            value: 0,
            calls: Cell::new(0),
        };

        lua.scope(|scope| {
            let ud = scope.create_userdata_ref_mut(&lua, &mut counter)?;
            ud.scoped_borrow_mut::<ScopedCounter, _>(|c| c.value = 5)
        })
        .expect("scoped_borrow_mut should succeed");
        assert_eq!(counter.value, 5);
    }

    /// The headline FFI-side guarantee: an `AnyUserData` smuggled out of its
    /// scope cannot hand out a `&T` from Rust either. Cell invalidation drives
    /// both sides; this test pins it down on the Rust side.
    #[test]
    fn scope_userdata_rust_side_borrow_after_scope_errors() {
        let lua = Lua::new();
        let mut counter = ScopedCounter {
            value: 7,
            calls: Cell::new(0),
        };

        let leaked: AnyUserData = lua
            .scope(|scope| scope.create_userdata_ref_mut(&lua, &mut counter))
            .expect("scope body should succeed");

        let err = leaked
            .scoped_borrow::<ScopedCounter, _>(|c| c.value)
            .expect_err("post-scope Rust borrow must fail");
        let msg = runtime_error_message(&err);
        assert!(
            msg.contains("no longer valid") || msg.contains("scope has ended"),
            "expected invalidation error, got: {msg}"
        );

        let err = leaked
            .scoped_borrow_mut::<ScopedCounter, _>(|c| c.value = 99)
            .expect_err("post-scope Rust mut-borrow must fail");
        let msg = runtime_error_message(&err);
        assert!(
            msg.contains("no longer valid") || msg.contains("scope has ended"),
            "expected invalidation error, got: {msg}"
        );

        assert_eq!(counter.value, 7, "the borrow must not have been touched");
    }

    /// The owned `AnyUserData::borrow`/`with_borrow` path is for
    /// `Lua::create_userdata` (Rc<UserDataCell<T>> host); calling it against a
    /// scoped userdata downcasts cleanly to None and errors. This is a safety
    /// claim worth pinning explicitly: the owned path cannot accidentally
    /// reach into a scoped cell.
    #[test]
    fn scope_userdata_owned_borrow_path_rejects_scoped_cells() {
        let lua = Lua::new();
        let mut counter = ScopedCounter {
            value: 1,
            calls: Cell::new(0),
        };

        let err = lua
            .scope(|scope| {
                let ud = scope.create_userdata_ref_mut(&lua, &mut counter)?;
                Ok(ud.with_borrow::<ScopedCounter, _>(|c| c.value))
            })
            .expect("scope body should succeed")
            .expect_err("owned borrow path must not reach a scoped cell");
        let msg = runtime_error_message(&err);
        assert!(
            msg.contains("type mismatch"),
            "expected type-mismatch error, got: {msg}"
        );
    }

    /// And the reverse: an owned (`Lua::create_userdata`) AnyUserData rejects
    /// `scoped_borrow`. Confirms the two paths are isolated.
    #[test]
    fn scope_userdata_scoped_borrow_rejects_owned_cells() {
        let lua = Lua::new();
        let ud = lua
            .create_userdata(ScopedCounter {
                value: 5,
                calls: Cell::new(0),
            })
            .expect("owned userdata should create");

        let err = ud
            .scoped_borrow::<ScopedCounter, _>(|c| c.value)
            .expect_err("scoped borrow must not reach an owned cell");
        let msg = runtime_error_message(&err);
        assert!(
            msg.contains("type mismatch"),
            "expected type-mismatch error, got: {msg}"
        );
    }

    /// `scope.create_function` accepts a closure that captures by reference
    /// from the surrounding stack frame; calling it from Lua sees the live
    /// borrow. Mirrors the userdata-side basic test, but for closures.
    #[test]
    fn scope_function_captures_borrow_and_is_callable_from_lua() {
        let lua = Lua::new();
        let mut acc: i64 = 0;

        let total: i64 = lua
            .scope(|scope| {
                let f = scope.create_function_mut(&lua, |_lua, n: i64| {
                    acc += n;
                    Ok(acc)
                })?;
                lua.globals().set("add", &f)?;
                lua.load("add(2); add(3); return add(5)").eval::<i64>()
            })
            .expect("scoped function should dispatch");
        assert_eq!(total, 10);
        assert_eq!(acc, 10);
    }

    /// The closure body sees borrowed state across multiple invocations
    /// inside one scope — verifies the closure isn't being re-built per call.
    #[test]
    fn scope_function_calls_share_one_closure() {
        let lua = Lua::new();
        let counts = Cell::new(0u32);

        lua.scope(|scope| {
            let f = scope.create_function(&lua, |_lua, ()| {
                counts.set(counts.get() + 1);
                Ok(())
            })?;
            lua.globals().set("tick", &f)?;
            lua.load("for _ = 1, 4 do tick() end").exec()
        })
        .expect("scope should succeed");
        assert_eq!(counts.get(), 4);
    }

    /// Headline safety property for functions: a `Function` smuggled past its
    /// scope must error cleanly when called, not reach into the dropped
    /// closure.
    #[test]
    fn scope_function_invalidated_after_scope_returns_runtime_error() {
        let lua = Lua::new();
        let mut acc: i64 = 0;

        lua.scope(|scope| {
            let f = scope.create_function_mut(&lua, |_lua, n: i64| {
                acc += n;
                Ok(acc)
            })?;
            lua.globals().set("add", &f)?;
            lua.load("add(1)").exec()
        })
        .expect("scope body should succeed");
        assert_eq!(acc, 1);

        let err = lua
            .load("return add(100)")
            .eval::<i64>()
            .expect_err("post-scope call must fail");
        let msg = runtime_error_message(&err);
        assert!(
            msg.contains("no longer valid") || msg.contains("scope has ended"),
            "expected invalidation error, got: {msg}"
        );
        assert_eq!(acc, 1, "the closure's borrow must not have been touched");
    }

    /// FnMut re-entry: if the closure calls back into Lua which calls itself,
    /// the inner `try_borrow_mut` on the closure's `RefCell` must reject the
    /// nested call rather than producing aliasing `&mut` captures.
    #[test]
    fn scope_function_reentrant_fnmut_is_rejected() {
        let lua = Lua::new();
        let mut count: i64 = 0;

        let err = lua
            .scope(|scope| {
                let f = scope.create_function_mut(&lua, |lua, ()| {
                    count += 1;
                    if count < 2 {
                        lua.load("recurse()").exec()?;
                    }
                    Ok(())
                })?;
                lua.globals().set("recurse", &f)?;
                lua.load("recurse()").exec()
            })
            .expect_err("re-entrant FnMut must error");
        let msg = runtime_error_message(&err);
        assert!(
            msg.contains("already borrowed"),
            "expected FnMut-conflict error, got: {msg}"
        );
    }

    /// Pairing test: a scoped userdata and a scoped function in the same
    /// scope can both borrow from the same stack frame (different parts of
    /// it). Models the Bevy use case: `&mut World` userdata plus a few
    /// closures that look at adjacent locals.
    #[test]
    fn scope_function_and_userdata_in_same_scope() {
        let lua = Lua::new();
        let mut bag = ScopedFielded { n: 0 };
        let log = Cell::new(0i64);

        lua.scope(|scope| {
            let ud = scope.create_userdata_ref_mut(&lua, &mut bag)?;
            let logger = scope.create_function(&lua, |_lua, n: i64| {
                log.set(log.get() + n);
                Ok(())
            })?;
            lua.globals().set("b", &ud)?;
            lua.globals().set("log", &logger)?;
            lua.load("b.n = 42; log(b.n); log(b.n + 1)").exec()
        })
        .expect("mixed scope body should succeed");
        assert_eq!(bag.n, 42);
        assert_eq!(log.get(), 85);
    }

    /// Even if the scope body errors before returning, the scoped function is
    /// still invalidated so a follow-up Lua call cannot resurrect the dead
    /// closure.
    #[test]
    fn scope_function_invalidated_even_when_body_errors() {
        let lua = Lua::new();
        let value = Cell::new(5i64);

        let _err = lua
            .scope(|scope| -> Result<()> {
                let f = scope.create_function(&lua, |_lua, ()| Ok(value.get()))?;
                lua.globals().set("get", &f)?;
                Err(LuaError::runtime(format_args!("aborting")))
            })
            .expect_err("scope body should propagate error");

        let err = lua
            .load("return get()")
            .eval::<i64>()
            .expect_err("function must be invalidated after error-exit scope");
        let msg = runtime_error_message(&err);
        assert!(
            msg.contains("no longer valid") || msg.contains("scope has ended"),
            "expected invalidation error, got: {msg}"
        );
    }

    /// Many functions in one scope, all calling into shared borrowed state.
    /// Stresses the invalidator list ordering: every closure must remain
    /// callable until the scope ends, and all are invalidated together.
    #[test]
    fn scope_function_many_closures_in_one_scope() {
        let lua = Lua::new();
        let total = Cell::new(0i64);
        let total_ref = &total;

        lua.scope(|scope| {
            for i in 1..=8 {
                let f = scope.create_function(&lua, move |_lua, ()| {
                    total_ref.set(total_ref.get() + i);
                    Ok(())
                })?;
                lua.globals().set(format!("f{}", i).as_str(), &f)?;
            }
            lua.load("f1(); f2(); f3(); f4(); f5(); f6(); f7(); f8()").exec()
        })
        .expect("scope with many closures should succeed");
        assert_eq!(total.get(), 36);
    }

    /// If the closure body returns an error, the scope still drops and
    /// invalidates everything it created. We confirm by then using the
    /// leaked global from a follow-up call — it must report invalidated, not
    /// stale-but-alive.
    #[test]
    fn scope_userdata_invalidated_even_when_body_errors() {
        let lua = Lua::new();
        let mut counter = ScopedCounter {
            value: 1,
            calls: Cell::new(0),
        };

        let err = lua
            .scope(|scope| -> Result<()> {
                let ud = scope.create_userdata_ref_mut(&lua, &mut counter)?;
                lua.globals().set("c", &ud)?;
                Err(LuaError::runtime(format_args!("aborting scope")))
            })
            .expect_err("scope body should propagate error");
        let _ = err;

        let leaked_err = lua
            .load("return c:get()")
            .eval::<i64>()
            .expect_err("leaked userdata must still be invalidated");
        let msg = runtime_error_message(&leaked_err);
        assert!(
            msg.contains("no longer valid") || msg.contains("scope has ended"),
            "expected invalidation error after scope-with-error, got: {msg}"
        );
    }

    /// Cloning an `AnyUserData` produces two handles to the same scope cell.
    /// Invalidation runs against the cell, so a clone that escapes via a
    /// global must fail at the same point a direct handle would. Pins the
    /// "every reference to the same cell sees invalidation together"
    /// invariant.
    #[test]
    fn scope_userdata_cloned_handles_invalidate_together() {
        let lua = Lua::new();
        let mut counter = ScopedCounter {
            value: 9,
            calls: Cell::new(0),
        };

        lua.scope(|scope| {
            let ud = scope.create_userdata_ref_mut(&lua, &mut counter)?;
            let clone = ud.clone();
            lua.globals().set("a", &ud)?;
            lua.globals().set("b", &clone)?;
            lua.load("assert(a:get() == 9); assert(b:get() == 9)").exec()
        })
        .expect("scope body should succeed");

        let err_a = lua
            .load("return a:get()")
            .eval::<i64>()
            .expect_err("original handle must error post-scope");
        let err_b = lua
            .load("return b:get()")
            .eval::<i64>()
            .expect_err("cloned handle must error post-scope");
        assert!(runtime_error_message(&err_a).contains("no longer valid"));
        assert!(runtime_error_message(&err_b).contains("no longer valid"));
    }

    /// Nested `Lua::scope` calls: cells created in the inner scope invalidate
    /// when the inner returns; cells in the outer remain live until the outer
    /// returns. Pins that scope cells don't leak across siblings/parents.
    #[test]
    fn scope_userdata_nested_scopes_isolated() {
        let lua = Lua::new();
        let mut outer_counter = ScopedCounter {
            value: 1,
            calls: Cell::new(0),
        };
        let mut inner_counter = ScopedCounter {
            value: 100,
            calls: Cell::new(0),
        };

        lua.scope(|outer| {
            let o = outer.create_userdata_ref_mut(&lua, &mut outer_counter)?;
            lua.globals().set("outer", &o)?;

            lua.scope(|inner| {
                let i = inner.create_userdata_ref_mut(&lua, &mut inner_counter)?;
                lua.globals().set("inner", &i)?;
                lua.load("assert(outer:get() == 1); assert(inner:get() == 100)").exec()
            })?;

            // Inner ended. `inner` global is dead, but `outer` is still live.
            let inner_err = lua
                .load("return inner:get()")
                .eval::<i64>()
                .expect_err("inner userdata must be dead after inner scope");
            assert!(runtime_error_message(&inner_err).contains("no longer valid"));

            let outer_alive: i64 = lua
                .load("return outer:get()")
                .eval()
                .expect("outer userdata must still be alive in outer scope");
            assert_eq!(outer_alive, 1);
            Ok(())
        })
        .expect("scope body should succeed");

        // Outer ended; both should now be dead.
        let err = lua
            .load("return outer:get()")
            .eval::<i64>()
            .expect_err("outer userdata must be dead after outer scope");
        assert!(runtime_error_message(&err).contains("no longer valid"));
    }

    // -- Direct exercises of the unsafe machinery, no Lua state --
    //
    // These tests bypass the full `Lua::scope` plumbing and poke `ScopedCell`
    // / `ScopedFnCell` directly. They exist so `cargo miri test scope_cell_`
    // can validate the scope unsafe surface in isolation. The full suite
    // still routes through the rest of the runtime, which currently has
    // pre-existing aliasing violations under Miri (lua-gc raw-pointer
    // patterns, unrelated to scope); these direct tests are the
    // miri-runnable subset.

    #[test]
    fn scope_cell_shared_then_shared_succeeds() {
        let mut data = 17_i32;
        let cell = ScopedCell::<i32>::new(&mut data);

        let a = cell.try_borrow().expect("first shared borrow");
        let b = cell.try_borrow().expect("second shared borrow");
        assert_eq!(*a, 17);
        assert_eq!(*b, 17);
        drop(a);
        drop(b);

        cell.invalidate();
        assert!(cell.try_borrow().is_err(), "post-invalidate must fail");
    }

    #[test]
    fn scope_cell_mut_then_shared_fails() {
        let mut data = 5_i32;
        let cell = ScopedCell::<i32>::new(&mut data);

        let mut m = cell.try_borrow_mut().expect("first mut borrow");
        *m = 42;
        let s = cell.try_borrow();
        assert!(s.is_err(), "shared borrow while mut-held must fail");
        drop(m);

        let s = cell.try_borrow().expect("shared borrow after mut release");
        assert_eq!(*s, 42);
    }

    #[test]
    fn scope_cell_shared_then_mut_fails() {
        let mut data = 99_i32;
        let cell = ScopedCell::<i32>::new(&mut data);

        let s = cell.try_borrow().expect("first shared borrow");
        let m = cell.try_borrow_mut();
        assert!(m.is_err(), "mut borrow while shared-held must fail");
        drop(s);

        let mut m = cell.try_borrow_mut().expect("mut borrow after shared release");
        *m = 100;
        drop(m);
        assert_eq!(data, 100);
    }

    #[test]
    fn scope_cell_invalidate_after_drop_of_guards_is_clean() {
        let mut data = String::from("hi");
        let cell = ScopedCell::<String>::new(&mut data);
        {
            let guard = cell.try_borrow().expect("borrow");
            assert_eq!(&*guard, "hi");
        }
        cell.invalidate();
        assert!(cell.try_borrow().is_err());
        assert!(cell.try_borrow_mut().is_err());
    }

    #[test]
    fn scope_cell_drop_guard_decrements_borrow_count() {
        let mut data = 0_i32;
        let cell = ScopedCell::<i32>::new(&mut data);
        {
            let _a = cell.try_borrow().expect("a");
            let _b = cell.try_borrow().expect("b");
            assert!(cell.try_borrow_mut().is_err());
        }
        cell.try_borrow_mut().expect("mut borrow once guards drop");
    }

    #[test]
    fn scope_fn_cell_dispatches_and_invalidates() {
        let counter = Cell::new(0i64);
        let adapter: Box<dyn Fn(&Lua, Vec<Value>) -> Result<Vec<Value>>> =
            Box::new(|_lua, _args| Ok(Vec::new()));
        let cell = Rc::new(ScopedFnCell {
            boxed: RefCell::new(Some(adapter)),
        });

        let lua = Lua::new();
        cell.try_call(&lua, Vec::new()).expect("pre-invalidate call");
        counter.set(counter.get() + 1);

        cell.invalidate();

        let err = cell
            .try_call(&lua, Vec::new())
            .expect_err("post-invalidate call must fail");
        let msg = runtime_error_message(&err);
        assert!(msg.contains("no longer valid"), "got: {msg}");
        assert_eq!(counter.get(), 1);
    }
}
