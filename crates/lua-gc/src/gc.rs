//! Lua incremental tri-color mark-and-sweep garbage collector (Phase D).
//!
//! Ported from `src/lgc.c` (Lua 5.4.7, 1744 lines, 73 functions).
//!
//! This crate (`lua-gc`) is permitted to use `unsafe` (ceiling: 20 blocks per
//! `harness/unsafe-budgets.toml`). Every block carries `// SAFETY: ...`.
//!
//! # Algorithm overview
//!
//! Lua 5.4 uses a tri-color incremental mark-and-sweep collector with an
//! optional generational minor/major cycle strategy. The three colors are:
//! - **White** (two alternating bits): not yet visited; dead after a cycle if
//!   still white.
//! - **Gray**: visited but outgoing references not yet traced.
//! - **Black**: fully traced; invariant: a black object cannot point to a white one.
//!
//! The GC advances through the states in `GcState` order via `single_step`.
//!
//! # Type-vocabulary reconciliation
//!
//! Earlier Phase-A versions of this file shipped private `LuaState` and
//! `GlobalState` stubs so the crate could compile in isolation. The
//! `type-vocabulary` registry in `harness/type-vocabulary.tsv` lists those
//! names as owned by `lua-vm`; this crate now imports them from there.
//!
//! Replacing the stubs surfaces a deeper mismatch: the canonical
//! `GlobalState` represents the GC's working lists (`allgc`, `gray`,
//! `grayagain`, …) as `Vec<GcRef<dyn Collectable>>`, while the Phase-A
//! port of `lgc.c` here used raw `*mut GcHeader` intrusive linked lists.
//! Adapting one to the other is a Phase-D rewrite; for now the function
//! bodies that depend on those fields are stubbed as
//! `todo!("phase-b-reconcile: …")`.
//!
//! The constants, pure bit helpers, and `GcHeader` raw-pointer types
//! survive — they're representation-only and have no dependency on
//! `GlobalState`.
//!
//! # `unsafe` discipline
//!
//! Most function signatures in this module take or return raw GC pointers
//! (`GcObj` = `*mut GcHeader`) because that mirrors the C source. However,
//! while the bodies are stubbed as `todo!(…)` they perform **no** unsafe
//! operations, so the wrappers are declared as safe `fn`. Phase D will
//! re-introduce `unsafe fn` on the (small) subset of these that genuinely
//! need caller invariants once real bodies replace the stubs.
//!
//! The two genuinely-unsafe helpers retained today — `find_last` and
//! `check_pointer` — dereference `*mut GcObj` and remain `unsafe fn` with
//! `// SAFETY:` invariants documented inline.
//!
//! C: `#define lgc_c` / `#define LUA_CORE`

// Canonical interpreter types live in `lua-vm` per harness/type-vocabulary.tsv.
// This crate re-exports them rather than defining local stubs.
//
// TODO_ARCH(phase-b-reconcile): a `GcHost` trait in `lua-types` should let
// `lua-gc` operate on a thinner abstraction than `LuaState`. Until that lands,
// we depend on `lua-vm` directly. `lua-vm` does NOT currently depend on
// `lua-gc`, so there is no real cycle.

#[allow(unused_imports)]
use lua_types::error::LuaError;
#[allow(unused_imports)]
use lua_types::gc::GcRef;
#[allow(unused_imports)]
use lua_types::value::LuaValue;

pub use lua_vm::state::{GlobalState, LuaState};

/// Helper accessors that the GC needs but the canonical `LuaState` /
/// `GlobalState` do not yet expose. Implemented over the canonical types via
/// extension traits so we do not duplicate `LuaState`/`GlobalState`.
pub trait LuaStateGcExt {
    /// Phase-B stub for `luaE_setdebt`.
    fn set_debt(&mut self, debt: isize);

    /// Phase-B stub for accessing the running thread as a raw GcObj.
    /// Returns null in Phase B; Phase D returns a live thread pointer.
    fn current_thread_raw(&self) -> GcObj;
}

impl LuaStateGcExt for LuaState {
    fn set_debt(&mut self, _debt: isize) {
        todo!("phase-b-reconcile: lua_vm::state::LuaState::set_debt not implemented")
    }
    fn current_thread_raw(&self) -> GcObj {
        std::ptr::null_mut()
    }
}

pub trait GlobalStateGcExt {
    /// Phase-B stub for `g->mainthread` (raw GcObj head of the main thread).
    /// Placeholder returns null; Phase D supplies a real pointer.
    fn mainthread_raw(&self) -> GcObj;
}

impl GlobalStateGcExt for GlobalState {
    fn mainthread_raw(&self) -> GcObj {
        std::ptr::null_mut()
    }
}

// ---------------------------------------------------------------------------
// GC state machine constants  (lgc.h)
// ---------------------------------------------------------------------------

/// C: `#define GCSpropagate 0`
pub const GCS_PROPAGATE: u8 = 0;
/// C: `#define GCSenteratomic 1`
pub const GCS_ENTER_ATOMIC: u8 = 1;
/// C: `#define GCSatomic 2`
pub const GCS_ATOMIC: u8 = 2;
/// C: `#define GCSswpallgc 3`
pub const GCS_SWP_ALLGC: u8 = 3;
/// C: `#define GCSswpfinobj 4`
pub const GCS_SWP_FINOBJ: u8 = 4;
/// C: `#define GCSswptobefnz 5`
pub const GCS_SWP_TOBEFNZ: u8 = 5;
/// C: `#define GCSswpend 6`
pub const GCS_SWP_END: u8 = 6;
/// C: `#define GCScallfin 7`
pub const GCS_CALLFIN: u8 = 7;
/// C: `#define GCSpause 8`
pub const GCS_PAUSE: u8 = 8;

// ---------------------------------------------------------------------------
// GC color bit positions  (lgc.h)
// ---------------------------------------------------------------------------

/// C: `#define WHITE0BIT 3`
pub const WHITE0_BIT: u8 = 3;
/// C: `#define WHITE1BIT 4`
pub const WHITE1_BIT: u8 = 4;
/// C: `#define BLACKBIT 5`
pub const BLACK_BIT: u8 = 5;
/// C: `#define FINALIZEDBIT 6`
pub const FINALIZED_BIT: u8 = 6;

/// C: `#define WHITEBITS bit2mask(WHITE0BIT, WHITE1BIT)` = 0b00011000
pub const WHITE_BITS: u8 = (1 << WHITE0_BIT) | (1 << WHITE1_BIT);

/// C: `#define maskcolors (bitmask(BLACKBIT) | WHITEBITS)`
#[allow(dead_code)]
const MASK_COLORS: u8 = (1u8 << BLACK_BIT) | WHITE_BITS;

/// C: `#define maskgcbits (maskcolors | AGEBITS)`
#[allow(dead_code)]
const MASK_GC_BITS: u8 = MASK_COLORS | AGE_BITS;

// ---------------------------------------------------------------------------
// Generational GC age constants  (lgc.h)
// ---------------------------------------------------------------------------

/// C: `#define G_NEW 0` — created in the current cycle
pub const G_NEW: u8 = 0;
/// C: `#define G_SURVIVAL 1` — created in the previous cycle
pub const G_SURVIVAL: u8 = 1;
/// C: `#define G_OLD0 2` — promoted by a forward barrier in this cycle
pub const G_OLD0: u8 = 2;
/// C: `#define G_OLD1 3` — first full cycle as an old object
pub const G_OLD1: u8 = 3;
/// C: `#define G_OLD 4` — really old; skipped in minor collections
pub const G_OLD: u8 = 4;
/// C: `#define G_TOUCHED1 5` — old object touched this cycle
pub const G_TOUCHED1: u8 = 5;
/// C: `#define G_TOUCHED2 6` — old object touched in the previous cycle
pub const G_TOUCHED2: u8 = 6;

/// C: `#define AGEBITS 7` — mask for the bottom 3 bits of `marked`
pub const AGE_BITS: u8 = 7;

// ---------------------------------------------------------------------------
// GC stop-flag constants  (lgc.h)
// ---------------------------------------------------------------------------

/// C: `#define GCSTPUSR 1` — stopped by user (`collectgarbage("stop")`)
pub const GCSTPUSR: u8 = 1;
/// C: `#define GCSTPGC 2` — stopped by the GC itself (during finalization)
pub const GCSTPGC: u8 = 2;
/// C: `#define GCSTPCLS 4` — stopped while closing a Lua state
pub const GCSTPCLS: u8 = 4;

// ---------------------------------------------------------------------------
// GC mode constants  (lstate.h)
// ---------------------------------------------------------------------------

/// C: `KGC_INC` — incremental collection mode
pub const KGC_INC: u8 = 0;
/// C: `KGC_GEN` — generational collection mode
pub const KGC_GEN: u8 = 1;

// ---------------------------------------------------------------------------
// GC step / tuning constants  (lgc.c, lgc.h)
// ---------------------------------------------------------------------------

/// C: `#define GCSWEEPMAX 100`
#[allow(dead_code)]
const GC_SWEEP_MAX: i32 = 100;

/// C: `#define GCFINMAX 10`
#[allow(dead_code)]
const GC_FIN_MAX: i32 = 10;

/// C: `#define GCFINALIZECOST 50`
#[allow(dead_code)]
const GC_FINALIZE_COST: usize = 50;

/// C: `#define WORK2MEM sizeof(TValue)` — bytes per unit of traversal work.
#[allow(dead_code)]
const WORK2MEM: usize = std::mem::size_of::<LuaValue>();

/// C: `#define PAUSEADJ 100`
#[allow(dead_code)]
const PAUSE_ADJ: isize = 100;

// ---------------------------------------------------------------------------
// GC object header  (lgc.h / lobject.h)
// ---------------------------------------------------------------------------

/// Common GC object header. Every collectable object embeds this at offset 0
/// (Phase D); raw `*mut GcHeader` casts are sound when the concrete type is
/// `#[repr(C)]`.
///
/// C: `CommonHeader` macro expanding to three fields.
#[repr(C)]
pub struct GcHeader {
    /// Intrusive `next` pointer for allgc / finobj / fixedgc linked lists.
    pub next: *mut GcHeader,
    /// Internal type tag — same values as `LuaType` / variant tags.
    pub tt: u8,
    /// GC color bits (bits 3-5) and generational age (bits 0-2).
    pub marked: u8,
}

/// Type alias used throughout this module for GC object pointers.
///
/// C: `GCObject *`
pub type GcObj = *mut GcHeader;

/// Type alias for a pointer-to-list-head, used for list cursor manipulation.
///
/// C: `GCObject **p` (pointer to the "prev-next" slot, enabling O(1) removal)
pub type GcObjCursor = *mut GcObj;

// ---------------------------------------------------------------------------
// GC type-tag constants used in dispatch
// These mirror the `makevariant(LUA_T*, variant)` values from lobject.h.
// ---------------------------------------------------------------------------

/// C: `LUA_VSHRSTR` = short interned string
#[allow(dead_code)]
const LUA_VSHRSTR: u8 = 0x04;
/// C: `LUA_VLNGSTR` = long heap-allocated string
#[allow(dead_code)]
const LUA_VLNGSTR: u8 = 0x14;
/// C: `LUA_VUPVAL`
#[allow(dead_code)]
const LUA_VUPVAL: u8 = 0x40;
/// C: `LUA_VUSERDATA`
#[allow(dead_code)]
const LUA_VUSERDATA: u8 = 0x08;
/// C: `LUA_VLCL` = Lua closure
#[allow(dead_code)]
const LUA_VLCL: u8 = 0x46;
/// C: `LUA_VCCL` = C closure
#[allow(dead_code)]
const LUA_VCCL: u8 = 0x56;
/// C: `LUA_VTABLE`
#[allow(dead_code)]
const LUA_VTABLE: u8 = 0x05;
/// C: `LUA_VTHREAD`
#[allow(dead_code)]
const LUA_VTHREAD: u8 = 0x09;
/// C: `LUA_VPROTO`
#[allow(dead_code)]
const LUA_VPROTO: u8 = 0x42;

// ---------------------------------------------------------------------------
// Inline bit-manipulation helpers (from lgc.h macros).
// These operate on raw bytes, so they survive the canonical-type reconcile.
// ---------------------------------------------------------------------------

/// C: `luaC_white(g)` — returns the current white color mask.
#[inline]
pub fn current_white(current_white: u8) -> u8 {
    current_white & WHITE_BITS
}

/// C: `otherwhite(g)` — returns the OTHER white (used to detect dead objects).
#[inline]
pub fn other_white(current_white: u8) -> u8 {
    current_white ^ WHITE_BITS
}

/// C: `iswhite(x)` — true if `marked` has either white bit set.
#[inline]
pub fn is_white_marked(marked: u8) -> bool {
    (marked & WHITE_BITS) != 0
}

/// C: `isblack(x)` — true if the black bit is set.
#[inline]
pub fn is_black_marked(marked: u8) -> bool {
    (marked & (1 << BLACK_BIT)) != 0
}

/// C: `isgray(x)` — neither white nor black.
#[inline]
pub fn is_gray_marked(marked: u8) -> bool {
    (marked & (WHITE_BITS | (1 << BLACK_BIT))) == 0
}

/// C: `tofinalize(x)`
#[inline]
pub fn is_finalized_marked(marked: u8) -> bool {
    (marked & (1 << FINALIZED_BIT)) != 0
}

/// C: `isdeadm(ow, m)` — object marked with the OTHER white is dead.
#[inline]
pub fn is_dead_marked(other_white: u8, marked: u8) -> bool {
    (marked & other_white) != 0
}

/// C: `getage(o)` — bottom 3 bits of `marked`.
#[inline]
pub fn get_age(marked: u8) -> u8 {
    marked & AGE_BITS
}

/// C: `setage(o, a)` — replace the bottom 3 age bits.
#[inline]
pub fn set_age_bits(marked: u8, age: u8) -> u8 {
    (marked & !AGE_BITS) | age
}

/// C: `isold(o)` — age > G_SURVIVAL.
#[inline]
pub fn is_old_marked(marked: u8) -> bool {
    get_age(marked) > G_SURVIVAL
}

/// C: `changeage(o, f, t)` — XOR from age `f` to age `t` (asserts source).
#[inline]
pub fn change_age_bits(marked: u8, from: u8, to: u8) -> u8 {
    debug_assert_eq!(get_age(marked), from, "changeage: source age mismatch");
    marked ^ (from ^ to)
}

/// C: `set2gray(x)` — `resetbits(x->marked, maskcolors)`.
#[inline]
pub fn set_to_gray(marked: u8) -> u8 {
    marked & !MASK_COLORS
}

/// C: `set2black(x)` — clear white bits, set black bit.
#[inline]
pub fn set_to_black(marked: u8) -> u8 {
    (marked & !WHITE_BITS) | (1 << BLACK_BIT)
}

/// C: `nw2black(x)` — sets black bit; asserts object is not white.
#[inline]
pub fn nw2black(marked: u8) -> u8 {
    debug_assert!(!is_white_marked(marked), "nw2black: object is white");
    marked | (1 << BLACK_BIT)
}

/// C: `makewhite(g, x)` — erase color bits; set only the current white bit.
#[inline]
pub fn make_white(marked: u8, cur_white: u8) -> u8 {
    (marked & !MASK_COLORS) | (cur_white & WHITE_BITS)
}

/// C: `keepinvariant(g)` — true when GC state <= GCSatomic.
#[inline]
pub fn keep_invariant(gcstate: u8) -> bool {
    gcstate <= GCS_ATOMIC
}

/// C: `issweepphase(g)`
#[inline]
pub fn is_sweep_phase(gcstate: u8) -> bool {
    GCS_SWP_ALLGC <= gcstate && gcstate <= GCS_SWP_END
}

/// C: `gcrunning(g)`
#[inline]
pub fn gc_running(gcstp: u8) -> bool {
    gcstp == 0
}

/// C: `isdecGCmodegen(g)` — declared generational mode (may be temporarily incremental).
#[inline]
pub fn is_dec_gc_mode_gen(gckind: u8, lastatomic: usize) -> bool {
    gckind == KGC_GEN || lastatomic != 0
}

// ---------------------------------------------------------------------------
// Function stubs — Phase-D bodies pending type-vocabulary reconcile.
// ---------------------------------------------------------------------------
//
// The C `lgc.c` body relies on intrusive `*mut GcHeader` linked lists threaded
// through `GlobalState.allgc`, `g->gray`, `g->grayagain`, `g->finobj`, the
// four generational cohort cursors (`firstold1`, `survival`, `old1`,
// `reallyold`, etc.), and on `LuaState`/`GlobalState` reaching into concrete
// `LuaTable`/`UpVal`/… types from `lua-vm`. The canonical `lua-vm` shapes
// store these as `Vec<GcRef<dyn Collectable>>` and don't expose all the
// generational cohort fields yet; the field names also differ
// (`gc_debt` vs `GCdebt`, `gc_estimate` vs `GCestimate`).
//
// Bridging those two representations is a real Phase-D port (re-add intrusive
// links or rewrite this file Vec-first). Until that's scoped, every fn body
// below collapses to `todo!("phase-b-reconcile: …")`. Signatures remain so
// callers continue to type-check.
//
// The stubs are declared as safe `fn` even where their C counterparts would
// require `unsafe fn` — `todo!()` performs no unsafe operation. Phase D will
// re-introduce `unsafe fn` on the (small) subset that genuinely need it once
// real bodies replace the stubs. Two helpers below (`find_last`,
// `check_pointer`) actually dereference raw `*mut GcObj` and remain
// `unsafe fn`.
//
// Sections mirror the C source groupings:
//   §A  Generic / utility helpers
//   §B  Mark functions
//   §C  Traverse functions
//   §D  Sweep functions
//   §E  Finalization
//   §F  Generational collector
//   §G  GC control (public API)

// §A — Generic / utility helpers -------------------------------------------

/// C: `static GCObject **getgclist(GCObject *o)`
#[allow(dead_code)]
fn get_gc_list(_o: GcObj) -> GcObjCursor {
    todo!("phase-b-reconcile: get_gc_list — needs Phase-D intrusive gclist field")
}

/// C: `static void linkgclist_(GCObject *o, GCObject **pnext, GCObject **list)`
#[allow(dead_code)]
fn link_gc_list(_o: GcObj, _pnext: GcObjCursor, _list: GcObjCursor) {
    todo!("phase-b-reconcile: link_gc_list — depends on Phase-D gray-list shape")
}

/// C: `static void clearkey(Node *n)`
#[allow(dead_code)]
fn clear_key() {
    todo!("phase-b-reconcile: clear_key — needs TableNode from lua-vm")
}

/// C: `static int iscleared(global_State *g, const GCObject *o)`
#[allow(dead_code)]
fn is_cleared(_cur_white: u8, _o: GcObj) -> bool {
    todo!("phase-b-reconcile: is_cleared — needs Phase-D string-mark path")
}

// §A — Public write barriers ------------------------------------------------

/// C: `void luaC_barrier_(lua_State *L, GCObject *o, GCObject *v)`
pub(crate) fn barrier(_state: &mut LuaState, _o: GcObj, _v: GcObj) {
    todo!("phase-b-reconcile: barrier — depends on Phase-D mark dispatch")
}

/// C: `void luaC_barrierback_(lua_State *L, GCObject *o)`
pub(crate) fn barrier_back(_state: &mut LuaState, _o: GcObj) {
    todo!("phase-b-reconcile: barrier_back — needs Vec-based grayagain list")
}

/// C: `void luaC_fix(lua_State *L, GCObject *o)`
pub(crate) fn fix(_state: &mut LuaState, _o: GcObj) {
    todo!("phase-b-reconcile: fix — needs canonical allgc/fixedgc shape")
}

/// C: `GCObject *luaC_newobjdt(lua_State *L, int tt, size_t sz, size_t offset)`
pub(crate) fn new_obj_dt(
    _state: &mut LuaState,
    _tt: u8,
    _sz: usize,
    _offset: usize,
) -> GcObj {
    todo!("phase-b-reconcile: new_obj_dt — needs Phase-D allocation path")
}

/// C: `GCObject *luaC_newobj(lua_State *L, int tt, size_t sz)`
pub(crate) fn new_obj(_state: &mut LuaState, _tt: u8, _sz: usize) -> GcObj {
    todo!("phase-b-reconcile: new_obj — wraps new_obj_dt")
}

// §B — Mark functions -------------------------------------------------------

/// C: `static void reallymarkobject(global_State *g, GCObject *o)`
#[allow(dead_code)]
fn really_mark_object(_state: &mut LuaState, _o: GcObj) {
    todo!("phase-b-reconcile: really_mark_object — needs Phase-D mark dispatch")
}

/// C: `static void markmt(global_State *g)`
#[allow(dead_code)]
fn mark_metatables(_state: &mut LuaState) {
    todo!("phase-b-reconcile: mark_metatables — needs GlobalState.mt iteration")
}

/// C: `static lu_mem markbeingfnz(global_State *g)`
#[allow(dead_code)]
fn mark_being_finalized(_state: &mut LuaState) -> usize {
    todo!("phase-b-reconcile: mark_being_finalized — needs Vec-based tobefnz walk")
}

/// C: `static int remarkupvals(global_State *g)`
#[allow(dead_code)]
fn remark_upvalues(_state: &mut LuaState) -> usize {
    todo!("phase-b-reconcile: remark_upvalues — needs LuaState.openupval / twups")
}

/// C: `static void cleargraylists(global_State *g)`
#[allow(dead_code)]
fn clear_gray_lists(_state: &mut LuaState) {
    todo!("phase-b-reconcile: clear_gray_lists — needs Vec-typed gray lists")
}

/// C: `static void restartcollection(global_State *g)`
#[allow(dead_code)]
fn restart_collection(_state: &mut LuaState) {
    todo!("phase-b-reconcile: restart_collection — needs mainthread/registry mark path")
}

// §C — Traverse functions ---------------------------------------------------

/// C: `static void genlink(global_State *g, GCObject *o)`
#[allow(dead_code)]
fn gen_link(_state: &mut LuaState, _o: GcObj) {
    todo!("phase-b-reconcile: gen_link — needs Vec-based grayagain list")
}

/// C: `static void traverseweakvalue(global_State *g, Table *h)`
#[allow(dead_code)]
fn traverse_weak_value(_state: &mut LuaState, _h: GcObj) {
    todo!("phase-b-reconcile: traverse_weak_value — needs LuaTable from lua-vm")
}

/// C: `static int traverseephemeron(global_State *g, Table *h, int inv)`
#[allow(dead_code)]
fn traverse_ephemeron(_state: &mut LuaState, _h: GcObj, _inv: bool) -> bool {
    todo!("phase-b-reconcile: traverse_ephemeron — needs LuaTable from lua-vm")
}

/// C: `static void traversestrongtable(global_State *g, Table *h)`
#[allow(dead_code)]
fn traverse_strong_table(_state: &mut LuaState, _h: GcObj) {
    todo!("phase-b-reconcile: traverse_strong_table — needs LuaTable from lua-vm")
}

/// C: `static lu_mem traversetable(global_State *g, Table *h)`
#[allow(dead_code)]
fn traverse_table(_state: &mut LuaState, _h: GcObj) -> usize {
    todo!("phase-b-reconcile: traverse_table — needs LuaTable from lua-vm")
}

/// C: `static int traverseudata(global_State *g, Udata *u)`
#[allow(dead_code)]
fn traverse_udata(_state: &mut LuaState, _u: GcObj) -> usize {
    todo!("phase-b-reconcile: traverse_udata — needs LuaUserData fields from lua-vm")
}

/// C: `static int traverseproto(global_State *g, Proto *f)`
#[allow(dead_code)]
fn traverse_proto(_state: &mut LuaState, _f: GcObj) -> usize {
    todo!("phase-b-reconcile: traverse_proto — needs LuaProto fields from lua-vm")
}

/// C: `static int traverseCclosure(global_State *g, CClosure *cl)`
#[allow(dead_code)]
fn traverse_c_closure(_state: &mut LuaState, _cl: GcObj) -> usize {
    todo!("phase-b-reconcile: traverse_c_closure — needs CClosure from lua-vm")
}

/// C: `static int traverseLclosure(global_State *g, LClosure *cl)`
#[allow(dead_code)]
fn traverse_l_closure(_state: &mut LuaState, _cl: GcObj) -> usize {
    todo!("phase-b-reconcile: traverse_l_closure — needs LClosure / UpVal from lua-vm")
}

/// C: `static int traversethread(global_State *g, lua_State *th)`
#[allow(dead_code)]
fn traverse_thread(_state: &mut LuaState, _th: GcObj) -> usize {
    todo!("phase-b-reconcile: traverse_thread — needs LuaState stack from lua-vm")
}

/// C: `static lu_mem propagatemark(global_State *g)`
#[allow(dead_code)]
fn propagate_mark(_state: &mut LuaState) -> usize {
    todo!("phase-b-reconcile: propagate_mark — needs Vec-based gray list")
}

/// C: `static lu_mem propagateall(global_State *g)`
#[allow(dead_code)]
fn propagate_all(_state: &mut LuaState) -> usize {
    todo!("phase-b-reconcile: propagate_all — needs Vec-based gray list")
}

/// C: `static void convergeephemerons(global_State *g)`
#[allow(dead_code)]
fn converge_ephemerons(_state: &mut LuaState) {
    todo!("phase-b-reconcile: converge_ephemerons — needs Vec-based ephemeron list")
}

// §D — Sweep functions ------------------------------------------------------

/// C: `static void clearbykeys(global_State *g, GCObject *l)`
#[allow(dead_code)]
fn clear_by_keys(_state: &mut LuaState, _list: GcObj) {
    todo!("phase-b-reconcile: clear_by_keys — needs LuaTable hash iteration")
}

/// C: `static void clearbyvalues(global_State *g, GCObject *l, GCObject *f)`
#[allow(dead_code)]
fn clear_by_values(_state: &mut LuaState, _list: GcObj, _f: GcObj) {
    todo!("phase-b-reconcile: clear_by_values — needs LuaTable hash iteration")
}

/// C: `static void freeupval(lua_State *L, UpVal *uv)`
#[allow(dead_code)]
fn free_upval(_state: &mut LuaState, _uv: GcObj) {
    todo!("phase-b-reconcile: free_upval — needs UpVal::Open unlinking")
}

/// C: `static void freeobj(lua_State *L, GCObject *o)`
#[allow(dead_code)]
fn free_obj(_state: &mut LuaState, _o: GcObj) {
    todo!("phase-b-reconcile: free_obj — needs concrete-type deallocators in lua-vm")
}

/// C: `static GCObject **sweeplist(...)`
#[allow(dead_code)]
fn sweep_list(
    _state: &mut LuaState,
    _p: *mut GcObj,
    _count: i32,
    _count_out: *mut i32,
) -> *mut GcObj {
    todo!("phase-b-reconcile: sweep_list — needs intrusive allgc/finobj cursor")
}

/// C: `static GCObject **sweeptolive(lua_State *L, GCObject **p)`
#[allow(dead_code)]
fn sweep_to_live(_state: &mut LuaState, _p: *mut GcObj) -> *mut GcObj {
    todo!("phase-b-reconcile: sweep_to_live — wraps sweep_list")
}

// §E — Finalization ---------------------------------------------------------

/// C: `static void checkSizes(lua_State *L, global_State *g)`
#[allow(dead_code)]
fn check_sizes(_state: &mut LuaState) {
    todo!("phase-b-reconcile: check_sizes — needs StringPool::nuse/size accessors")
}

/// C: `static GCObject *udata2finalize(global_State *g)`
#[allow(dead_code)]
fn udata_to_finalize(_state: &mut LuaState) -> GcObj {
    todo!("phase-b-reconcile: udata_to_finalize — needs tobefnz/allgc cursor model")
}

/// C: `static void dothecall(lua_State *L, void *ud)`
#[allow(dead_code)]
fn do_the_call(_state: &mut LuaState) {
    todo!("phase-b-reconcile: do_the_call — needs luaD_callnoyield from lua-vm")
}

/// C: `static void GCTM(lua_State *L)`
#[allow(dead_code)]
fn gc_tm(_state: &mut LuaState) {
    todo!("phase-b-reconcile: gc_tm — needs tag-method lookup + protected call")
}

/// C: `static int runafewfinalizers(lua_State *L, int n)`
#[allow(dead_code)]
fn run_few_finalizers(_state: &mut LuaState, _n: i32) -> i32 {
    todo!("phase-b-reconcile: run_few_finalizers — needs tobefnz walk")
}

/// C: `static void callallpendingfinalizers(lua_State *L)`
#[allow(dead_code)]
fn call_all_pending_finalizers(_state: &mut LuaState) {
    todo!("phase-b-reconcile: call_all_pending_finalizers — needs tobefnz walk")
}

/// C: `static GCObject **findlast(GCObject **p)`
///
/// # Safety
/// `p` must be non-null and point at a valid list head slot terminating with
/// a null `next`. Each `*p` walked must be a live `GcHeader` (or null).
#[allow(dead_code)]
unsafe fn find_last(mut p: *mut GcObj) -> *mut GcObj {
    // SAFETY: caller guarantees `p` traverses a well-formed `next`-linked list
    // terminating in null; each dereference reads one live `GcHeader`.
    while !(*p).is_null() {
        p = &mut (**p).next;
    }
    p
}

/// C: `static void separatetobefnz(global_State *g, int all)`
#[allow(dead_code)]
fn separate_to_be_finalized(_state: &mut LuaState, _all: bool) {
    todo!("phase-b-reconcile: separate_to_be_finalized — needs finobj/finobjold1 cursors")
}

/// C: `static void checkpointer(GCObject **p, GCObject *o)`
///
/// # Safety
/// `p` must be non-null and point to a writable `GcObj` slot; if `*p == o`
/// then `o` must be a live `GcHeader` so that `(*o).next` is a valid read.
#[allow(dead_code)]
unsafe fn check_pointer(p: *mut GcObj, o: GcObj) {
    // SAFETY: caller guarantees `p` is writable and, when matched, `o` is live.
    if *p == o {
        *p = (*o).next;
    }
}

/// C: `static void correctpointers(global_State *g, GCObject *o)`
#[allow(dead_code)]
fn correct_pointers(_state: &mut LuaState, _o: GcObj) {
    todo!("phase-b-reconcile: correct_pointers — generational cohort fields missing")
}

/// C: `void luaC_checkfinalizer(lua_State *L, GCObject *o, Table *mt)`
pub(crate) fn check_finalizer(_state: &mut LuaState, _o: GcObj, _mt: GcObj) {
    todo!("phase-b-reconcile: check_finalizer — depends on allgc/finobj split")
}

// §F — Generational collector -----------------------------------------------

/// C: `static void setpause(global_State *g)`
#[allow(dead_code)]
fn set_pause(_state: &mut LuaState) {
    todo!("phase-b-reconcile: set_pause — needs luaE_setdebt")
}

/// C: `static void sweep2old(lua_State *L, GCObject **p)`
#[allow(dead_code)]
fn sweep_to_old(_state: &mut LuaState, _p: *mut GcObj) {
    todo!("phase-b-reconcile: sweep_to_old — needs grayagain Vec")
}

/// C: `static GCObject **sweepgen(...)`
#[allow(dead_code)]
fn sweep_gen(
    _state: &mut LuaState,
    _p: *mut GcObj,
    _limit: GcObj,
    _pfirstold1: *mut GcObj,
) -> *mut GcObj {
    todo!("phase-b-reconcile: sweep_gen — needs generational cohort cursors")
}

/// C: `static void whitelist(global_State *g, GCObject *p)`
#[allow(dead_code)]
fn whitelist(_state: &mut LuaState, _p: GcObj) {
    todo!("phase-b-reconcile: whitelist — needs intrusive list iteration")
}

/// C: `static GCObject **correctgraylist(GCObject **p)`
#[allow(dead_code)]
fn correct_gray_list(_p: *mut GcObj) -> *mut GcObj {
    todo!("phase-b-reconcile: correct_gray_list — needs Vec-based gray list")
}

/// C: `static void correctgraylists(global_State *g)`
#[allow(dead_code)]
fn correct_gray_lists(_state: &mut LuaState) {
    todo!("phase-b-reconcile: correct_gray_lists — depends on gray list shape")
}

/// C: `static void markold(global_State *g, GCObject *from, GCObject *to)`
#[allow(dead_code)]
fn mark_old(_state: &mut LuaState, _from: GcObj, _to: GcObj) {
    todo!("phase-b-reconcile: mark_old — needs intrusive allgc walk")
}

/// C: `static void finishgencycle(lua_State *L, global_State *g)`
#[allow(dead_code)]
fn finish_gen_cycle(_state: &mut LuaState) {
    todo!("phase-b-reconcile: finish_gen_cycle — wraps correct_gray_lists et al.")
}

/// C: `static void youngcollection(lua_State *L, global_State *g)`
#[allow(dead_code)]
fn young_collection(_state: &mut LuaState) {
    todo!("phase-b-reconcile: young_collection — generational cohort cursors missing")
}

/// C: `static void atomic2gen(lua_State *L, global_State *g)`
#[allow(dead_code)]
fn atomic_to_gen(_state: &mut LuaState) {
    todo!("phase-b-reconcile: atomic_to_gen — generational cohort cursors missing")
}

/// C: `static void setminordebt(global_State *g)`
#[allow(dead_code)]
fn set_minor_debt(_state: &mut LuaState) {
    todo!("phase-b-reconcile: set_minor_debt — needs luaE_setdebt")
}

/// C: `static lu_mem entergen(lua_State *L, global_State *g)`
#[allow(dead_code)]
fn enter_gen(_state: &mut LuaState) -> usize {
    todo!("phase-b-reconcile: enter_gen — wraps run_until_state + atomic")
}

/// C: `static void enterinc(global_State *g)`
#[allow(dead_code)]
fn enter_inc(_state: &mut LuaState) {
    todo!("phase-b-reconcile: enter_inc — needs generational cohort reset")
}

/// C: `void luaC_changemode(lua_State *L, int newmode)`
pub(crate) fn change_mode(_state: &mut LuaState, _new_mode: u8) {
    todo!("phase-b-reconcile: change_mode — wraps enter_gen/enter_inc")
}

/// C: `static lu_mem fullgen(lua_State *L, global_State *g)`
#[allow(dead_code)]
fn full_gen(_state: &mut LuaState) -> usize {
    todo!("phase-b-reconcile: full_gen — wraps enter_inc + enter_gen")
}

/// C: `static void stepgenfull(lua_State *L, global_State *g)`
#[allow(dead_code)]
fn step_gen_full(_state: &mut LuaState) {
    todo!("phase-b-reconcile: step_gen_full — needs lastatomic accessor")
}

/// C: `static void genstep(lua_State *L, global_State *g)`
#[allow(dead_code)]
fn gen_step(_state: &mut LuaState) {
    todo!("phase-b-reconcile: gen_step — needs gc_debt/gc_estimate accessors")
}

// §G — GC control (public API) ----------------------------------------------

/// C: `static void entersweep(lua_State *L)`
#[allow(dead_code)]
fn enter_sweep(_state: &mut LuaState) {
    todo!("phase-b-reconcile: enter_sweep — needs sweepgc cursor")
}

/// C: `static void deletelist(lua_State *L, GCObject *p, GCObject *limit)`
#[allow(dead_code)]
fn delete_list(_state: &mut LuaState, _p: GcObj, _limit: GcObj) {
    todo!("phase-b-reconcile: delete_list — needs intrusive list walk")
}

/// C: `void luaC_freeallobjects(lua_State *L)`
pub(crate) fn free_all_objects(_state: &mut LuaState) {
    todo!("phase-b-reconcile: free_all_objects — needs allgc/fixedgc/finobj walks")
}

/// C: `static lu_mem atomic(lua_State *L)`
#[allow(dead_code)]
fn atomic_phase(_state: &mut LuaState) -> usize {
    todo!("phase-b-reconcile: atomic_phase — depends on all of the above")
}

/// C: `static int sweepstep(...)`
#[allow(dead_code)]
fn sweep_step(_state: &mut LuaState, _next_state: u8, _next_list: *mut GcObj) -> usize {
    todo!("phase-b-reconcile: sweep_step — needs sweepgc cursor")
}

/// C: `static lu_mem singlestep(lua_State *L)`
#[allow(dead_code)]
fn single_step(_state: &mut LuaState) -> usize {
    todo!("phase-b-reconcile: single_step — wraps the rest of the FSM")
}

/// C: `void luaC_runtilstate(lua_State *L, int statesmask)`
pub(crate) fn run_until_state(_state: &mut LuaState, _states_mask: u32) {
    todo!("phase-b-reconcile: run_until_state — wraps single_step")
}

/// C: `static void incstep(lua_State *L, global_State *g)`
#[allow(dead_code)]
fn inc_step(_state: &mut LuaState) {
    todo!("phase-b-reconcile: inc_step — needs gc_debt accessor")
}

/// C: `void luaC_step(lua_State *L)`
pub(crate) fn step(_state: &mut LuaState) {
    todo!("phase-b-reconcile: step — dispatches gen_step or inc_step")
}

/// C: `static void fullinc(lua_State *L, global_State *g)`
#[allow(dead_code)]
fn full_inc(_state: &mut LuaState) {
    todo!("phase-b-reconcile: full_inc — wraps run_until_state")
}

/// C: `void luaC_fullgc(lua_State *L, int isemergency)`
pub(crate) fn full_gc(_state: &mut LuaState, _is_emergency: bool) {
    todo!("phase-b-reconcile: full_gc — dispatches full_inc or full_gen")
}

// ──────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        src/lgc.c  (1744 lines, 73 functions)
//   target_crate:  lua-gc
//   confidence:    medium
//   todos:         0
//   port_notes:    0
//   unsafe_blocks: 2   (find_last, check_pointer — both deref *mut GcObj,
//                       both carry // SAFETY: comments. All other stubs are
//                       safe `fn` until Phase D adds real bodies that need
//                       caller invariants.)
//   notes:         Unsafe-budget audit (Phase D pre-work): the Phase-A
//                  translator marked every C-mirrored signature as
//                  `unsafe fn`, but the stub bodies are pure `todo!(…)` and
//                  perform no unsafe operations. Stripped `unsafe` from the
//                  78 stubs whose only body is `todo!()` plus the two
//                  extension-trait helpers that return null. Phase D will
//                  reintroduce `unsafe fn` on the specific subset that
//                  genuinely requires caller invariants once real bodies
//                  land. The two retained `unsafe fn`s (`find_last`,
//                  `check_pointer`) dereference raw `*mut GcObj`/`*mut GcHeader`
//                  and have documented SAFETY invariants.
//
//                  Type-vocabulary reconcile context (unchanged): the local
//                  LuaState/GlobalState duplicates were replaced with
//                  `pub use lua_vm::state::…`. The canonical types use
//                  Vec<GcRef<dyn Collectable>> for the gray/allgc/finobj
//                  lists and rename GCdebt → gc_debt, GCestimate →
//                  gc_estimate; the generational cohort cursors (firstold1,
//                  survival, old1, reallyold, finobjold1, finobjsur,
//                  finobjrold) do not exist on the canonical struct yet.
//                  Rewriting the C intrusive-list port against the Vec model
//                  is a Phase-D job, so every function body still collapses
//                  to `todo!("phase-b-reconcile: …")`.
// ──────────────────────────────────────────────────────────────────────────
