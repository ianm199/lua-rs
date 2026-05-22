//! `LuaValue` — the tagged-union value type. PORT_STRATEGY §3.2.

use crate::closure::LuaClosure;
use crate::gc::GcRef;
use crate::string::LuaString;
use crate::userdata::LuaUserData;
use std::ffi::c_void;

/// The dynamically-typed Lua value. Replaces C's `TValue`.
#[derive(Debug, Clone)]
pub enum LuaValue {
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(GcRef<LuaString>),
    Table(GcRef<LuaTable>),
    Function(LuaClosure),
    UserData(GcRef<LuaUserData>),
    LightUserData(*mut c_void),
    Thread(GcRef<LuaThread>),
}

impl LuaValue {
    pub fn type_tag(&self) -> crate::LuaType {
        use crate::LuaType::*;
        match self {
            LuaValue::Nil               => Nil,
            LuaValue::Bool(_)           => Boolean,
            LuaValue::Int(_)            => Number,
            LuaValue::Float(_)          => Number,
            LuaValue::Str(_)            => String,
            LuaValue::Table(_)          => Table,
            LuaValue::Function(_)       => Function,
            LuaValue::UserData(_)       => UserData,
            LuaValue::LightUserData(_)  => LightUserData,
            LuaValue::Thread(_)         => Thread,
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            LuaValue::Nil               => "nil",
            LuaValue::Bool(_)           => "boolean",
            LuaValue::Int(_)            => "number",
            LuaValue::Float(_)          => "number",
            LuaValue::Str(_)            => "string",
            LuaValue::Table(_)          => "table",
            LuaValue::Function(_)       => "function",
            LuaValue::UserData(_)       => "userdata",
            LuaValue::LightUserData(_)  => "userdata",
            LuaValue::Thread(_)         => "thread",
        }
    }

    pub fn is_nil(&self) -> bool   { matches!(self, LuaValue::Nil) }
    pub fn is_falsy(&self) -> bool { matches!(self, LuaValue::Nil | LuaValue::Bool(false)) }
    pub fn is_truthy(&self) -> bool { !self.is_falsy() }
    pub fn is_collectable(&self) -> bool {
        matches!(self,
            LuaValue::Str(_) | LuaValue::Table(_) | LuaValue::Function(_) |
            LuaValue::UserData(_) | LuaValue::Thread(_))
    }

    pub fn as_int(&self) -> Option<i64> {
        match self { LuaValue::Int(i) => Some(*i), _ => None }
    }
    pub fn as_float(&self) -> Option<f64> {
        match self { LuaValue::Float(f) => Some(*f), _ => None }
    }
    pub fn as_string(&self) -> Option<&GcRef<LuaString>> {
        match self { LuaValue::Str(s) => Some(s), _ => None }
    }
    pub fn as_table(&self) -> Option<&GcRef<LuaTable>> {
        match self { LuaValue::Table(t) => Some(t), _ => None }
    }
}

impl Default for LuaValue {
    fn default() -> Self { LuaValue::Nil }
}

impl PartialEq for LuaValue {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (LuaValue::Nil, LuaValue::Nil) => true,
            (LuaValue::Bool(a), LuaValue::Bool(b)) => a == b,
            (LuaValue::Int(a), LuaValue::Int(b)) => a == b,
            (LuaValue::Float(a), LuaValue::Float(b)) => a == b,
            (LuaValue::Str(a), LuaValue::Str(b)) => GcRef::ptr_eq(a, b) || a.as_bytes() == b.as_bytes(),
            (LuaValue::Table(a), LuaValue::Table(b)) => GcRef::ptr_eq(a, b),
            (LuaValue::Function(a), LuaValue::Function(b)) => closure_eq(a, b),
            (LuaValue::UserData(a), LuaValue::UserData(b)) => GcRef::ptr_eq(a, b),
            (LuaValue::LightUserData(a), LuaValue::LightUserData(b)) => a == b,
            (LuaValue::Thread(a), LuaValue::Thread(b)) => GcRef::ptr_eq(a, b),
            _ => false,
        }
    }
}

/// Float-to-integer rounding mode (matches C-Lua's F2Imod).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum F2Imod {
    Floor,
    Ceil,
    Round,
}

// Heap-allocated value types. LuaTable now holds real (Vec-backed) storage —
// previously this was a placeholder unit struct and writes/reads were no-ops,
// causing `print` registration to silently fail in `open_libs`. The rich
// array+hash version in `lua-vm/src/table.rs` is a Phase-D performance
// upgrade target; the simple Vec-pair implementation here is correct for
// Lua semantics and unblocks the runtime.

use std::cell::{Cell, RefCell};

const WEAK_KEYS: u8 = 1 << 0;
const WEAK_VALUES: u8 = 1 << 1;

#[derive(Debug, Default)]
pub struct LuaTable {
    entries: RefCell<Vec<(LuaValue, LuaValue)>>,
    metatable: RefCell<Option<GcRef<LuaTable>>>,
    weak_mode: Cell<u8>,
}

impl LuaTable {
    pub fn placeholder() -> Self { Self::default() }

    /// Read a key; returns `LuaValue::Nil` if absent or if key is nil.
    ///
    /// Weak-table semantics: under Phase D-2, weak entries are pruned at
    /// `Heap::full_collect` time via the reachability-driven post-mark hook
    /// — not eagerly on every read. Between collections a weak entry whose
    /// target has no other strong path is still observable until the next
    /// cycle; that matches C-Lua's stop-the-world atomic-phase clearing.
    pub fn get(&self, k: &LuaValue) -> LuaValue {
        if matches!(k, LuaValue::Nil) { return LuaValue::Nil; }
        for (ek, ev) in self.entries.borrow().iter() {
            if lua_key_eq(ek, k) { return ev.clone(); }
        }
        LuaValue::Nil
    }

    /// Lookup by short-string key (used by metatable __index lookups).
    pub fn get_short_str(&self, k: &GcRef<crate::string::LuaString>) -> LuaValue {
        let key = LuaValue::Str(k.clone());
        self.get(&key)
    }

    pub fn get_str_bytes(&self, key_bytes: &[u8]) -> LuaValue {
        for (k, v) in self.entries.borrow().iter() {
            if let LuaValue::Str(s) = k {
                if s.as_bytes() == key_bytes {
                    return v.clone();
                }
            }
        }
        LuaValue::Nil
    }

    /// Raw set without metamethod dispatch. nil keys are rejected (Lua
    /// semantics: `table[nil] = x` is an error; we silently ignore here
    /// since callers should validate). Setting a value to nil clears the
    /// slot's value but retains the key as a tombstone so that
    /// `next(t, k)` callers iterating while erasing can still locate the
    /// last-yielded key — matching C-Lua's hash-slot semantics.
    pub fn raw_set(&self, k: LuaValue, v: LuaValue) {
        if matches!(k, LuaValue::Nil) { return; }
        let mut entries = self.entries.borrow_mut();
        for i in 0..entries.len() {
            if lua_key_eq(&entries[i].0, &k) {
                entries[i].1 = v;
                return;
            }
        }
        if !matches!(v, LuaValue::Nil) {
            entries.push((k, v));
        }
    }

    /// Returns true if `k` is currently a slot in the entries table,
    /// regardless of whether its value is nil. Used by `next` to
    /// distinguish "key was here (now value=nil)" from "key was never
    /// inserted" — only the latter is an "invalid key to 'next'" error.
    pub fn contains_key(&self, k: &LuaValue) -> bool {
        if matches!(k, LuaValue::Nil) { return false; }
        self.entries.borrow().iter().any(|(ek, _)| lua_key_eq(ek, k))
    }

    pub fn metatable(&self) -> Option<GcRef<LuaTable>> {
        self.metatable.borrow().clone()
    }

    pub fn set_metatable(&self, mt: Option<GcRef<LuaTable>>) {
        let mode = mt.as_ref().map(|t| extract_weak_mode(t)).unwrap_or(0);
        self.weak_mode.set(mode);
        *self.metatable.borrow_mut() = mt;
    }

    pub fn weak_mode(&self) -> u8 { self.weak_mode.get() }

    pub fn len(&self) -> usize { self.entries.borrow().len() }
    pub fn is_empty(&self) -> bool { self.entries.borrow().is_empty() }

    /// Implements Lua's `next(t, k)` for iteration. When `k` is `Nil`,
    /// returns the first entry. Otherwise returns the entry that follows
    /// `k` in insertion order. Returns `None` when iteration is done.
    ///
    /// Tombstone entries (key present, value nil) are skipped — but they
    /// stay in the entries Vec so a `for k,v in pairs(t) do t[k] = nil end`
    /// loop can still resume from the just-cleared key. Weak-table pruning
    /// is no longer performed here; see [`prune_weak_dead`] for the
    /// GC-time weak sweep.
    pub fn next_pair(&self, k: &LuaValue) -> Option<(LuaValue, LuaValue)> {
        let entries = self.entries.borrow();
        let start = if matches!(k, LuaValue::Nil) {
            0
        } else {
            let mut found = None;
            for (i, (ek, _)) in entries.iter().enumerate() {
                if lua_key_eq(ek, k) { found = Some(i + 1); break; }
            }
            found?
        };
        for (ek, ev) in entries[start..].iter() {
            if !matches!(ev, LuaValue::Nil) {
                return Some((ek.clone(), ev.clone()));
            }
        }
        None
    }

    /// Drop weak entries whose weakly-tracked target is unreachable.
    ///
    /// Called from the post-mark hook (`Heap::full_collect_with_post_mark`)
    /// while the GC marker still holds the visited set. `is_reachable(id)`
    /// returns true iff the object at GC identity `id` was reached during
    /// the mark phase. Non-collectable LuaValue variants (`Int`, `Float`,
    /// `Bool`, `Nil`, `LightUserData`, primitive strings owned outside the
    /// heap) are treated as always reachable.
    ///
    /// Mode dispatch (`mode` is the `WEAK_KEYS | WEAK_VALUES` bitmask):
    ///   * `__mode = "v"`: clear when the value side is unreachable.
    ///   * `__mode = "kv"`: clear when either side is unreachable.
    ///   * `__mode = "k"`: clear when the key side is unreachable. NOTE:
    ///     this is a *simplification* of Lua's ephemeron semantics — full
    ///     ephemerons require a fixed-point iteration where a value's
    ///     reachability is conditional on the key's, and vice versa for
    ///     cycles. Here the trace impl marks values strongly regardless
    ///     of key reachability, so values held only by an entry that we
    ///     are about to clear survive one extra cycle. Sufficient for
    ///     `gc.lua`'s weak-table block; full ephemerons remain a follow-up.
    ///
    /// Tombstone semantics (value = Nil with the key slot still occupied)
    /// are preserved: a tombstone is NOT subject to weak-side checks
    /// because callers rely on the key remaining for `next(t, last_key)`.
    pub fn prune_weak_dead(&self, is_reachable: &dyn Fn(usize) -> bool) {
        let mode = self.weak_mode.get();
        if mode == 0 {
            return;
        }
        let weak_k = (mode & WEAK_KEYS) != 0;
        let weak_v = (mode & WEAK_VALUES) != 0;
        let mut entries = self.entries.borrow_mut();
        entries.retain(|(k, v)| {
            if matches!(v, LuaValue::Nil) {
                return true;
            }
            if weak_v && !value_is_reachable(v, is_reachable) {
                return false;
            }
            if weak_k && !value_is_reachable(k, is_reachable) {
                return false;
            }
            true
        });
    }

    /// Ephemeron-convergence helper. For a pure `__mode = "k"` table
    /// (weak keys but NOT weak values), returns the list of values whose
    /// key is currently reachable per `is_reachable`. The caller marks
    /// each returned value via the GC marker — propagating reachability
    /// that depends on the key's reachability, which is the defining
    /// property of ephemerons. Returns an empty Vec for tables that are
    /// not pure weak-key: `"v"` and `"kv"` modes treat values as weak
    /// regardless of key reachability, so they get no convergence boost.
    pub fn ephemeron_values_to_mark(&self, is_reachable: &dyn Fn(usize) -> bool) -> Vec<LuaValue> {
        let mode = self.weak_mode.get();
        if (mode & WEAK_KEYS) == 0 || (mode & WEAK_VALUES) != 0 {
            return Vec::new();
        }
        let entries = self.entries.borrow();
        let mut out = Vec::new();
        for (k, v) in entries.iter() {
            if matches!(v, LuaValue::Nil) {
                continue;
            }
            if value_is_reachable(k, is_reachable) {
                out.push(v.clone());
            }
        }
        out
    }
}

/// Reachability check for a `LuaValue` used by [`LuaTable::prune_weak_dead`].
/// Returns `true` for non-collectable variants (no identity to check) so
/// they survive the weak-table sweep.
fn value_is_reachable(v: &LuaValue, is_reachable: &dyn Fn(usize) -> bool) -> bool {
    match v {
        LuaValue::Table(t) => is_reachable(t.identity()),
        LuaValue::UserData(u) => is_reachable(u.identity()),
        LuaValue::Thread(th) => is_reachable(th.identity()),
        LuaValue::Function(c) => match c {
            LuaClosure::Lua(x) => is_reachable(x.identity()),
            LuaClosure::C(x) => is_reachable(x.identity()),
            LuaClosure::LightC(_) => true,
        },
        LuaValue::Str(s) => is_reachable(s.identity()),
        LuaValue::Nil
        | LuaValue::Bool(_)
        | LuaValue::Int(_)
        | LuaValue::Float(_)
        | LuaValue::LightUserData(_) => true,
    }
}

/// Inspect a metatable's `__mode` field (a string of any combination of
/// 'k' and 'v') and produce the corresponding `WEAK_KEYS | WEAK_VALUES`
/// bitmask. Returns 0 when no `__mode` is set or it is not a string.
fn extract_weak_mode(mt: &LuaTable) -> u8 {
    let entries = mt.entries.borrow();
    for (k, v) in entries.iter() {
        if let LuaValue::Str(ks) = k {
            if ks.as_bytes() == b"__mode" {
                if let LuaValue::Str(vs) = v {
                    let bytes = vs.as_bytes();
                    let mut mode = 0u8;
                    if bytes.iter().any(|b| *b == b'k') { mode |= WEAK_KEYS; }
                    if bytes.iter().any(|b| *b == b'v') { mode |= WEAK_VALUES; }
                    return mode;
                }
                return 0;
            }
        }
    }
    0
}

fn same_rc(a: &LuaValue, b: &LuaValue) -> bool {
    use std::rc::Rc;
    match (a, b) {
        (LuaValue::Table(t1), LuaValue::Table(t2))       => GcRef::ptr_eq(&t1, &t2),
        (LuaValue::UserData(u1), LuaValue::UserData(u2)) => GcRef::ptr_eq(&u1, &u2),
        (LuaValue::Thread(th1), LuaValue::Thread(th2))   => GcRef::ptr_eq(&th1, &th2),
        (LuaValue::Function(c1), LuaValue::Function(c2)) => closure_eq(c1, c2),
        _ => false,
    }
}

fn closure_eq(a: &LuaClosure, b: &LuaClosure) -> bool {
    match (a, b) {
        (LuaClosure::Lua(x), LuaClosure::Lua(y)) => GcRef::ptr_eq(x, y),
        (LuaClosure::C(x), LuaClosure::C(y)) => GcRef::ptr_eq(x, y),
        (LuaClosure::LightC(x), LuaClosure::LightC(y)) => x == y,
        _ => false,
    }
}

/// Key equality for hash-table lookup. Matches Lua semantics:
///   - Nil never equals anything (handled at call sites)
///   - Bool/Int/Float/String compare by value
///   - Int <-> Float compare numerically (Lua coerces)
///   - Table/Function/UserData/Thread compare by GcRef identity
fn lua_key_eq(a: &LuaValue, b: &LuaValue) -> bool {
    match (a, b) {
        (LuaValue::Nil, LuaValue::Nil) => true,
        (LuaValue::Bool(x), LuaValue::Bool(y)) => x == y,
        (LuaValue::Int(x), LuaValue::Int(y)) => x == y,
        (LuaValue::Float(x), LuaValue::Float(y)) => x == y,
        (LuaValue::Int(i), LuaValue::Float(f)) | (LuaValue::Float(f), LuaValue::Int(i)) => *f == *i as f64,
        (LuaValue::Str(x), LuaValue::Str(y)) => x.as_bytes() == y.as_bytes(),
        (LuaValue::Table(x), LuaValue::Table(y)) => GcRef::ptr_eq(x, y),
        (LuaValue::UserData(x), LuaValue::UserData(y)) => GcRef::ptr_eq(x, y),
        (LuaValue::Thread(x), LuaValue::Thread(y)) => GcRef::ptr_eq(x, y),
        (LuaValue::Function(x), LuaValue::Function(y)) => closure_eq(x, y),
        (LuaValue::LightUserData(x), LuaValue::LightUserData(y)) => x == y,
        _ => false,
    }
}

/// Identity of a Lua thread (coroutine).
///
/// The real per-thread `LuaState` lives in `lua-vm` and is held by
/// `GlobalState` keyed by this id. `LuaValue::Thread` carries a
/// `GcRef<LuaThread>` so that pointer-equality of the wrapping `GcRef`
/// still implements thread-identity comparison, but the only payload is
/// the registry key — keeping `LuaState` outside `lua-types` avoids the
/// `lua-types` → `lua-vm` crate cycle.
///
/// Convention: `id == 0` is reserved for the main thread. Coroutines are
/// assigned ids starting at 1.
#[derive(Debug)]
pub struct LuaThread {
    pub id: u64,
}
impl LuaThread {
    pub fn new(id: u64) -> Self { LuaThread { id } }
    pub fn placeholder() -> Self { LuaThread { id: 0 } }
}
