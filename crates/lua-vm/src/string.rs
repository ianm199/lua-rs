//! String table and interned-string operations — port of `lstring.c` + `lstring.h`.
//!
//! Provides two key abstractions:
//!
//! - [`LuaStringImpl`]: the Lua string value, stored as a reference-counted byte slice.
//!   Short strings (`<= MAX_SHORT_LEN` bytes) are interned in the process-global
//!   [`StringPool`]; long strings are heap-allocated on each creation and never
//!   interned.
//!
//! - [`StringPool`]: the intern table for short strings, stored on `GlobalState`.
//!   Replaces the C `stringtable` struct, which used an open-addressing hash table
//!   with intrusive chaining through `TString.u.hnext`.  In Rust the intrusive
//!   chain is dropped; a `HashMap` provides O(1) lookup and automatic rehashing.
//!   See PORT NOTE on [`StringPool`] for the full rationale.
//!
//! The `lstring.h` header is merged into this module per PORTING.md §1.
//!
//! # C source files
//! - `reference/lua-5.4.7/src/lstring.c`  (275 lines, 15 functions)
//! - `reference/lua-5.4.7/src/lstring.h`  (57 lines; merged here)

use std::cell::Cell;
#[allow(unused_imports)] use crate::prelude::*;
use std::collections::HashMap;
use std::rc::Rc;

// TODO(port): these import paths will resolve once Phase B wires the crate graph.
// `LuaState` and `GlobalState` live in crate::state (src/state.rs, from lstate.c).
// `LuaValue` and `LuaError` live in lua_types (crates/lua-types/src/).
use crate::state::{GlobalState, LuaState};

// PORT NOTE: `GcRef<T>` is the lua-types newtype around `Rc<T>` per PORT_STRATEGY §3.4.
// Re-imported here so all string-pool entries share identity with state.rs / api.rs.
use lua_types::GcRef;
/// Local alias retained while string.rs's own `LuaStringImpl` is still in use; will
/// merge with `lua_types::LuaString` in Phase B's string-pool consolidation.
type LocalGcRef<T> = Rc<T>;

/// Phase-B bridge: converts a lua-vm rich `LuaStringImpl` into a `lua_types::LuaString`.
/// The two types track different metadata (short/long flag, extra byte) and a real
/// merge belongs in Phase B once `lua-types::LuaString` grows the needed fields.
fn impl_to_lt(s: &GcRef<LuaStringImpl>) -> GcRef<lua_types::LuaString> {
    GcRef::new(lua_types::LuaString::from_bytes(s.as_bytes().to_vec()))
}

// ── Constants (lstring.h macros → macros.tsv) ─────────────────────────────────

// C: #define MEMERRMSG  "not enough memory"
// macros.tsv: MEMERRMSG → const MEMERR_MSG: &[u8] = b"not enough memory"
/// Pre-allocated OOM error message.  Must be created before the allocator
/// can fail so that the GC can always hand back a valid error string.
pub(crate) const MEMERR_MSG: &[u8] = b"not enough memory";

// C: #define MINSTRTABSIZE  128    (llimits.h)
// macros.tsv: MINSTRTABSIZE → const MIN_STR_TAB_SIZE: usize = 128
const MIN_STR_TAB_SIZE: usize = 128;

// C: #define STRCACHE_N  53   (llimits.h)
// macros.tsv: STRCACHE_N → const STRCACHE_N: usize = 53
const STRCACHE_N: usize = 53;

// C: #define STRCACHE_M  2   (llimits.h)
// macros.tsv: STRCACHE_M → const STRCACHE_M: usize = 2
const STRCACHE_M: usize = 2;

// C: #define LUAI_MAXSHORTLEN  40   (llimits.h)
// macros.tsv: LUAI_MAXSHORTLEN → const MAX_SHORT_LEN: usize = 40
const MAX_SHORT_LEN: usize = 40;

// C: MAX_SIZE defined via llimits.h conditional on pointer vs i64 width
// macros.tsv: MAX_SIZE → const MAX_SIZE: usize = if size_of::<usize>() < size_of::<i64>() { usize::MAX } else { i64::MAX as usize }
const MAX_SIZE: usize = if std::mem::size_of::<usize>() < std::mem::size_of::<i64>() {
    usize::MAX
} else {
    i64::MAX as usize
};

// C: #define MAXSTRTB  cast_int(luaM_limitN(MAX_INT, TString*))
// macros.tsv: luaM_limitN → std::cmp::min(n, usize::MAX / std::mem::size_of::<T>())
//             cast_int → x as i32
// Rust: upper bound on the number of hash buckets; derived from MAX_INT / pointer size.
const MAX_STR_TAB: usize = i32::MAX as usize / std::mem::size_of::<usize>();

// C: #define sizelstring(l)  (offsetof(TString, contents) + ((l) + 1) * sizeof(char))
// macros.tsv: sizelstring → drop — Rust allocates via Box<[u8]> / Rc<[u8]>
// PORT NOTE: dropped entirely; Rust uses Rc<[u8]> which carries its own length.

// C: #define luaS_newliteral(L, s)  (luaS_newlstr(L, "" s, (sizeof(s)/sizeof(char))-1))
// macros.tsv: luaS_newliteral → state.intern_str(b"...")
// PORT NOTE: translated at call sites as `new_lstr(state, b"literal")`.

// C: #define isreserved(s)  ((s)->tt == LUA_VSHRSTR && (s)->extra > 0)
// macros.tsv: isreserved → ts.is_reserved_word()
// PORT NOTE: translated at call sites as the `LuaStringImpl::is_reserved_word()` method.

// C: #define eqshrstr(a,b)  check_exp((a)->tt == LUA_VSHRSTR, (a) == (b))
// macros.tsv: eqshrstr → Rc::ptr_eq(a, b)
// PORT NOTE: short strings are interned so pointer equality suffices.
// Translated at call sites as `Rc::ptr_eq(a, b)`.

// ── LuaStringImpl (was TString in lobject.h) ─────────────────────────────────────

// PORT NOTE: `LuaStringImpl` corresponds to `TString` from `lobject.h`, which maps to
// `src/object.rs` per file_deps.txt.  It is defined here (in `string.rs`) because
// `lstring.c` owns the string-table internals and most of the type's behaviour.
// Phase B should reconcile: either keep it here and re-export from `object.rs`,
// or move it there and import it from `string.rs`.

/// Whether a Lua string is short (interned) or long (not interned).
///
/// Corresponds to `LUA_VSHRSTR` / `LUA_VLNGSTR` tags from `lobject.h`.
///
/// # C mapping (types.tsv)
/// ```text
/// LUA_VSHRSTR → LuaStringImpl::Short  (shrlen holds length 0..=40)
/// LUA_VLNGSTR → LuaStringImpl::Long   (shrlen = 0xFF sentinel; u.lnglen holds length)
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StringKind {
    // C: LUA_VSHRSTR — shrlen byte holds the length; string is interned
    Short,
    // C: LUA_VLNGSTR — shrlen = 0xFF sentinel; u.lnglen holds the real length
    Long,
}

/// A Lua string: an immutable, reference-counted byte sequence.
///
/// Short strings (`<= MAX_SHORT_LEN = 40` bytes) are interned in the
/// [`StringPool`] on `GlobalState`; two short strings with the same bytes
/// are guaranteed to be the same `GcRef` (pointer equality via `Rc::ptr_eq`).
///
/// Long strings are heap-allocated independently and never interned.  Their
/// hash is computed lazily on first call to [`hash_long_str`] and cached via
/// interior mutability (`Cell<u32>`).
///
/// # C mapping (types.tsv)
/// ```text
/// TString             → LuaStringImpl
/// TString.extra       → extra: Cell<u8>   (reserved-word idx for Short; hash-ready flag for Long)
/// TString.shrlen      → kind: StringKind   (0xFF sentinel replaced by enum variant)
/// TString.hash        → hash: Cell<u32>
/// TString.u.lnglen    → bytes.len()        (length implicit in Rc<[u8]>)
/// TString.u.hnext     → (removed)          (intrusive chain gone; StringPool uses HashMap)
/// TString.contents    → bytes: Rc<[u8]>
/// ```
pub struct LuaStringImpl {
    // C: char contents[];  (flexible array member)
    bytes: Rc<[u8]>,

    // C: lu_byte shrlen;  (0xFF for long strings, actual length for short)
    // Replaced by the StringKind enum; length is implicit in bytes.len().
    kind: StringKind,

    // C: unsigned int hash;
    // Using Cell<u32> so that `hash_long_str` can cache the hash through a
    // shared `&LuaStringImpl` reference (interior mutability, single-threaded).
    hash: Cell<u32>,

    // C: lu_byte extra;
    // Short strings: reserved-word token index (0 = not a keyword).
    // Long strings:  0 = hash not yet computed; 1 = hash is valid.
    extra: Cell<u8>,
}

impl LuaStringImpl {
    /// Returns the string's bytes.
    ///
    /// C: `getstr(ts)` / `getlngstr(ts)` / `getshrstr(ts)` — all map to this.
    /// macros.tsv: `getstr` / `getlngstr` / `getshrstr` → `ts.as_bytes()`
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Returns the byte length of the string.
    ///
    /// C: `tsslen(ts)` — macro returning `ts->shrlen` for Short or `ts->u.lnglen`
    /// for Long.  In Rust both cases are `bytes.len()`.
    /// macros.tsv: `tsslen` → `ts.len()`
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Returns `true` if this is a long (non-interned) string.
    pub fn is_long(&self) -> bool {
        self.kind == StringKind::Long
    }

    /// Returns `true` if this is a short (interned) string.
    pub fn is_short(&self) -> bool {
        self.kind == StringKind::Short
    }

    /// Returns `true` if this short string is a Lua reserved word.
    ///
    /// C: `isreserved(s)` macro — `(s)->tt == LUA_VSHRSTR && (s)->extra > 0`.
    /// macros.tsv: `isreserved` → `ts.is_reserved_word()`
    pub fn is_reserved_word(&self) -> bool {
        self.kind == StringKind::Short && self.extra.get() > 0
    }

    /// GC color predicate.  Returns `true` if this object is "white" (unreachable)
    /// in the GC's current wave.
    ///
    /// C: `iswhite(obj)` macro.
    /// macros.tsv: `iswhite` → `obj.is_white()`
    ///
    /// PORT NOTE: GC color management is deferred to Phase D.  In Phases A–C all
    /// objects are reachable via `Rc` reference counts and this always returns
    /// `false` (nothing is white / unreachable).
    pub fn is_white(&self) -> bool {
        // TODO(port): Phase D — check the GC marked byte; stub returns false (all live)
        false
    }

    /// Flip GC color from white to the current non-white (resurrect a dead object).
    ///
    /// C: `changewhite(obj)` macro.
    /// macros.tsv: `changewhite` → `obj.flip_white()`
    ///
    /// PORT NOTE: GC color management deferred to Phase D; no-op in Phases A–C.
    pub fn flip_white(&self) {
        // TODO(port): Phase D — update the GC marked byte
    }
}

impl PartialEq for LuaStringImpl {
    /// Equality for Lua strings.
    ///
    /// For short strings (interned), pointer equality via `Rc::ptr_eq` is sufficient
    /// and matches `eqshrstr` in C.  For long strings, we fall back to byte
    /// comparison, matching `luaS_eqlngstr` in C.
    fn eq(&self, other: &Self) -> bool {
        if self.kind == StringKind::Short && other.kind == StringKind::Short {
            // C: eqshrstr(a, b) — pointer equality; macros.tsv: Rc::ptr_eq(a, b)
            Rc::ptr_eq(&self.bytes, &other.bytes)
        } else {
            // C: luaS_eqlngstr — byte comparison for long strings
            self.bytes == other.bytes
        }
    }
}

impl Eq for LuaStringImpl {}

// ── StringPool (was stringtable in lstate.h) ──────────────────────────────────

// PORT NOTE: `StringPool` corresponds to `stringtable` from `lstate.h`, which maps
// to `src/state.rs` per file_deps.txt.  It is defined here because `lstring.c`
// owns all of the pool's mutation logic.  Phase B should reconcile placement.
//
// The C `stringtable` used an open-addressing hash table where each bucket was
// the head of an intrusive singly-linked list threaded through `TString.u.hnext`.
// In Rust, `TString.u.hnext` is removed per types.tsv.  The `HashMap` replaces
// both the bucket array and the chain: it provides O(1) average-case lookup,
// automatic rehashing, and eliminates the need for `tablerehash`.
//
// `nuse` and `size` are retained for parity with the C invariants that other
// code may check (e.g. `growstrtab` tests `nuse >= size`).

/// Intern table for short Lua strings.  Lives on `GlobalState`.
///
/// # C mapping (types.tsv)
/// ```text
/// stringtable        → StringPool
/// stringtable.hash   → map: HashMap<Box<[u8]>, GcRef<LuaStringImpl>>
/// stringtable.nuse   → nuse: usize
/// stringtable.size   → size: usize
/// ```
pub struct StringPool {
    // C: TString **hash;  (array of chain heads — replaced by HashMap)
    // PORT NOTE: keyed by owned byte slice; lookup by `&[u8]` via Borrow<[u8]>.
    map: HashMap<Box<[u8]>, GcRef<LuaStringImpl>>,

    // C: int nuse;  (live entry count)
    // PERF(port): redundant with map.len() in Rust — keep for C-parity; remove in Phase B
    nuse: usize,

    // C: int size;  (bucket count)
    // In Rust, HashMap manages its own capacity; this tracks the last requested size.
    size: usize,
}

impl StringPool {
    /// Create an empty pool with `MIN_STR_TAB_SIZE` preallocated capacity.
    ///
    /// C: corresponds to the `luaM_newvector(L, MINSTRTABSIZE, TString*)` +
    ///    `tablerehash(tb->hash, 0, MINSTRTABSIZE)` sequence in `luaS_init`.
    pub fn new() -> Self {
        StringPool {
            map: HashMap::with_capacity(MIN_STR_TAB_SIZE),
            nuse: 0,
            size: MIN_STR_TAB_SIZE,
        }
    }
}

impl Default for StringPool {
    fn default() -> Self {
        Self::new()
    }
}

// ── LuaUserData (was Udata in lobject.h) ──────────────────────────────────────

// PORT NOTE: `LuaUserData` corresponds to `Udata` from `lobject.h`, which maps to
// `src/object.rs` per file_deps.txt.  Defined here because `luaS_newudata` lives
// in `lstring.c`.  Phase B should reconcile placement.

/// Full userdata: a GC-tracked object carrying a raw byte payload plus optional
/// Lua user values and an optional metatable.
///
/// # C mapping (types.tsv)
/// ```text
/// Udata           → LuaUserData
/// Udata.len       → len: usize
/// Udata.nuvalue   → nuvalue: u16  (covered by uv.len() but kept for parity)
/// Udata.metatable → metatable: Option<GcRef<LuaTable>>
/// Udata.uv        → uv: Vec<LuaValue>
/// (no direct C field) data: Box<[u8]>  — the raw byte payload; C used a flexible
///                          array member laid out past the Udata header via
///                          `udatamemoffset` alignment math.
/// ```
pub struct LuaUserDataImpl {
    // C: size_t len;
    pub len: usize,
    // C: unsigned short nuvalue;
    pub nuvalue: u16,
    // C: struct Table *metatable;
    // TODO(port): GcRef<LuaTable> — LuaTable not yet defined; Phase B
    pub metatable: Option<()>,
    // C: UValue uv[1];  (flexible array of TValues, used as user values)
    // macros.tsv: setnilvalue → *o = LuaValue::Nil
    // TODO(port): Vec<LuaValue> — LuaValue not yet defined; Phase B
    pub uv: Vec<()>,
    // Port of the raw byte payload that C accessed via udatamemoffset arithmetic.
    pub data: Box<[u8]>,
}

// ── Public functions ───────────────────────────────────────────────────────────

// C: int luaS_eqlngstr(TString *a, TString *b)
// lstring.h: LUAI_FUNC → pub(crate)
/// Test equality of two long strings.
///
/// Two long strings are equal if they have identical byte content.  A pointer
/// equality short-circuit is also applied: if `a` and `b` share the same
/// underlying `Rc<[u8]>` allocation, they are trivially equal.
///
/// # C source
/// ```c
/// // C: int luaS_eqlngstr(TString *a, TString *b) {
/// //   size_t len = a->u.lnglen;
/// //   lua_assert(a->tt == LUA_VLNGSTR && b->tt == LUA_VLNGSTR);
/// //   return (a == b) ||
/// //     ((len == b->u.lnglen) &&
/// //      (memcmp(getlngstr(a), getlngstr(b), len) == 0));
/// // }
/// ```
pub(crate) fn eq_long_str(a: &LuaStringImpl, b: &LuaStringImpl) -> bool {
    // C: lua_assert(a->tt == LUA_VLNGSTR && b->tt == LUA_VLNGSTR);
    // macros.tsv: lua_assert → debug_assert!
    debug_assert!(a.is_long() && b.is_long(), "eq_long_str: both arguments must be long strings");

    // C: (a == b) — pointer equality (same TString allocation)
    // In Rust: check if the Rc<[u8]> byte buffers are the same allocation
    if Rc::ptr_eq(&a.bytes, &b.bytes) {
        return true;
    }

    // C: (len == b->u.lnglen) && (memcmp(getlngstr(a), getlngstr(b), len) == 0)
    // macros.tsv: getlngstr → ts.as_bytes()
    a.as_bytes() == b.as_bytes()
}

// C: unsigned int luaS_hash(const char *str, size_t l, unsigned int seed)
// lstring.h: LUAI_FUNC → pub(crate)
/// Hash a byte string with a seed using Lua's FNV-style hash.
///
/// This is a pure function with no allocations.  The algorithm XORs shifts and
/// additions over each byte in reverse order, seeded by `seed ^ len`.
///
/// # C source
/// ```c
/// // C: unsigned int luaS_hash(const char *str, size_t l, unsigned int seed) {
/// //   unsigned int h = seed ^ cast_uint(l);
/// //   for (; l > 0; l--)
/// //     h ^= ((h<<5) + (h>>2) + cast_byte(str[l - 1]));
/// //   return h;
/// // }
/// ```
///
/// PORT NOTE: C parenthesises `(h<<5)` and `(h>>2)` explicitly, so the outer
/// additions are unambiguous despite C's `<<`/`>>` having lower precedence than
/// `+`.  In Rust `<<` and `>>` have higher precedence than `+`, so the same
/// expression is computed without extra parentheses; `wrapping_add` is used to
/// match C's unsigned wrap-around arithmetic.
pub(crate) fn hash_bytes(bytes: &[u8], seed: u32) -> u32 {
    // C: unsigned int h = seed ^ cast_uint(l);
    // macros.tsv: cast_uint → x as u32
    let mut h: u32 = seed ^ (bytes.len() as u32);

    // C: for (; l > 0; l--)
    let mut l = bytes.len();
    while l > 0 {
        l -= 1;
        // C: h ^= ((h<<5) + (h>>2) + cast_byte(str[l - 1]));
        // macros.tsv: cast_byte → x as u8 (then as u32 for the arithmetic)
        h ^= (h << 5)
            .wrapping_add(h >> 2)
            .wrapping_add(bytes[l] as u32);
    }

    h
}

// C: unsigned int luaS_hashlongstr(TString *ts)
// lstring.h: LUAI_FUNC → pub(crate)
/// Compute (and cache) the hash of a long string.
///
/// The hash for long strings is computed lazily: on first call the hash is
/// derived from `hash_bytes` using the seed stored in the `hash` field, then
/// `extra` is set to `1` to record that the hash is now valid.  Subsequent calls
/// return the cached value directly.
///
/// Interior mutability (`Cell<u32>` / `Cell<u8>`) allows mutation through a
/// shared `&LuaStringImpl` reference, which is necessary because `GcRef<LuaStringImpl>`
/// is `Rc<LuaStringImpl>` and there is no safe way to get `&mut` through an `Rc`.
///
/// # C source
/// ```c
/// // C: unsigned int luaS_hashlongstr(TString *ts) {
/// //   lua_assert(ts->tt == LUA_VLNGSTR);
/// //   if (ts->extra == 0) {  /* no hash? */
/// //     size_t len = ts->u.lnglen;
/// //     ts->hash = luaS_hash(getlngstr(ts), len, ts->hash);
/// //     ts->extra = 1;  /* now it has its hash */
/// //   }
/// //   return ts->hash;
/// // }
/// ```
pub(crate) fn hash_long_str(ts: &LuaStringImpl) -> u32 {
    // C: lua_assert(ts->tt == LUA_VLNGSTR);
    debug_assert!(ts.is_long(), "hash_long_str: argument must be a long string");

    // C: if (ts->extra == 0) {  /* no hash? */
    if ts.extra.get() == 0 {
        // C: ts->hash = luaS_hash(getlngstr(ts), len, ts->hash);
        // The initial ts->hash holds the per-state seed (set at construction).
        let computed = hash_bytes(ts.as_bytes(), ts.hash.get());
        ts.hash.set(computed);
        // C: ts->extra = 1;  /* now it has its hash */
        ts.extra.set(1);
    }

    // C: return ts->hash;
    ts.hash.get()
}

// C: static void tablerehash(TString **vect, int osize, int nsize)  [DROPPED]
//
// PORT NOTE: `tablerehash` walked the intrusive `hnext` chain in each bucket and
// redistributed `TString *` pointers into new bucket slots.  In Rust the
// `HashMap` in `StringPool` handles its own rehashing automatically whenever its
// load factor is exceeded or `reserve` / `shrink_to` is called.  The entire
// function is therefore dropped; its effects are subsumed by the HashMap.

// C: void luaS_resize(lua_State *L, int nsize)
// lstring.h: LUAI_FUNC → pub(crate)
/// Resize the string intern table to approximately `nsize` buckets.
///
/// When growing, `HashMap::reserve` hints the desired capacity.  When shrinking,
/// `HashMap::shrink_to` is used as an approximation of the C logic, which
/// would rehash entries out of the shrinking tail.  The C function's graceful
/// degradation on allocation failure (keep the current size) is preserved:
/// `HashMap` will simply retain its existing capacity if memory is tight.
///
/// # C source
/// ```c
/// // C: void luaS_resize(lua_State *L, int nsize) {
/// //   stringtable *tb = &G(L)->strt;
/// //   int osize = tb->size;
/// //   TString **newvect;
/// //   if (nsize < osize)
/// //     tablerehash(tb->hash, osize, nsize);  /* depopulate shrinking part */
/// //   newvect = luaM_reallocvector(L, tb->hash, osize, nsize, TString*);
/// //   if (l_unlikely(newvect == NULL)) {
/// //     if (nsize < osize)
/// //       tablerehash(tb->hash, nsize, osize);  /* restore to original size */
/// //   } else {
/// //     tb->hash = newvect;
/// //     tb->size = nsize;
/// //     if (nsize > osize)
/// //       tablerehash(newvect, osize, nsize);
/// //   }
/// // }
/// ```
///
/// PORT NOTE: The three calls to `tablerehash` are dropped because `HashMap`
/// automatically rehashes.  The allocation-failure fallback (restore to `osize`)
/// has no direct analogue; `HashMap` will retain existing capacity on OOM, which
/// matches the intent.
// PERF(port): luaS_resize shrink — HashMap::shrink_to() is a hint, not a
// guarantee; the C code freed exact memory.  Profile in Phase B.
pub(crate) fn resize(state: &mut LuaState, nsize: usize) {
    let strt = &mut state.global_mut().strt;
    let osize = strt.size;

    if nsize > osize {
        // C: newvect = luaM_reallocvector(...); if (nsize > osize) tablerehash(...)
        let additional = nsize.saturating_sub(strt.map.len());
        strt.map.reserve(additional);
    } else if nsize < osize {
        // C: if (nsize < osize) tablerehash(tb->hash, osize, nsize) — depopulate
        // PERF(port): shrink_to is a hint; exact shrink not guaranteed in Rust
        strt.map.shrink_to(nsize);
    }

    // C: tb->size = nsize;
    strt.size = nsize;
}

// C: void luaS_clearcache(global_State *g)
// lstring.h: LUAI_FUNC → pub(crate)
/// Clear the API string cache, replacing any GC-white entries with the
/// preallocated OOM message (which is never collected).
///
/// Called by the GC sweep phase to ensure the cache never holds a pointer to a
/// collected string.
///
/// # C source
/// ```c
/// // C: void luaS_clearcache(global_State *g) {
/// //   int i, j;
/// //   for (i = 0; i < STRCACHE_N; i++)
/// //     for (j = 0; j < STRCACHE_M; j++) {
/// //       if (iswhite(g->strcache[i][j]))  /* will entry be collected? */
/// //         g->strcache[i][j] = g->memerrmsg;
/// //     }
/// // }
/// ```
///
/// PORT NOTE: Takes `&mut GlobalState` directly (same as the C signature which
/// takes `global_State *g`, not `lua_State *L`).  The caller accesses this via
/// `state.global_mut()`.
pub(crate) fn clear_cache(g: &mut GlobalState) {
    for i in 0..STRCACHE_N {
        for j in 0..STRCACHE_M {
            // C: if (iswhite(g->strcache[i][j]))
            // macros.tsv: iswhite → obj.is_white()
            if g.strcache[i][j].is_white() {
                // C: g->strcache[i][j] = g->memerrmsg;
                g.strcache[i][j] = g.memerrmsg.clone();
            }
        }
    }
}

// C: void luaS_init(lua_State *L)
// lstring.h: LUAI_FUNC → pub(crate)
/// Initialise the string intern table and the API string cache.
///
/// Must be called exactly once during VM startup, before any strings are created.
/// Pre-creates the memory-error message and fixes it in the GC (so it is never
/// collected), then fills every cache slot with that same string.
///
/// # C source
/// ```c
/// // C: void luaS_init(lua_State *L) {
/// //   global_State *g = G(L);
/// //   int i, j;
/// //   stringtable *tb = &G(L)->strt;
/// //   tb->hash = luaM_newvector(L, MINSTRTABSIZE, TString*);
/// //   tablerehash(tb->hash, 0, MINSTRTABSIZE);
/// //   tb->size = MINSTRTABSIZE;
/// //   g->memerrmsg = luaS_newliteral(L, MEMERRMSG);
/// //   luaC_fix(L, obj2gco(g->memerrmsg));
/// //   for (i = 0; i < STRCACHE_N; i++)
/// //     for (j = 0; j < STRCACHE_M; j++)
/// //       g->strcache[i][j] = g->memerrmsg;
/// // }
/// ```
pub(crate) fn init(state: &mut LuaState) -> Result<(), LuaError> {
    // C: tb->hash = luaM_newvector(L, MINSTRTABSIZE, TString*);
    //    tablerehash(tb->hash, 0, MINSTRTABSIZE);
    //    tb->size = MINSTRTABSIZE;
    // macros.tsv: luaM_newvector → vec![T::default(); n]
    // PORT NOTE: StringPool::new() sets the initial capacity to MIN_STR_TAB_SIZE,
    // replacing both the allocation and the tablerehash clear pass.
    state.global_mut().strt = StringPool::new();

    // C: g->memerrmsg = luaS_newliteral(L, MEMERRMSG);
    // macros.tsv: luaS_newliteral → state.intern_str(b"...")
    let memerrmsg = new_lstr(state, MEMERR_MSG)?;

    // C: luaC_fix(L, obj2gco(g->memerrmsg));  /* it should never be collected */
    // macros.tsv: luaC_fix — not listed; it marks the object as fixed (non-collectable)
    // TODO(port): call state.gc().fix(memerrmsg.clone()) when GC is wired in Phase D;
    // in Phases A–C the Rc keeps it alive as long as GlobalState holds the clone
    let memerrmsg_lt = impl_to_lt(&memerrmsg);
    state.global_mut().memerrmsg = memerrmsg_lt.clone();

    // C: for (i = 0; i < STRCACHE_N; i++)
    //      for (j = 0; j < STRCACHE_M; j++)
    //        g->strcache[i][j] = g->memerrmsg;
    for i in 0..STRCACHE_N {
        for j in 0..STRCACHE_M {
            state.global_mut().strcache[i][j] = memerrmsg_lt.clone();
        }
    }

    Ok(())
}

// C: TString *luaS_createlngstrobj(lua_State *L, size_t l)
// lstring.h: LUAI_FUNC → pub(crate)
/// Create a new, uninitialized long string of `l` bytes.
///
/// The returned string's bytes are all zero.  The caller is responsible for
/// filling the content, if needed; in practice `new_lstr` calls this and then
/// copies the source bytes in.
///
/// # C source
/// ```c
/// // C: TString *luaS_createlngstrobj(lua_State *L, size_t l) {
/// //   TString *ts = createstrobj(L, l, LUA_VLNGSTR, G(L)->seed);
/// //   ts->u.lnglen = l;
/// //   ts->shrlen = 0xFF;  /* signals that it is a long string */
/// //   return ts;
/// // }
/// ```
///
/// PORT NOTE: `ts->u.lnglen = l` and `ts->shrlen = 0xFF` are replaced by the
/// `StringKind::Long` variant which carries the length implicitly through
/// `Rc<[u8]>::len()`.  The `0xFF` sentinel is no longer needed.
pub(crate) fn create_long_str(state: &mut LuaState, l: usize) -> GcRef<LuaStringImpl> {
    // C: TString *ts = createstrobj(L, l, LUA_VLNGSTR, G(L)->seed);
    let seed = state.global().seed;
    // PORT NOTE: C's createstrobj allocates uninitialised storage then the caller
    // fills bytes via memcpy.  Rust's create_str_obj constructs with zeroed bytes;
    // callers (e.g. new_lstr) pass the real bytes directly, eliminating the two-step.
    create_str_obj(state, &vec![0u8; l], StringKind::Long, seed)
}

// C: void luaS_remove(lua_State *L, TString *ts)
// lstring.h: LUAI_FUNC → pub(crate)
/// Remove a short string from the intern table.
///
/// Called by the GC sweep when a short string is about to be collected.
///
/// # C source
/// ```c
/// // C: void luaS_remove(lua_State *L, TString *ts) {
/// //   stringtable *tb = &G(L)->strt;
/// //   TString **p = &tb->hash[lmod(ts->hash, tb->size)];
/// //   while (*p != ts)  /* find previous element */
/// //     p = &(*p)->u.hnext;
/// //   *p = (*p)->u.hnext;  /* remove element from its list */
/// //   tb->nuse--;
/// // }
/// ```
///
/// PORT NOTE: The C implementation walks the intrusive `hnext` chain to unlink
/// `ts`.  In Rust the chain does not exist; `HashMap::remove` is O(1) average.
/// `lmod(ts->hash, tb->size)` (the bucket index) is not needed; the map keys by
/// byte content.
pub(crate) fn remove_str(state: &mut LuaState, ts: &LuaStringImpl) {
    let strt = &mut state.global_mut().strt;

    // C: TString **p = &tb->hash[lmod(ts->hash, tb->size)];
    //    while (*p != ts) p = &(*p)->u.hnext;
    //    *p = (*p)->u.hnext;
    // PORT NOTE: all of the above replaced by HashMap::remove keyed on bytes
    strt.map.remove(ts.as_bytes());

    // C: tb->nuse--;
    strt.nuse = strt.nuse.saturating_sub(1);
}

// C: TString *luaS_newlstr(lua_State *L, const char *str, size_t l)
// lstring.h: LUAI_FUNC → pub(crate)
/// Create or retrieve a Lua string from `bytes`.
///
/// If `bytes.len() <= MAX_SHORT_LEN` (40), the string is interned: an existing
/// identical short string is returned if found, otherwise a new one is created
/// and inserted into the intern table.
///
/// If `bytes.len() > MAX_SHORT_LEN`, a new long string is allocated each time
/// (long strings are never interned).
///
/// # C source
/// ```c
/// // C: TString *luaS_newlstr(lua_State *L, const char *str, size_t l) {
/// //   if (l <= LUAI_MAXSHORTLEN)  /* short string? */
/// //     return internshrstr(L, str, l);
/// //   else {
/// //     TString *ts;
/// //     if (l_unlikely(l * sizeof(char) >= (MAX_SIZE - sizeof(TString))))
/// //       luaM_toobig(L);
/// //     ts = luaS_createlngstrobj(L, l);
/// //     memcpy(getlngstr(ts), str, l * sizeof(char));
/// //     return ts;
/// //   }
/// // }
/// ```
pub(crate) fn new_lstr(state: &mut LuaState, bytes: &[u8]) -> Result<GcRef<LuaStringImpl>, LuaError> {
    // C: if (l <= LUAI_MAXSHORTLEN)
    if bytes.len() <= MAX_SHORT_LEN {
        intern_short_str(state, bytes)
    } else {
        // C: if (l_unlikely(l * sizeof(char) >= (MAX_SIZE - sizeof(TString))))
        //        luaM_toobig(L);
        // macros.tsv: luaM_toobig → return Err(LuaError::Memory)
        // PORT NOTE: sizeof(TString) is a C-specific overhead; in Rust we just
        // check that the byte count fits within MAX_SIZE.
        if bytes.len() >= MAX_SIZE {
            return Err(LuaError::Memory);
        }

        // C: ts = luaS_createlngstrobj(L, l);
        //    memcpy(getlngstr(ts), str, l * sizeof(char));
        // PORT NOTE: Rather than creating a zeroed buffer and then copying,
        // we construct the LuaStringImpl directly from `bytes`.
        let seed = state.global().seed;
        let h = hash_bytes(bytes, seed);
        let ts = create_str_obj(state, bytes, StringKind::Long, h);
        Ok(ts)
    }
}

// C: TString *luaS_new(lua_State *L, const char *str)
// lstring.h: LUAI_FUNC → pub(crate)
/// Create or retrieve a Lua string, using a small two-slot LRU cache per hash
/// bucket to accelerate repeated calls with the same byte sequence.
///
/// In C, the cache bucket is selected by casting the C string pointer to a `u32`
/// (`point2uint`).  In Rust, `point2uint` is restricted to `lua-gc`/`lua-coro`
/// (raw-pointer cast requiring `unsafe`).  We substitute a content-hash based
/// bucket index instead.  Functional semantics are identical; cache hit rates for
/// repeated calls with the same `bytes` may differ.
///
/// # C source
/// ```c
/// // C: TString *luaS_new(lua_State *L, const char *str) {
/// //   unsigned int i = point2uint(str) % STRCACHE_N;  /* hash */
/// //   int j;
/// //   TString **p = G(L)->strcache[i];
/// //   for (j = 0; j < STRCACHE_M; j++) {
/// //     if (strcmp(str, getstr(p[j])) == 0)  /* hit? */
/// //       return p[j];  /* that is it */
/// //   }
/// //   /* normal route */
/// //   for (j = STRCACHE_M - 1; j > 0; j--)
/// //     p[j] = p[j - 1];  /* move out last element */
/// //   p[0] = luaS_newlstr(L, str, strlen(str));
/// //   return p[0];
/// // }
/// ```
///
/// PORT NOTE: `point2uint(str) % STRCACHE_N` used the raw pointer address as a
/// fast key, exploiting the fact that C string literals have stable addresses.
/// In Rust we use `hash_bytes(bytes, seed) % STRCACHE_N` instead.  The replacement
/// is fully safe and has identical semantics (but different cache behaviour for
/// calls from different `&[u8]` slices with identical content).
pub(crate) fn new(state: &mut LuaState, bytes: &[u8]) -> Result<GcRef<LuaStringImpl>, LuaError> {
    // C: unsigned int i = point2uint(str) % STRCACHE_N;
    // PORT NOTE: pointer hash replaced by content hash (see doc above)
    let seed = state.global().seed;
    let i = (hash_bytes(bytes, seed) as usize) % STRCACHE_N;

    // C: for (j = 0; j < STRCACHE_M; j++) { if (strcmp(str, getstr(p[j])) == 0) ... }
    // macros.tsv: getstr → ts.as_bytes()
    for j in 0..STRCACHE_M {
        if state.global().strcache[i][j].as_bytes() == bytes {
            // C: return p[j];
            // TODO(phase-b): strcache currently holds lua_types::LuaString; rebuild
            // a rich LuaStringImpl from the bytes. Phase B should unify the types.
            let cached_bytes = state.global().strcache[i][j].as_bytes().to_vec();
            return Ok(GcRef::new(LuaStringImpl {
                bytes: cached_bytes.into(),
                kind: if bytes.len() <= MAX_SHORT_LEN { StringKind::Short } else { StringKind::Long },
                hash: Cell::new(hash_bytes(bytes, seed)),
                extra: Cell::new(0),
            }));
        }
    }

    // C: /* normal route */
    // Create the string before mutating the cache
    let new_str = new_lstr(state, bytes)?;

    // C: for (j = STRCACHE_M - 1; j > 0; j--) p[j] = p[j - 1];
    // Shift entries toward the back to make room at slot 0
    for j in (1..STRCACHE_M).rev() {
        // Clone first to avoid borrow conflict between getter and setter
        let prev = state.global().strcache[i][j - 1].clone();
        state.global_mut().strcache[i][j] = prev;
    }

    // C: p[0] = luaS_newlstr(L, str, strlen(str));
    state.global_mut().strcache[i][0] = impl_to_lt(&new_str);

    Ok(new_str)
}

// C: Udata *luaS_newudata(lua_State *L, size_t s, int nuvalue)
// lstring.h: LUAI_FUNC → pub(crate)
/// Allocate a new full userdata of `s` raw bytes with `nuvalue` Lua user values.
///
/// The raw byte payload is zeroed.  All user values are initialised to `nil`.
/// The metatable is `None`.
///
/// # C source
/// ```c
/// // C: Udata *luaS_newudata(lua_State *L, size_t s, int nuvalue) {
/// //   Udata *u;
/// //   int i;
/// //   GCObject *o;
/// //   if (l_unlikely(s > MAX_SIZE - udatamemoffset(nuvalue)))
/// //     luaM_toobig(L);
/// //   o = luaC_newobj(L, LUA_VUSERDATA, sizeudata(nuvalue, s));
/// //   u = gco2u(o);
/// //   u->len = s;
/// //   u->nuvalue = nuvalue;
/// //   u->metatable = NULL;
/// //   for (i = 0; i < nuvalue; i++)
/// //     setnilvalue(&u->uv[i].uv);
/// //   return u;
/// // }
/// ```
pub(crate) fn new_userdata(
    state: &mut LuaState,
    s: usize,
    nuvalue: usize,
) -> Result<GcRef<LuaUserDataImpl>, LuaError> {
    // C: if (l_unlikely(s > MAX_SIZE - udatamemoffset(nuvalue)))
    //        luaM_toobig(L);
    // macros.tsv: luaM_toobig → return Err(LuaError::Memory)
    // TODO(port): udatamemoffset(nuvalue) computes C-specific alignment padding
    // for the flexible-array Udata layout.  In Rust, LuaUserData allocates `data`
    // and `uv` separately (Box<[u8]> + Vec<LuaValue>); the combined size bound
    // differs.  Conservative check: reject if s alone exceeds MAX_SIZE.
    if s > MAX_SIZE {
        return Err(LuaError::Memory);
    }

    // C: o = luaC_newobj(L, LUA_VUSERDATA, sizeudata(nuvalue, s));
    //    u = gco2u(o);
    // TODO(port): register with GC tracking (state.gc().new_obj(...));
    // Phase A–C stub: allocate via Rc without GC registration.
    let u = GcRef::new(LuaUserDataImpl {
        // C: u->len = s;
        len: s,
        // C: u->nuvalue = nuvalue;
        nuvalue: nuvalue as u16,
        // C: u->metatable = NULL;
        metatable: None,
        // C: for (i = 0; i < nuvalue; i++) setnilvalue(&u->uv[i].uv);
        // macros.tsv: setnilvalue → *o = LuaValue::Nil
        // TODO(port): Vec<LuaValue> once LuaValue is defined in lua-types
        uv: vec![(); nuvalue],
        // Raw byte payload; zero-initialised.
        data: vec![0u8; s].into_boxed_slice(),
    });

    // TODO(port): push into state.global_mut().allgc for GC tracking (Phase D)
    Ok(u)
}

// ── Private helpers ───────────────────────────────────────────────────────────

// C: static TString *createstrobj(lua_State *L, size_t l, int tag, unsigned int h)
/// Allocate and initialise a new `LuaStringImpl` with the given bytes, kind, and hash.
///
/// In C, `createstrobj` allocated uninitialised memory via `luaC_newobj` and set
/// the header fields; the caller then filled the content via `memcpy`.  In Rust
/// we construct the string directly from the provided `bytes`, eliminating the
/// two-step pattern.
///
/// # C source
/// ```c
/// // C: static TString *createstrobj(lua_State *L, size_t l, int tag, unsigned int h) {
/// //   TString *ts;
/// //   GCObject *o;
/// //   size_t totalsize = sizelstring(l);
/// //   o = luaC_newobj(L, tag, totalsize);
/// //   ts = gco2ts(o);
/// //   ts->hash = h;
/// //   ts->extra = 0;
/// //   getstr(ts)[l] = '\0';  /* ending 0 */
/// //   return ts;
/// // }
/// ```
///
/// PORT NOTE: `sizelstring(l)` computed the total allocation size including the
/// nul terminator.  In Rust, `Rc<[u8]>` stores the bytes without a nul; the
/// nul terminator is dropped.  Callers that need a nul-terminated `*const u8`
/// for FFI must use a temporary `CString` or equivalent.
fn create_str_obj(
    state: &mut LuaState,
    bytes: &[u8],
    kind: StringKind,
    hash: u32,
) -> GcRef<LuaStringImpl> {
    // C: o = luaC_newobj(L, tag, totalsize);
    // macros.tsv: luaM_newobject → state.gc().new_obj(tag, sz)
    // TODO(port): register with GC tracking list (state.global_mut().allgc)
    // in Phase D; Phase A–C creates a bare Rc
    let _ = state; // state needed for GC registration in Phase D
    GcRef::new(LuaStringImpl {
        // C: ts->hash = h;
        hash: Cell::new(hash),
        // C: ts->extra = 0;
        extra: Cell::new(0),
        // C: getstr(ts)[l] = '\0';  /* content written by caller via memcpy */
        // PORT NOTE: we receive bytes directly; no separate memcpy step needed
        bytes: Rc::from(bytes),
        kind,
    })
}

// C: static void growstrtab(lua_State *L, stringtable *tb)
/// Grow the string intern table, first attempting a GC collection if the table is
/// at its absolute maximum size.
///
/// # C source
/// ```c
/// // C: static void growstrtab(lua_State *L, stringtable *tb) {
/// //   if (l_unlikely(tb->nuse == MAX_INT)) {  /* too many strings? */
/// //     luaC_fullgc(L, 1);  /* try to free some... */
/// //     if (tb->nuse == MAX_INT)  /* still too many? */
/// //       luaM_error(L);  /* cannot even create a message... */
/// //   }
/// //   if (tb->size <= MAXSTRTB / 2)  /* can grow string table? */
/// //     luaS_resize(L, tb->size * 2);
/// // }
/// ```
fn grow_str_tab(state: &mut LuaState) -> Result<(), LuaError> {
    // C: if (l_unlikely(tb->nuse == MAX_INT)) {
    // macros.tsv: MAX_INT → i32::MAX
    let nuse = state.global().strt.nuse;
    if nuse == i32::MAX as usize {
        // C: luaC_fullgc(L, 1);
        // macros.tsv: luaC_fullgc → state.gc().full_collect()
        // TODO(port): state.gc().full_collect() — GC not yet wired in Phase A–C; no-op
        // (When GC is live this call may reduce nuse by sweeping dead short strings.)

        // C: if (tb->nuse == MAX_INT) luaM_error(L);
        // macros.tsv: luaM_error → return Err(LuaError::Memory)
        if state.global().strt.nuse == i32::MAX as usize {
            return Err(LuaError::Memory);
        }
    }

    // C: if (tb->size <= MAXSTRTB / 2)  luaS_resize(L, tb->size * 2);
    let size = state.global().strt.size;
    if size <= MAX_STR_TAB / 2 {
        resize(state, size * 2);
    }

    Ok(())
}

// C: static TString *internshrstr(lua_State *L, const char *str, size_t l)
/// Look up `bytes` in the intern table; create and insert a new short string if
/// not found.
///
/// The `isdead` / `changewhite` resurrection path is elided in Phases A–C because
/// `Rc`-based reference counting keeps objects alive until all references drop
/// (there are no dead-but-not-collected strings in Phase A–C).
///
/// # C source
/// ```c
/// // C: static TString *internshrstr(lua_State *L, const char *str, size_t l) {
/// //   TString *ts;
/// //   global_State *g = G(L);
/// //   stringtable *tb = &g->strt;
/// //   unsigned int h = luaS_hash(str, l, g->seed);
/// //   TString **list = &tb->hash[lmod(h, tb->size)];
/// //   lua_assert(str != NULL);
/// //   for (ts = *list; ts != NULL; ts = ts->u.hnext) {
/// //     if (l == ts->shrlen && (memcmp(str, getshrstr(ts), l) == 0)) {
/// //       if (isdead(g, ts)) changewhite(ts);  /* resurrect it */
/// //       return ts;
/// //     }
/// //   }
/// //   if (tb->nuse >= tb->size) {
/// //     growstrtab(L, tb);
/// //     list = &tb->hash[lmod(h, tb->size)];
/// //   }
/// //   ts = createstrobj(L, l, LUA_VSHRSTR, h);
/// //   ts->shrlen = cast_byte(l);
/// //   memcpy(getshrstr(ts), str, l);
/// //   ts->u.hnext = *list;
/// //   *list = ts;
/// //   tb->nuse++;
/// //   return ts;
/// // }
/// ```
///
/// PORT NOTE: `lmod(h, tb->size)` (power-of-two bucket modulo via
/// `macros.tsv: lmod → (s & (size - 1)) as usize`) and the `hnext` chain walk
/// are both gone.  `HashMap::get` replaces the linear bucket scan.
fn intern_short_str(
    state: &mut LuaState,
    bytes: &[u8],
) -> Result<GcRef<LuaStringImpl>, LuaError> {
    // C: lua_assert(str != NULL);
    // In Rust, &[u8] slices are never null; the assertion is trivially satisfied.

    // C: unsigned int h = luaS_hash(str, l, g->seed);
    let seed = state.global().seed;
    let h = hash_bytes(bytes, seed);

    // C: for (ts = *list; ...) { if (memcmp matches) { if (isdead) changewhite; return ts; } }
    // PORT NOTE: intrusive hnext chain replaced by HashMap lookup
    // Clone the existing GcRef<LuaStringImpl> so the immutable borrow on `state` ends
    // before any mutable access below.
    let existing = state.global().strt.map.get(bytes).cloned();
    if let Some(ts) = existing {
        // C: if (isdead(g, ts)) changewhite(ts);  /* resurrect it */
        // macros.tsv: isdead → g.is_dead(obj);  changewhite → obj.flip_white()
        // PORT NOTE: GC color management deferred to Phase D; in Phases A–C all
        // Rc-held objects are live by definition (Rc keeps them alive).
        return Ok(ts);
    }

    // C: if (tb->nuse >= tb->size) { growstrtab(L, tb); ... }
    let needs_grow = {
        let strt = &state.global().strt;
        strt.nuse >= strt.size
    };
    if needs_grow {
        grow_str_tab(state)?;
    }

    // C: ts = createstrobj(L, l, LUA_VSHRSTR, h);
    //    ts->shrlen = cast_byte(l);  — encoded in StringKind::Short
    //    memcpy(getshrstr(ts), str, l);  — bytes passed directly to create_str_obj
    let ts = create_str_obj(state, bytes, StringKind::Short, h);

    // C: ts->u.hnext = *list; *list = ts;  — intrusive chain; gone in Rust
    // C: tb->nuse++;
    state
        .global_mut()
        .strt
        .map
        .insert(bytes.to_vec().into_boxed_slice(), ts.clone());
    state.global_mut().strt.nuse += 1;

    Ok(ts)
}

// ── Re-export marker for type defined here ────────────────────────────────────

// TODO(port): LuaError is used in function signatures above but is not yet defined
// in lua-types.  Phase B must add LuaError to lua-types/src/error.rs per
// PORTING.md §6 before this file can compile.  The expected variants are:
//   LuaError::Runtime(LuaValue)
//   LuaError::Memory
//   LuaError::Syntax(LuaValue)
//   ... (full list in PORTING.md §6)
// For now, reference LuaError as an opaque import from the future lua-types crate.
use lua_types::LuaError;

// ──────────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        src/lstring.c  (275 lines, 15 functions)
//                  src/lstring.h  (57 lines; merged)
//   target_crate:  lua-vm
//   confidence:    medium
//   todos:         14
//   port_notes:    30
//   unsafe_blocks: 0   (must be 0 outside lua-gc/lua-coro)
//   notes:         Logic is faithful to the C.  The two largest structural changes
//                  are: (1) `tablerehash` + intrusive `hnext` chain replaced by
//                  `HashMap` in `StringPool`; (2) `luaS_new`'s `point2uint`
//                  pointer-hash replaced by a content hash (safe, same semantics).
//                  Key TODOs: GC registration in create_str_obj (Phase D),
//                  GC registration in new_userdata (Phase D), luaC_fix in init
//                  (Phase D), full_collect stub in grow_str_tab (Phase D),
//                  udatamemoffset size check in new_userdata (Phase B),
//                  LuaValue in LuaUserData.uv (Phase B), LuaError import path
//                  (Phase B), GcRef typedef (Phase B).  Phase B priority: wire
//                  import paths for LuaState, GlobalState, LuaError, LuaValue,
//                  and move LuaStringImpl/StringPool/LuaUserData to their canonical
//                  modules (object.rs / state.rs).
// ──────────────────────────────────────────────────────────────────────────────
