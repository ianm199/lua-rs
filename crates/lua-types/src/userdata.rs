//! `LuaUserData` — Lua's heap-allocated userdata. Carries a typed byte
//! buffer plus optional user values (a Vec of TValues).

use std::any::Any;
use std::cell::RefCell;
use std::rc::Rc;

use crate::gc::GcRef;
use crate::table::LuaTable;
use crate::value::LuaValue;

pub struct LuaUserData {
    pub data: Box<[u8]>,
    pub uv: Vec<LuaValue>,
    pub metatable: RefCell<Option<GcRef<LuaTable>>>,
    pub host_value: RefCell<Option<Rc<dyn Any>>>,
}

impl std::fmt::Debug for LuaUserData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LuaUserData")
            .field("data_len", &self.data.len())
            .field("uv_len", &self.uv.len())
            .field("has_metatable", &self.metatable.borrow().is_some())
            .field("has_host_value", &self.host_value.borrow().is_some())
            .finish()
    }
}

impl LuaUserData {
    pub fn placeholder() -> Self {
        LuaUserData {
            data: Box::new([]),
            uv: Vec::new(),
            metatable: RefCell::new(None),
            host_value: RefCell::new(None),
        }
    }

    pub fn metatable(&self) -> Option<GcRef<LuaTable>> {
        self.metatable.borrow().clone()
    }

    pub fn set_metatable(&self, mt: Option<GcRef<LuaTable>>) {
        *self.metatable.borrow_mut() = mt;
    }

    pub fn host_value(&self) -> Option<Rc<dyn Any>> {
        self.host_value.borrow().clone()
    }

    pub fn set_host_value(&self, value: Option<Rc<dyn Any>>) {
        *self.host_value.borrow_mut() = value;
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        src/lobject.h (Udata + Udata0)
//   target_crate:  lua-types
//   confidence:    high
//   todos:         0
//   port_notes:    0
//   unsafe_blocks: 0
//   notes:         LuaUserData type, including metatable slot and uservalues. C uses a
//                  flexible-array trailing payload; we use a typed Vec / Box of the
//                  user payload + uservalues vector.
// ──────────────────────────────────────────────────────────────────────────────
