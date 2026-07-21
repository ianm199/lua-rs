//! Global interpreter state.
//!
//! Manages per-thread ([`LuaState`]) and process-wide ([`GlobalState`]) Lua state:
//! creation, initialization, teardown, and coroutine lifecycle helpers. Mirrors
//! the responsibilities of Lua's `lstate.c`/`lstate.h`.
//!
//! C's `LX`/`LG` structs and the `fromstate` macro exist only to allocate the
//! main thread and global state as one contiguous block via pointer
//! arithmetic; here `GlobalState` and `LuaState` are separate heap-allocated
//! values linked via `Rc<RefCell<GlobalState>>`, so no equivalent is needed.

use std::cell::RefCell;
use std::hash::{BuildHasherDefault, Hasher};
use std::rc::Rc;

pub use lua_types::error::LuaError;
pub use lua_types::{CallInfoIdx, StackIdx};

/// Thin wrapper letting stack-indexing methods accept either `StackIdx` or a
/// raw integer via a single `impl Into<StackIdxConv>` bound.
pub struct StackIdxConv(pub StackIdx);

/// Explicit `StackIdx` → `i32` conversion for call sites that need a signed
/// index and would otherwise hit an ambiguous non-primitive-cast error.
#[inline(always)]
pub fn stack_idx_to_i32(i: StackIdx) -> i32 {
    i.0 as i32
}

impl From<u32> for StackIdxConv {
    #[inline(always)]
    fn from(v: u32) -> Self {
        StackIdxConv(StackIdx(v))
    }
}
impl From<i32> for StackIdxConv {
    #[inline(always)]
    fn from(v: i32) -> Self {
        StackIdxConv(StackIdx(v.max(0) as u32))
    }
}
impl From<usize> for StackIdxConv {
    #[inline(always)]
    fn from(v: usize) -> Self {
        StackIdxConv(StackIdx(v as u32))
    }
}
impl From<StackIdx> for StackIdxConv {
    #[inline(always)]
    fn from(v: StackIdx) -> Self {
        StackIdxConv(v)
    }
}
pub use lua_types::closure::{
    LuaCClosure as LuaClosureC, LuaCFnPtr, LuaClosure, LuaLClosure as LuaClosureLua,
};
pub use lua_types::gc::GcRef;
pub use lua_types::proto::LuaProto;
pub use lua_types::string::LuaString;
pub use lua_types::upval::UpVal;
pub use lua_types::userdata::LuaUserData;
pub use lua_types::value::{F2Imod, LuaTable, LuaValue};

pub struct LuaByteHasher {
    hash: u64,
}

impl Default for LuaByteHasher {
    fn default() -> Self {
        Self {
            hash: 0xcbf2_9ce4_8422_2325,
        }
    }
}

impl Hasher for LuaByteHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        const PRIME: u64 = 0x0000_0100_0000_01b3;
        for &byte in bytes {
            self.hash ^= u64::from(byte);
            self.hash = self.hash.wrapping_mul(PRIME);
        }
    }

    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.write(&[i]);
    }

    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.write(&i.to_ne_bytes());
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }
}

pub type LuaByteBuildHasher = BuildHasherDefault<LuaByteHasher>;

/// The short-string intern table, shaped like C-Lua's `stringtable`
/// (lstring.c): power-of-two hash buckets of `GcRef<LuaString>` chained by
/// Vec instead of `u.hnext`. Compared to the previous
/// `HashMap<Box<[u8]>, GcRef<LuaString>>`:
///
/// - lookup hashes the input bytes ONCE and never allocates (the entry-API
///   shape boxed the key on every call — rejected experiment
///   `intern-hitpath-borrowed-lookup` documents why a partial fix loses);
/// - insert reuses the same hash, no second probe;
/// - bytes are stored once (in the `LuaString`), not duplicated in a map key;
/// - dead strings are removed O(dead) by `(hash, identity)` pairs collected
///   during the GC mark phase, replacing the O(table·log live)
///   sort + binary-search retain that dominated churn-heavy profiles
///   (concat_chain 20260609T2201Z: intern machinery ~25% of wall).
pub struct InternedStringMap {
    buckets: Vec<Vec<GcRef<LuaString>>>,
    count: usize,
}

impl Default for InternedStringMap {
    fn default() -> Self {
        InternedStringMap {
            buckets: (0..64).map(|_| Vec::new()).collect(),
            count: 0,
        }
    }
}

impl InternedStringMap {
    #[inline]
    fn mask(&self) -> usize {
        self.buckets.len() - 1
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Number of hash buckets currently allocated (the power-of-two table
    /// size, C's `strt.size`). Exposed for the shrink-policy test, which
    /// asserts the array grows under a flood and shrinks back toward 64 once
    /// the interned strings are collected.
    pub fn bucket_count(&self) -> usize {
        self.buckets.len()
    }

    #[inline]
    pub fn find(&self, bytes: &[u8], hash: u32) -> Option<GcRef<LuaString>> {
        let bucket = &self.buckets[hash as usize & self.mask()];
        bucket
            .iter()
            .find(|s| s.hash() == hash && s.as_bytes() == bytes)
            .cloned()
    }

    /// C's `luaS_resize` growth rule: keep average chain length ~1.
    pub fn insert(&mut self, s: GcRef<LuaString>) {
        if self.count >= self.buckets.len() {
            self.resize(self.buckets.len() * 2);
        }
        let m = self.mask();
        self.buckets[s.hash() as usize & m].push(s);
        self.count += 1;
    }

    fn resize(&mut self, new_len: usize) {
        let old = std::mem::replace(
            &mut self.buckets,
            (0..new_len).map(|_| Vec::new()).collect(),
        );
        let m = self.mask();
        for bucket in old {
            for s in bucket {
                self.buckets[s.hash() as usize & m].push(s);
            }
        }
    }

    /// C's `luaS_resize` shrink path, driven by `lgc.c:checkSizes`
    /// (`if (g->strt.nuse < g->strt.size / 4) luaS_resize(L, size/2)`).
    /// Shrinks the bucket array when the live load factor falls below 25%
    /// (`count * 4 < buckets.len()`), down to `next_power_of_two(count)`
    /// floored at the initial 64 (C's `MINSTRTABSIZE`). The 4× gap is
    /// hysteresis: a table at load factor just under 1.0 will not thrash,
    /// since the next grow-then-shrink cycle needs the population to drop
    /// fourfold first. Rehashing only relocates the surviving
    /// `GcRef<LuaString>` entries by their cached `hash()`; it never derefs a
    /// string, so it is safe to call from the post-mark/sweep GC hook AFTER
    /// all dead entries have been removed (only live refs remain).
    pub fn shrink_if_sparse(&mut self) {
        if self.count * 4 >= self.buckets.len() {
            return;
        }
        let target = self.count.next_power_of_two().max(64);
        if target < self.buckets.len() {
            self.resize(target);
        }
    }

    /// O(dead): removes one entry located by its cached hash + GC identity.
    pub fn remove(&mut self, hash: u32, identity: usize) {
        let m = self.mask();
        let bucket = &mut self.buckets[hash as usize & m];
        if let Some(pos) = bucket.iter().position(|s| s.identity() == identity) {
            bucket.swap_remove(pos);
            self.count -= 1;
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &GcRef<LuaString>> {
        self.buckets.iter().flatten()
    }

    pub fn contains_key(&self, bytes: &[u8]) -> bool {
        self.find(bytes, LuaString::hash_bytes(bytes, 0)).is_some()
    }
}

/// A Lua-callable function pointer. C: `lua_CFunction`.
///
/// `lua_types::closure::LuaCFnPtr` is a `usize`-sized placeholder in that
/// crate (which cannot reference `LuaState` without a circular dependency);
/// this alias carries the real, lua-vm-facing signature.
pub type LuaCFunction = fn(&mut LuaState) -> Result<usize, LuaError>;

pub type LuaRustFunction = Rc<dyn Fn(&mut LuaState) -> Result<usize, LuaError>>;

#[derive(Clone)]
pub enum LuaCallable {
    Bare(LuaCFunction),
    Rust(LuaRustFunction),
}

impl std::fmt::Debug for LuaCallable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LuaCallable::Bare(_) => f.write_str("LuaCallable::Bare(..)"),
            LuaCallable::Rust(_) => f.write_str("LuaCallable::Rust(..)"),
        }
    }
}

impl LuaCallable {
    pub fn bare(f: LuaCFunction) -> Self {
        LuaCallable::Bare(f)
    }

    pub fn rust(f: LuaRustFunction) -> Self {
        LuaCallable::Rust(f)
    }

    pub fn as_bare(&self) -> Option<LuaCFunction> {
        match self {
            LuaCallable::Bare(f) => Some(*f),
            LuaCallable::Rust(_) => None,
        }
    }

    pub fn call(&self, state: &mut LuaState) -> Result<usize, LuaError> {
        match self {
            LuaCallable::Bare(f) => f(state),
            LuaCallable::Rust(f) => f(state),
        }
    }
}

#[derive(Clone, Debug)]
pub enum FinalizerObject {
    Table(GcRef<LuaTable>),
    UserData(GcRef<LuaUserData>),
}

impl FinalizerObject {
    pub fn identity(&self) -> usize {
        match self {
            FinalizerObject::Table(t) => t.identity(),
            FinalizerObject::UserData(u) => u.identity(),
        }
    }

    pub fn metatable(&self) -> Option<GcRef<LuaTable>> {
        match self {
            FinalizerObject::Table(t) => t.metatable(),
            FinalizerObject::UserData(u) => u.metatable(),
        }
    }

    pub fn as_lua_value(&self) -> LuaValue {
        match self {
            FinalizerObject::Table(t) => LuaValue::Table(t.clone()),
            FinalizerObject::UserData(u) => LuaValue::UserData(u.clone()),
        }
    }

    pub fn mark(&self, marker: &mut lua_gc::Marker) {
        match self {
            FinalizerObject::Table(t) => marker.mark(t.0),
            FinalizerObject::UserData(u) => marker.mark(u.0),
        }
    }

    pub fn heap_ptr(&self) -> Option<std::ptr::NonNull<lua_gc::GcBox<dyn lua_gc::Trace>>> {
        Some(match self {
            FinalizerObject::Table(t) => t.0.as_trace_ptr(),
            FinalizerObject::UserData(u) => u.0.as_trace_ptr(),
        })
    }

    pub fn age(&self) -> lua_gc::GcAge {
        match self {
            FinalizerObject::Table(t) => t.0.age(),
            FinalizerObject::UserData(u) => u.0.age(),
        }
    }

    pub fn is_finalized(&self) -> bool {
        match self {
            FinalizerObject::Table(t) => t.0.is_finalized(),
            FinalizerObject::UserData(u) => u.0.is_finalized(),
        }
    }

    pub fn set_finalized(&self, finalized: bool) {
        match self {
            FinalizerObject::Table(t) => t.0.set_finalized(finalized),
            FinalizerObject::UserData(u) => u.0.set_finalized(finalized),
        }
    }
}

impl lua_gc::FinalizerEntry for FinalizerObject {
    fn identity(&self) -> usize {
        FinalizerObject::identity(self)
    }

    fn heap_ptr(&self) -> Option<std::ptr::NonNull<lua_gc::GcBox<dyn lua_gc::Trace>>> {
        FinalizerObject::heap_ptr(self)
    }

    fn age(&self) -> lua_gc::GcAge {
        FinalizerObject::age(self)
    }

    fn is_finalized(&self) -> bool {
        FinalizerObject::is_finalized(self)
    }

    fn set_finalized(&self, finalized: bool) {
        FinalizerObject::set_finalized(self, finalized);
    }
}

#[derive(Clone, Debug)]
pub struct WeakTableEntry {
    table: lua_types::gc::GcWeak<LuaTable>,
    kind: lua_gc::WeakListKind,
}

impl WeakTableEntry {
    pub fn new(table: &GcRef<LuaTable>) -> Self {
        let mode = table.weak_mode();
        let weak_keys = (mode & (1 << 0)) != 0;
        let weak_values = (mode & (1 << 1)) != 0;
        let kind = match (weak_keys, weak_values) {
            (true, true) => lua_gc::WeakListKind::AllWeak,
            (true, false) => lua_gc::WeakListKind::Ephemeron,
            (false, true) => lua_gc::WeakListKind::WeakValues,
            (false, false) => lua_gc::WeakListKind::WeakValues,
        };
        Self {
            table: table.downgrade(),
            kind,
        }
    }
}

impl lua_gc::WeakEntry for WeakTableEntry {
    type Strong = GcRef<LuaTable>;

    fn identity(&self) -> usize {
        self.table.identity()
    }

    fn list_kind(&self) -> lua_gc::WeakListKind {
        self.kind
    }

    fn upgrade(&self) -> Option<Self::Strong> {
        self.table.upgrade()
    }
}

// ─── Constants ───────────────────────────────────────────────────────────────

pub(crate) const EXTRA_STACK: usize = 5;

pub(crate) const LUA_MINSTACK: usize = 20;

pub(crate) const BASIC_STACK_SIZE: usize = 2 * LUA_MINSTACK;

/// Maximum nested non-yielding C-call recursion depth — the single source of
/// truth for the call-depth guard (also used by `do_::ccall_inner` and
/// `do_::lua_resume`).
///
/// This is the structural defense that keeps a recursive interpreter sound for
/// untrusted code: a recursive Rust interpreter consumes host (Rust) stack per
/// nested Lua→Lua call, so unbounded Lua recursion would otherwise overflow the
/// OS thread stack and crash the process. Tripping this limit instead raises a
/// catchable `"stack overflow"` / `"C stack overflow"` Lua error.
///
/// Safe margin: each nested call frame consumes a bounded amount of Rust stack,
/// so `MAXCCALLS` frames fit within the default ~8 MiB thread stack with room to
/// spare — verified on macOS/Linux release builds against deep non-tail
/// recursion, infinite `__index`/`__concat`/`__tostring` metamethod chains, and
/// nested-coroutine `__close` cascades, all of which error cleanly rather than
/// SIGSEGV (see the `recursion_*` sandbox tests). Embedders that run the VM on a
/// smaller thread stack should lower this constant proportionally (roughly
/// `stack_bytes / 40_000`).

pub(crate) const LUAI_MAXCCALLS: u32 = 200;

pub(crate) const CIST_C: u16 = 1 << 1;

pub(crate) const CIST_OAH: u16 = 1 << 0;
pub(crate) const CIST_FRESH: u16 = 1 << 2;
pub(crate) const CIST_HOOKED: u16 = 1 << 3;
pub(crate) const CIST_YPCALL: u16 = 1 << 4;
pub(crate) const CIST_TAIL: u16 = 1 << 5;
pub(crate) const CIST_HOOKYIELD: u16 = 1 << 6;
pub(crate) const CIST_FIN: u16 = 1 << 7;
pub(crate) const CIST_TRAN: u16 = 1 << 8;
pub(crate) const CIST_RECST: u32 = 10;
/// Marks a CallInfo whose `__lt` call is standing in for a missing `__le`, so
/// that if the `__lt` metamethod yields, the comparison-resume path
/// (`vm::finish_op`) knows to negate the result — the synchronous derive in
/// `tagmethods::call_order_tm` cannot, since the negation happens after the
/// yield unwinds the stack. Bits 10-12 are CIST_RECST, so this is bit 13.
pub(crate) const CIST_LEQ: u16 = 1 << 13;

/// Per-frame hook trap flag, folded out of `CallInfoFrame` into a free
/// callstatus bit by T2-C2. C 5.4 stores trap in `ci->u.l.trap` (a Lua-only
/// union field); we instead store it in callstatus bit 14 so reading it costs
/// one mask with no enum-discriminant branch. Bit 14 is free (LEQ=13 is the
/// top used bit; RECST occupies 10-12). Only Lua frames are ever trapped
/// (`set_traps` guards on `is_lua()`), and the bit is cleared whenever a
/// CallInfo slot is repurposed for a new call (see `prep_call_info`).
pub(crate) const CIST_TRAP: u16 = 1 << 14;

const LUA_NUMTYPES: usize = 9;

const GCSTPUSR: u8 = 1;
const GCSTPGC: u8 = 2;

const GCS_PAUSE: u8 = 0;

const LUAI_GCPAUSE: u32 = 200;
const LUAI_GCMUL: u32 = 100;
const LUAI_GCSTEPSIZE: u8 = 13;
const LUAI_GENMAJORMUL: u32 = 100;
const LUAI_GENMINORMUL: u8 = 20;

const WHITE0BIT: u8 = 0;

const STRCACHE_N: usize = 53;
const STRCACHE_M: usize = 2;

/// `registry[LUA_RIDX_MAINTHREAD]`'s integer key on 5.2-5.4 (`lua.h`'s
/// `LUA_RIDX_MAINTHREAD`). 5.5 renumbers it to `3`; see
/// [`set_versioned_registry_slots`].
const LUA_RIDX_MAINTHREAD: i64 = 1;

/// `registry[LUA_RIDX_GLOBALS]`'s integer key on 5.2+ (`lua.h`'s
/// `LUA_RIDX_GLOBALS`). Absent on 5.1.
const LUA_RIDX_GLOBALS: i64 = 2;

// ─── GcKind enum ─────────────────────────────────────────────────────────────

/// Garbage collector operating mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcKind {
    Incremental = 0,
    Generational = 1,
}

/// State of the built-in warning handler, mirroring the `warnfoff` /
/// `warnfon` / `warnfcont` static functions in upstream `lauxlib.c`.
///
/// `Off` is the install-time default (warnings disabled until `warn("@on")`).
/// `On` is ready to begin a fresh message. `Cont` means the previous `warn`
/// call had `tocont` set, so the next message part continues the current line
/// without re-printing the `Lua warning: ` prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarnMode {
    Off,
    On,
    Cont,
}

/// Output mode for the testC/ltests warning sink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestWarnMode {
    Normal,
    Allow,
    Store,
}

// ─── LuaStatus enum ──────────────────────────────────────────────────────────

/// Thread / call status codes.
///
pub use lua_types::status::LuaStatus;

// ─── StackValue ───────────────────────────────────────────────────────────────

/// One slot on the Lua value stack.
///
/// C's `StackValue` overlays a to-be-closed delta (`tbclist.delta: u16`) in a
/// union with the value, because C threads its tbclist through the stack
/// itself. The Rust port keeps that list as a side structure
/// (`LuaState.tbclist: Vec<StackIdx>`), so the slot is just the value and
/// stays 16 bytes — matching C's slot size without the union. A vestigial
/// `tbc_delta: u16` field (written, never read) padded every slot to 24 bytes
/// until it was removed on 2026-06-09 (PERF_PUSH_SPEC.md P3).
#[derive(Clone)]
pub struct StackValue {
    pub val: LuaValue,
}

impl Default for StackValue {
    fn default() -> Self {
        StackValue {
            val: LuaValue::Nil,
        }
    }
}

// ─── CallInfo ────────────────────────────────────────────────────────────────

/// Saved state for a Lua or C call frame.
///
/// The C intrusive doubly-linked list (`previous`, `next` as raw pointers) is
/// replaced by `Option<CallInfoIdx>` indices into `LuaState::call_info`.
#[derive(Clone)]
pub struct CallInfo {
    pub func: StackIdx,

    pub top: StackIdx,

    pub previous: Option<CallInfoIdx>,

    pub next: Option<CallInfoIdx>,

    pub u: CallInfoFrame,

    pub u2: CallInfoExtra,

    pub nresults: i16,

    /// Bit-packed `CIST_*` flags.
    pub callstatus: u16,

    /// Lua 5.5: number of `__call` metamethods traversed before entering this
    /// frame. Upstream stores this in the repacked 5.5 `callstatus` bits; keep
    /// it separate here so older transfer/recover-status bits stay unchanged.
    pub call_metamethods: u8,

    /// Lua 5.1: count of tail calls "lost" into this reused frame. Mirrors C
    /// 5.1's `ci->tailcalls`. Each `OP_TAILCALL` reuses the current frame and
    /// increments this; a normal call gets a fresh slot (via `next_ci`) where
    /// it is reset to 0. Used only by the 5.1 `debug.getstack` level walk to
    /// synthesize the `(tail call)` frames the 5.1 debug model exposes; 5.2+
    /// dropped that model in favour of the `istailcall` flag and never read it.
    /// Lives in the 3 spare bytes after `call_metamethods`, so `CallInfo` stays
    /// 72 B (the size guard above still holds).
    pub tailcalls: u16,
}

/// Compile-time guard that the T2-C2 `CallInfoFrame` flatten kept the per-frame
/// footprint unchanged: the payload stays 32 B and `CallInfo` stays 72 B. The
/// approved design (Option A) is conditioned on not growing `CallInfo`; if a
/// future field change breaks this, the build fails here rather than silently
/// regressing per-frame RSS. The byte counts are 64-bit-specific (the payload
/// holds a fn pointer and two `isize`s), so the guard is gated off 32-bit
/// targets — on wasm32 the struct is naturally smaller and there is no C
/// layout-parity claim to enforce.
#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(core::mem::size_of::<CallInfoFrame>() == 32);
    assert!(core::mem::size_of::<CallInfo>() == 72);
};

/// Payload of `CallInfo.u`.
///
/// C 5.4 declares this as an anonymous union with a Lua-frame member (`l`)
/// and a C-frame member (`c`); which member is live is implied by the
/// `CIST_C` callstatus bit. T2-C2 flattened the previous 2-variant Rust enum
/// into this branch-free struct: all fields are always present, so the hot
/// accessors (`saved_pc`, `set_saved_pc`, `nextra_args`) are plain field
/// reads with no discriminant test. The C-frame fields (`k`, `old_errfunc`,
/// `ctx`) are only meaningful on C frames; the Lua-frame fields (`savedpc`,
/// `nextraargs`) only on Lua frames. Accessors carry `debug_assert!`s on the
/// frame kind that replace the old enum's wrong-variant panic tripwire at
/// zero release cost. The `trap` flag that used to live in the Lua variant
/// now lives in callstatus bit `CIST_TRAP`.
///
/// Layout: `u32 + i32 + Option<fn> + isize + isize` = 32 B (8-byte aligned),
/// identical to the previous enum payload, so `CallInfo` stays 72 B.
#[derive(Clone, Copy)]
pub struct CallInfoFrame {
    /// Lua-frame: index of the next bytecode instruction. C: `u.l.savedpc`.
    pub savedpc: u32,
    /// Lua-frame: count of extra varargs collected at entry. C: `u.l.nextraargs`.
    pub nextraargs: i32,
    /// C-frame: continuation function for a yieldable C call. C: `u.c.k`.
    pub k: Option<LuaKFunction>,
    /// C-frame: saved `errfunc` to restore on pcall recovery. C: `u.c.old_errfunc`.
    pub old_errfunc: isize,
    /// C-frame: continuation context. C: `u.c.ctx`.
    pub ctx: isize,
}

/// Continuation function for yieldable C calls.  C: `lua_KFunction`.
pub type LuaKFunction = fn(&mut LuaState, status: i32, ctx: isize) -> Result<usize, LuaError>;

/// Payload of `CallInfo.u2`.
#[derive(Default, Clone, Copy)]
pub struct CallInfoExtra {
    pub value: i32,
    pub ftransfer: u16,
    pub ntransfer: u16,
}

impl CallInfoFrame {
    /// Default C-call frame: no continuation, zero context. The Lua-frame
    /// fields are benignly zeroed so any latent read on a misclassified frame
    /// reads the same value the old C variant produced.
    pub fn c_default() -> Self {
        CallInfoFrame {
            savedpc: 0,
            nextraargs: 0,
            k: None,
            old_errfunc: 0,
            ctx: 0,
        }
    }

    /// Default Lua-call frame: pc=0, no extra args. The C-frame fields are
    /// benignly zeroed to exactly the values `c_default` produced, so a Lua
    /// frame that is later reclassified or read as a C frame sees identical
    /// defaults to the pre-flatten enum (which carried no C fields). The
    /// `trap` flag now lives in callstatus bit `CIST_TRAP`, cleared by the
    /// `callstatus` write in `prep_call_info`, not here.
    pub fn lua_default() -> Self {
        CallInfoFrame {
            savedpc: 0,
            nextraargs: 0,
            k: None,
            old_errfunc: 0,
            ctx: 0,
        }
    }
}

impl Default for CallInfo {
    fn default() -> Self {
        CallInfo {
            func: StackIdx(0),
            top: StackIdx(0),
            previous: None,
            next: None,
            u: CallInfoFrame::c_default(),
            u2: CallInfoExtra::default(),
            nresults: 0,
            callstatus: 0,
            call_metamethods: 0,
            tailcalls: 0,
        }
    }
}

impl CallInfo {
    pub fn is_lua(&self) -> bool {
        (self.callstatus & CIST_C) == 0
    }
    pub fn is_lua_code(&self) -> bool {
        self.is_lua()
    }
    /// Whether the active function is a vararg function.
    ///
    /// Currently returns `false` unconditionally — vararg introspection via
    /// `debug.getinfo` reports no vararg info instead of panicking.
    pub fn is_vararg_func(&self) -> bool {
        false
    }
    /// Index of the next bytecode instruction on this Lua frame.
    ///
    /// `debug_assert!` guards that the frame is a Lua frame, restoring the
    /// wrong-variant tripwire the pre-flatten enum gave for free.
    pub fn saved_pc(&self) -> u32 {
        debug_assert!(self.is_lua(), "saved_pc on a C frame");
        self.u.savedpc
    }
    /// Write the next-instruction index on this Lua frame.
    pub fn set_saved_pc(&mut self, pc: u32) {
        debug_assert!(self.is_lua(), "set_saved_pc on a C frame");
        self.u.savedpc = pc;
    }
    /// Count of extra varargs collected at entry on this Lua frame.
    pub fn nextra_args(&self) -> i32 {
        debug_assert!(self.is_lua(), "nextra_args on a C frame");
        self.u.nextraargs
    }
    /// Write the extra-vararg count on this Lua frame.
    pub fn set_nextra_args(&mut self, n: i32) {
        debug_assert!(self.is_lua(), "set_nextra_args on a C frame");
        self.u.nextraargs = n;
    }
    pub fn transfer_ftransfer(&self) -> u16 {
        self.u2.ftransfer
    }
    pub fn transfer_ntransfer(&self) -> u16 {
        self.u2.ntransfer
    }
    /// Set or clear the per-frame hook trap, stored in callstatus bit
    /// `CIST_TRAP` (T2-C2 moved it out of the `CallInfoFrame` payload). Only
    /// Lua frames are ever trapped: `set_traps` guards on `is_lua()` before
    /// calling here, and `trace_call`/`trace_exec` operate on the current Lua
    /// frame. The `debug_assert!` preserves that invariant. Reusing a slot for
    /// a new call clears the bit via the `callstatus = mask` write in
    /// `prep_call_info`, exactly as the old full-variant store reset `trap`.
    pub fn set_trap(&mut self, t: bool) {
        debug_assert!(self.is_lua(), "set_trap on a C frame");
        if t {
            self.callstatus |= CIST_TRAP;
        } else {
            self.callstatus &= !CIST_TRAP;
        }
    }
    /// Read the per-frame hook trap from callstatus bit `CIST_TRAP`. One mask,
    /// no enum-discriminant branch — this is the hot read in the OP_CALL /
    /// OP_RETURN `updatetrap` paths.
    #[inline(always)]
    pub fn trap(&self) -> bool {
        (self.callstatus & CIST_TRAP) != 0
    }
    /// Read the 3-bit recover-status field packed into bits 10-12 of callstatus.
    ///
    pub fn recover_status(&self) -> i32 {
        ((self.callstatus >> CIST_RECST) & 7) as i32
    }
    /// Write the 3-bit recover-status field. `status` must fit in three bits.
    ///
    pub fn set_recover_status<T: Into<i32>>(&mut self, status: T) {
        let st = (status.into() & 7) as u16;
        self.callstatus = (self.callstatus & !(7u16 << CIST_RECST)) | (st << CIST_RECST);
    }
    pub fn get_oah(&self) -> bool {
        (self.callstatus & CIST_OAH) != 0
    }
    /// Store the current `allowhook` value into callstatus bit 0 (CIST_OAH).
    ///
    pub fn set_oah(&mut self, allow: bool) {
        self.callstatus = (self.callstatus & !CIST_OAH) | (if allow { CIST_OAH } else { 0 });
    }
    /// Saved `errfunc` to restore on pcall recovery (C-call frame).
    ///
    /// `debug_assert!` guards that the frame is a C frame, restoring the
    /// wrong-variant tripwire the pre-flatten enum gave for free.
    pub fn u_c_old_errfunc(&self) -> isize {
        debug_assert!(!self.is_lua(), "u_c_old_errfunc on a Lua frame");
        self.u.old_errfunc
    }
    /// Continuation context on a C-call frame.
    pub fn u_c_ctx(&self) -> isize {
        debug_assert!(!self.is_lua(), "u_c_ctx on a Lua frame");
        self.u.ctx
    }
    /// Continuation function on a C-call frame.
    pub fn u_c_k(&self) -> Option<LuaKFunction> {
        debug_assert!(!self.is_lua(), "u_c_k on a Lua frame");
        self.u.k
    }
    /// Set continuation function on a C-call frame.
    ///
    /// `debug_assert!` guards that the frame is a C frame (callers reach here
    /// from `lua_callk`/`lua_pcallk`, where `L->ci` is the calling C builtin's
    /// frame).
    pub fn set_u_c_k(&mut self, k: Option<LuaKFunction>) {
        debug_assert!(!self.is_lua(), "set_u_c_k on a Lua frame");
        self.u.k = k;
    }
    /// Set continuation context on a C-call frame.
    pub fn set_u_c_ctx(&mut self, ctx: isize) {
        debug_assert!(!self.is_lua(), "set_u_c_ctx on a Lua frame");
        self.u.ctx = ctx;
    }
    /// Set saved old_errfunc on a C-call frame.
    pub fn set_u_c_old_errfunc(&mut self, old_errfunc: isize) {
        debug_assert!(!self.is_lua(), "set_u_c_old_errfunc on a Lua frame");
        self.u.old_errfunc = old_errfunc;
    }
    /// Set the `u2.funcidx` field, used by yieldable pcall for error recovery.
    ///
    pub fn set_u2_funcidx(&mut self, idx: i32) {
        self.u2.value = idx;
    }
}

// ─── Value/proto/instruction helpers ───────────────────────────────────────────

/// Extension methods on `LuaValue`, kept in lua-vm alongside `LuaState`
/// rather than in `lua_types::value`.
pub trait LuaValueExt {
    fn base_type(&self) -> lua_types::LuaType;
    fn to_number_no_strconv(&self) -> Option<f64>;
    fn to_number_with_strconv(&self) -> Option<f64>;
    fn to_integer_no_strconv(&self) -> Option<i64>;
    fn to_integer_with_strconv(&self) -> Option<i64>;
    fn full_type_tag(&self) -> u8;
}

impl LuaValueExt for LuaValue {
    fn base_type(&self) -> lua_types::LuaType {
        self.type_tag()
    }
    fn to_number_no_strconv(&self) -> Option<f64> {
        match self {
            LuaValue::Float(f) => Some(*f),
            LuaValue::Int(i) => Some(*i as f64),
            _ => None,
        }
    }
    fn to_number_with_strconv(&self) -> Option<f64> {
        if let Some(n) = self.to_number_no_strconv() {
            return Some(n);
        }
        if let LuaValue::Str(s) = self {
            let mut tmp = LuaValue::Nil;
            let sz = crate::object::str2num(s.as_bytes(), &mut tmp);
            if sz == 0 {
                return None;
            }
            return match tmp {
                LuaValue::Int(i) => Some(i as f64),
                LuaValue::Float(f) => Some(f),
                _ => None,
            };
        }
        None
    }
    fn to_integer_no_strconv(&self) -> Option<i64> {
        match self {
            LuaValue::Int(i) => Some(*i),
            LuaValue::Float(f) if f.fract() == 0.0 && f.is_finite() => {
                //   d >= LUA_MININTEGER && d < -(lua_Number)LUA_MININTEGER.
                // Without this, Rust's `as i64` saturates and silently
                // produces i64::MAX / i64::MIN for out-of-range floats.
                let min_f = i64::MIN as f64;
                let max_plus1_f = -(i64::MIN as f64);
                if *f >= min_f && *f < max_plus1_f {
                    Some(*f as i64)
                } else {
                    None
                }
            }
            _ => None,
        }
    }
    fn to_integer_with_strconv(&self) -> Option<i64> {
        if let Some(i) = self.to_integer_no_strconv() {
            return Some(i);
        }
        if let LuaValue::Str(s) = self {
            let mut tmp = LuaValue::Nil;
            let sz = crate::object::str2num(s.as_bytes(), &mut tmp);
            if sz == 0 {
                return None;
            }
            return tmp.to_integer_no_strconv();
        }
        None
    }
    fn full_type_tag(&self) -> u8 {
        match self {
            LuaValue::Nil => 0x00,
            LuaValue::Bool(false) => 0x01,
            LuaValue::Bool(true) => 0x11,
            LuaValue::Int(_) => 0x03,
            LuaValue::Float(_) => 0x13,
            LuaValue::Str(s) if s.is_short() => 0x04,
            LuaValue::Str(_) => 0x14,
            LuaValue::LightUserData(_) => 0x02,
            LuaValue::Table(_) => 0x05,
            LuaValue::Function(LuaClosure::Lua(_)) => 0x06,
            LuaValue::Function(LuaClosure::LightC(_)) => 0x16,
            LuaValue::Function(LuaClosure::C(_)) => 0x26,
            LuaValue::UserData(_) => 0x07,
            LuaValue::Thread(_) => 0x08,
        }
    }
}

/// Extension methods on `lua_types::LuaType`.
pub trait LuaTypeExt {
    fn type_name(&self) -> &'static [u8];
}

impl LuaTypeExt for lua_types::LuaType {
    fn type_name(&self) -> &'static [u8] {
        use lua_types::LuaType::*;
        match self {
            None => b"no value",
            Nil => b"nil",
            Boolean => b"boolean",
            LightUserData => b"userdata",
            Number => b"number",
            String => b"string",
            Table => b"table",
            Function => b"function",
            UserData => b"userdata",
            Thread => b"thread",
        }
    }
}

/// StackIdx checked-arithmetic helpers. Returns the raw `u32` because
/// callers use the result in arithmetic comparisons against other `u32`
/// quantities (stack-distance offsets).
pub trait StackIdxExt {
    fn saturating_sub(self, n: impl Into<StackIdxConv>) -> u32;
    fn wrapping_sub(self, n: impl Into<StackIdxConv>) -> u32;
    fn raw(self) -> u32;
}
impl StackIdxExt for StackIdx {
    #[inline(always)]
    fn saturating_sub(self, n: impl Into<StackIdxConv>) -> u32 {
        self.0.saturating_sub(n.into().0 .0)
    }
    #[inline(always)]
    fn wrapping_sub(self, n: impl Into<StackIdxConv>) -> u32 {
        self.0.wrapping_sub(n.into().0 .0)
    }
    #[inline(always)]
    fn raw(self) -> u32 {
        self.0
    }
}

/// `GcRef<LuaTable>` / `GcRef<LuaUserData>` field-access helpers, forwarding
/// to the canonical `LuaTable`/`LuaUserData` methods through the deref, for
/// use by api.rs and tagmethods.rs.
///
/// Nil/NaN key validation lives inside [`LuaTable::try_raw_set`] (alongside
/// the integer-fast-path match), so this wrapper does not double-check.
pub trait LuaTableRefExt {
    fn metatable(&self) -> Option<GcRef<LuaTable>>;
    fn has_metatable(&self) -> bool;
    fn as_ptr(&self) -> *const ();
    fn get(&self, _k: &LuaValue) -> LuaValue;
    fn get_int(&self, _k: i64) -> LuaValue;
    fn get_short_str(&self, _k: &GcRef<LuaString>) -> LuaValue;
    fn raw_set(&self, _state: &mut LuaState, _k: LuaValue, _v: LuaValue) -> Result<(), LuaError>;
    fn raw_set_int(&self, _state: &mut LuaState, _k: i64, _v: LuaValue) -> Result<(), LuaError>;
    fn raw_set_short_str(
        &self,
        _state: &mut LuaState,
        _k: GcRef<LuaString>,
        _v: LuaValue,
    ) -> Result<(), LuaError>;
    fn invalidate_tm_cache(&self);
    fn resize(&self, _state: &mut LuaState, _na: usize, _nh: usize) -> Result<(), LuaError>;
    fn next(&self, _k: LuaValue) -> Result<Option<(LuaValue, LuaValue)>, LuaError>;
}
impl LuaTableRefExt for GcRef<LuaTable> {
    #[inline]
    fn metatable(&self) -> Option<GcRef<LuaTable>> {
        (**self).metatable()
    }
    #[inline]
    fn has_metatable(&self) -> bool {
        (**self).has_metatable()
    }
    #[inline]
    fn as_ptr(&self) -> *const () {
        GcRef::identity(self) as *const ()
    }
    #[inline]
    fn get(&self, k: &LuaValue) -> LuaValue {
        (**self).get(k)
    }
    #[inline]
    fn get_int(&self, k: i64) -> LuaValue {
        (**self).get_int(k)
    }
    #[inline]
    fn get_short_str(&self, k: &GcRef<LuaString>) -> LuaValue {
        (**self).get_short_str(k)
    }
    /// Forwards to [`LuaTable::try_raw_set`], which performs the nil/NaN
    /// key validation internally as part of its integer-fast-path match.
    ///
    /// A collectable KEY stored into the table needs its own backward
    /// barrier, mirroring C-Lua's `luaC_barrierback(L, obj2gco(t), key)`
    /// inside `luaH_newkey` (ltable.c:717). Without it a young key inserted
    /// into an old table is invisible to the generational collector and is
    /// freed while still referenced — the 2026-06-09 table livelock.
    #[inline(always)]
    fn raw_set(&self, state: &mut LuaState, k: LuaValue, v: LuaValue) -> Result<(), LuaError> {
        match k {
            LuaValue::Int(i) => return self.raw_set_int(state, i, v),
            LuaValue::Str(s) if s.is_short() => return self.raw_set_short_str(state, s, v),
            k => {
                if k.is_collectable() {
                    state.gc_table_barrier_back(self, &k);
                }
                let before = (**self).buffer_bytes();
                let result = (**self).try_raw_set(k, v);
                if result.is_ok() {
                    account_table_buffer_delta(self, before);
                }
                result
            }
        }
    }
    #[inline(always)]
    fn raw_set_int(&self, _state: &mut LuaState, k: i64, v: LuaValue) -> Result<(), LuaError> {
        match (**self).try_update_int(k, v) {
            Ok(()) => Ok(()),
            Err(v) => {
                let before = (**self).buffer_bytes();
                let result = (**self).try_raw_set_int(k, v);
                if result.is_ok() {
                    account_table_buffer_delta(self, before);
                }
                result
            }
        }
    }
    /// The miss arm inserts a NEW short-string key, so the key itself is
    /// barriered (C-Lua does this inside `luaH_newkey`, ltable.c:717); the
    /// hit arm only overwrites a value whose key is already traced.
    #[inline(always)]
    fn raw_set_short_str(
        &self,
        state: &mut LuaState,
        k: GcRef<LuaString>,
        v: LuaValue,
    ) -> Result<(), LuaError> {
        match (**self).try_update_short_str(&k, v) {
            Ok(()) => Ok(()),
            Err(v) => {
                state.gc_table_barrier_back(self, &LuaValue::Str(k));
                let before = (**self).buffer_bytes();
                let result = (**self).try_raw_set(LuaValue::Str(k), v);
                if result.is_ok() {
                    account_table_buffer_delta(self, before);
                }
                result
            }
        }
    }
    fn invalidate_tm_cache(&self) {}
    fn resize(&self, _state: &mut LuaState, na: usize, nh: usize) -> Result<(), LuaError> {
        let before = (**self).buffer_bytes();
        let na32 = na.min(u32::MAX as usize) as u32;
        let nh32 = nh.min(u32::MAX as usize) as u32;
        let result = (**self).resize(na32, nh32);
        if result.is_ok() {
            account_table_buffer_delta(self, before);
        }
        result
    }
    fn next(&self, k: LuaValue) -> Result<Option<(LuaValue, LuaValue)>, LuaError> {
        (**self).try_next_pair(&k)
    }
}

#[inline]
fn account_table_buffer_delta(t: &GcRef<LuaTable>, before: usize) {
    let after = (**t).buffer_bytes();
    if after > before {
        t.account_buffer((after - before) as isize);
    } else if before > after {
        t.account_buffer(-((before - after) as isize));
    }
}

pub trait LuaUserDataRefExt {
    fn metatable(&self) -> Option<GcRef<LuaTable>>;
    fn set_metatable(&self, mt: Option<GcRef<LuaTable>>);
    fn as_ptr(&self) -> *const ();
    fn len(&self) -> usize;
}
impl LuaUserDataRefExt for GcRef<LuaUserData> {
    fn metatable(&self) -> Option<GcRef<LuaTable>> {
        (**self).metatable()
    }
    fn set_metatable(&self, mt: Option<GcRef<LuaTable>>) {
        (**self).set_metatable(mt);
    }
    fn as_ptr(&self) -> *const () {
        GcRef::identity(self) as *const ()
    }
    fn len(&self) -> usize {
        self.0.data.len()
    }
}

pub trait LuaStringRefExt {
    fn is_white(&self) -> bool;
    fn hash(&self) -> u32;
    fn as_gc_ref(&self) -> GcRef<LuaString>;
}
impl LuaStringRefExt for GcRef<LuaString> {
    fn is_white(&self) -> bool {
        false
    }
    fn hash(&self) -> u32 {
        self.0.hash()
    }
    fn as_gc_ref(&self) -> GcRef<LuaString> {
        self.clone()
    }
}

pub trait LuaLClosureRefExt {
    fn proto(&self) -> &GcRef<LuaProto>;
    fn nupvalues(&self) -> usize;
}
impl LuaLClosureRefExt for GcRef<lua_types::closure::LuaLClosure> {
    fn proto(&self) -> &GcRef<LuaProto> {
        &self.0.proto
    }
    fn nupvalues(&self) -> usize {
        self.0.upvals.len()
    }
}

/// `LuaClosure` accessor — `nupvalues()` reports the upvalue count uniformly.
pub trait LuaClosureExt {
    fn nupvalues(&self) -> usize;
}
impl LuaClosureExt for LuaClosure {
    fn nupvalues(&self) -> usize {
        match self {
            LuaClosure::Lua(l) => l.0.upvals.len(),
            LuaClosure::C(c) => c.0.upvalues.borrow().len(),
            LuaClosure::LightC(_) => 0,
        }
    }
}

/// `LuaProto` source bytes accessor.
pub trait LuaProtoExt {
    fn source_bytes(&self) -> &[u8];
    fn source_string(&self) -> Option<&GcRef<LuaString>>;
}
impl LuaProtoExt for LuaProto {
    fn source_bytes(&self) -> &[u8] {
        match &self.source {
            Some(s) => s.0.as_bytes(),
            None => &[],
        }
    }
    fn source_string(&self) -> Option<&GcRef<LuaString>> {
        self.source.as_ref()
    }
}

// ─── Collectable trait (GC interface) ────────────────────────────────────────

/// Marker trait for GC-managed objects.
pub trait Collectable: std::fmt::Debug {}

impl std::fmt::Debug for LuaState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LuaState")
    }
}
impl Collectable for LuaState {}

// ─── GlobalState ─────────────────────────────────────────────────────────────

/// Function-pointer signature for the text-source parser, installed on
/// [`GlobalState::parser_hook`] by the embedder.
///
/// The implementation lives in `lua-parse`; `lua-vm` cannot depend on it
/// directly (that would form a cycle), so the parser is reached via this
/// function pointer registered at startup.
///
/// The parser receives the live [`ZIO`](crate::zio::ZIO) (already positioned
/// past `firstchar`) rather than a fully-materialised buffer, so it can pull
/// reader chunks on demand and stop at the first syntax error — matching C's
/// `lua_load`, where the lexer drives the `lua_Reader` byte by byte.
pub type ParserHook = fn(
    state: &mut LuaState,
    z: &mut crate::zio::ZIO,
    name: &[u8],
    firstchar: i32,
) -> Result<GcRef<lua_types::closure::LuaLClosure>, LuaError>;

/// Function-pointer signature for reading a file's full contents into memory,
/// installed on [`GlobalState::file_loader_hook`] by the embedder.
///
/// `std::fs` is banned outside `lua-cli`, so `lua-stdlib`'s `loadfile` and
/// `searcher_lua` reach the filesystem via this hook. `None` keeps the file
/// system unreachable, which is appropriate for embeddings where modules are
/// served exclusively from `package.preload`.
pub type FileLoaderHook = fn(filename: &[u8]) -> Result<Vec<u8>, LuaError>;

/// Function-pointer signature for opening a file handle, installed on
/// [`GlobalState::file_open_hook`] by the embedder.
///
/// `std::fs` is banned outside `lua-cli`, so `lua-stdlib`'s io library reaches
/// the filesystem via this hook. `None` causes `io.open` and `io.output(name)`
/// to return a "file system not available" error, which is appropriate for
/// sandboxed embeddings.
///
/// `mode` is a Lua fopen-style mode string (e.g. `b"r"`, `b"w"`, `b"a"`,
/// `b"r+"`, etc.). The hook must honour at least `r`, `w`, and `a`.
///
/// The error type is `std::io::Error`, not `LuaError`: `io::Error` is the only
/// type in this boundary that can carry `raw_os_error()` end to end.
/// `lua-stdlib`'s `io.open`/`io.input`/`io.output` read the errno straight off
/// this error and render the message with `strerror`-shaped text, matching
/// C-Lua's `luaL_fileresult`/`luaL_error(..., strerror(errno))`. Converting to
/// `LuaError` here (as earlier revisions did) discards `raw_os_error()`, which
/// was issue #301 (errno 0 + a verbose Rust `Display` message reaching Lua).
pub type FileOpenHook =
    fn(filename: &[u8], mode: &[u8]) -> std::io::Result<Box<dyn lua_types::LuaFileHandle>>;

/// Function-pointer signature for writing bytes to a host-provided output
/// stream, installed on [`GlobalState::stdout_hook`] or
/// [`GlobalState::stderr_hook`] by the embedder.
///
/// Bare `wasm32-unknown-unknown` has no ambient stdout/stderr. Keeping output
/// behind explicit hooks lets sandboxed and WASM hosts decide whether output is
/// unavailable, buffered, or bridged to something like a browser console.
pub type OutputHook = fn(bytes: &[u8]) -> std::io::Result<()>;

/// Function-pointer signature for reading bytes from a host-provided input
/// stream, installed on [`GlobalState::stdin_hook`] by the embedder.
pub type InputHook = fn(buf: &mut [u8]) -> std::io::Result<usize>;

/// Function-pointer signature for reading a host environment variable.
///
/// Returning `None` maps naturally to Lua's `os.getenv` result for a missing
/// variable and is also the sandbox/bare-WASM default when no environment is
/// exposed.
pub type EnvHook = fn(name: &[u8]) -> Option<Vec<u8>>;

/// Function-pointer signature for retrieving the current Unix time in seconds.
pub type UnixTimeHook = fn() -> i64;

/// Function-pointer signature for retrieving program CPU time in seconds.
///
/// Backs `os.clock`. C's `clock()` reads `CLOCK_PROCESS_CPUTIME_ID`, which has no
/// `std` equivalent and is unavailable on bare WASM; the stdlib falls back to a
/// monotonic wall-clock baseline (matching wasi-libc/Emscripten's emulation) when
/// this hook is unset. A host wanting true CPU time can install one (e.g. via the
/// `cpu-time` crate) without changing the sandboxed crates.
pub type CpuClockHook = fn() -> f64;

/// Function-pointer signature for the host's local timezone offset.
///
/// Given a Unix timestamp (seconds, UTC), returns the offset in seconds that the
/// host's local timezone applies at that instant, such that
/// `local_broken_down = gmtime(timestamp + offset)`. Positive east of UTC (e.g.
/// `+3600` for CET), negative west (e.g. `-14400` for US EDT). This backs the
/// local-time semantics of `os.date` (non-`!` formats) and `os.time`, which C
/// implements with `localtime_r`/`mktime`. Reading the host timezone database
/// requires `libc` FFI (`unsafe`), banned in `lua-stdlib`, so the host installs
/// this hook. When unset the stdlib uses UTC (offset 0), keeping the
/// `os.date`/`os.time` round-trip exact on hosts without a timezone.
pub type LocalOffsetHook = fn(timestamp: i64) -> i64;

/// Function-pointer signature for host entropy used by default PRNG seeds and
/// table-sort pivot randomisation. Hosts without entropy may leave it unset; the
/// stdlib then uses deterministic fallback values instead of touching OS stubs.
pub type EntropyHook = fn() -> u64;

/// Function-pointer signature for generating a host temporary filename.
///
/// Used by `os.tmpname` and `io.tmpfile`. The hook should return a path-like byte
/// string that the host's `file_open_hook` can understand.
pub type TempNameHook = fn() -> Result<Vec<u8>, LuaError>;

/// Function-pointer signature for spawning a child process with a connected
/// pipe, installed on [`GlobalState::popen_hook`] by the embedder.
///
/// `std::process::Command` is banned outside `lua-cli`, so `lua-stdlib`'s
/// `io.popen` reaches the OS through this hook. `None` causes `io.popen` to
/// raise a clean Lua error ("popen not enabled in this build"), which is
/// appropriate for sandboxed embeddings.
///
/// `mode` is the Lua popen mode string — `b"r"` for reading the child's
/// stdout, `b"w"` for writing to the child's stdin.
pub type PopenHook =
    fn(cmd: &[u8], mode: &[u8]) -> Result<Box<dyn lua_types::LuaFileHandle>, LuaError>;

/// Function-pointer signature for removing a file, installed on
/// [`GlobalState::file_remove_hook`] by the embedder.
///
/// `std::fs` is banned outside `lua-cli`, so `lua-stdlib`'s `os.remove`
/// reaches the filesystem via this hook. Returns `Ok(())` on success.
///
/// The error type is `std::io::Error` so `os.remove`'s failure triple
/// (`nil, msg, errno`) can report the real `raw_os_error()` — see
/// [`FileOpenHook`]'s doc for why `LuaError` cannot carry it (#301).
pub type FileRemoveHook = fn(filename: &[u8]) -> std::io::Result<()>;

/// Function-pointer signature for renaming a file, installed on
/// [`GlobalState::file_rename_hook`] by the embedder.
///
/// `std::fs` is banned outside `lua-cli`, so `lua-stdlib`'s `os.rename`
/// reaches the filesystem via this hook. Returns `Ok(())` on success.
///
/// The error type is `std::io::Error`, for the same reason as
/// [`FileRemoveHook`] (#301).
pub type FileRenameHook = fn(from: &[u8], to: &[u8]) -> std::io::Result<()>;

/// Reason a shell command terminated, returned by [`OsExecuteHook`].
///
/// Mirrors the two string literals that C-Lua's `l_inspectstat` / `luaL_execresult`
/// can produce: `"exit"` for normal process exit, `"signal"` for signal termination
/// (POSIX only).
#[derive(Clone, Copy, Debug)]
pub enum OsExecuteReason {
    /// Process exited with an exit code (`WIFEXITED` / `ExitStatus::code()` is `Some`).
    Exit,
    /// Process was terminated by a signal (`WIFSIGNALED` / `ExitStatus::signal()` is `Some`).
    Signal,
}

/// Result returned by [`OsExecuteHook`], carrying the three values that
/// C-Lua's `luaL_execresult` pushes: `(boolean|nil, "exit"|"signal", int)`.
#[derive(Debug)]
pub struct OsExecuteResult {
    /// `true` when the command exited successfully (exit code 0).
    pub success: bool,
    /// How the process terminated.
    pub reason: OsExecuteReason,
    /// Exit code (for `Exit`) or signal number (for `Signal`).
    pub code: i32,
}

/// Function-pointer signature for executing a shell command, installed on
/// [`GlobalState::os_execute_hook`] by the embedder.
///
/// `std::process` is banned outside `lua-cli`, so `lua-stdlib`'s `os.execute`
/// reaches the shell via this hook. Returns an [`OsExecuteResult`] on success,
/// or a [`LuaError`] when the spawn itself fails.
pub type OsExecuteHook = fn(cmd: &[u8]) -> Result<OsExecuteResult, LuaError>;

/// Opaque handle to a dynamically loaded library, allocated by a
/// [`DynLibLoadHook`] backend and stored in `package._CLIBS`.
///
/// The handle is a backend-owned `u64`; the embedder is free to use it as an
/// index into a `Vec<libloading::Library>` or a `HashMap` key. `lua-stdlib`
/// stores the value verbatim and never inspects it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DynLibId(pub u64);

/// Resolved dynamic-library symbol.
///
/// Only `RustNative` is callable by this build of the VM. `LuaCAbi` resolves
/// to a real C function pointer compiled against stock Lua 5.4's `lua_State *`
/// ABI but cannot be safely invoked here — it is reported as an `"init"`
/// failure with a clear message. `Unsupported` carries an embedder-provided
/// reason byte-string.
pub enum DynamicSymbol {
    /// Function pointer that follows this build's Rust-native module ABI:
    /// `fn(&mut LuaState) -> Result<usize, LuaError>`.
    RustNative(LuaCFunction),
    /// Symbol exported against stock Lua 5.4's C ABI. The function pointer is
    /// resolved but never called from this build, since `lua_State *` is not
    /// our `LuaState`. Kept as a payload so a future C-ABI facade can pick it
    /// up; the embedder is responsible for ensuring the underlying library
    /// outlives this value.
    LuaCAbi(*const ()),
    /// Embedder-provided refusal reason, e.g. "symbol resolved but ABI version
    /// mismatch". Reported verbatim as an `"init"` failure.
    Unsupported { reason: Vec<u8> },
}

/// Function-pointer signature for loading a dynamic library, installed on
/// [`GlobalState::dynlib_load_hook`] by the embedder.
///
/// `libloading`/`dlopen`/`LoadLibraryEx` are FFI calls and require `unsafe`,
/// which is banned in `lua-stdlib`. `lua-cli` installs a `libloading`-backed
/// implementation. `None` causes `package.loadlib` to return the C-Lua
/// `"absent"` failure shape, matching the fallback platform stub.
///
/// `see_global` mirrors C-Lua's `seeglb` (POSIX `RTLD_GLOBAL`): set when the
/// caller invokes `package.loadlib(path, "*")`.
pub type DynLibLoadHook =
    fn(state: &mut LuaState, path: &[u8], see_global: bool) -> Result<DynLibId, LuaError>;

/// Function-pointer signature for resolving a symbol in a previously loaded
/// dynamic library, installed on [`GlobalState::dynlib_symbol_hook`].
///
/// The hook receives the [`DynLibId`] returned by [`DynLibLoadHook`] and the
/// requested symbol name. Returning `DynamicSymbol::RustNative` makes the
/// symbol callable; `LuaCAbi`/`Unsupported` propagate to `package.loadlib`
/// as an `"init"` failure with a clear message.
pub type DynLibSymbolHook =
    fn(state: &mut LuaState, handle: DynLibId, symbol: &[u8]) -> Result<DynamicSymbol, LuaError>;

/// Function-pointer signature for unloading a dynamic library, installed on
/// [`GlobalState::dynlib_unload_hook`].
///
/// Called from the `_CLIBS` `__gc` metamethod when the Lua state closes.
/// `libloading`'s safety model requires every loaded library to outlive the
/// last symbol it exports; the CLI backend is therefore free to ignore this
/// hook and keep libraries alive until process exit.
pub type DynLibUnloadHook = fn(handle: DynLibId);

/// One row of [`GlobalState::threads`]. Pairs the per-thread `LuaState`
/// with the canonical `GcRef<LuaThread>` so every `push_thread` for the
/// same id shares pointer-identity; the state is wrapped in
/// `Rc<RefCell<...>>` so `resume`/`yield` can mutate the child thread while
/// the parent holds a borrow.
pub struct ThreadRegistryEntry {
    /// The owned coroutine `LuaState`. Wrapped in `Rc<RefCell<...>>` so
    /// that `coroutine.resume` can borrow the child mutably while the
    /// parent is still in scope. Single-threaded — borrows never overlap
    /// in practice because only one resume path is live at a time.
    pub state: Rc<RefCell<LuaState>>,
    /// Canonical thread-value handle. Reused on every push so
    /// `GcRef::ptr_eq` is true across pushes.
    pub value: GcRef<lua_types::value::LuaThread>,
}

/// Stable key for a value pinned in [`ExternalRootSet`].
///
/// The generation is part of the key so a handle that has already unrooted its
/// slot cannot accidentally observe a later handle's value after slot reuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExternalRootKey {
    index: usize,
    generation: u64,
}

#[derive(Debug)]
struct ExternalRootSlot {
    value: Option<LuaValue>,
    generation: u64,
}

/// Values held alive by external Rust handles.
///
/// This is the embedding API's GC anchor. It intentionally lives directly on
/// `GlobalState` instead of inside the Lua registry table: handle drop/unroot
/// must be cheap, infallible, and independent of the Lua stack protocol.
#[derive(Debug, Default)]
pub struct ExternalRootSet {
    slots: Vec<ExternalRootSlot>,
    free: Vec<usize>,
    live: usize,
}

impl ExternalRootSet {
    pub fn insert(&mut self, value: LuaValue) -> ExternalRootKey {
        if let Some(index) = self.free.pop() {
            let slot = &mut self.slots[index];
            debug_assert!(slot.value.is_none(), "free external-root slot is occupied");
            slot.generation = slot.generation.wrapping_add(1).max(1);
            slot.value = Some(value);
            self.live += 1;
            ExternalRootKey {
                index,
                generation: slot.generation,
            }
        } else {
            let index = self.slots.len();
            self.slots.push(ExternalRootSlot {
                value: Some(value),
                generation: 1,
            });
            self.live += 1;
            ExternalRootKey {
                index,
                generation: 1,
            }
        }
    }

    pub fn get(&self, key: ExternalRootKey) -> Option<&LuaValue> {
        let slot = self.slots.get(key.index)?;
        if slot.generation == key.generation {
            slot.value.as_ref()
        } else {
            None
        }
    }

    pub fn replace(&mut self, key: ExternalRootKey, value: LuaValue) -> Option<LuaValue> {
        let slot = self.slots.get_mut(key.index)?;
        if slot.generation != key.generation || slot.value.is_none() {
            return None;
        }
        slot.value.replace(value)
    }

    pub fn remove(&mut self, key: ExternalRootKey) -> Option<LuaValue> {
        let slot = self.slots.get_mut(key.index)?;
        if slot.generation != key.generation {
            return None;
        }
        let old = slot.value.take()?;
        self.free.push(key.index);
        self.live -= 1;
        Some(old)
    }

    pub fn iter_values(&self) -> impl Iterator<Item = &LuaValue> {
        self.slots.iter().filter_map(|slot| slot.value.as_ref())
    }

    pub fn len(&self) -> usize {
        self.live
    }

    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    pub fn vacant_len(&self) -> usize {
        self.free.len()
    }
}

/// Process-wide state shared by all Lua threads.
///
/// Not exposed directly at the API; accessed via `state.global()` / `state.global_mut()`.
pub struct GlobalState {
    /// Hook for the Lua text parser. Set by the embedder (`lua-cli`
    /// or stdlib host) to bridge the cyclic crate split between `lua-vm` and
    /// `lua-parse`: when `f_parser` decides the chunk is text, it invokes
    /// this hook instead of the parser stub. `None` leaves the stub in place
    /// so unit tests that never load text still work.
    pub parser_hook: Option<ParserHook>,

    /// Transient slot carrying the CLI's `argv` into the `pmain` C closure.
    /// Mirrors `lua.c`'s `lua_pushinteger(argc)/lua_pushlightuserdata(argv)`
    /// arguments to `pmain`: a lua-rs C closure cannot capture Rust values, so
    /// `lua-cli`'s `run` parks `argv` here, pushes a zero-arg `pmain` closure,
    /// and `pcall_k`s it; `pmain` `take()`s it back out. Lives on `GlobalState`
    /// to keep `lua-cli` free of `unsafe` light-userdata round-tripping.
    pub cli_argv: Option<Vec<Vec<u8>>>,

    /// Transient slot carrying the CLI's native-module `preload` callback into
    /// the `pmain` C closure, paired with [`GlobalState::cli_argv`]. The type
    /// matches `lua-cli::interp::run`'s `preload` parameter.
    pub cli_preload: Option<fn(&mut LuaState) -> Result<(), LuaError>>,

    /// The Lua language version this state speaks. The single source of truth
    /// for version-gated behavior in the layers that read the state (parser,
    /// stdlib openers). The embedder sets this from the [`Lua`] instance's
    /// [`lua_types::LuaVersion`] at construction; it defaults to
    /// [`lua_types::LuaVersion::V54`] so any state built without an explicit
    /// version keeps the existing 5.4 behavior unchanged.
    pub lua_version: lua_types::LuaVersion,

    /// Hook for reading a Lua source file from disk. Set by `lua-cli`
    /// (or any embedder that wants `require`/`loadfile` to reach the file
    /// system) since `std::fs` is banned in `lua-stdlib`. `None` makes
    /// `loadfile` and the Lua-file searcher report a file-not-found error.
    pub file_loader_hook: Option<FileLoaderHook>,

    /// Hook for opening a file handle for read/write/append. Set by
    /// `lua-cli` since `std::fs` is banned in `lua-stdlib`. `None` causes
    /// `io.open` and `io.output(name)` to return an error; standard output and
    /// error are controlled separately through output hooks/native fallbacks.
    pub file_open_hook: Option<FileOpenHook>,

    /// Hook for host stdout. When absent, native builds fall back to Rust stdout
    /// for compatibility; bare `wasm32-unknown-unknown` reports stdout
    /// unavailable instead of touching a stubbed stdio implementation.
    pub stdout_hook: Option<OutputHook>,

    /// Hook for host stderr. See [`GlobalState::stdout_hook`].
    pub stderr_hook: Option<OutputHook>,

    /// Hook for host stdin. When absent, native builds fall back to Rust stdin
    /// for compatibility; bare `wasm32-unknown-unknown` behaves like EOF.
    pub stdin_hook: Option<InputHook>,

    /// Hook for host environment lookups. `None` makes `os.getenv` return nil.
    pub env_hook: Option<EnvHook>,

    /// Hook for host wall-clock time. Required for `os.time()` and `os.date()`
    /// without an explicit timestamp under bare WASM.
    pub unix_time_hook: Option<UnixTimeHook>,

    /// Hook for host program CPU time. Backs `os.clock`. When unset, native builds
    /// use a monotonic wall-clock baseline and bare WASM reports it unavailable.
    pub cpu_clock_hook: Option<CpuClockHook>,

    /// Hook for the host's local timezone offset at a given instant. Backs the
    /// local-time semantics of `os.date` (non-`!` formats) and `os.time`. When
    /// unset, both use UTC, matching the prior behaviour and keeping the
    /// `os.date`/`os.time` round-trip exact under bare WASM.
    pub local_offset_hook: Option<LocalOffsetHook>,

    /// Hook for host entropy. Used by default `math.randomseed` and table sort
    /// pivot randomisation; absent hooks fall back to deterministic seeds.
    pub entropy_hook: Option<EntropyHook>,

    /// Hook for host temporary filenames. Used by `os.tmpname` and `io.tmpfile`.
    pub temp_name_hook: Option<TempNameHook>,

    /// Hook for spawning a child process and connecting one stream
    /// (stdin or stdout) to a Lua file handle. Set by `lua-cli` since
    /// `std::process::Command` is banned in `lua-stdlib`. `None` causes
    /// `io.popen` to raise a Lua error rather than panic.
    pub popen_hook: Option<PopenHook>,

    /// Per-VM slot for `lua-stdlib`'s `io` stream side table (the `LStream`
    /// map keyed by file-handle userdata identity). Type-erased because the
    /// concrete map type lives in `lua-stdlib`, which this crate cannot name;
    /// `io_lib` lazily installs it on first use and downcasts on access.
    /// Owned here — rather than in a process-wide `thread_local!` — so the
    /// table dies with the VM: issue #317 traced the leak canary's
    /// per-iteration retention to stream entries of dropped VMs accumulating
    /// forever in the thread-global map whenever a file userdata landed at a
    /// never-before-seen address.
    pub io_streams: Option<std::rc::Rc<dyn std::any::Any>>,

    /// Hook for removing a file. Set by `lua-cli` since `std::fs` is
    /// banned in `lua-stdlib`. `None` causes `os.remove` to return an error.
    pub file_remove_hook: Option<FileRemoveHook>,

    /// Hook for renaming a file. Set by `lua-cli` since `std::fs` is
    /// banned in `lua-stdlib`. `None` causes `os.rename` to return an error.
    pub file_rename_hook: Option<FileRenameHook>,

    /// Hook for executing a shell command. Set by `lua-cli` since
    /// `std::process` is banned in `lua-stdlib`. `None` causes `os.execute`
    /// to report no shell available (matching C-Lua's `system(NULL) == 0`).
    pub os_execute_hook: Option<OsExecuteHook>,

    /// Hook for loading a dynamic library (`dlopen` /
    /// `LoadLibraryEx`). Set by `lua-cli` since `libloading` is FFI and
    /// requires `unsafe`, which is banned in `lua-stdlib`. `None` causes
    /// `package.loadlib` to return the `"absent"` fallback shape.
    pub dynlib_load_hook: Option<DynLibLoadHook>,

    /// Hook for resolving a symbol in a previously loaded
    /// dynamic library (`dlsym` / `GetProcAddress`). Set by `lua-cli`.
    /// `None` is treated as "absent" by `package.loadlib`.
    pub dynlib_symbol_hook: Option<DynLibSymbolHook>,

    /// Hook for unloading a dynamic library (`dlclose` /
    /// `FreeLibrary`). Set by `lua-cli`. `None` keeps libraries loaded
    /// until process exit, which matches `libloading`'s safety model.
    pub dynlib_unload_hook: Option<DynLibUnloadHook>,

    /// Per-runtime sandbox budget shared across all threads. Inactive by
    /// default (`interval == 0`); see [`SandboxLimits`].
    pub sandbox: SandboxLimits,

    pub gc_debt: isize,

    pub gc_estimate: usize,

    pub lastatomic: usize,

    pub l_registry: LuaValue,

    /// External Rust handles root their referents here while they are live.
    /// Traced from `GlobalState::trace`.
    pub external_roots: ExternalRootSet,

    /// The globals table, kept as a direct field rather than through the
    /// canonical registry path (`registry[LUA_RIDX_GLOBALS]`) because the
    /// lua-types `LuaTable` placeholder has no storage of its own;
    /// `get_global_table` reads it from here. Same reasoning for `loaded`
    /// (the module cache normally at `registry[_LOADED]`).
    pub globals: LuaValue,
    pub loaded: LuaValue,

    /// A dedicated completeness flag rather than C's trick of checking
    /// `ttisnil(&g->nilvalue)`. See `is_complete()`.
    pub nilvalue: LuaValue,

    pub seed: u32,

    pub currentwhite: u8,

    pub gcstate: u8,

    pub gckind: u8,

    pub gcstopem: bool,

    pub genminormul: u8,

    pub genmajormul: u8,

    pub gcstp: u8,

    pub gcemergency: bool,

    pub gcpause: u8,

    pub gcstepmul: u8,

    pub gcstepsize: u8,

    /// Lua 5.5 `collectgarbage("param", name [, value])` storage, indexed by
    /// [`Gc55Param`]: `[minormul, majorminor, minormajor, pause, stepmul,
    /// stepsize]`. The 5.5 GC parameters use a wider value range than the
    /// packed `u8` fields above, so they get their own storage. This is a
    /// faithful-shape backing store: it preserves the read-current /
    /// write-returns-old contract and the upstream default values, without
    /// claiming to retune the incremental collector. Initialized to the
    /// values observed on the reference `lua5.5.0` binary.
    pub gc55_params: [i64; 6],

    /// The GC owns its allgc/finobj/tobefnz/grayagain intrusive lists inside
    /// `self.heap` (`lua_gc::Heap`), not on `GlobalState` directly.
    pub sweepgc_cursor: usize,

    /// Cross-table weak-sweep registry.
    ///
    /// Heap collection snapshots this list before mark, then the post-mark
    /// weak-table pass clears entries whose weak target is held only by other
    /// weak slots. The registry holds weak table entries so it does not pin
    /// dead weak tables after sweep removes their heap allocation token.
    /// Replaced by proper `weak` / `ephemeron` / `allweak` cohorts once the
    /// Lua-style generational lists land.
    pub weak_tables_registry: lua_gc::WeakRegistry<WeakTableEntry>,

    /// Typed handles for finalizable tables/userdata. The heap owns the
    /// corresponding intrusive finobj/tobefnz list placement.
    pub finalizers: lua_gc::FinalizerRegistry<FinalizerObject>,

    /// Error raised by a `__gc` finalizer during an explicit `collectgarbage`
    /// on 5.2 / 5.3, parked here for the `collectgarbage` wrapper to re-raise.
    ///
    /// C-Lua re-throws the wrapped `error in __gc metamethod (%s)` directly out
    /// of `GCTM` via `luaD_throw`. The Rust `api::gc` entry point returns `i32`
    /// (its many callers cannot all thread a `Result`), so the explicit-collect
    /// path stashes the wrapped error here and the `collectgarbage` built-in
    /// drains it into the `Result<usize, LuaError>` it already returns. Only
    /// the explicit-collect path sets this; the automatic GC-step and close
    /// paths never do (matching `GCTM(L, 0)` and the dispatch-loop swallow).
    pub gc_finalizer_error: Option<LuaValue>,

    pub twups: Vec<GcRef<LuaState>>,

    pub panic: Option<LuaCFunction>,

    /// Always `None`: populating this would create a self-referential `Rc`
    /// cycle from the main thread's `LuaState` back to its own
    /// `GlobalState`. See `new_state`.
    pub mainthread: Option<GcRef<LuaState>>,

    /// Registry of all live coroutine threads, keyed by `ThreadId`.
    /// `coroutine.create` allocates a fresh `LuaState`, registers it here,
    /// and returns a value that resolves back to the same state on every
    /// `coroutine.status` / `coroutine.resume` call.
    ///
    /// Each entry pairs the per-thread `LuaState` with the canonical
    /// `GcRef<LuaThread>` value, so two `LuaValue::Thread` pushes of the
    /// same id share `GcRef::ptr_eq` identity. The main thread is NOT
    /// stored here — its `LuaState` is owned externally by the embedder.
    /// `main_thread_id` is reserved as `0` and a `LuaValue::Thread`
    /// carrying id `0` is recognized as the main thread by lookup helpers.
    pub threads: std::collections::HashMap<u64, ThreadRegistryEntry>,

    /// Cached `LuaValue::Thread` payload for the main thread (id 0).
    /// Built once during `new_state` so every `push_thread` on the main
    /// thread shares the same `GcRef<LuaThread>` and thus compares
    /// pointer-equal under `LuaValue::PartialEq`.
    pub main_thread_value: GcRef<lua_types::value::LuaThread>,

    /// Identity of the currently-running thread. `0` (main) until
    /// `coro_lib::resume` swaps it to the child coroutine's id; restored to
    /// the parent's id on yield or return.
    pub current_thread_id: u64,

    /// Thread currently being reset/closed by `coroutine.close`, if any. This is
    /// used to recognize reentrant closes from that thread's `__close` methods.
    pub closing_thread_id: Option<u64>,

    /// Identity of the main thread. Convention: `0`. Held as a field so
    /// the lookup helpers can read it without hard-coding the constant.
    pub main_thread_id: u64,

    /// Monotonic counter handing out fresh ids in `new_thread`. Starts
    /// at `1` because `0` is reserved for the main thread.
    pub next_thread_id: u64,

    /// Lua 5.1 per-thread global table (`lua_State.l_gt`) for coroutine
    /// threads, keyed by thread id.
    ///
    /// In 5.1 every thread has its own global table: `setfenv(0, t)` and
    /// `getfenv(0)` read/write the *running* thread's `l_gt`, and a freshly
    /// loaded top-level chunk takes the running thread's `l_gt` as its
    /// environment. A coroutine inherits its creator's `l_gt` at creation,
    /// after which the two are independent — a coroutine's `setfenv(0, t)`
    /// must not clobber the main thread's globals.
    ///
    /// The main thread (`main_thread_id`) is the single exception: its `l_gt`
    /// is the existing [`Self::globals`] field (single source of truth, never
    /// duplicated here). Only coroutine ids appear as keys, seeded in
    /// `new_thread`. Empty (and never read) on 5.2–5.5, where all threads
    /// share one global table and this field is inert. Traced as a root and
    /// pruned of dead-thread entries after each collection.
    pub thread_globals: std::collections::HashMap<u64, LuaValue>,

    /// Lua 5.1 per-closure environment for closures that carry no `_ENV`
    /// upvalue, keyed by closure identity ([`GcRef::identity`]).
    ///
    /// The reused modern parser threads an `_ENV` upvalue only onto closures
    /// that reference a free (global) name; a closure that references none has
    /// no such upvalue. 5.1's `setfenv(f, t)` must still set such a closure's
    /// environment and `getfenv(f)` must read it back, so the environment is
    /// stored here, independent of the upvalue array. A closure that *does*
    /// have an `_ENV` upvalue never appears here — its environment is that
    /// upvalue's closed value. Empty (and never read) on 5.2–5.5. Traced as a
    /// root and pruned of dead-closure entries after each collection.
    pub closure_envs: std::collections::HashMap<usize, LuaValue>,

    pub memerrmsg: GcRef<LuaString>,

    pub tmname: Vec<GcRef<LuaString>>,

    pub mt: [Option<GcRef<LuaTable>>; LUA_NUMTYPES],

    pub strcache: [[GcRef<LuaString>; STRCACHE_M]; STRCACHE_N],

    /// Stable intern map for the public [`LuaString`] type. The parser and
    /// stdlib need pointer-equality across `intern_str` calls so
    /// `GcRef::ptr_eq` can resolve variable identity. Without this map each
    /// call allocates a fresh `GcRef` and locals/upvalues fail to resolve.
    pub interned_lt: InternedStringMap,

    pub warnf: Option<Box<dyn FnMut(&[u8], bool)>>,

    /// State of the default warning handler (the `warnfoff`/`warnfon`/
    /// `warnfcont` chain from upstream `lauxlib.c`). `luaL_openlibs` installs
    /// `warnfoff`, so warnings start disabled until `warn("@on")`. Only
    /// consulted when no custom `warnf` was installed via the C API.
    pub warn_mode: WarnMode,

    /// testC/ltests warning sink, enabled only by the CLI's `LUA_RS_TESTC`
    /// support. It mirrors `ltests.c`'s `warnf`: a separate on/off bit, an
    /// output mode (`normal`, `allow`, `store`), and a continuation buffer so
    /// multi-part warnings can be asserted via global `_WARN`.
    pub test_warn_enabled: bool,
    pub test_warn_on: bool,
    pub test_warn_mode: TestWarnMode,
    pub test_warn_last_to_cont: bool,
    pub test_warn_buffer: Vec<u8>,

    /// Registry of native `LuaCFunction` pointers. Lua-types cannot reference
    /// `LuaState`, so `LuaClosure::LightC` carries a `usize` index into this
    /// vector instead of the real function pointer. `push_c_function`
    /// registers the function and stores the resulting index in the closure.
    pub c_functions: Vec<LuaCallable>,

    /// Owns the allgc intrusive list and runs collections. Starts paused;
    /// `new_state` unpauses it once the state is fully initialized, after
    /// which `step` runs during VM dispatch.
    pub heap: std::rc::Rc<lua_gc::Heap>,

    /// Cross-thread open-upvalue mirror. Maps `(thread_id, stack_idx)`
    /// to the live value of an open upvalue whose home thread is currently
    /// suspended while another thread runs. `coroutine.resume` snapshots the
    /// parent's open upvalues into this map before yielding control to the
    /// child, and reads the (possibly mutated) values back into the parent's
    /// stack when the child suspends or returns. From the running thread's
    /// perspective, `upvalue_get` / `upvalue_set` consult the mirror whenever
    /// an open upvalue's `thread_id` does not match `current_thread_id`.
    ///
    /// This avoids a stack refactor: the parent's `LuaState` is held by a
    /// `&mut` reference up the call stack during resume, so its stack cannot
    /// be reached directly through any `Rc<RefCell<_>>`. The mirror is the
    /// shared scratchpad that bridges the gap for the duration of a resume.
    pub cross_thread_upvals: std::collections::HashMap<(u64, StackIdx), LuaValue>,

    /// Workaround for GC use-after-free across coroutine boundaries.
    /// When `aux_resume` switches to a child thread, the parent's live stack
    /// values would otherwise become unreachable to the tracer for the duration
    /// of the resume (the parent `LuaState` is held only as a stack-borrowed
    /// `&mut` up the call chain and is not part of any traced root set). To
    /// keep those values alive, `aux_resume` pushes a snapshot of the parent
    /// stack here before transferring control, and pops it on suspension or
    /// completion. The tracer visits every snapshot as a GC root via the
    /// `Trace for GlobalState` impl in `trace_impls.rs`.
    ///
    /// A reachability-driven thread sweep supersedes most of this, but the
    /// snapshot still guards values that live only on the parent's stack
    /// (i.e. not yet rooted by any thread node).
    pub suspended_parent_stacks: Vec<Vec<LuaValue>>,

    /// Open-upvalue handles belonging to the same suspended parent windows as
    /// `suspended_parent_stacks`. Stack snapshots keep the pointed-to values
    /// alive; this roots the `UpVal` objects themselves so a GC inside the
    /// child coroutine cannot sweep entries still present in the parent's
    /// `openupval` list.
    pub suspended_parent_open_upvals: Vec<Vec<GcRef<UpVal>>>,

    /// Capacity pools for the two snapshot kinds above. A resume previously
    /// allocated and freed a fresh `Vec` per snapshot — 2 mallocs + 2 frees
    /// on every `coroutine.resume` (the top allocator frames in the
    /// coroutine_pingpong profile, 20260609T2243Z). Popped snapshots park
    /// their (cleared) buffers here for reuse; pool depth is bounded by the
    /// maximum resume nesting depth. The pooled vectors are always empty, so
    /// they root nothing.
    pub snapshot_stack_pool: Vec<Vec<LuaValue>>,
    pub snapshot_upval_pool: Vec<Vec<GcRef<UpVal>>>,

    /// Capacity pool for the transient value buffers a single `aux_resume`
    /// builds — the argument list copied off the parent stack on entry and the
    /// result list copied off the child stack on return. Each was a fresh
    /// `Vec<LuaValue>` allocated and freed per resume. Callers pop a buffer,
    /// fill it, drain it back to empty, then park it here; pool depth is
    /// bounded by the maximum resume-nesting depth. Parked buffers are always
    /// empty, so they root nothing.
    pub resume_value_pool: Vec<Vec<LuaValue>>,

    /// Capacity pool for the open-upvalue slot list `aux_resume` snapshots on
    /// entry (parent thread id + stack index per open upvalue). This buffer
    /// spans the child resume — it is read again on return to flush
    /// cross-thread upvalues back — so it follows the same pop/park discipline
    /// as the snapshot pools. The pairs are plain `Copy` data and root nothing.
    pub resume_upval_slot_pool: Vec<Vec<(u64, StackIdx)>>,

    /// Capacity pool for the upvalue-flush buffer `aux_resume` builds on return
    /// (stack index + mutated value per cross-thread upvalue). Popped, filled,
    /// drained back to the parent stack, then parked empty.
    pub resume_flush_pool: Vec<Vec<(StackIdx, LuaValue)>>,
}

/// `LUA_MASKCOUNT` (`1 << LUA_HOOKCOUNT`) — the count-hook event mask the
/// sandbox arms on every thread to drive per-interval budget enforcement.
const SANDBOX_COUNT_MASK: u8 = 1 << 3;

/// Sandbox trip code: not tripped.
pub const SANDBOX_TRIP_NONE: u8 = 0;
/// Sandbox trip code: the instruction budget reached zero.
pub const SANDBOX_TRIP_INSTRUCTIONS: u8 = 1;
/// Sandbox trip code: GC-tracked memory exceeded the configured ceiling.
pub const SANDBOX_TRIP_MEMORY: u8 = 2;

/// Per-runtime sandbox budget, shared by every thread (main + coroutines) via
/// the `Rc<RefCell<GlobalState>>` they all hold. Every field is a `Cell` so the
/// VM can charge the budget through the shared `Ref` it borrows in the
/// count-hook path — no `&mut` and no write-borrow on the hot path.
/// `interval == 0` means inactive; in that case the VM never sets the
/// count-hook mask, so there is zero overhead.
#[derive(Default)]
pub struct SandboxLimits {
    /// Count-hook interval in instructions; `0` = sandbox inactive.
    pub interval: std::cell::Cell<i32>,
    /// Whether an instruction budget is enforced.
    pub instr_limited: std::cell::Cell<bool>,
    /// Instructions left before the budget trips.
    pub instr_remaining: std::cell::Cell<u64>,
    /// Configured instruction limit, retained so `reset` can refill.
    pub instr_limit: std::cell::Cell<u64>,
    /// GC-byte ceiling; `None` = no memory limit.
    pub mem_limit: std::cell::Cell<Option<usize>>,
    /// One of the `SANDBOX_TRIP_*` codes.
    pub tripped: std::cell::Cell<u8>,
    /// Sticky once a limit trips: the abort is *uncatchable*. While set,
    /// `pcall`/`xpcall`/`coroutine.resume` re-raise the trip error instead of
    /// swallowing it, so untrusted code cannot defeat the budget by catching
    /// it in a loop. Cleared only by [`LuaState::sandbox_reset`].
    pub aborting: std::cell::Cell<bool>,
}

impl GlobalState {
    /// True while a sandbox instruction/memory budget is active on this runtime.
    pub fn sandbox_active(&self) -> bool {
        self.sandbox.interval.get() != 0
    }

    /// Total live bytes allocated, as reported by the collector-owned heap
    /// accounting model.
    pub fn total_bytes(&self) -> usize {
        self.heap.bytes_used().max(1)
    }

    /// Look up the coroutine `LuaState` registered under `id`. Returns
    /// `None` for the main-thread id (the main `LuaState` is owned by
    /// the embedder, not stored in `threads`) and for ids that were
    /// never issued or have already been closed.
    pub fn get_thread(&self, id: u64) -> Option<&ThreadRegistryEntry> {
        self.threads.get(&id)
    }

    /// Return the canonical `GcRef<LuaThread>` for `id`. For the main
    /// thread that's `main_thread_value`; for a coroutine it's the
    /// value stored in the registry. Returns `None` if `id` is unknown.
    pub fn thread_value_for(&self, id: u64) -> Option<GcRef<lua_types::value::LuaThread>> {
        if id == self.main_thread_id {
            Some(self.main_thread_value.clone())
        } else {
            self.threads.get(&id).map(|e| e.value.clone())
        }
    }

    /// Returns `true` when the state has been fully initialized. Checks
    /// `nilvalue == Nil`, mirroring C's check of `ttisnil(&g->nilvalue)`.
    pub fn is_complete(&self) -> bool {
        matches!(self.nilvalue, LuaValue::Nil)
    }

    /// Returns the "current white" GC color bitmask.
    ///
    /// The effective dual-white collector state lives in `lua_gc::Heap`;
    /// this field preserves the translated `global_State` shape for code
    /// that still reads the upstream bitmask.
    pub fn current_white(&self) -> u8 {
        self.currentwhite
    }

    /// Returns the "other white" GC color bitmask.
    pub fn other_white(&self) -> u8 {
        self.currentwhite ^ 0x03
    }

    /// Returns `true` if the GC is in generational mode.
    pub fn is_gen_mode(&self) -> bool {
        self.gckind == GcKind::Generational as u8 || self.lastatomic != 0
    }

    /// Returns `true` if the GC is currently running.
    pub fn gc_running(&self) -> bool {
        self.gcstp == 0
    }

    /// Returns `true` while the GC is in its propagation phase.
    pub fn keep_invariant(&self) -> bool {
        self.heap.gc_state().is_invariant()
    }

    /// Returns `true` while the GC is in a sweep phase.
    pub fn is_sweep_phase(&self) -> bool {
        self.heap.gc_state().is_sweep()
    }

    // ── GC parameter accessors ────────────────────────────────────────────────
    pub fn gc_debt(&self) -> isize {
        self.gc_debt
    }
    pub fn set_gc_debt(&mut self, d: isize) {
        self.gc_debt = d;
    }
    pub fn gc_at_pause(&self) -> bool {
        self.heap.gc_state().is_pause()
    }
    fn get_gc_param(p: u8) -> i32 {
        (p as i32) * 4
    }
    fn set_gc_param_slot(slot: &mut u8, p: i32) {
        *slot = (p / 4) as u8;
    }
    pub fn gc_pause_param(&self) -> i32 {
        Self::get_gc_param(self.gcpause)
    }
    pub fn set_gc_pause_param(&mut self, p: i32) {
        Self::set_gc_param_slot(&mut self.gcpause, p);
    }
    pub fn gc_stepmul_param(&self) -> i32 {
        Self::get_gc_param(self.gcstepmul)
    }
    pub fn set_gc_stepmul_param(&mut self, p: i32) {
        Self::set_gc_param_slot(&mut self.gcstepmul, p);
    }
    pub fn gc_genmajormul_param(&self) -> i32 {
        Self::get_gc_param(self.genmajormul)
    }
    pub fn set_gc_genmajormul(&mut self, p: i32) {
        Self::set_gc_param_slot(&mut self.genmajormul, p);
    }
    /// Lua 5.5 `collectgarbage("param", name [, value])`. `idx` is the 0-based
    /// param index (`minormul=0 .. stepsize=5`). When `value >= 0` the param is
    /// set; the previous value is always returned.
    pub fn gc55_param(&mut self, idx: usize, value: i64) -> i64 {
        let old = self.gc55_params[idx];
        if value >= 0 {
            self.gc55_params[idx] = value;
        }
        old
    }
    pub fn gc_stop_flags(&self) -> u8 {
        self.gcstp
    }
    pub fn set_gc_stop_flags(&mut self, f: u8) {
        self.gcstp = f;
    }
    pub fn stop_gc_internal(&mut self) -> u8 {
        let old = self.gcstp;
        self.gcstp |= GCSTPGC;
        old
    }
    pub fn set_gc_stop_user(&mut self) {
        // GCSTPUSR (lgc.h:155) = 1 — bit set when GC is stopped by user (lua_gc(L, LUA_GCSTOP)).
        self.gcstp = GCSTPUSR;
    }
    pub fn clear_gc_stop(&mut self) {
        self.gcstp = 0;
    }
    pub fn is_gc_running(&self) -> bool {
        self.gcstp == 0
    }
    /// True when the GC has been disabled internally (state setup, mid-GC,
    /// or while closing); user-stop via `collectgarbage("stop")` does NOT
    /// set this bit, so `lua_gc` continues to honour Count/Step/etc.
    ///
    pub fn is_gc_stopped_internally(&self) -> bool {
        (self.gcstp & GCSTPGC) != 0
    }

    /// Returns the interned `__xxx` name string for tag method `tm`, or
    /// `None` if `tmname` has not yet been initialised (early bootstrap).
    ///
    /// The lua-vm crate carries two distinct `TagMethod` enums (one in
    /// `lua-types`, one in `crate::tagmethods`) with identical
    /// `#[repr(u8)]` ordering. The [`TmIndex`] trait bridges them so callers
    /// from either side can index `tmname` uniformly.
    pub fn tm_name<T: TmIndex>(&self, tm: T) -> Option<GcRef<LuaString>> {
        self.tmname.get(tm.tm_index()).cloned()
    }
}

/// Discriminant-to-index conversion for the two parallel `TagMethod` enums.
///
/// Both `lua_types::tagmethod::TagMethod` and `crate::tagmethods::TagMethod`
/// are `#[repr(u8)]` with the same ORDER TM layout, so casting through `u8`
/// yields the correct `GlobalState.tmname` index for either type.
pub trait TmIndex: Copy {
    fn tm_index(self) -> usize;
}
impl TmIndex for lua_types::tagmethod::TagMethod {
    fn tm_index(self) -> usize {
        self as u8 as usize
    }
}
impl TmIndex for crate::tagmethods::TagMethod {
    fn tm_index(self) -> usize {
        self as u8 as usize
    }
}
impl TmIndex for usize {
    fn tm_index(self) -> usize {
        self
    }
}
impl TmIndex for u8 {
    fn tm_index(self) -> usize {
        self as usize
    }
}

use lua_types::tagmethod::TagMethod;

// ─── LuaState ────────────────────────────────────────────────────────────────

/// Per-thread Lua execution state.
///
/// All stack-pointer fields in C (`StkIdRel`, `StkId`) become `StackIdx` (u32
/// index into `stack: Vec<StackValue>`).  The C intrusive `CallInfo` linked list
/// becomes `call_info: Vec<CallInfo>` indexed by `CallInfoIdx`.
///
/// No `extra_space` field: C's `LUA_EXTRASPACE` reserves a small, fixed-size
/// raw memory region immediately before `lua_State` for embedder bookkeeping,
/// addressable via `lua_getextraspace` without a registry lookup — a niche
/// embedding feature (issue #275 item 3). This port has no equivalent field
/// or accessor; an embedder that needs per-thread scratch storage should use
/// the registry (`Lua::create_registry_value`/`set_named_registry_value`) or
/// a side table keyed by thread identity instead. Tracked as a known
/// unsupported embedding feature in `docs/EMBEDDING_API_IMPLEMENTATION.md`'s
/// Known Limits.
pub struct LuaState {
    // ── Thread status ──

    pub status: u8,

    pub allowhook: bool,

    pub nci: u32,

    // ── Stack ──

    pub top: StackIdx,

    /// Redundant once the stack is a `Vec` (kept for parity with the C
    /// field).
    pub stack_last: StackIdx,

    pub stack: Vec<StackValue>,

    // ── Call info ──

    pub ci: CallInfoIdx,

    /// C's `base_ci` is `call_info[0]` here; there is no separate field.
    pub call_info: Vec<CallInfo>,

    // ── Upvalues / to-be-closed ──

    pub openupval: Vec<GcRef<UpVal>>,

    /// Per-thread mirror of C 5.3's per-open-upvalue `touched` flag
    /// (`lfunc.h` `UpVal.u.open.touched`). Set when this thread creates or
    /// writes an open upvalue; consulted only by the legacy (5.1/5.2/5.3)
    /// `remarkupvals` pass in [`trace_reachable_threads`]. In those versions
    /// an unmarked thread's touched open upvalues have their values re-marked
    /// **once** during the atomic phase, then the flag is cleared (mirroring
    /// C clearing `touched` and dropping the thread from `g->twups`). This is
    /// what makes a cycle reachable only through a suspended thread survive
    /// one extra collection before being finalized. From 5.4 the C condition
    /// became `!iswhite(uv)` instead of `touched`, so this flag is never read
    /// for modern versions and the baked 5.4/5.5 behavior is unchanged.
    pub legacy_open_upval_touched: std::cell::Cell<bool>,

    pub tbclist: Vec<StackIdx>,

    // ── Global state ──

    /// Shared via `Rc<RefCell<>>` for shared ownership across coroutine
    /// threads. C's `l_G` is accessed through the [`LuaState::global`] /
    /// [`LuaState::global_mut`] methods rather than a raw field.
    pub(crate) global: Rc<RefCell<GlobalState>>,

    // ── Hooks ──

    pub hook: Option<Box<dyn FnMut(&mut LuaState, &crate::debug::LuaDebug)>>,

    pub hookmask: u8,

    pub basehookcount: i32,

    pub hookcount: i32,

    // ── Error handling ──

    // C's `errorJmp` (setjmp/longjmp chain) has no field here — the `?`
    // operator on `Result<T, LuaError>` replaces it entirely.

    pub errfunc: isize,

    // ── C-call depth ──

    pub n_ccalls: u32,

    // ── Debug / hooks ──

    pub oldpc: u32,

    // ── GC color ──

    pub marked: u8,

    /// Owner thread id for this `LuaState`, cached as a plain `u64` so the
    /// hot path of `upvalue_get` can compare against an open upvalue's
    /// `thread_id` without taking a `RefCell::borrow` on the shared
    /// `GlobalState`.
    ///
    /// Invariant: while this `LuaState` is the actively running thread,
    /// `GlobalState::current_thread_id == self.cached_thread_id`. This is
    /// maintained structurally by `new_state`/`new_thread` (which set
    /// `cached_thread_id` to the thread's own id once at construction)
    /// combined with the coroutine resume protocol: `coro_lib::resume`
    /// writes `co_state.global.current_thread_id = co_id` before the
    /// coroutine runs, and restores `parent_thread_id` on yield/return.
    /// Because each thread caches its own id (not the global's id), the
    /// invariant survives every context switch without an explicit refresh
    /// at the resume site.
    pub cached_thread_id: u64,

    /// Local GC gate.
    ///
    /// Avoids borrowing `GlobalState` on every call edge when GC/finalizers
    /// are not currently due.
    pub gc_check_needed: bool,
}

impl LuaState {
    /// Access the process-wide `GlobalState` immutably.
    ///
    /// Returns `std::cell::Ref<GlobalState>` because `GlobalState` is held in
    /// `Rc<RefCell<...>>`. Call sites that do `state.global().field` work fine
    /// via `Deref`. Callers must not hold the `Ref` across a `global_mut()` call.
    pub fn global(&self) -> std::cell::Ref<'_, GlobalState> {
        self.global.borrow()
    }

    /// Access the process-wide `GlobalState` mutably.
    pub fn global_mut(&self) -> std::cell::RefMut<'_, GlobalState> {
        self.global.borrow_mut()
    }

    /// Clone the `Rc` handle to the GlobalState for sharing with a new coroutine.
    ///
    /// Used in `new_thread` to give the child thread access to the same GlobalState.
    pub fn global_rc(&self) -> Rc<RefCell<GlobalState>> {
        Rc::clone(&self.global)
    }

    /// Lua 5.1 global table of the thread `id` (`lua_State.l_gt`).
    ///
    /// The main thread's `l_gt` is the shared [`GlobalState::globals`] field;
    /// a coroutine's lives in [`GlobalState::thread_globals`] under its id,
    /// seeded at creation in `new_thread`. A coroutine id that is missing from
    /// the map is a `new_thread` seeding bug, not a recoverable state.
    ///
    /// 5.1 only. Other versions never call this — they share one global table.
    pub fn v51_thread_lgt(&self, id: u64) -> LuaValue {
        let g = self.global();
        if id == g.main_thread_id {
            g.globals.clone()
        } else {
            g.thread_globals
                .get(&id)
                .expect("v51 coroutine l_gt must be seeded in new_thread")
                .clone()
        }
    }

    /// Set the Lua 5.1 global table of thread `id` (`lua_State.l_gt`).
    ///
    /// Writes the main thread's `l_gt` through [`GlobalState::globals`] and a
    /// coroutine's through [`GlobalState::thread_globals`]. 5.1 only.
    pub fn v51_set_thread_lgt(&self, id: u64, t: LuaValue) {
        let mut g = self.global_mut();
        if id == g.main_thread_id {
            g.globals = t;
        } else {
            g.thread_globals.insert(id, t);
        }
    }

    /// Enable the ltests-style warning sink used by `LUA_RS_TESTC`.
    pub fn enable_test_warning_handler(&mut self) -> Result<(), LuaError> {
        {
            let mut g = self.global_mut();
            g.test_warn_enabled = true;
            g.test_warn_on = false;
            g.test_warn_mode = TestWarnMode::Normal;
            g.test_warn_last_to_cont = false;
            g.test_warn_buffer.clear();
        }
        self.push(LuaValue::Bool(false));
        crate::api::set_global(self, b"_WARN")
    }

    /// Return the current C-call recursion depth (lower 16 bits of `n_ccalls`).
    pub fn c_calls(&self) -> u32 {
        self.n_ccalls & 0xffff
    }

    /// Increment the non-yieldable call count (upper 16 bits of `n_ccalls`).
    pub fn inc_nny(&mut self) {
        self.n_ccalls += 0x10000;
    }

    /// Decrement the non-yieldable call count.
    pub fn dec_nny(&mut self) {
        self.n_ccalls -= 0x10000;
    }

    /// Returns `true` if the thread can yield (no non-yieldable frames on the stack).
    pub fn is_yieldable(&self) -> bool {
        (self.n_ccalls & 0xffff0000) == 0
    }

    /// Reset the hook countdown to the baseline.
    pub fn reset_hook_count(&mut self) {
        self.hookcount = self.basehookcount;
    }

    /// Activate the per-runtime sandbox budget and arm the current thread.
    ///
    /// Stores the budget in `GlobalState` (shared across every thread) and
    /// sets the count-hook mask on this thread so the dispatch loop traps every
    /// `interval` instructions. Coroutines created afterwards inherit the mask
    /// via `preinit_thread`, so metering spans all threads — closing the
    /// coroutine-escape that a per-thread closure could not. Pass `None` for a
    /// limit to leave that dimension unbounded.
    pub fn install_sandbox_limits(
        &mut self,
        interval: i32,
        instr_limit: Option<u64>,
        mem_limit: Option<usize>,
    ) {
        let interval = interval.max(1);
        {
            let g = self.global();
            g.sandbox.interval.set(interval);
            g.sandbox.instr_limited.set(instr_limit.is_some());
            g.sandbox.instr_remaining.set(instr_limit.unwrap_or(0));
            g.sandbox.instr_limit.set(instr_limit.unwrap_or(0));
            g.sandbox.mem_limit.set(mem_limit);
            g.sandbox.tripped.set(SANDBOX_TRIP_NONE);
        }
        self.hookmask |= SANDBOX_COUNT_MASK;
        self.basehookcount = interval;
        self.hookcount = interval;
        crate::debug::arm_traps(self);
    }

    /// Charge the shared budget for one count-hook interval. Returns the abort
    /// error if a limit has been crossed (and records why in `tripped`).
    /// Called from `trace_exec` on every thread, once per `interval`
    /// instructions — never on the budget-disabled hot path.
    pub fn sandbox_charge_interval(&self) -> Option<LuaError> {
        let interval = self.global().sandbox.interval.get();
        self.sandbox_charge(interval as u64)
    }

    /// Charge `amount` instructions against the runtime-wide budget and sample
    /// the memory ceiling. Returns the uncatchable abort error if a limit is
    /// crossed (recording the reason and arming the sticky `aborting` flag), or
    /// `None` otherwise. No-op when no sandbox is active.
    ///
    /// Used both by the per-interval VM charge and by loop-heavy stdlib
    /// functions (the pattern matcher) so a single native call cannot run for
    /// longer than the instruction budget allows.
    pub fn sandbox_charge(&self, amount: u64) -> Option<LuaError> {
        let g = self.global();
        if g.sandbox.interval.get() == 0 {
            return None;
        }
        if g.sandbox.instr_limited.get() {
            let rem = g.sandbox.instr_remaining.get().saturating_sub(amount);
            g.sandbox.instr_remaining.set(rem);
            if rem == 0 {
                g.sandbox.tripped.set(SANDBOX_TRIP_INSTRUCTIONS);
                g.sandbox.aborting.set(true);
                return Some(LuaError::runtime(format_args!(
                    "sandbox: instruction budget exhausted"
                )));
            }
        }
        if let Some(limit) = g.sandbox.mem_limit.get() {
            if g.total_bytes() > limit {
                g.sandbox.tripped.set(SANDBOX_TRIP_MEMORY);
                g.sandbox.aborting.set(true);
                return Some(LuaError::runtime(format_args!(
                    "sandbox: memory limit exceeded"
                )));
            }
        }
        None
    }

    /// Reject a size-known-upfront allocation that would push GC-tracked memory
    /// past the ceiling, *before* the buffer is built. Returns the uncatchable
    /// memory abort if `total_bytes() + additional` exceeds the limit. Used by
    /// stdlib functions that allocate a large buffer of a computed size in one
    /// instruction (e.g. `string.rep`, `string.pack`, `table.concat`), where the
    /// per-instruction `sandbox_check_memory` would only fire *after* the
    /// allocation already happened.
    pub fn sandbox_reserve(&self, additional: usize) -> Option<LuaError> {
        let g = self.global();
        if g.sandbox.interval.get() == 0 {
            return None;
        }
        if let Some(limit) = g.sandbox.mem_limit.get() {
            let projected = g.total_bytes().saturating_add(additional);
            if projected > limit {
                g.sandbox.tripped.set(SANDBOX_TRIP_MEMORY);
                g.sandbox.aborting.set(true);
                return Some(LuaError::runtime(format_args!(
                    "sandbox: memory limit exceeded"
                )));
            }
        }
        None
    }

    /// Upper bound on the work a single pattern-match call may do before it must
    /// stop and let the caller charge the budget. Equal to the remaining
    /// instruction budget when an instruction limit is active, else `0` meaning
    /// "unlimited" (preserving non-sandboxed behavior exactly).
    pub fn sandbox_match_step_limit(&self) -> u64 {
        let g = self.global();
        if g.sandbox.interval.get() != 0 && g.sandbox.instr_limited.get() {
            g.sandbox.instr_remaining.get()
        } else {
            0
        }
    }

    /// Whether a sandbox abort is in flight. While true, protected-call builtins
    /// (`pcall`/`xpcall`/`coroutine.resume`) must re-raise rather than catch, so
    /// the budget trip is uncatchable. Set on trip, cleared by `sandbox_reset`.
    pub fn sandbox_aborting(&self) -> bool {
        self.global().sandbox.aborting.get()
    }

    /// Whether an instruction budget is active (vs. only a memory limit / none).
    pub fn sandbox_instr_limited(&self) -> bool {
        self.global().sandbox.instr_limited.get()
    }

    /// Instructions left before the budget trips (meaningful only when
    /// [`sandbox_instr_limited`](Self::sandbox_instr_limited)).
    pub fn sandbox_instr_remaining(&self) -> u64 {
        self.global().sandbox.instr_remaining.get()
    }

    /// The configured instruction limit (for computing "used").
    pub fn sandbox_instr_limit(&self) -> u64 {
        self.global().sandbox.instr_limit.get()
    }

    /// The current trip code (one of the `SANDBOX_TRIP_*` constants).
    pub fn sandbox_tripped_code(&self) -> u8 {
        self.global().sandbox.tripped.get()
    }

    /// Refill the instruction budget to its configured limit and clear the
    /// trip flag, so the same runtime can run another chunk.
    pub fn sandbox_reset(&self) {
        let g = self.global();
        if g.sandbox.instr_limited.get() {
            g.sandbox.instr_remaining.set(g.sandbox.instr_limit.get());
        }
        g.sandbox.tripped.set(SANDBOX_TRIP_NONE);
        g.sandbox.aborting.set(false);
    }

    /// Returns the current stack capacity (slots between base and stack_last).
    pub fn stack_size(&self) -> usize {
        self.stack_last.0 as usize
    }

    /// Push a value onto the stack, incrementing `top`.
    #[inline(always)]
    pub fn push(&mut self, val: LuaValue) {
        let top = self.top.0 as usize;
        if top < self.stack.len() {
            self.stack[top] = StackValue { val };
        } else {
            self.stack.push(StackValue { val });
        }
        self.top = StackIdx(self.top.0 + 1);
    }

    /// Pop the top value from the stack, decrementing `top`.
    ///
    #[inline(always)]
    pub fn pop(&mut self) -> LuaValue {
        if self.top.0 == 0 {
            return LuaValue::Nil;
        }
        self.top = StackIdx(self.top.0 - 1);
        self.stack[self.top.0 as usize].val.clone()
    }

    /// Retrieve the value at the given stack index without removing it.
    #[inline(always)]
    pub fn stack_val(&self, idx: StackIdx) -> &LuaValue {
        &self.stack[idx.0 as usize].val
    }

    /// Write a value to a specific stack slot.
    #[inline(always)]
    pub fn set_stack_val(&mut self, idx: StackIdx, val: LuaValue) {
        self.stack[idx.0 as usize].val = val;
    }

    /// Returns a handle for GC operations (checked/forced collection,
    /// barriers) implemented in `lua-gc`.
    pub fn gc(&mut self) -> GcHandle<'_> {
        GcHandle { _state: self }
    }

    /// Pin a Lua value in the external root set and return its stable key.
    pub fn external_root_value(&mut self, value: LuaValue) -> ExternalRootKey {
        self.global_mut().external_roots.insert(value)
    }

    /// Read a value currently pinned by an external root key.
    pub fn external_rooted_value(&self, key: ExternalRootKey) -> Option<LuaValue> {
        self.global().external_roots.get(key).cloned()
    }

    /// Replace the value pinned by an external root key.
    pub fn external_replace_root(
        &mut self,
        key: ExternalRootKey,
        value: LuaValue,
    ) -> Option<LuaValue> {
        self.global_mut().external_roots.replace(key, value)
    }

    /// Remove an external root. Returns `None` for stale or already-removed keys.
    pub fn external_unroot_value(&mut self, key: ExternalRootKey) -> Option<LuaValue> {
        self.global_mut().external_roots.remove(key)
    }

    /// Best-effort external root removal for destructors that may run while
    /// the collector holds an immutable `GlobalState` borrow.
    pub fn try_external_unroot_value(
        &mut self,
        key: ExternalRootKey,
    ) -> std::result::Result<Option<LuaValue>, std::cell::BorrowMutError> {
        self.global
            .try_borrow_mut()
            .map(|mut global| global.external_roots.remove(key))
    }

    /// Create a new empty table and register it with the GC.
    pub fn new_table(&mut self) -> GcRef<LuaTable> {
        self.mark_gc_check_needed();
        GcRef::new(LuaTable::placeholder())
    }

    /// Create a fresh table with pre-sized array/hash parts.
    ///
    /// mirrors the `luaH_new` + `luaH_resize` pair in one call so we don't
    /// pay an extra resize path for hot construction sites.
    pub fn new_table_with_sizes(
        &mut self,
        array_size: u32,
        hash_size: u32,
    ) -> Result<GcRef<LuaTable>, LuaError> {
        self.mark_gc_check_needed();
        let t = GcRef::new(LuaTable::placeholder());
        self.table_resize(&t, array_size as usize, hash_size as usize)?;
        Ok(t)
    }

    /// Intern a byte string in the global string pool.
    ///
    /// In C, short strings (≤ LUAI_MAXSHORTLEN = 40 bytes) are interned globally
    /// via `luaS_newlstr`, while long strings allocate a fresh TString each
    /// call so distinct long strings keep distinct object identity (observable
    /// via `string.format("%p", s)`). The parser separately deduplicates
    /// long-string literals within a single chunk through `luaX_newstring`'s
    /// `ls->h` anchor table.
    ///
    /// Short-string interning, `luaS_newlstr` shape: one hash of the input
    /// bytes, a bucket walk on hit (zero allocation), and an insert that
    /// reuses the same hash on miss. See the `InternedStringMap` doc for why
    /// this replaced the owned-key `HashMap` entry API.
    pub fn intern_str(&mut self, bytes: &[u8]) -> Result<GcRef<LuaString>, LuaError> {
        if bytes.len() <= crate::string::MAX_SHORT_LEN {
            let hash = LuaString::hash_bytes(bytes, 0);
            {
                let global = self.global();
                if let Some(existing) = global.interned_lt.find(bytes, hash) {
                    return Ok(existing);
                }
            }
            let new_ref = GcRef::new(LuaString::from_slice(bytes));
            new_ref.account_buffer(new_ref.buffer_bytes() as isize);
            self.global_mut().interned_lt.insert(new_ref.clone());
            self.mark_gc_check_needed();
            Ok(new_ref)
        } else {
            self.mark_gc_check_needed();
            let new_ref = GcRef::new(LuaString::from_slice(bytes));
            new_ref.account_buffer(new_ref.buffer_bytes() as isize);
            Ok(new_ref)
        }
    }

    /// Returns the current CallInfo index (the active call frame).
    #[inline(always)]
    pub fn top_idx(&self) -> StackIdx {
        self.top
    }
}

// ─── Stack / register access ───────────────────────────────────────────────
//
// Methods used by api.rs, debug.rs, do_.rs, vm.rs, tagmethods.rs, etc.

impl LuaState {
    #[inline(always)]
    pub fn get_at(&self, idx: impl Into<StackIdxConv>) -> LuaValue {
        let i: StackIdx = idx.into().0;
        match self.stack.get(i.0 as usize) {
            Some(slot) => slot.val.clone(),
            None => LuaValue::Nil,
        }
    }
    #[inline(always)]
    pub fn set_at(&mut self, idx: impl Into<StackIdxConv>, v: LuaValue) {
        let i: StackIdx = idx.into().0;
        self.stack[i.0 as usize].val = v;
    }

    /// Clear stack slots in `[start, end)` without changing `top`.
    ///
    /// Under the exact-rooting model (abe2b52) the collector traces only the
    /// live window `[0, top)` and, per collect, nils the dead tail above `top`
    /// up to the high-water mark before tracing — so a reserved-but-unused tail
    /// is normally cleared by the collector itself, not by callers. This helper
    /// exists for the call-setup paths that raise `ci.top` above the live `top`
    /// (e.g. a tail call reserving its callee frame): on those paths the gap
    /// `[live_top, new_ci_top)` can be reached by the per-collect dead-tail
    /// clear and the trace within a single collect off the same raised top, so
    /// it must not retain stale collectable `GcRef`s left by a previous frame —
    /// retaining them is the #140 use-after-free class. Callers clear the gap
    /// here so the raised top is GC-safe before any allocation can collect.
    pub fn clear_stack_range(&mut self, start: StackIdx, end: StackIdx) {
        if end.0 <= start.0 {
            return;
        }
        let end_u = end.0 as usize;
        if end_u > self.stack.len() {
            self.stack.resize_with(end_u, StackValue::default);
        }
        for i in start.0..end.0 {
            self.stack[i as usize].val = LuaValue::Nil;
        }
    }
    /// Hot-path accessor: returns `Some(i)` only when the stack slot at `idx`
    /// holds a `LuaValue::Int(i)`. Returns `None` for any other tag (including
    /// out-of-bounds, which behaves as `Nil`).
    ///
    /// `ttisinteger` predicate that gates the integer arithmetic fast path in
    /// `lvm.c`'s `op_arith_aux` macro. Avoids the full `LuaValue` clone that
    /// `get_at` performs — the operand is only needed for its `i64` payload.
    #[inline(always)]
    pub fn get_int_at(&self, idx: impl Into<StackIdxConv>) -> Option<i64> {
        let i: StackIdx = idx.into().0;
        match self.stack.get(i.0 as usize) {
            Some(slot) => match &slot.val {
                LuaValue::Int(v) => Some(*v),
                _ => None,
            },
            None => None,
        }
    }
    /// Hot-path accessor: returns `Some((a, b))` only when both stack slots
    /// at `rb` and `rc` hold integers. Equivalent to two `get_int_at` calls
    /// but is shaped so the arithmetic opcode dispatch arms can pattern-match
    /// the common case with a single `if let`.
    ///
    /// the `op_arith_aux` macro.
    #[inline(always)]
    pub fn get_int_pair_at(
        &self,
        rb: impl Into<StackIdxConv>,
        rc: impl Into<StackIdxConv>,
    ) -> Option<(i64, i64)> {
        let rb: StackIdx = rb.into().0;
        let rc: StackIdx = rc.into().0;
        match (self.stack[rb.0 as usize].val, self.stack[rc.0 as usize].val) {
            (LuaValue::Int(ib), LuaValue::Int(ic)) => Some((ib, ic)),
            _ => None,
        }
    }
    /// Hot-path accessor: returns `Some(f)` when the slot holds a `Float(f)`
    /// or coerces an `Int(i)` to `f64`. Returns `None` for any other tag.
    /// No `LuaValue` clone — only the primitive payload travels back.
    ///
    #[inline(always)]
    pub fn get_num_at(&self, idx: impl Into<StackIdxConv>) -> Option<f64> {
        let i: StackIdx = idx.into().0;
        match self.stack.get(i.0 as usize) {
            Some(slot) => match &slot.val {
                LuaValue::Float(f) => Some(*f),
                LuaValue::Int(v) => Some(*v as f64),
                _ => None,
            },
            None => None,
        }
    }
    /// Hot-path accessor: returns `Some(f)` only when the slot holds a
    /// `LuaValue::Float(f)`. Does NOT coerce integers; the integer branch is
    /// the caller's responsibility. Used by opcode arms that have already
    /// ruled out the integer fast path.
    #[inline(always)]
    pub fn get_float_at(&self, idx: impl Into<StackIdxConv>) -> Option<f64> {
        let i: StackIdx = idx.into().0;
        match self.stack.get(i.0 as usize) {
            Some(slot) => match &slot.val {
                LuaValue::Float(f) => Some(*f),
                _ => None,
            },
            None => None,
        }
    }
    /// Hot-path accessor: pair version of `get_num_at` — returns `Some((a,b))`
    /// when both slots coerce to `f64` (Float or Int), `None` if either does
    /// not. Used by the float fast path of the arith opcodes.
    ///
    #[inline(always)]
    pub fn get_num_pair_at(
        &self,
        rb: impl Into<StackIdxConv>,
        rc: impl Into<StackIdxConv>,
    ) -> Option<(f64, f64)> {
        let rb: StackIdx = rb.into().0;
        let rc: StackIdx = rc.into().0;
        match (self.stack[rb.0 as usize].val, self.stack[rc.0 as usize].val) {
            (LuaValue::Float(nb), LuaValue::Float(nc)) => Some((nb, nc)),
            (LuaValue::Int(ib), LuaValue::Int(ic)) => Some((ib as f64, ic as f64)),
            (LuaValue::Int(ib), LuaValue::Float(nc)) => Some((ib as f64, nc)),
            (LuaValue::Float(nb), LuaValue::Int(ic)) => Some((nb, ic as f64)),
            _ => None,
        }
    }
    /// Set `top` to an absolute stack index. Grows the backing stack vector
    /// (filling new slots with `Nil`) when `idx` is past `stack.len()`, but
    /// never clobbers existing slots between the old top and the new top —
    /// VM opcodes (Call, ForPrep, etc.) write registers via `set_at` and then
    /// raise `top` to signal "these are now live"; nil-filling here would
    /// erase the just-written values.
    ///
    /// The `setnilvalue(s2v(L->top.p++))` clear loop in `lua_settop` (lapi.c)
    /// is part of the public API path and lives in `api::set_top` instead.
    /// Callers here pass an absolute `StackIdx`, not the relative `idx` of
    /// the public `lua_settop`. The to-be-closed (`tbclist`) close path
    /// lives in `func.rs`, not here.
    #[inline(always)]
    pub fn set_top(&mut self, idx: impl Into<StackIdxConv>) {
        let new_top: StackIdx = idx.into().0;
        let new_top_u = new_top.0 as usize;
        if new_top_u > self.stack.len() {
            self.stack.resize_with(new_top_u, StackValue::default);
        }
        self.top = new_top;
    }
    /// Primitive "set top index" — just writes `self.top`, no nil-fill.
    ///
    /// Callers (`api.rs::set_top`, `raw_set`, etc.) pre-nil-fill or only
    /// shrink, so this routine intentionally does no clearing or resizing.
    /// The to-be-closed (`tbclist`) close path lives in `func.rs`, not here.
    #[inline(always)]
    pub fn set_top_idx(&mut self, idx: impl Into<StackIdxConv>) {
        let new_top: StackIdx = idx.into().0;
        self.top = new_top;
    }
    /// Decrement `top` by 1 (saturating at zero).
    ///
    #[inline(always)]
    pub fn dec_top(&mut self) {
        if self.top.0 > 0 {
            self.top = StackIdx(self.top.0 - 1);
        }
    }
    #[inline(always)]
    pub fn pop_n(&mut self, n: usize) {
        let cur = self.top.0 as usize;
        let new = cur.saturating_sub(n);
        self.top = StackIdx(new as u32);
    }
    /// Returns the value at the given stack index without removing it.
    ///
    #[inline(always)]
    pub fn peek_at(&mut self, idx: impl Into<StackIdxConv>) -> LuaValue {
        let i: StackIdx = idx.into().0;
        match self.stack.get(i.0 as usize) {
            Some(slot) => slot.val.clone(),
            None => LuaValue::Nil,
        }
    }
    /// Returns the value just below `top` (the topmost live slot) without
    /// removing it.
    ///
    #[inline(always)]
    pub fn peek_top(&mut self) -> LuaValue {
        if self.top.0 == 0 {
            return LuaValue::Nil;
        }
        self.stack[(self.top.0 - 1) as usize].val.clone()
    }
    /// Returns the topmost slot interpreted as a string. Panics if the slot
    /// is not a `LuaValue::Str`. Callers (e.g. `luaO_pushvfstring`) guarantee
    /// the value has been pushed as an interned string immediately prior.
    ///
    pub fn peek_string_at_top(&mut self) -> GcRef<LuaString> {
        match self.peek_top() {
            LuaValue::Str(s) => s,
            _ => panic!("peek_string_at_top: top of stack is not a string"),
        }
    }
    /// Mutable reference to the value at the given stack slot.
    ///
    pub fn stack_at(&mut self, idx: impl Into<StackIdxConv>) -> &mut LuaValue {
        let i: StackIdx = idx.into().0;
        &mut self.stack[i.0 as usize].val
    }
    /// Writes `Nil` to the given stack slot.
    ///
    pub fn stack_set_nil(&mut self, idx: impl Into<StackIdxConv>) {
        let i: StackIdx = idx.into().0;
        let slot = i.0 as usize;
        if slot < self.stack.len() {
            self.stack[slot].val = LuaValue::Nil;
        }
    }
    /// Resizes the underlying stack vector to `size` slots, padding new slots
    /// with `StackValue::default()` (which is `Nil`). Returns `Ok(())` on
    /// success — `Vec::resize_with` in Rust does not have a fallible path the
    /// way `luaM_reallocvector` does in C, so the `Result` is here for
    /// signature parity with future fallible allocators.
    ///
    /// newsize+EXTRA_STACK, StackValue)`.
    pub fn stack_resize(&mut self, size: usize) -> Result<(), LuaError> {
        self.stack.resize_with(size, StackValue::default);
        Ok(())
    }
    pub fn stack_available(&mut self) -> usize {
        (self.stack_last.0 as usize).saturating_sub(self.top.0 as usize)
    }
    pub fn check_stack(&mut self, n: i32) -> Result<(), LuaError> {
        let free = (self.stack_last.0 as i32) - (self.top.0 as i32);
        if free <= n {
            self.grow_stack(n, true)?;
        }
        Ok(())
    }
    /// Inherent method wrapper around the free function `do_::grow_stack`,
    /// preserving the historical `Result<(), LuaError>` signature used by
    /// `check_stack` and other VM call sites. The bool returned by the
    /// underlying implementation distinguishes soft failure (when
    /// `raise_error` is false) from success; that distinction is dropped here
    /// because every current caller passes `raise_error = true` and only
    /// cares about error propagation.
    ///
    pub fn grow_stack(&mut self, n: i32, raise_error: bool) -> Result<(), LuaError> {
        crate::do_::grow_stack(self, n, raise_error).map(|_| ())
    }

    #[inline(always)]
    pub fn get_ci(&self, idx: CallInfoIdx) -> &CallInfo {
        &self.call_info[idx.as_usize()]
    }
    #[inline(always)]
    pub fn get_ci_mut(&mut self, idx: CallInfoIdx) -> &mut CallInfo {
        &mut self.call_info[idx.as_usize()]
    }
    #[inline(always)]
    pub fn current_call_info(&self) -> &CallInfo {
        &self.call_info[self.ci.as_usize()]
    }
    #[inline(always)]
    pub fn current_call_info_mut(&mut self) -> &mut CallInfo {
        let i = self.ci.as_usize();
        &mut self.call_info[i]
    }
    #[inline(always)]
    pub fn current_ci_idx(&self) -> CallInfoIdx {
        self.ci
    }
    pub fn call_stack_mut(&mut self) -> &mut Vec<CallInfo> {
        &mut self.call_info
    }
    #[inline(always)]
    pub fn next_ci(&mut self) -> Result<CallInfoIdx, LuaError> {
        let idx = match self.call_info[self.ci.as_usize()].next {
            Some(idx) => idx,
            None => extend_ci(self),
        };
        self.call_info[idx.as_usize()].tailcalls = 0;
        Ok(idx)
    }

    /// Records that a Lua tail call reused the frame at `ci`, so the 5.1
    /// `debug.getstack` level walk can synthesize the `(tail call)` frames the
    /// 5.1 debug model exposes. Gated to 5.1: 5.2+ dropped synthetic tail frames
    /// for the `istailcall` flag and never read `tailcalls`, so on those versions
    /// this is a single version compare and return — the hot tail-call path is
    /// otherwise byte-identical.
    #[inline(always)]
    pub fn note_lua_tailcall(&mut self, ci: CallInfoIdx) {
        if self.global().lua_version == lua_types::LuaVersion::V51 {
            self.call_info[ci.as_usize()].tailcalls =
                self.call_info[ci.as_usize()].tailcalls.saturating_add(1);
        }
    }
    #[inline(always)]
    pub fn prev_ci(&self, idx: CallInfoIdx) -> Option<CallInfoIdx> {
        self.call_info[idx.as_usize()].previous
    }
    pub fn get_prev_ci(&self, idx: CallInfoIdx) -> Option<&CallInfo> {
        self.call_info[idx.as_usize()]
            .previous
            .map(|p| &self.call_info[p.as_usize()])
    }
    #[inline(always)]
    pub fn is_base_ci(&self, idx: CallInfoIdx) -> bool {
        idx.as_usize() == 0
    }
    #[inline(always)]
    pub fn is_current_ci(&self, idx: CallInfoIdx) -> bool {
        idx == self.ci
    }
    pub fn ci_next_func(&self, idx: CallInfoIdx) -> StackIdx {
        let next = self.call_info[idx.as_usize()]
            .next
            .expect("ci_next_func: no next CallInfo");
        self.call_info[next.as_usize()].func
    }
    #[inline(always)]
    pub fn ci_top(&self, idx: CallInfoIdx) -> StackIdx {
        self.call_info[idx.as_usize()].top
    }
    /// Hot-loop trap read: `(callstatus & CIST_TRAP) != 0`, one mask with no
    /// enum-discriminant branch. A C frame never has `CIST_TRAP` set, so this
    /// returns `false` for C frames identically to the pre-flatten fallback,
    /// without needing a frame-kind branch — that is the whole point of T2-C2.
    #[inline(always)]
    pub fn ci_trap(&mut self, idx: CallInfoIdx) -> bool {
        self.call_info[idx.as_usize()].trap()
    }
    #[inline(always)]
    pub fn ci_savedpc(&self, idx: CallInfoIdx) -> u32 {
        self.call_info[idx.as_usize()].saved_pc()
    }
    #[inline(always)]
    pub fn set_ci_savedpc(&mut self, idx: CallInfoIdx, pc: u32) {
        self.call_info[idx.as_usize()].set_saved_pc(pc);
    }
    #[inline(always)]
    pub fn set_ci_previous(&mut self, idx: CallInfoIdx) {
        self.ci = self.call_info[idx.as_usize()]
            .previous
            .expect("set_ci_previous: returning frame has no previous CallInfo");
    }
    #[inline(always)]
    pub fn ci_previous(&self, idx: CallInfoIdx) -> Option<CallInfoIdx> {
        self.call_info[idx.as_usize()].previous
    }
    #[inline(always)]
    pub fn ci_adjust_func(&mut self, idx: CallInfoIdx, delta: i32) {
        let ci = &mut self.call_info[idx.as_usize()];
        ci.func = StackIdx((ci.func.0 as i32 - delta) as u32);
    }
    #[inline(always)]
    pub fn ci_base(&self, idx: CallInfoIdx) -> StackIdx {
        self.call_info[idx.as_usize()].func + 1
    }
    #[inline(always)]
    pub fn ci_is_fresh(&self, idx: CallInfoIdx) -> bool {
        (self.call_info[idx.as_usize()].callstatus & CIST_FRESH) != 0
    }
    #[inline(always)]
    pub fn ci_lua_closure(
        &self,
        idx: CallInfoIdx,
    ) -> Option<GcRef<lua_types::closure::LuaLClosure>> {
        let func_idx = self.call_info[idx.as_usize()].func;
        match self.stack.get(func_idx.0 as usize).map(|slot| slot.val) {
            Some(LuaValue::Function(lua_types::closure::LuaClosure::Lua(cl))) => Some(cl),
            _ => None,
        }
    }
    #[inline(always)]
    pub fn ci_nextraargs(&self, idx: CallInfoIdx) -> i32 {
        self.call_info[idx.as_usize()].nextra_args()
    }
    #[inline(always)]
    pub fn ci_nres(&self, idx: CallInfoIdx) -> i32 {
        self.call_info[idx.as_usize()].u2.value
    }
    #[inline(always)]
    pub fn ci_nres_set(&mut self, idx: CallInfoIdx, n: i32) {
        self.call_info[idx.as_usize()].u2.value = n;
    }
    #[inline(always)]
    pub fn ci_nresults(&self, idx: CallInfoIdx) -> i32 {
        self.call_info[idx.as_usize()].nresults as i32
    }
    pub fn ci_prev_instruction(&self, idx: CallInfoIdx) -> lua_types::opcode::Instruction {
        let pc = self.call_info[idx.as_usize()].saved_pc();
        let cl = self
            .ci_lua_closure(idx)
            .expect("ci_prev_instruction: CallInfo does not hold a Lua closure");
        cl.proto.code[(pc - 1) as usize]
    }
    pub fn ci_prev2_instruction(&self, idx: CallInfoIdx) -> lua_types::opcode::Instruction {
        let pc = self.call_info[idx.as_usize()].saved_pc();
        let cl = self
            .ci_lua_closure(idx)
            .expect("ci_prev2_instruction: CallInfo does not hold a Lua closure");
        cl.proto.code[(pc - 2) as usize]
    }
    pub fn ci_skip_next_instruction(&mut self, idx: CallInfoIdx) {
        let pc = self.call_info[idx.as_usize()].saved_pc();
        self.call_info[idx.as_usize()].set_saved_pc(pc + 1);
    }
    pub fn ci_step_pc_back(&mut self, idx: CallInfoIdx) {
        let pc = self.call_info[idx.as_usize()].saved_pc();
        self.call_info[idx.as_usize()].set_saved_pc(pc - 1);
    }
    pub fn get_ci_pcrel(&mut self, idx: CallInfoIdx) -> u32 {
        self.call_info[idx.as_usize()].saved_pc().saturating_sub(1)
    }
    pub fn get_ci_u2_funcidx(&mut self, idx: CallInfoIdx) -> i32 {
        self.call_info[idx.as_usize()].u2.value
    }
    pub fn get_ci_u2_nres(&mut self, idx: CallInfoIdx) -> i32 {
        self.call_info[idx.as_usize()].u2.value
    }
    pub fn get_ci_u2_nyield(&mut self, idx: CallInfoIdx) -> i32 {
        self.call_info[idx.as_usize()].u2.value
    }
    pub fn get_ci_vararg_info(&mut self, idx: CallInfoIdx) -> (bool, i32, i32) {
        let nextraargs = self.call_info[idx.as_usize()].nextra_args();
        match self.ci_lua_closure(idx) {
            Some(cl) => (cl.proto.is_vararg, nextraargs, cl.proto.numparams as i32),
            None => (false, nextraargs, 0),
        }
    }
    pub fn get_ci_lua_proto_numparams(&mut self, idx: CallInfoIdx) -> u8 {
        self.ci_lua_closure(idx)
            .map(|cl| cl.proto.numparams)
            .unwrap_or(0)
    }
    pub fn set_ci_u2_nres(&mut self, idx: CallInfoIdx, n: i32) {
        self.call_info[idx.as_usize()].u2.value = n;
    }
    pub fn set_ci_u2_nyield(&mut self, idx: CallInfoIdx, n: i32) {
        self.call_info[idx.as_usize()].u2.value = n;
    }
    pub fn set_ci_transfer_info(&mut self, idx: CallInfoIdx, ftransfer: u16, ntransfer: u16) {
        let ci = &mut self.call_info[idx.as_usize()];
        ci.u2.ftransfer = ftransfer;
        ci.u2.ntransfer = ntransfer;
    }
    pub fn shrink_ci(&mut self) {
        shrink_ci(self)
    }
    pub fn check_c_stack(&mut self) -> Result<(), LuaError> {
        check_c_stack(self)
    }

    pub fn status(&mut self) -> LuaStatus {
        LuaStatus::from_raw(self.status as i32)
    }
    pub fn errfunc(&mut self) -> isize {
        self.errfunc
    }
    pub fn old_pc(&mut self) -> u32 {
        self.oldpc
    }
    pub fn set_old_pc(&mut self, pc: u32) {
        self.oldpc = pc;
    }
    pub fn set_oldpc(&mut self, pc: u32) {
        self.oldpc = pc;
    }
    pub fn _hook_call_noargs(&mut self) {}
    pub fn hook(&self) -> Option<&Box<dyn FnMut(&mut LuaState, &crate::debug::LuaDebug)>> {
        self.hook.as_ref()
    }
    pub fn has_hook(&mut self) -> bool {
        self.hook.is_some()
    }
    pub fn hook_count(&mut self) -> i32 {
        self.hookcount
    }
    pub fn set_hook_count(&mut self, n: i32) {
        self.hookcount = n;
    }
    pub fn hook_mask(&self) -> u8 {
        self.hookmask
    }
    pub fn set_hook_mask(&mut self, m: u8) {
        self.hookmask = m;
    }
    pub fn base_hook_count(&self) -> i32 {
        self.basehookcount
    }
    pub fn set_base_hook_count(&mut self, n: i32) {
        self.basehookcount = n;
    }
    pub fn set_hook(&mut self, h: Option<Box<dyn FnMut(&mut LuaState, &crate::debug::LuaDebug)>>) {
        self.hook = h;
    }
    /// Fire a count or line hook on the currently executing frame.
    ///
    /// C-Lua's `luaG_traceexec` is the sole caller of `luaD_hook` for count and
    /// line events, and it relies on `L->ci` being the same Lua frame after the
    /// hook as before it: the bytecode dispatch loop in `luaV_execute` reads
    /// `L->ci` (via `trace_exec`) on the next instruction and writes its
    /// `savedpc`, which is only valid on a Lua frame. C maintains that invariant
    /// implicitly — the unprotected `lua_call` inside the hook restores `L->ci`
    /// via `luaD_poscall` on success, and on a hook error it `longjmp`s straight
    /// to the nearest protected boundary, which resets `L->ci`, so a corrupted
    /// `ci` never re-enters the dispatch loop.
    ///
    /// Our port reports hook-callback errors through a different path that can
    /// leave `self.ci` advanced to the hook's own (C) frame instead of unwinding
    /// it, so the invariant the dispatch loop depends on would be broken. Saving
    /// `self.ci` here and restoring it after the hook reasserts exactly the
    /// invariant C guarantees, so the next `trace_exec` writes `savedpc` on the
    /// Lua frame it is actually executing rather than panicking on a C frame.
    pub fn call_hook_event(&mut self, event: i32, line: i32) -> Result<(), LuaError> {
        let saved_ci = self.ci;
        let r = crate::do_::hook(self, event, line, 0, 0);
        self.ci = saved_ci;
        r
    }

    pub fn registry_value(&self) -> LuaValue {
        self.global().l_registry.clone()
    }
    pub fn registry_get(&self, key: usize) -> LuaValue {
        let reg = self.global().l_registry.clone();
        match reg {
            LuaValue::Table(t) => t.get(&LuaValue::Int(key as i64)),
            _ => LuaValue::Nil,
        }
    }

    pub fn new_string(&mut self, bytes: &[u8]) -> Result<GcRef<LuaString>, LuaError> {
        self.intern_or_create_str(bytes)
    }

    // ── State-owned allocation API ──────────────────────────────────────────
    // These methods are the canonical allocation surface. They wrap
    // `GcRef::new`, which allocates on the currently active heap. Callers
    // must reach them through `&mut LuaState`, which mirrors C-Lua's
    // requirement that every allocation passes `lua_State *L`.

    /// Allocate a new Lua function prototype.
    ///
    /// The caller is expected to populate fields on the returned value while
    /// it has no other outstanding references (immediately after allocation).
    pub fn new_proto(&mut self) -> GcRef<LuaProto> {
        self.mark_gc_check_needed();
        GcRef::new(LuaProto::placeholder())
    }

    /// Allocate a Lua-side closure (compiled function + upvalue slots).
    pub fn new_lclosure(&mut self, proto: GcRef<LuaProto>, nupvals: usize) -> GcRef<LuaClosureLua> {
        self.mark_gc_check_needed();
        let mut upvals = Vec::with_capacity(nupvals);
        for _ in 0..nupvals {
            upvals.push(std::cell::Cell::new(self.new_upval_closed(LuaValue::Nil)));
        }
        let upvals = upvals.into_boxed_slice();
        let closure = GcRef::new(LuaClosureLua { proto, upvals });
        closure.account_buffer(closure.buffer_bytes() as isize);
        closure
    }

    /// Allocate a closed upvalue holding the given value.
    pub fn new_upval_closed(&mut self, v: LuaValue) -> GcRef<UpVal> {
        self.mark_gc_check_needed();
        GcRef::new(UpVal::closed(v))
    }

    /// Allocate an open upvalue referring to a thread's stack slot.
    pub fn new_upval_open(&mut self, thread_id: u64, level: StackIdx) -> GcRef<UpVal> {
        self.mark_gc_check_needed();
        self.legacy_open_upval_touched.set(true);
        GcRef::new(UpVal::open(thread_id, level))
    }
    /// Mirrors `luaS_newlstr`: short strings are interned globally so equal
    /// content shares a single TString; long strings (> LUAI_MAXSHORTLEN = 40)
    /// always create a fresh TString without interning. This is what lets
    /// `string.format("%p", "long" .. "concat")` differ from a same-content
    /// literal — concat must produce a new object even when the literal already
    /// lives in the lexer's constant pool.
    pub fn intern_or_create_str(&mut self, bytes: &[u8]) -> Result<GcRef<LuaString>, LuaError> {
        self.intern_str(bytes)
    }
    /// Allocates a full userdata with `size` payload bytes and `nuvalue` nil
    /// user-value slots, returning the `GcRef` without pushing it. Mirrors C's
    /// `lua_newuserdatauv` allocation step; the untyped counterpart of
    /// [`Self::new_userdata_typed`] (no `__name`-carrying metatable is
    /// attached). Callers such as `api::new_userdata_uv` push the result and run
    /// the GC step.
    pub fn new_userdata(
        &mut self,
        size: usize,
        nuvalue: usize,
    ) -> Result<GcRef<LuaUserData>, LuaError> {
        debug_assert!(nuvalue < u16::MAX as usize, "invalid value");
        self.mark_gc_check_needed();
        let u = GcRef::new(LuaUserData {
            data: vec![0u8; size].into_boxed_slice(),
            uv: std::cell::RefCell::new(vec![LuaValue::Nil; nuvalue]),
            metatable: std::cell::RefCell::new(None),
            host_value: std::cell::RefCell::new(None),
        });
        u.account_buffer(u.buffer_bytes() as isize);
        Ok(u)
    }

    /// Registers a C function pointer `f` in the per-state `c_functions` table
    /// and returns its `LuaCFnPtr` index. `LuaClosure::LightC`/`LuaCClosure`
    /// carry this index rather than a raw pointer because `lua_types` cannot
    /// reference `LuaState`. When `dedup` is set (upvalue-less light C
    /// functions) an already-registered identical `f` is reused; otherwise a
    /// fresh slot is always allocated, since closures with upvalues must not
    /// share identity.
    pub fn register_c_function(&mut self, f: LuaCFunction, dedup: bool) -> LuaCFnPtr {
        let mut g = self.global_mut();
        if dedup {
            if let Some(i) = g.c_functions.iter().position(|existing| {
                existing
                    .as_bare()
                    .is_some_and(|existing| std::ptr::fn_addr_eq(existing, f))
            }) {
                return i;
            }
        }
        let i = g.c_functions.len();
        g.c_functions.push(LuaCallable::bare(f));
        i
    }

    /// Creates a C closure over `f` with `n` (nil-initialised) upvalue slots and
    /// returns it without pushing. Mirrors `lua_pushcclosure`'s object step:
    /// `n == 0` yields a light C function, `n > 0` a full C closure. The stack
    /// filling of upvalues is the caller's responsibility (see
    /// `api::push_cclosure`).
    ///
    /// `n` is validated against `0..=MAX_UPVAL` before anything is registered,
    /// so an out-of-range count errors without polluting the `c_functions`
    /// table.
    pub fn new_c_closure(&mut self, f: LuaCFunction, n: i32) -> Result<LuaClosure, LuaError> {
        if n < 0 || n as i64 > crate::api::MAX_UPVAL as i64 {
            return Err(LuaError::runtime(format_args!(
                "C closure upvalue count {n} out of range (0..={})",
                crate::api::MAX_UPVAL
            )));
        }
        let idx = self.register_c_function(f, n == 0);
        if n == 0 {
            return Ok(LuaClosure::LightC(idx));
        }
        self.mark_gc_check_needed();
        let cl = LuaClosure::C(GcRef::new(lua_types::closure::LuaCClosure {
            func: idx,
            upvalues: std::cell::RefCell::new(vec![LuaValue::Nil; n as usize]),
        }));
        if let LuaClosure::C(ccl) = &cl {
            ccl.account_buffer(ccl.buffer_bytes() as isize);
        }
        Ok(cl)
    }
    pub fn push_closure(
        &mut self,
        proto_idx: usize,
        ci: CallInfoIdx,
        base: StackIdx,
        ra: StackIdx,
    ) -> Result<(), LuaError> {
        let parent_cl = self
            .ci_lua_closure(ci)
            .expect("push_closure: current frame is not a Lua closure");
        let child_proto = parent_cl.proto.p[proto_idx].clone();
        let nup = child_proto.upvalues.len();
        let mut upvals: Vec<std::cell::Cell<GcRef<UpVal>>> = Vec::with_capacity(nup);
        for i in 0..nup {
            let desc = &child_proto.upvalues[i];
            let uv = if desc.instack {
                let level = base + desc.idx as i32;
                crate::func::find_upval(self, level)
            } else {
                parent_cl.upval(desc.idx as usize)
            };
            upvals.push(std::cell::Cell::new(uv));
        }
        // LUA_COMPAT closure caching (5.2/5.3 only): if the last closure built
        // from this proto captured the identical upvalues, reuse it so the two
        // compare `==` (C's `getcached`). 5.1 never cached; 5.4/5.5 removed it.
        let cache_enabled = matches!(
            self.global().lua_version,
            lua_types::LuaVersion::V52 | lua_types::LuaVersion::V53
        );
        if cache_enabled {
            if let Some(cached) = child_proto.cache.borrow().as_ref() {
                if cached.upvals.len() == nup
                    && (0..nup).all(|i| GcRef::ptr_eq(&cached.upvals[i].get(), &upvals[i].get()))
                {
                    let reused = cached.clone();
                    self.set_at(ra, LuaValue::Function(LuaClosure::Lua(reused)));
                    return Ok(());
                }
            }
        }
        // Upvals are pre-populated from the parent frame here, so this builds
        // the closure directly rather than through `state.new_lclosure`,
        // which would fill fresh Nil upvals and drop the captured bindings.
        self.mark_gc_check_needed();
        let new_cl = GcRef::new(LuaClosureLua {
            proto: child_proto.clone(),
            upvals: upvals.into_boxed_slice(),
        });
        new_cl.account_buffer(new_cl.buffer_bytes() as isize);
        if cache_enabled {
            *child_proto.cache.borrow_mut() = Some(new_cl.clone());
        }
        self.set_at(ra, LuaValue::Function(LuaClosure::Lua(new_cl)));
        Ok(())
    }
    pub fn new_tbc_upval(&mut self, idx: StackIdx) -> Result<(), LuaError> {
        crate::func::new_tbc_upval(self, idx)
    }

    /// Read an open or closed upvalue.
    ///
    /// Closed upvalues own their value and read trivially. Open upvalues
    /// point at a stack slot on the home thread that captured them.
    ///
    /// Resolution order for an open upvalue whose home is not the current
    /// thread:
    ///
    /// 1. If the home thread is registered in `GlobalState::threads` and
    ///    its `RefCell` is currently borrowable, read straight from its
    ///    stack. This is the path used when the main thread reads a
    ///    closure created inside a now-suspended coroutine, or when one
    ///    coroutine reads an upvalue homed on a sibling suspended
    ///    coroutine.
    /// 2. Otherwise fall back to `GlobalState::cross_thread_upvals`. This
    ///    is the path used while inside a `coroutine.resume`: the parent
    ///    thread's `LuaState` is held by an outer `&mut` and is not
    ///    reachable through any `Rc<RefCell<_>>`, so `aux_resume`
    ///    snapshots the parent's open upvalues into the mirror across the
    ///    resume boundary.
    #[inline(always)]
    pub fn upvalue_get(&self, cl: &GcRef<LuaClosureLua>, n: usize) -> LuaValue {
        let uv = cl.upval(n);
        let (thread_id, idx) = match uv.try_open_payload() {
            Some(p) => p,
            None => return uv.closed_value(),
        };
        let current = self.cached_thread_id;
        if thread_id == current {
            return self.stack[idx.0 as usize].val;
        }
        self.upvalue_get_cross_thread(thread_id, idx)
    }

    #[cold]
    #[inline(never)]
    fn upvalue_get_cross_thread(&self, tid: u64, idx: StackIdx) -> LuaValue {
        let entry_rc = {
            let g = self.global();
            g.threads.get(&tid).map(|e| e.state.clone())
        };
        if let Some(rc) = entry_rc {
            if let Ok(home_state) = rc.try_borrow() {
                return home_state.get_at(idx);
            }
        }
        let g = self.global();
        match g.cross_thread_upvals.get(&(tid, idx)) {
            Some(v) => *v,
            None => LuaValue::Nil,
        }
    }
    /// Write an open or closed upvalue.
    ///
    /// Mirrors [`upvalue_get`]: open upvalues homed on the current thread
    /// write through `self.stack`. For cross-thread open upvalues, the
    /// home thread's stack is written directly when its `RefCell` is
    /// borrowable, otherwise the write lands in
    /// `GlobalState::cross_thread_upvals` (the active-resume case where
    /// the home thread is borrow-locked further up the call stack).
    #[inline(always)]
    pub fn upvalue_set(
        &mut self,
        cl: &GcRef<LuaClosureLua>,
        n: usize,
        val: LuaValue,
    ) -> Result<(), LuaError> {
        let uv = cl.upval(n);
        match uv.try_open_payload() {
            Some((thread_id, idx)) => {
                let current = self.cached_thread_id;
                if thread_id == current {
                    self.stack[idx.0 as usize].val = val;
                } else {
                    self.upvalue_set_cross_thread(thread_id, idx, val)?;
                }
            }
            None => {
                uv.set_closed_value(val);
            }
        }
        if val.is_collectable() {
            self.gc_barrier_upval(&uv, &val);
        }
        Ok(())
    }

    #[cold]
    #[inline(never)]
    fn upvalue_set_cross_thread(
        &mut self,
        tid: u64,
        idx: StackIdx,
        val: LuaValue,
    ) -> Result<(), LuaError> {
        let entry_rc = {
            let g = self.global();
            g.threads.get(&tid).map(|e| e.state.clone())
        };
        if let Some(rc) = entry_rc {
            if let Ok(mut home_state) = rc.try_borrow_mut() {
                home_state.set_at(idx, val);
                return Ok(());
            }
        }
        let mut g = self.global_mut();
        g.cross_thread_upvals.insert((tid, idx), val);
        Ok(())
    }

    pub fn protected_call_raw(
        &mut self,
        func: StackIdx,
        nresults: i32,
        errfunc: StackIdx,
    ) -> Result<(), LuaError> {
        let ef = errfunc.0 as isize;
        let status = crate::do_::pcall(self, |s| s.call_no_yield(func, nresults), func, ef);
        match status {
            LuaStatus::Ok => Ok(()),
            LuaStatus::ErrSyntax => {
                let err_val = self.get_at(func);
                self.set_top(func);
                Err(LuaError::Syntax(err_val))
            }
            LuaStatus::Yield => {
                self.set_top(func);
                Err(LuaError::Yield)
            }
            _ => {
                let err_val = self.get_at(func);
                self.set_top(func);
                Err(LuaError::Runtime(err_val))
            }
        }
    }
    pub fn protected_parser(
        &mut self,
        z: crate::zio::ZIO,
        name: &[u8],
        mode: Option<&[u8]>,
    ) -> LuaStatus {
        crate::do_::protected_parser(self, z, name, mode)
    }
    pub fn do_call(&mut self, func: StackIdx, nresults: i32) -> Result<(), LuaError> {
        crate::do_::call(self, func, nresults)
    }
    pub fn do_call_no_yield(&mut self, func: StackIdx, nresults: i32) -> Result<(), LuaError> {
        crate::do_::callnoyield(self, func, nresults)
    }
    pub fn call_no_yield(&mut self, func: StackIdx, nresults: i32) -> Result<(), LuaError> {
        crate::do_::callnoyield(self, func, nresults)
    }
    pub fn call_at(&mut self, func: StackIdx, nresults: i32) -> Result<(), LuaError> {
        crate::do_::call(self, func, nresults)
    }
    #[inline(always)]
    pub fn call_known_c_at(&mut self, func: StackIdx, nresults: i32) -> Result<bool, LuaError> {
        crate::do_::call_known_c(self, func, nresults)
    }
    #[inline(always)]
    pub fn precall(
        &mut self,
        func: StackIdx,
        nresults: i32,
    ) -> Result<Option<CallInfoIdx>, LuaError> {
        crate::do_::precall(self, func, nresults)
    }
    #[inline(always)]
    pub fn pretailcall(
        &mut self,
        ci: CallInfoIdx,
        func: StackIdx,
        narg1: i32,
        delta: i32,
    ) -> Result<i32, LuaError> {
        crate::do_::pretailcall(self, ci, func, narg1, delta)
    }
    #[inline(always)]
    pub fn poscall<N: TryInto<i32>>(&mut self, ci: CallInfoIdx, nres: N) -> Result<(), LuaError>
    where
        <N as TryInto<i32>>::Error: std::fmt::Debug,
    {
        let n = nres.try_into().expect("poscall: nres out of i32 range");
        crate::do_::poscall(self, ci, n)
    }
    pub fn adjust_results(&mut self, nresults: i32) {
        const LUA_MULTRET: i32 = -1;
        if nresults <= LUA_MULTRET {
            let ci_idx = self.ci.as_usize();
            if self.call_info[ci_idx].top.0 < self.top.0 {
                self.call_info[ci_idx].top = self.top;
            }
        }
    }
    pub fn adjust_varargs(
        &mut self,
        ci: CallInfoIdx,
        nfixparams: i32,
        cl: &GcRef<lua_types::closure::LuaLClosure>,
    ) -> Result<(), LuaError> {
        crate::tagmethods::adjust_varargs(self, nfixparams, ci, &cl.0.proto)
    }
    pub fn get_varargs(&mut self, ci: CallInfoIdx, ra: StackIdx, n: i32) -> Result<i32, LuaError> {
        crate::tagmethods::get_varargs(self, ci, ra, n)?;
        Ok(0)
    }

    pub fn close_upvals(&mut self, level: StackIdx) -> Result<(), LuaError> {
        crate::func::close_upval(self, level);
        Ok(())
    }
    pub fn close_upvals_status(&mut self, level: StackIdx, _status: i32) -> Result<(), LuaError> {
        crate::func::close_upval(self, level);
        Ok(())
    }
    pub fn close_upvals_from_base(&mut self, ci: CallInfoIdx) -> Result<(), LuaError> {
        let base = self.ci_base(ci);
        crate::func::close_upval(self, base);
        Ok(())
    }

    pub fn arith_op(
        &mut self,
        op: i32,
        p1: &LuaValue,
        p2: &LuaValue,
    ) -> Result<LuaValue, LuaError> {
        let arith_op = match op {
            0 => lua_types::arith::ArithOp::Add,
            1 => lua_types::arith::ArithOp::Sub,
            2 => lua_types::arith::ArithOp::Mul,
            3 => lua_types::arith::ArithOp::Mod,
            4 => lua_types::arith::ArithOp::Pow,
            5 => lua_types::arith::ArithOp::Div,
            6 => lua_types::arith::ArithOp::Idiv,
            7 => lua_types::arith::ArithOp::Band,
            8 => lua_types::arith::ArithOp::Bor,
            9 => lua_types::arith::ArithOp::Bxor,
            10 => lua_types::arith::ArithOp::Shl,
            11 => lua_types::arith::ArithOp::Shr,
            12 => lua_types::arith::ArithOp::Unm,
            13 => lua_types::arith::ArithOp::Bnot,
            _ => return Err(LuaError::runtime(format_args!("invalid arith op {}", op))),
        };
        let mut res = LuaValue::Nil;
        if crate::object::raw_arith(self, arith_op, p1, p2, &mut res)? {
            Ok(res)
        } else {
            Err(LuaError::arith_error(p1, p2, "perform arithmetic on"))
        }
    }
    pub fn concat(&mut self, n: i32) -> Result<(), LuaError> {
        crate::vm::concat(self, n)
    }
    pub fn less_than(&mut self, l: &LuaValue, r: &LuaValue) -> Result<bool, LuaError> {
        crate::vm::less_than(self, l, r)
    }
    pub fn less_equal(&mut self, l: &LuaValue, r: &LuaValue) -> Result<bool, LuaError> {
        crate::vm::less_equal(self, l, r)
    }
    pub fn equal_obj(&self, _ctx: Option<&LuaValue>, l: &LuaValue, r: &LuaValue) -> bool {
        crate::vm::equal_obj(None, l, r).unwrap_or(false)
    }
    pub fn equal_obj_with_tm(&mut self, l: &LuaValue, r: &LuaValue) -> Result<bool, LuaError> {
        crate::vm::equal_obj(Some(self), l, r)
    }
    pub fn obj_len(&mut self, v: &LuaValue) -> Result<LuaValue, LuaError> {
        match v {
            LuaValue::Table(_) => {
                // Lua 5.1 `#t` ignores a table `__len` metamethod (table
                // `__len` is 5.2+); always use the primitive length under V51.
                let consult_len_tm =
                    !matches!(self.global().lua_version, lua_types::LuaVersion::V51);
                let tm = if consult_len_tm {
                    let mt = self.table_metatable(v);
                    self.fast_tm_table(mt.as_ref(), TagMethod::Len)
                } else {
                    LuaValue::Nil
                };
                if matches!(tm, LuaValue::Nil) {
                    let n = self.table_length(v)?;
                    return Ok(LuaValue::Int(n));
                }
                self.push(LuaValue::Nil);
                let slot = StackIdx(self.top.0 - 1);
                crate::tagmethods::call_tm_res(self, tm, v.clone(), v.clone(), slot)?;
                Ok(self.pop())
            }
            LuaValue::Str(s) => Ok(LuaValue::Int(s.len() as i64)),
            other => {
                let tm = crate::tagmethods::get_tm_by_obj(
                    self,
                    other,
                    crate::tagmethods::TagMethod::Len,
                );
                if matches!(tm, LuaValue::Nil) {
                    let mut msg = b"attempt to get length of a ".to_vec();
                    msg.extend_from_slice(&self.obj_type_name(other));
                    msg.extend_from_slice(b" value");
                    return Err(crate::debug::prefixed_runtime_pub(self, msg));
                }
                self.push(LuaValue::Nil);
                let slot = StackIdx(self.top.0 - 1);
                crate::tagmethods::call_tm_res(self, tm, v.clone(), v.clone(), slot)?;
                Ok(self.pop())
            }
        }
    }
    pub fn obj_to_string(&mut self, idx: i32) -> Result<GcRef<LuaString>, LuaError> {
        let slot: StackIdx = if idx > 0 {
            let ci_func = self.current_call_info().func;
            ci_func + idx
        } else {
            debug_assert!(idx != 0, "invalid index");
            StackIdx((self.top_idx().0 as i32 + idx) as u32)
        };
        let val = self.get_at(slot);
        match val {
            LuaValue::Str(s) => Ok(s),
            LuaValue::Int(_) | LuaValue::Float(_) => {
                let s = crate::object::num_to_string(self, &val)?;
                self.set_at(slot, LuaValue::Str(s.clone()));
                Ok(s)
            }
            _ => Err(LuaError::type_error(&val, "convert to string")),
        }
    }
    pub fn coerce_to_string(&mut self, idx: StackIdx) -> Result<GcRef<LuaString>, LuaError> {
        let val = self.get_at(idx);
        match val {
            LuaValue::Str(s) => Ok(s),
            LuaValue::Int(_) | LuaValue::Float(_) => {
                let s = crate::object::num_to_string(self, &val)?;
                self.set_at(idx, LuaValue::Str(s.clone()));
                Ok(s)
            }
            _ => Err(LuaError::type_error(&val, "convert to string")),
        }
    }
    /// Parses `s` as a Lua number, returning `(value, consumed)` on success.
    ///
    /// `object::str2num` is number-model-blind and yields an integer whenever
    /// the text parses as one. The float-only versions (5.1/5.2) have no integer
    /// subtype, so a string coercion there is normalised to a float: otherwise
    /// `tonumber("10")` would carry an integer payload that diverges from the
    /// reference on any path that inspects the subtype.
    pub fn str_to_num(&mut self, s: &[u8]) -> Option<(LuaValue, usize)> {
        let mut out = LuaValue::Nil;
        let float_only =
            self.global().lua_version.number_model() == lua_types::NumberModel::FloatOnly;
        let sz = if float_only {
            crate::object::str2num_float_only(s, &mut out)
        } else {
            crate::object::str2num(s, &mut out)
        };
        if sz == 0 {
            return None;
        }
        Some((out, sz))
    }

    #[inline(always)]
    pub fn fast_get(&mut self, t: &LuaValue, k: &LuaValue) -> Result<Option<LuaValue>, LuaError> {
        let LuaValue::Table(tbl) = t else {
            return Ok(None);
        };
        let v = tbl.get(k);
        if matches!(v, LuaValue::Nil) {
            Ok(None)
        } else {
            Ok(Some(v))
        }
    }
    #[inline(always)]
    pub fn fast_get_int(&mut self, t: &LuaValue, k: i64) -> Result<Option<LuaValue>, LuaError> {
        let LuaValue::Table(tbl) = t else {
            return Ok(None);
        };
        let v = tbl.get_int(k);
        if matches!(v, LuaValue::Nil) {
            Ok(None)
        } else {
            Ok(Some(v))
        }
    }
    #[inline(always)]
    pub fn fast_get_short_str(
        &mut self,
        t: &LuaValue,
        k: &LuaValue,
    ) -> Result<Option<LuaValue>, LuaError> {
        let LuaValue::Table(tbl) = t else {
            return Ok(None);
        };
        let LuaValue::Str(s) = k else {
            return Ok(None);
        };
        let v = tbl.get_short_str(s);
        if matches!(v, LuaValue::Nil) {
            Ok(None)
        } else {
            Ok(Some(v))
        }
    }
    #[inline(always)]
    pub fn fast_tm_table(&mut self, t: Option<&GcRef<LuaTable>>, tm: TagMethod) -> LuaValue {
        let Some(mt) = t else {
            return LuaValue::Nil;
        };
        debug_assert!((tm as u8) <= TagMethod::Eq as u8);
        let ename = self.global().tmname[tm as usize].clone();
        mt.get_short_str(&ename)
    }
    pub fn fast_tm_ud(&mut self, u: &GcRef<LuaUserData>, tm: TagMethod) -> LuaValue {
        // metatable then index by the interned `__xxx` name.
        let mt = u.metatable();
        self.fast_tm_table(mt.as_ref(), tm)
    }

    pub fn table_get_with_tm(&mut self, t: &LuaValue, k: &LuaValue) -> Result<LuaValue, LuaError> {
        // Fast path: when the table has no metatable, `__index` can never
        // fire — so we can return the raw slot value (Nil if absent) without
        // routing through finish_get's push/pop scaffolding. Halves the
        // get-hot-path cost on tables without metamethods, which is the
        // common case in table.remove/insert shift loops and most user code.
        if let LuaValue::Table(tbl) = t {
            if !tbl.has_metatable() {
                return Ok(tbl.get(k));
            }
        }
        if let Some(v) = self.fast_get(t, k)? {
            return Ok(v);
        }
        let res = self.top_idx();
        self.push(LuaValue::Nil);
        crate::vm::finish_get(self, t.clone(), k.clone(), res, true, None, None)?;
        let value = self.get_at(res);
        self.pop();
        Ok(value)
    }
    /// Set `t[k] = v` with `__newindex` metamethod awareness.
    ///
    /// Fast path: when the table has no metatable, `__newindex` can never
    /// fire, so the existence check via `fast_get` is pure waste —
    /// `try_raw_set` handles both "key exists" and "key absent" cases via
    /// a single lookup internally. Removing the `fast_get` halves the
    /// lookups per set on the metamethod-free path (table.remove/insert
    /// hot loops, most user code).
    ///
    /// The GC backward barrier is invoked before the store (with `&v`)
    /// instead of after; the barrier only inspects the value's color, not
    /// its location, so the order is semantically equivalent to upstream
    /// C-Lua and lets us move `v` straight into `table_raw_set` without
    /// the extra `v.clone()` that the post-store ordering forced.
    #[inline]
    pub fn table_set_with_tm(
        &mut self,
        t: &LuaValue,
        k: LuaValue,
        v: LuaValue,
    ) -> Result<(), LuaError> {
        if let LuaValue::Table(tbl) = t {
            if !tbl.has_metatable() {
                self.gc_table_barrier_back(tbl, &v);
                return self.table_raw_set(t, k, v);
            }
        }
        if self.fast_get(t, &k)?.is_some() {
            self.gc_value_barrier_back(t, &v);
            return self.table_raw_set(t, k, v);
        }
        crate::vm::finish_set(self, t.clone(), k, v, true, None, None)
    }
    #[inline]
    pub fn table_raw_set(
        &mut self,
        t: &LuaValue,
        k: LuaValue,
        v: LuaValue,
    ) -> Result<(), LuaError> {
        let LuaValue::Table(tbl) = t else {
            return Err(LuaError::type_error(t, "index"));
        };
        let tbl = tbl.clone();
        tbl.raw_set(self, k, v)
    }
    #[inline]
    pub fn table_array_set(
        &mut self,
        t: &LuaValue,
        idx: usize,
        v: LuaValue,
    ) -> Result<(), LuaError> {
        let LuaValue::Table(tbl) = t else {
            return Err(LuaError::type_error(t, "index"));
        };
        let tbl = tbl.clone();
        tbl.raw_set_int(self, idx as i64 + 1, v)
    }
    pub fn table_ensure_array(&mut self, t: &LuaValue, n: usize) -> Result<(), LuaError> {
        let LuaValue::Table(tbl) = t else {
            return Err(LuaError::type_error(t, "index"));
        };
        if n > tbl.array_len() {
            tbl.resize(self, n, 0)?;
        }
        Ok(())
    }
    pub fn table_length(&mut self, t: &LuaValue) -> Result<i64, LuaError> {
        let LuaValue::Table(tbl) = t else {
            return Err(LuaError::type_error(t, "get length of"));
        };
        Ok(tbl.getn() as i64)
    }
    pub fn table_metatable(&mut self, v: &LuaValue) -> Option<GcRef<LuaTable>> {
        match v {
            LuaValue::Table(t) => t.metatable(),
            LuaValue::UserData(u) => u.metatable(),
            other => {
                let idx = other.base_type() as usize;
                self.global().mt[idx].clone()
            }
        }
    }
    pub fn table_resize(
        &mut self,
        t: &GcRef<LuaTable>,
        na: usize,
        nh: usize,
    ) -> Result<(), LuaError> {
        self.mark_gc_check_needed();
        t.resize(self, na, nh)
    }
    pub fn table_getn(&self, t: &GcRef<LuaTable>) -> i64 {
        // C's `luaH_getn` returns a boundary i such that t[i] is present and
        // t[i+1] is absent (or 0 if t[1] is absent), found via a binary or
        // unbound search over the array part. This instead walks integer
        // keys linearly from 1 — each `get_int` call is O(1), but the walk
        // itself is O(n) in the boundary rather than C's O(log n).
        let mut i: i64 = 1;
        loop {
            let v = t.get_int(i);
            if matches!(v, LuaValue::Nil) {
                return i - 1;
            }
            i += 1;
        }
    }

    pub fn try_bin_tm(
        &mut self,
        p1: &LuaValue,
        p1_idx: Option<StackIdx>,
        p2: &LuaValue,
        p2_idx: Option<StackIdx>,
        res: StackIdx,
        tm: lua_types::tagmethod::TagMethod,
    ) -> Result<(), LuaError> {
        let event = crate::tagmethods::TagMethod::from_u8(tm as u8);
        crate::tagmethods::try_bin_tm(self, p1, p1_idx, p2, p2_idx, res, event)
    }
    pub fn try_bin_i_tm(
        &mut self,
        p1: &LuaValue,
        p1_idx: Option<StackIdx>,
        imm: i64,
        flip: bool,
        res: StackIdx,
        tm: lua_types::tagmethod::TagMethod,
    ) -> Result<(), LuaError> {
        let event = crate::tagmethods::TagMethod::from_u8(tm as u8);
        crate::tagmethods::try_bini_tm(self, p1, p1_idx, imm, flip, res, event)
    }
    pub fn try_bin_assoc_tm(
        &mut self,
        p1: &LuaValue,
        p1_idx: Option<StackIdx>,
        p2: &LuaValue,
        p2_idx: Option<StackIdx>,
        flip: bool,
        res: StackIdx,
        tm: lua_types::tagmethod::TagMethod,
    ) -> Result<(), LuaError> {
        let event = crate::tagmethods::TagMethod::from_u8(tm as u8);
        crate::tagmethods::try_bin_assoc_tm(self, p1, p1_idx, p2, p2_idx, flip, res, event)
    }
    pub fn try_concat_tm(&mut self, _p1: &LuaValue, _p2: &LuaValue) -> Result<(), LuaError> {
        crate::tagmethods::try_concat_tm(self)
    }
    pub fn call_tm(
        &mut self,
        f: LuaValue,
        p1: &LuaValue,
        p2: &LuaValue,
        p3: &LuaValue,
    ) -> Result<(), LuaError> {
        crate::tagmethods::call_tm(self, f, p1.clone(), p2.clone(), p3.clone())
    }
    pub fn call_tm_res(
        &mut self,
        f: LuaValue,
        p1: &LuaValue,
        p2: &LuaValue,
        res: StackIdx,
    ) -> Result<(), LuaError> {
        crate::tagmethods::call_tm_res(self, f, p1.clone(), p2.clone(), res)
    }
    pub fn call_tm_res_bool(
        &mut self,
        f: LuaValue,
        p1: &LuaValue,
        p2: &LuaValue,
    ) -> Result<bool, LuaError> {
        let res = self.top_idx();
        self.push(LuaValue::Nil);
        crate::tagmethods::call_tm_res(self, f, p1.clone(), p2.clone(), res)?;
        let result = self.get_at(res).clone();
        self.pop();
        Ok(!matches!(result, LuaValue::Nil | LuaValue::Bool(false)))
    }
    pub fn call_order_tm(
        &mut self,
        p1: &LuaValue,
        p2: &LuaValue,
        tm: lua_types::tagmethod::TagMethod,
    ) -> Result<bool, LuaError> {
        let event = crate::tagmethods::TagMethod::from_u8(tm as u8);
        crate::tagmethods::call_order_tm(self, p1, p2, event)
    }
    pub fn call_order_i_tm(
        &mut self,
        p1: &LuaValue,
        v2: i64,
        flip: bool,
        isfloat: bool,
        tm: lua_types::tagmethod::TagMethod,
    ) -> Result<bool, LuaError> {
        let event = crate::tagmethods::TagMethod::from_u8(tm as u8);
        crate::tagmethods::call_orderi_tm(self, p1, v2 as i32, flip, isfloat, event)
    }

    #[inline(always)]
    pub fn proto_code(
        &self,
        cl: &GcRef<lua_types::closure::LuaLClosure>,
        pc: u32,
    ) -> lua_types::opcode::Instruction {
        cl.proto.code[pc as usize]
    }
    #[inline(always)]
    pub fn proto_const(&self, cl: &GcRef<lua_types::closure::LuaLClosure>, idx: usize) -> LuaValue {
        cl.proto.k[idx].clone()
    }
    /// Hot-path accessor: returns `Some(i)` only when the constant pool entry
    /// at `idx` is an `Int`. Avoids the full `LuaValue` clone that
    /// `proto_const` performs.
    ///
    /// arithmetic opcode macros (`op_arithK`).
    #[inline(always)]
    pub fn proto_const_int(
        &self,
        cl: &GcRef<lua_types::closure::LuaLClosure>,
        idx: usize,
    ) -> Option<i64> {
        match &cl.proto.k[idx] {
            LuaValue::Int(v) => Some(*v),
            _ => None,
        }
    }
    /// Hot-path accessor: returns `Some(f)` for `Float(f)` or `Int(i)` (coerced)
    /// constants. Avoids the full `LuaValue` clone. Used by the float fast
    /// path of `OP_ADDK`/`OP_SUBK`/`OP_MULK`/`OP_DIVK`/`OP_POWK`.
    #[inline(always)]
    pub fn proto_const_num(
        &self,
        cl: &GcRef<lua_types::closure::LuaLClosure>,
        idx: usize,
    ) -> Option<f64> {
        match &cl.proto.k[idx] {
            LuaValue::Float(f) => Some(*f),
            LuaValue::Int(v) => Some(*v as f64),
            _ => None,
        }
    }
    pub fn get_proto_instr(&self, ci: CallInfoIdx, pc: u32) -> lua_types::opcode::Instruction {
        let cl = self
            .ci_lua_closure(ci)
            .expect("get_proto_instr: CallInfo does not hold a Lua closure");
        cl.proto.code[pc as usize]
    }
    /// flag as `bool` (C returns `int` 0/1).
    ///
    /// The C function reads `L->ci` directly, so the `_idx` argument is unused;
    /// the VM passes its locally tracked `ci` for symmetry with `trace_exec`.
    pub fn trace_call(&mut self, _idx: CallInfoIdx) -> Result<bool, LuaError> {
        Ok(crate::debug::trace_call(self)? != 0)
    }
    /// returning `bool` for the trap flag. `_idx` is unused for the same reason
    /// as `trace_call`; `pc` is the 0-based index of the next instruction.
    pub fn trace_exec(&mut self, _idx: CallInfoIdx, pc: u32) -> Result<bool, LuaError> {
        Ok(crate::debug::trace_exec(self, pc)? != 0)
    }
    /// Fires the call hook for the frame at `idx`.
    ///
    /// On Lua 5.1 a tail call reuses the caller's frame but still fires an
    /// ordinary `"call"` hook for the callee (the synthetic call event is on the
    /// return side, as a `"tail return"`). 5.2+ instead report the call as a
    /// `"tail call"` event, which `do_::hookcall` derives from the `CIST_TAIL`
    /// callstatus bit. To get the 5.1 naming without disturbing `CIST_TAIL`
    /// (which `debug.getinfo` still reads to report `what == "tail"`), the bit is
    /// masked off across the `hookcall` call and restored afterwards, so on 5.1
    /// the event is always `LUA_HOOKCALL`. 5.2+ takes the unchanged delegation.
    pub fn hook_call(&mut self, idx: CallInfoIdx) -> Result<(), LuaError> {
        if self.global().lua_version == lua_types::LuaVersion::V51 {
            let had_tail = self.call_info[idx.as_usize()].callstatus & CIST_TAIL;
            self.call_info[idx.as_usize()].callstatus &= !CIST_TAIL;
            let r = crate::do_::hookcall(self, idx);
            self.call_info[idx.as_usize()].callstatus |= had_tail;
            return r;
        }
        crate::do_::hookcall(self, idx)
    }
    #[inline(always)]
    fn gc_step_flags(&self) -> Option<(bool, bool)> {
        let g = self.global();
        if !g.is_gc_running() {
            return None;
        }
        let should_collect = g.heap.would_collect();
        let has_finalizers = g.finalizers.has_to_be_finalized();
        if should_collect || has_finalizers {
            Some((should_collect, has_finalizers))
        } else {
            None
        }
    }

    #[inline(always)]
    fn should_check_gc(&mut self) -> bool {
        if self.gc_check_needed {
            return true;
        }
        if self.global().finalizers.has_to_be_finalized() {
            self.gc_check_needed = true;
            return true;
        }
        false
    }

    #[inline(always)]
    pub(crate) fn mark_gc_check_needed(&mut self) {
        self.gc_check_needed = true;
    }

    /// The stack prefix the GC must treat as live: `[0 .. gc_trace_bound())`.
    ///
    /// C's `traversethread` walks exactly `[stack .. top)` (`lgc.c`). The
    /// companion invariant is that `top` covers every live slot at every
    /// point a collect can run: C functions keep `top` exact at all times,
    /// and lvm.c's Protect sites raise it (`savestate`: `top = ci->top`)
    /// before any mid-opcode operation that can collect. Checkpoints whose
    /// C original runs under Protect must therefore do the same top fixup
    /// at the call site (e.g. `get_varargs`). Widening the bound here to
    /// `ci.top` instead was tried and rejected: every suspended frame's
    /// reserved slice stays traced forever, and the over-retention drives
    /// the generational pacer into permanent re-collection (db.lua's
    /// line-hook section went from seconds to timeout).
    pub fn gc_trace_bound(&self) -> usize {
        (self.top.0 as usize).min(self.stack.len())
    }

    /// Nil this thread's dead stack slice `[gc_trace_bound() ..
    /// stack.len())`. C clears the equivalent slice in `traversethread`'s
    /// atomic phase (`lgc.c`: `setnilvalue` from `top` to `stack_last +
    /// EXTRA_STACK`). Without the clear, a slot that was above the bound at
    /// one collect (its object swept) and drifts back under a raised bound
    /// later feeds a dangling GcRef to the marker — #140 bug B.
    pub fn clear_dead_stack_tail(&mut self) {
        let bound = self.gc_trace_bound();
        for slot in &mut self.stack[bound..] {
            slot.val = LuaValue::Nil;
        }
    }

    /// Clear the dead stack slice of this thread and of every registered
    /// coroutine whose state is borrowable. Threads held by a resume chain
    /// or a debug-API guard are rooted via `suspended_parent_stacks`
    /// snapshots and skipped here; their tails are cleared at their next
    /// reachable collect. Call before any collection runs (C parity:
    /// `traversethread`'s atomic clear).
    pub fn gc_clear_dead_stack_tails(&mut self) {
        self.clear_dead_stack_tail();
        let global = self.global.clone();
        let g = global.borrow();
        for entry in g.threads.values() {
            if let Ok(mut t) = entry.state.try_borrow_mut() {
                t.clear_dead_stack_tail();
            }
        }
    }

    /// `gc_clear_dead_stack_tails` gated on `would_collect` — the one-line
    /// companion for call sites that invoke `gc().check_step()` directly.
    /// Zero cost when no collection is due.
    pub fn gc_pre_collect_clear(&mut self) {
        if self.global().heap.would_collect() {
            self.gc_clear_dead_stack_tails();
        }
    }

    #[inline(always)]
    pub fn gc_check_step(&mut self) {
        if !self.allowhook {
            return;
        }
        if !self.should_check_gc() {
            return;
        }
        let Some((should_collect, has_finalizers)) = self.gc_step_flags() else {
            self.gc_check_needed = false;
            return;
        };
        if should_collect || has_finalizers {
            if should_collect {
                self.gc_clear_dead_stack_tails();
                self.gc().check_step();
            }
            crate::api::run_pending_finalizers(self);
            self.gc_check_needed = true;
        }
        let should_keep_checking = {
            let g = self.global();
            g.heap.would_collect() || g.finalizers.has_to_be_finalized()
        };
        self.gc_check_needed = should_keep_checking;
    }
    #[inline(always)]
    pub fn gc_cond_step(&mut self) {
        if !self.allowhook {
            return;
        }
        if !self.should_check_gc() {
            return;
        }
        let Some((should_collect, has_finalizers)) = self.gc_step_flags() else {
            self.gc_check_needed = false;
            return;
        };
        if should_collect || has_finalizers {
            if should_collect {
                self.gc_clear_dead_stack_tails();
                self.gc().check_step();
            }
            crate::api::run_pending_finalizers(self);
            self.gc_check_needed = true;
        }
        let should_keep_checking = {
            let g = self.global();
            g.heap.would_collect() || g.finalizers.has_to_be_finalized()
        };
        self.gc_check_needed = should_keep_checking;
    }
    pub fn gc_barrier_back(&mut self, t: &dyn std::any::Any, v: &LuaValue) {
        self.gc().barrier_back(t, v);
    }
    #[inline(always)]
    pub fn gc_value_barrier_back(&mut self, t: &LuaValue, v: &LuaValue) {
        if !v.is_collectable() {
            return;
        }
        if let LuaValue::Table(tbl) = t {
            self.gc_table_barrier_back(tbl, v);
        } else {
            self.gc_barrier_back(t, v);
        }
    }
    #[inline(always)]
    pub fn gc_table_barrier_back(&mut self, t: &GcRef<LuaTable>, v: &LuaValue) {
        if !v.is_collectable() {
            return;
        }
        self.gc().table_barrier_back(t, v);
    }
    pub fn gc_barrier_upval(&mut self, uv: &GcRef<UpVal>, v: &LuaValue) {
        self.gc().barrier(uv, v);
    }
    /// Compares `GlobalState::current_thread_id` against `main_thread_id`;
    /// `false` while a coroutine resume has swapped `current_thread_id` to
    /// the child's id.
    pub fn is_main_thread(&mut self) -> bool {
        let g = self.global();
        g.current_thread_id == g.main_thread_id
    }
    pub fn obj_type_name<'v>(&self, v: &'v LuaValue) -> std::borrow::Cow<'static, [u8]> {
        let honors_name = self.global().lua_version.honors_name_metafield();
        match v {
            LuaValue::LightUserData(_) => std::borrow::Cow::Borrowed(b"light userdata"),
            LuaValue::Table(t) => {
                if honors_name {
                    if let Some(mt) = t.metatable() {
                        if let LuaValue::Str(s) = mt.get_str_bytes(b"__name") {
                            return std::borrow::Cow::Owned(s.as_bytes().to_vec());
                        }
                    }
                }
                std::borrow::Cow::Borrowed(crate::tagmethods::type_name(v.base_type()))
            }
            LuaValue::UserData(u) => {
                if honors_name {
                    if let Some(mt) = u.metatable() {
                        if let LuaValue::Str(s) = mt.get_str_bytes(b"__name") {
                            return std::borrow::Cow::Owned(s.as_bytes().to_vec());
                        }
                    }
                }
                std::borrow::Cow::Borrowed(crate::tagmethods::type_name(v.base_type()))
            }
            _ => std::borrow::Cow::Borrowed(crate::tagmethods::type_name(v.base_type())),
        }
    }

    pub fn full_type_name(&mut self, v: &LuaValue) -> Result<Vec<u8>, LuaError> {
        crate::tagmethods::obj_type_name(self, v)
    }
    pub fn emit_warning(&mut self, _msg: &[u8], _to_cont: bool) {
        warning(self, _msg, _to_cont)
    }
}

// ─── GcHandle ───────────────────────────────────────────────────────────────

/// A short-lived handle returned by `state.gc()` for GC operations.
pub struct GcHandle<'a> {
    _state: &'a mut LuaState,
}

/// Composite root passed to `Heap::full_collect`. `new_state` leaves
/// `GlobalState.mainthread = None` (to break the self-referential Rc
/// cycle — see the field doc), so the running thread's stack and openupval
/// list are not reachable from `GlobalState::trace`. Wrapping both
/// references in a single `Trace`-implementing root injects the active
/// thread as a second mark source for the duration of the collection.
struct CollectRoots<'a> {
    global: &'a GlobalState,
    thread: &'a LuaState,
}

#[derive(Clone, Copy)]
enum HeapCollectMode {
    Full,
    Step,
    Minor,
}

impl<'a> lua_gc::Trace for CollectRoots<'a> {
    fn trace(&self, m: &mut lua_gc::Marker) {
        self.global.trace(m);
        self.thread.trace(m);
    }
}

#[derive(Clone, Copy)]
enum BarrierKind {
    Forward,
    Backward,
}

fn barrier_lua_value<P>(
    heap: &lua_gc::Heap,
    parent: GcRef<P>,
    child: &LuaValue,
    generational: bool,
    kind: BarrierKind,
) where
    P: lua_gc::Trace + 'static,
{
    if !child.is_collectable() {
        return;
    }
    if generational && matches!(kind, BarrierKind::Backward) {
        heap.generational_backward_barrier(parent.0);
    }
    match child {
        LuaValue::Str(c) => barrier_gc_child(heap, parent, *c, generational, kind),
        LuaValue::Table(c) => barrier_gc_child(heap, parent, *c, generational, kind),
        LuaValue::Function(LuaClosure::Lua(c)) => {
            barrier_gc_child(heap, parent, *c, generational, kind)
        }
        LuaValue::Function(LuaClosure::C(c)) => {
            barrier_gc_child(heap, parent, *c, generational, kind)
        }
        LuaValue::UserData(c) => barrier_gc_child(heap, parent, *c, generational, kind),
        LuaValue::Thread(c) => barrier_gc_child(heap, parent, *c, generational, kind),
        LuaValue::Nil
        | LuaValue::Bool(_)
        | LuaValue::Int(_)
        | LuaValue::Float(_)
        | LuaValue::LightUserData(_)
        | LuaValue::Function(LuaClosure::LightC(_)) => {}
    }
}

fn barrier_gc_child<P, C>(
    heap: &lua_gc::Heap,
    parent: GcRef<P>,
    child: GcRef<C>,
    generational: bool,
    kind: BarrierKind,
) where
    P: lua_gc::Trace + 'static,
    C: lua_gc::Trace + 'static,
{
    if generational && matches!(kind, BarrierKind::Forward) {
        heap.generational_forward_barrier(parent.0, child.0);
    } else if matches!(kind, BarrierKind::Backward) {
        heap.barrier_back(parent.0, child.0);
    } else {
        heap.barrier(parent.0, child.0);
    }
}

fn barrier_child_any<P>(
    heap: &lua_gc::Heap,
    parent: GcRef<P>,
    child: &dyn std::any::Any,
    generational: bool,
    kind: BarrierKind,
) where
    P: lua_gc::Trace + 'static,
{
    if let Some(v) = child.downcast_ref::<LuaValue>() {
        barrier_lua_value(heap, parent, v, generational, kind);
    } else if let Some(c) = child.downcast_ref::<GcRef<LuaString>>() {
        barrier_gc_child(heap, parent, c.clone(), generational, kind);
    } else if let Some(c) = child.downcast_ref::<GcRef<LuaTable>>() {
        barrier_gc_child(heap, parent, c.clone(), generational, kind);
    } else if let Some(c) = child.downcast_ref::<GcRef<LuaClosureLua>>() {
        barrier_gc_child(heap, parent, c.clone(), generational, kind);
    } else if let Some(c) = child.downcast_ref::<GcRef<LuaClosureC>>() {
        barrier_gc_child(heap, parent, c.clone(), generational, kind);
    } else if let Some(c) = child.downcast_ref::<GcRef<LuaUserData>>() {
        barrier_gc_child(heap, parent, c.clone(), generational, kind);
    } else if let Some(c) = child.downcast_ref::<GcRef<lua_types::value::LuaThread>>() {
        barrier_gc_child(heap, parent, c.clone(), generational, kind);
    } else if let Some(c) = child.downcast_ref::<GcRef<LuaProto>>() {
        barrier_gc_child(heap, parent, c.clone(), generational, kind);
    } else if let Some(c) = child.downcast_ref::<GcRef<UpVal>>() {
        barrier_gc_child(heap, parent, c.clone(), generational, kind);
    }
}

fn barrier_any(
    heap: &lua_gc::Heap,
    parent: &dyn std::any::Any,
    child: &dyn std::any::Any,
    generational: bool,
    kind: BarrierKind,
) {
    if let Some(v) = parent.downcast_ref::<LuaValue>() {
        match v {
            LuaValue::Str(p) => barrier_child_any(heap, *p, child, generational, kind),
            LuaValue::Table(p) => barrier_child_any(heap, *p, child, generational, kind),
            LuaValue::Function(LuaClosure::Lua(p)) => {
                barrier_child_any(heap, *p, child, generational, kind)
            }
            LuaValue::Function(LuaClosure::C(p)) => {
                barrier_child_any(heap, *p, child, generational, kind)
            }
            LuaValue::UserData(p) => barrier_child_any(heap, *p, child, generational, kind),
            LuaValue::Thread(p) => barrier_child_any(heap, *p, child, generational, kind),
            LuaValue::Nil
            | LuaValue::Bool(_)
            | LuaValue::Int(_)
            | LuaValue::Float(_)
            | LuaValue::LightUserData(_)
            | LuaValue::Function(LuaClosure::LightC(_)) => {}
        }
    } else if let Some(p) = parent.downcast_ref::<GcRef<LuaString>>() {
        barrier_child_any(heap, p.clone(), child, generational, kind);
    } else if let Some(p) = parent.downcast_ref::<GcRef<LuaTable>>() {
        barrier_child_any(heap, p.clone(), child, generational, kind);
    } else if let Some(p) = parent.downcast_ref::<GcRef<LuaClosureLua>>() {
        barrier_child_any(heap, p.clone(), child, generational, kind);
    } else if let Some(p) = parent.downcast_ref::<GcRef<LuaClosureC>>() {
        barrier_child_any(heap, p.clone(), child, generational, kind);
    } else if let Some(p) = parent.downcast_ref::<GcRef<LuaUserData>>() {
        barrier_child_any(heap, p.clone(), child, generational, kind);
    } else if let Some(p) = parent.downcast_ref::<GcRef<lua_types::value::LuaThread>>() {
        barrier_child_any(heap, p.clone(), child, generational, kind);
    } else if let Some(p) = parent.downcast_ref::<GcRef<LuaProto>>() {
        barrier_child_any(heap, p.clone(), child, generational, kind);
    } else if let Some(p) = parent.downcast_ref::<GcRef<UpVal>>() {
        barrier_child_any(heap, p.clone(), child, generational, kind);
    }
}

/// Fixed-point trace of every registered coroutine whose thread handle was
/// reached from a real root.
///
/// A thread whose `RefCell` is mutably borrowed at collect time CANNOT be
/// traced — its stack is silently invisible to the marker. Exactly two
/// borrow-holders are legitimate: the currently-running thread (rooted
/// directly via `CollectRoots.thread`) and resume-chain parents (rooted via
/// the `suspended_parent_stacks` snapshots, one per nesting level). Any
/// other borrow held across a collect point un-roots that coroutine for the
/// whole cycle — the bug-A root-loss class from issue #140 — so debug builds
/// assert the failure count stays within snapshot coverage.
fn trace_reachable_threads(
    global: &GlobalState,
    _current_thread_id: u64,
    marker: &mut lua_gc::Marker,
) {
    use lua_gc::Trace;

    #[cfg(debug_assertions)]
    let mut uncovered_borrowed: Vec<u64> = Vec::new();

    loop {
        let visited_before = marker.visited_count();
        for (id, entry) in global.threads.iter() {
            if thread_entry_marked_alive(marker, *id, entry) {
                match entry.state.try_borrow() {
                    Ok(thread) => thread.trace(marker),
                    Err(_) => {
                        #[cfg(debug_assertions)]
                        if *id != _current_thread_id && !uncovered_borrowed.contains(id) {
                            uncovered_borrowed.push(*id);
                        }
                    }
                }
            }
        }
        marker.drain_gray_queue();
        if marker.visited_count() == visited_before {
            break;
        }
    }

    remark_legacy_open_upvalues(global, marker);

    #[cfg(debug_assertions)]
    debug_assert!(
        uncovered_borrowed.len() <= global.suspended_parent_stacks.len(),
        "GC root loss: {} marked-alive coroutine(s) (ids {:?}) were mutably \
         borrowed at collect time with only {} parent snapshot(s) covering \
         them — their stacks were NOT traced this cycle, so anything only \
         reachable from them will be swept (issue #140 bug-A class). A borrow \
         of a coroutine's state must not be held across an allocation \
         checkpoint without pushing a parent GC snapshot.",
        uncovered_borrowed.len(),
        uncovered_borrowed,
        global.suspended_parent_stacks.len()
    );
}

/// Legacy (5.1/5.2/5.3) `remarkupvals` (lgc.c). After the main mark phase has
/// settled which threads are reachable, an unmarked thread's *touched* open
/// upvalues have their pointed-to stack values re-marked exactly once, then the
/// thread's touched flag is cleared — mirroring C marking each touched upvalue's
/// value and removing the thread from `g->twups`.
///
/// This re-mark resurrects an object reachable only through a suspended thread's
/// cycle for one extra collection. Concretely, a `co <-> closure <-> table`
/// cycle with a `__gc` on the table is *not* finalized after the first
/// `collectgarbage()` (the touched open upvalue keeps the table marked, and the
/// table transitively re-marks the thread); on the next collection the flag is
/// already cleared, nothing re-marks the table, and it is separated to be
/// finalized. That two-collection delay is exactly what 5.3's `gc.lua` asserts
/// (`assert(not collected)` after the first collect).
///
/// From 5.4 the C condition became `!iswhite(uv)` rather than `touched`, so a
/// white open upvalue on an unmarked thread is never re-marked and the same
/// cycle is finalized after a single collect. Modern versions therefore skip
/// this pass entirely, leaving the baked 5.4/5.5 behavior untouched.
fn remark_legacy_open_upvalues(global: &GlobalState, marker: &mut lua_gc::Marker) {
    use lua_gc::Trace;

    let legacy = matches!(
        global.lua_version,
        lua_types::LuaVersion::V51 | lua_types::LuaVersion::V52 | lua_types::LuaVersion::V53
    );
    if !legacy {
        return;
    }

    let mut remarked_any = false;
    for (id, entry) in global.threads.iter() {
        if entry.value.id != *id {
            continue;
        }
        if thread_entry_marked_alive(marker, *id, entry) {
            continue;
        }
        let Ok(thread) = entry.state.try_borrow() else {
            continue;
        };
        if thread.openupval.is_empty() || !thread.legacy_open_upval_touched.get() {
            continue;
        }
        thread.legacy_open_upval_touched.set(false);
        for uv in thread.openupval.iter() {
            let Some((_tid, idx)) = uv.try_open_payload() else {
                continue;
            };
            let slot = idx.0 as usize;
            if slot < thread.stack.len() {
                thread.stack[slot].val.trace(marker);
                remarked_any = true;
            }
        }
    }

    if !remarked_any {
        return;
    }
    marker.drain_gray_queue();

    loop {
        let visited_before = marker.visited_count();
        for (id, entry) in global.threads.iter() {
            if thread_entry_marked_alive(marker, *id, entry) {
                if let Ok(thread) = entry.state.try_borrow() {
                    thread.trace(marker);
                }
            }
        }
        marker.drain_gray_queue();
        if marker.visited_count() == visited_before {
            break;
        }
    }
}

fn thread_entry_marked_alive(
    marker: &lua_gc::Marker,
    id: u64,
    entry: &ThreadRegistryEntry,
) -> bool {
    marker.is_marked_or_old(entry.value.0) && entry.value.id == id
}

fn lua_value_marked_or_old(marker: &lua_gc::Marker, value: &LuaValue) -> bool {
    match value {
        LuaValue::Str(v) => marker.is_marked_or_old(v.0),
        LuaValue::Table(v) => marker.is_marked_or_old(v.0),
        LuaValue::Function(LuaClosure::Lua(v)) => marker.is_marked_or_old(v.0),
        LuaValue::Function(LuaClosure::C(v)) => marker.is_marked_or_old(v.0),
        LuaValue::UserData(v) => marker.is_marked_or_old(v.0),
        LuaValue::Thread(v) => marker.is_marked_or_old(v.0),
        LuaValue::Nil
        | LuaValue::Bool(_)
        | LuaValue::Int(_)
        | LuaValue::Float(_)
        | LuaValue::LightUserData(_)
        | LuaValue::Function(LuaClosure::LightC(_)) => true,
    }
}

fn finalizer_marked_or_old(marker: &lua_gc::Marker, object: &FinalizerObject) -> bool {
    match object {
        FinalizerObject::Table(t) => marker.is_marked_or_old(t.0),
        FinalizerObject::UserData(u) => marker.is_marked_or_old(u.0),
    }
}

fn weak_snapshot_tables<'a>(
    snapshot: &'a lua_gc::WeakRegistrySnapshot<GcRef<LuaTable>>,
) -> impl Iterator<Item = &'a GcRef<LuaTable>> {
    snapshot
        .weak_values
        .iter()
        .chain(snapshot.ephemeron.iter())
        .chain(snapshot.all_weak.iter())
}

fn close_open_upvalues_for_unreachable_threads(global: &GlobalState, marker: &mut lua_gc::Marker) {
    use lua_gc::Trace;

    let mut closed_values = Vec::<LuaValue>::new();
    for (id, entry) in global.threads.iter() {
        if entry.value.id != *id {
            continue;
        }
        if thread_entry_marked_alive(marker, *id, entry) {
            continue;
        }
        let Ok(thread) = entry.state.try_borrow() else {
            continue;
        };
        for uv in thread.openupval.iter() {
            if !marker.is_visited(uv.identity()) {
                continue;
            }
            let Some((thread_id, idx)) = uv.try_open_payload() else {
                continue;
            };
            if thread_id != *id {
                continue;
            }
            let value = thread.get_at(idx);
            uv.close_with(value.clone());
            closed_values.push(value);
        }
    }
    for value in closed_values {
        value.trace(marker);
    }
    marker.drain_gray_queue();
}

/// Mark-phase half of intern-table cleanup (C: dead strings leave the
/// stringtable via `luaS_remove` at free time). The marker and a `&mut`
/// global cannot coexist, so this records each DEAD entry's
/// `(hash, identity)` pair during the immutable post-mark hook; the
/// steady-state output is empty or tiny, replacing the old all-live-ids
/// vector (O(table) push + sort + per-entry binary-search retain).
fn record_dead_interned_strings(
    global: &GlobalState,
    marker: &lua_gc::Marker,
    dead_pairs: &std::cell::RefCell<Vec<(u32, usize)>>,
) {
    let mut dead = dead_pairs.borrow_mut();
    for s in global.interned_lt.iter() {
        let id = s.identity();
        if !marker.is_visited(id) {
            dead.push((s.hash(), id));
        }
    }
}

/// Sweep-phase half: O(dead) bucket removals by cached hash + identity,
/// then a single batch-end shrink check.
///
/// The shrink check runs ONCE per collection here — after every dead entry
/// for this cycle has been removed, so only live `GcRef<LuaString>`s remain
/// to rehash — and deliberately NOT inside `remove()`. A per-removal shrink
/// would rehash O(live) entries on each of the O(dead) removals, turning the
/// batch into O(live²); deferring to batch end keeps removal O(dead) and adds
/// one bounded load-factor check per GC cycle. This mirrors C, where
/// `luaS_remove` only unlinks and the shrink lives in `checkSizes`, called
/// once per `singlestep`/`fullgc` cycle.
fn remove_dead_interned_strings(global: &mut GlobalState, dead_pairs: Vec<(u32, usize)>) {
    for (hash, id) in dead_pairs {
        global.interned_lt.remove(hash, id);
    }
    global.interned_lt.shrink_if_sparse();
}

impl<'a> GcHandle<'a> {
    /// Drives implicit collection when the heap's byte threshold is
    /// exceeded. Without this hook, loops that allocate without an
    /// explicit `collectgarbage()` call (e.g. `closure.lua`'s
    /// `while x[1] do local a = A..A end` GC-driven loop) never settle.
    pub fn check_step(&self) {
        if !self._state.global().is_gc_running() {
            return;
        }
        if self._state.global().is_gen_mode() {
            let should_collect = {
                let g = self._state.global();
                g.heap.would_collect() || g.gc_debt() > 0
            };
            if should_collect {
                self.generational_step();
            }
        } else {
            self.collect_via_heap(/* force = */ false);
        }
    }

    pub fn full_collect(&self) {
        if self._state.global().is_gen_mode() {
            self.fullgen();
        } else {
            self.collect_via_heap(/* force = */ true);
        }
    }

    fn negative_debt(bytes: usize) -> isize {
        -(bytes.min(isize::MAX as usize) as isize)
    }

    fn set_minor_debt(&self) {
        let mut g = self._state.global_mut();
        let total = g.total_bytes();
        let growth = (total / 100).saturating_mul(g.genminormul as usize);
        g.heap
            .set_threshold_bytes(total.saturating_add(growth.max(1)));
        set_debt(&mut *g, Self::negative_debt(growth));
    }

    fn set_pause_debt(&self) {
        let mut g = self._state.global_mut();
        let total = g.total_bytes();
        let pause = g.gc_pause_param().max(0) as usize;
        let threshold = g.gc_estimate.max(1).saturating_mul(pause) / 100;
        let debt = if threshold > total {
            Self::negative_debt(threshold - total)
        } else {
            0
        };
        let heap_threshold = if threshold > total {
            threshold
        } else {
            total.saturating_add(1)
        };
        g.heap.set_threshold_bytes(heap_threshold);
        set_debt(&mut *g, debt);
    }

    fn enter_incremental_mode(&self) {
        let mut g = self._state.global_mut();
        g.heap.reset_all_ages();
        g.finalizers.reset_generation_boundaries();
        g.gckind = GcKind::Incremental as u8;
        g.lastatomic = 0;
    }

    fn enter_generational_mode(&self) -> usize {
        self.collect_via_heap_mode(HeapCollectMode::Full);
        let numobjs = {
            let mut g = self._state.global_mut();
            g.heap.promote_all_to_old();
            g.finalizers.promote_all_pending_to_old();
            g.heap.allgc_count()
        };
        let total = self._state.global().total_bytes();
        {
            let mut g = self._state.global_mut();
            g.gckind = GcKind::Generational as u8;
            g.lastatomic = 0;
            g.gc_estimate = total;
        }
        self.set_minor_debt();
        numobjs
    }

    fn fullgen(&self) -> usize {
        self.enter_incremental_mode();
        self.enter_generational_mode()
    }

    fn stepgenfull(&self, lastatomic: usize) {
        if self._state.global().gckind == GcKind::Generational as u8 {
            self.enter_incremental_mode();
        }
        self.collect_via_heap_mode(HeapCollectMode::Full);
        let newatomic = self._state.global().heap.allgc_count().max(1);
        if newatomic < lastatomic.saturating_add(lastatomic >> 3) {
            {
                let mut g = self._state.global_mut();
                g.heap.promote_all_to_old();
                g.finalizers.promote_all_pending_to_old();
            }
            let total = self._state.global().total_bytes();
            {
                let mut g = self._state.global_mut();
                g.gckind = GcKind::Generational as u8;
                g.lastatomic = 0;
                g.gc_estimate = total;
            }
            self.set_minor_debt();
        } else {
            {
                let mut g = self._state.global_mut();
                g.heap.reset_all_ages();
                g.finalizers.reset_generation_boundaries();
            }
            let total = self._state.global().total_bytes();
            {
                let mut g = self._state.global_mut();
                g.gckind = GcKind::Incremental as u8;
                g.lastatomic = newatomic;
                g.gc_estimate = total;
            }
            self.set_pause_debt();
        }
    }

    /// Shared driver behind both `full_collect` (force-collect) and
    /// `check_step` (collect only if heap byte threshold exceeded).
    ///
    /// Snapshots the weak-tables registry, invokes the heap's collect path
    /// with a post-mark weak-prune hook, and rebuilds the registry by
    /// retaining only entries whose target was reachable. The same hook
    /// works for both modes — the heap short-circuits when force=false and
    /// the threshold isn't met.
    fn collect_via_heap(&self, force: bool) {
        self.collect_via_heap_mode(if force {
            HeapCollectMode::Full
        } else {
            HeapCollectMode::Step
        });
    }

    fn collect_via_heap_mode(&self, mode: HeapCollectMode) {
        use lua_gc::Trace;
        let state_ref: &LuaState = &*self._state;

        // Fast path: when the caller did not force a collection, skip all
        // the snapshot work (3 Vec allocations + 3 HashSet allocations) if
        // the heap is paused or under threshold — a `step()` in that state
        // is a no-op, so the snapshot would be pure waste. Called millions
        // of times per recursive workload via `gc_check_step` in `precall`.
        if matches!(mode, HeapCollectMode::Step) {
            let g = state_ref.global.borrow();
            if !g.heap.would_collect() {
                return;
            }
        }

        // Snapshot weak tables BEFORE the collect. `identity()` reads only
        // the pointer address — safe even on still-dangling weak handles —
        // and dedup by identity keeps the iteration linear.
        let weak_tables_snapshot: lua_gc::WeakRegistrySnapshot<GcRef<LuaTable>> = {
            let mut g = state_ref.global.borrow_mut();
            g.weak_tables_registry.live_snapshot_by_kind()
        };

        // Snapshot pending finalizers. `GlobalState::trace` deliberately
        // does NOT root these — that's how the post-mark hook below can
        // distinguish "still reachable from program state" from "only kept
        // alive by the finalizer registry."
        let weak_table_capacity = weak_tables_snapshot.len();
        let (pending_snapshot, thread_capacity, _interned_capacity): (
            Vec<FinalizerObject>,
            usize,
            usize,
        ) = {
            let g = state_ref.global.borrow();
            let pending = match mode {
                HeapCollectMode::Minor => g.finalizers.pending_minor_snapshot(),
                HeapCollectMode::Full | HeapCollectMode::Step => g.finalizers.pending_snapshot(),
            };
            (pending, g.threads.len(), g.interned_lt.len())
        };
        let finalizer_capacity = pending_snapshot.len();

        let alive_ids: std::cell::RefCell<std::collections::HashSet<usize>> =
            std::cell::RefCell::new(std::collections::HashSet::new());
        let newly_unreachable: std::cell::RefCell<Vec<FinalizerObject>> =
            std::cell::RefCell::new(Vec::new());
        let alive_thread_ids: std::cell::RefCell<std::collections::HashSet<u64>> =
            std::cell::RefCell::new(std::collections::HashSet::new());
        let alive_closure_env_ids: std::cell::RefCell<std::collections::HashSet<usize>> =
            std::cell::RefCell::new(std::collections::HashSet::new());
        let dead_interned: std::cell::RefCell<Vec<(u32, usize)>> = std::cell::RefCell::new(Vec::new());
        let collect_ran = std::cell::Cell::new(false);

        {
            let global = state_ref.global.borrow();
            global.heap.unpause();
            let roots = CollectRoots {
                global: &*global,
                thread: state_ref,
            };
            let hook = |marker: &mut lua_gc::Marker| {
                collect_ran.set(true);
                alive_ids.borrow_mut().reserve(weak_table_capacity);
                newly_unreachable.borrow_mut().reserve(finalizer_capacity);
                alive_thread_ids.borrow_mut().reserve(thread_capacity);
                trace_reachable_threads(&*global, global.current_thread_id, marker);
                close_open_upvalues_for_unreachable_threads(&*global, marker);
                // Lua 5.1 weak-key tables are NOT ephemerons. In 5.1
                // `traversetable` (lgc.c) a `__mode='k'` table still marks its
                // VALUES strongly (`if (!weakvalue) markvalue(g, gval(n))`), so
                // a value that references its own weak key keeps that key alive
                // (`a[t]=t` survives). Ephemeron semantics — a value reachable
                // only if the key is independently reachable — arrived in 5.2.
                let legacy_weak_key_values =
                    matches!(global.lua_version, lua_types::LuaVersion::V51);
                loop {
                    let visited_before = marker.visited_count();
                    for t in &weak_tables_snapshot.ephemeron {
                        if !marker.is_marked_or_old(t.0) {
                            continue;
                        }
                        if legacy_weak_key_values {
                            let mut to_mark = Vec::new();
                            t.for_each_entry(|_k, v| to_mark.push(v.clone()));
                            for v in &to_mark {
                                v.trace(marker);
                            }
                        } else {
                            let to_mark = t.ephemeron_values_to_mark_with_value(&|v| {
                                lua_value_marked_or_old(marker, v)
                            });
                            for v in &to_mark {
                                v.trace(marker);
                            }
                        }
                    }
                    marker.drain_gray_queue();
                    if marker.visited_count() == visited_before {
                        break;
                    }
                }
                // Clear dead weak VALUES before finalizer resurrection,
                // mirroring C's `clearvalues(g->weak, NULL)` /
                // `clearvalues(g->allweak, NULL)` which run in `atomic`
                // *before* `separatetobefnz`/`markbeingfnz` (lua-5.3.6
                // lgc.c). An object that is only reachable through a
                // to-be-finalized table is still white at this point, so a
                // weak-value reference to it is dropped — and stays dropped
                // even after resurrection re-marks the object — so the
                // finalizer observes the already-cleared slot. Keys are left
                // untouched here; they are cleared post-resurrection.
                //
                // C iterates the *entire* weak list regardless of table mark
                // state, so a weak-value table reachable only via a soon-to-be
                // resurrected object (its `__gc` closure stored weakly is the
                // canonical case) still has its dead values dropped here, not
                // after resurrection re-marks the table. The mark check only
                // gates whether surviving value-strings are re-marked: strings
                // are values weak tables never collect, but only when the
                // owning table itself is reachable.
                for t in weak_tables_snapshot
                    .weak_values
                    .iter()
                    .chain(weak_tables_snapshot.all_weak.iter())
                {
                    let to_mark = t.prune_weak_dead_with_value(
                        &|_| true,
                        &|v| lua_value_marked_or_old(marker, v),
                    );
                    if marker.is_marked_or_old(t.0) {
                        for v in &to_mark {
                            v.trace(marker);
                        }
                    }
                }
                marker.drain_gray_queue();
                for pf in &pending_snapshot {
                    if !finalizer_marked_or_old(marker, pf) {
                        pf.mark(marker);
                        newly_unreachable.borrow_mut().push(pf.clone());
                    }
                }
                marker.drain_gray_queue();
                loop {
                    let visited_before = marker.visited_count();
                    for t in &weak_tables_snapshot.ephemeron {
                        if !marker.is_marked_or_old(t.0) {
                            continue;
                        }
                        if legacy_weak_key_values {
                            let mut to_mark = Vec::new();
                            t.for_each_entry(|_k, v| to_mark.push(v.clone()));
                            for v in &to_mark {
                                v.trace(marker);
                            }
                        } else {
                            let to_mark = t.ephemeron_values_to_mark_with_value(&|v| {
                                lua_value_marked_or_old(marker, v)
                            });
                            for v in &to_mark {
                                v.trace(marker);
                            }
                        }
                    }
                    marker.drain_gray_queue();
                    if marker.visited_count() == visited_before {
                        break;
                    }
                }
                // Clear dead weak KEYS post-resurrection, mirroring C's
                // `clearkeys(g->ephemeron, NULL)` / `clearkeys(g->allweak,
                // NULL)`. A key kept alive only by a resurrected (now-marked)
                // object survives; a key pending its own finalization is
                // already marked by `markbeingfnz`, so `marked_or_old`
                // keeps it visible until its `__gc` runs. Values in these
                // originally-tracked tables were already settled in the
                // pre-resurrection value pass and must not be re-evaluated
                // (resurrection can re-mark a value that was correctly
                // cleared), so this pass touches keys only — matching the
                // `origweak` skip in C's later `clearvalues(g->weak,
                // origweak)`.
                for t in weak_tables_snapshot
                    .ephemeron
                    .iter()
                    .chain(weak_tables_snapshot.all_weak.iter())
                {
                    if !marker.is_marked_or_old(t.0) {
                        continue;
                    }
                    let to_mark = t.prune_weak_dead_with_value(
                        &|v| lua_value_marked_or_old(marker, v),
                        &|_| true,
                    );
                    for v in &to_mark {
                        v.trace(marker);
                    }
                }
                for t in weak_snapshot_tables(&weak_tables_snapshot) {
                    if marker.is_marked_or_old(t.0) {
                        alive_ids.borrow_mut().insert(t.identity());
                    }
                }
                marker.drain_gray_queue();
                {
                    let mut alive = alive_thread_ids.borrow_mut();
                    for (id, entry) in global.threads.iter() {
                        if thread_entry_marked_alive(marker, *id, entry) {
                            alive.insert(*id);
                        }
                    }
                }
                {
                    let mut alive = alive_closure_env_ids.borrow_mut();
                    for id in global.closure_envs.keys() {
                        if marker.is_visited(*id) {
                            alive.insert(*id);
                        }
                    }
                }
                record_dead_interned_strings(&*global, marker, &dead_interned);
            };
            match mode {
                HeapCollectMode::Full => global.heap.full_collect_with_post_mark(&roots, hook),
                HeapCollectMode::Step => global.heap.step_with_post_mark(&roots, hook),
                HeapCollectMode::Minor => global.heap.minor_collect_with_post_mark(&roots, hook),
            }
        }

        if !collect_ran.get() {
            return;
        }

        // After collect, drop weak-table-registry entries whose target was
        // swept. This keeps the registry bounded and avoids retaining weak
        // handles whose target can no longer upgrade.
        let alive_set = alive_ids.into_inner();
        let promote: Vec<FinalizerObject> = newly_unreachable.into_inner();
        let alive_thread_ids = alive_thread_ids.into_inner();
        let alive_closure_env_ids = alive_closure_env_ids.into_inner();
        let dead_interned = dead_interned.into_inner();
        let mut g = state_ref.global.borrow_mut();
        remove_dead_interned_strings(&mut *g, dead_interned);
        g.weak_tables_registry.retain_identities(&alive_set);
        let main_thread_id = g.main_thread_id;
        g.threads.retain(|id, _| alive_thread_ids.contains(id));
        g.cross_thread_upvals
            .retain(|(id, _), _| *id == main_thread_id || alive_thread_ids.contains(id));
        // Lua 5.1 side maps (empty on 5.2–5.5): drop a coroutine's per-thread
        // global table once the coroutine is gone, and a closure's stored
        // environment once that closure is no longer visited.
        g.thread_globals
            .retain(|id, _| alive_thread_ids.contains(id));
        g.closure_envs
            .retain(|id, _| alive_closure_env_ids.contains(id));
        // Move newly-unreachable finalizables from `pending_finalizers` to
        // `to_be_finalized`. The latter is rooted by `GlobalState::trace`,
        // so these tables remain alive until their `__gc` runs.
        let promoted = g.finalizers.promote_pending_to_finalized(promote);
        for object in &promoted {
            if let Some(ptr) = object.heap_ptr() {
                g.heap.move_finobj_to_tobefnz(ptr);
            }
        }
        if matches!(mode, HeapCollectMode::Minor) {
            g.finalizers.finish_minor_collection();
        }
    }

    /// Run one generational collection step.
    pub fn generational_step(&self) -> bool {
        self.generational_step_with_major(true)
    }

    /// Run a generational step forced to the regular minor path.
    ///
    /// Used for `collectgarbage("step", 0)`: upstream `genstep` treats
    /// `GCdebt <= 0` as an explicit zero-size step and performs a minor
    /// collection, unless a previous bad major has already armed `lastatomic`.
    pub fn generational_step_minor_only(&self) -> bool {
        self.generational_step_with_major(false)
    }

    fn generational_step_with_major(&self, allow_major: bool) -> bool {
        let (lastatomic, majorbase, majorinc, should_major) = {
            let g = self._state.global();
            let majorbase = if g.gc_estimate == 0 {
                g.total_bytes()
            } else {
                g.gc_estimate
            };
            let majormul = g.gc_genmajormul_param().max(0) as usize;
            let majorinc = (majorbase / 100).saturating_mul(majormul);
            let debt_due = g.gc_debt() > 0 || g.heap.would_collect();
            let should_major =
                allow_major && debt_due && g.total_bytes() > majorbase.saturating_add(majorinc);
            (g.lastatomic, majorbase, majorinc, should_major)
        };

        if lastatomic != 0 {
            self.stepgenfull(lastatomic);
            debug_assert!(self._state.global().is_gen_mode());
            return true;
        }

        if should_major {
            let numobjs = self.fullgen();
            let after = self._state.global().total_bytes();
            if after < majorbase.saturating_add(majorinc / 2) {
                self.set_minor_debt();
            } else {
                {
                    let mut g = self._state.global_mut();
                    g.lastatomic = numobjs.max(1);
                }
                self.set_pause_debt();
            }
        } else {
            self.collect_via_heap_mode(HeapCollectMode::Minor);
            self.set_minor_debt();
            self._state.global_mut().gc_estimate = majorbase;
        }

        debug_assert!(self._state.global().is_gen_mode());
        true
    }

    /// No-op stub for `luaC_step(L)`; unused. See [`GcHandle::check_step`]
    /// for the real incremental-collection entry point.
    pub fn step(&self) { /* phase-b no-op */
    }

    /// Run one budgeted incremental step of the GC.
    ///
    /// `work_units` is the number of GC work units the step is allowed to
    /// perform (one gray trace, one sweep visit, or one phase transition).
    /// Returns `true` if the step completed a cycle and the collector is
    /// now in the `Pause` state; `false` otherwise.
    ///
    /// Mirrors `collect_via_heap` for the post-mark weak-table /
    /// finalizer-promotion logic, but only the atomic-phase transition will
    /// invoke the snapshot-walking hook — propagate and sweep steps reuse
    /// the snapshot but never execute it. The snapshot is rebuilt on every
    /// call; the cost is `O(weak_tables_registry)` per step.
    pub fn incremental_step(&self, work_units: isize) -> bool {
        self.incremental_step_to_state(work_units, None)
    }

    /// TestC/debug helper: run the incremental collector until a specific heap
    /// state is entered, preserving the same weak-table/finalizer post-mark
    /// hooks as [`Self::incremental_step`]. This is intentionally not used for
    /// normal pacing; it exists so official tests can inspect mid-cycle colors.
    pub fn run_until_gc_state_for_test(&self, target: lua_gc::GcState) -> bool {
        self.incremental_step_to_state(isize::MAX / 4, Some(target));
        self._state.global().heap.gc_state() == target
    }

    fn incremental_step_to_state(
        &self,
        work_units: isize,
        target: Option<lua_gc::GcState>,
    ) -> bool {
        use lua_gc::{StepBudget, StepOutcome, Trace};
        let state_ref: &LuaState = &*self._state;

        let weak_tables_snapshot: lua_gc::WeakRegistrySnapshot<GcRef<LuaTable>> = {
            let mut g = state_ref.global.borrow_mut();
            g.weak_tables_registry.live_snapshot_by_kind()
        };

        let weak_table_capacity = weak_tables_snapshot.len();
        let (pending_snapshot, thread_capacity, _interned_capacity): (
            Vec<FinalizerObject>,
            usize,
            usize,
        ) = {
            let g = state_ref.global.borrow();
            (
                g.finalizers.pending_snapshot(),
                g.threads.len(),
                g.interned_lt.len(),
            )
        };
        let finalizer_capacity = pending_snapshot.len();

        let alive_ids: std::cell::RefCell<std::collections::HashSet<usize>> =
            std::cell::RefCell::new(std::collections::HashSet::new());
        let newly_unreachable: std::cell::RefCell<Vec<FinalizerObject>> =
            std::cell::RefCell::new(Vec::new());
        let alive_thread_ids: std::cell::RefCell<std::collections::HashSet<u64>> =
            std::cell::RefCell::new(std::collections::HashSet::new());
        let alive_closure_env_ids: std::cell::RefCell<std::collections::HashSet<usize>> =
            std::cell::RefCell::new(std::collections::HashSet::new());
        let dead_interned: std::cell::RefCell<Vec<(u32, usize)>> = std::cell::RefCell::new(Vec::new());
        let atomic_ran = std::cell::Cell::new(false);

        let stop_target = {
            let g = state_ref.global.borrow();
            match (target, g.heap.gc_state()) {
                (Some(target), _) => Some(target),
                (None, lua_gc::GcState::CallFin) => None,
                (None, _) => Some(lua_gc::GcState::CallFin),
            }
        };

        let outcome = {
            let global = state_ref.global.borrow();
            global.heap.unpause();
            let roots = CollectRoots {
                global: &*global,
                thread: state_ref,
            };
            let hook = |marker: &mut lua_gc::Marker| {
                atomic_ran.set(true);
                alive_ids.borrow_mut().reserve(weak_table_capacity);
                newly_unreachable.borrow_mut().reserve(finalizer_capacity);
                alive_thread_ids.borrow_mut().reserve(thread_capacity);
                trace_reachable_threads(&*global, global.current_thread_id, marker);
                close_open_upvalues_for_unreachable_threads(&*global, marker);
                // Lua 5.1 weak-key tables are NOT ephemerons; see the
                // full-collect hook above. `__mode='k'` still marks its values
                // strongly so a value referencing its own weak key survives.
                let legacy_weak_key_values =
                    matches!(global.lua_version, lua_types::LuaVersion::V51);
                loop {
                    let visited_before = marker.visited_count();
                    for t in &weak_tables_snapshot.ephemeron {
                        let t_id = t.identity();
                        if !marker.is_visited(t_id) {
                            continue;
                        }
                        if legacy_weak_key_values {
                            let mut to_mark = Vec::new();
                            t.for_each_entry(|_k, v| to_mark.push(v.clone()));
                            for v in &to_mark {
                                v.trace(marker);
                            }
                        } else {
                            let to_mark = t.ephemeron_values_to_mark(&|id| marker.is_visited(id));
                            for v in &to_mark {
                                v.trace(marker);
                            }
                        }
                    }
                    marker.drain_gray_queue();
                    if marker.visited_count() == visited_before {
                        break;
                    }
                }
                // Clear dead weak VALUES before finalizer resurrection,
                // mirroring C's `clearvalues(g->weak, NULL)` /
                // `clearvalues(g->allweak, NULL)` which run *before*
                // `markbeingfnz` in `atomic` (lua-5.3.6 lgc.c). See the
                // full-collect hook above for the rationale; this is the
                // incremental atomic and is the path the in-mode gc.lua
                // weak-table-plus-finalizer test exercises. C iterates the
                // entire weak list regardless of table mark state, so a
                // weak-value table reachable only via a soon-to-be-resurrected
                // object (canonically a `__gc` closure stored weakly) still
                // has its dead values dropped here. The visited check only
                // gates re-marking surviving value-strings. Keys untouched.
                for t in weak_tables_snapshot
                    .weak_values
                    .iter()
                    .chain(weak_tables_snapshot.all_weak.iter())
                {
                    let to_mark =
                        t.prune_weak_dead_with(&|_| true, &|id| marker.is_visited(id));
                    if marker.is_visited(t.identity()) {
                        for v in &to_mark {
                            v.trace(marker);
                        }
                    }
                }
                marker.drain_gray_queue();
                for pf in &pending_snapshot {
                    if !marker.is_visited(pf.identity()) {
                        pf.mark(marker);
                        newly_unreachable.borrow_mut().push(pf.clone());
                    }
                }
                marker.drain_gray_queue();
                loop {
                    let visited_before = marker.visited_count();
                    for t in &weak_tables_snapshot.ephemeron {
                        let t_id = t.identity();
                        if !marker.is_visited(t_id) {
                            continue;
                        }
                        if legacy_weak_key_values {
                            let mut to_mark = Vec::new();
                            t.for_each_entry(|_k, v| to_mark.push(v.clone()));
                            for v in &to_mark {
                                v.trace(marker);
                            }
                        } else {
                            let to_mark = t.ephemeron_values_to_mark(&|id| marker.is_visited(id));
                            for v in &to_mark {
                                v.trace(marker);
                            }
                        }
                    }
                    marker.drain_gray_queue();
                    if marker.visited_count() == visited_before {
                        break;
                    }
                }
                // Clear dead weak KEYS post-resurrection, mirroring C's
                // `clearkeys(g->ephemeron, NULL)` / `clearkeys(g->allweak,
                // NULL)`. Values in these originally-tracked tables were
                // already settled in the pre-resurrection pass and must not
                // be re-evaluated, so this pass is keys-only (the `origweak`
                // skip in C's later `clearvalues(g->weak, origweak)`).
                for t in weak_tables_snapshot
                    .ephemeron
                    .iter()
                    .chain(weak_tables_snapshot.all_weak.iter())
                {
                    if !marker.is_visited(t.identity()) {
                        continue;
                    }
                    let to_mark =
                        t.prune_weak_dead_with(&|id| marker.is_visited(id), &|_| true);
                    for v in &to_mark {
                        v.trace(marker);
                    }
                }
                for t in weak_snapshot_tables(&weak_tables_snapshot) {
                    if marker.is_visited(t.identity()) {
                        alive_ids.borrow_mut().insert(t.identity());
                    }
                }
                marker.drain_gray_queue();
                {
                    let mut alive = alive_thread_ids.borrow_mut();
                    for (id, entry) in global.threads.iter() {
                        if thread_entry_marked_alive(marker, *id, entry) {
                            alive.insert(*id);
                        }
                    }
                }
                {
                    let mut alive = alive_closure_env_ids.borrow_mut();
                    for id in global.closure_envs.keys() {
                        if marker.is_visited(*id) {
                            alive.insert(*id);
                        }
                    }
                }
                record_dead_interned_strings(&*global, marker, &dead_interned);
            };
            let budget = StepBudget::from_work(work_units);
            if let Some(target) = stop_target {
                global
                    .heap
                    .incremental_run_until_state_with_post_mark(&roots, target, work_units, hook)
            } else {
                global
                    .heap
                    .incremental_step_with_post_mark(&roots, budget, hook)
            }
        };

        if atomic_ran.get() {
            let alive_set = alive_ids.into_inner();
            let promote: Vec<FinalizerObject> = newly_unreachable.into_inner();
            let alive_thread_ids = alive_thread_ids.into_inner();
            let alive_closure_env_ids = alive_closure_env_ids.into_inner();
            let dead_interned = dead_interned.into_inner();
            let mut g = state_ref.global.borrow_mut();
            remove_dead_interned_strings(&mut *g, dead_interned);
            g.weak_tables_registry.retain_identities(&alive_set);
            let main_thread_id = g.main_thread_id;
            g.threads.retain(|id, _| alive_thread_ids.contains(id));
            g.cross_thread_upvals
                .retain(|(id, _), _| *id == main_thread_id || alive_thread_ids.contains(id));
            // Lua 5.1 side maps (empty on 5.2–5.5): drop a coroutine's
            // per-thread global table once the coroutine is gone, and a
            // closure's stored environment once that closure is unvisited.
            g.thread_globals
                .retain(|id, _| alive_thread_ids.contains(id));
            g.closure_envs
                .retain(|id, _| alive_closure_env_ids.contains(id));
            let promoted = g.finalizers.promote_pending_to_finalized(promote);
            for object in &promoted {
                if let Some(ptr) = object.heap_ptr() {
                    g.heap.move_finobj_to_tobefnz(ptr);
                }
            }
        }

        let mut paused = matches!(outcome, StepOutcome::Paused);
        if target.is_none()
            && self._state.global().heap.gc_state() == lua_gc::GcState::CallFin
            && !self._state.global().finalizers.has_to_be_finalized()
        {
            paused = self._state.global().heap.finish_callfin_phase();
        }

        paused
    }

    /// Run only the weak-table atomic cleanup used by legacy generational
    /// callers that need mark/prune behavior without sweeping.
    ///
    /// Explicit generational steps now use [`Self::generational_step`], which
    /// performs a young sweep. This helper remains for call sites that only
    /// need the weak-table atomic pass.
    pub fn prune_weak_tables_mark_only(&self) {
        use lua_gc::Trace;
        let state_ref: &LuaState = &*self._state;

        let weak_tables_snapshot: lua_gc::WeakRegistrySnapshot<GcRef<LuaTable>> = {
            let mut g = state_ref.global.borrow_mut();
            g.weak_tables_registry.live_snapshot_by_kind()
        };
        let _interned_capacity = {
            let g = state_ref.global.borrow();
            g.interned_lt.len()
        };

        let dead_interned: std::cell::RefCell<Vec<(u32, usize)>> = std::cell::RefCell::new(Vec::new());

        {
            let global = state_ref.global.borrow();
            global.heap.unpause();
            let roots = CollectRoots {
                global: &*global,
                thread: state_ref,
            };
            let hook = |marker: &mut lua_gc::Marker| {
                trace_reachable_threads(&*global, global.current_thread_id, marker);
                loop {
                    let visited_before = marker.visited_count();
                    for t in &weak_tables_snapshot.ephemeron {
                        let t_id = t.identity();
                        if !marker.is_visited(t_id) {
                            continue;
                        }
                        let to_mark = t.ephemeron_values_to_mark(&|id| marker.is_visited(id));
                        for v in &to_mark {
                            v.trace(marker);
                        }
                    }
                    marker.drain_gray_queue();
                    if marker.visited_count() == visited_before {
                        break;
                    }
                }
                for t in weak_snapshot_tables(&weak_tables_snapshot) {
                    if marker.is_visited(t.identity()) {
                        let to_mark = t.prune_weak_dead(&|id| marker.is_visited(id));
                        for v in &to_mark {
                            v.trace(marker);
                        }
                    }
                }
                marker.drain_gray_queue();
                record_dead_interned_strings(&*global, marker, &dead_interned);
            };
            global.heap.mark_only_with_post_mark(&roots, hook);
        }

        let dead_interned = dead_interned.into_inner();
        let mut g = state_ref.global.borrow_mut();
        remove_dead_interned_strings(&mut *g, dead_interned);
    }

    /// Set the GC kind (incremental/generational).
    pub fn change_mode(&self, mode: GcKind) {
        let old = self._state.global().gckind;
        if old == mode as u8 {
            self._state.global_mut().lastatomic = 0;
            return;
        }
        match mode {
            GcKind::Generational => {
                self.enter_generational_mode();
            }
            GcKind::Incremental => {
                self.enter_incremental_mode();
            }
        }
    }

    /// No-op stub for `luaC_fix(L, o)` (pin an object so GC won't collect
    /// it). Called from `tagmethods.rs` for interned tag-method name
    /// strings, which stay alive via the traced `GlobalState::tmname` root
    /// instead.
    pub fn fix_object<T: lua_gc::Trace + 'static>(&self, _o: &GcRef<T>) { /* phase-b no-op */
    }

    /// Free all collectable objects deterministically (called during state
    /// teardown by `close_state`), mirroring C-Lua `lua_close`'s guarantee
    /// that finalizers and frees complete before the call returns.
    ///
    /// Object destruction historically rode on `GlobalState`'s eventual `Rc`
    /// drop, so `close_state` could return with every heap object still live
    /// (issue #260). This now drains the heap through
    /// [`Heap::drop_all`](lua_gc::Heap::drop_all) — every destructor runs
    /// before this returns — and clears the `GlobalState`-side GC bookkeeping
    /// (weak-table registry, finalizer registry, external roots) whose entries
    /// would otherwise name freed boxes. `drop_all` clears the allocation-token
    /// map, so a pre-existing `GcWeak` stops upgrading immediately, and leaves
    /// the heap `closed`, so a `HeapGuard` that outlived close panics instead
    /// of resurrecting a torn-down heap.
    ///
    /// This does not *run* finalizers: `close_state` calls
    /// `api::run_close_finalizers` before invoking this on the complete-state
    /// branch (mirroring C's `luaC_freeallobjects` = `separatetobefnz` +
    /// `callallpendingfinalizers` + free); this only frees.
    ///
    /// A [`HeapGuard`](lua_gc::HeapGuard) for **this state's own heap** is
    /// pushed around the drain: a destructor that allocates via `GcRef::new`
    /// resolves the top of the TLS guard stack, so without the push its box
    /// would land in whatever heap the caller happened to have active — a
    /// different VM's heap under a foreign guard, or a panic with no guard at
    /// all. With it, teardown-time allocations always land in (and are drained
    /// from) the heap being closed, regardless of ambient guard state.
    /// The registry clears below also break the `GlobalState` ↔ coroutine
    /// reference cycle: each [`ThreadRegistryEntry`] holds an
    /// `Rc<RefCell<LuaState>>` whose `LuaState.global` is a strong
    /// `Rc<RefCell<GlobalState>>` back-edge, so a live suspended coroutine at
    /// close would otherwise keep the `GlobalState` — and its `Rc<Heap>` —
    /// alive forever after the outer state drops. The sibling cross-thread
    /// maps hold only `Copy` `GcRef` handles (no cycle), but those handles
    /// dangle once `drop_all` frees the boxes, so they are cleared with it.
    pub fn free_all_objects(&self) {
        let heap = self._state.global().heap.clone();
        {
            let _own_heap = lua_gc::HeapGuard::push(&heap);
            heap.drop_all();
        }
        let mut g = self._state.global_mut();
        g.weak_tables_registry = lua_gc::WeakRegistry::default();
        g.finalizers = lua_gc::FinalizerRegistry::default();
        g.external_roots = ExternalRootSet::default();
        g.threads.clear();
        g.thread_globals.clear();
        g.cross_thread_upvals.clear();
        g.suspended_parent_stacks.clear();
        g.suspended_parent_open_upvals.clear();
        g.twups.clear();
    }

    /// GC write barrier for a TValue.
    pub fn barrier(&self, p: &dyn std::any::Any, v: &LuaValue) {
        let g = self._state.global();
        barrier_any(&g.heap, p, v, g.is_gen_mode(), BarrierKind::Forward);
    }

    /// Backward write barrier.
    pub fn barrier_back(&self, p: &dyn std::any::Any, v: &LuaValue) {
        let g = self._state.global();
        barrier_any(&g.heap, p, v, g.is_gen_mode(), BarrierKind::Backward);
    }

    /// Typed table backward barrier for table mutation hot paths.
    pub fn table_barrier_back(&self, p: &GcRef<LuaTable>, v: &LuaValue) {
        let g = self._state.global();
        barrier_lua_value(&g.heap, *p, v, g.is_gen_mode(), BarrierKind::Backward);
    }

    /// Object write barrier.
    pub fn obj_barrier(&self, p: &dyn std::any::Any, o: &dyn std::any::Any) {
        let g = self._state.global();
        barrier_any(&g.heap, p, o, g.is_gen_mode(), BarrierKind::Forward);
    }

    /// Backward object write barrier.
    ///
    pub fn obj_barrier_back(&self, p: &dyn std::any::Any, o: &dyn std::any::Any) {
        let g = self._state.global();
        barrier_any(&g.heap, p, o, g.is_gen_mode(), BarrierKind::Backward);
    }
}

// ─── Functions from lstate.c ──────────────────────────────────────────────────

/// C's `luai_makeseed` mixes ASLR entropy (pointer addresses of a heap var,
/// stack var, and code symbol) with the current time via `luaS_hash`. Raw
/// pointer addresses require `unsafe`, which is forbidden outside
/// lua-gc/lua-coro, so native builds use time-only entropy for now — a
/// pointer- or `getrandom`-based source would meaningfully improve
/// hash-DoS resistance and remains unaddressed. Bare WASM uses a fixed
/// seed so state creation never touches a stubbed host clock.
fn make_seed() -> u32 {
    #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
    {
        return crate::string::hash_bytes(b"lua-rs-wasm-seed", 0x9e37_79b9);
    }

    #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
    {
        use std::time::{SystemTime, UNIX_EPOCH};
        let t = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as u32)
            .unwrap_or(0);

        crate::string::hash_bytes(&t.to_le_bytes(), t)
    }
}

/// Adjust the compatibility `GCdebt` value against the collector-owned live
/// byte count.
pub(crate) fn set_debt(g: &mut GlobalState, mut debt: isize) {
    let tb = g.total_bytes() as isize;
    debug_assert!(tb > 0);
    // isize::MAX stands in for C's MAX_LMEM ceiling.
    if debt < tb.saturating_sub(isize::MAX) {
        debt = tb - isize::MAX;
    }
    g.gc_debt = debt;
}

/// Deprecated no-op that returns `LUAI_MAXCCALLS`.
pub fn set_c_stack_limit(_state: &mut LuaState, _limit: u32) -> i32 {
    let _ = (_state, _limit);
    LUAI_MAXCCALLS as i32
}

/// Allocate a fresh `CallInfo` beyond the current frame and return its index.
pub(crate) fn extend_ci(state: &mut LuaState) -> CallInfoIdx {
    debug_assert!(
        state.call_info[state.ci.0 as usize].next.is_none(),
        "extend_ci: current ci already has a cached next frame"
    );

    let current_idx = state.ci;
    // Pushes onto the Vec rather than a separate heap allocation (`luaM_new`).
    let new_idx = CallInfoIdx(state.call_info.len() as u32);

    state.call_info.push(CallInfo {
        previous: Some(current_idx),
        next: None,
        u: CallInfoFrame::lua_default(),
        ..CallInfo::default()
    });

    state.call_info[current_idx.0 as usize].next = Some(new_idx);

    state.nci += 1;

    new_idx
}

/// Free all cached (unused) `CallInfo` frames beyond the current frame.
///
/// In C, each `CallInfo` is an independent heap allocation freed by
/// `luaM_free`. Here, all `CallInfo` entries live in
/// `state.call_info: Vec<CallInfo>`: this walks the link chain to count
/// removals (updating `nci`), then truncates the Vec — safe as long as all
/// free entries have indices greater than `state.ci`.
fn free_ci(state: &mut LuaState) {
    let ci_idx = state.ci.0 as usize;

    let mut next_opt = state.call_info[ci_idx].next.take();

    while let Some(idx) = next_opt {
        next_opt = state.call_info[idx.0 as usize].next;
        state.nci = state.nci.saturating_sub(1);
    }

    // Truncate: drop all entries beyond the current ci. Assumes all cached
    // frames have contiguous indices greater than state.ci; not verified.
    state.call_info.truncate(ci_idx + 1);
}

/// Free approximately half of the cached `CallInfo` frames beyond the current frame.
///
/// The C code removes every other node from the free-list chain by pointer
/// manipulation. Removing elements from the middle of a `Vec` here would
/// shift subsequent elements and invalidate `CallInfoIdx` values that point
/// past the removal site, so this instead approximates by halving the free
/// count via truncation — an O(n) copy for the drop where a slab allocator
/// supporting O(1) removal without index invalidation would do better.
pub(crate) fn shrink_ci(state: &mut LuaState) {
    let ci_idx = state.ci.0 as usize;

    if state.call_info[ci_idx].next.is_none() {
        return;
    }

    let free_count = state.call_info.len().saturating_sub(ci_idx + 1);
    if free_count <= 1 {
        return;
    }

    // Remove every other cached frame (halve the free list).
    let keep = free_count / 2;
    let removed = free_count - keep;
    let new_len = ci_idx + 1 + keep;
    state.call_info.truncate(new_len);
    state.nci = state.nci.saturating_sub(removed as u32);

    // Terminate the now-last cached frame.
    if let Some(last) = state.call_info.last_mut() {
        last.next = None;
    }
}

/// Check whether the C-call depth has reached its limit and raise an error if so.
pub(crate) fn check_c_stack(state: &mut LuaState) -> Result<(), LuaError> {
    // Mirrors luaG_runerror.
    if state.c_calls() == LUAI_MAXCCALLS {
        return Err(LuaError::runtime(format_args!("C stack overflow")));
    }
    // Mirrors luaD_throw(L, LUA_ERRERR).
    if state.c_calls() >= (LUAI_MAXCCALLS / 10 * 11) {
        return Err(LuaError::with_status(LuaStatus::ErrErr));
    }
    Ok(())
}

/// Increment the C-call depth counter, checking for overflow.
pub fn inc_c_stack(state: &mut LuaState) -> Result<(), LuaError> {
    state.n_ccalls += 1;
    if state.c_calls() >= LUAI_MAXCCALLS {
        check_c_stack(state)?;
    }
    Ok(())
}

/// In C, `L` is a separate thread used only for memory allocation (via
/// `luaM_newvector`). There is no custom allocator here; all allocation
/// goes through the global Rust allocator, so this takes only the new
/// thread (`thread`) and ignores the caller.
fn stack_init(thread: &mut LuaState) {
    let total_slots = BASIC_STACK_SIZE + EXTRA_STACK;
    thread.stack = vec![StackValue::default(); total_slots];

    // In C, tbclist.p = stack.p is a sentinel meaning "no tbc vars"; here
    // the Vec is simply empty when there are no tbc variables.
    thread.tbclist = Vec::new();

    // The new stack slots are already initialized to LuaValue::Nil via
    // StackValue::default().

    thread.top = StackIdx(0);

    thread.stack_last = StackIdx(BASIC_STACK_SIZE as u32);

    let base_ci = CallInfo {
        func: StackIdx(0),
        top: StackIdx(1 + LUA_MINSTACK as u32),
        previous: None,
        next: None,
        callstatus: CIST_C,
        call_metamethods: 0,
        tailcalls: 0,
        nresults: 0,
        u: CallInfoFrame::c_default(),
        u2: CallInfoExtra::default(),
    };

    if thread.call_info.is_empty() {
        thread.call_info.push(base_ci);
    } else {
        thread.call_info[0] = base_ci;
        thread.call_info.truncate(1);
    }

    thread.stack[0] = StackValue {
        val: LuaValue::Nil,
    };

    thread.top = StackIdx(1);

    thread.ci = CallInfoIdx(0);
}

fn free_stack(state: &mut LuaState) {
    if state.stack.is_empty() {
        return;
    }
    state.ci = CallInfoIdx(0);
    free_ci(state);
    debug_assert_eq!(state.nci, 0, "nci should be 0 after free_ci");
    state.stack.clear();
    state.stack.shrink_to_fit();
}

/// Populate the registry table's fixed slots and the globals/loaded shadow
/// fields.
///
/// Mirrors C's `init_registry`, which sets `registry[LUA_RIDX_MAINTHREAD]`
/// to the main thread and `registry[LUA_RIDX_GLOBALS]` to the globals table
/// (`reference/lua-5.4.7/src/lstate.c`). Both have equivalents here:
/// `GlobalState::main_thread_value` is a `GcRef<LuaThread>` — a lightweight
/// identity handle (just a `u64` id), not the actual `LuaState` — built once
/// in `new_state` specifically so it can be shared without the
/// self-referential-`Rc` problem that would come from storing a
/// `GcRef<LuaState>` pointing at the state's own `GlobalState`; the globals
/// table is kept as a direct `GlobalState::globals` field for hot-path
/// lookups (`get_global_table`/`push_globals` read it there) and is *also*
/// mirrored into `registry[LUA_RIDX_GLOBALS]` here for C-API fidelity, so
/// `debug.getregistry()[2]` returns `_G`. `loaded` stays a direct field
/// only. Which numeric indices apply is version-dependent — see
/// [`set_versioned_registry_slots`].
///
/// The globals/loaded tables are created before that call so the globals
/// handle is available to write into the registry. Runs at the pre-override
/// default version (`new_state` always constructs with
/// `LuaVersion::default()`); a version-selecting entry point
/// (`bootstrap_state`, the CLI) calls [`set_versioned_registry_slots`] again
/// once the real version is known. Both calls happen during bootstrap,
/// before any user code — see that function's startup-only contract.
fn init_registry(state: &mut LuaState) -> Result<(), LuaError> {
    let registry = state.new_table();
    state.global_mut().l_registry = LuaValue::Table(registry);

    let globals = state.new_table();
    state.global_mut().globals = LuaValue::Table(globals);
    let loaded = state.new_table();
    state.global_mut().loaded = LuaValue::Table(loaded);

    set_versioned_registry_slots(state)?;

    Ok(())
}

/// Populate the version-dependent numeric registry slots — the main thread
/// and the globals table — for the current [`lua_types::LuaVersion`].
///
/// Which index holds what changed across the Lua line, so this is gated on
/// the active version (all indices verified against the reference binaries):
///
/// - **5.1** — no numeric registry convenience indices at all; globals live
///   in the per-thread environment (`fenv`), not the registry, and there is
///   no `LUA_RIDX_MAINTHREAD`. `debug.getregistry()` has no numeric keys.
/// - **5.2 / 5.3 / 5.4** — `registry[1]` is `LUA_RIDX_MAINTHREAD` (the main
///   thread) and `registry[2]` is `LUA_RIDX_GLOBALS` (the globals table).
///   `registry[3]` is the `luaL_ref` free-list head (`freelist =
///   LUA_RIDX_LAST + 1`; see `lua_ref` in lua-stdlib) and is NOT written
///   here.
/// - **5.5** — `LUA_RIDX_MAINTHREAD` was renumbered to `3`; `registry[2]` is
///   still the globals table; and `registry[1]` is initialized to `false`,
///   which is the `luaL_ref` free-list head's "uninitialized" sentinel in C
///   5.5 (`luaL_ref` accepts `nil` or `false` there before its first use),
///   not a placeholder reserved for some future index.
///
/// STARTUP-ONLY. This must run during VM bootstrap, before any user code or
/// `luaL_ref` call touches the registry. It is called twice in practice
/// (once from `init_registry` at the default version, once from the
/// version-selecting entry point once the real version is set). To make that
/// second call safe it clears a slot only when the slot still holds a value
/// *this function itself wrote* at the earlier default version (the
/// main-thread handle or the globals table) — never an arbitrary entry, so a
/// `luaL_ref` free-list head would survive. It is NOT a general reset and is
/// NOT safe to call after `luaL_ref`/user code has begun using the registry:
/// the 5.x set paths still overwrite `registry[1..=3]` with fixed values,
/// which post-startup could clobber a live reference. Do not expose it as
/// such.
pub fn set_versioned_registry_slots(state: &mut LuaState) -> Result<(), LuaError> {
    let registry = match state.global().l_registry.clone() {
        LuaValue::Table(t) => t,
        _ => return Ok(()),
    };
    let main_thread = LuaValue::Thread(state.global().main_thread_value.clone());
    let globals = state.global().globals.clone();
    let version = state.global().lua_version;

    for idx in [1i64, LUA_RIDX_GLOBALS, 3] {
        let cur = registry.get_int(idx);
        if cur == main_thread || cur == globals {
            registry.raw_set_int(state, idx, LuaValue::Nil)?;
        }
    }

    match version {
        lua_types::LuaVersion::V51 => {}
        lua_types::LuaVersion::V55 => {
            registry.raw_set_int(state, 1, LuaValue::Bool(false))?;
            registry.raw_set_int(state, LUA_RIDX_GLOBALS, globals)?;
            registry.raw_set_int(state, 3, main_thread)?;
        }
        _ => {
            registry.raw_set_int(state, LUA_RIDX_MAINTHREAD, main_thread)?;
            registry.raw_set_int(state, LUA_RIDX_GLOBALS, globals)?;
        }
    }
    Ok(())
}

/// Pre-allocated out-of-memory message, built once at startup so the GC can
/// always hand back a valid error string even when the allocator is failing.
const MEMERR_MSG: &[u8] = b"not enough memory";

/// Build the bootstrap out-of-memory message and pre-fill every API string
/// cache slot with it.
///
/// Must run once during bootstrap, before any allocation can fail. Goes
/// through [`LuaState::intern_str`] — the same interning path every other
/// Lua string in the VM uses — rather than a separate bootstrap-only string
/// table, so `GlobalState::memerrmsg` is an ordinary member of
/// `GlobalState::interned_lt` and the `GcRef::ptr_eq` identity check in
/// `api::lua_error` keeps working unchanged.
///
/// C fixes the message in the GC (`luaC_fix`, marking it non-collectable);
/// there is no equivalent call here (`state.gc().fix_object` is a no-op —
/// see its doc in state.rs). The message instead stays alive for the life
/// of the process simply because `GlobalState::memerrmsg` holds a
/// permanent strong reference to it.
fn init_memerrmsg(state: &mut LuaState) -> Result<(), LuaError> {
    let memerrmsg = state.intern_str(MEMERR_MSG)?;
    state.global_mut().memerrmsg = memerrmsg.clone();

    for i in 0..STRCACHE_N {
        for j in 0..STRCACHE_M {
            state.global_mut().strcache[i][j] = memerrmsg.clone();
        }
    }

    Ok(())
}

/// Does not call `lua_lex::init`: that function pre-interns the reserved
/// words, mirroring C's `luaX_init`, but this port's keyword recognition
/// matches scanned bytes directly against `LUAX_TOKENS` rather than relying
/// on identity/tagging of a pre-created string (see `lua_lex::init`'s doc),
/// and every reserved word is re-interned anyway the first time it is
/// scanned. The call has no observable lexical/parse effect and no startup
/// correctness dependency, so it is unnecessary here, not missing.
fn lua_open(state: &mut LuaState) -> Result<(), LuaError> {
    stack_init(state);
    init_registry(state)?;
    init_memerrmsg(state)?;
    crate::tagmethods::init(state)?;
    state.global_mut().gcstp = 0;
    state.global().heap.unpause();
    // Setting nilvalue = Nil signals completestate() → is_complete() = true.
    state.global_mut().nilvalue = LuaValue::Nil;
    Ok(())
}

fn preinit_thread(thread: &mut LuaState, global: Rc<RefCell<GlobalState>>) {
    thread.global = global;
    thread.stack = Vec::new();
    thread.call_info = Vec::new();
    // ci is initialized to 0 but call_info is empty; stack_init() must be
    // called before any use of call_info.
    thread.ci = CallInfoIdx(0);
    thread.nci = 0;
    // In C, L->twups = L is a self-reference sentinel meaning "no open
    // upvals". Here, GlobalState.twups is a Vec<GcRef<LuaState>>; absence
    // from that Vec is the sentinel, so there is no per-thread twups field.
    thread.n_ccalls = 0;
    thread.hook = None;
    thread.hookmask = 0;
    thread.basehookcount = 0;
    thread.allowhook = true;
    thread.hookcount = thread.basehookcount;

    // Sandbox inheritance: a coroutine joins the runtime-wide instruction/memory
    // budget so metering spans every thread, not just the main one. The budget
    // itself lives in `GlobalState` (shared); the new thread only needs the
    // count-hook mask armed so the dispatch loop traps and charges it.
    {
        let (active, interval) = {
            let g = thread.global.borrow();
            (g.sandbox_active(), g.sandbox.interval.get())
        };
        if active {
            thread.hookmask = SANDBOX_COUNT_MASK;
            thread.basehookcount = interval;
            thread.hookcount = interval;
        }
    }
    thread.openupval = Vec::new();
    thread.status = LuaStatus::Ok as u8;
    thread.errfunc = 0;
    thread.oldpc = 0;
    thread.gc_check_needed = true;
}

/// Tear down a state: run the remaining close-time `__gc` finalizers, then
/// free every heap object, mirroring C's `close_state` → `luaC_freeallobjects`
/// (= `separatetobefnz` + `callallpendingfinalizers` + free).
///
/// On the complete-state branch, `api::run_close_finalizers` fires the
/// finalizers BEFORE `free_all_objects` tears the boxes down (issue #260).
/// `run_close_finalizers` drains the pending registry via `take_pending`, so
/// it is idempotent — a host that already ran it (lua-cli's `pmain` does, and
/// then never calls `close`) sees a no-op here, never a double-run. The
/// incomplete branch (`lua_open` failed mid-bootstrap) has no registered
/// finalizers, so it goes straight to the free, as C does.
fn close_state(state: &mut LuaState) {
    let is_complete = state.global().is_complete();

    if !is_complete {
        state.gc().free_all_objects();
    } else {
        state.ci = CallInfoIdx(0);
        crate::api::run_close_finalizers(state);
        state.gc().free_all_objects();
    }

    free_stack(state);

    // Rust's allocator (via Drop) handles deallocation of GlobalState and
    // LuaState automatically; there is no custom-allocator free call here.
}

/// Allocate a fresh coroutine `LuaState`, register it under a new
/// `ThreadId`, and push the resulting `LuaValue::Thread(value)` onto
/// `state`'s stack.
///
/// If `initial_body` is `Some(f)`, `f` is also pushed onto the new
/// thread's stack so that `coroutine.status` reports `"suspended"`
/// rather than `"dead"`. `co_create` uses this to stage the body function
/// without a full stack transfer between threads.
pub fn new_thread(state: &mut LuaState, initial_body: Option<LuaValue>) -> Result<(), LuaError> {
    state.gc_pre_collect_clear();
    state.gc().check_step();

    // Unlike C, where the new thread is GC-allocated as part of the allgc
    // list, this creates a plain `LuaState` owned by `Rc<RefCell<>>` outside
    // the traced heap; reachability while suspended is instead handled by
    // `trace_reachable_threads` and the cross-thread snapshot fields on
    // `GlobalState` (see their field docs).

    let global_rc = state.global_rc();
    let hookmask = state.hookmask;
    let basehookcount = state.basehookcount;

    let reserved_id = {
        let mut g = state.global_mut();
        let id = g.next_thread_id;
        g.next_thread_id += 1;
        id
    };

    // Lua 5.1: a coroutine inherits its creator's global table (`l_gt`) at
    // creation, after which the two are independent. Seed the new thread's
    // `thread_globals` slot with the running thread's current `l_gt` so the
    // coroutine's `setfenv(0, t)` and freshly-loaded chunks resolve through
    // its own slot, never the main thread's `globals`. Inert on 5.2–5.5.
    if matches!(state.global().lua_version, lua_types::LuaVersion::V51) {
        let creator_id = state.global().current_thread_id;
        let creator_lgt = state.v51_thread_lgt(creator_id);
        state
            .global_mut()
            .thread_globals
            .insert(reserved_id, creator_lgt);
    }

    let mut new_thread = LuaState {
        status: LuaStatus::Ok as u8,
        allowhook: true,
        nci: 0,
        top: StackIdx(0),
        stack_last: StackIdx(0),
        stack: Vec::new(),
        ci: CallInfoIdx(0),
        call_info: Vec::new(),
        openupval: Vec::new(),
        legacy_open_upval_touched: std::cell::Cell::new(false),
        tbclist: Vec::new(),
        global: global_rc.clone(),
        hook: None,
        hookmask: 0,
        basehookcount: 0,
        hookcount: 0,
        errfunc: 0,
        n_ccalls: 0,
        oldpc: 0,
        marked: 0,
        cached_thread_id: reserved_id,
        gc_check_needed: false,
    };

    preinit_thread(&mut new_thread, global_rc);

    new_thread.hookmask = hookmask;
    new_thread.basehookcount = basehookcount;
    // The hook function itself does not propagate to the new thread: `hook`
    // is a non-`Clone` `Box<dyn FnMut(...)>`, so only the mask/counts above
    // are copied; sharing the closure would need an `Arc<Mutex<...>>`.
    new_thread.reset_hook_count();

    stack_init(&mut new_thread);

    if let Some(body) = initial_body {
        new_thread.push(body);
    }

    let thread_ref: Rc<RefCell<LuaState>> = Rc::new(RefCell::new(new_thread));

    let value = {
        let mut g = state.global_mut();
        let id = reserved_id;
        let value = GcRef::new(lua_types::value::LuaThread::new(id));
        g.threads.insert(
            id,
            ThreadRegistryEntry {
                state: thread_ref,
                value: value.clone(),
            },
        );
        value
    };

    state.push(LuaValue::Thread(value));

    Ok(())
}

/// Reset a thread to its base state, closing all to-be-closed variables.
///
/// Returns the final status code as an `i32` (mirrors the C API).
///
/// Mirrors C's `luaE_resetthread` (`reference/lua-5.4.7/src/lstate.c`) line
/// for line, including its final `luaD_reallocstack(L, ci->top.p -
/// L->stack.p, 0)` call — routed here through `crate::do_::realloc_stack`
/// with `raise_error = false`, matching C's `raiseerror = 0`. That routes
/// through the OOM/emergency-GC-stop path (the point of issue #275 item 4)
/// and updates `stack_last`, which the old raw `Vec::resize` left stale.
///
/// `realloc_stack` shrinks the logical stack length back down but, because
/// it goes through `Vec::resize_with`, leaves the backing allocation's
/// capacity at the high-water mark — a deliberate high-water policy that is
/// correct for the hot grow/shrink paths but means memory a coroutine grew
/// deep for is not returned. C's `luaM_reallocvector` reallocates to exactly
/// the new size, releasing that memory, so this cold reset path follows the
/// realloc with an explicit `shrink_to_fit` to reclaim the capacity too.
/// (`reset_thread` runs on coroutine close/reset, never in a hot loop, so
/// the extra realloc is not a concern.)
pub fn reset_thread(state: &mut LuaState, status: i32) -> i32 {
    // Public low-level entry point: the __close error path under
    // close_protected() materializes error messages, which requires an
    // active heap (issue #253 review). Self-sufficient like lua_resume.
    let _heap_guard = {
        let g = state.global.borrow();
        lua_gc::HeapGuard::push(&g.heap)
    };
    state.ci = CallInfoIdx(0);
    let ci_idx = 0usize;

    if !state.stack.is_empty() {
        state.stack[0].val = LuaValue::Nil;
    }

    state.call_info[ci_idx].func = StackIdx(0);
    state.call_info[ci_idx].call_metamethods = 0;
    state.call_info[ci_idx].callstatus = CIST_C;

    let mut status = if status == LuaStatus::Yield as i32 {
        LuaStatus::Ok as i32
    } else {
        status
    };

    state.status = LuaStatus::Ok as u8;

    let close_status = crate::do_::close_protected(state, StackIdx(1), LuaStatus::from_raw(status));
    status = close_status as i32;

    if status != LuaStatus::Ok as i32 {
        crate::do_::set_error_obj(state, LuaStatus::from_raw(status), StackIdx(1));
    } else {
        state.top = StackIdx(1);
    }

    let new_ci_top = StackIdx(state.top.0 + LUA_MINSTACK as u32);
    state.call_info[ci_idx].top = new_ci_top;

    let _ = crate::do_::realloc_stack(state, new_ci_top.0 as usize, false);
    state.stack.shrink_to_fit();

    status
}

/// Close a coroutine thread from the perspective of another thread.
pub fn close_thread(state: &mut LuaState, from: Option<&LuaState>) -> i32 {
    state.n_ccalls = match from {
        Some(f) => f.c_calls(),
        None => 0,
    };
    let current_status = state.status as i32;
    let result = reset_thread(state, current_status);
    result
}

/// Deprecated wrapper for `close_thread(L, NULL)`.
pub fn reset_thread_api(state: &mut LuaState) -> i32 {
    close_thread(state, None)
}

/// Create a new independent Lua state.  Returns `None` only on OOM.
///
/// The C API takes a custom allocator `(f, ud)`; the Rust-native API uses
/// the global Rust allocator instead, so those parameters are dropped.
/// Equivalent to `LuaState::new()` at the call site.
pub fn new_state() -> Option<LuaState> {
    // In Rust, allocation failure panics by default; we use Result internally.

    // Build a dummy LuaString for memerrmsg and strcache initialization.
    // This is a chicken-and-egg problem: GlobalState.memerrmsg needs to be initialized
    // before init_memerrmsg runs, but init_memerrmsg is what creates the real one.
    // The heap is created ahead of the GlobalState literal so these two
    // pre-state allocations (the placeholder string and the main thread
    // value) can live on its heap-owned uncollected list instead of falling
    // through GcRef::new's detached arm — this was the last per-VM
    // process-lifetime leak, and removing it lets LUA_RS_GC_STRICT_GUARD
    // police the detached arm with zero sanctioned exceptions. Moving the
    // heap into the literal below is safe: the owner-list heads are Cells
    // whose GcBox pointees are stable heap allocations.
    let heap = lua_gc::Heap::new();
    let placeholder_str = GcRef(heap.allocate_uncollected(LuaString::placeholder()));
    let main_thread_value = GcRef(heap.allocate_uncollected(lua_types::value::LuaThread::new(0)));

    let initial_white = 1u8 << WHITE0BIT;

    // A non-nil nilvalue signals "state not yet complete"; see is_complete().

    let global = GlobalState {
        parser_hook: None,
        cli_argv: None,
        cli_preload: None,
        lua_version: lua_types::LuaVersion::default(),
        file_loader_hook: None,
        file_open_hook: None,
        stdout_hook: None,
        stderr_hook: None,
        stdin_hook: None,
        env_hook: None,
        unix_time_hook: None,
        cpu_clock_hook: None,
        local_offset_hook: None,
        entropy_hook: None,
        temp_name_hook: None,
        popen_hook: None,
        io_streams: None,
        file_remove_hook: None,
        file_rename_hook: None,
        os_execute_hook: None,
        dynlib_load_hook: None,
        dynlib_symbol_hook: None,
        dynlib_unload_hook: None,
        sandbox: SandboxLimits::default(),
        gc_debt: 0,
        gc_estimate: 0,
        lastatomic: 0,
        l_registry: LuaValue::Nil,
        external_roots: ExternalRootSet::default(),
        globals: LuaValue::Nil,
        loaded: LuaValue::Nil,
        nilvalue: LuaValue::Int(0),
        seed: make_seed(),
        currentwhite: initial_white,
        gcstate: GCS_PAUSE,
        gckind: GcKind::Incremental as u8,
        gcstopem: false,
        genminormul: LUAI_GENMINORMUL,
        genmajormul: (LUAI_GENMAJORMUL / 4) as u8,
        gcstp: GCSTPGC,
        gcemergency: false,
        gcpause: (LUAI_GCPAUSE / 4) as u8,
        gcstepmul: (LUAI_GCMUL / 4) as u8,
        gcstepsize: LUAI_GCSTEPSIZE,
        // Lua 5.5 collectgarbage("param") defaults, observed on lua5.5.0:
        // [minormul, majorminor, minormajor, pause, stepmul, stepsize].
        gc55_params: [20, 50, 68, 250, 200, 9600],
        sweepgc_cursor: 0,
        weak_tables_registry: lua_gc::WeakRegistry::default(),
        finalizers: lua_gc::FinalizerRegistry::default(),
        gc_finalizer_error: None,
        twups: Vec::new(),
        panic: None,
        mainthread: None,
        threads: std::collections::HashMap::new(),
        main_thread_value,
        current_thread_id: 0,
        closing_thread_id: None,
        main_thread_id: 0,
        next_thread_id: 1,
        thread_globals: std::collections::HashMap::new(),
        closure_envs: std::collections::HashMap::new(),
        memerrmsg: placeholder_str.clone(),
        tmname: Vec::new(),
        mt: std::array::from_fn(|_| None),
        strcache: std::array::from_fn(|_| std::array::from_fn(|_| placeholder_str.clone())),
        interned_lt: InternedStringMap::default(),
        warnf: None,
        warn_mode: WarnMode::Off,
        test_warn_enabled: false,
        test_warn_on: false,
        test_warn_mode: TestWarnMode::Normal,
        test_warn_last_to_cont: false,
        test_warn_buffer: Vec::new(),
        c_functions: Vec::new(),
        heap,
        cross_thread_upvals: std::collections::HashMap::new(),
        suspended_parent_stacks: Vec::new(),
        suspended_parent_open_upvals: Vec::new(),
        snapshot_stack_pool: Vec::new(),
        snapshot_upval_pool: Vec::new(),
        resume_value_pool: Vec::new(),
        resume_upval_slot_pool: Vec::new(),
        resume_flush_pool: Vec::new(),
    };

    let global_rc = Rc::new(RefCell::new(global));

    // issue #249: from here through the end of lua_open() below (registry,
    // string pool, tagmethod tables), route allocations onto the heap's
    // bootstrap ("uncollected but heap-owned") list rather than the normal
    // collectable list. The object graph isn't self-consistent for root
    // tracing yet, and lua_open() clears `paused` partway through, so an
    // allocation-triggered step during setup must not sweep these objects.
    // Heap::drop_all() still frees them when the Lua instance drops — they
    // just never leak past that, unlike the previous `Gc::new_uncollected`
    // fallback. RAII scope: the lua_open error return below cannot leave the
    // heap stuck in bootstrap mode. Callers that continue bootstrapping
    // (stdlib install) open a nested window of their own; callers that use
    // new_state() directly (low-level GC tests) get a heap that allocates
    // normally once this scope drops at return.
    let _bootstrap_scope = global_rc.borrow().heap.bootstrap_scope();
    let _bootstrap_guard = lua_gc::HeapGuard::push(&global_rc.borrow().heap);

    let initial_marked = initial_white;

    let mut main_thread = LuaState {
        status: LuaStatus::Ok as u8,
        allowhook: true,
        nci: 0,
        top: StackIdx(0),
        stack_last: StackIdx(0),
        stack: Vec::new(),
        ci: CallInfoIdx(0),
        call_info: Vec::new(),
        openupval: Vec::new(),
        legacy_open_upval_touched: std::cell::Cell::new(false),
        tbclist: Vec::new(),
        global: global_rc.clone(),
        hook: None,
        hookmask: 0,
        basehookcount: 0,
        hookcount: 0,
        errfunc: 0,
        n_ccalls: 0,
        oldpc: 0,
        marked: initial_marked,
        cached_thread_id: 0,
        gc_check_needed: false,
    };

    preinit_thread(&mut main_thread, global_rc.clone());

    main_thread.inc_nny();

    // `mainthread` is left `None` here — see its field doc on `GlobalState`.

    // `lua_open` returns a `Result`, which already gives the same protection
    // C gets from wrapping `f_luaopen` in `luaD_rawrunprotected` (a setjmp/
    // longjmp catch point); calling it directly and matching the result is
    // the Rust-native equivalent, not a missing wrapper.
    match lua_open(&mut main_thread) {
        Ok(()) => {}
        Err(_) => {
            close_state(&mut main_thread);
            return None;
        }
    }

    Some(main_thread)
}

/// Close the Lua state and free all resources.
///
/// In C, `lua_close` gets the main thread via `G(L)->mainthread` and closes
/// that regardless of which thread is passed. Here, callers must pass the
/// main `LuaState` directly; this does not traverse to the main thread —
/// the caller owns the root state — and does not assert that `state` is
/// indeed the main thread before closing it.
pub fn close(mut state: LuaState) {
    close_state(&mut state);
}

/// Forward a warning message through the configured warning sink.
pub(crate) fn warning(state: &mut LuaState, msg: &[u8], to_cont: bool) {
    let test_warn_enabled = state.global().test_warn_enabled;
    if test_warn_enabled {
        test_warn(state, msg, to_cont);
        return;
    }

    // The warnf closure is taken out of GlobalState (and restored after the
    // call) rather than invoked while holding a borrow, so a closure that
    // calls back into Lua does not hit a re-entrant borrow_mut() panic.
    let has_warnf = state.global().warnf.is_some();
    if has_warnf {
        // Take the warnf closure out to avoid re-entrant borrow.
        let mut warnf = state.global_mut().warnf.take();
        if let Some(ref mut f) = warnf {
            f(msg, to_cont);
        }
        // Restore the closure.
        state.global_mut().warnf = warnf;
        return;
    }
    default_warn(state, msg, to_cont);
}

fn test_warn(state: &mut LuaState, msg: &[u8], to_cont: bool) {
    let is_control = {
        let g = state.global();
        !g.test_warn_last_to_cont && !to_cont && msg.first() == Some(&b'@')
    };
    if is_control {
        let mut g = state.global_mut();
        match &msg[1..] {
            b"off" => g.test_warn_on = false,
            b"on" => g.test_warn_on = true,
            b"normal" => g.test_warn_mode = TestWarnMode::Normal,
            b"allow" => g.test_warn_mode = TestWarnMode::Allow,
            b"store" => g.test_warn_mode = TestWarnMode::Store,
            _ => {}
        }
        return;
    }

    let finished = {
        let mut g = state.global_mut();
        g.test_warn_last_to_cont = to_cont;
        g.test_warn_buffer.extend_from_slice(msg);
        if to_cont {
            None
        } else {
            Some((
                std::mem::take(&mut g.test_warn_buffer),
                g.test_warn_mode,
                g.test_warn_on,
            ))
        }
    };

    let Some((message, mode, warn_on)) = finished else {
        return;
    };
    match mode {
        TestWarnMode::Normal => {
            if warn_on && message.first() == Some(&b'#') {
                write_warning_message(&message);
            }
        }
        TestWarnMode::Allow => {
            if warn_on {
                write_warning_message(&message);
            }
        }
        TestWarnMode::Store => {
            if let Ok(s) = state.intern_str(&message) {
                state.push(LuaValue::Str(s));
                let _ = crate::api::set_global(state, b"_WARN");
            }
        }
    }
}

fn write_warning_message(message: &[u8]) {
    use std::io::Write;
    let stderr = std::io::stderr();
    let mut h = stderr.lock();
    let _ = h.write_all(b"Lua warning: ");
    let _ = h.write_all(message);
    let _ = h.write_all(b"\n");
}

/// The default warning handler: a faithful port of the `warnfoff` /
/// `warnfon` / `warnfcont` chain in upstream `lauxlib.c`. State is held in
/// `GlobalState::warn_mode` (C threads it via `lua_setwarnf`); output goes to
/// stderr (`lua_writestringerror`).
fn default_warn(state: &mut LuaState, msg: &[u8], to_cont: bool) {
    use std::io::Write;
    // checkcontrol: a leading-`@` non-continuation message is a control word.
    if !to_cont && msg.first() == Some(&b'@') {
        match &msg[1..] {
            b"off" => state.global_mut().warn_mode = WarnMode::Off,
            b"on" => state.global_mut().warn_mode = WarnMode::On,
            _ => {}
        }
        return;
    }
    let mode = state.global().warn_mode;
    match mode {
        WarnMode::Off => {}
        WarnMode::On | WarnMode::Cont => {
            let stderr = std::io::stderr();
            let mut h = stderr.lock();
            if mode == WarnMode::On {
                let _ = h.write_all(b"Lua warning: ");
            }
            let _ = h.write_all(msg);
            if to_cont {
                state.global_mut().warn_mode = WarnMode::Cont;
            } else {
                let _ = h.write_all(b"\n");
                state.global_mut().warn_mode = WarnMode::On;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_noop_cclosure(_: &mut LuaState) -> Result<usize, LuaError> {
        Ok(0)
    }

    /// Issue #278: `new_userdata` (the untyped `lua_newuserdatauv` allocation
    /// step) builds a real full userdata rather than erroring. It carries
    /// `size` payload bytes, `nuvalue` nil user-value slots, and no metatable.
    #[test]
    fn new_userdata_builds_untyped_full_userdata() {
        let mut state = new_state().expect("state should initialize");
        let _heap_guard = {
            let g = state.global();
            lua_gc::HeapGuard::push(&g.heap)
        };
        let ud = state
            .new_userdata(16, 2)
            .expect("new_userdata should build, not error");
        assert_eq!(ud.data.len(), 16);
        assert_eq!(ud.uv.borrow().len(), 2);
        assert!(ud.uv.borrow().iter().all(|v| matches!(v, LuaValue::Nil)));
        assert!(ud.metatable.borrow().is_none());
    }

    /// Issue #278: `new_c_closure` builds a light C function for `n == 0` and a
    /// full C closure with `n` nil upvalue slots for `n > 0`, rather than
    /// erroring.
    #[test]
    fn new_c_closure_builds_light_and_full_closures() {
        let mut state = new_state().expect("state should initialize");
        let _heap_guard = {
            let g = state.global();
            lua_gc::HeapGuard::push(&g.heap)
        };
        let light = state
            .new_c_closure(test_noop_cclosure, 0)
            .expect("light closure should build");
        assert!(
            matches!(light, LuaClosure::LightC(_)),
            "n==0 must yield a light C function"
        );

        let full = state
            .new_c_closure(test_noop_cclosure, 3)
            .expect("full closure should build");
        match full {
            LuaClosure::C(ccl) => {
                let ups = ccl.upvalues.borrow();
                assert_eq!(ups.len(), 3);
                assert!(ups.iter().all(|v| matches!(v, LuaValue::Nil)));
            }
            _ => panic!("n>0 must yield a full C closure"),
        }
    }

    /// Issue #278: `register_c_function` reuses an identical function's slot
    /// when `dedup` is set (the upvalue-less fast path) and always allocates a
    /// fresh slot otherwise (closures with upvalues must not share identity).
    #[test]
    fn register_c_function_dedups_only_when_requested() {
        let mut state = new_state().expect("state should initialize");
        let _heap_guard = {
            let g = state.global();
            lua_gc::HeapGuard::push(&g.heap)
        };
        let a = state.register_c_function(test_noop_cclosure, true);
        let b = state.register_c_function(test_noop_cclosure, true);
        assert_eq!(a, b, "dedup=true should reuse the identical fn's slot");
        let c = state.register_c_function(test_noop_cclosure, false);
        assert_ne!(a, c, "dedup=false must always allocate a fresh slot");
    }

    /// Issue #278: `to_display_string` (C's `luaL_tolstring`; the single
    /// stringifier `auxlib::to_lua_string`, `tostring`, `print` and
    /// `debug.debug` all route through) must render a table with its real
    /// per-value address, never the old fixed `"0x?"` placeholder, and two
    /// distinct tables must render distinct addresses.
    #[test]
    fn to_display_string_renders_distinct_real_table_addresses() {
        let mut state = new_state().expect("state should initialize");
        let _heap_guard = {
            let g = state.global();
            lua_gc::HeapGuard::push(&g.heap)
        };

        let t1 = state.new_table();
        state.push(LuaValue::Table(t1));
        let s1 = state
            .to_display_string(-1)
            .expect("to_display_string on table 1");

        let t2 = state.new_table();
        state.push(LuaValue::Table(t2));
        let s2 = state
            .to_display_string(-1)
            .expect("to_display_string on table 2");

        assert!(
            s1.starts_with(b"table: 0x"),
            "expected a real address, got {:?}",
            String::from_utf8_lossy(&s1)
        );
        assert!(
            !s1.windows(3).any(|w| w == b"0x?"),
            "the fixed '0x?' placeholder must be gone, got {:?}",
            String::from_utf8_lossy(&s1)
        );
        assert_ne!(s1, s2, "two distinct tables must render distinct addresses");
    }

    /// Issue #278 fix-round finding 1: the raw `new_userdata` / `new_c_closure`
    /// constructors allocate `GcRef`s directly, so they must flag a GC check
    /// (the #276-class pacing invariant) — otherwise the debt accumulates with
    /// no safe point noticing.
    #[test]
    fn raw_constructors_flag_gc_check_needed() {
        let mut state = new_state().expect("state should initialize");
        let _heap_guard = {
            let g = state.global();
            lua_gc::HeapGuard::push(&g.heap)
        };

        state.gc_check_needed = false;
        let _ = state.new_userdata(32, 1).expect("new_userdata");
        assert!(state.gc_check_needed, "new_userdata must flag a GC check");

        state.gc_check_needed = false;
        let _ = state
            .new_c_closure(test_noop_cclosure, 2)
            .expect("new_c_closure n>0");
        assert!(
            state.gc_check_needed,
            "new_c_closure(n>0) must flag a GC check"
        );
    }

    /// Issue #278 fix-round finding 2: light userdata renders with the plain
    /// `luaL_typename` "userdata" (as C's `luaL_tolstring` does), never the
    /// error-message-only "light userdata".
    #[test]
    fn to_display_string_light_userdata_uses_plain_userdata_name() {
        let mut state = new_state().expect("state should initialize");
        let _heap_guard = {
            let g = state.global();
            lua_gc::HeapGuard::push(&g.heap)
        };
        let ptr = 0x1234usize as *mut core::ffi::c_void;
        state.push(LuaValue::LightUserData(ptr));
        let s = state
            .to_display_string(-1)
            .expect("display light userdata");
        assert!(
            s.starts_with(b"userdata: 0x"),
            "expected 'userdata: 0x...', got {:?}",
            String::from_utf8_lossy(&s)
        );
        assert!(
            !s.starts_with(b"light userdata"),
            "must not use the error-message name 'light userdata'"
        );
    }

    /// Issue #278 fix-round finding 3: a light C function's pointer must be its
    /// real code address, never the tiny `c_functions` registry index.
    #[test]
    fn to_pointer_lightc_is_real_address_not_index() {
        let mut state = new_state().expect("state should initialize");
        let _heap_guard = {
            let g = state.global();
            lua_gc::HeapGuard::push(&g.heap)
        };
        let cl = state
            .new_c_closure(test_noop_cclosure, 0)
            .expect("light C function");
        let idx = match cl {
            LuaClosure::LightC(i) => i,
            _ => panic!("n==0 must yield LightC"),
        };
        state.push(LuaValue::Function(cl));
        let ptr = crate::api::to_pointer(&state, -1).expect("LightC must have a pointer");
        assert!(
            ptr > u16::MAX as usize,
            "expected a real address, got {ptr:#x} (registry index was {idx})"
        );
        assert_eq!(
            ptr, test_noop_cclosure as usize,
            "must resolve to the registered fn's real address"
        );
    }

    /// Issue #278 fix-round finding 6: `to_cfunction` resolves the registered
    /// bare fn for light C functions / C closures, and is `None` otherwise.
    #[test]
    fn to_cfunction_resolves_registered_fn() {
        let mut state = new_state().expect("state should initialize");
        let _heap_guard = {
            let g = state.global();
            lua_gc::HeapGuard::push(&g.heap)
        };
        let cl = state
            .new_c_closure(test_noop_cclosure, 0)
            .expect("light C function");
        state.push(LuaValue::Function(cl));
        let f = crate::api::to_cfunction(&state, -1).expect("to_cfunction must resolve");
        assert_eq!(f as usize, test_noop_cclosure as usize);

        state.push(LuaValue::Int(7));
        assert!(
            crate::api::to_cfunction(&state, -1).is_none(),
            "non-function must yield None"
        );
    }

    /// Issue #278 fix-round finding 5: `new_c_closure` validates the upvalue
    /// count against `0..=MAX_UPVAL` before registering anything, so an
    /// out-of-range count errors without polluting `c_functions`.
    #[test]
    fn new_c_closure_rejects_out_of_range_upvalue_count() {
        let mut state = new_state().expect("state should initialize");
        let _heap_guard = {
            let g = state.global();
            lua_gc::HeapGuard::push(&g.heap)
        };
        let before = state.global().c_functions.len();
        assert!(
            state.new_c_closure(test_noop_cclosure, 256).is_err(),
            "n>255 must error"
        );
        assert!(
            state.new_c_closure(test_noop_cclosure, -1).is_err(),
            "negative n must error"
        );
        assert_eq!(
            before,
            state.global().c_functions.len(),
            "an invalid upvalue count must not pollute c_functions"
        );
        assert!(
            state.new_c_closure(test_noop_cclosure, 255).is_ok(),
            "n==255 (MAX_UPVAL) must succeed"
        );
    }

    /// Issue #275 item 2 + item 3: the version-dependent numeric registry
    /// slots — `LUA_RIDX_MAINTHREAD` (the main thread) and `LUA_RIDX_GLOBALS`
    /// (`_G`) — must match C's `init_registry` for every version, and the
    /// slots that don't apply must be absent. Verified against the reference
    /// binaries: 5.1 has no numeric registry keys; 5.2/5.3/5.4 use index 1 =
    /// mainthread, index 2 = globals; 5.5 uses index 3 = mainthread, index 2
    /// = globals, index 1 = false (the `luaL_ref` free-list sentinel).
    /// Exercises the default-constructed state (V54), then V51, V55, V52 in
    /// turn — the same transitions the startup callers make.
    #[test]
    fn registry_versioned_slots_match_version() {
        let mut state = new_state().expect("state should initialize");
        let _heap_guard = {
            let g = state.global();
            lua_gc::HeapGuard::push(&g.heap)
        };

        let registry = match state.global().l_registry.clone() {
            LuaValue::Table(t) => t,
            other => panic!("l_registry should be a table, got {other:?}"),
        };
        let main_thread = LuaValue::Thread(state.global().main_thread_value.clone());
        let globals = state.global().globals.clone();
        assert!(matches!(globals, LuaValue::Table(_)), "globals should be a table");

        assert_eq!(registry.get_int(1), main_thread);
        assert_eq!(registry.get_int(LUA_RIDX_GLOBALS), globals);
        assert_eq!(registry.get_int(3), LuaValue::Nil);

        state.global_mut().lua_version = lua_types::LuaVersion::V51;
        set_versioned_registry_slots(&mut state).expect("V51 slot update should succeed");
        assert_eq!(registry.get_int(1), LuaValue::Nil);
        assert_eq!(registry.get_int(LUA_RIDX_GLOBALS), LuaValue::Nil);
        assert_eq!(registry.get_int(3), LuaValue::Nil);

        state.global_mut().lua_version = lua_types::LuaVersion::V55;
        set_versioned_registry_slots(&mut state).expect("V55 slot update should succeed");
        assert_eq!(registry.get_int(1), LuaValue::Bool(false));
        assert_eq!(registry.get_int(LUA_RIDX_GLOBALS), globals);
        assert_eq!(registry.get_int(3), main_thread);

        state.global_mut().lua_version = lua_types::LuaVersion::V52;
        set_versioned_registry_slots(&mut state).expect("V52 slot update should succeed");
        assert_eq!(registry.get_int(1), main_thread);
        assert_eq!(registry.get_int(LUA_RIDX_GLOBALS), globals);
        assert_eq!(registry.get_int(3), LuaValue::Nil);
    }

    /// Issue #275 item 2 (idempotence/safety): a startup re-run of
    /// `set_versioned_registry_slots` must never clobber a registry entry it
    /// did not write itself — in particular the `luaL_ref` free-list head,
    /// which lives at `registry[3]` in this port (`FREELIST_REF`) for
    /// 5.2-5.4. Simulates a foreign entry at index 3, re-runs the setter at
    /// the same version, and confirms the foreign entry survives while the
    /// main-thread/globals slots are still correct.
    #[test]
    fn versioned_registry_slots_preserve_foreign_entries_on_rerun() {
        let mut state = new_state().expect("state should initialize");
        let _heap_guard = {
            let g = state.global();
            lua_gc::HeapGuard::push(&g.heap)
        };

        let registry = match state.global().l_registry.clone() {
            LuaValue::Table(t) => t,
            other => panic!("l_registry should be a table, got {other:?}"),
        };
        let main_thread = LuaValue::Thread(state.global().main_thread_value.clone());
        let globals = state.global().globals.clone();

        registry
            .raw_set_int(&mut state, 3, LuaValue::Int(99))
            .expect("seeding a foreign registry entry should succeed");

        set_versioned_registry_slots(&mut state).expect("V54 re-run should succeed");

        assert_eq!(registry.get_int(3), LuaValue::Int(99));
        assert_eq!(registry.get_int(1), main_thread);
        assert_eq!(registry.get_int(LUA_RIDX_GLOBALS), globals);
    }

    /// Issue #275 item 4: `reset_thread` must route the post-close stack
    /// resize through `crate::do_::realloc_stack` rather than a raw
    /// `Vec::resize`, matching C's `luaE_resetthread`
    /// (`reference/lua-5.4.7/src/lstate.c`), whose final step is
    /// `luaD_reallocstack(L, ci->top.p - L->stack.p, 0)`. A raw resize only
    /// grows and never updates `stack_last`; this test first grows the stack
    /// well past a fresh reset's minimal size, then resets and checks that
    /// the logical length shrank, `stack_last` (`stack_size()`) reflects the
    /// new smaller size instead of the stale pre-reset one, AND the backing
    /// allocation's capacity was reclaimed (the `shrink_to_fit` that makes
    /// this faithful to C's `luaM_reallocvector` releasing memory — a plain
    /// `resize_with` would leave capacity at the high-water mark).
    #[test]
    fn reset_thread_shrinks_stack_and_updates_stack_last() {
        let mut state = new_state().expect("state should initialize");
        let _heap_guard = {
            let g = state.global();
            lua_gc::HeapGuard::push(&g.heap)
        };

        let grown_size = state.stack_size() + 500;
        crate::do_::realloc_stack(&mut state, grown_size, false)
            .expect("growing the stack should succeed");
        assert_eq!(state.stack_size(), grown_size);
        assert_eq!(state.stack.len(), grown_size + EXTRA_STACK);
        let grown_capacity = state.stack.capacity();

        let status = reset_thread(&mut state, LuaStatus::Ok as i32);
        assert_eq!(status, LuaStatus::Ok as i32);

        let expected_size = (state.top.0 + LUA_MINSTACK as u32) as usize;
        assert_eq!(
            state.stack_size(),
            expected_size,
            "stack_last should track reset_thread's realloc instead of the stale pre-reset size"
        );
        assert_eq!(
            state.stack.len(),
            expected_size + EXTRA_STACK,
            "reset_thread should shrink the stack length back down, matching C's luaE_resetthread"
        );
        assert!(
            state.stack.capacity() < grown_capacity,
            "reset_thread should reclaim the backing capacity (was {}, now {})",
            grown_capacity,
            state.stack.capacity()
        );
    }

    #[test]
    fn memerrmsg_is_the_same_object_a_matching_intern_str_call_returns() {
        let mut state = new_state().expect("state should initialize");
        let _heap_guard = {
            let g = state.global();
            lua_gc::HeapGuard::push(&g.heap)
        };

        let memerrmsg = state.global().memerrmsg.clone();
        let interned = state
            .intern_str(crate::string::MAX_SHORT_LEN.to_string().as_bytes())
            .expect("interning a short string should not fail");
        assert!(!GcRef::ptr_eq(&memerrmsg, &interned));

        let same_bytes = state
            .intern_str(memerrmsg.as_bytes())
            .expect("interning the memerrmsg bytes should not fail");
        assert!(
            GcRef::ptr_eq(&memerrmsg, &same_bytes),
            "memerrmsg must be a member of the same intern table as every other \
             short string, matching upstream Lua's `eqshrstr` identity semantics \
             (a script string spelling the OOM message aliases memerrmsg)"
        );
    }

    #[test]
    fn external_root_keys_reject_stale_slot_after_reuse() {
        let mut roots = ExternalRootSet::default();

        let first = roots.insert(LuaValue::Int(1));
        assert_eq!(roots.len(), 1);
        assert_eq!(roots.get(first), Some(&LuaValue::Int(1)));

        assert_eq!(roots.remove(first), Some(LuaValue::Int(1)));
        assert!(roots.get(first).is_none());
        assert!(roots.remove(first).is_none());
        assert_eq!(roots.len(), 0);
        assert_eq!(roots.vacant_len(), 1);
        assert!(roots.replace(first, LuaValue::Int(9)).is_none());
        assert!(roots.is_empty());

        let second = roots.insert(LuaValue::Int(2));
        assert_eq!(first.index, second.index);
        assert_ne!(first, second);
        assert!(roots.get(first).is_none());
        assert_eq!(roots.get(second), Some(&LuaValue::Int(2)));
        assert!(roots.replace(first, LuaValue::Int(3)).is_none());
    }

    #[test]
    fn external_roots_keep_heap_value_alive_until_unrooted() {
        let mut state = new_state().expect("state should initialize");
        let _heap_guard = {
            let g = state.global();
            lua_gc::HeapGuard::push(&g.heap)
        };

        let table = state.new_table();
        assert_eq!(state.global().heap.allgc_count(), 1);

        let key = state.external_root_value(LuaValue::Table(table));
        state.gc().full_collect();
        assert_eq!(state.global().heap.allgc_count(), 1);
        assert_eq!(state.global().external_roots.len(), 1);

        assert!(state.external_unroot_value(key).is_some());
        state.gc().full_collect();
        assert_eq!(state.global().heap.allgc_count(), 0);
        assert!(state.global().external_roots.is_empty());
    }

    /// Issue #260 repro at the VM surface: `free_all_objects` (the body of
    /// `close_state`) must free heap objects and invalidate a pre-existing
    /// `GcWeak` immediately, with an outer `HeapGuard` and a strong `Rc<Heap>`
    /// both still alive — proving the weak dies because close cleared the
    /// allocation tokens, not because the heap `Rc` was dropped.
    #[test]
    fn free_all_objects_kills_weak_handle_while_guard_and_heap_alive() {
        let mut state = new_state().expect("state should initialize");
        let heap_rc = state.global().heap.clone();
        let guard = lua_gc::HeapGuard::push(&heap_rc);

        let table = state.new_table();
        let weak = table.downgrade();
        assert!(
            weak.upgrade().is_some(),
            "weak handle upgrades while the table is live"
        );

        state.gc().free_all_objects();

        assert!(
            weak.upgrade().is_none(),
            "free_all_objects must free the table and drop its weak token during \
             the close call, with the outer HeapGuard and a strong heap Rc still alive"
        );
        assert!(state.global().heap.is_closed());
        assert!(std::rc::Rc::strong_count(&heap_rc) >= 1);

        drop(guard);
        drop(state);
    }

    thread_local! {
        static CLOSE_GC_RUNS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    fn close_gc_probe(_state: &mut LuaState) -> Result<usize, LuaError> {
        CLOSE_GC_RUNS.with(|c| c.set(c.get() + 1));
        Ok(0)
    }

    /// Build a table carrying a `__gc = gc_fn` metatable on `state`,
    /// registering it in the pending-finalizers list via the canonical
    /// `api::set_metatable` path, and leave the object table on the stack so
    /// it stays live to close.
    fn install_finalizable_table_with(state: &mut LuaState, gc_fn: LuaCFunction) {
        let heap = state.global().heap.clone();
        let _guard = lua_gc::HeapGuard::push(&heap);
        crate::api::create_table(state, 0, 0).expect("object table");
        crate::api::create_table(state, 0, 1).expect("metatable");
        crate::api::push_cclosure(state, gc_fn, 0).expect("push __gc");
        crate::api::set_field(state, -2, b"__gc").expect("mt.__gc");
        crate::api::set_metatable(state, -2).expect("setmetatable");
    }

    fn install_finalizable_table(state: &mut LuaState) {
        install_finalizable_table_with(state, close_gc_probe);
    }

    /// Codex finding 3 on issue #260: `state::close` must run close-time
    /// `__gc` finalizers (C `lua_close` → `luaC_freeallobjects` order) now
    /// that `free_all_objects` really frees — an object alive at close must
    /// observe its finalizer exactly once, before its box is torn down.
    #[test]
    fn state_close_runs_gc_finalizer_exactly_once() {
        CLOSE_GC_RUNS.with(|c| c.set(0));
        let mut state = new_state().expect("state should initialize");
        install_finalizable_table(&mut state);
        assert_eq!(
            CLOSE_GC_RUNS.with(|c| c.get()),
            0,
            "finalizer must not fire while the object is live"
        );

        close(state);
        assert_eq!(
            CLOSE_GC_RUNS.with(|c| c.get()),
            1,
            "close must run the pending __gc exactly once before freeing"
        );
    }

    /// The CLI composition half of finding 3: lua-cli's `pmain` calls
    /// `api::run_close_finalizers` itself before the state winds down. A
    /// subsequent `close_state` invocation must see the drained pending
    /// registry and not double-run the finalizer.
    #[test]
    fn manual_close_finalizer_pass_then_close_runs_gc_exactly_once() {
        CLOSE_GC_RUNS.with(|c| c.set(0));
        let mut state = new_state().expect("state should initialize");
        install_finalizable_table(&mut state);

        crate::api::run_close_finalizers(&mut state);
        assert_eq!(CLOSE_GC_RUNS.with(|c| c.get()), 1);

        close(state);
        assert_eq!(
            CLOSE_GC_RUNS.with(|c| c.get()),
            1,
            "close after a manual run_close_finalizers pass must not double-run __gc"
        );
    }

    /// Codex round 2 finding 3 on #260, queue-only half: leftovers parked in
    /// `to_be_finalized` by a bounded incremental pass must still run at
    /// close even when `pending` is empty — the old early-return on empty
    /// `pending` silently dropped them.
    #[test]
    fn close_runs_queue_only_finalizer_leftovers() {
        CLOSE_GC_RUNS.with(|c| c.set(0));
        let mut state = new_state().expect("state should initialize");
        install_finalizable_table(&mut state);

        {
            let mut g = state.global_mut();
            let pending: Vec<FinalizerObject> = g.finalizers.take_pending();
            assert!(!pending.is_empty());
            for object in pending.into_iter().rev() {
                let heap_ptr = object.heap_ptr();
                g.finalizers.push_to_be_finalized(object);
                if let Some(ptr) = heap_ptr {
                    g.heap.move_finobj_to_tobefnz(ptr);
                }
            }
            assert_eq!(g.finalizers.pending_len(), 0);
            assert!(g.finalizers.has_to_be_finalized());
        }

        close(state);
        assert_eq!(
            CLOSE_GC_RUNS.with(|c| c.get()),
            1,
            "a finalizer parked in to_be_finalized with pending empty must \
             still run at close"
        );
    }

    /// `__gc` probe that counts its run and re-registers ITS OWN object for
    /// finalization again (setmetatable with the same probe). Used to prove
    /// close's cross-pass exactly-once holds for queue-only objects: their
    /// identities enter `seen` via the entry seed, not via `take_pending`.
    fn self_reregister_gc_probe(state: &mut LuaState) -> Result<usize, LuaError> {
        CLOSE_GC_RUNS.with(|c| c.set(c.get() + 1));
        crate::api::push_value(state, 1);
        crate::api::create_table(state, 0, 1)?;
        crate::api::push_cclosure(state, self_reregister_gc_probe, 0)?;
        crate::api::set_field(state, -2, b"__gc")?;
        crate::api::set_metatable(state, -2)?;
        state.pop();
        Ok(0)
    }

    /// Codex round-3 finding on #260: a queue-only object (already parked in
    /// `to_be_finalized` when close starts) never passes through
    /// `take_pending`, so without seeding `seen` from `to_be_finalized` at
    /// entry, its self-re-registration would pass the seen-check and its
    /// `__gc` would run a second time. Must run exactly once.
    #[test]
    fn queue_only_self_reregistering_finalizer_runs_exactly_once() {
        CLOSE_GC_RUNS.with(|c| c.set(0));
        let mut state = new_state().expect("state should initialize");
        install_finalizable_table_with(&mut state, self_reregister_gc_probe);

        {
            let mut g = state.global_mut();
            let pending: Vec<FinalizerObject> = g.finalizers.take_pending();
            assert!(!pending.is_empty());
            for object in pending.into_iter().rev() {
                let heap_ptr = object.heap_ptr();
                g.finalizers.push_to_be_finalized(object);
                if let Some(ptr) = heap_ptr {
                    g.heap.move_finobj_to_tobefnz(ptr);
                }
            }
            assert_eq!(g.finalizers.pending_len(), 0);
            assert!(g.finalizers.has_to_be_finalized());
        }

        close(state);
        assert_eq!(
            CLOSE_GC_RUNS.with(|c| c.get()),
            1,
            "a self-re-registering queue-only finalizer must run exactly \
             once at close — seen must be seeded from to_be_finalized"
        );
    }

    fn regen_gc_probe(state: &mut LuaState) -> Result<usize, LuaError> {
        CLOSE_GC_RUNS.with(|c| c.set(c.get() + 1));
        crate::api::create_table(state, 0, 0)?;
        crate::api::create_table(state, 0, 1)?;
        crate::api::push_cclosure(state, close_gc_probe, 0)?;
        crate::api::set_field(state, -2, b"__gc")?;
        crate::api::set_metatable(state, -2)?;
        state.pop();
        Ok(0)
    }

    /// Codex round 2 finding 3 on #260, re-registration half: a `__gc` that
    /// registers a fresh finalizable object during the close drain must have
    /// that object's finalizer run by a later pass of the both-queue loop
    /// (once each — the cross-pass seen-set prevents re-running).
    #[test]
    fn finalizer_registering_new_finalizable_during_close_runs_it() {
        CLOSE_GC_RUNS.with(|c| c.set(0));
        let mut state = new_state().expect("state should initialize");
        install_finalizable_table_with(&mut state, regen_gc_probe);

        close(state);
        assert_eq!(
            CLOSE_GC_RUNS.with(|c| c.get()),
            2,
            "regen_gc_probe runs once, and the fresh finalizable it registered \
             during the drain runs exactly once on a later pass"
        );
    }

    /// Codex round 2 finding 5 on #260: `GlobalState::threads` holds
    /// `Rc<RefCell<LuaState>>` entries whose `.global` is a strong back-edge
    /// to the `GlobalState` — a live suspended coroutine at close is a
    /// reference cycle that would keep the heap alive forever after the
    /// outer state drops. `free_all_objects` must break it.
    #[test]
    fn close_with_live_coroutine_lets_heap_drop() {
        let mut state = new_state().expect("state should initialize");
        let heap_weak = std::rc::Rc::downgrade(&state.global().heap);
        {
            let heap = state.global().heap.clone();
            let _guard = lua_gc::HeapGuard::push(&heap);
            new_thread(&mut state, None).expect("coroutine creation");
        }
        assert_eq!(state.global().threads.len(), 1);

        close(state);
        assert!(
            heap_weak.upgrade().is_none(),
            "the heap must actually drop once the outer state drops after \
             close — a lingering threads-map cycle would keep it alive"
        );
    }

    /// Flips its flag when dropped; the inner payload for
    /// [`VmAllocOnDrop`]'s teardown-time allocation.
    struct VmDropFlag(std::rc::Rc<std::cell::Cell<bool>>);
    impl lua_gc::Trace for VmDropFlag {
        fn trace(&self, _m: &mut lua_gc::Marker) {}
    }
    impl Drop for VmDropFlag {
        fn drop(&mut self) {
            self.0.set(true);
        }
    }

    /// A payload whose `Drop` allocates through `GcRef::new` — the path every
    /// real VM destructor uses — so it resolves the top of the TLS guard
    /// stack. Pins codex finding 4 on issue #260: `free_all_objects` must
    /// push a guard for the state's own heap or this lands in the wrong heap
    /// (or panics with no ambient guard at all).
    struct VmAllocOnDrop {
        flag: std::rc::Rc<std::cell::Cell<bool>>,
        inner: std::rc::Rc<std::cell::Cell<bool>>,
    }
    impl lua_gc::Trace for VmAllocOnDrop {
        fn trace(&self, _m: &mut lua_gc::Marker) {}
    }
    impl Drop for VmAllocOnDrop {
        fn drop(&mut self) {
            self.flag.set(true);
            let _ = GcRef::new(VmDropFlag(self.inner.clone()));
        }
    }

    #[test]
    fn free_all_objects_provides_own_heap_guard_with_no_ambient_guard() {
        let mut state = new_state().expect("state should initialize");
        let heap = state.global().heap.clone();
        let outer = std::rc::Rc::new(std::cell::Cell::new(false));
        let inner = std::rc::Rc::new(std::cell::Cell::new(false));
        let _gc = heap.allocate(VmAllocOnDrop {
            flag: outer.clone(),
            inner: inner.clone(),
        });

        state.gc().free_all_objects();

        assert!(outer.get(), "the destructor itself must have run");
        assert!(
            inner.get(),
            "GcRef::new inside a teardown destructor must land in the closing \
             heap (via free_all_objects's own guard) and be drained, even with \
             no ambient HeapGuard"
        );
        assert!(heap.is_closed());
    }

    #[test]
    fn free_all_objects_targets_own_heap_under_foreign_guard() {
        let mut state1 = new_state().expect("state 1 should initialize");
        let state2 = new_state().expect("state 2 should initialize");
        let heap1 = state1.global().heap.clone();
        let heap2 = state2.global().heap.clone();
        let _foreign = lua_gc::HeapGuard::push(&heap2);

        let outer = std::rc::Rc::new(std::cell::Cell::new(false));
        let inner = std::rc::Rc::new(std::cell::Cell::new(false));
        let _gc = heap1.allocate(VmAllocOnDrop {
            flag: outer.clone(),
            inner: inner.clone(),
        });
        let heap2_baseline = heap2.allgc_count();

        state1.gc().free_all_objects();

        assert!(outer.get());
        assert!(
            inner.get(),
            "the teardown allocation must land in (and be drained from) the \
             heap being closed, not the foreign heap on the TLS guard stack"
        );
        assert_eq!(
            heap2.allgc_count(),
            heap2_baseline,
            "the foreign heap must be untouched by another state's close"
        );
        assert!(heap1.is_closed());
        assert!(!heap2.is_closed());
        drop(state2);
    }

    #[test]
    fn interned_string_table_grows_then_shrinks_on_collection() {
        let mut state = new_state().expect("state should initialize");
        let _heap_guard = {
            let g = state.global();
            lua_gc::HeapGuard::push(&g.heap)
        };

        let initial_buckets = state.global().interned_lt.bucket_count();
        assert_eq!(
            initial_buckets, 64,
            "the intern table starts at C's MINSTRTABSIZE-equivalent of 64"
        );
        let baseline_live = state.global().interned_lt.len();

        let mut anchors: Vec<GcRef<LuaString>> = Vec::with_capacity(2000);
        for i in 0..2000usize {
            let key = format!("intern-shrink-probe-{i:08}");
            let s = state
                .intern_str(key.as_bytes())
                .expect("short string interns");
            anchors.push(s);
        }

        let grown_buckets = state.global().interned_lt.bucket_count();
        assert_eq!(
            state.global().interned_lt.len(),
            baseline_live + 2000,
            "all 2000 distinct short strings are interned and live alongside \
             the runtime's own rooted strings"
        );
        assert!(
            grown_buckets >= 2048,
            "interning 2000 strings must force several bucket doublings past \
             the initial 64 (saw {grown_buckets})"
        );

        drop(anchors);
        state.gc().full_collect();

        let shrunk_buckets = state.global().interned_lt.bucket_count();
        let surviving_live = state.global().interned_lt.len();
        assert!(
            surviving_live <= baseline_live,
            "every probe string is unreachable and must be swept out of the \
             intern table, leaving at most the runtime's own strings (baseline \
             {baseline_live}, surviving {surviving_live})"
        );
        assert!(
            surviving_live < 2000,
            "the 2000 probe strings must not survive collection (surviving \
             {surviving_live})"
        );
        assert_eq!(
            shrunk_buckets, 64,
            "the batch-end shrink check must reclaim the stale buckets back to \
             the 64-bucket floor (saw {shrunk_buckets}, grew to {grown_buckets})"
        );
        assert!(
            shrunk_buckets < grown_buckets,
            "shrink must strictly reduce the bucket array"
        );
    }

    #[test]
    fn table_buffer_accounting_refunds_on_sweep() {
        let mut state = new_state().expect("state should initialize");
        let _heap_guard = {
            let g = state.global();
            lua_gc::HeapGuard::push(&g.heap)
        };

        let table = state.new_table();
        let key = state.external_root_value(LuaValue::Table(table));
        let header_bytes = state.global().heap.bytes_used();
        assert!(header_bytes > 0);

        for i in 1..=128 {
            table
                .raw_set_int(&mut state, i, LuaValue::Int(i))
                .expect("integer table insert should succeed");
        }
        let grown_bytes = state.global().heap.bytes_used();
        assert!(
            grown_bytes > header_bytes,
            "table array/hash buffer growth must be charged to the GC heap"
        );

        state.gc().full_collect();
        assert_eq!(
            state.global().heap.bytes_used(),
            grown_bytes,
            "rooted table buffer bytes should remain charged after collection"
        );

        assert!(state.external_unroot_value(key).is_some());
        state.gc().full_collect();
        assert_eq!(state.global().heap.bytes_used(), 0);
        assert_eq!(state.global().heap.allgc_count(), 0);
    }

    #[test]
    fn userdata_buffer_accounting_refunds_on_sweep() {
        let mut state = new_state().expect("state should initialize");
        let _heap_guard = {
            let g = state.global();
            lua_gc::HeapGuard::push(&g.heap)
        };

        let payload_len = 4096;
        let userdata = state
            .new_userdata_typed(b"accounting", payload_len, 3)
            .expect("userdata allocation should succeed");
        state.pop_n(1);
        let key = state.external_root_value(LuaValue::UserData(userdata));
        let allocated_bytes = state.global().heap.bytes_used();
        assert!(
            allocated_bytes > payload_len,
            "userdata payload bytes must be charged to the GC heap"
        );

        state.gc().full_collect();
        assert_eq!(
            state.global().heap.bytes_used(),
            allocated_bytes,
            "rooted userdata payload bytes should remain charged after collection"
        );

        assert!(state.external_unroot_value(key).is_some());
        state.gc().full_collect();
        assert_eq!(state.global().heap.bytes_used(), 0);
        assert_eq!(state.global().heap.allgc_count(), 0);
    }

    #[test]
    fn cclosure_upvalue_accounting_refunds_on_sweep() {
        let mut state = new_state().expect("state should initialize");
        let _heap_guard = {
            let g = state.global();
            lua_gc::HeapGuard::push(&g.heap)
        };

        let nupvalues = 64;
        for i in 0..nupvalues {
            state.push(LuaValue::Int(i as i64));
        }
        crate::api::push_cclosure(&mut state, test_noop_cclosure, nupvalues as i32)
            .expect("C closure creation should succeed");
        let LuaValue::Function(LuaClosure::C(ccl)) = state.get_at(state.top_idx() - 1) else {
            panic!("expected heavy C closure");
        };
        let expected_payload = ccl.buffer_bytes();
        let key = state.external_root_value(LuaValue::Function(LuaClosure::C(ccl)));
        state.pop_n(1);
        let allocated_bytes = state.global().heap.bytes_used();
        assert!(
            allocated_bytes >= expected_payload,
            "C closure upvalue vector bytes must be charged to the GC heap"
        );

        state.gc().full_collect();
        assert_eq!(
            state.global().heap.bytes_used(),
            allocated_bytes,
            "rooted C closure payload bytes should remain charged after collection"
        );

        assert!(state.external_unroot_value(key).is_some());
        state.gc().full_collect();
        assert_eq!(state.global().heap.bytes_used(), 0);
        assert_eq!(state.global().heap.allgc_count(), 0);
    }

    #[test]
    fn proto_and_lclosure_accounting_refunds_on_sweep() {
        let mut state = new_state().expect("state should initialize");
        let _heap_guard = {
            let g = state.global();
            lua_gc::HeapGuard::push(&g.heap)
        };

        let mut proto = LuaProto::placeholder();
        proto.code = vec![lua_types::opcode::Instruction(0); 2048];
        proto.lineinfo = vec![0; 2048];
        proto.k = vec![LuaValue::Int(1); 512];
        let expected_proto_payload = proto.buffer_bytes();
        let proto = GcRef::new(proto);
        proto.account_buffer(expected_proto_payload as isize);

        let closure = state.new_lclosure(proto, 16);
        let expected_closure_payload = closure.buffer_bytes();
        let key = state.external_root_value(LuaValue::Function(LuaClosure::Lua(closure)));
        let allocated_bytes = state.global().heap.bytes_used();
        assert!(
            allocated_bytes >= expected_proto_payload + expected_closure_payload,
            "proto and Lua closure vector bytes must be charged to the GC heap"
        );

        state.gc().full_collect();
        assert_eq!(
            state.global().heap.bytes_used(),
            allocated_bytes,
            "rooted proto and Lua closure payload bytes should remain charged after collection"
        );

        assert!(state.external_unroot_value(key).is_some());
        state.gc().full_collect();
        assert_eq!(state.global().heap.bytes_used(), 0);
        assert_eq!(state.global().heap.allgc_count(), 0);
    }

    #[test]
    fn string_buffer_accounting_refunds_on_sweep() {
        let mut state = new_state().expect("state should initialize");
        let _heap_guard = {
            let g = state.global();
            lua_gc::HeapGuard::push(&g.heap)
        };

        let payload = vec![b'x'; crate::string::MAX_SHORT_LEN + 4096];
        let string = state
            .intern_str(&payload)
            .expect("long string should allocate");
        let key = state.external_root_value(LuaValue::Str(string));
        let allocated_bytes = state.global().heap.bytes_used();
        assert!(
            allocated_bytes > payload.len(),
            "long string backing bytes must be charged to the GC heap"
        );

        state.gc().full_collect();
        assert_eq!(
            state.global().heap.bytes_used(),
            allocated_bytes,
            "rooted string buffer bytes should remain charged after collection"
        );

        assert!(state.external_unroot_value(key).is_some());
        state.gc().full_collect();
        assert_eq!(state.global().heap.bytes_used(), 0);
        assert_eq!(state.global().heap.allgc_count(), 0);
    }

    #[test]
    fn interned_short_string_cache_does_not_root_unreferenced_string() {
        let mut state = new_state().expect("state should initialize");
        let _heap_guard = {
            let g = state.global();
            lua_gc::HeapGuard::push(&g.heap)
        };

        let payload = b"weak-cache-probe-a";
        let string = state
            .intern_str(payload)
            .expect("short string should intern");
        let id = string.identity();
        assert!(state.global().interned_lt.contains_key(&payload[..]));
        assert_eq!(
            state.global().heap.register_allocation_token(id),
            state.global().heap.register_allocation_token(id),
            "token registration is get-or-insert while the string is provably live"
        );
        assert!(state.global().heap.allocation_token(id).is_some());

        state.gc().full_collect();
        assert!(!state.global().interned_lt.contains_key(&payload[..]));
        assert_eq!(state.global().heap.allocation_token(id), None);
    }

    #[test]
    fn interned_short_string_cache_keeps_reachable_string_until_unrooted() {
        let mut state = new_state().expect("state should initialize");
        let _heap_guard = {
            let g = state.global();
            lua_gc::HeapGuard::push(&g.heap)
        };

        let payload = b"weak-cache-probe-b";
        let string = state
            .intern_str(payload)
            .expect("short string should intern");
        let id = string.identity();
        state.global().heap.register_allocation_token(id);
        let key = state.external_root_value(LuaValue::Str(string));

        state.gc().full_collect();
        assert!(state.global().interned_lt.contains_key(&payload[..]));
        assert!(state.global().heap.allocation_token(id).is_some());

        assert!(state.external_unroot_value(key).is_some());
        state.gc().full_collect();
        assert!(!state.global().interned_lt.contains_key(&payload[..]));
        assert_eq!(state.global().heap.allocation_token(id), None);
    }

    #[test]
    fn gc_phase_predicates_follow_heap_state() {
        let mut state = new_state().expect("state should initialize");
        let _heap_guard = {
            let g = state.global();
            lua_gc::HeapGuard::push(&g.heap)
        };

        {
            let mut g = state.global_mut();
            g.gckind = GcKind::Incremental as u8;
            g.lastatomic = 0;
            assert!(!g.is_gen_mode());
            g.lastatomic = 1;
            assert!(g.is_gen_mode());
            g.lastatomic = 0;
        }

        let mut roots = Vec::new();
        for _ in 0..16 {
            let table = state.new_table();
            roots.push(state.external_root_value(LuaValue::Table(table)));
        }

        let mut saw_keep = false;
        let mut saw_sweep = false;
        for _ in 0..128 {
            state.gc().incremental_step(1);
            let g = state.global();
            let heap_state = g.heap.gc_state();
            assert_eq!(g.keep_invariant(), heap_state.is_invariant());
            assert_eq!(g.is_sweep_phase(), heap_state.is_sweep());
            saw_keep |= g.keep_invariant();
            saw_sweep |= g.is_sweep_phase();
            if heap_state.is_pause() && saw_keep && saw_sweep {
                break;
            }
        }

        assert!(
            saw_keep,
            "incremental cycle should expose an invariant phase"
        );
        assert!(saw_sweep, "incremental cycle should expose a sweep phase");

        for key in roots {
            assert!(state.external_unroot_value(key).is_some());
        }
        state.gc().full_collect();
    }

    #[test]
    fn gc_barrier_keeps_new_child_stored_in_black_parent() {
        let mut state = new_state().expect("state should initialize");
        let _heap_guard = {
            let g = state.global();
            lua_gc::HeapGuard::push(&g.heap)
        };

        let parent = state.new_table();
        let parent_key = state.external_root_value(LuaValue::Table(parent));
        state.gc().incremental_step(1);
        assert!(
            state.global().keep_invariant(),
            "test setup should leave the parent marked during an active cycle"
        );

        let child = state.new_table();
        let parent_value = LuaValue::Table(parent);
        let child_value = LuaValue::Table(child);
        parent
            .raw_set_int(&mut state, 1, child_value)
            .expect("table store should succeed");
        state.gc_barrier_back(&parent_value, &child_value);

        for _ in 0..128 {
            if state.gc().incremental_step(1) {
                break;
            }
        }

        assert_eq!(state.global().heap.allgc_count(), 2);
        assert_eq!(
            parent.get_int(1).as_table().map(|t| t.identity()),
            Some(child.identity())
        );

        assert!(state.external_unroot_value(parent_key).is_some());
        state.gc().full_collect();
        assert_eq!(state.global().heap.allgc_count(), 0);
    }

    #[test]
    fn generational_mode_promotes_and_barriers_age_objects() {
        let mut state = new_state().expect("state should initialize");
        let _heap_guard = {
            let g = state.global();
            lua_gc::HeapGuard::push(&g.heap)
        };

        let parent = state.new_table();
        let parent_key = state.external_root_value(LuaValue::Table(parent));

        state.gc().change_mode(GcKind::Generational);
        assert_eq!(parent.0.age(), lua_gc::GcAge::Old);
        assert_eq!(parent.0.color(), lua_gc::Color::Black);
        let majorbase = state.global().gc_estimate;
        assert!(majorbase > 0);
        assert!(state.global().gc_debt() <= 0);

        let child = state.new_table();
        let parent_value = LuaValue::Table(parent);
        let child_value = LuaValue::Table(child);
        parent
            .raw_set_int(&mut state, 1, child_value.clone())
            .expect("table store should succeed");
        state.gc_barrier_back(&parent_value, &child_value);
        assert_eq!(parent.0.age(), lua_gc::GcAge::Touched1);
        assert_eq!(parent.0.color(), lua_gc::Color::Gray);
        assert_eq!(child.0.age(), lua_gc::GcAge::New);

        let metatable = state.new_table();
        parent.set_metatable(Some(metatable));
        state.gc().obj_barrier(&parent, &metatable);
        assert_eq!(metatable.0.age(), lua_gc::GcAge::Old0);

        assert!(state.gc().generational_step_minor_only());
        assert_eq!(parent.0.age(), lua_gc::GcAge::Touched2);
        assert_eq!(child.0.age(), lua_gc::GcAge::Survival);
        assert_eq!(metatable.0.age(), lua_gc::GcAge::Old1);
        assert_eq!(state.global().gc_estimate, majorbase);
        assert!(state.global().gc_debt() <= 0);

        state.gc().change_mode(GcKind::Incremental);
        assert_eq!(parent.0.age(), lua_gc::GcAge::New);
        assert_eq!(child.0.age(), lua_gc::GcAge::New);
        assert_eq!(metatable.0.age(), lua_gc::GcAge::New);

        assert!(state.external_unroot_value(parent_key).is_some());
        state.gc().full_collect();
    }

    #[test]
    fn generational_upvalue_write_barrier_marks_young_child_old0() {
        let mut state = new_state().expect("state should initialize");
        let _heap_guard = {
            let g = state.global();
            lua_gc::HeapGuard::push(&g.heap)
        };

        let proto = state.new_proto();
        let closure = state.new_lclosure(proto, 1);
        let closure_key = state.external_root_value(LuaValue::Function(LuaClosure::Lua(closure)));
        state.gc().change_mode(GcKind::Generational);
        let uv = closure.upval(0);
        assert_eq!(uv.0.age(), lua_gc::GcAge::Old);

        let child = state.new_table();
        state
            .upvalue_set(&closure, 0, LuaValue::Table(child))
            .expect("closed upvalue write should succeed");
        assert_eq!(child.0.age(), lua_gc::GcAge::Old0);

        assert!(state.external_unroot_value(closure_key).is_some());
        state.gc().full_collect();
    }

    #[test]
    fn cclosure_setupvalue_replaces_upvalue() {
        let mut state = new_state().expect("state should initialize");
        let _heap_guard = {
            let g = state.global();
            lua_gc::HeapGuard::push(&g.heap)
        };

        let first = state.new_table();
        state.push(LuaValue::Table(first));
        crate::api::push_cclosure(&mut state, test_noop_cclosure, 1)
            .expect("C closure creation should succeed");
        let LuaValue::Function(LuaClosure::C(ccl)) = state.get_at(state.top_idx() - 1) else {
            panic!("expected heavy C closure");
        };

        let second = state.new_table();
        state.push(LuaValue::Table(second));
        let name =
            crate::api::setup_value(&mut state, -2, 1).expect("C closure upvalue should exist");

        assert!(name.is_empty());
        let upvalues = ccl.upvalues.borrow();
        let LuaValue::Table(actual) = upvalues[0].clone() else {
            panic!("expected table upvalue");
        };
        assert_eq!(actual.identity(), second.identity());
    }

    #[test]
    fn generational_cclosure_setupvalue_barrier_marks_young_child_old0() {
        let mut state = new_state().expect("state should initialize");
        let _heap_guard = {
            let g = state.global();
            lua_gc::HeapGuard::push(&g.heap)
        };

        state.push(LuaValue::Nil);
        crate::api::push_cclosure(&mut state, test_noop_cclosure, 1)
            .expect("C closure creation should succeed");
        let LuaValue::Function(LuaClosure::C(ccl)) = state.get_at(state.top_idx() - 1) else {
            panic!("expected heavy C closure");
        };
        let closure_key = state.external_root_value(LuaValue::Function(LuaClosure::C(ccl)));

        state.gc().change_mode(GcKind::Generational);
        assert_eq!(ccl.0.age(), lua_gc::GcAge::Old);

        let child = state.new_table();
        state.push(LuaValue::Table(child));
        crate::api::setup_value(&mut state, -2, 1).expect("C closure upvalue should exist");

        assert_eq!(child.0.age(), lua_gc::GcAge::Old0);

        assert!(state.external_unroot_value(closure_key).is_some());
        state.gc().full_collect();
    }

    #[test]
    fn generational_closure_upvalue_slot_barrier_marks_new_upval_old0() {
        let mut state = new_state().expect("state should initialize");
        let _heap_guard = {
            let g = state.global();
            lua_gc::HeapGuard::push(&g.heap)
        };

        let proto = state.new_proto();
        let closure = state.new_lclosure(proto, 1);
        let closure_key = state.external_root_value(LuaValue::Function(LuaClosure::Lua(closure)));
        state.gc().change_mode(GcKind::Generational);
        assert_eq!(closure.0.age(), lua_gc::GcAge::Old);

        let replacement = state.new_upval_closed(LuaValue::Nil);
        closure.set_upval(0, replacement);
        state.gc().obj_barrier(&closure, &replacement);
        assert_eq!(replacement.0.age(), lua_gc::GcAge::Old0);

        assert!(state.external_unroot_value(closure_key).is_some());
        state.gc().full_collect();
    }

    #[test]
    fn cross_thread_upvalue_mirror_traces_values_as_roots() {
        let mut state = new_state().expect("state should initialize");
        let _heap_guard = {
            let g = state.global();
            lua_gc::HeapGuard::push(&g.heap)
        };

        let mirrored = state.new_table();
        state
            .global_mut()
            .cross_thread_upvals
            .insert((999, StackIdx(0)), LuaValue::Table(mirrored));

        state.gc().full_collect();
        assert_eq!(state.global().heap.allgc_count(), 1);

        state.global_mut().cross_thread_upvals.clear();
        state.gc().full_collect();
        assert_eq!(state.global().heap.allgc_count(), 0);
    }

    #[test]
    fn generational_full_collect_promotes_new_survivors_to_old() {
        let mut state = new_state().expect("state should initialize");
        let _heap_guard = {
            let g = state.global();
            lua_gc::HeapGuard::push(&g.heap)
        };

        state.gc().change_mode(GcKind::Generational);
        let table = state.new_table();
        let table_key = state.external_root_value(LuaValue::Table(table));
        assert_eq!(table.0.age(), lua_gc::GcAge::New);

        state.gc().full_collect();
        assert_eq!(table.0.age(), lua_gc::GcAge::Old);
        assert_eq!(table.0.color(), lua_gc::Color::Black);

        assert!(state.external_unroot_value(table_key).is_some());
        state.gc().full_collect();
    }

    #[test]
    fn gc_packed_params_return_user_visible_values() {
        let mut state = new_state().expect("state should initialize");
        assert_eq!(
            crate::api::gc(&mut state, crate::api::GcArgs::SetPause { value: 200 }),
            200
        );
        assert_eq!(state.global().gc_pause_param(), 200);
        assert_eq!(
            crate::api::gc(&mut state, crate::api::GcArgs::SetStepMul { value: 200 }),
            100
        );
        assert_eq!(state.global().gc_stepmul_param(), 200);

        crate::api::gc(
            &mut state,
            crate::api::GcArgs::Gen {
                minormul: 0,
                majormul: 200,
            },
        );
        assert_eq!(state.global().gc_genmajormul_param(), 200);
    }

    #[test]
    fn generational_step_runs_bad_major_when_growth_exceeds_genmajormul() {
        let mut state = new_state().expect("state should initialize");
        let _heap_guard = {
            let g = state.global();
            lua_gc::HeapGuard::push(&g.heap)
        };

        let root = state.new_table();
        let root_key = state.external_root_value(LuaValue::Table(root));
        state.gc().change_mode(GcKind::Generational);

        let root_value = LuaValue::Table(root);
        for i in 1..=64 {
            let child = state.new_table();
            let child_value = LuaValue::Table(child);
            root.raw_set_int(&mut state, i, child_value.clone())
                .expect("table store should succeed");
            state.gc_barrier_back(&root_value, &child_value);
        }

        {
            let mut g = state.global_mut();
            g.gc_estimate = 1;
            set_debt(&mut *g, 1);
        }

        assert!(state.gc().generational_step());
        let g = state.global();
        assert!(g.is_gen_mode());
        assert!(
            g.lastatomic > 0,
            "bad major collection should arm stepgenfull"
        );
        assert!(g.gc_estimate > 1);
        assert!(g.gc_debt() <= 0);
        assert_eq!(root.0.age(), lua_gc::GcAge::Old);
        drop(g);

        assert!(state.external_unroot_value(root_key).is_some());
        state.gc().full_collect();
    }

    #[test]
    fn generational_implicit_step_runs_major_when_heap_threshold_exceeded() {
        let mut state = new_state().expect("state should initialize");
        let _heap_guard = {
            let g = state.global();
            lua_gc::HeapGuard::push(&g.heap)
        };

        let root = state.new_table();
        let root_key = state.external_root_value(LuaValue::Table(root));
        state.gc().change_mode(GcKind::Generational);

        let root_value = LuaValue::Table(root);
        for i in 1..=64 {
            let child = state.new_table();
            let child_value = LuaValue::Table(child);
            root.raw_set_int(&mut state, i, child_value.clone())
                .expect("table store should succeed");
            state.gc_barrier_back(&root_value, &child_value);
        }

        {
            let mut g = state.global_mut();
            g.gc_estimate = 1;
            set_debt(&mut *g, -1);
            g.heap.set_threshold_bytes(1);
        }

        assert!(state.gc().generational_step());
        let g = state.global();
        assert!(g.is_gen_mode());
        assert!(
            g.lastatomic > 0,
            "implicit threshold-triggered growth should arm a bad major"
        );
        assert!(g.gc_debt() <= 0);
        drop(g);

        assert!(state.external_unroot_value(root_key).is_some());
        state.gc().full_collect();
    }

    #[test]
    fn generational_stepgenfull_returns_to_gen_after_good_collection() {
        let mut state = new_state().expect("state should initialize");
        let _heap_guard = {
            let g = state.global();
            lua_gc::HeapGuard::push(&g.heap)
        };

        let root = state.new_table();
        let root_key = state.external_root_value(LuaValue::Table(root));
        state.gc().change_mode(GcKind::Generational);
        {
            let mut g = state.global_mut();
            g.lastatomic = 1024;
        }

        assert!(state.gc().generational_step());
        let g = state.global();
        assert_eq!(g.gckind, GcKind::Generational as u8);
        assert_eq!(g.lastatomic, 0);
        assert!(g.gc_debt() <= 0);
        assert_eq!(root.0.age(), lua_gc::GcAge::Old);
        assert_eq!(root.0.color(), lua_gc::Color::Black);
        drop(g);

        assert!(state.external_unroot_value(root_key).is_some());
        state.gc().full_collect();
    }

    #[test]
    fn generational_step_zero_reports_false_without_positive_debt() {
        let mut state = new_state().expect("state should initialize");
        let _heap_guard = {
            let g = state.global();
            lua_gc::HeapGuard::push(&g.heap)
        };

        state.gc().change_mode(GcKind::Generational);
        assert_eq!(
            crate::api::gc(&mut state, crate::api::GcArgs::Step { data: 0 }),
            0
        );
        assert_eq!(
            crate::api::gc(&mut state, crate::api::GcArgs::Step { data: 1 }),
            1
        );
    }
}
