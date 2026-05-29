//! Lua table implementation (array + hash hybrid).
//!
//! Canonical port of `reference/lua-5.4.7/src/ltable.c`. Lives in
//! `lua-types` because `LuaValue::Table(GcRef<LuaTable>)` is defined here
//! and the table storage must be reachable without depending on
//! `lua-vm`. The crate `lua_unsafe = "forbid"` lint is preserved.
//!
//! # Interior mutability
//!
//! `GcRef<T>` only yields `&T` on deref, so the mutable algorithms in
//! C-Lua's `ltable.c` (which write through `Table *`) must operate
//! through a `RefCell`. The split is:
//!
//! * `LuaTable` — outer handle. All public methods take `&self`.
//! * `TableInner` — storage + algorithms. All mutating methods are
//!   `&mut TableInner` and are reached via `inner.borrow_mut()`.
//!
//! The hash part uses Brent's variation of chained scatter tables.
//! The key invariant: if an element is not in its *main position*
//! (the slot its hash maps to), the colliding element *is* in its
//! own main position.

use std::cell::{Cell, RefCell};

use crate::closure::LuaClosure;
use crate::error::LuaError;
use crate::gc::GcRef;
use crate::string::LuaString;
use crate::value::LuaValue;

// ── Constants ─────────────────────────────────────────────────────────────────

/// Largest `k` such that `2^k` fits in a signed `i32`.
const MAXABITS: u32 = (std::mem::size_of::<i32>() as u32) * 8 - 1;

/// Maximum size of the array part.
pub const MAXASIZE: u32 = 1u32 << MAXABITS;

/// Largest `k` such that `2^k` fits in a signed `i32` minus one (hash part).
pub const MAXHBITS: u32 = MAXABITS - 1;

/// Maximum size of the hash part (power-of-2 count of nodes).
const MAXHSIZE: u32 = 1u32 << MAXHBITS;

/// Minimum hash node count when lazily materializing a brand-new dummy table
/// on first non-array key insertion.
///
/// In workloads that create many tiny record-like tables (`binarytrees`),
/// a dummy→rehash path on every first insert adds avoidable overhead.
const DUMMY_TABLE_INIT_HASH_NODES: u32 = 4;

/// Bit 7 of `TableFlags`: when set, `alimit` is NOT the real array size.
const BIT_RAS: u8 = 1 << 7;

/// Soft cap on array growth in a single `raw_set` call.
///
/// Prevents pathological inserts of a far-away integer key (e.g.
/// `t[1<<30] = 1`) from allocating gigabytes when an array slot would
/// be the wrong choice; falls back to the hash part for keys above the
/// cap. Matches C-Lua's behavioural bound, which is governed by
/// `MAXASIZE` and the rehash density heuristic.
pub const ARRAY_GROW_CAP: u32 = 1u32 << 20;

/// Cap on total entries (array + hash). Growing a table past this with a
/// fresh key raises `LuaError::Memory` (pcall reports `"not enough memory"`),
/// emulating C-Lua's `malloc`-NULL termination of an unbounded
/// `for i = 1, math.huge do a[i] = ... end` loop.
///
/// Ideally this would be a byte budget rather than an entry count, but the
/// GC's byte counter does not track a table's internal `Vec` capacity, so a
/// byte budget cannot see table growth. Until table memory is GC-accounted,
/// this stays an entry cap, sized to comfortably hold realistic large tables
/// (a 10M-element array, issue #37) while keeping the unbounded-loop stress
/// tests terminating within the harness time and memory limits.
pub const TOTAL_GROW_CAP: usize = 1usize << 24;

const WEAK_KEYS: u8 = 1 << 0;
const WEAK_VALUES: u8 = 1 << 1;

// ── TableFlags ─────────────────────────────────────────────────────────────────

/// Bitfield for a [`LuaTable`]: lower bits record absent fast-access
/// metamethods; bit 7 encodes whether `alimit` is the real array size.
#[derive(Clone, Copy, Debug, Default)]
pub struct TableFlags(pub u8);

impl TableFlags {
    /// `isrealasize(t)` — bit 7 clear means alimit IS the real array size.
    #[inline]
    pub fn is_real_asize(self) -> bool {
        (self.0 & BIT_RAS) == 0
    }

    /// `setrealasize(t)` — clear bit 7 so alimit becomes the canonical size.
    #[inline]
    pub fn set_real_asize(&mut self) {
        self.0 &= !BIT_RAS;
    }

    /// `setnorealasize(t)` — set bit 7 so alimit is only a hint.
    #[inline]
    pub fn set_no_real_asize(&mut self) {
        self.0 |= BIT_RAS;
    }

    /// `invalidateTMcache(t)` — clear all fast-access metamethod bits.
    #[inline]
    pub fn invalidate_tm_cache(&mut self) {
        const MASK_FLAGS: u8 = 0x7F;
        self.0 &= !MASK_FLAGS;
    }
}

// ── TableNode ──────────────────────────────────────────────────────────────────

/// One node in a table's hash part.
///
/// signed offset into the same node vector.
pub struct TableNode {
    /// Value stored at this key.  C: `gval(n)`.
    pub value: LuaValue,
    /// Key stored in this node.  C: `n->u.key_val` + `n->u.key_tt`.
    pub key: LuaValue,
    /// Collision-chain offset (positive or negative; zero means end of chain).
    pub next: i32,
}

impl TableNode {
    fn empty() -> Self {
        TableNode { value: LuaValue::Nil, key: LuaValue::Nil, next: 0 }
    }

    fn key_is_nil(&self) -> bool { matches!(self.key, LuaValue::Nil) }
    fn key_is_int(&self) -> bool { matches!(self.key, LuaValue::Int(_)) }
    fn key_int(&self) -> i64 {
        if let LuaValue::Int(i) = self.key { i }
        else { panic!("TableNode::key_int: key is not int") }
    }
    fn key_is_short_str(&self) -> bool {
        if let LuaValue::Str(s) = &self.key { s.is_short() }
        else { false }
    }
    fn key_string(&self) -> &GcRef<LuaString> {
        if let LuaValue::Str(s) = &self.key { s }
        else { panic!("TableNode::key_string: key is not a string") }
    }
    fn key_value(&self) -> LuaValue { self.key.clone() }
    fn set_key(&mut self, k: &LuaValue) { self.key = k.clone(); }
}

// ── TableSlotRef ───────────────────────────────────────────────────────────────

/// Internal slot reference returned by the "get" family of functions.
///
/// Replaces C's `const TValue *` pattern, which may point into either
/// the array part, the hash part, or the static `absentkey` sentinel.
#[derive(Debug, Clone, Copy)]
pub enum TableSlotRef {
    /// Key lives in the array part at this 0-based index.
    Array(usize),
    /// Key lives in the hash part at this 0-based node index.
    Hash(usize),
    /// Key is absent from the table (C: `&absentkey`).
    Absent,
}

// ── ceil_log2 ─────────────────────────────────────────────────────────────────

/// Computes `ceil(log2(x))`; returns the minimum `k` such that `2^k >= x`.
fn ceil_log2(x: u32) -> i32 {
    static LOG_2: [u8; 256] = [
        0,1,2,2,3,3,3,3,4,4,4,4,4,4,4,4,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,
        6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,
        7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,
        7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,
        8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,
        8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,
        8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,
        8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,
    ];
    let mut l: i32 = 0;
    let mut x = x.wrapping_sub(1);
    while x >= 256 { l += 8; x >>= 8; }
    l + LOG_2[x as usize] as i32
}

// ── float hash (frexp-based) ──────────────────────────────────────────────────

/// Hash a `f64` to an `i32` bucket index.
///
/// Uses `frexp` decomposition to produce a well-distributed integer hash.
/// Handles inf/NaN by returning 0.
///
fn hash_float(n: f64) -> i32 {
    if n.is_nan() || n.is_infinite() {
        return 0;
    }
    let (mantissa, exp) = frexp(n);
    let scaled = mantissa * -(i32::MIN as f64);
    let ni = scaled as i64;
    if ni as f64 != scaled {
        return 0;
    }
    let u = (exp as u32).wrapping_add(ni as u32);
    if u <= i32::MAX as u32 { u as i32 } else { !(u as i32) }
}

/// Decompose `x` into mantissa ∈ `[0.5, 1)` and integer exponent.
fn frexp(x: f64) -> (f64, i32) {
    if x == 0.0 || x.is_nan() || x.is_infinite() {
        return (x, 0);
    }
    let bits = x.to_bits();
    let exp_bits = ((bits >> 52) & 0x7FFu64) as i32;
    if exp_bits == 0 {
        let scaled = x * (2.0f64.powi(64));
        let (m, e) = frexp(scaled);
        return (m, e - 64);
    }
    let exp = exp_bits - 1022;
    let mantissa_bits = (bits & !(0x7FFu64 << 52)) | (0x3FEu64 << 52);
    (f64::from_bits(mantissa_bits), exp)
}

// ── TableInner ─────────────────────────────────────────────────────────────────

/// Hybrid array + hash storage backing a [`LuaTable`].
///
/// All mutating algorithms live as `&mut TableInner` methods so they
/// can be called from the outer `&self` API via `RefCell::borrow_mut`.
pub struct TableInner {
    pub flags: TableFlags,
    pub lsizenode: u8,
    pub alimit: u32,
    pub array: Vec<LuaValue>,
    pub node: Vec<TableNode>,
    pub lastfree: Option<usize>,
}

impl TableInner {
    fn new() -> Self {
        TableInner {
            flags: TableFlags(0x7F),
            lsizenode: 0,
            alimit: 0,
            array: Vec::new(),
            node: Vec::new(),
            lastfree: None,
        }
    }

    /// `isdummy(t)` — true when the table has no allocated hash part.
    #[inline]
    fn is_dummy(&self) -> bool { self.lastfree.is_none() }

    /// `sizenode(t)` — nominal hash-part capacity (`1 << lsizenode`).
    #[inline]
    fn sizenode(&self) -> u32 { 1u32 << self.lsizenode }

    /// `allocsizenode(t)` — 0 when dummy, else `1 << lsizenode`.
    #[inline]
    fn alloc_sizenode(&self) -> u32 {
        if self.is_dummy() { 0 } else { self.sizenode() }
    }

    /// `isrealasize(t)` accessor.
    #[inline]
    fn is_real_asize(&self) -> bool { self.flags.is_real_asize() }

    /// `ispow2(x)` — C treats 0 as a power of two.
    #[inline]
    fn is_pow2(x: u32) -> bool { x == 0 || x.is_power_of_two() }

    /// Returns the real size of the array part. C: `luaH_realasize`.
    fn real_asize(&self) -> u32 {
        if self.limit_equals_asize() {
            return self.alimit;
        }
        let mut size = self.alimit;
        size |= size >> 1;
        size |= size >> 2;
        size |= size >> 4;
        size |= size >> 8;
        size |= size >> 16;
        size = size.wrapping_add(1);
        debug_assert!(
            Self::is_pow2(size) && size / 2 < self.alimit && self.alimit < size
        );
        size
    }

    #[inline]
    fn limit_equals_asize(&self) -> bool {
        self.is_real_asize() || Self::is_pow2(self.alimit)
    }

    fn is_pow2_real_asize(&self) -> bool {
        !self.is_real_asize() || Self::is_pow2(self.alimit)
    }

    fn set_limit_to_size(&mut self) -> u32 {
        self.alimit = self.real_asize();
        self.flags.set_real_asize();
        self.alimit
    }

    // ── Hash helper functions ──────────────────────────────────────────────

    fn hash_idx_for_int(&self, i: i64) -> usize {
        let ui = i as u64;
        let sn = self.sizenode() as usize;
        let modulo = (sn - 1) | 1;
        if ui <= i32::MAX as u64 {
            (ui as usize) % modulo
        } else {
            (ui as usize) % modulo
        }
    }

    #[inline]
    fn hashpow2_idx(&self, h: u32) -> usize {
        (h & (self.sizenode() - 1)) as usize
    }

    #[inline]
    fn hashmod_idx(&self, h: usize) -> usize {
        let sn = self.sizenode() as usize;
        let modulo = (sn - 1) | 1;
        h % modulo
    }

    fn main_position(&self, key: &LuaValue) -> usize {
        match key {
            LuaValue::Int(i) => self.hash_idx_for_int(*i),
            LuaValue::Float(f) => {
                let h = hash_float(*f);
                self.hashmod_idx(h as usize)
            }
            LuaValue::Str(s) if s.is_short() => self.hashpow2_idx(s.hash()),
            LuaValue::Str(s) => self.hashpow2_idx(s.hash()),
            LuaValue::Bool(false) => self.hashpow2_idx(0),
            LuaValue::Bool(true) => self.hashpow2_idx(1),
            LuaValue::LightUserData(p) => {
                let h = (*p as usize as u32) as usize;
                self.hashmod_idx(h)
            }
            LuaValue::Function(LuaClosure::LightC(f)) => {
                let h = (*f as u32) as usize;
                self.hashmod_idx(h)
            }
            LuaValue::Table(t) => {
                let h = (GcRef::identity(t) as u32) as usize;
                self.hashmod_idx(h)
            }
            LuaValue::Function(LuaClosure::Lua(cl)) => {
                let h = (GcRef::identity(cl) as u32) as usize;
                self.hashmod_idx(h)
            }
            LuaValue::Function(LuaClosure::C(cl)) => {
                let h = (GcRef::identity(cl) as u32) as usize;
                self.hashmod_idx(h)
            }
            LuaValue::UserData(u) => {
                let h = (GcRef::identity(u) as u32) as usize;
                self.hashmod_idx(h)
            }
            LuaValue::Thread(th) => {
                let h = (GcRef::identity(th) as u32) as usize;
                self.hashmod_idx(h)
            }
            LuaValue::Nil => 0,
        }
    }

    fn main_position_from_node(&self, nd: usize) -> usize {
        let key = self.node[nd].key_value();
        self.main_position(&key)
    }

    // ── Key equality ───────────────────────────────────────────────────────

    fn equal_key(k1: &LuaValue, n2: &TableNode) -> bool {
        let types_match = std::mem::discriminant(k1) == std::mem::discriminant(&n2.key);
        if !types_match {
            return false;
        }
        match &n2.key {
            LuaValue::Nil => true,
            LuaValue::Bool(b) => matches!(k1, LuaValue::Bool(b2) if b == b2),
            LuaValue::Int(ni) => matches!(k1, LuaValue::Int(ki) if ki == ni),
            LuaValue::Float(nf) => matches!(k1, LuaValue::Float(kf) if kf == nf),
            LuaValue::LightUserData(np) => matches!(k1, LuaValue::LightUserData(kp) if kp == np),
            LuaValue::Function(LuaClosure::LightC(nf)) => {
                matches!(k1, LuaValue::Function(LuaClosure::LightC(kf)) if kf == nf)
            }
            LuaValue::Str(ns) if ns.is_long() => {
                if let LuaValue::Str(ks) = k1 {
                    ks.as_bytes() == ns.as_bytes()
                } else { false }
            }
            _ => Self::gc_ptr_eq(k1, &n2.key),
        }
    }

    fn gc_ptr_eq(a: &LuaValue, b: &LuaValue) -> bool {
        match (a, b) {
            (LuaValue::Str(sa), LuaValue::Str(sb)) => GcRef::ptr_eq(sa, sb),
            (LuaValue::Table(ta), LuaValue::Table(tb)) => GcRef::ptr_eq(ta, tb),
            (LuaValue::Function(LuaClosure::Lua(fa)), LuaValue::Function(LuaClosure::Lua(fb))) => {
                GcRef::ptr_eq(fa, fb)
            }
            (LuaValue::Function(LuaClosure::C(fa)), LuaValue::Function(LuaClosure::C(fb))) => {
                GcRef::ptr_eq(fa, fb)
            }
            (LuaValue::UserData(ua), LuaValue::UserData(ub)) => GcRef::ptr_eq(ua, ub),
            (LuaValue::Thread(ta), LuaValue::Thread(tb)) => GcRef::ptr_eq(ta, tb),
            _ => false,
        }
    }

    // ── Generic hash-part lookup ───────────────────────────────────────────

    fn get_generic_slot(&self, key: &LuaValue) -> TableSlotRef {
        if self.is_dummy() { return TableSlotRef::Absent; }
        let mut n = self.main_position(key);
        loop {
            if Self::equal_key(key, &self.node[n]) {
                return TableSlotRef::Hash(n);
            }
            let nx = self.node[n].next;
            if nx == 0 { return TableSlotRef::Absent; }
            n = (n as isize + nx as isize) as usize;
        }
    }

    // ── arrayindex / findindex ─────────────────────────────────────────────

    fn array_index(k: i64) -> u32 {
        let uk = k as u64;
        if uk.wrapping_sub(1) < MAXASIZE as u64 { k as u32 } else { 0 }
    }

    /// Find the linear traversal position of `key`. Returns 0 for `Nil`
    /// (first iteration). Errors with `"invalid key to 'next'"` when
    /// the key is non-nil and not present in the table.
    fn find_index(&self, key: &LuaValue, asize: u32) -> Result<u32, LuaError> {
        if matches!(key, LuaValue::Nil) { return Ok(0); }
        let i = if let LuaValue::Int(k) = key { Self::array_index(*k) } else { 0 };
        if i.wrapping_sub(1) < asize { return Ok(i); }
        let slot = self.get_generic_slot(key);
        match slot {
            TableSlotRef::Absent => {
                Err(LuaError::runtime(format_args!("invalid key to 'next'")))
            }
            TableSlotRef::Hash(node_idx) => Ok((node_idx as u32 + 1) + asize),
            TableSlotRef::Array(_) => unreachable!("getgeneric returned Array slot"),
        }
    }

    /// Iteration step: given a key (`Nil` for first call), return the
    /// next `(key, value)` pair in C-Lua's array-then-hash order.
    fn next_pair(&self, key: &LuaValue) -> Result<Option<(LuaValue, LuaValue)>, LuaError> {
        let asize = self.real_asize();
        let i = self.find_index(key, asize)?;
        let mut i = i as usize;
        while i < asize as usize {
            if !matches!(self.array[i], LuaValue::Nil) {
                return Ok(Some((LuaValue::Int((i + 1) as i64), self.array[i].clone())));
            }
            i += 1;
        }
        let mut hi = i.saturating_sub(asize as usize);
        while hi < self.node.len() {
            if !matches!(self.node[hi].value, LuaValue::Nil) {
                return Ok(Some((self.node[hi].key_value(), self.node[hi].value.clone())));
            }
            hi += 1;
        }
        Ok(None)
    }

    // ── Rehash helpers ─────────────────────────────────────────────────────

    fn compute_sizes(nums: &[u32], pna: &mut u32) -> u32 {
        let mut twotoi: u32 = 1;
        let mut a: u32 = 0;
        let mut na: u32 = 0;
        let mut optimal: u32 = 0;
        for i in 0..nums.len() {
            if twotoi == 0 || *pna <= twotoi / 2 { break; }
            a += nums[i];
            if a > twotoi / 2 {
                optimal = twotoi;
                na = a;
            }
            twotoi = twotoi.wrapping_mul(2);
        }
        debug_assert!(optimal == 0 || optimal / 2 < na && na <= optimal);
        *pna = na;
        optimal
    }

    fn count_int(key: i64, nums: &mut [u32]) -> bool {
        let k = Self::array_index(key);
        if k != 0 {
            nums[ceil_log2(k) as usize] += 1;
            true
        } else { false }
    }

    fn num_use_array(&self, nums: &mut [u32]) -> u32 {
        debug_assert!(self.is_real_asize(), "numusearray: alimit must be real size");
        let asize = self.alimit as usize;
        let mut ause: u32 = 0;
        let mut i: usize = 1;
        let mut ttlg: usize = 1;
        for lg in 0..=(MAXABITS as usize) {
            let mut lc: u32 = 0;
            let lim = if ttlg > asize { asize } else { ttlg };
            if i > lim { break; }
            while i <= lim {
                if !matches!(self.array[i - 1], LuaValue::Nil) { lc += 1; }
                i += 1;
            }
            nums[lg] += lc;
            ause += lc;
            ttlg = ttlg.saturating_mul(2);
        }
        ause
    }

    fn num_use_hash(&self, nums: &mut [u32], pna: &mut u32) -> i32 {
        let mut totaluse: i32 = 0;
        let mut ause: u32 = 0;
        let mut i = self.node.len();
        while i > 0 {
            i -= 1;
            let n = &self.node[i];
            if !matches!(n.value, LuaValue::Nil) {
                if n.key_is_int() {
                    if Self::count_int(n.key_int(), nums) { ause += 1; }
                }
                totaluse += 1;
            }
        }
        *pna += ause;
        totaluse
    }

    fn set_node_vector(&mut self, size: u32) -> Result<(), LuaError> {
        if size == 0 {
            self.node = Vec::new();
            self.lsizenode = 0;
            self.lastfree = None;
        } else {
            let lsize = ceil_log2(size);
            if lsize as u32 > MAXHBITS || (1u32 << lsize) > MAXHSIZE {
                return Err(LuaError::runtime(format_args!("table overflow")));
            }
            let actual_size = 1u32 << lsize;
            let mut nodes = Vec::with_capacity(actual_size as usize);
            for _ in 0..actual_size { nodes.push(TableNode::empty()); }
            self.node = nodes;
            self.lsizenode = lsize as u8;
            self.lastfree = Some(actual_size as usize);
        }
        Ok(())
    }

    fn reinsert(&mut self, old_nodes: Vec<(LuaValue, LuaValue)>) -> Result<(), LuaError> {
        for (k, v) in old_nodes {
            self.set(&k, v)?;
        }
        Ok(())
    }

    /// Resize the table to new array and hash sizes.
    fn resize(&mut self, new_asize: u32, nhsize: u32) -> Result<(), LuaError> {
        let old_asize = self.set_limit_to_size();

        let (mut new_hash_node, mut new_hash_lsize, mut new_hash_lastfree) = {
            let mut tmp = TableInner::new();
            tmp.set_node_vector(nhsize)?;
            (tmp.node, tmp.lsizenode, tmp.lastfree)
        };

        if new_asize < old_asize {
            self.alimit = new_asize;
            std::mem::swap(&mut self.node, &mut new_hash_node);
            std::mem::swap(&mut self.lsizenode, &mut new_hash_lsize);
            std::mem::swap(&mut self.lastfree, &mut new_hash_lastfree);

            for i in (new_asize as usize)..(old_asize as usize) {
                if !matches!(self.array[i], LuaValue::Nil) {
                    let v = self.array[i].clone();
                    self.set_int((i + 1) as i64, v)?;
                }
            }

            self.alimit = old_asize;
            std::mem::swap(&mut self.node, &mut new_hash_node);
            std::mem::swap(&mut self.lsizenode, &mut new_hash_lsize);
            std::mem::swap(&mut self.lastfree, &mut new_hash_lastfree);
        }

        self.array.resize_with(new_asize as usize, || LuaValue::Nil);

        std::mem::swap(&mut self.node, &mut new_hash_node);
        std::mem::swap(&mut self.lsizenode, &mut new_hash_lsize);
        std::mem::swap(&mut self.lastfree, &mut new_hash_lastfree);
        self.alimit = new_asize;

        let old_hash_entries: Vec<(LuaValue, LuaValue)> = new_hash_node
            .iter()
            .filter(|n| !matches!(n.value, LuaValue::Nil))
            .map(|n| (n.key_value(), n.value.clone()))
            .collect();
        drop(new_hash_node);
        self.reinsert(old_hash_entries)?;

        Ok(())
    }

    fn rehash(&mut self, extra_key: &LuaValue) -> Result<(), LuaError> {
        let mut nums = [0u32; MAXABITS as usize + 1];
        self.set_limit_to_size();

        let na = self.num_use_array(&mut nums);
        let mut na = na;
        let mut totaluse = na as i32;

        totaluse += self.num_use_hash(&mut nums, &mut na);

        if let LuaValue::Int(ek) = extra_key {
            if Self::count_int(*ek, &mut nums) { na += 1; }
        }
        totaluse += 1;

        let asize = Self::compute_sizes(&nums, &mut na);

        let nh = (totaluse - na as i32).max(0) as u32;
        self.resize(asize, nh)
    }

    fn get_free_pos(&mut self) -> Option<usize> {
        if self.is_dummy() { return None; }
        loop {
            let lf = self.lastfree?;
            if lf == 0 {
                self.lastfree = None;
                return None;
            }
            let idx = lf - 1;
            self.lastfree = Some(idx);
            if self.node[idx].key_is_nil() {
                return Some(idx);
            }
        }
    }

    fn find_chain_predecessor(&self, idx: usize) -> Option<usize> {
        self.node.iter().enumerate().find(|(prev, node)| {
            node.next != 0 && (*prev as isize + node.next as isize) == idx as isize
        }).map(|(prev, _)| prev)
    }

    fn clear_node(&mut self, idx: usize) {
        self.node[idx].key = LuaValue::Nil;
        self.node[idx].value = LuaValue::Nil;
        self.node[idx].next = 0;
    }

    fn remove_hash_node(&mut self, idx: usize) {
        if let Some(prev) = self.find_chain_predecessor(idx) {
            let next = self.node[idx].next;
            self.node[prev].next = if next == 0 {
                0
            } else {
                let target = idx as isize + next as isize;
                (target - prev as isize) as i32
            };
            self.clear_node(idx);
            return;
        }

        let next = self.node[idx].next;
        if next == 0 {
            self.clear_node(idx);
            return;
        }

        let next_idx = (idx as isize + next as isize) as usize;
        let moved_next = self.node[next_idx].next;
        let moved_key = self.node[next_idx].key_value();
        let moved_value = self.node[next_idx].value.clone();
        self.node[idx].key = moved_key;
        self.node[idx].value = moved_value;
        self.node[idx].next = if moved_next == 0 {
            0
        } else {
            let target = next_idx as isize + moved_next as isize;
            (target - idx as isize) as i32
        };
        self.clear_node(next_idx);
    }

    fn clear_dead_hash_node(&mut self, idx: usize) {
        self.remove_hash_node(idx);
    }

    fn new_key(&mut self, key: &LuaValue, value: LuaValue) -> Result<(), LuaError> {
        if matches!(key, LuaValue::Nil) {
            return Err(LuaError::runtime(format_args!("table index is nil")));
        }
        let normalised_key;
        let key = if let LuaValue::Float(f) = key {
            let f = *f;
            if f.is_nan() {
                return Err(LuaError::runtime(format_args!("table index is NaN")));
            }
            let k = f as i64;
            if k as f64 == f {
                normalised_key = LuaValue::Int(k);
                &normalised_key
            } else { key }
        } else { key };

        if matches!(value, LuaValue::Nil) { return Ok(()); }

        if self.is_dummy() && !matches!(key, LuaValue::Int(_)) {
            self.set_node_vector(DUMMY_TABLE_INIT_HASH_NODES)?;
            let mp = self.main_position(key);
            self.node[mp].set_key(key);
            self.node[mp].value = value;
            return Ok(());
        }

        let mp = self.main_position(key);
        let mp_occupied = self.is_dummy() || !matches!(self.node[mp].value, LuaValue::Nil);
        if mp_occupied {
            let f = self.get_free_pos();
            let f = match f {
                None => {
                    self.rehash(key)?;
                    return self.set(key, value);
                }
                Some(idx) => idx,
            };

            debug_assert!(!self.is_dummy());
            let othern = self.main_position_from_node(mp);

            if othern != mp {
                let mut prev = othern;
                while (prev as isize + self.node[prev].next as isize) as usize != mp {
                    prev = (prev as isize + self.node[prev].next as isize) as usize;
                }
                self.node[prev].next = (f as isize - prev as isize) as i32;
                let mp_key = self.node[mp].key_value();
                let mp_val = self.node[mp].value.clone();
                let mp_next = self.node[mp].next;
                self.node[f].key = mp_key;
                self.node[f].value = mp_val;
                if mp_next != 0 {
                    self.node[f].next = mp_next + (mp as isize - f as isize) as i32;
                    self.node[mp].next = 0;
                } else {
                    self.node[f].next = 0;
                }
                self.node[mp].value = LuaValue::Nil;
            } else {
                if self.node[mp].next != 0 {
                    let target = (mp as isize + self.node[mp].next as isize) as usize;
                    self.node[f].next = (target as isize - f as isize) as i32;
                } else {
                    debug_assert!(self.node[f].next == 0);
                }
                self.node[mp].next = (f as isize - mp as isize) as i32;
                self.node[f].set_key(key);
                debug_assert!(matches!(self.node[f].value, LuaValue::Nil));
                self.node[f].value = value;
                return Ok(());
            }
        }
        self.node[mp].set_key(key);
        debug_assert!(matches!(self.node[mp].value, LuaValue::Nil));
        self.node[mp].value = value;
        Ok(())
    }

    fn get_int_slot(&self, key: i64) -> TableSlotRef {
        let alimit = self.alimit as u64;
        let uk = key as u64;
        if uk.wrapping_sub(1) < alimit {
            return TableSlotRef::Array((key - 1) as usize);
        }
        if !self.is_real_asize() && alimit > 0 {
            let masked = (uk.wrapping_sub(1)) & !(alimit.wrapping_sub(1));
            if masked < alimit {
                return TableSlotRef::Array((key - 1) as usize);
            }
        }
        if self.is_dummy() { return TableSlotRef::Absent; }
        let mut n = self.hash_idx_for_int(key);
        loop {
            if self.node[n].key_is_int() && self.node[n].key_int() == key {
                return TableSlotRef::Hash(n);
            }
            let nx = self.node[n].next;
            if nx == 0 { break; }
            n = (n as isize + nx as isize) as usize;
        }
        TableSlotRef::Absent
    }

    /// Read an integer key directly to a [`LuaValue`], mirroring C's
    /// `luaH_getint`. The array-part fast path returns the slot in a
    /// single bounds-checked load without constructing an intermediate
    /// [`TableSlotRef`] enum; only when the key falls through to the
    /// hash part do we walk the chain. Equivalent in observable
    /// behaviour to `slot_value(get_int_slot(key))`.
    #[inline]
    fn get_int_value(&self, key: i64) -> LuaValue {
        let alimit = self.alimit as u64;
        let uk = key as u64;
        if uk.wrapping_sub(1) < alimit {
            return self.array[(key - 1) as usize].clone();
        }
        self.get_int_value_cold(key)
    }

    #[cold]
    #[inline(never)]
    fn get_int_value_cold(&self, key: i64) -> LuaValue {
        let alimit = self.alimit as u64;
        let uk = key as u64;
        if !self.is_real_asize() && alimit > 0 {
            let masked = (uk.wrapping_sub(1)) & !(alimit.wrapping_sub(1));
            if masked < alimit {
                return self.array[(key - 1) as usize].clone();
            }
        }
        if self.is_dummy() { return LuaValue::Nil; }
        let mut n = self.hash_idx_for_int(key);
        loop {
            if self.node[n].key_is_int() && self.node[n].key_int() == key {
                return self.node[n].value.clone();
            }
            let nx = self.node[n].next;
            if nx == 0 { break; }
            n = (n as isize + nx as isize) as usize;
        }
        LuaValue::Nil
    }

    fn get_short_str_slot(&self, key: &GcRef<LuaString>) -> TableSlotRef {
        debug_assert!(key.is_short());
        if self.is_dummy() { return TableSlotRef::Absent; }
        let mut n = self.hashpow2_idx(key.hash());
        loop {
            if self.node[n].key_is_short_str() {
                let ks = self.node[n].key_string();
                if GcRef::ptr_eq(ks, key) || ks.as_bytes() == key.as_bytes() {
                    return TableSlotRef::Hash(n);
                }
            }
            let nx = self.node[n].next;
            if nx == 0 { return TableSlotRef::Absent; }
            n = (n as isize + nx as isize) as usize;
        }
    }

    /// Read a short-string key directly to a [`LuaValue`], mirroring the
    /// shape of [`Self::get_int_value`]: a single hash-chain walk that
    /// produces the slot's value without constructing an intermediate
    /// [`TableSlotRef`] enum. Short strings are interned, so pointer
    /// equality wins almost every comparison; the byte-equality fallback
    /// handles the rare cross-interning-table path. Callers must ensure
    /// `key.is_short()` before dispatching here.
    #[inline]
    fn get_str_value(&self, key: &GcRef<LuaString>) -> LuaValue {
        debug_assert!(key.is_short());
        if self.is_dummy() { return LuaValue::Nil; }
        let mut n = self.hashpow2_idx(key.hash());
        loop {
            if self.node[n].key_is_short_str() {
                let ks = self.node[n].key_string();
                if GcRef::ptr_eq(ks, key) || ks.as_bytes() == key.as_bytes() {
                    return self.node[n].value.clone();
                }
            }
            let nx = self.node[n].next;
            if nx == 0 { return LuaValue::Nil; }
            n = (n as isize + nx as isize) as usize;
        }
    }

    /// Cold fallback for keys that miss the integer- and short-string
    /// fast paths in [`LuaTable::get`] (long strings, booleans,
    /// non-integer floats, table / function keys, light userdata, …).
    /// Routes through the existing `get_slot` + `slot_value` pair.
    #[cold]
    #[inline(never)]
    fn get_generic_value(&self, key: &LuaValue) -> LuaValue {
        let slot = self.get_slot(key);
        self.slot_value(slot)
    }

    fn get_str_slot(&self, key: &GcRef<LuaString>) -> TableSlotRef {
        if key.is_short() {
            self.get_short_str_slot(key)
        } else {
            let ko = LuaValue::Str(key.clone());
            self.get_generic_slot(&ko)
        }
    }

    fn get_slot(&self, key: &LuaValue) -> TableSlotRef {
        match key {
            LuaValue::Str(s) if s.is_short() => self.get_short_str_slot(s),
            LuaValue::Int(i) => self.get_int_slot(*i),
            LuaValue::Nil => TableSlotRef::Absent,
            LuaValue::Float(f) => {
                let f = *f;
                let k = f as i64;
                if k as f64 == f { self.get_int_slot(k) }
                else { self.get_generic_slot(key) }
            }
            _ => self.get_generic_slot(key),
        }
    }

    fn slot_value(&self, slot: TableSlotRef) -> LuaValue {
        match slot {
            TableSlotRef::Array(i) => self.array[i].clone(),
            TableSlotRef::Hash(i) => self.node[i].value.clone(),
            TableSlotRef::Absent => LuaValue::Nil,
        }
    }

    fn finish_set(&mut self, key: &LuaValue, slot: TableSlotRef, value: LuaValue) -> Result<(), LuaError> {
        match slot {
            TableSlotRef::Absent => self.new_key(key, value),
            TableSlotRef::Array(i) => { self.array[i] = value; Ok(()) }
            TableSlotRef::Hash(i) => { self.node[i].value = value; Ok(()) }
        }
    }

    fn set(&mut self, key: &LuaValue, value: LuaValue) -> Result<(), LuaError> {
        let slot = self.get_slot(key);
        self.finish_set(key, slot, value)
    }

    /// Set by integer key. May grow the array part up to
    /// [`ARRAY_GROW_CAP`] for keys just past `alimit` to amortise the
    /// common `t[#t+1] = v` pattern.
    fn set_int(&mut self, key: i64, value: LuaValue) -> Result<(), LuaError> {
        let slot = self.get_int_slot(key);
        if matches!(slot, TableSlotRef::Absent) {
            if key > 0 && (key as u64) <= ARRAY_GROW_CAP as u64 {
                let cur = self.alimit as i64;
                if key == cur + 1 && !matches!(value, LuaValue::Nil) {
                    let new_size = (key as u32).next_power_of_two().max(4);
                    let capped = new_size.min(ARRAY_GROW_CAP);
                    if capped > self.alimit {
                        let nsize = self.alloc_sizenode();
                        self.resize(capped, nsize)?;
                        let new_slot = self.get_int_slot(key);
                        return self.finish_set(&LuaValue::Int(key), new_slot, value);
                    }
                }
            }
        }
        match slot {
            TableSlotRef::Absent => {
                let k = LuaValue::Int(key);
                self.new_key(&k, value)
            }
            TableSlotRef::Array(i) => { self.array[i] = value; Ok(()) }
            TableSlotRef::Hash(i) => { self.node[i].value = value; Ok(()) }
        }
    }

    /// Integer-key entry used by [`LuaTable::try_raw_set`] /
    /// [`LuaTable::try_raw_set_int`]. The array fast path writes
    /// directly into a slot that is by definition already allocated
    /// (it lives inside the `Vec` at offset `key-1 < alimit`), so the
    /// `TOTAL_GROW_CAP` guard cannot apply. Only the cold path can
    /// allocate; the guard runs there.
    #[inline]
    fn try_raw_set_int_fast(&mut self, key: i64, value: LuaValue) -> Result<(), LuaError> {
        let alimit = self.alimit as u64;
        let uk = key as u64;
        if uk.wrapping_sub(1) < alimit {
            self.array[(key - 1) as usize] = value;
            return Ok(());
        }
        self.try_raw_set_int_cold(key, value)
    }

    #[cold]
    #[inline(never)]
    fn try_raw_set_int_cold(&mut self, key: i64, value: LuaValue) -> Result<(), LuaError> {
        if self.array.len() + self.node.len() >= TOTAL_GROW_CAP
            && matches!(self.get_int_slot(key), TableSlotRef::Absent)
        {
            return Err(LuaError::Memory);
        }
        self.set_int_value_cold(key, value)
    }

    /// Cold fallback for [`Self::set_int_value`]: handles the
    /// alimit-aliased slot (non-real-`asize` tables), hash-part lookup
    /// + in-place store, the array-grow-on-`#t+1` heuristic, and
    /// `new_key` insertion. Split into a `#[cold] #[inline(never)]`
    /// helper so LLVM lays out the array fast path as straight-line
    /// code in the inlined caller.
    #[cold]
    #[inline(never)]
    fn set_int_value_cold(&mut self, key: i64, value: LuaValue) -> Result<(), LuaError> {
        let alimit = self.alimit as u64;
        let uk = key as u64;
        if !self.is_real_asize() && alimit > 0 {
            let masked = (uk.wrapping_sub(1)) & !(alimit.wrapping_sub(1));
            if masked < alimit {
                self.array[(key - 1) as usize] = value;
                return Ok(());
            }
        }
        if !self.is_dummy() {
            let mut n = self.hash_idx_for_int(key);
            loop {
                if self.node[n].key_is_int() && self.node[n].key_int() == key {
                    self.node[n].value = value;
                    return Ok(());
                }
                let nx = self.node[n].next;
                if nx == 0 { break; }
                n = (n as isize + nx as isize) as usize;
            }
        }
        if key > 0 && (key as u64) <= ARRAY_GROW_CAP as u64 {
            let cur = self.alimit as i64;
            if key == cur + 1 && !matches!(value, LuaValue::Nil) {
                let new_size = (key as u32).next_power_of_two().max(4);
                let capped = new_size.min(ARRAY_GROW_CAP);
                if capped > self.alimit {
                    let nsize = self.alloc_sizenode();
                    self.resize(capped, nsize)?;
                    let new_slot = self.get_int_slot(key);
                    return self.finish_set(&LuaValue::Int(key), new_slot, value);
                }
            }
        }
        let k = LuaValue::Int(key);
        self.new_key(&k, value)
    }

    // ── boundary search ────────────────────────────────────────────────────

    fn hash_search(&self, mut j: u64) -> u64 {
        let mut i: u64;
        if j == 0 { j = 1; }
        loop {
            i = j;
            if j <= (i64::MAX as u64) / 2 {
                j *= 2;
            } else {
                j = i64::MAX as u64;
                let s = self.get_int_slot(j as i64);
                if matches!(s, TableSlotRef::Absent)
                    || matches!(self.slot_value(s), LuaValue::Nil)
                {
                    break;
                } else { return j; }
            }
            let s = self.get_int_slot(j as i64);
            if matches!(s, TableSlotRef::Absent) { break; }
            if matches!(self.slot_value(s), LuaValue::Nil) { break; }
        }
        while j - i > 1 {
            let m = i / 2 + j / 2;
            let s = self.get_int_slot(m as i64);
            let empty = matches!(s, TableSlotRef::Absent)
                || matches!(self.slot_value(s), LuaValue::Nil);
            if empty { j = m; } else { i = m; }
        }
        i
    }

    fn bin_search(array: &[LuaValue], mut i: u32, mut j: u32) -> u32 {
        while j - i > 1 {
            let m = (i + j) / 2;
            if matches!(array[(m - 1) as usize], LuaValue::Nil) { j = m; }
            else { i = m; }
        }
        i
    }

    /// Find a boundary `i` such that `t[i]` is present and `t[i+1]` is absent,
    /// or 0 if `t[1]` is absent. C: `luaH_getn`.
    fn getn(&mut self) -> u64 {
        let limit = self.alimit;
        if limit > 0 && matches!(self.array[(limit - 1) as usize], LuaValue::Nil) {
            if limit >= 2 && !matches!(self.array[(limit - 2) as usize], LuaValue::Nil) {
                if self.is_pow2_real_asize() && !Self::is_pow2(limit - 1) {
                    self.alimit = limit - 1;
                    self.flags.set_no_real_asize();
                }
                return (limit - 1) as u64;
            } else {
                let boundary = Self::bin_search(&self.array, 0, limit);
                if self.is_pow2_real_asize() && boundary > self.real_asize() / 2 {
                    self.alimit = boundary;
                    self.flags.set_no_real_asize();
                }
                return boundary as u64;
            }
        }
        if !self.limit_equals_asize() {
            if matches!(self.array[limit as usize], LuaValue::Nil) {
                return limit as u64;
            }
            let real = self.real_asize();
            if matches!(self.array[(real - 1) as usize], LuaValue::Nil) {
                let old_alimit = self.alimit;
                let boundary = Self::bin_search(&self.array, old_alimit, real);
                self.alimit = boundary;
                return boundary as u64;
            }
        }
        let limit = self.real_asize();
        debug_assert!(
            limit == self.real_asize()
                && (limit == 0 || !matches!(self.array[(limit - 1) as usize], LuaValue::Nil))
        );
        let next_key = (limit as i64).saturating_add(1);
        let next_slot = self.get_int_slot(next_key);
        let next_empty = matches!(next_slot, TableSlotRef::Absent)
            || matches!(self.slot_value(next_slot), LuaValue::Nil);
        if self.is_dummy() || next_empty {
            return limit as u64;
        }
        self.hash_search(limit as u64)
    }
}

// ── LuaTable (outer handle) ────────────────────────────────────────────────────

/// A Lua table: hybrid array + hash map.
///
/// All public methods take `&self` so the type works through
/// `GcRef<LuaTable>` (which only dereferences to a shared borrow).
/// Mutations are routed through an internal [`RefCell`].
#[derive(Debug)]
pub struct LuaTable {
    inner: RefCell<TableInner>,
    metatable: RefCell<Option<GcRef<LuaTable>>>,
    weak_mode: Cell<u8>,
}

impl std::fmt::Debug for TableInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TableInner")
            .field("alimit", &self.alimit)
            .field("array_len", &self.array.len())
            .field("node_len", &self.node.len())
            .finish()
    }
}

impl Default for LuaTable {
    fn default() -> Self {
        LuaTable {
            inner: RefCell::new(TableInner::new()),
            metatable: RefCell::new(None),
            weak_mode: Cell::new(0),
        }
    }
}

impl LuaTable {
    /// Construct an empty table. Used as a placeholder by callers that
    /// will populate it via the normal API.
    pub fn placeholder() -> Self { Self::default() }

    /// Borrow inner storage for read access. Intended for advanced
    /// callers (e.g. the GC trace impl); prefer the typed methods.
    pub fn with_inner<R>(&self, f: impl FnOnce(&TableInner) -> R) -> R {
        f(&self.inner.borrow())
    }

    /// Read a key. Returns `LuaValue::Nil` if absent or if `k` is nil.
    /// Integer keys take the direct array-part fast path used by
    /// [`LuaTable::get_int`]; short-string keys take the analogous
    /// hash-chain fast path used by [`LuaTable::get_short_str`]; every
    /// other key shape falls through to the cold generic slot lookup.
    /// Marked `#[inline(always)]` so the dispatch folds into the
    /// caller (the hot `state::fast_get` / `state::table_get_with_tm`
    /// frames in the VM); profiling at #[inline] showed LLVM was still
    /// emitting a cross-crate function call here.
    #[inline(always)]
    pub fn get(&self, k: &LuaValue) -> LuaValue {
        let inner = self.inner.borrow();
        match k {
            LuaValue::Nil => LuaValue::Nil,
            LuaValue::Int(i) => inner.get_int_value(*i),
            LuaValue::Str(s) if s.is_short() => inner.get_str_value(s),
            _ => inner.get_generic_value(k),
        }
    }

    /// Read by integer key. Hot path: callers like `state.fast_get_int`
    /// and `state.table_get_with_tm` dispatch here on every integer-key
    /// access in user code (`t[1]`, `OP_GETI`, ipairs loops, etc.). The
    /// array-part lookup folds into a single bounds-checked load,
    /// matching C's `luaH_getint`.
    #[inline(always)]
    pub fn get_int(&self, key: i64) -> LuaValue {
        let inner = self.inner.borrow();
        inner.get_int_value(key)
    }

    /// Read by string key. Despite the name (kept for compatibility
    /// with the old API), this dispatches internally to either the
    /// short- or long-string path; passing a long string is safe. The
    /// short-string branch (the common case — all interned identifiers
    /// and most table-field keys are short) takes the folded hash-walk
    /// in [`TableInner::get_str_value`]; long strings still go through
    /// the slot indirection.
    #[inline(always)]
    pub fn get_short_str(&self, k: &GcRef<LuaString>) -> LuaValue {
        let inner = self.inner.borrow();
        if k.is_short() {
            inner.get_str_value(k)
        } else {
            let slot = inner.get_str_slot(k);
            inner.slot_value(slot)
        }
    }

    /// Read by raw byte-string key. Linear scan over the hash part —
    /// rarely-used helper for callers that don't have a `GcRef<LuaString>`
    /// handle.
    pub fn get_str_bytes(&self, key_bytes: &[u8]) -> LuaValue {
        let mut found = LuaValue::Nil;
        self.for_each_entry(|k, v| {
            if !matches!(found, LuaValue::Nil) { return; }
            if let LuaValue::Str(s) = k {
                if s.as_bytes() == key_bytes {
                    found = v.clone();
                }
            }
        });
        found
    }

    /// Raw set without metamethod dispatch. Nil keys are an error;
    /// NaN-float keys are an error. Setting `v == Nil` clears the slot.
    pub fn raw_set(&self, k: LuaValue, v: LuaValue) {
        if matches!(k, LuaValue::Nil) { return; }
        if let LuaValue::Float(f) = &k {
            if f.is_nan() { return; }
        }
        let mut inner = self.inner.borrow_mut();
        let _ = inner.set(&k, v);
    }

    /// Raw set with explicit error returns; preferred path used by
    /// `LuaTableRefExt::raw_set` in `lua-vm`. Integer keys (and floats
    /// that are exact integers) take the same direct array-part fast
    /// path used by [`LuaTable::try_raw_set_int`]; other key shapes
    /// fall through to the generic slot lookup.
    #[inline]
    pub fn try_raw_set(&self, k: LuaValue, v: LuaValue) -> Result<(), LuaError> {
        match &k {
            LuaValue::Nil => {
                Err(LuaError::runtime(format_args!("table index is nil")))
            }
            LuaValue::Float(f) if f.is_nan() => {
                Err(LuaError::runtime(format_args!("table index is NaN")))
            }
            LuaValue::Int(i) => {
                let key = *i;
                let mut inner = self.inner.borrow_mut();
                inner.try_raw_set_int_fast(key, v)
            }
            LuaValue::Float(f) => {
                let f = *f;
                let k_int = f as i64;
                if k_int as f64 == f {
                    let mut inner = self.inner.borrow_mut();
                    inner.try_raw_set_int_fast(k_int, v)
                } else {
                    self.try_raw_set_generic(k, v)
                }
            }
            _ => self.try_raw_set_generic(k, v),
        }
    }

    /// Generic-key path for [`Self::try_raw_set`]. Split out so the
    /// integer fast path stays branch-light and inlineable.
    #[cold]
    #[inline(never)]
    fn try_raw_set_generic(&self, k: LuaValue, v: LuaValue) -> Result<(), LuaError> {
        let mut inner = self.inner.borrow_mut();
        if inner.array.len() + inner.node.len() >= TOTAL_GROW_CAP
            && matches!(inner.get_slot(&k), TableSlotRef::Absent)
        {
            return Err(LuaError::Memory);
        }
        inner.set(&k, v)
    }

    /// Raw set by integer key with explicit error returns. Routes the
    /// array-part fast path through [`TableInner::set_int_value`] — a
    /// single bounds-checked store with no intermediate
    /// [`TableSlotRef`] indirection — and only consults the
    /// `TOTAL_GROW_CAP` allocation guard when the key would create a
    /// new slot.
    #[inline]
    pub fn try_raw_set_int(&self, k: i64, v: LuaValue) -> Result<(), LuaError> {
        let mut inner = self.inner.borrow_mut();
        inner.try_raw_set_int_fast(k, v)
    }

    /// Resize the table to new array and hash sizes (sizing hint from
    /// the bytecode's `OP_NEWTABLE`).
    pub fn resize(&self, new_asize: u32, new_hsize: u32) -> Result<(), LuaError> {
        let mut inner = self.inner.borrow_mut();
        inner.resize(new_asize, new_hsize)
    }

    /// Number of array-part slots currently allocated. Cheap counter
    /// for sizing decisions; NOT the Lua `#t` length operator.
    pub fn array_len(&self) -> usize { self.inner.borrow().array.len() }

    /// Total occupied slots (array + hash) — used for legacy
    /// `len()` callers; prefer `getn` for the Lua `#` operator.
    pub fn len(&self) -> usize {
        let inner = self.inner.borrow();
        let mut n = 0usize;
        for v in inner.array.iter() {
            if !matches!(v, LuaValue::Nil) { n += 1; }
        }
        for node in inner.node.iter() {
            if !matches!(node.value, LuaValue::Nil) { n += 1; }
        }
        n
    }
    pub fn is_empty(&self) -> bool { self.len() == 0 }

    /// `#t` boundary (C: `luaH_getn`). Mutates internal caching state.
    pub fn getn(&self) -> u64 {
        let mut inner = self.inner.borrow_mut();
        inner.getn()
    }

    /// Returns true iff `k` resolves to a slot in this table (array or
    /// hash). Used by `next` to validate the resumption key.
    pub fn contains_key(&self, k: &LuaValue) -> bool {
        if matches!(k, LuaValue::Nil) { return false; }
        let inner = self.inner.borrow();
        let slot = inner.get_slot(k);
        !matches!(slot, TableSlotRef::Absent)
    }

    pub fn metatable(&self) -> Option<GcRef<LuaTable>> {
        self.metatable.borrow().clone()
    }

    /// Install a metatable. Inspects its `__mode` field eagerly so the
    /// GC trace impl can read [`weak_mode`] without re-entering the
    /// metatable RefCell.
    pub fn set_metatable(&self, mt: Option<GcRef<LuaTable>>) {
        let mode = mt.as_ref().map(|t| extract_weak_mode(t)).unwrap_or(0);
        self.weak_mode.set(mode);
        *self.metatable.borrow_mut() = mt;
    }

    pub fn weak_mode(&self) -> u8 { self.weak_mode.get() }

    /// Implements Lua's `next(t, k)`.
    pub fn next_pair(&self, k: &LuaValue) -> Option<(LuaValue, LuaValue)> {
        let inner = self.inner.borrow();
        inner.next_pair(k).ok().flatten()
    }

    /// Like [`next_pair`] but reports the `"invalid key to 'next'"`
    /// error when `k` is non-nil and not present.
    pub fn try_next_pair(&self, k: &LuaValue) -> Result<Option<(LuaValue, LuaValue)>, LuaError> {
        let inner = self.inner.borrow();
        inner.next_pair(k)
    }

    /// Walk every live (key, value) pair via the given closure.
    /// Used by the GC trace impl to avoid the overhead of repeatedly
    /// re-entering `find_index` from `next_pair`.
    pub fn for_each_entry(&self, mut f: impl FnMut(&LuaValue, &LuaValue)) {
        let inner = self.inner.borrow();
        for (i, v) in inner.array.iter().enumerate() {
            if !matches!(v, LuaValue::Nil) {
                let k = LuaValue::Int((i + 1) as i64);
                f(&k, v);
            }
        }
        for node in inner.node.iter() {
            if !matches!(node.value, LuaValue::Nil) {
                f(&node.key, &node.value);
            }
        }
    }

    /// Drop weak entries whose weakly-tracked target is unreachable,
    /// and return the list of values whose strings must still be
    /// marked by the caller.
    pub fn prune_weak_dead(&self, is_reachable: &dyn Fn(usize) -> bool) -> Vec<LuaValue> {
        let mode = self.weak_mode.get();
        if mode == 0 { return Vec::new(); }
        let weak_k = (mode & WEAK_KEYS) != 0;
        let weak_v = (mode & WEAK_VALUES) != 0;
        let mut to_mark: Vec<LuaValue> = Vec::new();
        let mut inner = self.inner.borrow_mut();
        for i in 0..inner.array.len() {
            let v = inner.array[i].clone();
            if matches!(v, LuaValue::Nil) { continue; }
            if weak_v && value_is_dead_collectable(&v, is_reachable) {
                inner.array[i] = LuaValue::Nil;
                continue;
            }
            if weak_v {
                if matches!(v, LuaValue::Str(_)) { to_mark.push(v); }
            }
        }
        let mut i = 0;
        while i < inner.node.len() {
            let v = inner.node[i].value.clone();
            if matches!(v, LuaValue::Nil) {
                i += 1;
                continue;
            }
            let k = inner.node[i].key.clone();
            if weak_v && value_is_dead_collectable(&v, is_reachable) {
                inner.clear_dead_hash_node(i);
                continue;
            }
            if weak_k && value_is_dead_collectable(&k, is_reachable) {
                inner.clear_dead_hash_node(i);
                continue;
            }
            if weak_k {
                if matches!(k, LuaValue::Str(_)) { to_mark.push(k); }
            }
            if weak_v {
                if matches!(v, LuaValue::Str(_)) { to_mark.push(v); }
            }
            i += 1;
        }
        to_mark
    }

    /// Ephemeron-convergence helper for pure `__mode = "k"` tables.
    pub fn ephemeron_values_to_mark(&self, is_reachable: &dyn Fn(usize) -> bool) -> Vec<LuaValue> {
        let mode = self.weak_mode.get();
        if (mode & WEAK_KEYS) == 0 || (mode & WEAK_VALUES) != 0 {
            return Vec::new();
        }
        let inner = self.inner.borrow();
        let mut out = Vec::new();
        for node in inner.node.iter() {
            if matches!(node.value, LuaValue::Nil) { continue; }
            if !value_is_dead_collectable(&node.key, is_reachable) {
                out.push(node.value.clone());
            }
        }
        for (i, v) in inner.array.iter().enumerate() {
            if matches!(v, LuaValue::Nil) { continue; }
            let k = LuaValue::Int((i + 1) as i64);
            if !value_is_dead_collectable(&k, is_reachable) {
                out.push(v.clone());
            }
        }
        out
    }
}

// ── Free helpers ──────────────────────────────────────────────────────────────

/// True iff `v` is a collectable non-string LuaValue whose target was
/// unreached during the mark phase. Strings are explicitly excluded.
fn value_is_dead_collectable(v: &LuaValue, is_reachable: &dyn Fn(usize) -> bool) -> bool {
    match v {
        LuaValue::Table(t) => !is_reachable(t.identity()),
        LuaValue::UserData(u) => !is_reachable(u.identity()),
        LuaValue::Thread(th) => !is_reachable(th.identity()),
        LuaValue::Function(c) => match c {
            LuaClosure::Lua(x) => !is_reachable(x.identity()),
            LuaClosure::C(x) => !is_reachable(x.identity()),
            LuaClosure::LightC(_) => false,
        },
        LuaValue::Str(_)
        | LuaValue::Nil
        | LuaValue::Bool(_)
        | LuaValue::Int(_)
        | LuaValue::Float(_)
        | LuaValue::LightUserData(_) => false,
    }
}

/// Inspect a metatable's `__mode` field and produce the corresponding
/// `WEAK_KEYS | WEAK_VALUES` bitmask. Returns 0 when no `__mode` is
/// set or it is not a string.
fn extract_weak_mode(mt: &LuaTable) -> u8 {
    let inner = mt.inner.borrow();
    for node in inner.node.iter() {
        if let LuaValue::Str(ks) = &node.key {
            if ks.as_bytes() == b"__mode" {
                if let LuaValue::Str(vs) = &node.value {
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

// ──────────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        src/ltable.c (~995 lines, 28 functions), src/ltable.h
//   target_crate:  lua-types
//   confidence:    high
//   todos:         0
//   port_notes:    0
//   unsafe_blocks: 0
//   notes:         Canonical LuaTable: hybrid array + hash. Mirrors C's Table
//                  struct (flags, lsizenode, alimit, array, node, lastfree) with
//                  Vec<LuaValue> + Vec<TableNode> in place of raw C pointers, and
//                  Option<usize> indexing in place of Node*. The luaH_getn
//                  boundary search + alimit-aware integer-key fast path are
//                  ported faithfully (see getn() and get_int_slot()). The
//                  integer-key read path also exposes get_int_value, which
//                  mirrors C's luaH_getint by returning the array slot directly
//                  in one bounds-checked load (no TableSlotRef indirection) and
//                  splitting the rare alimit-aliased / hash-part path into a
//                  cold helper. The short-string read path mirrors that shape
//                  via get_str_value (single hash-chain walk, no TableSlotRef
//                  round-trip); LuaTable::get dispatches on integer/short-string
//                  keys inline and routes everything else through a #[cold]
//                  get_generic_value, matching C's luaH_get fast-path structure.
//                  LuaTable::get / get_int / get_short_str are #[inline(always)]
//                  so the dispatch folds into the cross-crate VM hot frames
//                  (state::fast_get / state::table_get_with_tm). Weak-table mode
//                  flags + the prune_weak_dead / ephemeron_values_to_mark
//                  helpers integrate with the lua-gc Trace impl.
// ──────────────────────────────────────────────────────────────────────────────
