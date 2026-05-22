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
    /// If this table has weak keys or values (`__mode`), any matching entry
    /// whose weakly-tagged component has no other strong holders (Rc
    /// `strong_count == 1`) is removed and `Nil` is returned. This is the
    /// Phase-A/B/C stand-in for the real incremental sweeper.
    pub fn get(&self, k: &LuaValue) -> LuaValue {
        if matches!(k, LuaValue::Nil) { return LuaValue::Nil; }
        let mode = self.weak_mode.get();
        if mode != 0 {
            let entries = self.entries.borrow();
            let found = entries.iter().position(|(ek, _)| lua_key_eq(ek, k));
            if let Some(i) = found {
                let (ek, ev) = &entries[i];
                let dead = entry_is_weakly_dead(ek, ev, mode);
                if dead {
                    drop(entries);
                    self.entries.borrow_mut().swap_remove(i);
                    return LuaValue::Nil;
                }
                return ev.clone();
            }
            return LuaValue::Nil;
        }
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
    /// Skips weakly-dead entries on the way; this keeps `pairs()` over a
    /// weak table from observing collected slots.
    pub fn next_pair(&self, k: &LuaValue) -> Option<(LuaValue, LuaValue)> {
        let mode = self.weak_mode.get();
        if mode != 0 {
            let mut entries = self.entries.borrow_mut();
            entries.retain(|(ek, ev)| !entry_is_weakly_dead(ek, ev, mode));
        }
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

/// True when the entry's weakly-tagged component(s) have no strong
/// references outside this single entry. Strings and other non-collectable
/// values are never considered dead — strings live in the intern pool and
/// would otherwise vanish from weak caches almost immediately.
///
/// The naive form (strong_count == 1) is wrong when an entry stores the
/// *same* Rc in both key and value slots (`a[t] = t`): the in-entry slots
/// then contribute 2 strong refs, and we must also distinguish slots that
/// keep the target alive (strong) from slots that do not (weak per the
/// table's `__mode`).
fn entry_is_weakly_dead(k: &LuaValue, v: &LuaValue, mode: u8) -> bool {
    let weak_k = (mode & WEAK_KEYS) != 0;
    let weak_v = (mode & WEAK_VALUES) != 0;
    if weak_k && is_dead_side(k, k, v, weak_k, weak_v) {
        return true;
    }
    if weak_v && is_dead_side(v, k, v, weak_k, weak_v) {
        return true;
    }
    false
}

fn is_dead_side(
    target: &LuaValue,
    k: &LuaValue,
    v: &LuaValue,
    weak_k: bool,
    weak_v: bool,
) -> bool {
    let total = match strong_count_of(target) {
        Some(n) => n,
        None => return false,
    };
    let key_slot_refs_target = if same_rc(target, k) { 1usize } else { 0 };
    let value_slot_refs_target = if same_rc(target, v) { 1usize } else { 0 };
    let strong_from_key_slot = if !weak_k { key_slot_refs_target } else { 0 };
    let strong_from_value_slot = if !weak_v { value_slot_refs_target } else { 0 };
    if strong_from_key_slot + strong_from_value_slot > 0 {
        return false;
    }
    let in_entry = key_slot_refs_target + value_slot_refs_target;
    total <= in_entry
}

fn strong_count_of(v: &LuaValue) -> Option<usize> {
    use std::rc::Rc;
    match v {
        LuaValue::Table(t)    => Some(t.strong_count()),
        LuaValue::UserData(u) => Some(u.strong_count()),
        LuaValue::Thread(th)  => Some(th.strong_count()),
        LuaValue::Function(c) => match c {
            LuaClosure::Lua(x)    => Some(x.strong_count()),
            LuaClosure::C(x)      => Some(x.strong_count()),
            LuaClosure::LightC(_) => None,
        },
        _ => None,
    }
}

/// Cross-table weak-sweep used at `collectgarbage("collect")` time.
///
/// The per-table `entry_is_weakly_dead` check called from `get`/`next_pair`
/// undercounts when the same target is held weakly across multiple tables
/// (the "bug-in-5.1" case in `testes/gc.lua`): each call inspects only one
/// entry and treats the other table's weak Rc as if it were strong.
///
/// This routine iterates the full list of currently-registered weak tables
/// and removes entries whose weak target is held only by weak slots —
/// counted across *all* tables in the list. Iterates until the sweep makes
/// no further progress so that chain-clears (clearing one entry exposes
/// another) settle to a fixed point.
pub fn sweep_weak_tables(tables: &[GcRef<LuaTable>]) {
    if tables.is_empty() {
        return;
    }
    loop {
        let mut any_removed = false;
        for t in tables {
            let mode = t.weak_mode.get();
            if mode == 0 {
                continue;
            }
            let dead_indices: Vec<usize> = {
                let entries = t.entries.borrow();
                let mut result = Vec::new();
                for (i, (k, v)) in entries.iter().enumerate() {
                    // PORT NOTE: tombstone entries (key present, value Nil)
                    // are left behind by `raw_set(k, Nil)` so that
                    // `next(t, k)` can locate the removed key during
                    // iteration. C-Lua's collector clears them in the
                    // atomic weak-sweep phase. Without this, a string key
                    // held only by a tombstone in a weak table would never
                    // be released (the per-entry `entry_is_weakly_dead`
                    // check returns false for strings), and the
                    // `collectgarbage("count") <= m + 1` assert at the end
                    // of gc.lua's weak-string-keys block would fail. Tables
                    // not in `weak_tables_registry` are not touched here so
                    // mid-iteration `for k,v in pairs(t) do t[k] = nil end`
                    // semantics on non-weak tables are preserved.
                    if matches!(v, LuaValue::Nil) {
                        result.push(i);
                        continue;
                    }
                    if entry_is_weakly_dead_global(k, v, mode, tables) {
                        result.push(i);
                    }
                }
                result
            };
            if !dead_indices.is_empty() {
                let mut entries = t.entries.borrow_mut();
                for &i in dead_indices.iter().rev() {
                    entries.swap_remove(i);
                }
                drop(entries);
                any_removed = true;
            }
        }
        if !any_removed {
            break;
        }
    }
}

fn entry_is_weakly_dead_global(
    k: &LuaValue,
    v: &LuaValue,
    mode: u8,
    all_tables: &[GcRef<LuaTable>],
) -> bool {
    let weak_k = (mode & WEAK_KEYS) != 0;
    let weak_v = (mode & WEAK_VALUES) != 0;
    if weak_k && is_target_dead_global(k, all_tables) {
        return true;
    }
    if weak_v && is_target_dead_global(v, all_tables) {
        return true;
    }
    false
}

fn is_target_dead_global(target: &LuaValue, all_tables: &[GcRef<LuaTable>]) -> bool {
    is_target_dead_global_with_fdos(target, all_tables, &[])
}

/// Same as `is_target_dead_global` but additionally treats `fdos` (finalizable-
/// dead objects: tables in `pending_finalizers` whose only strong ref is the
/// queue itself) as if they were already dropped — so any strong ref the
/// target receives from an FDO's entries or metatable is discounted.
fn is_target_dead_global_with_fdos(
    target: &LuaValue,
    all_tables: &[GcRef<LuaTable>],
    fdos: &[GcRef<LuaTable>],
) -> bool {
    let total = match strong_count_of(target) {
        Some(n) => n,
        None => return false,
    };
    let mut extra_rc = 0usize;
    if let LuaValue::Table(target_tbl) = target {
        for t in all_tables {
            if GcRef::ptr_eq(target_tbl, t) {
                extra_rc += 1;
            }
        }
        for f in fdos {
            if GcRef::ptr_eq(target_tbl, f) {
                extra_rc += 1;
            }
        }
    }
    let mut weak_refs = 0usize;
    for t in all_tables {
        let tmode = t.weak_mode.get();
        if tmode == 0 {
            continue;
        }
        let tweak_k = (tmode & WEAK_KEYS) != 0;
        let tweak_v = (tmode & WEAK_VALUES) != 0;
        for (ek, ev) in t.entries.borrow().iter() {
            if tweak_k && same_rc(target, ek) {
                weak_refs += 1;
            }
            if tweak_v && same_rc(target, ev) {
                weak_refs += 1;
            }
        }
    }
    let mut fdo_refs = 0usize;
    for f in fdos {
        for (fk, fv) in f.entries.borrow().iter() {
            if same_rc(target, fk) {
                fdo_refs += 1;
            }
            if same_rc(target, fv) {
                fdo_refs += 1;
            }
        }
        if let Some(mt) = f.metatable() {
            if let LuaValue::Table(target_tbl) = target {
                if GcRef::ptr_eq(&mt, target_tbl) {
                    fdo_refs += 1;
                }
            }
        }
    }
    let strong = total
        .saturating_sub(extra_rc)
        .saturating_sub(weak_refs)
        .saturating_sub(fdo_refs);
    strong == 0
}

/// Pre-finalizer weak-**value** sweep.
///
/// Clears entries in weak-value tables whose value is reachable only through
/// finalizable-dead objects (`fdos`) and other weak slots. Weak-key entries
/// are NOT touched here — those are cleared by the post-finalizer
/// [`sweep_weak_tables`] pass, by which point the FDOs have been dropped.
///
/// PORT NOTE: mirrors the order in C-Lua's `atomic()`:
///   `clearbyvalues(g, g->weak, NULL)` — runs **before** `markbeingfnz`
///   resurrects finalizable objects' transitive closure. The "bug-in-5.1"
///   testes/gc.lua case (`a.x = t; setmetatable(a, {__gc = … assert(C.key == nil)})`)
///   requires this ordering: `C.key` (a weak value pointing at `t`) must be
///   cleared before `a`'s finalizer runs, even though `a.x = t` keeps `t`
///   strongly reachable through the finalizable `a`.
pub fn sweep_weak_values_pre_finalizer(
    weak_tables: &[GcRef<LuaTable>],
    fdos: &[GcRef<LuaTable>],
) {
    if weak_tables.is_empty() || fdos.is_empty() {
        return;
    }
    loop {
        let mut any_removed = false;
        for t in weak_tables {
            let mode = t.weak_mode.get();
            if (mode & WEAK_VALUES) == 0 {
                continue;
            }
            let dead_indices: Vec<usize> = {
                let entries = t.entries.borrow();
                let mut result = Vec::new();
                for (i, (_, v)) in entries.iter().enumerate() {
                    if is_target_dead_global_with_fdos(v, weak_tables, fdos) {
                        result.push(i);
                    }
                }
                result
            };
            if !dead_indices.is_empty() {
                let mut entries = t.entries.borrow_mut();
                for &i in dead_indices.iter().rev() {
                    entries.swap_remove(i);
                }
                drop(entries);
                any_removed = true;
            }
        }
        if !any_removed {
            break;
        }
    }
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

#[derive(Debug)]
pub struct LuaThread {
    _private: (),
}
impl LuaThread {
    pub fn placeholder() -> Self { LuaThread { _private: () } }
}
