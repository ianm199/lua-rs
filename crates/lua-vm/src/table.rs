//! Lua table implementation (array + hash hybrid).
//!
//! Port of `reference/lua-5.4.7/src/ltable.c` (995 lines, 28 functions).
//! The companion header `ltable.h` is merged here per PORTING.md §1.
//!
//! # Design overview
//!
//! Tables keep elements in two parts: an **array part** (`Vec<LuaValue>`,
//! 1-based indices 1..=alimit) and a **hash part** (`Vec<TableNode>`,
//! sized to a power-of-two).  Non-negative integer keys are candidates for
//! the array part; all other keys go into the hash part.
//!
//! The hash part uses Brent's variation of chained scatter tables.  The key
//! invariant: if an element is not in its *main position* (the slot its hash
//! maps to), then the colliding element *is* in its own main position.
//!
//! ## C → Rust structural differences
//!
//! | C pattern | Rust pattern |
//! |---|---|
//! | `Node *` (pointer into hash array) | `usize` index into `self.node` |
//! | `const TValue *` return from get functions | `TableSlotRef` enum |
//! | `&absentkey` sentinel | `TableSlotRef::Absent` |
//! | `lastfree == NULL` (dummy) | `lastfree: None` |
//! | `dummynode` static | empty `node: vec![]` + `lastfree: None` |
//! | `nodefromval(v)` pointer subtraction | index stored in `TableSlotRef::Hash(i)` |
//!
//! ## `TableSlotRef`
//!
//! C functions like `luaH_getint` return `const TValue *`, which may point to
//! an array slot, a hash-node value, or the static `absentkey` sentinel.
//! Rust can't model this as a borrow that spans the C-style "get then set"
//! two-phase pattern, so we use an explicit `TableSlotRef` that carries an
//! index into either the array or the node vector.  Phase B will revisit
//! whether a `&LuaValue` return is feasible with lifetime annotations.
//!
//! # C source files
//! - `reference/lua-5.4.7/src/ltable.c`  (995 lines, 28 functions)
//! - `reference/lua-5.4.7/src/ltable.h`  (63 lines; merged here)

// C: #define ltable_c
// C: #define LUA_CORE

use std::rc::Rc;
#[allow(unused_imports)] use crate::prelude::*;

// TODO(port): import paths will stabilize in Phase B once the crate graph is wired.
use crate::object::ceil_log2;
use crate::state::{GcRef, LuaState, LuaValue, LuaClosure};
use lua_types::LuaString;
use lua_types::error::LuaError;
use lua_types::StackIdx;

// ── Constants ─────────────────────────────────────────────────────────────────

/// Largest `k` such that `2^k` fits in a signed `i32` (`sizeof(int)*8 - 1`).
/// C: `#define MAXABITS cast_int(sizeof(int) * CHAR_BIT - 1)`
const MAXABITS: u32 = (std::mem::size_of::<i32>() as u32) * 8 - 1; // = 31

/// Maximum size of the array part: `min(2^MAXABITS, usize::MAX / sizeof(LuaValue))`.
/// C: `#define MAXASIZE luaM_limitN(1u << MAXABITS, TValue)`
// PORT NOTE: Using u32 here since alimit is u32 in LuaTable; real cap is memory-limited.
const MAXASIZE: u32 = 1u32 << MAXABITS; // 2_147_483_648 on 32-bit int

/// Largest `k` such that `2^k` fits in a signed `i32` minus one (hash part).
/// C: `#define MAXHBITS (MAXABITS - 1)`
const MAXHBITS: u32 = MAXABITS - 1; // = 30

/// Maximum size of the hash part (power-of-2 count of nodes).
/// C: `#define MAXHSIZE luaM_limitN(1u << MAXHBITS, Node)`
const MAXHSIZE: u32 = 1u32 << MAXHBITS; // 1_073_741_824

/// Bit 7 of `TableFlags`: when set, `alimit` is NOT the real array size.
/// When clear, `alimit` IS the real array size.
/// C: `#define BITRAS (1 << 7)` with `isrealasize = !testbit(flags, 7)`.
const BIT_RAS: u8 = 1 << 7;

// ── TableFlags ─────────────────────────────────────────────────────────────────

/// Bitfield for a [`LuaTable`]: lower bits record absent fast-access metamethods;
/// bit 7 (`BIT_RAS`) encodes whether `alimit` is the real array size.
///
/// C: `lu_byte flags` in `Table`.  Uses per-PORTING.md §4.9 Tag-style newtype.
#[derive(Clone, Copy, Debug, Default)]
pub struct TableFlags(pub u8);

impl TableFlags {
    /// `isrealasize(t)` — bit 7 clear means alimit IS the real array size.
    #[inline]
    pub fn is_real_asize(self) -> bool {
        (self.0 & BIT_RAS) == 0
    }

    /// `setrealasize(t)` — clear BIT_RAS so alimit becomes the canonical size.
    #[inline]
    pub fn set_real_asize(&mut self) {
        self.0 &= !BIT_RAS;
    }

    /// `setnorealasize(t)` — set BIT_RAS so alimit is only a hint.
    #[inline]
    pub fn set_no_real_asize(&mut self) {
        self.0 |= BIT_RAS;
    }

    /// `invalidateTMcache(t)` — clear all fast-access metamethod bits.
    /// Forces the next access to look up the metatable properly.
    /// C: `(t)->flags &= ~maskflags`
    /// TODO(port): MASK_FLAGS depends on TM_EQ ordinal from ltm.h; using 0x7F placeholder.
    #[inline]
    pub fn invalidate_tm_cache(&mut self) {
        // maskflags = (1 << (TM_EQ + 1)) - 1; TM_EQ = 6 in Lua 5.4 → 0x7F
        const MASK_FLAGS: u8 = 0x7F;
        self.0 &= !MASK_FLAGS;
    }
}

// ── TableNode ──────────────────────────────────────────────────────────────────

/// One node in a table's hash part.
///
/// C: `Node` (= `struct Node` composed of `NodeKey` + `TValue i_val`).
/// The C struct uses a union `{key_tt, key_val, next}` packed for layout;
/// in Rust we use plain named fields.  The GC color / "dead key" flag is
/// present in C but elided in Phases A–C (no tracing GC yet).
pub struct TableNode {
    /// Value stored at this key.  C: `n->i_val` / `gval(n)`.
    pub value: LuaValue,
    /// Key stored in this node.  C: `n->u.key_val` + `n->u.key_tt`.
    pub key: LuaValue,
    /// Collision-chain offset (positive = next is `self + next`, negative allowed).
    /// Zero means end of chain.  C: `n->u.next` (`int`).
    pub next: i32,
}

impl TableNode {
    fn empty() -> Self {
        TableNode { value: LuaValue::Nil, key: LuaValue::Nil, next: 0 }
    }

    // C: keyisnil(n)
    fn key_is_nil(&self) -> bool { matches!(self.key, LuaValue::Nil) }

    // C: keyisinteger(n)
    fn key_is_int(&self) -> bool { matches!(self.key, LuaValue::Int(_)) }

    // C: keyival(n)
    fn key_int(&self) -> i64 {
        if let LuaValue::Int(i) = self.key { i }
        else { panic!("TableNode::key_int: key is not int") }
    }

    // C: keyisshrstr(n)
    fn key_is_short_str(&self) -> bool {
        if let LuaValue::Str(s) = &self.key { s.is_short() }
        else { false }
    }

    // C: keystrval(n) — returns the LuaString in the key.
    fn key_string(&self) -> &GcRef<LuaString> {
        if let LuaValue::Str(s) = &self.key { s }
        else { panic!("TableNode::key_string: key is not a string") }
    }

    // C: keyisdead(n) — dead keys are GC tombstones; no dead keys in Phase A–C.
    fn key_is_dead(&self) -> bool { false }

    // C: setnilkey(n) — mark key slot as empty.
    fn set_key_nil(&mut self) { self.key = LuaValue::Nil; }

    // C: setnodekey(L, n, obj)
    fn set_key(&mut self, k: &LuaValue) { self.key = k.clone(); }

    // C: getnodekey(L, obj, n) — copy node key out as a TValue.
    fn key_value(&self) -> LuaValue { self.key.clone() }

    // C: iscollectable(key) — whether this key holds a GC-managed reference.
    fn key_is_collectable(&self) -> bool {
        matches!(&self.key,
            LuaValue::Str(_)
            | LuaValue::Table(_)
            | LuaValue::Function(LuaClosure::Lua(_))
            | LuaValue::Function(LuaClosure::C(_))
            | LuaValue::UserData(_)
            | LuaValue::Thread(_)
        )
    }
}

// ── TableSlotRef ───────────────────────────────────────────────────────────────

/// Internal slot reference returned by the "get" family of functions.
///
/// Replaces the C pattern of returning `const TValue *` which may point into
/// either the array part, the hash part, or the static `absentkey` sentinel.
/// Callers use this to either read the value or to feed into `finish_set`.
///
/// PORT NOTE: In C, `nodefromval(v)` recovers the `Node *` from a value pointer
/// via C struct-layout pointer arithmetic.  In Rust we carry the hash-node index
/// explicitly in `Hash(usize)` so traversal code (`findindex`) can compute the
/// linear position without pointer arithmetic.
#[derive(Debug, Clone, Copy)]
pub enum TableSlotRef {
    /// Key lives in the array part at this 0-based index.
    Array(usize),
    /// Key lives in the hash part at this 0-based node index.
    Hash(usize),
    /// Key is absent from the table (C: `&absentkey`).
    Absent,
}

impl TableSlotRef {
    // C: isabstkey(slot)
    fn is_absent(self) -> bool { matches!(self, TableSlotRef::Absent) }
}

// ── LuaTable ───────────────────────────────────────────────────────────────────

/// A Lua table: hybrid array + hash map.
///
/// C: `struct Table` in `lobject.h`.  The full field mapping is in
/// `ANALYSES/types.tsv` under `Table → LuaTable`.
///
/// # Dummy hash part
///
/// When a table has no hash part, `self.node` is empty and
/// `self.lastfree` is `None`.  This replaces the C idiom of storing a
/// pointer to a shared static `dummynode_` and checking `lastfree == NULL`.
pub struct LuaTable {
    /// Bit-packed: lower bits = absent-metamethod flags; bit 7 = BIT_RAS.
    /// C: `lu_byte flags`.
    pub flags: TableFlags,
    /// `log2` of the hash-part node count.  `1u32 << lsizenode` = capacity.
    /// C: `lu_byte lsizenode`.
    pub lsizenode: u8,
    /// Array-part size hint or real size depending on `flags.is_real_asize()`.
    /// C: `unsigned int alimit`.
    pub alimit: u32,
    /// The array part; 0-based in Rust (Lua index `k` → `self.array[k-1]`).
    /// C: `TValue *array`.
    pub array: Vec<LuaValue>,
    /// The hash part; length is `1 << lsizenode` when not dummy, 0 when dummy.
    /// C: `Node *node`.
    pub node: Vec<TableNode>,
    /// Cursor into `self.node` for free-slot scanning (one-past the last tried
    /// position).  `None` when using the dummy hash part.
    /// C: `Node *lastfree` (NULL = dummy).
    pub lastfree: Option<usize>,
    /// Optional metatable.
    /// C: `struct Table *metatable`.
    pub metatable: Option<GcRef<LuaTable>>,
}

impl LuaTable {
    // ── Predicate helpers ──────────────────────────────────────────────────

    /// `isdummy(t)` — true when the table has no allocated hash part.
    #[inline]
    pub fn is_dummy(&self) -> bool {
        self.lastfree.is_none()
    }

    /// `sizenode(t)` — nominal hash-part capacity (`1 << lsizenode`).
    /// This is the modulo used for hash indexing, not the vec length.
    #[inline]
    fn sizenode(&self) -> u32 {
        1u32 << self.lsizenode
    }

    /// `allocsizenode(t)` — 0 when dummy, else `1 << lsizenode`.
    #[inline]
    pub fn alloc_sizenode(&self) -> u32 {
        if self.is_dummy() { 0 } else { self.sizenode() }
    }

    /// `isrealasize(t)` accessor; delegates to `TableFlags`.
    #[inline]
    fn is_real_asize(&self) -> bool { self.flags.is_real_asize() }

    // ── ispow2 (C macro with 0-as-pow2 semantics) ─────────────────────────

    /// `ispow2(x)` — C treats 0 as a power of two (`(x & (x-1)) == 0`).
    #[inline]
    fn is_pow2(x: u32) -> bool {
        x == 0 || x.is_power_of_two()
    }

    // ── luaH_realasize ────────────────────────────────────────────────────

    /// Returns the real size of the array part.
    ///
    /// When `flags.is_real_asize()`, this is just `alimit`.  Otherwise
    /// `alimit` is a hint and the real size is the next power of two above it.
    ///
    /// C: `unsigned int luaH_realasize (const Table *t)` (`LUAI_FUNC`).
    pub fn real_asize(&self) -> u32 {
        // C: if (limitequalsasize(t)) return t->alimit;
        if self.limit_equals_asize() {
            return self.alimit;
        }
        // Compute the smallest power of 2 not smaller than alimit.
        let mut size = self.alimit;
        size |= size >> 1;
        size |= size >> 2;
        size |= size >> 4;
        size |= size >> 8;
        size |= size >> 16;
        // For 64-bit unsigned int Lua would also do >> 32; but alimit is u32 here.
        size = size.wrapping_add(1);
        debug_assert!(
            Self::is_pow2(size) && size / 2 < self.alimit && self.alimit < size
        );
        size
    }

    /// `limitequalsasize(t)` — true when `alimit` equals the real array size.
    /// C: `#define limitequalsasize(t) (isrealasize(t) || ispow2((t)->alimit))`
    #[inline]
    fn limit_equals_asize(&self) -> bool {
        self.is_real_asize() || Self::is_pow2(self.alimit)
    }

    /// `ispow2realasize(t)` — is the real array size a power of two?
    /// C: `static int ispow2realasize (const Table *t)`.
    fn is_pow2_real_asize(&self) -> bool {
        !self.is_real_asize() || Self::is_pow2(self.alimit)
    }

    /// `setlimittosize(t)` — force `alimit` to the actual real array size
    /// and clear the BIT_RAS flag.
    /// C: `static unsigned int setlimittosize (Table *t)`.
    fn set_limit_to_size(&mut self) -> u32 {
        self.alimit = self.real_asize();
        self.flags.set_real_asize();
        self.alimit
    }

    // ── Hash helper functions ──────────────────────────────────────────────

    /// Returns the node index for an integer key using `hashmod`.
    /// C: `static Node *hashint (const Table *t, lua_Integer i)`.
    fn hash_idx_for_int(&self, i: i64) -> usize {
        // C: lua_Unsigned ui = l_castS2U(i);
        let ui = i as u64;
        let sn = self.sizenode() as usize;
        let modulo = (sn - 1) | 1; // odd modulo avoids power-of-2 bias
        if ui <= i32::MAX as u64 {
            // C: return hashmod(t, cast_int(ui));
            (ui as usize) % modulo
        } else {
            // C: return hashmod(t, ui);
            (ui as usize) % modulo
        }
    }

    /// Returns a node index using `hashpow2` (power-of-2 hash).
    /// `h` is the pre-computed hash value.
    /// C: `#define hashpow2(t,n) gnode(t, lmod((n), sizenode(t)))`.
    #[inline]
    fn hashpow2_idx(&self, h: u32) -> usize {
        // lmod(h, sizenode) = h & (sizenode - 1);  sizenode is power-of-2
        (h & (self.sizenode() - 1)) as usize
    }

    /// Returns a node index using `hashmod` (general hash).
    /// C: `#define hashmod(t,n) gnode(t, ((n) % ((sizenode(t)-1)|1)))`.
    #[inline]
    fn hashmod_idx(&self, h: usize) -> usize {
        let sn = self.sizenode() as usize;
        let modulo = (sn - 1) | 1;
        h % modulo
    }

    /// The main position (hash bucket index) for a given key.
    /// C: `static Node *mainpositionTV (const Table *t, const TValue *key)`.
    fn main_position(&self, key: &LuaValue) -> usize {
        match key {
            LuaValue::Int(i) => {
                // C: return hashint(t, i);
                self.hash_idx_for_int(*i)
            }
            LuaValue::Float(f) => {
                // C: return hashmod(t, l_hashfloat(n));
                let h = hash_float(*f);
                self.hashmod_idx(h as usize)
            }
            LuaValue::Str(s) if s.is_short() => {
                // C: hashstr(t, ts) = hashpow2(t, ts->hash)
                self.hashpow2_idx(s.hash())
            }
            LuaValue::Str(s) => {
                // Long string: C: hashpow2(t, luaS_hashlongstr(ts))
                // TODO(port): LuaString::hash_long() method needed; using hash() as fallback
                self.hashpow2_idx(s.hash())
            }
            LuaValue::Bool(false) => {
                // C: hashboolean(t, 0) = hashpow2(t, 0)
                self.hashpow2_idx(0)
            }
            LuaValue::Bool(true) => {
                // C: hashboolean(t, 1) = hashpow2(t, 1)
                self.hashpow2_idx(1)
            }
            LuaValue::LightUserData(p) => {
                // C: hashpointer(t, p) = hashmod(t, point2uint(p))
                // point2uint(p) = p as usize as u32
                // TODO(port): raw pointer to usize may need unsafe in Phase B
                let h = (*p as usize as u32) as usize;
                self.hashmod_idx(h)
            }
            LuaValue::Function(LuaClosure::LightC(f)) => {
                // C: hashpointer(t, f) where f is a lua_CFunction
                // TODO(port): fn pointer cast to usize for hashing
                let h = (*f as usize as u32) as usize;
                self.hashmod_idx(h)
            }
            LuaValue::Table(t) => {
                // C: hashpointer(t, gcvalue(key))
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
            LuaValue::Nil => {
                // nil cannot be a key; caller should guard against this
                0
            }
        }
    }

    /// Main position derived from a node (using the node's own key).
    /// C: `l_sinline Node *mainpositionfromnode (const Table *t, Node *nd)`.
    fn main_position_from_node(&self, nd: usize) -> usize {
        // C: TValue key; getnodekey(NULL, &key, nd); return mainpositionTV(t, &key);
        let key = self.node[nd].key_value();
        self.main_position(&key)
    }

    // ── Key equality ───────────────────────────────────────────────────────

    /// Raw key equality between a `LuaValue` key and a `TableNode`.
    /// Floats with integer values are normalised: the C code never stores a
    /// float that equals an integer in the hash; integers only compare to
    /// integers.  Dead keys compare by pointer identity.
    ///
    /// C: `static int equalkey (const TValue *k1, const Node *n2, int deadok)`.
    fn equal_key(k1: &LuaValue, n2: &TableNode, deadok: bool) -> bool {
        // C: if ((rawtt(k1) != keytt(n2)) && !(deadok && keyisdead(n2) && iscollectable(k1)))
        //      return 0;
        // In Rust: types must match unless deadok + dead key + collectable.
        let types_match = Self::value_type_tag_matches(k1, &n2.key);
        if !types_match {
            // C: deadok && keyisdead(n2) && iscollectable(k1)
            // Phase A: keyisdead always false; dead-key path never taken.
            if !(deadok && n2.key_is_dead() && Self::is_collectable(k1)) {
                return false;
            }
        }

        match &n2.key {
            LuaValue::Nil => true, // C: case LUA_VNIL: return 1
            LuaValue::Bool(b) => {
                // C: case LUA_VFALSE/LUA_VTRUE: return 1 (type already matched)
                matches!(k1, LuaValue::Bool(b2) if b == b2)
            }
            LuaValue::Int(ni) => {
                // C: case LUA_VNUMINT: return (ivalue(k1) == keyival(n2));
                matches!(k1, LuaValue::Int(ki) if ki == ni)
            }
            LuaValue::Float(nf) => {
                // C: case LUA_VNUMFLT: return luai_numeq(fltvalue(k1), fltvalueraw(keyval(n2)));
                matches!(k1, LuaValue::Float(kf) if kf == nf)
            }
            LuaValue::LightUserData(np) => {
                // C: case LUA_VLIGHTUSERDATA: return pvalue(k1) == pvalueraw(keyval(n2));
                matches!(k1, LuaValue::LightUserData(kp) if kp == np)
            }
            LuaValue::Function(LuaClosure::LightC(nf)) => {
                // C: case LUA_VLCF: return fvalue(k1) == fvalueraw(keyval(n2));
                matches!(k1, LuaValue::Function(LuaClosure::LightC(kf)) if *kf as usize == *nf as usize)
            }
            LuaValue::Str(ns) if ns.is_long() => {
                // C: case ctb(LUA_VLNGSTR): return luaS_eqlngstr(tsvalue(k1), keystrval(n2));
                // luaS_eqlngstr → byte equality for long strings
                if let LuaValue::Str(ks) = k1 {
                    ks.as_bytes() == ns.as_bytes()
                } else { false }
            }
            _ => {
                // C: default (short strings + all GC objects): pointer equality
                // eqshrstr for short strings: GcRef::ptr_eq (they are interned)
                // gcvalue equality: pointer identity
                Self::gc_ptr_eq(k1, &n2.key)
            }
        }
    }

    /// Whether `v` is a GC-managed value (has a collectable bit in C).
    fn is_collectable(v: &LuaValue) -> bool {
        matches!(v,
            LuaValue::Str(_)
            | LuaValue::Table(_)
            | LuaValue::Function(LuaClosure::Lua(_))
            | LuaValue::Function(LuaClosure::C(_))
            | LuaValue::UserData(_)
            | LuaValue::Thread(_)
        )
    }

    /// Whether two values have the same type tag (variant discriminant).
    /// C: `rawtt(k1) == keytt(n2)`.
    fn value_type_tag_matches(a: &LuaValue, b: &LuaValue) -> bool {
        std::mem::discriminant(a) == std::mem::discriminant(b)
    }

    /// Pointer equality for GC-managed values.  Short strings are interned so
    /// this is correct for them; other GC types also use pointer identity.
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

    // ── Generic hash-part lookup ────────────────────────────────────────────

    /// Search the hash part for `key`, returning a `TableSlotRef`.
    ///
    /// `deadok`: accept dead (tombstone) keys as equal to their original
    /// value (used by `findindex` for table traversal).
    ///
    /// C: `static const TValue *getgeneric (Table *t, const TValue *key, int deadok)`.
    fn get_generic_slot(&self, key: &LuaValue, deadok: bool) -> TableSlotRef {
        if self.is_dummy() { return TableSlotRef::Absent; }
        let mut n = self.main_position(key);
        loop {
            if Self::equal_key(key, &self.node[n], deadok) {
                return TableSlotRef::Hash(n);
            }
            let nx = self.node[n].next;
            if nx == 0 {
                return TableSlotRef::Absent;
            }
            // C: n += nx;  (signed offset into the node array)
            n = (n as isize + nx as isize) as usize;
        }
    }

    // ── arrayindex ──────────────────────────────────────────────────────────

    /// Returns the 1-based array index for `k`, or 0 if `k` is out of range.
    ///
    /// A key qualifies for the array part if it is in `[1, MAXASIZE]`.
    /// C: `static unsigned int arrayindex (lua_Integer k)`.
    fn array_index(k: i64) -> u32 {
        // C: if (l_castS2U(k) - 1u < MAXASIZE) return cast_uint(k);
        let uk = k as u64;
        if uk.wrapping_sub(1) < MAXASIZE as u64 {
            k as u32
        } else {
            0
        }
    }

    // ── findindex ───────────────────────────────────────────────────────────

    /// Finds the traversal position of `key` in the table.
    ///
    /// Returns a linear index: `[0, asize)` = array part, `[asize, asize+nh)`
    /// = hash part.  Zero means "first iteration" (key is nil).
    ///
    /// C: `static unsigned int findindex (lua_State *L, Table *t, TValue *key,
    ///                                    unsigned int asize)`.
    fn find_index(
        &self,
        state: &mut LuaState,
        key: &LuaValue,
        asize: u32,
    ) -> Result<u32, LuaError> {
        // C: if (ttisnil(key)) return 0;  /* first iteration */
        if matches!(key, LuaValue::Nil) { return Ok(0); }

        // C: i = ttisinteger(key) ? arrayindex(ivalue(key)) : 0;
        let i = if let LuaValue::Int(k) = key {
            Self::array_index(*k)
        } else {
            0
        };

        // C: if (i - 1u < asize) return i;  /* yes; that's the index */
        if i.wrapping_sub(1) < asize {
            return Ok(i);
        }

        // Search hash part.
        // C: const TValue *n = getgeneric(t, key, 1);
        let slot = self.get_generic_slot(key, true);
        match slot {
            TableSlotRef::Absent => {
                // C: luaG_runerror(L, "invalid key to 'next'");
                Err(LuaError::runtime(format_args!("invalid key to 'next'")))
            }
            TableSlotRef::Hash(node_idx) => {
                // C: i = cast_int(nodefromval(n) - gnode(t, 0));
                // hash elements are numbered after array ones
                Ok((node_idx as u32 + 1) + asize)
            }
            TableSlotRef::Array(_) => {
                // Should not happen: getgeneric only returns Hash or Absent.
                unreachable!("getgeneric returned Array slot")
            }
        }
    }

    // ── luaH_next ───────────────────────────────────────────────────────────

    /// Table traversal: given the current key at `key_idx` on the stack,
    /// pushes the next key-value pair and returns `true`, or returns `false`
    /// if the table is exhausted.
    ///
    /// C: `int luaH_next (lua_State *L, Table *t, StkId key)`.
    pub fn next(
        &self,
        state: &mut LuaState,
        key_idx: StackIdx,
    ) -> Result<bool, LuaError> {
        let asize = self.real_asize();
        // C: unsigned int i = findindex(L, t, s2v(key), asize);
        let key = state.stack_at(key_idx).clone();
        let i = self.find_index(state, &key, asize)?;

        // C: for (; i < asize; i++) { if (!isempty(&t->array[i])) { ... return 1; } }
        let mut i = i as usize;
        while i < asize as usize {
            if !matches!(self.array[i], LuaValue::Nil) {
                // C: setivalue(s2v(key), i + 1);
                state.set_at(key_idx, LuaValue::Int((i + 1) as i64));
                // C: setobj2s(L, key + 1, &t->array[i]);
                state.set_at(StackIdx(key_idx.0 + 1), self.array[i].clone());
                return Ok(true);
            }
            i += 1;
        }

        // C: for (i -= asize; cast_int(i) < sizenode(t); i++) { hash part }
        let mut hi = i.saturating_sub(asize as usize);
        while hi < self.node.len() {
            if !matches!(self.node[hi].value, LuaValue::Nil) {
                // C: getnodekey(L, s2v(key), n);
                state.set_at(key_idx, self.node[hi].key_value());
                // C: setobj2s(L, key + 1, gval(n));
                state.set_at(StackIdx(key_idx.0 + 1), self.node[hi].value.clone());
                return Ok(true);
            }
            hi += 1;
        }

        Ok(false) // no more elements
    }

    // ── freehash ────────────────────────────────────────────────────────────

    /// Drop the hash part (become dummy again).
    /// C: `static void freehash (lua_State *L, Table *t)`.
    fn free_hash(&mut self) {
        // C: if (!isdummy(t)) luaM_freearray(L, t->node, cast_sizet(sizenode(t)));
        // Rust: Drop of Vec handles deallocation.
        self.node.clear();
        self.lastfree = None;
        self.lsizenode = 0;
    }

    // ── Rehash helpers ──────────────────────────────────────────────────────

    /// Compute the optimal array-part size given the histogram `nums`.
    ///
    /// `nums[i]` = number of integer keys in `(2^(i-1), 2^i]`.
    /// `pna` enters as the total integer key count and leaves as the count
    /// that will reside in the array part.  Returns the optimal array size.
    ///
    /// C: `static unsigned int computesizes (unsigned int nums[], unsigned int *pna)`.
    fn compute_sizes(nums: &[u32], pna: &mut u32) -> u32 {
        let mut twotoi: u32 = 1;
        let mut a: u32 = 0;
        let mut na: u32 = 0;
        let mut optimal: u32 = 0;

        // C: for (i = 0, twotoi = 1; twotoi > 0 && *pna > twotoi/2; i++, twotoi *= 2)
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

    /// Tally integer key `key` into `nums`; returns 1 if it was a valid
    /// array-part candidate, 0 otherwise.
    /// C: `static int countint (lua_Integer key, unsigned int *nums)`.
    fn count_int(key: i64, nums: &mut [u32]) -> bool {
        let k = Self::array_index(key);
        if k != 0 {
            // C: nums[luaO_ceillog2(k)]++;
            nums[ceil_log2(k) as usize] += 1;
            true
        } else {
            false
        }
    }

    /// Count keys in the array part; fill `nums` histogram.
    /// Returns total non-nil key count in the array part.
    /// C: `static unsigned int numusearray (const Table *t, unsigned int *nums)`.
    fn num_use_array(&self, nums: &mut [u32]) -> u32 {
        // C: unsigned int asize = limitasasize(t); (check_exp(isrealasize(t), t->alimit))
        debug_assert!(self.is_real_asize(), "numusearray: alimit must be real size");
        let asize = self.alimit as usize;
        let mut ause: u32 = 0;
        let mut i: usize = 1; // 1-based Lua index

        // C: for (lg = 0, ttlg = 1; lg <= MAXABITS; lg++, ttlg *= 2)
        let mut ttlg: usize = 1;
        for lg in 0..=(MAXABITS as usize) {
            let mut lc: u32 = 0;
            let lim = if ttlg > asize { asize } else { ttlg };
            if i > lim { break; }
            while i <= lim {
                if !matches!(self.array[i - 1], LuaValue::Nil) {
                    lc += 1;
                }
                i += 1;
            }
            nums[lg] += lc;
            ause += lc;
            ttlg = ttlg.saturating_mul(2);
        }
        ause
    }

    /// Count keys in the hash part; update `nums` and `pna`.
    /// Returns total number of non-nil entries in the hash part.
    /// C: `static int numusehash (const Table *t, unsigned int *nums, unsigned int *pna)`.
    fn num_use_hash(&self, nums: &mut [u32], pna: &mut u32) -> i32 {
        let mut totaluse: i32 = 0;
        let mut ause: u32 = 0;
        let mut i = self.node.len();
        // C: while (i--) { Node *n = &t->node[i]; if (!isempty(gval(n))) { ... } }
        while i > 0 {
            i -= 1;
            let n = &self.node[i];
            if !matches!(n.value, LuaValue::Nil) {
                if n.key_is_int() {
                    if Self::count_int(n.key_int(), nums) {
                        ause += 1;
                    }
                }
                totaluse += 1;
            }
        }
        *pna += ause;
        totaluse
    }

    // ── setnodevector ───────────────────────────────────────────────────────

    /// (Re)initialise the hash part with `size` nodes, or become dummy if `size == 0`.
    /// C: `static void setnodevector (lua_State *L, Table *t, unsigned int size)`.
    fn set_node_vector(
        &mut self,
        _state: &mut LuaState,
        size: u32,
    ) -> Result<(), LuaError> {
        if size == 0 {
            // C: t->node = dummynode; t->lsizenode = 0; t->lastfree = NULL;
            self.node = Vec::new();
            self.lsizenode = 0;
            self.lastfree = None;
        } else {
            let lsize = ceil_log2(size);
            // C: if (lsize > MAXHBITS || (1u << lsize) > MAXHSIZE)
            if lsize as u32 > MAXHBITS || (1u32 << lsize) > MAXHSIZE {
                return Err(LuaError::runtime(format_args!("table overflow")));
            }
            let actual_size = 1u32 << lsize;
            // C: t->node = luaM_newvector(L, size, Node);
            let mut nodes = Vec::with_capacity(actual_size as usize);
            for _ in 0..actual_size {
                nodes.push(TableNode::empty());
            }
            self.node = nodes;
            self.lsizenode = lsize as u8;
            // C: t->lastfree = gnode(t, size);  /* all positions are free */
            // In Rust: one-past-end index.
            self.lastfree = Some(actual_size as usize);
        }
        Ok(())
    }

    // ── reinsert ────────────────────────────────────────────────────────────

    /// Re-insert all non-nil entries from `old_nodes` into `self`.
    /// C: `static void reinsert (lua_State *L, Table *ot, Table *t)`.
    fn reinsert(
        &mut self,
        state: &mut LuaState,
        old_nodes: Vec<(LuaValue, LuaValue)>,
    ) -> Result<(), LuaError> {
        for (k, v) in old_nodes {
            // C: luaH_set(L, t, &k, gval(old));
            self.set(state, &k, v)?;
        }
        Ok(())
    }

    // ── luaH_resize ─────────────────────────────────────────────────────────

    /// Resize the table to the new array and hash sizes.
    ///
    /// C: `void luaH_resize (lua_State *L, Table *t, unsigned int newasize,
    ///                                               unsigned int nhsize)` (`LUAI_FUNC`).
    ///
    /// PORT NOTE: In C, `exchangehashpart` swaps three pointers between two
    /// `Table` local values to handle allocation-failure rollback atomically.
    /// In Rust we replicate the logic using explicit field swaps and early
    /// returns on `Result::Err`.
    pub fn resize(
        &mut self,
        state: &mut LuaState,
        new_asize: u32,
        nhsize: u32,
    ) -> Result<(), LuaError> {
        // C: unsigned int oldasize = setlimittosize(t);
        let old_asize = self.set_limit_to_size();

        // Create a new temporary table to hold the new hash part.
        let mut new_hash_node: Vec<TableNode> = Vec::new();
        let mut new_hash_lsize: u8 = 0;
        let mut new_hash_lastfree: Option<usize> = None;

        // C: setnodevector(L, &newt, nhsize);
        {
            let mut tmp = LuaTable::empty();
            tmp.set_node_vector(state, nhsize)?;
            new_hash_node = tmp.node;
            new_hash_lsize = tmp.lsizenode;
            new_hash_lastfree = tmp.lastfree;
        }

        if new_asize < old_asize {
            // Array is shrinking: move vanishing array entries to the new hash.
            // C: t->alimit = newasize; exchangehashpart(t, &newt);
            // Swap hash part in so the inserts go into the new hash.
            self.alimit = new_asize;
            std::mem::swap(&mut self.node, &mut new_hash_node);
            std::mem::swap(&mut self.lsizenode, &mut new_hash_lsize);
            std::mem::swap(&mut self.lastfree, &mut new_hash_lastfree);

            // C: for (i = newasize; i < oldasize; i++) luaH_setint(L, t, i+1, &t->array[i]);
            for i in (new_asize as usize)..(old_asize as usize) {
                if !matches!(self.array[i], LuaValue::Nil) {
                    let v = self.array[i].clone();
                    self.set_int(state, (i + 1) as i64, v)?;
                }
            }

            // C: t->alimit = oldasize; exchangehashpart(t, &newt);  /* restore for error */
            self.alimit = old_asize;
            std::mem::swap(&mut self.node, &mut new_hash_node);
            std::mem::swap(&mut self.lsizenode, &mut new_hash_lsize);
            std::mem::swap(&mut self.lastfree, &mut new_hash_lastfree);
        }

        // Reallocate the array part.
        // C: newarray = luaM_reallocvector(L, t->array, oldasize, newasize, TValue);
        // In Rust: resize the Vec.
        // C: if (l_unlikely(newarray == NULL && newasize > 0)) { freehash(&newt); luaM_error(L); }
        self.array.resize_with(new_asize as usize, || LuaValue::Nil);

        // C: exchangehashpart(t, &newt);  /* 't' has new hash, 'newt' has old */
        std::mem::swap(&mut self.node, &mut new_hash_node);
        std::mem::swap(&mut self.lsizenode, &mut new_hash_lsize);
        std::mem::swap(&mut self.lastfree, &mut new_hash_lastfree);
        self.alimit = new_asize;

        // C: for (i = oldasize; i < newasize; i++) setempty(&t->array[i]);
        // (Already done by resize_with above.)

        // C: reinsert(L, &newt, t);  /* 'newt' now has the old hash */
        // Collect old hash entries from `new_hash_node` (now holding the old hash).
        let old_hash_entries: Vec<(LuaValue, LuaValue)> = new_hash_node
            .iter()
            .filter(|n| !matches!(n.value, LuaValue::Nil))
            .map(|n| (n.key_value(), n.value.clone()))
            .collect();
        // C: freehash(L, &newt);  (old hash freed automatically when Vec drops)
        drop(new_hash_node);
        self.reinsert(state, old_hash_entries)?;

        Ok(())
    }

    /// Resize only the array part, keeping the current hash size.
    /// C: `void luaH_resizearray (lua_State *L, Table *t, unsigned int nasize)` (`LUAI_FUNC`).
    pub fn resize_array(
        &mut self,
        state: &mut LuaState,
        nasize: u32,
    ) -> Result<(), LuaError> {
        // C: int nsize = allocsizenode(t);  luaH_resize(L, t, nasize, nsize);
        let nsize = self.alloc_sizenode();
        self.resize(state, nasize, nsize)
    }

    // ── rehash ──────────────────────────────────────────────────────────────

    /// Rehash the whole table to accommodate `extra_key` as a new key.
    /// Counts all existing keys, computes the optimal array size, then
    /// resizes.
    /// C: `static void rehash (lua_State *L, Table *t, const TValue *ek)`.
    fn rehash(&mut self, state: &mut LuaState, extra_key: &LuaValue) -> Result<(), LuaError> {
        let mut nums = [0u32; MAXABITS as usize + 1];
        self.set_limit_to_size();

        // C: na = numusearray(t, nums); totaluse = na;
        let na = self.num_use_array(&mut nums);
        let mut na = na;
        let mut totaluse = na as i32;

        // C: totaluse += numusehash(t, nums, &na);
        totaluse += self.num_use_hash(&mut nums, &mut na);

        // C: if (ttisinteger(ek)) na += countint(ivalue(ek), nums);
        if let LuaValue::Int(ek) = extra_key {
            if Self::count_int(*ek, &mut nums) {
                na += 1;
            }
        }
        totaluse += 1;

        // C: asize = computesizes(nums, &na);
        let asize = Self::compute_sizes(&nums, &mut na);

        // C: luaH_resize(L, t, asize, totaluse - na);
        let nh = (totaluse - na as i32).max(0) as u32;
        self.resize(state, asize, nh)
    }

    // ── luaH_new ────────────────────────────────────────────────────────────

    /// Create a new empty table.
    ///
    /// C: `Table *luaH_new (lua_State *L)` (`LUAI_FUNC`).
    ///
    /// PORT NOTE: In C, `luaC_newobj` allocates through the GC and returns a
    /// `GCObject *`.  In Phase A we use `Rc::new`.  Phase D will route through
    /// the real GC allocator.
    pub fn new(_state: &mut LuaState) -> GcRef<LuaTable> {
        // C: GCObject *o = luaC_newobj(L, LUA_VTABLE, sizeof(Table));
        //    Table *t = gco2t(o);
        //    t->metatable = NULL; t->flags = cast_byte(maskflags);
        //    t->array = NULL; t->alimit = 0;
        //    setnodevector(L, t, 0);
        // TODO(port): maskflags value depends on TM_EQ from ltm.h; using 0x7F placeholder.
        let t = LuaTable {
            flags: TableFlags(0x7F), // maskflags — "no metamethod present" bits all set
            lsizenode: 0,
            alimit: 0,
            array: Vec::new(),
            node: Vec::new(),
            lastfree: None,
            metatable: None,
        };
        GcRef::new(t)
    }

    /// Construct an empty `LuaTable` (not GC-wrapped).
    /// Used internally as a temporary during resize operations.
    fn empty() -> LuaTable {
        LuaTable {
            flags: TableFlags(0x7F),
            lsizenode: 0,
            alimit: 0,
            array: Vec::new(),
            node: Vec::new(),
            lastfree: None,
            metatable: None,
        }
    }

    // ── luaH_free: handled by Drop ──────────────────────────────────────────
    // C: `void luaH_free (lua_State *L, Table *t)` calls freehash + luaM_freearray + luaM_free.
    // In Rust the Vec<T> fields drop automatically; no explicit free needed.

    // ── getfreepos ──────────────────────────────────────────────────────────

    /// Find a free slot in the hash part, scanning backwards from `lastfree`.
    /// Returns `Some(index)` of a free node, or `None` if the hash is full.
    /// C: `static Node *getfreepos (Table *t)`.
    fn get_free_pos(&mut self) -> Option<usize> {
        // C: if (!isdummy(t)) { while (t->lastfree > t->node) { t->lastfree--; if (keyisnil) return } }
        if self.is_dummy() { return None; }
        loop {
            let lf = self.lastfree?;
            if lf == 0 {
                // C: loop exits when lastfree == node (offset 0 → no more to try)
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

    // ── luaH_newkey ─────────────────────────────────────────────────────────

    /// Insert a new key-value pair into the hash part (or grow the table).
    ///
    /// Implements Brent's variation: if `mp` (main position) is occupied, the
    /// occupying node is moved to a free slot unless it is itself in its main
    /// position, in which case the new node is linked after it.
    ///
    /// C: `static void luaH_newkey (lua_State *L, Table *t, const TValue *key,
    ///                                              TValue *value)`.
    fn new_key(
        &mut self,
        state: &mut LuaState,
        key: &LuaValue,
        value: LuaValue,
    ) -> Result<(), LuaError> {
        // C: if (l_unlikely(ttisnil(key))) luaG_runerror(L, "table index is nil");
        if matches!(key, LuaValue::Nil) {
            return Err(LuaError::runtime(format_args!("table index is nil")));
        }

        // Normalise float keys that have integer values.
        // C: else if (ttisfloat(key)) { lua_Number f = fltvalue(key); lua_Integer k;
        //      if (luaV_flttointeger(f, &k, F2Ieq)) { setivalue(&aux, k); key = &aux; }
        //      else if (l_unlikely(luai_numisnan(f))) luaG_runerror(L, "table index is NaN");
        let normalised_key;
        let key = if let LuaValue::Float(f) = key {
            let f = *f;
            if f.is_nan() {
                return Err(LuaError::runtime(format_args!("table index is NaN")));
            }
            // luaV_flttointeger with F2Ieq: exact integer representation only
            let k = f as i64;
            if k as f64 == f {
                normalised_key = LuaValue::Int(k);
                &normalised_key
            } else {
                key
            }
        } else {
            key
        };

        // C: if (ttisnil(value)) return;  /* do not insert nil values */
        if matches!(value, LuaValue::Nil) { return Ok(()); }

        let mp = self.main_position(key);

        // C: if (!isempty(gval(mp)) || isdummy(t)) { ... /* main position taken */ }
        let mp_occupied = !matches!(self.node[mp].value, LuaValue::Nil) || self.is_dummy();
        if mp_occupied {
            let f = self.get_free_pos();
            let f = match f {
                None => {
                    // C: rehash(L, t, key); luaH_set(L, t, key, value); return;
                    self.rehash(state, key)?;
                    return self.set(state, key, value);
                }
                Some(idx) => idx,
            };

            debug_assert!(!self.is_dummy());
            let othern = self.main_position_from_node(mp);

            if othern != mp {
                // C: colliding node is NOT in its main position — move it to f.
                // Find the node that chains to mp.
                let mut prev = othern;
                while (prev as isize + self.node[prev].next as isize) as usize != mp {
                    prev = (prev as isize + self.node[prev].next as isize) as usize;
                }
                // C: gnext(othern) = cast_int(f - othern);
                self.node[prev].next = (f as isize - prev as isize) as i32;
                // C: *f = *mp;  (copy the colliding node into free position)
                let mp_key = self.node[mp].key_value();
                let mp_val = self.node[mp].value.clone();
                let mp_next = self.node[mp].next;
                self.node[f].key = mp_key;
                self.node[f].value = mp_val;
                // C: if (gnext(mp) != 0) { gnext(f) += cast_int(mp - f); gnext(mp) = 0; }
                if mp_next != 0 {
                    self.node[f].next = mp_next + (mp as isize - f as isize) as i32;
                    self.node[mp].next = 0;
                } else {
                    self.node[f].next = 0;
                }
                // C: setempty(gval(mp));
                self.node[mp].value = LuaValue::Nil;
                // Now mp is free for our new key.
            } else {
                // C: colliding node IS in its main position — new key goes to f.
                // C: if (gnext(mp) != 0) gnext(f) = cast_int((mp + gnext(mp)) - f);
                if self.node[mp].next != 0 {
                    let target = (mp as isize + self.node[mp].next as isize) as usize;
                    self.node[f].next = (target as isize - f as isize) as i32;
                } else {
                    debug_assert!(self.node[f].next == 0);
                }
                // C: gnext(mp) = cast_int(f - mp);
                self.node[mp].next = (f as isize - mp as isize) as i32;
                // New entry goes into f.
                // mp_idx is now f after the chain update.
                // C: mp = f;
                // We write key/value into f below after the if-else.
                self.node[f].set_key(key);
                // C: luaC_barrierback(L, obj2gco(t), key);  (GC barrier; no-op in Phase A–C)
                debug_assert!(matches!(self.node[f].value, LuaValue::Nil));
                // C: setobj2t(L, gval(mp), value);
                self.node[f].value = value;
                return Ok(());
            }
        }

        // C: setnodekey(L, mp, key);
        self.node[mp].set_key(key);
        // C: luaC_barrierback(L, obj2gco(t), key);  (no-op)
        debug_assert!(matches!(self.node[mp].value, LuaValue::Nil));
        // C: setobj2t(L, gval(mp), value);
        self.node[mp].value = value;
        Ok(())
    }

    // ── luaH_getint ─────────────────────────────────────────────────────────

    /// Look up an integer key, returning a `TableSlotRef`.
    ///
    /// Checks the array part first (with the Xmilia trick for non-real-size
    /// alimit), then falls through to the hash part.
    ///
    /// C: `const TValue *luaH_getint (Table *t, lua_Integer key)` (`LUAI_FUNC`).
    pub fn get_int_slot(&self, key: i64) -> TableSlotRef {
        let alimit = self.alimit as u64;
        let uk = key as u64;

        // C: if (l_castS2U(key) - 1u < alimit)  /* 'key' in [1, t->alimit]? */
        if uk.wrapping_sub(1) < alimit {
            return TableSlotRef::Array((key - 1) as usize);
        }

        // C: else if (!isrealasize(t) && (((l_castS2U(key)-1u) & ~(alimit-1u)) < alimit))
        if !self.is_real_asize() {
            let masked = (uk.wrapping_sub(1)) & !(alimit.wrapping_sub(1));
            if masked < alimit {
                // C: t->alimit = cast_uint(key);  /* probably '#t' is here now */
                // PORT NOTE: This mutates alimit but C takes *mut Table; in Rust
                // we'd need &mut self. Since this is a "hint" update, we skip
                // mutation here and let Phase B decide on interior mutability.
                // TODO(port): alimit hint update in get_int_slot requires &mut self or RefCell.
                return TableSlotRef::Array((key - 1) as usize);
            }
        }

        // C: check the hash part
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

    // ── luaH_getshortstr ────────────────────────────────────────────────────

    /// Look up a short (interned) string key.
    /// C: `const TValue *luaH_getshortstr (Table *t, TString *key)` (`LUAI_FUNC`).
    pub fn get_short_str_slot(&self, key: &GcRef<LuaString>) -> TableSlotRef {
        debug_assert!(key.is_short());
        if self.is_dummy() { return TableSlotRef::Absent; }
        let mut n = self.hashpow2_idx(key.hash());
        loop {
            // C: if (keyisshrstr(n) && eqshrstr(keystrval(n), key))
            if self.node[n].key_is_short_str() {
                let ks = self.node[n].key_string();
                // C: eqshrstr = pointer equality (interned)
                if GcRef::ptr_eq(ks, key) {
                    return TableSlotRef::Hash(n);
                }
            }
            let nx = self.node[n].next;
            if nx == 0 {
                return TableSlotRef::Absent;
            }
            n = (n as isize + nx as isize) as usize;
        }
    }

    /// Look up any string key (dispatches to short-string path or generic path).
    /// C: `const TValue *luaH_getstr (Table *t, TString *key)` (`LUAI_FUNC`).
    pub fn get_str_slot(&self, key: &GcRef<LuaString>) -> TableSlotRef {
        if key.is_short() {
            self.get_short_str_slot(key)
        } else {
            // C: TValue ko; setsvalue(NULL, &ko, key); return getgeneric(t, &ko, 0);
            let ko = LuaValue::Str(key.clone());
            self.get_generic_slot(&ko, false)
        }
    }

    // ── luaH_get ────────────────────────────────────────────────────────────

    /// Main table lookup — dispatches by key type.
    /// C: `const TValue *luaH_get (Table *t, const TValue *key)` (`LUAI_FUNC`).
    pub fn get_slot(&self, key: &LuaValue) -> TableSlotRef {
        match key {
            LuaValue::Str(s) if s.is_short() => self.get_short_str_slot(s),
            LuaValue::Int(i) => self.get_int_slot(*i),
            LuaValue::Nil => TableSlotRef::Absent,
            LuaValue::Float(f) => {
                // C: if (luaV_flttointeger(fltvalue(key), &k, F2Ieq)) return luaH_getint(t, k);
                let f = *f;
                let k = f as i64;
                if k as f64 == f {
                    self.get_int_slot(k)
                } else {
                    self.get_generic_slot(key, false)
                }
            }
            _ => self.get_generic_slot(key, false),
        }
    }

    // ── Value accessors from a slot ref ─────────────────────────────────────

    /// Read the `LuaValue` at a slot reference.
    pub fn slot_value(&self, slot: TableSlotRef) -> LuaValue {
        match slot {
            TableSlotRef::Array(i) => self.array[i].clone(),
            TableSlotRef::Hash(i) => self.node[i].value.clone(),
            TableSlotRef::Absent => LuaValue::Nil,
        }
    }

    // ── luaH_finishset ──────────────────────────────────────────────────────

    /// Finish a raw set operation: either insert a new key or overwrite an
    /// existing slot.
    ///
    /// C: `void luaH_finishset (lua_State *L, Table *t, const TValue *key,
    ///                          const TValue *slot, TValue *value)` (`LUAI_FUNC`).
    pub fn finish_set(
        &mut self,
        state: &mut LuaState,
        key: &LuaValue,
        slot: TableSlotRef,
        value: LuaValue,
    ) -> Result<(), LuaError> {
        match slot {
            // C: if (isabstkey(slot)) luaH_newkey(L, t, key, value);
            TableSlotRef::Absent => self.new_key(state, key, value),
            // C: else setobj2t(L, cast(TValue *, slot), value);
            TableSlotRef::Array(i) => {
                self.array[i] = value;
                Ok(())
            }
            TableSlotRef::Hash(i) => {
                self.node[i].value = value;
                Ok(())
            }
        }
    }

    // ── luaH_set ────────────────────────────────────────────────────────────

    /// General table set.
    /// C: `void luaH_set (lua_State *L, Table *t, const TValue *key, TValue *value)` (`LUAI_FUNC`).
    pub fn set(
        &mut self,
        state: &mut LuaState,
        key: &LuaValue,
        value: LuaValue,
    ) -> Result<(), LuaError> {
        // C: const TValue *slot = luaH_get(t, key);  luaH_finishset(L, t, key, slot, value);
        let slot = self.get_slot(key);
        self.finish_set(state, key, slot, value)
    }

    // ── luaH_setint ─────────────────────────────────────────────────────────

    /// Set an integer key.
    /// C: `void luaH_setint (lua_State *L, Table *t, lua_Integer key, TValue *value)` (`LUAI_FUNC`).
    pub fn set_int(
        &mut self,
        state: &mut LuaState,
        key: i64,
        value: LuaValue,
    ) -> Result<(), LuaError> {
        let slot = self.get_int_slot(key);
        match slot {
            TableSlotRef::Absent => {
                // C: TValue k; setivalue(&k, key); luaH_newkey(L, t, &k, value);
                let k = LuaValue::Int(key);
                self.new_key(state, &k, value)
            }
            TableSlotRef::Array(i) => { self.array[i] = value; Ok(()) }
            TableSlotRef::Hash(i) => { self.node[i].value = value; Ok(()) }
        }
    }

    // ── hash_search ─────────────────────────────────────────────────────────

    /// Find a boundary in the hash part by doubling then binary-searching.
    ///
    /// Pre: caller guarantees `j+1` is present in the table.
    /// Returns the boundary index `i` such that `t[i]` is present and `t[i+1]`
    /// is absent (or `j` if `t[LUA_MAXINTEGER]` is present).
    ///
    /// C: `static lua_Unsigned hash_search (Table *t, lua_Unsigned j)`.
    fn hash_search(&self, mut j: u64) -> u64 {
        let mut i: u64;
        if j == 0 { j = 1; } // caller ensures j+1 is present
        loop {
            i = j;
            if j <= (i64::MAX as u64) / 2 {
                j *= 2;
            } else {
                j = i64::MAX as u64;
                // C: if (isempty(luaH_getint(t, j))) break; else return j;
                if matches!(self.get_int_slot(j as i64), TableSlotRef::Absent)
                    || matches!(self.slot_value(self.get_int_slot(j as i64)), LuaValue::Nil)
                {
                    break;
                } else {
                    return j;
                }
            }
            // C: while (!isempty(luaH_getint(t, j)))
            let slot = self.get_int_slot(j as i64);
            if matches!(slot, TableSlotRef::Absent) { break; }
            if matches!(self.slot_value(slot), LuaValue::Nil) { break; }
        }
        // Binary search between i (present) and j (absent).
        // C: while (j - i > 1u) { lua_Unsigned m = (i + j) / 2; ... }
        while j - i > 1 {
            let m = i / 2 + j / 2; // avoid overflow
            let slot = self.get_int_slot(m as i64);
            let empty = matches!(slot, TableSlotRef::Absent)
                || matches!(self.slot_value(slot), LuaValue::Nil);
            if empty { j = m; } else { i = m; }
        }
        i
    }

    // ── binsearch ───────────────────────────────────────────────────────────

    /// Binary search for the boundary in the array part between indices `i`
    /// (present) and `j` (absent).
    /// C: `static unsigned int binsearch (const TValue *array, unsigned int i,
    ///                                    unsigned int j)`.
    fn bin_search(array: &[LuaValue], mut i: u32, mut j: u32) -> u32 {
        // C: while (j - i > 1u) { unsigned int m = (i + j) / 2; ... }
        while j - i > 1 {
            let m = (i + j) / 2;
            if matches!(array[(m - 1) as usize], LuaValue::Nil) {
                j = m;
            } else {
                i = m;
            }
        }
        i
    }

    // ── luaH_getn ───────────────────────────────────────────────────────────

    /// Find a boundary of the table: an integer index `i` such that `t[i]`
    /// is present and `t[i+1]` is absent, or 0 if `t[1]` is absent.
    ///
    /// C: `lua_Unsigned luaH_getn (Table *t)` (`LUAI_FUNC`).
    ///
    /// PORT NOTE: The C function mutates `t->alimit` as a hint; this requires
    /// `&mut self` in Rust.  The C code also modifies `t->flags` via
    /// `setnorealasize`.  All mutations are faithfully ported.
    pub fn getn(&mut self) -> u64 {
        let limit = self.alimit;

        // (1) t[limit] is empty: boundary must be before limit.
        if limit > 0 && matches!(self.array[(limit - 1) as usize], LuaValue::Nil) {
            if limit >= 2 && !matches!(self.array[(limit - 2) as usize], LuaValue::Nil) {
                // C: 'limit - 1' is a boundary; can it be a new limit?
                if self.is_pow2_real_asize() && !Self::is_pow2(limit - 1) {
                    self.alimit = limit - 1;
                    self.flags.set_no_real_asize();
                }
                return (limit - 1) as u64;
            } else {
                // C: binary search in [0, limit]
                let boundary = Self::bin_search(&self.array, 0, limit);
                // Can this boundary be the new limit?
                if self.is_pow2_real_asize() && boundary > self.real_asize() / 2 {
                    self.alimit = boundary;
                    self.flags.set_no_real_asize();
                }
                return boundary as u64;
            }
        }

        // (2) t[limit] is present and array has more elements after limit.
        if !self.limit_equals_asize() {
            if matches!(self.array[limit as usize], LuaValue::Nil) {
                // C: 'limit + 1' is empty → limit is the boundary
                return limit as u64;
            }
            // Check the last element of the array part.
            let real = self.real_asize();
            if matches!(self.array[(real - 1) as usize], LuaValue::Nil) {
                let old_alimit = self.alimit;
                let boundary = Self::bin_search(&self.array, old_alimit, real);
                self.alimit = boundary;
                return boundary as u64;
            }
            // New limit is present → fall through to hash check.
            // (limit is now real_asize)
        }

        // (3) limit is the last element; check the hash part.
        let limit = self.real_asize();
        debug_assert!(
            limit == self.real_asize()
                && (limit == 0 || !matches!(self.array[(limit - 1) as usize], LuaValue::Nil))
        );
        // C: if (isdummy(t) || isempty(luaH_getint(t, cast(lua_Integer, limit + 1))))
        let next_key = (limit as i64).saturating_add(1);
        let next_slot = self.get_int_slot(next_key);
        let next_empty = matches!(next_slot, TableSlotRef::Absent)
            || matches!(self.slot_value(next_slot), LuaValue::Nil);
        if self.is_dummy() || next_empty {
            return limit as u64;
        }
        self.hash_search(limit as u64)
    }

    // ── Debug (LUA_DEBUG only) ──────────────────────────────────────────────

    /// Return the main position of `key` (for test/debug use).
    /// C: `Node *luaH_mainposition (const Table *t, const TValue *key)` (LUA_DEBUG only).
    #[cfg(debug_assertions)]
    pub fn mainposition(&self, key: &LuaValue) -> usize {
        self.main_position(key)
    }
}

// ── Module-level free functions ───────────────────────────────────────────────

/// Hash a `f64` to an `i32` bucket index.
///
/// Uses `frexp` decomposition to produce a well-distributed integer hash.
/// Handles inf/NaN by returning 0.
///
/// C: `static int l_hashfloat (lua_Number n)` (guarded by `#if !defined(l_hashfloat)`).
fn hash_float(n: f64) -> i32 {
    // C: n = l_mathop(frexp)(n, &i) * -cast_num(INT_MIN);
    // frexp: splits n into mantissa ∈ [0.5, 1) and exponent e such that n = m * 2^e
    if n.is_nan() || n.is_infinite() {
        return 0;
    }
    let (mantissa, exp) = frexp(n);
    // C: n = mantissa * -(INT_MIN as f64)  (≈ mantissa * 2147483648.0)
    let scaled = mantissa * -(i32::MIN as f64);
    // C: if (!lua_numbertointeger(n, &ni)) return 0;
    let ni = scaled as i64;
    if ni as f64 != scaled {
        return 0; // inf/NaN check after multiplication
    }
    // C: unsigned int u = cast_uint(i) + cast_uint(ni);
    //    return cast_int(u <= cast_uint(INT_MAX) ? u : ~u);
    let u = (exp as u32).wrapping_add(ni as u32);
    if u <= i32::MAX as u32 { u as i32 } else { !(u as i32) }
}

/// Decompose `x` into mantissa ∈ `[0.5, 1)` and integer exponent.
/// Equivalent to C's `frexp(3)`.
fn frexp(x: f64) -> (f64, i32) {
    // PORT NOTE: Rust std does not expose frexp directly. We replicate it using
    // the IEEE 754 bit layout.  This is equivalent to libm frexp.
    if x == 0.0 || x.is_nan() || x.is_infinite() {
        return (x, 0);
    }
    let bits = x.to_bits();
    let exp_bits = ((bits >> 52) & 0x7FFu64) as i32;
    if exp_bits == 0 {
        // Denormalised number: scale up.
        let scaled = x * (2.0f64.powi(64));
        let (m, e) = frexp(scaled);
        return (m, e - 64);
    }
    let exp = exp_bits - 1022; // biased exponent minus (bias - 1) gives exponent + 1
    // Set exponent to -1 (bits = 0x3FE) to get mantissa in [0.5, 1)
    let mantissa_bits = (bits & !(0x7FFu64 << 52)) | (0x3FEu64 << 52);
    (f64::from_bits(mantissa_bits), exp)
}

// ──────────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        src/ltable.c  (995 lines, 28 functions)
//   target_crate:  lua-vm
//   confidence:    medium
//   todos:         5
//   port_notes:    6
//   unsafe_blocks: 0
//   notes:         Logic faithfully ported; main structural changes are (1)
//                  TableSlotRef replaces const TValue * return pattern,
//                  (2) node indices replace C pointer arithmetic (nodefromval),
//                  (3) get_int_slot alimit-hint mutation needs &mut self in Phase B,
//                  (4) frexp implemented via IEEE 754 bit manipulation,
//                  (5) GC barriers are no-ops per PORTING.md §4.5 Phase A–C,
//                  (6) maskflags / TM_EQ constant is a placeholder (0x7F).
// ──────────────────────────────────────────────────────────────────────────────
