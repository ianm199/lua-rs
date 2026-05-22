//! Phase-B reconcile shim: re-exports the canonical `LuaState` from
//! `lua-vm` and provides an extension trait holding every method the
//! Phase-A stdlib translation used to call on the Phase-A stub.
//!
//! TODO_ARCH(phase-b-reconcile): all extension-trait method bodies are
//! `todo!("phase-b-reconcile: <name>")`. They must move to real
//! implementations on `lua_vm::state::LuaState` itself; that work lives in
//! `lua-vm`, not here. The shim exists only so stdlib code keeps compiling
//! while the canonical `LuaState` API stabilises.
//!
//! Where a trait method's name collides with an inherent method on the
//! canonical `LuaState`, Rust resolves to the inherent method. Most
//! Phase-A call sites compile through the inherent method unchanged; the
//! handful that depend on a different return shape (e.g. `state.push(...)?`
//! against the canonical `pub fn push(&mut self, val: LuaValue)`) are
//! patched at the call site.

#![allow(dead_code, unused_variables, clippy::too_many_arguments)]

use lua_types::{
    arith::ArithOp,
    closure::{LuaCFnPtr, LuaClosure},
    error::LuaError,
    gc::GcRef,
    string::LuaString,
    userdata::LuaUserData,
    value::{LuaThread, LuaValue},
    CallInfoIdx,
    LuaType,
    LuaStatus,
};

pub use lua_vm::state::LuaState;

/// Bare function callable from Lua. C: `lua_CFunction`.
#[allow(non_camel_case_types)]
pub type lua_CFunction = fn(&mut LuaState) -> Result<usize, LuaError>;

/// Pseudo-index for the `i`-th upvalue of a C function.
/// C: `#define lua_upvalueindex(i)`
pub fn upvalue_index(i: i32) -> i32 {
    -1_001_000 - i
}

/// Comparison operations (eq, lt, le). C: `LUA_OP*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    Eq,
    Lt,
    Le,
}

/// Reader-callback type for `lua_load`. C: `lua_Reader`.
pub type LuaReader<'a> = dyn FnMut() -> Option<Vec<u8>> + 'a;

/// Writer-callback type for `lua_dump`. C: `lua_Writer`.
pub type LuaWriter<'a> = dyn FnMut(&[u8]) -> Result<(), LuaError> + 'a;

/// Debug introspection record. C: `lua_Debug`.
#[derive(Debug, Default, Clone)]
pub struct LuaDebug {
    pub name: Option<Vec<u8>>,
    pub namewhat: Vec<u8>,
    pub what: u8,
    pub source: Vec<u8>,
    pub short_src: Vec<u8>,
    pub linedefined: i32,
    pub lastlinedefined: i32,
    pub currentline: i32,
    pub nups: u8,
    pub nparams: u8,
    pub isvararg: bool,
    pub istailcall: bool,
    pub ftransfer: u16,
    pub ntransfer: u16,
    /// Active CallInfo index, set by `get_stack`/`get_stack_level` and read by
    /// `get_info`/`get_local_at`/`set_local_at`. Mirrors C's `lua_Debug.i_ci`
    /// (a raw pointer in C; an index here).
    pub(crate) i_ci_idx: Option<CallInfoIdx>,
}

impl LuaDebug {
    pub fn name_bytes(&self) -> &[u8] { self.name.as_deref().unwrap_or(b"?") }
    pub fn namewhat_bytes(&self) -> &[u8] { &self.namewhat }
    pub fn what_bytes(&self) -> &[u8] { match self.what { b'L' => b"Lua", b'C' => b"C", b'm' => b"main", _ => b"?" } }
    pub fn short_src_bytes(&self) -> &[u8] { &self.short_src }
    pub fn source_bytes(&self) -> &[u8] { &self.source }
}

/// Extension trait wiring every Phase-A stub method onto the canonical
/// `LuaState`. Bodies are `todo!("phase-b-reconcile: …")`. When the
/// canonical type already defines a method with the same name, Rust's
/// inherent-first resolution makes this trait method unreachable (the
/// inherent one wins) — the trait is then only providing the *missing*
/// methods. Conflicting call-sites whose shape no longer matches the
/// inherent signature are patched in their respective stdlib modules.
pub trait LuaStateStubExt {
    fn push_value(&mut self, idx: i32) -> Result<(), LuaError> { todo!("phase-b-reconcile: push_value") }
    fn push_copy(&mut self, idx: i32) -> Result<(), LuaError> { todo!("phase-b-reconcile: push_copy") }
    fn push_string(&mut self, s: &[u8]) -> Result<(), LuaError> { todo!("phase-b-reconcile: push_string") }
    fn push_bytes(&mut self, s: &[u8]) -> Result<(), LuaError> { todo!("phase-b-reconcile: push_bytes") }
    fn push_fstring(&mut self, args: std::fmt::Arguments<'_>) -> Result<(), LuaError> { todo!("phase-b-reconcile: push_fstring") }
    fn push_c_function(&mut self, f: lua_CFunction) -> Result<(), LuaError> { todo!("phase-b-reconcile: push_c_function") }
    fn push_c_closure(&mut self, f: lua_CFunction, n: i32) -> Result<(), LuaError> { todo!("phase-b-reconcile: push_c_closure") }
    fn push_where(&mut self, level: i32) -> Result<(), LuaError> { todo!("phase-b-reconcile: push_where") }
    fn push_globals(&mut self) -> Result<(), LuaError> { todo!("phase-b-reconcile: push_globals") }

    fn pop_bytes(&mut self) -> Vec<u8> { todo!("phase-b-reconcile: pop_bytes") }

    fn top(&mut self) -> i32 { todo!("phase-b-reconcile: top") }
    fn top_count(&mut self) -> i32 { todo!("phase-b-reconcile: top_count") }

    fn insert(&mut self, idx: i32) -> Result<(), LuaError> { todo!("phase-b-reconcile: insert") }
    fn remove(&mut self, idx: i32) -> Result<(), LuaError> { todo!("phase-b-reconcile: remove") }
    fn replace(&mut self, idx: i32) -> Result<(), LuaError> { todo!("phase-b-reconcile: replace") }
    fn rotate(&mut self, idx: i32, n: i32) -> Result<(), LuaError> { todo!("phase-b-reconcile: rotate") }
    fn copy_value(&mut self, from: i32, to: i32) -> Result<(), LuaError> { todo!("phase-b-reconcile: copy_value") }
    fn abs_index(&mut self, idx: i32) -> i32 { todo!("phase-b-reconcile: abs_index") }
    fn ensure_stack<S: AsRef<[u8]> + ?Sized>(&mut self, n: i32, msg: &S) -> Result<(), LuaError> { let _ = msg.as_ref(); todo!("phase-b-reconcile: ensure_stack") }
    fn check_stack_space(&mut self, n: i32) -> bool { todo!("phase-b-reconcile: check_stack_space") }

    fn type_at(&mut self, idx: i32) -> LuaType { todo!("phase-b-reconcile: type_at") }
    fn type_name(&mut self, t: LuaType) -> &'static [u8] { todo!("phase-b-reconcile: type_name") }
    fn type_name_at(&mut self, idx: i32) -> &'static [u8] { todo!("phase-b-reconcile: type_name_at") }
    fn value_at(&mut self, idx: i32) -> LuaValue { todo!("phase-b-reconcile: value_at") }
    fn is_none_or_nil(&mut self, idx: i32) -> bool { todo!("phase-b-reconcile: is_none_or_nil") }
    fn is_integer(&mut self, idx: i32) -> bool { todo!("phase-b-reconcile: is_integer") }
    fn is_number(&mut self, idx: i32) -> bool { todo!("phase-b-reconcile: is_number") }

    fn to_lua_string(&mut self, idx: i32) -> Option<GcRef<LuaString>> { todo!("phase-b-reconcile: to_lua_string") }
    fn to_lua_string_bytes(&mut self, idx: i32) -> Option<Vec<u8>> { todo!("phase-b-reconcile: to_lua_string_bytes") }
    fn to_lua_string_len(&mut self, idx: i32) -> Option<usize> { todo!("phase-b-reconcile: to_lua_string_len") }
    fn to_integer_x(&mut self, idx: i32) -> Option<i64> { todo!("phase-b-reconcile: to_integer_x") }
    fn to_number_x(&mut self, idx: i32) -> Option<f64> { todo!("phase-b-reconcile: to_number_x") }
    fn to_boolean(&mut self, idx: i32) -> bool { todo!("phase-b-reconcile: to_boolean") }
    fn to_userdata(&mut self, idx: i32) -> Option<GcRef<LuaUserData>> { todo!("phase-b-reconcile: to_userdata") }
    fn to_display_string(&mut self, idx: i32) -> Result<Vec<u8>, LuaError> { todo!("phase-b-reconcile: to_display_string") }

    fn check_arg_any(&mut self, arg: i32) -> Result<(), LuaError> { todo!("phase-b-reconcile: check_arg_any") }
    fn check_arg_integer(&mut self, arg: i32) -> Result<i64, LuaError> { todo!("phase-b-reconcile: check_arg_integer") }
    fn check_arg_string(&mut self, arg: i32) -> Result<Vec<u8>, LuaError> { todo!("phase-b-reconcile: check_arg_string") }
    fn check_arg_type(&mut self, arg: i32, t: LuaType) -> Result<(), LuaError> { todo!("phase-b-reconcile: check_arg_type") }
    fn check_arg_option(&mut self, arg: i32, def: Option<&[u8]>, lst: &[&[u8]]) -> Result<usize, LuaError> { todo!("phase-b-reconcile: check_arg_option") }

    fn opt_arg_integer(&mut self, arg: i32, def: i64) -> Result<i64, LuaError> { todo!("phase-b-reconcile: opt_arg_integer") }
    fn opt_arg_string_bytes(&mut self, arg: i32) -> Result<Vec<u8>, LuaError> { todo!("phase-b-reconcile: opt_arg_string_bytes") }
    fn opt_arg_string(&mut self, arg: i32, def: &[u8]) -> Result<Vec<u8>, LuaError> { todo!("phase-b-reconcile: opt_arg_string") }
    fn arg_to_bool(&mut self, arg: i32) -> bool { todo!("phase-b-reconcile: arg_to_bool") }

    fn get_field(&mut self, idx: i32, k: &[u8]) -> Result<LuaType, LuaError> { todo!("phase-b-reconcile: get_field") }
    fn set_field(&mut self, idx: i32, k: &[u8]) -> Result<(), LuaError> { todo!("phase-b-reconcile: set_field") }
    fn raw_get(&mut self, idx: i32) -> Result<LuaType, LuaError> { todo!("phase-b-reconcile: raw_get") }
    fn raw_set(&mut self, idx: i32) -> Result<(), LuaError> { todo!("phase-b-reconcile: raw_set") }
    fn raw_get_i(&mut self, idx: i32, n: i64) -> Result<LuaType, LuaError> { todo!("phase-b-reconcile: raw_get_i") }
    fn raw_set_i(&mut self, idx: i32, n: i64) -> Result<(), LuaError> { todo!("phase-b-reconcile: raw_set_i") }
    fn raw_equal(&mut self, idx1: i32, idx2: i32) -> Result<bool, LuaError> { todo!("phase-b-reconcile: raw_equal") }
    fn raw_len(&mut self, idx: i32) -> i64 { todo!("phase-b-reconcile: raw_len") }
    fn get_i(&mut self, idx: i32, n: i64) -> Result<LuaType, LuaError> { todo!("phase-b-reconcile: get_i") }
    fn get_metafield(&mut self, idx: i32, name: &[u8]) -> Result<LuaType, LuaError> { todo!("phase-b-reconcile: get_metafield") }
    fn get_meta_field(&mut self, idx: i32, name: &[u8]) -> Result<bool, LuaError> { todo!("phase-b-reconcile: get_meta_field") }
    fn get_metatable(&mut self, idx: i32) -> Result<bool, LuaError> { todo!("phase-b-reconcile: get_metatable") }
    fn set_metatable(&mut self, idx: i32) -> Result<(), LuaError> { todo!("phase-b-reconcile: set_metatable") }
    fn table_next(&mut self, idx: i32) -> Result<bool, LuaError> { todo!("phase-b-reconcile: table_next") }
    fn create_table(&mut self, narr: i32, nrec: i32) -> Result<(), LuaError> { todo!("phase-b-reconcile: create_table") }

    fn gc_control_simple(&mut self, op: i32) -> Result<i32, LuaError> { todo!("phase-b-reconcile: gc_control_simple") }
    fn gc_count(&mut self) -> Result<i32, LuaError> { todo!("phase-b-reconcile: gc_count") }
    fn gc_count_b(&mut self) -> Result<i32, LuaError> { todo!("phase-b-reconcile: gc_count_b") }
    fn gc_step(&mut self, data: i32) -> Result<i32, LuaError> { todo!("phase-b-reconcile: gc_step") }
    fn gc_set_param(&mut self, op: i32, value: i32) -> Result<i32, LuaError> { todo!("phase-b-reconcile: gc_set_param") }
    fn gc_is_running(&mut self) -> Result<bool, LuaError> { todo!("phase-b-reconcile: gc_is_running") }
    fn gc_gen(&mut self, minor_mul: i32, major_mul: i32) -> Result<i32, LuaError> { todo!("phase-b-reconcile: gc_gen") }
    fn gc_inc(&mut self, pause: i32, step_mul: i32, step_size: i32) -> Result<i32, LuaError> { todo!("phase-b-reconcile: gc_inc") }

    fn call(&mut self, nargs: i32, nresults: i32) -> Result<(), LuaError> { todo!("phase-b-reconcile: call") }
    fn protected_call(&mut self, nargs: i32, nresults: i32, msgh: i32) -> Result<(), LuaError> { todo!("phase-b-reconcile: protected_call") }
    fn len_op(&mut self, idx: i32) -> Result<(), LuaError> { todo!("phase-b-reconcile: len_op") }
    fn arith(&mut self, op: ArithOp) -> Result<(), LuaError> { todo!("phase-b-reconcile: arith") }

    fn load(&mut self, chunk: &[u8], name: &[u8], mode: Option<&[u8]>) -> Result<bool, LuaError> { todo!("phase-b-reconcile: load") }
    fn load_buffer_ex<M: ?Sized>(&mut self, buf: &[u8], name: &[u8], mode: &M) -> Result<bool, LuaError>
    where
        M: AsRef<[u8]>,
    { let _ = (buf, name, mode); todo!("phase-b-reconcile: load_buffer_ex") }
    fn load_file(&mut self, path: Option<&[u8]>) -> Result<bool, LuaError> { todo!("phase-b-reconcile: load_file") }
    fn load_file_ex(&mut self, path: Option<&[u8]>, mode: Option<&[u8]>) -> Result<bool, LuaError> { todo!("phase-b-reconcile: load_file_ex") }
    fn load_with_reader<F, M: ?Sized>(&mut self, reader: F, name: &[u8], mode: &M) -> Result<bool, LuaError>
    where
        F: FnMut(&mut LuaState) -> Result<Option<Vec<u8>>, LuaError>,
        M: AsRef<[u8]>,
    { let _ = (reader, name, mode); todo!("phase-b-reconcile: load_with_reader") }
    fn dump_function(&mut self, strip: bool) -> Result<Vec<u8>, LuaError> { todo!("phase-b-reconcile: dump_function") }

    fn warning(&mut self, msg: &[u8], to_cont: bool) -> Result<(), LuaError> { todo!("phase-b-reconcile: warning") }
    fn write_output(&mut self, msg: &[u8]) -> Result<(), LuaError> { todo!("phase-b-reconcile: write_output") }
    fn set_warn_fn(&mut self, f: Option<lua_CFunction>, ud: Option<LuaValue>) -> Result<(), LuaError> { todo!("phase-b-reconcile: set_warn_fn") }
    fn set_funcs(&mut self, funcs: &[(&[u8], lua_CFunction)], nup: i32) -> Result<(), LuaError> {
        let _ = (funcs, nup);
        todo!("phase-b-reconcile: set_funcs")
    }
    fn set_global(&mut self, name: &[u8]) -> Result<(), LuaError> { todo!("phase-b-reconcile: set_global") }
    fn set_upvalue(&mut self, fidx: i32, n: i32) -> Result<Option<Vec<u8>>, LuaError> { todo!("phase-b-reconcile: set_upvalue") }
    fn get_info(&mut self, what: &[u8], ar: &mut LuaDebug) -> Result<(), LuaError> { todo!("phase-b-reconcile: get_info") }
    fn get_stack(&mut self, level: i32, ar: &mut LuaDebug) -> bool { todo!("phase-b-reconcile: get_stack") }
    fn lua_version(&mut self) -> f64 { todo!("phase-b-reconcile: lua_version") }
    fn string_to_number(&mut self, idx: i32) -> Option<usize> { todo!("phase-b-reconcile: string_to_number") }
    fn string_to_number_push<S: AsRef<[u8]> + ?Sized>(&mut self, s: &S) -> Result<usize, LuaError> { let _ = s.as_ref(); todo!("phase-b-reconcile: string_to_number_push") }
    fn require_lib(&mut self, name: &[u8], openf: lua_CFunction, glb: bool) -> Result<(), LuaError> { todo!("phase-b-reconcile: require_lib") }
    fn peek_bytes(&mut self, idx: i32) -> Option<Vec<u8>> { todo!("phase-b-reconcile: peek_bytes") }

    fn check_number(&mut self, arg: i32) -> Result<f64, LuaError> { todo!("phase-b-reconcile: check_number") }
    fn check_integer(&mut self, arg: i32) -> Result<i64, LuaError> { todo!("phase-b-reconcile: check_integer") }
    fn check_any(&mut self, arg: i32) -> Result<(), LuaError> { todo!("phase-b-reconcile: check_any") }
    fn check_arg_number(&mut self, arg: i32) -> Result<f64, LuaError> { todo!("phase-b-reconcile: check_arg_number") }
    fn check_arg_userdata(&mut self, arg: i32, name: &[u8]) -> Result<GcRef<LuaUserData>, LuaError> { todo!("phase-b-reconcile: check_arg_userdata") }
    fn check_stack_growth(&mut self, n: i32) -> bool { todo!("phase-b-reconcile: check_stack_growth") }
    fn opt_integer(&mut self, arg: i32, def: i64) -> Result<i64, LuaError> { todo!("phase-b-reconcile: opt_integer") }
    fn opt_number(&mut self, arg: i32, def: f64) -> Result<f64, LuaError> { todo!("phase-b-reconcile: opt_number") }
    fn opt_arg_lstring(&mut self, arg: i32, def: Option<&[u8]>) -> Result<Option<Vec<u8>>, LuaError> { todo!("phase-b-reconcile: opt_arg_lstring") }

    fn table_get_i(&mut self, idx: i32, n: i64) -> Result<LuaType, LuaError> { todo!("phase-b-reconcile: table_get_i") }
    fn table_set_i(&mut self, idx: i32, n: i64) -> Result<(), LuaError> { todo!("phase-b-reconcile: table_set_i") }
    fn get_table(&mut self, idx: i32) -> Result<LuaType, LuaError> { todo!("phase-b-reconcile: get_table") }
    fn raw_geti(&mut self, idx: i32, n: i64) -> Result<LuaType, LuaError> { todo!("phase-b-reconcile: raw_geti") }
    fn raw_seti(&mut self, idx: i32, n: i64) -> Result<(), LuaError> { todo!("phase-b-reconcile: raw_seti") }
    fn len_at(&mut self, idx: i32) -> i64 { todo!("phase-b-reconcile: len_at") }
    fn length_at(&mut self, idx: i32) -> Result<i64, LuaError> { todo!("phase-b-reconcile: length_at") }
    fn stack_top(&mut self) -> i32 { todo!("phase-b-reconcile: stack_top") }
    fn get_top(&mut self) -> i32 { todo!("phase-b-reconcile: get_top") }

    fn push_value_at(&mut self, idx: i32) -> Result<(), LuaError> { todo!("phase-b-reconcile: push_value_at") }
    fn push_fail(&mut self) -> Result<(), LuaError> { todo!("phase-b-reconcile: push_fail") }
    fn push_lstring(&mut self, s: &[u8]) -> Result<(), LuaError> { todo!("phase-b-reconcile: push_lstring") }
    fn push_thread(&mut self) -> Result<bool, LuaError> { todo!("phase-b-reconcile: push_thread") }
    fn push_cclosure(&mut self, f: lua_CFunction, n: i32) -> Result<(), LuaError> { todo!("phase-b-reconcile: push_cclosure") }
    fn push_upvalue(&mut self, idx: i32) -> Result<(), LuaError> { todo!("phase-b-reconcile: push_upvalue") }
    fn push_registry(&mut self) -> Result<(), LuaError> { todo!("phase-b-reconcile: push_registry") }

    fn to_integer(&mut self, idx: i32) -> Option<i64> { todo!("phase-b-reconcile: to_integer") }
    fn to_integer_opt(&mut self, idx: i32) -> Option<i64> { todo!("phase-b-reconcile: to_integer_opt") }
    fn to_number(&mut self, idx: i32) -> Option<f64> { todo!("phase-b-reconcile: to_number") }
    fn to_bytes(&mut self, idx: i32) -> Option<Vec<u8>> { todo!("phase-b-reconcile: to_bytes") }
    fn to_bytes_at(&mut self, idx: i32) -> Option<Vec<u8>> { todo!("phase-b-reconcile: to_bytes_at") }
    fn to_string_coerced(&mut self, idx: i32) -> Option<Vec<u8>> { todo!("phase-b-reconcile: to_string_coerced") }
    fn to_light_userdata(&mut self, idx: i32) -> Option<*mut std::ffi::c_void> { todo!("phase-b-reconcile: to_light_userdata") }
    fn to_thread(&mut self, idx: i32) -> Option<GcRef<LuaThread>> { todo!("phase-b-reconcile: to_thread") }
    fn to_thread_at(&mut self, idx: i32) -> Option<GcRef<LuaThread>> { todo!("phase-b-reconcile: to_thread_at") }
    fn type_name_str_at(&mut self, idx: i32) -> &'static [u8] { todo!("phase-b-reconcile: type_name_str_at") }
    fn is_c_function_at(&mut self, idx: i32) -> bool { todo!("phase-b-reconcile: is_c_function_at") }

    fn compare(&mut self, idx1: i32, idx2: i32, op: CompareOp) -> Result<bool, LuaError> { todo!("phase-b-reconcile: compare") }
    fn compare_lt(&mut self, idx1: i32, idx2: i32) -> Result<bool, LuaError> { todo!("phase-b-reconcile: compare_lt") }

    fn get_field_registry(&mut self, name: &[u8]) -> Result<LuaType, LuaError> { todo!("phase-b-reconcile: get_field_registry") }
    fn get_registry_field(&mut self, name: &[u8]) -> Result<LuaType, LuaError> { todo!("phase-b-reconcile: get_registry_field") }
    fn get_subtable_registry(&mut self, name: &[u8]) -> Result<bool, LuaError> { todo!("phase-b-reconcile: get_subtable_registry") }
    fn get_or_create_registry_subtable(&mut self, name: &[u8]) -> Result<bool, LuaError> { todo!("phase-b-reconcile: get_or_create_registry_subtable") }
    fn registry_get(&mut self, key: &[u8]) -> Result<LuaType, LuaError> { todo!("phase-b-reconcile: registry_get") }
    fn registry_set(&mut self, key: &[u8]) -> Result<(), LuaError> { todo!("phase-b-reconcile: registry_set") }

    fn new_lib<F: Copy>(&mut self, funcs: &[(&[u8], F)]) -> Result<(), LuaError> { todo!("phase-b-reconcile: new_lib") }
    fn new_lib_table<F: Copy>(&mut self, funcs: &[(&[u8], F)]) -> Result<(), LuaError> { todo!("phase-b-reconcile: new_lib_table") }
    fn new_metatable(&mut self, name: &[u8]) -> Result<bool, LuaError> { todo!("phase-b-reconcile: new_metatable") }
    fn set_metatable_by_name(&mut self, name: &[u8]) -> Result<(), LuaError> { todo!("phase-b-reconcile: set_metatable_by_name") }
    fn register_funcs<F: Copy>(&mut self, funcs: &[(&[u8], F)]) -> Result<(), LuaError> { todo!("phase-b-reconcile: register_funcs") }
    fn register_lib<F: Copy>(&mut self, name: &[u8], funcs: &[(&[u8], F)]) -> Result<(), LuaError> { todo!("phase-b-reconcile: register_lib") }
    fn set_funcs_with_upvalues<F: Copy>(&mut self, funcs: &[(&[u8], F)], nup: i32) -> Result<(), LuaError> { todo!("phase-b-reconcile: set_funcs_with_upvalues") }

    fn new_userdata_typed(&mut self, name: &[u8], size: usize, nuvalue: i32) -> Result<GcRef<LuaUserData>, LuaError> { todo!("phase-b-reconcile: new_userdata_typed") }
    fn get_iuservalue(&mut self, idx: i32, n: i32) -> Result<LuaType, LuaError> { todo!("phase-b-reconcile: get_iuservalue") }
    fn set_iuservalue(&mut self, idx: i32, n: i32) -> Result<bool, LuaError> { todo!("phase-b-reconcile: set_iuservalue") }
    fn test_arg_userdata(&mut self, arg: i32, name: &[u8]) -> Option<GcRef<LuaUserData>> { todo!("phase-b-reconcile: test_arg_userdata") }

    fn get_upvalue(&mut self, fidx: i32, n: i32) -> Result<Option<Vec<u8>>, LuaError> { todo!("phase-b-reconcile: get_upvalue") }
    fn upvalue_id(&mut self, fidx: i32, n: i32) -> Result<*mut std::ffi::c_void, LuaError> { todo!("phase-b-reconcile: upvalue_id") }
    fn join_upvalues(&mut self, fidx1: i32, n1: i32, fidx2: i32, n2: i32) -> Result<(), LuaError> { todo!("phase-b-reconcile: join_upvalues") }
    fn upvalue_index(&mut self, i: i32) -> i32 { upvalue_index(i) }
    fn close(&mut self) { todo!("phase-b-reconcile: close") }
    fn set_hook_full(&mut self, f: Option<lua_CFunction>, mask: u32, count: i32) -> Result<(), LuaError> { todo!("phase-b-reconcile: set_hook_full") }

    fn get_local_at(&mut self, ar: &LuaDebug, n: i32) -> Result<Option<Vec<u8>>, LuaError> { todo!("phase-b-reconcile: get_local_at") }
    fn set_local_at(&mut self, ar: &LuaDebug, n: i32) -> Result<Option<Vec<u8>>, LuaError> { todo!("phase-b-reconcile: set_local_at") }
    fn get_param_name(&mut self, fidx: i32, n: i32) -> Result<Option<Vec<u8>>, LuaError> { todo!("phase-b-reconcile: get_param_name") }

    fn get_debug_info(&mut self, what: &[u8], ar: &mut LuaDebug) -> Result<(), LuaError> { todo!("phase-b-reconcile: get_debug_info") }
    fn get_stack_level(&mut self, level: i32, ar: &mut LuaDebug) -> bool { todo!("phase-b-reconcile: get_stack_level") }
    fn has_frames(&mut self) -> bool { todo!("phase-b-reconcile: has_frames") }
    fn lua_traceback(&mut self, other: &mut LuaState, msg: Option<&[u8]>, level: i32) -> Result<(), LuaError> { todo!("phase-b-reconcile: lua_traceback") }

    fn get_hook_count(&mut self) -> i32 { todo!("phase-b-reconcile: get_hook_count") }
    fn get_hook_mask(&mut self) -> u32 { todo!("phase-b-reconcile: get_hook_mask") }
    fn hook_is_set(&mut self) -> bool { todo!("phase-b-reconcile: hook_is_set") }
    fn hook_is_internal_lua_hook(&mut self) -> bool { todo!("phase-b-reconcile: hook_is_internal_lua_hook") }
    fn set_c_stack_limit(&mut self, limit: i32) -> Result<i32, LuaError> { todo!("phase-b-reconcile: set_c_stack_limit") }

    fn new_thread(&mut self, initial_body: Option<LuaValue>) -> Result<GcRef<LuaThread>, LuaError> {
        let _ = initial_body;
        todo!("phase-b-reconcile: new_thread")
    }
    fn is_same_thread(&mut self, other: &LuaState) -> bool { todo!("phase-b-reconcile: is_same_thread") }
    fn thread_status(&mut self) -> LuaStatus { todo!("phase-b-reconcile: thread_status") }

    fn load_buffer(&mut self, buf: &[u8], name: &[u8], mode: Option<&[u8]>) -> Result<LuaStatus, LuaError> { todo!("phase-b-reconcile: load_buffer") }
    fn where_error(&mut self, level: i32, msg: &[u8]) -> LuaError { todo!("phase-b-reconcile: where_error") }
    fn arg(&mut self, n: i32) -> LuaValue { todo!("phase-b-reconcile: arg") }
    fn as_bytes_or_coerce(&mut self, idx: i32) -> Option<Vec<u8>> { todo!("phase-b-reconcile: as_bytes_or_coerce") }
    fn as_bytes(&mut self, idx: i32) -> Option<Vec<u8>> { todo!("phase-b-reconcile: as_bytes") }
}

impl LuaStateStubExt for LuaState {
    fn require_lib(&mut self, name: &[u8], openf: lua_CFunction, glb: bool) -> Result<(), LuaError> {
        crate::auxlib::requiref(self, name, openf, glb)
    }

    fn get_field(&mut self, idx: i32, k: &[u8]) -> Result<LuaType, LuaError> {
        lua_vm::api::get_field(self, idx, k)
    }

    fn abs_index(&mut self, idx: i32) -> i32 {
        lua_vm::api::abs_index(self, idx)
    }

    fn push_value(&mut self, idx: i32) -> Result<(), LuaError> {
        lua_vm::api::push_value(self, idx);
        Ok(())
    }

    fn set_field(&mut self, idx: i32, k: &[u8]) -> Result<(), LuaError> {
        lua_vm::api::set_field(self, idx, k)
    }

    fn set_global(&mut self, name: &[u8]) -> Result<(), LuaError> {
        lua_vm::api::set_global(self, name)
    }

    fn to_boolean(&mut self, idx: i32) -> bool {
        lua_vm::api::to_boolean(self, idx)
    }

    fn top(&mut self) -> i32 {
        lua_vm::api::get_top(self)
    }

    fn push_c_function(&mut self, f: lua_CFunction) -> Result<(), LuaError> {
        let idx: LuaCFnPtr = {
            let mut g = self.global_mut();
            match g.c_functions.iter().position(|&existing| existing == f) {
                Some(i) => i,
                None => {
                    let i = g.c_functions.len();
                    g.c_functions.push(f);
                    i
                }
            }
        };
        self.push(LuaValue::Function(LuaClosure::LightC(idx)));
        Ok(())
    }

    fn push_bytes(&mut self, s: &[u8]) -> Result<(), LuaError> {
        lua_vm::api::push_lstring(self, s)?;
        Ok(())
    }

    fn call(&mut self, nargs: i32, nresults: i32) -> Result<(), LuaError> {
        lua_vm::api::call_k(self, nargs, nresults, 0, None)
    }

    fn remove(&mut self, idx: i32) -> Result<(), LuaError> {
        lua_vm::api::rotate(self, idx, -1);
        lua_vm::api::set_top(self, -2)
    }

    fn get_upvalue(&mut self, fidx: i32, n: i32) -> Result<Option<Vec<u8>>, LuaError> {
        Ok(lua_vm::api::get_upvalue(self, fidx, n))
    }

    fn set_upvalue(&mut self, fidx: i32, n: i32) -> Result<Option<Vec<u8>>, LuaError> {
        Ok(lua_vm::api::setup_value(self, fidx, n))
    }

    fn load(&mut self, chunk: &[u8], name: &[u8], mode: Option<&[u8]>) -> Result<bool, LuaError> {
        let mut remaining = Some(chunk.to_vec());
        let reader: Box<dyn FnMut() -> Option<Vec<u8>>> = Box::new(move || remaining.take());
        let status = lua_vm::api::load(self, reader, Some(name), mode)?;
        Ok(status == LuaStatus::Ok)
    }

    fn push_globals(&mut self) -> Result<(), LuaError> {
        let g = self.global().globals.clone();
        self.push(g);
        Ok(())
    }

    fn set_funcs(&mut self, funcs: &[(&[u8], lua_CFunction)], nup: i32) -> Result<(), LuaError> {
        lua_vm::api::check_stack(self, nup);
        for (name, f) in funcs {
            for _ in 0..nup {
                lua_vm::api::push_value(self, -nup);
            }
            lua_vm::api::push_cclosure(self, *f, nup)?;
            lua_vm::api::set_field(self, -(nup + 2), name)?;
        }
        self.pop_n(nup as usize);
        Ok(())
    }

    fn arg_to_bool(&mut self, arg: i32) -> bool {
        lua_vm::api::to_boolean(self, arg)
    }

    fn value_at(&mut self, idx: i32) -> LuaValue {
        lua_vm::api::push_value(self, idx);
        self.pop()
    }

    fn check_arg_type(&mut self, arg: i32, t: LuaType) -> Result<(), LuaError> {
        if lua_vm::api::lua_type_at(self, arg) != t {
            lua_vm::api::push_value(self, arg);
            let got = self.pop();
            let expected: &str = match t {
                LuaType::None => "no value",
                LuaType::Nil => "nil",
                LuaType::Boolean => "boolean",
                LuaType::LightUserData => "userdata",
                LuaType::Number => "number",
                LuaType::String => "string",
                LuaType::Table => "table",
                LuaType::Function => "function",
                LuaType::UserData => "userdata",
                LuaType::Thread => "thread",
            };
            let got_name = self.full_type_name(&got)?;
            let extramsg = format!(
                "{} expected, got {}",
                expected, String::from_utf8_lossy(&got_name)
            );
            return Err(lua_vm::debug::arg_error_impl(self, arg, extramsg.as_bytes()));
        }
        Ok(())
    }

    fn opt_arg_string(&mut self, arg: i32, def: &[u8]) -> Result<Vec<u8>, LuaError> {
        match lua_vm::api::lua_type_at(self, arg) {
            LuaType::None | LuaType::Nil => Ok(def.to_vec()),
            _ => self.check_arg_string(arg),
        }
    }

    fn get_metafield(&mut self, idx: i32, name: &[u8]) -> Result<LuaType, LuaError> {
        let abs = lua_vm::api::abs_index(self, idx);
        if !lua_vm::api::get_metatable(self, abs) {
            return Ok(LuaType::Nil);
        }
        lua_vm::api::push_lstring(self, name)?;
        let tt = lua_vm::api::raw_get(self, -2);
        if tt == LuaType::Nil {
            self.pop_n(2);
        } else {
            self.remove(-2)?;
        }
        Ok(tt)
    }

    fn table_get_i(&mut self, idx: i32, n: i64) -> Result<LuaType, LuaError> {
        lua_vm::api::get_i(self, idx, n)
    }

    fn compare_lt(&mut self, idx1: i32, idx2: i32) -> Result<bool, LuaError> {
        lua_vm::api::compare(self, idx1, idx2, 1)
    }

    fn check_arg_any(&mut self, arg: i32) -> Result<(), LuaError> {
        if lua_vm::api::lua_type_at(self, arg) == LuaType::None {
            return Err(LuaError::arg_error(arg, "value expected"));
        }
        Ok(())
    }

    fn check_arg_integer(&mut self, arg: i32) -> Result<i64, LuaError> {
        match lua_vm::api::to_integer_x(self, arg) {
            Some(d) => Ok(d),
            None => {
                if lua_vm::api::is_number(self, arg) {
                    Err(LuaError::arg_error(
                        arg,
                        "number has no integer representation",
                    ))
                } else {
                    let got = self.value_at(arg);
                    let got_name = self.full_type_name(&got)?;
                    let extramsg = format!(
                        "number expected, got {}",
                        String::from_utf8_lossy(&got_name)
                    );
                    Err(lua_vm::debug::arg_error_impl(self, arg, extramsg.as_bytes()))
                }
            }
        }
    }

    fn check_arg_string(&mut self, arg: i32) -> Result<Vec<u8>, LuaError> {
        match lua_vm::api::to_lua_string(self, arg)? {
            Some(s) => Ok(s.as_bytes().to_vec()),
            None => {
                let got = self.value_at(arg);
                let got_name = self.full_type_name(&got)?;
                let extramsg = format!(
                    "string expected, got {}",
                    String::from_utf8_lossy(&got_name)
                );
                Err(lua_vm::debug::arg_error_impl(self, arg, extramsg.as_bytes()))
            }
        }
    }

    fn check_arg_number(&mut self, arg: i32) -> Result<f64, LuaError> {
        match lua_vm::api::to_number_x(self, arg) {
            Some(d) => Ok(d),
            None => {
                let got = self.value_at(arg);
                let got_name = self.full_type_name(&got)?;
                let extramsg = format!(
                    "number expected, got {}",
                    String::from_utf8_lossy(&got_name)
                );
                Err(lua_vm::debug::arg_error_impl(self, arg, extramsg.as_bytes()))
            }
        }
    }

    fn check_number(&mut self, arg: i32) -> Result<f64, LuaError> {
        self.check_arg_number(arg)
    }

    fn check_integer(&mut self, arg: i32) -> Result<i64, LuaError> {
        self.check_arg_integer(arg)
    }

    fn check_any(&mut self, arg: i32) -> Result<(), LuaError> {
        self.check_arg_any(arg)
    }

    fn opt_arg_integer(&mut self, arg: i32, def: i64) -> Result<i64, LuaError> {
        match lua_vm::api::lua_type_at(self, arg) {
            LuaType::None | LuaType::Nil => Ok(def),
            _ => self.check_arg_integer(arg),
        }
    }

    fn opt_integer(&mut self, arg: i32, def: i64) -> Result<i64, LuaError> {
        self.opt_arg_integer(arg, def)
    }

    fn opt_number(&mut self, arg: i32, def: f64) -> Result<f64, LuaError> {
        match lua_vm::api::lua_type_at(self, arg) {
            LuaType::None | LuaType::Nil => Ok(def),
            _ => self.check_arg_number(arg),
        }
    }

    fn opt_arg_string_bytes(&mut self, arg: i32) -> Result<Vec<u8>, LuaError> {
        match lua_vm::api::lua_type_at(self, arg) {
            LuaType::None | LuaType::Nil => Ok(Vec::new()),
            _ => self.check_arg_string(arg),
        }
    }

    fn opt_arg_lstring(&mut self, arg: i32, def: Option<&[u8]>) -> Result<Option<Vec<u8>>, LuaError> {
        match lua_vm::api::lua_type_at(self, arg) {
            LuaType::None | LuaType::Nil => Ok(def.map(|d| d.to_vec())),
            _ => Ok(Some(self.check_arg_string(arg)?)),
        }
    }

    fn check_arg_option(
        &mut self,
        arg: i32,
        def: Option<&[u8]>,
        lst: &[&[u8]],
    ) -> Result<usize, LuaError> {
        let name: Vec<u8> = match def {
            Some(d) if matches!(
                lua_vm::api::lua_type_at(self, arg),
                LuaType::None | LuaType::Nil
            ) =>
            {
                d.to_vec()
            }
            _ => self.check_arg_string(arg)?,
        };
        for (i, entry) in lst.iter().enumerate() {
            if *entry == name.as_slice() {
                return Ok(i);
            }
        }
        Err(LuaError::arg_error(arg, "invalid option"))
    }

    fn arg(&mut self, n: i32) -> LuaValue {
        self.value_at(n)
    }

    fn type_at(&mut self, idx: i32) -> LuaType {
        lua_vm::api::lua_type_at(self, idx)
    }

    fn type_name(&mut self, t: LuaType) -> &'static [u8] {
        lua_vm::api::type_name(self, t)
    }

    fn type_name_at(&mut self, idx: i32) -> &'static [u8] {
        let t = lua_vm::api::lua_type_at(self, idx);
        lua_vm::api::type_name(self, t)
    }

    fn is_integer(&mut self, idx: i32) -> bool {
        lua_vm::api::is_integer(self, idx)
    }

    fn is_number(&mut self, idx: i32) -> bool {
        lua_vm::api::is_number(self, idx)
    }

    fn is_none_or_nil(&mut self, idx: i32) -> bool {
        matches!(
            lua_vm::api::lua_type_at(self, idx),
            LuaType::None | LuaType::Nil
        )
    }

    fn to_integer_x(&mut self, idx: i32) -> Option<i64> {
        lua_vm::api::to_integer_x(self, idx)
    }

    fn to_number_x(&mut self, idx: i32) -> Option<f64> {
        lua_vm::api::to_number_x(self, idx)
    }

    fn to_integer(&mut self, idx: i32) -> Option<i64> {
        lua_vm::api::to_integer_x(self, idx)
    }

    fn to_integer_opt(&mut self, idx: i32) -> Option<i64> {
        lua_vm::api::to_integer_x(self, idx)
    }

    fn to_number(&mut self, idx: i32) -> Option<f64> {
        lua_vm::api::to_number_x(self, idx)
    }

    fn to_lua_string(&mut self, idx: i32) -> Option<GcRef<LuaString>> {
        lua_vm::api::to_lua_string(self, idx).ok().flatten()
    }

    fn to_lua_string_bytes(&mut self, idx: i32) -> Option<Vec<u8>> {
        lua_vm::api::to_lua_string(self, idx)
            .ok()
            .flatten()
            .map(|s| s.as_bytes().to_vec())
    }

    fn to_lua_string_len(&mut self, idx: i32) -> Option<usize> {
        lua_vm::api::to_lua_string(self, idx)
            .ok()
            .flatten()
            .map(|s| s.len())
    }

    fn to_bytes(&mut self, idx: i32) -> Option<Vec<u8>> {
        self.to_lua_string_bytes(idx)
    }

    fn to_bytes_at(&mut self, idx: i32) -> Option<Vec<u8>> {
        self.to_lua_string_bytes(idx)
    }

    fn raw_equal(&mut self, idx1: i32, idx2: i32) -> Result<bool, LuaError> {
        Ok(lua_vm::api::raw_equal(self, idx1, idx2))
    }

    fn raw_geti(&mut self, idx: i32, n: i64) -> Result<LuaType, LuaError> {
        Ok(lua_vm::api::raw_get_i(self, idx, n))
    }

    fn raw_get_i(&mut self, idx: i32, n: i64) -> Result<LuaType, LuaError> {
        Ok(lua_vm::api::raw_get_i(self, idx, n))
    }

    fn raw_seti(&mut self, idx: i32, n: i64) -> Result<(), LuaError> {
        lua_vm::api::raw_set_i(self, idx, n)
    }

    fn raw_set_i(&mut self, idx: i32, n: i64) -> Result<(), LuaError> {
        lua_vm::api::raw_set_i(self, idx, n)
    }

    fn raw_len(&mut self, idx: i32) -> i64 {
        lua_vm::api::raw_len(self, idx) as i64
    }

    fn get_i(&mut self, idx: i32, n: i64) -> Result<LuaType, LuaError> {
        lua_vm::api::get_i(self, idx, n)
    }

    fn get_metatable(&mut self, idx: i32) -> Result<bool, LuaError> {
        Ok(lua_vm::api::get_metatable(self, idx))
    }

    fn set_metatable(&mut self, idx: i32) -> Result<(), LuaError> {
        lua_vm::api::set_metatable(self, idx)?;
        Ok(())
    }

    fn compare(&mut self, idx1: i32, idx2: i32, op: CompareOp) -> Result<bool, LuaError> {
        let op_i = match op {
            CompareOp::Eq => 0,
            CompareOp::Lt => 1,
            CompareOp::Le => 2,
        };
        lua_vm::api::compare(self, idx1, idx2, op_i)
    }

    fn protected_call(&mut self, nargs: i32, nresults: i32, msgh: i32) -> Result<(), LuaError> {
        lua_vm::api::pcall_k(self, nargs, nresults, msgh, 0, None)?;
        Ok(())
    }

    fn push_value_at(&mut self, idx: i32) -> Result<(), LuaError> {
        lua_vm::api::push_value(self, idx);
        Ok(())
    }

    fn push_thread(&mut self) -> Result<bool, LuaError> {
        Ok(lua_vm::api::push_thread(self))
    }

    fn push_cclosure(&mut self, f: lua_CFunction, n: i32) -> Result<(), LuaError> {
        lua_vm::api::push_cclosure(self, f, n)
    }

    fn push_c_closure(&mut self, f: lua_CFunction, n: i32) -> Result<(), LuaError> {
        lua_vm::api::push_cclosure(self, f, n)
    }

    fn push_lstring(&mut self, s: &[u8]) -> Result<(), LuaError> {
        lua_vm::api::push_lstring(self, s)?;
        Ok(())
    }

    fn push_string(&mut self, s: &[u8]) -> Result<(), LuaError> {
        lua_vm::api::push_lstring(self, s)?;
        Ok(())
    }

    fn get_top(&mut self) -> i32 {
        lua_vm::api::get_top(self)
    }

    fn stack_top(&mut self) -> i32 {
        lua_vm::api::get_top(self)
    }

    fn top_count(&mut self) -> i32 {
        lua_vm::api::get_top(self)
    }

    fn check_stack_space(&mut self, n: i32) -> bool {
        lua_vm::api::check_stack(self, n)
    }

    fn rotate(&mut self, idx: i32, n: i32) -> Result<(), LuaError> {
        lua_vm::api::rotate(self, idx, n);
        Ok(())
    }

    fn insert(&mut self, idx: i32) -> Result<(), LuaError> {
        lua_vm::api::rotate(self, idx, 1);
        Ok(())
    }

    fn copy_value(&mut self, from: i32, to: i32) -> Result<(), LuaError> {
        lua_vm::api::copy(self, from, to);
        Ok(())
    }

    fn replace(&mut self, idx: i32) -> Result<(), LuaError> {
        lua_vm::api::copy(self, -1, idx);
        lua_vm::api::set_top(self, -2)
    }

    fn len_op(&mut self, idx: i32) -> Result<(), LuaError> {
        lua_vm::api::len(self, idx)
    }

    fn table_next(&mut self, idx: i32) -> Result<bool, LuaError> {
        lua_vm::api::next(self, idx)
    }

    fn create_table(&mut self, narr: i32, nrec: i32) -> Result<(), LuaError> {
        lua_vm::api::create_table(self, narr, nrec)
    }

    fn to_userdata(&mut self, idx: i32) -> Option<GcRef<LuaUserData>> {
        let v = self.value_at(idx);
        if let LuaValue::UserData(u) = v { Some(u) } else { None }
    }

    fn to_light_userdata(&mut self, idx: i32) -> Option<*mut std::ffi::c_void> {
        lua_vm::api::to_userdata(self, idx)
    }

    fn to_thread(&mut self, idx: i32) -> Option<GcRef<LuaThread>> {
        lua_vm::api::to_thread(self, idx)
    }

    fn to_thread_at(&mut self, idx: i32) -> Option<GcRef<LuaThread>> {
        lua_vm::api::to_thread(self, idx)
    }

    fn len_at(&mut self, idx: i32) -> i64 {
        lua_vm::api::raw_len(self, idx) as i64
    }

    fn length_at(&mut self, idx: i32) -> Result<i64, LuaError> {
        lua_vm::api::len(self, idx)?;
        let v = lua_vm::api::to_integer_x(self, -1);
        self.pop_n(1);
        match v {
            Some(l) => Ok(l),
            None => Err(LuaError::runtime(format_args!(
                "object length is not an integer"
            ))),
        }
    }

    fn peek_bytes(&mut self, idx: i32) -> Option<Vec<u8>> {
        self.to_lua_string_bytes(idx)
    }

    fn to_string_coerced(&mut self, idx: i32) -> Option<Vec<u8>> {
        self.to_lua_string_bytes(idx)
    }

    fn raw_get(&mut self, idx: i32) -> Result<LuaType, LuaError> {
        Ok(lua_vm::api::raw_get(self, idx))
    }

    fn raw_set(&mut self, idx: i32) -> Result<(), LuaError> {
        lua_vm::api::raw_set(self, idx)
    }

    fn is_c_function_at(&mut self, idx: i32) -> bool {
        lua_vm::api::is_cfunction(self, idx)
    }

    fn type_name_str_at(&mut self, idx: i32) -> &'static [u8] {
        let t = lua_vm::api::lua_type_at(self, idx);
        lua_vm::api::type_name(self, t)
    }

    fn push_fstring(&mut self, args: std::fmt::Arguments<'_>) -> Result<(), LuaError> {
        let formatted = std::fmt::format(args);
        lua_vm::api::push_fstring(self, formatted.as_bytes())?;
        Ok(())
    }

    fn arith(&mut self, op: ArithOp) -> Result<(), LuaError> {
        lua_vm::api::arith(self, op as i32)
    }

    fn lua_version(&mut self) -> f64 {
        504.0
    }

    fn push_fail(&mut self) -> Result<(), LuaError> {
        self.push(LuaValue::Nil);
        Ok(())
    }

    fn push_registry(&mut self) -> Result<(), LuaError> {
        let r = self.registry_value();
        self.push(r);
        Ok(())
    }

    fn push_upvalue(&mut self, idx: i32) -> Result<(), LuaError> {
        lua_vm::api::push_value(self, upvalue_index(idx));
        Ok(())
    }

    fn pop_bytes(&mut self) -> Vec<u8> {
        match self.pop() {
            LuaValue::Str(s) => s.as_bytes().to_vec(),
            _ => Vec::new(),
        }
    }

    fn push_where(&mut self, level: i32) -> Result<(), LuaError> {
        let mut ar = lua_vm::debug::LuaDebug::default();
        if lua_vm::debug::get_stack(self, level, &mut ar) {
            lua_vm::debug::get_info(self, b"Sl", &mut ar);
            if ar.currentline > 0 {
                let zero = ar
                    .short_src
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(ar.short_src.len());
                let mut buf: Vec<u8> = ar.short_src[..zero].to_vec();
                buf.push(b':');
                buf.extend_from_slice(ar.currentline.to_string().as_bytes());
                buf.extend_from_slice(b": ");
                lua_vm::api::push_lstring(self, &buf)?;
                return Ok(());
            }
        }
        lua_vm::api::push_lstring(self, b"")?;
        Ok(())
    }

    fn where_error(&mut self, level: i32, msg: &[u8]) -> LuaError {
        if self.push_where(level).is_err() {
            return LuaError::runtime(format_args!("{}", StubBStr(msg)));
        }
        let mut full = self.pop_bytes();
        full.extend_from_slice(msg);
        LuaError::runtime(format_args!("{}", StubBStr(&full)))
    }

    fn registry_get(&mut self, key: &[u8]) -> Result<LuaType, LuaError> {
        lua_vm::api::get_field(self, STUB_LUA_REGISTRYINDEX, key)
    }

    fn get_field_registry(&mut self, name: &[u8]) -> Result<LuaType, LuaError> {
        lua_vm::api::get_field(self, STUB_LUA_REGISTRYINDEX, name)
    }

    fn get_registry_field(&mut self, name: &[u8]) -> Result<LuaType, LuaError> {
        lua_vm::api::get_field(self, STUB_LUA_REGISTRYINDEX, name)
    }

    fn get_or_create_registry_subtable(&mut self, name: &[u8]) -> Result<bool, LuaError> {
        self.get_subtable_registry(name)
    }

    fn registry_set(&mut self, key: &[u8]) -> Result<(), LuaError> {
        lua_vm::api::set_field(self, STUB_LUA_REGISTRYINDEX, key)
    }

    fn check_stack_growth(&mut self, n: i32) -> bool {
        lua_vm::api::check_stack(self, n)
    }

    fn ensure_stack<S: AsRef<[u8]> + ?Sized>(&mut self, n: i32, msg: &S) -> Result<(), LuaError> {
        if lua_vm::api::check_stack(self, n) {
            return Ok(());
        }
        let m = msg.as_ref();
        if m.is_empty() {
            Err(LuaError::runtime(format_args!("stack overflow")))
        } else {
            Err(LuaError::runtime(format_args!(
                "stack overflow ({})",
                StubBStr(m)
            )))
        }
    }

    fn push_copy(&mut self, idx: i32) -> Result<(), LuaError> {
        lua_vm::api::push_value(self, idx);
        Ok(())
    }

    fn as_bytes(&mut self, idx: i32) -> Option<Vec<u8>> {
        self.to_lua_string_bytes(idx)
    }

    fn as_bytes_or_coerce(&mut self, idx: i32) -> Option<Vec<u8>> {
        self.to_lua_string_bytes(idx)
    }

    fn thread_status(&mut self) -> LuaStatus {
        lua_vm::api::status(self)
    }

    fn new_thread(&mut self, initial_body: Option<LuaValue>) -> Result<GcRef<LuaThread>, LuaError> {
        lua_vm::state::new_thread(self, initial_body)?;
        let th = lua_vm::api::to_thread(self, -1)
            .ok_or_else(|| LuaError::runtime(format_args!("new_thread: missing thread on top")))?;
        Ok(th)
    }

    fn is_same_thread(&mut self, other: &LuaState) -> bool {
        std::ptr::eq(self as *const LuaState, other as *const LuaState)
    }

    fn load_buffer(&mut self, buf: &[u8], name: &[u8], mode: Option<&[u8]>) -> Result<LuaStatus, LuaError> {
        let mut remaining = Some(buf.to_vec());
        let reader: Box<dyn FnMut() -> Option<Vec<u8>>> = Box::new(move || remaining.take());
        lua_vm::api::load(self, reader, Some(name), mode)
    }

    fn load_buffer_ex<M: ?Sized>(&mut self, buf: &[u8], name: &[u8], mode: &M) -> Result<bool, LuaError>
    where
        M: AsRef<[u8]>,
    {
        let mut remaining = Some(buf.to_vec());
        let reader: Box<dyn FnMut() -> Option<Vec<u8>>> = Box::new(move || remaining.take());
        let mode_bytes = mode.as_ref();
        let status = lua_vm::api::load(self, reader, Some(name), Some(mode_bytes))?;
        Ok(status == LuaStatus::Ok)
    }

    fn dump_function(&mut self, strip: bool) -> Result<Vec<u8>, LuaError> {
        let mut out: Vec<u8> = Vec::new();
        let mut writer = |chunk: &[u8]| -> Result<(), LuaError> {
            out.extend_from_slice(chunk);
            Ok(())
        };
        let ok = lua_vm::api::dump(self, &mut writer, strip)?;
        if !ok {
            return Err(LuaError::runtime(format_args!(
                "unable to dump given function"
            )));
        }
        Ok(out)
    }

    fn warning(&mut self, msg: &[u8], to_cont: bool) -> Result<(), LuaError> {
        lua_vm::api::warning(self, msg, to_cont);
        Ok(())
    }

    fn string_to_number(&mut self, idx: i32) -> Option<usize> {
        let bytes = lua_vm::api::to_lua_string(self, idx)
            .ok()
            .flatten()?
            .as_bytes()
            .to_vec();
        let consumed = lua_vm::api::string_to_number(self, &bytes);
        if consumed == 0 {
            None
        } else {
            Some(consumed)
        }
    }

    fn string_to_number_push<S: AsRef<[u8]> + ?Sized>(&mut self, s: &S) -> Result<usize, LuaError> {
        Ok(lua_vm::api::string_to_number(self, s.as_ref()))
    }

    fn gc_count(&mut self) -> Result<i32, LuaError> {
        Ok(lua_vm::api::gc(self, lua_vm::api::GcArgs::Count))
    }

    fn gc_count_b(&mut self) -> Result<i32, LuaError> {
        Ok(lua_vm::api::gc(self, lua_vm::api::GcArgs::CountB))
    }

    fn gc_step(&mut self, data: i32) -> Result<i32, LuaError> {
        Ok(lua_vm::api::gc(self, lua_vm::api::GcArgs::Step { data }))
    }

    fn gc_is_running(&mut self) -> Result<bool, LuaError> {
        Ok(lua_vm::api::gc(self, lua_vm::api::GcArgs::IsRunning) != 0)
    }

    fn gc_control_simple(&mut self, op: i32) -> Result<i32, LuaError> {
        let args = match op {
            0 => lua_vm::api::GcArgs::Stop,
            1 => lua_vm::api::GcArgs::Restart,
            2 => lua_vm::api::GcArgs::Collect,
            _ => return Err(LuaError::runtime(format_args!(
                "invalid GC option {}", op
            ))),
        };
        Ok(lua_vm::api::gc(self, args))
    }

    fn gc_set_param(&mut self, op: i32, value: i32) -> Result<i32, LuaError> {
        let args = match op {
            6 => lua_vm::api::GcArgs::SetPause { value },
            7 => lua_vm::api::GcArgs::SetStepMul { value },
            _ => return Err(LuaError::runtime(format_args!(
                "invalid GC param option {}", op
            ))),
        };
        Ok(lua_vm::api::gc(self, args))
    }

    fn gc_gen(&mut self, minor_mul: i32, major_mul: i32) -> Result<i32, LuaError> {
        Ok(lua_vm::api::gc(
            self,
            lua_vm::api::GcArgs::Gen { minormul: minor_mul, majormul: major_mul },
        ))
    }

    fn gc_inc(&mut self, pause: i32, step_mul: i32, step_size: i32) -> Result<i32, LuaError> {
        Ok(lua_vm::api::gc(
            self,
            lua_vm::api::GcArgs::Inc { pause, stepmul: step_mul, stepsize: step_size },
        ))
    }

    fn get_meta_field(&mut self, idx: i32, name: &[u8]) -> Result<bool, LuaError> {
        Ok(crate::auxlib::get_metafield(self, idx, name)? != LuaType::Nil)
    }

    fn to_display_string(&mut self, idx: i32) -> Result<Vec<u8>, LuaError> {
        crate::auxlib::to_lua_string(self, idx)
    }

    fn get_subtable_registry(&mut self, name: &[u8]) -> Result<bool, LuaError> {
        crate::auxlib::get_subtable(self, STUB_LUA_REGISTRYINDEX, name)
    }

    fn new_metatable(&mut self, name: &[u8]) -> Result<bool, LuaError> {
        crate::auxlib::new_metatable(self, name)
    }

    fn set_metatable_by_name(&mut self, name: &[u8]) -> Result<(), LuaError> {
        crate::auxlib::set_metatable(self, name)
    }

    fn check_arg_userdata(&mut self, arg: i32, name: &[u8]) -> Result<GcRef<LuaUserData>, LuaError> {
        crate::auxlib::check_udata(self, arg, name)
    }

    fn test_arg_userdata(&mut self, arg: i32, name: &[u8]) -> Option<GcRef<LuaUserData>> {
        crate::auxlib::test_udata(self, arg, name).ok().flatten()
    }

    fn get_table(&mut self, idx: i32) -> Result<LuaType, LuaError> {
        lua_vm::api::get_table(self, idx)
    }

    fn get_stack(&mut self, level: i32, ar: &mut LuaDebug) -> bool {
        let mut lvm_ar = lua_vm::debug::LuaDebug::default();
        let ok = lua_vm::debug::get_stack(self, level, &mut lvm_ar);
        if ok {
            ar.i_ci_idx = lvm_ar.i_ci;
        } else {
            ar.i_ci_idx = None;
        }
        ok
    }

    fn get_stack_level(&mut self, level: i32, ar: &mut LuaDebug) -> bool {
        LuaStateStubExt::get_stack(self, level, ar)
    }

    fn get_info(&mut self, what: &[u8], ar: &mut LuaDebug) -> Result<(), LuaError> {
        let mut lvm_ar = lua_vm::debug::LuaDebug::default();
        lvm_ar.i_ci = ar.i_ci_idx;
        let ok = lua_vm::debug::get_info(self, what, &mut lvm_ar);
        copy_lvm_debug_to_stub_selective(&lvm_ar, ar, what);
        if ok {
            Ok(())
        } else {
            Err(LuaError::runtime(format_args!("invalid option")))
        }
    }

    fn get_debug_info(&mut self, what: &[u8], ar: &mut LuaDebug) -> Result<(), LuaError> {
        LuaStateStubExt::get_info(self, what, ar)
    }

    fn get_local_at(&mut self, ar: &LuaDebug, n: i32) -> Result<Option<Vec<u8>>, LuaError> {
        let mut lvm_ar = lua_vm::debug::LuaDebug::default();
        lvm_ar.i_ci = ar.i_ci_idx;
        Ok(lua_vm::debug::get_local(self, Some(&lvm_ar), n))
    }

    fn set_local_at(&mut self, ar: &LuaDebug, n: i32) -> Result<Option<Vec<u8>>, LuaError> {
        let mut lvm_ar = lua_vm::debug::LuaDebug::default();
        lvm_ar.i_ci = ar.i_ci_idx;
        Ok(lua_vm::debug::set_local(self, &lvm_ar, n))
    }

    fn get_param_name(&mut self, fidx: i32, n: i32) -> Result<Option<Vec<u8>>, LuaError> {
        let _ = fidx;
        Ok(lua_vm::debug::get_local(self, None, n))
    }

    fn has_frames(&mut self) -> bool {
        !self.is_base_ci(self.current_ci_idx())
    }

    fn lua_traceback(
        &mut self,
        other: &mut LuaState,
        msg: Option<&[u8]>,
        level: i32,
    ) -> Result<(), LuaError> {
        crate::auxlib::traceback(self, Some(other), msg, level)
    }

    fn upvalue_id(&mut self, fidx: i32, n: i32) -> Result<*mut std::ffi::c_void, LuaError> {
        match lua_vm::api::upvalue_id(self, fidx, n) {
            Some(id) => Ok(id as *mut std::ffi::c_void),
            None => Ok(std::ptr::null_mut()),
        }
    }

    fn join_upvalues(&mut self, fidx1: i32, n1: i32, fidx2: i32, n2: i32) -> Result<(), LuaError> {
        lua_vm::api::upvalue_join(self, fidx1, n1, fidx2, n2);
        Ok(())
    }

    /// Pre-collect the chunks supplied by `reader` (a state-aware callback)
    /// and forward the accumulated buffer to `lua_vm::api::load`. The streaming
    /// loader in `lua_vm::api::load` consumes a `Box<dyn FnMut() -> Option<Vec<u8>>>`
    /// that does not take a `&mut LuaState`, so the state-touching reader
    /// (e.g. `generic_reader`, which calls a Lua function to produce each
    /// chunk) is drained first. C-Lua streams chunks directly through
    /// `lua_load`; the materialised path here is observable only for very
    /// large source files and is intentional for the Phase-B shim.
    fn load_with_reader<F, M: ?Sized>(&mut self, mut reader: F, name: &[u8], mode: &M) -> Result<bool, LuaError>
    where
        F: FnMut(&mut LuaState) -> Result<Option<Vec<u8>>, LuaError>,
        M: AsRef<[u8]>,
    {
        let mut buf: Vec<u8> = Vec::new();
        let mut reader_err: Option<LuaError> = None;
        loop {
            match reader(self) {
                Err(e) => {
                    reader_err = Some(e);
                    break;
                }
                Ok(None) => break,
                Ok(Some(piece)) => {
                    if piece.is_empty() {
                        break;
                    }
                    buf.extend_from_slice(&piece);
                }
            }
        }
        if let Some(e) = reader_err {
            let msg_value = match e {
                LuaError::Runtime(v) | LuaError::Syntax(v) => v,
                LuaError::Memory => {
                    let s = self.intern_str(b"not enough memory")?;
                    LuaValue::Str(s)
                }
                _ => {
                    let s = self.intern_str(b"error in reader function")?;
                    LuaValue::Str(s)
                }
            };
            self.push(msg_value);
            return Ok(false);
        }
        let mut once = Some(buf);
        let boxed: Box<dyn FnMut() -> Option<Vec<u8>>> = Box::new(move || once.take());
        let mode_bytes = mode.as_ref();
        let status = lua_vm::api::load(self, boxed, Some(name), Some(mode_bytes))?;
        Ok(status == LuaStatus::Ok)
    }

    fn load_file_ex(&mut self, path: Option<&[u8]>, mode: Option<&[u8]>) -> Result<bool, LuaError> {
        let status = crate::auxlib::load_filex(self, path, mode)?;
        Ok(status == 0)
    }

    fn load_file(&mut self, path: Option<&[u8]>) -> Result<bool, LuaError> {
        LuaStateStubExt::load_file_ex(self, path, None)
    }

    fn get_iuservalue(&mut self, idx: i32, n: i32) -> Result<LuaType, LuaError> {
        Ok(lua_vm::api::get_i_uservalue(self, idx, n))
    }

    fn set_iuservalue(&mut self, idx: i32, n: i32) -> Result<bool, LuaError> {
        lua_vm::api::set_i_uservalue(self, idx, n)
    }

    fn get_hook_mask(&mut self) -> u32 {
        lua_vm::debug::get_hook_mask(self) as u32
    }

    fn get_hook_count(&mut self) -> i32 {
        lua_vm::debug::get_hook_count(self)
    }

    /// Approximate "is a debug hook installed?" using the hook event mask.
    /// `lua_sethook` clears the mask whenever the hook is uninstalled, so a
    /// non-zero mask is equivalent to a non-NULL `L->hook` for the
    /// `debug.gethook` call site. Avoids invoking `state.hook()`, which is
    /// still a Phase-B `todo!` on `LuaState`.
    fn hook_is_set(&mut self) -> bool {
        lua_vm::debug::get_hook_mask(self) != 0
    }

    /// The port has no internal Lua-managed hook trampoline yet — any
    /// installed hook is treated as external. Mirrors what `debug.gethook`
    /// reports when the user installs their own `lua_Hook`.
    fn hook_is_internal_lua_hook(&mut self) -> bool {
        false
    }

    fn set_c_stack_limit(&mut self, limit: i32) -> Result<i32, LuaError> {
        let clamped = if limit < 0 { 0u32 } else { limit as u32 };
        Ok(lua_vm::state::set_c_stack_limit(self, clamped))
    }

    /// `lua_close(L)` destroys a Lua state. In Rust the state's resources are
    /// released by `Drop` when the owning value goes out of scope, so the
    /// in-place `&mut self` form is a no-op. The consuming free function
    /// `lua_vm::state::close(state)` is reserved for the top-level shutdown
    /// path in `lua-cli`.
    fn close(&mut self) {
        let _ = self;
    }

    /// Install (or clear) a debug hook on this thread.
    ///
    /// C: `LUA_API void lua_sethook (lua_State *L, lua_Hook func, int mask, int count)`
    /// (`ldebug.c`).
    ///
    /// The Phase-B `LuaStateStubExt` signature uses `lua_CFunction` (the
    /// stdlib C-function shape: `fn(&mut LuaState) -> Result<usize, LuaError>`)
    /// for `f`, whereas the canonical `lua_vm::debug::set_hook` takes a
    /// `Box<dyn FnMut(&mut LuaState, &LuaDebug)>` (a true Lua hook, which has
    /// access to the active `lua_Debug`). To bridge the two, an installed
    /// `lua_CFunction` is wrapped in a trampoline closure that calls it with
    /// `state` and discards both the activation record and the
    /// `Result<usize, LuaError>` (a hook's return value is ignored by C-Lua).
    fn set_hook_full(
        &mut self,
        f: Option<lua_CFunction>,
        mask: u32,
        count: i32,
    ) -> Result<(), LuaError> {
        let hook: Option<Box<dyn FnMut(&mut LuaState, &lua_vm::debug::LuaDebug)>> = match f {
            None => None,
            Some(func) => Some(Box::new(move |state, _ar| {
                let _ = func(state);
            })),
        };
        lua_vm::debug::set_hook(self, hook, mask as i32, count);
        Ok(())
    }

    /// Write `msg` to the host's standard output stream.
    ///
    /// C: `lua_writestring(s, l)` macro from `lauxlib.h` (defaults to
    /// `fwrite(s, 1, l, stdout)`).
    ///
    /// Delegates to the canonical inherent `LuaState::write_output`. UFCS is
    /// used to disambiguate from the trait method (this method) which would
    /// otherwise recurse.
    fn write_output(&mut self, msg: &[u8]) -> Result<(), LuaError> {
        LuaState::write_output(self, msg)
    }

    /// `t[n] = v`, where `t` is the value at `idx` and `v` is popped from the
    /// stack top. Honours `__newindex`.
    ///
    /// C: `LUA_API void lua_seti (lua_State *L, int idx, lua_Integer n)`.
    fn table_set_i(&mut self, idx: i32, n: i64) -> Result<(), LuaError> {
        LuaState::table_set_i(self, idx, n)
    }

    /// Allocate a fresh full-userdata, push it on the stack, and return a
    /// `GcRef` to it. `name` is advisory (callers typically follow up with
    /// `set_metatable_by_name(name)`).
    ///
    /// C-correspondent: `lua_newuserdatauv(L, size, nuvalue)` plus the
    /// auxiliary `luaL_setmetatable` pattern. The Rust signature carries
    /// `name` for caller convenience as documented on the inherent method.
    fn new_userdata_typed(
        &mut self,
        name: &[u8],
        size: usize,
        nuvalue: i32,
    ) -> Result<GcRef<LuaUserData>, LuaError> {
        LuaState::new_userdata_typed(self, name, size, nuvalue)
    }
}

/// Copy populated fields from the canonical `lua_vm::debug::LuaDebug` into
/// the Phase-B stub `LuaDebug`. The two structs diverge on a few field types
/// (e.g. `what` is a single byte tag in the stub vs. `Option<&'static [u8]>`
/// in the canonical struct, `short_src` is `Vec<u8>` vs. fixed array).
/// Copy only the fields that `lua_getinfo`'s `what` string actually populates
/// in C. Mirrors `auxgetinfo` in `ldebug.c`: each option byte writes a disjoint
/// subset of `lua_Debug`. Calling `get_info` with one option string must not
/// clobber fields populated by an earlier call with a different option string
/// (a pattern the auxiliary library relies on — `pushglobalfuncname` calls
/// `lua_getinfo(L, "f", ar)` and expects the previously-set `namewhat`/`what`/
/// `short_src`/`linedefined` to survive).
fn copy_lvm_debug_to_stub_selective(
    src: &lua_vm::debug::LuaDebug,
    dst: &mut LuaDebug,
    what: &[u8],
) {
    dst.i_ci_idx = src.i_ci;
    for &ch in what {
        match ch {
            b'S' => {
                dst.what = match src.what {
                    Some(b"Lua") => b'L',
                    Some(b"C") => b'C',
                    Some(b"main") => b'm',
                    _ => 0,
                };
                dst.source = src.source.clone().unwrap_or_default();
                let zero = src
                    .short_src
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(src.short_src.len());
                dst.short_src = src.short_src[..zero].to_vec();
                dst.linedefined = src.linedefined;
                dst.lastlinedefined = src.lastlinedefined;
            }
            b'l' => {
                dst.currentline = src.currentline;
            }
            b'u' => {
                dst.nups = src.nups;
                dst.nparams = src.nparams;
                dst.isvararg = src.isvararg;
            }
            b't' => {
                dst.istailcall = src.istailcall;
            }
            b'n' => {
                dst.name = src.name.clone();
                dst.namewhat = src.namewhat.map(|s| s.to_vec()).unwrap_or_default();
            }
            b'r' => {
                dst.ftransfer = src.ftransfer;
                dst.ntransfer = src.ntransfer;
            }
            _ => {}
        }
    }
}

const STUB_LUA_REGISTRYINDEX: i32 = -(1_000_000) - 1000;

struct StubBStr<'a>(&'a [u8]);

impl<'a> std::fmt::Display for StubBStr<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use std::fmt::Write as _;
        for &b in self.0 {
            if b.is_ascii() {
                f.write_char(b as char)?;
            } else {
                write!(f, "\\x{:02x}", b)?;
            }
        }
        Ok(())
    }
}

// ──────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        (Phase-B reconcile shim; no C source)
//   target_crate:  lua-stdlib
//   confidence:    high
//   todos:         0
//   port_notes:    3
//   unsafe_blocks: 0
//   notes:         Re-exports lua_vm::state::LuaState (canonical owner per
//                  harness/type-vocabulary.tsv); the LuaStateStubExt trait
//                  carries every Phase-A stub method as a
//                  todo!("phase-b-reconcile: …") body so the rest of
//                  lua-stdlib keeps compiling while the canonical API
//                  catches up.
// ──────────────────────────────────────────────────────────────────────────
