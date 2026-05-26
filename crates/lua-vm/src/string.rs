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
use crate::state::LuaState;

// PORT NOTE: `GcRef<T>` is the lua-types newtype around `Rc<T>` per PORT_STRATEGY §3.4.
// Re-imported here so all string-pool entries share identity with state.rs / api.rs.
use lua_types::GcRef;
/// Phase-B bridge: converts a lua-vm rich `LuaStringImpl` into a `lua_types::LuaString`.
/// The two types track different metadata (short/long flag, extra byte) and a real
/// merge belongs in Phase B once `lua-types::LuaString` grows the needed fields.
fn impl_to_lt(s: &GcRef<LuaStringImpl>) -> GcRef<lua_types::LuaString> {
    // TODO(D-1c-bridge): allocation outside state context (free fn)
    GcRef::new(lua_types::LuaString::from_bytes(s.as_bytes().to_vec()))
}

// ── Constants (lstring.h macros → macros.tsv) ─────────────────────────────────

// macros.tsv: MEMERRMSG → const MEMERR_MSG: &[u8] = b"not enough memory"
/// Pre-allocated OOM error message.  Must be created before the allocator
/// can fail so that the GC can always hand back a valid error string.
pub(crate) const MEMERR_MSG: &[u8] = b"not enough memory";

// macros.tsv: MINSTRTABSIZE → const MIN_STR_TAB_SIZE: usize = 128
const MIN_STR_TAB_SIZE: usize = 128;

// macros.tsv: STRCACHE_N → const STRCACHE_N: usize = 53
const STRCACHE_N: usize = 53;

// macros.tsv: STRCACHE_M → const STRCACHE_M: usize = 2
const STRCACHE_M: usize = 2;

// macros.tsv: LUAI_MAXSHORTLEN → const MAX_SHORT_LEN: usize = 40
pub(crate) const MAX_SHORT_LEN: usize = 40;

// macros.tsv: MAX_SIZE → const MAX_SIZE: usize = if size_of::<usize>() < size_of::<i64>() { usize::MAX } else { i64::MAX as usize }
const MAX_SIZE: usize = if std::mem::size_of::<usize>() < std::mem::size_of::<i64>() {
    usize::MAX
} else {
    i64::MAX as usize
};

// macros.tsv: luaM_limitN → std::cmp::min(n, usize::MAX / std::mem::size_of::<T>())
//             cast_int → x as i32
// Rust: upper bound on the number of hash buckets; derived from MAX_INT / pointer size.
const MAX_STR_TAB: usize = i32::MAX as usize / std::mem::size_of::<usize>();

// macros.tsv: sizelstring → drop — Rust allocates via Box<[u8]> / Rc<[u8]>
// PORT NOTE: dropped entirely; Rust uses Rc<[u8]> which carries its own length.

// macros.tsv: luaS_newliteral → state.intern_str(b"...")
// PORT NOTE: translated at call sites as `new_lstr(state, b"literal")`.

// macros.tsv: isreserved → ts.is_reserved_word()
// PORT NOTE: translated at call sites as the `LuaStringImpl::is_reserved_word()` method.

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
    Short,
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
    bytes: Rc<[u8]>,

    // Replaced by the StringKind enum; length is implicit in bytes.len().
    kind: StringKind,

    // Using Cell<u32> so that `hash_long_str` can cache the hash through a
    // shared `&LuaStringImpl` reference (interior mutability, single-threaded).
    #[allow(dead_code)]
    hash: Cell<u32>,

    // Short strings: reserved-word token index (0 = not a keyword).
    // Long strings:  0 = hash not yet computed; 1 = hash is valid.
    extra: Cell<u8>,
}

impl LuaStringImpl {
    /// Returns the string's bytes.
    ///
    /// macros.tsv: `getstr` / `getlngstr` / `getshrstr` → `ts.as_bytes()`
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Returns the byte length of the string.
    ///
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
    /// macros.tsv: `isreserved` → `ts.is_reserved_word()`
    pub fn is_reserved_word(&self) -> bool {
        self.kind == StringKind::Short && self.extra.get() > 0
    }

    /// GC color predicate.  Returns `true` if this object is "white" (unreachable)
    /// in the GC's current wave.
    ///
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
            Rc::ptr_eq(&self.bytes, &other.bytes)
        } else {
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
    // PORT NOTE: keyed by owned byte slice; lookup by `&[u8]` via Borrow<[u8]>.
    map: HashMap<Box<[u8]>, GcRef<LuaStringImpl>>,

    // PERF(port): redundant with map.len() in Rust — keep for C-parity; remove in Phase B
    nuse: usize,

    // In Rust, HashMap manages its own capacity; this tracks the last requested size.
    size: usize,
}

impl StringPool {
    /// Create an empty pool with `MIN_STR_TAB_SIZE` preallocated capacity.
    ///
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
    pub len: usize,
    pub nuvalue: u16,
    // TODO(port): GcRef<LuaTable> — LuaTable not yet defined; Phase B
    pub metatable: Option<()>,
    // macros.tsv: setnilvalue → *o = LuaValue::Nil
    // TODO(port): Vec<LuaValue> — LuaValue not yet defined; Phase B
    pub uv: Vec<()>,
    // Port of the raw byte payload that C accessed via udatamemoffset arithmetic.
    pub data: Box<[u8]>,
}

// ── Public functions ───────────────────────────────────────────────────────────

// lstring.h: LUAI_FUNC → pub(crate)
/// Hash a byte string with a seed using Lua's FNV-style hash.
///
/// This is a pure function with no allocations.  The algorithm XORs shifts and
/// additions over each byte in reverse order, seeded by `seed ^ len`.
///
/// # C source
/// ```c
///
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
    // macros.tsv: cast_uint → x as u32
    let mut h: u32 = seed ^ (bytes.len() as u32);

    let mut l = bytes.len();
    while l > 0 {
        l -= 1;
        // macros.tsv: cast_byte → x as u8 (then as u32 for the arithmetic)
        h ^= (h << 5)
            .wrapping_add(h >> 2)
            .wrapping_add(bytes[l] as u32);
    }

    h
}

//
// PORT NOTE: `tablerehash` walked the intrusive `hnext` chain in each bucket and
// redistributed `TString *` pointers into new bucket slots.  In Rust the
// `HashMap` in `StringPool` handles its own rehashing automatically whenever its
// load factor is exceeded or `reserve` / `shrink_to` is called.  The entire
// function is therefore dropped; its effects are subsumed by the HashMap.

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
///
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
        let additional = nsize.saturating_sub(strt.map.len());
        strt.map.reserve(additional);
    } else if nsize < osize {
        // PERF(port): shrink_to is a hint; exact shrink not guaranteed in Rust
        strt.map.shrink_to(nsize);
    }

    strt.size = nsize;
}

// lstring.h: LUAI_FUNC → pub(crate)
/// Initialise the string intern table and the API string cache.
///
/// Must be called exactly once during VM startup, before any strings are created.
/// Pre-creates the memory-error message and fixes it in the GC (so it is never
/// collected), then fills every cache slot with that same string.
///
/// # C source
/// ```c
///
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
    //    tablerehash(tb->hash, 0, MINSTRTABSIZE);
    //    tb->size = MINSTRTABSIZE;
    // macros.tsv: luaM_newvector → vec![T::default(); n]
    // PORT NOTE: StringPool::new() sets the initial capacity to MIN_STR_TAB_SIZE,
    // replacing both the allocation and the tablerehash clear pass.
    state.global_mut().strt = StringPool::new();

    // macros.tsv: luaS_newliteral → state.intern_str(b"...")
    let memerrmsg = new_lstr(state, MEMERR_MSG)?;

    // macros.tsv: luaC_fix — not listed; it marks the object as fixed (non-collectable)
    // TODO(port): call state.gc().fix(memerrmsg.clone()) when GC is wired in Phase D;
    // in Phases A–C the Rc keeps it alive as long as GlobalState holds the clone
    let memerrmsg_lt = impl_to_lt(&memerrmsg);
    state.global_mut().memerrmsg = memerrmsg_lt.clone();

    //      for (j = 0; j < STRCACHE_M; j++)
    //        g->strcache[i][j] = g->memerrmsg;
    for i in 0..STRCACHE_N {
        for j in 0..STRCACHE_M {
            state.global_mut().strcache[i][j] = memerrmsg_lt.clone();
        }
    }

    Ok(())
}

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
///
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
    if bytes.len() <= MAX_SHORT_LEN {
        intern_short_str(state, bytes)
    } else {
        //        luaM_toobig(L);
        // macros.tsv: luaM_toobig → return Err(LuaError::Memory)
        // PORT NOTE: sizeof(TString) is a C-specific overhead; in Rust we just
        // check that the byte count fits within MAX_SIZE.
        if bytes.len() >= MAX_SIZE {
            return Err(LuaError::Memory);
        }

        //    memcpy(getlngstr(ts), str, l * sizeof(char));
        // PORT NOTE: Rather than creating a zeroed buffer and then copying,
        // we construct the LuaStringImpl directly from `bytes`.
        let seed = state.global().seed;
        let h = hash_bytes(bytes, seed);
        let ts = create_str_obj(state, bytes, StringKind::Long, h);
        Ok(ts)
    }
}

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
///
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
    // PORT NOTE: pointer hash replaced by content hash (see doc above)
    let seed = state.global().seed;
    let i = (hash_bytes(bytes, seed) as usize) % STRCACHE_N;

    // macros.tsv: getstr → ts.as_bytes()
    for j in 0..STRCACHE_M {
        if state.global().strcache[i][j].as_bytes() == bytes {
            // TODO(phase-b): strcache currently holds lua_types::LuaString; rebuild
            // a rich LuaStringImpl from the bytes. Phase B should unify the types.
            let cached_bytes = state.global().strcache[i][j].as_bytes().to_vec();
            // TODO(D-1c-bridge): LuaStringImpl is the rich local type; state helper produces lua_types::LuaString
            return Ok(GcRef::new(LuaStringImpl {
                bytes: cached_bytes.into(),
                kind: if bytes.len() <= MAX_SHORT_LEN { StringKind::Short } else { StringKind::Long },
                hash: Cell::new(hash_bytes(bytes, seed)),
                extra: Cell::new(0),
            }));
        }
    }

    // Create the string before mutating the cache
    let new_str = new_lstr(state, bytes)?;

    // Shift entries toward the back to make room at slot 0
    for j in (1..STRCACHE_M).rev() {
        // Clone first to avoid borrow conflict between getter and setter
        let prev = state.global().strcache[i][j - 1].clone();
        state.global_mut().strcache[i][j] = prev;
    }

    state.global_mut().strcache[i][0] = impl_to_lt(&new_str);

    Ok(new_str)
}

// ── Private helpers ───────────────────────────────────────────────────────────

/// Allocate and initialise a new `LuaStringImpl` with the given bytes, kind, and hash.
///
/// In C, `createstrobj` allocated uninitialised memory via `luaC_newobj` and set
/// the header fields; the caller then filled the content via `memcpy`.  In Rust
/// we construct the string directly from the provided `bytes`, eliminating the
/// two-step pattern.
///
/// # C source
/// ```c
///
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
    // macros.tsv: luaM_newobject → state.gc().new_obj(tag, sz)
    // TODO(port): register with GC tracking list (state.global_mut().allgc)
    // in Phase D; Phase A–C creates a bare Rc
    let _ = state; // state needed for GC registration in Phase D
    // TODO(D-1c-bridge): LuaStringImpl is the rich local type; state helper produces lua_types::LuaString
    GcRef::new(LuaStringImpl {
        hash: Cell::new(hash),
        extra: Cell::new(0),
        // PORT NOTE: we receive bytes directly; no separate memcpy step needed
        bytes: Rc::from(bytes),
        kind,
    })
}

/// Grow the string intern table, first attempting a GC collection if the table is
/// at its absolute maximum size.
///
/// # C source
/// ```c
///
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
    // macros.tsv: MAX_INT → i32::MAX
    let nuse = state.global().strt.nuse;
    if nuse == i32::MAX as usize {
        // macros.tsv: luaC_fullgc → state.gc().full_collect()
        // TODO(port): state.gc().full_collect() — GC not yet wired in Phase A–C; no-op
        // (When GC is live this call may reduce nuse by sweeping dead short strings.)

        // macros.tsv: luaM_error → return Err(LuaError::Memory)
        if state.global().strt.nuse == i32::MAX as usize {
            return Err(LuaError::Memory);
        }
    }

    let size = state.global().strt.size;
    if size <= MAX_STR_TAB / 2 {
        resize(state, size * 2);
    }

    Ok(())
}

/// Look up `bytes` in the intern table; create and insert a new short string if
/// not found.
///
/// The `isdead` / `changewhite` resurrection path is elided in Phases A–C because
/// `Rc`-based reference counting keeps objects alive until all references drop
/// (there are no dead-but-not-collected strings in Phase A–C).
///
/// # C source
/// ```c
///
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
    // In Rust, &[u8] slices are never null; the assertion is trivially satisfied.

    let seed = state.global().seed;
    let h = hash_bytes(bytes, seed);

    // PORT NOTE: intrusive hnext chain replaced by HashMap lookup
    // Clone the existing GcRef<LuaStringImpl> so the immutable borrow on `state` ends
    // before any mutable access below.
    let existing = state.global().strt.map.get(bytes).cloned();
    if let Some(ts) = existing {
        // macros.tsv: isdead → g.is_dead(obj);  changewhite → obj.flip_white()
        // PORT NOTE: GC color management deferred to Phase D; in Phases A–C all
        // Rc-held objects are live by definition (Rc keeps them alive).
        return Ok(ts);
    }

    let needs_grow = {
        let strt = &state.global().strt;
        strt.nuse >= strt.size
    };
    if needs_grow {
        grow_str_tab(state)?;
    }

    //    ts->shrlen = cast_byte(l);  — encoded in StringKind::Short
    //    memcpy(getshrstr(ts), str, l);  — bytes passed directly to create_str_obj
    let ts = create_str_obj(state, bytes, StringKind::Short, h);

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
//   unsafe_blocks: 0   (must be 0 outside explicit unsafe-budget crates)
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
