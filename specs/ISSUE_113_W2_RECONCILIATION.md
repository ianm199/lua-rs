# Issue #113 Wave 2 — reconciliation of spec rev-3 against landed main

Status: Phase-0 contract for branch `omnilua-dev/issue-113-wave2-ownervec`,
cut from `origin/main @ ca2abb2a`. This note reconciles the rev-3 W2 design
(`ISSUE_113_GCHEADER_DIET_SPEC.md`, written before W1 and #260 landed)
against `crates/lua-gc/src/heap.rs` **as it is now**. It is the contract the
implementation codes against; where rev-3 and the landed code diverge, the
disposition column is authoritative.

Landed-since-rev-3 facts (verified against the file):

- **W1 landed.** `gray_next` is gone from `GcHeader`; the grayagain revisit set
  is a heap-owned `RefCell<Vec<NonNull<GcBox<dyn Trace>>>>` (`grayagain`) plus a
  capacity-recycling `grayagain_scratch`. `HDR_GRAY_LISTED` is the O(1)
  membership bit. Header is **24 B on 64-bit** (`color+age+flags+pad+size(u32)+
  next(16)`), pinned by `gcheader_is_24_bytes_after_grayagain_diet`. All eight
  grayagain ops (`remember_minor_revisit`, `mark_minor_revisit_objects`,
  `take_grayagain`, `replace_grayagain`, `clear_grayagain`, `unlink_grayagain`,
  `grayagain_count`, and the scratch swap in `sweep_young`) are already Vec-based
  and need **no change** in W2 except that `unlink_grayagain` stops being reached
  through the deleted `correct_generation_pointers` and is called directly.
- **#260 landed deterministic close.** `closed: Cell<bool>` (set once at
  `drop_all` top, never cleared) + `tearing_down: Cell<bool>` (transient, RAII
  reset via `TearingDownReset`). `collection_inert() == paused || closed` gates
  all collection entry points (`step`, `step_with_post_mark`, `full_collect_
  with_post_mark`, `minor_collect_with_post_mark`, `incremental_step_with_post_
  mark`, `mark_only_with_post_mark`, `incremental_run_until_state_with_post_
  mark`, and `would_collect`). `drop_all` is **drain-until-stable**: it loops the
  five owner-list drains until a pass frees nothing, then zeroes accounting, with
  a **10 000-pass panic** against a nonconvergent allocate-in-`Drop`.
  `remember_minor_revisit` early-returns when `closed`.

## Part A — rev-3 W2 mechanism-by-mechanism

| rev-3 W2 mechanism | landed-code state | disposition |
|---|---|---|
| `OwnerVec { slots: Vec<Option<NonNull>>, tombstones, reallyold, old1, survival }` | none — lists are intrusive `head`/`finobj`/`tobefnz` cells chained via `GcHeader::next` | **COMPOSE (new).** Add the type; three `RefCell<OwnerVec>` replace the three head cells. |
| Header age authoritative, vectors = coarse position cohorts | **already true.** `sweep_young_range` frees on `is_white() && !age.is_old()`; `Marker::should_trace_age` skips exact-`Old`. Cohorts are the cursor cells. | **COMPOSE.** No age-logic change; only the cohort *representation* moves from pointer cursors to `usize` counts. |
| Tombstone-never-shift + 25% density + move-time trigger | none (intrusive unlink shifts nothing but has no tombstones/density concept) | **COMPOSE (new).** `tombstones*4 > slots.len()` constant; compaction mandatory at cycle-ends, threshold at `start_cycle`+after-move-outside-sweep. |
| One whole-vector compaction (slice-only withdrawn) | none | **COMPOSE (new).** Single `compact()` recounts each boundary = survivors below its old physical index. |
| Full incremental sweep via `sweep_index`/`sweep_watermark` two-phase scan/release | `sweep_budgeted` walks `sweep_prev_next` (raw `Cell` pointer), frees inline one-at-a-time (no borrow held — it chases `header.next`) | **REPLACE** `sweep_prev_next` with two `Cell<usize>`; **COMPOSE** the two-phase scan/release because an `OwnerVec` is a `RefCell<Vec>` and cannot free under its borrow (the intrusive walk could). |
| `pending_release` + `releasing` window | **partially covered by #260** — see Part B | **COMPOSE.** `pending_release` = sweep/minor dead-box transitional owner (new); `releasing` folds into `collection_inert()` (new); #260's `closed`/`tearing_down`/drain-until-stable stay the teardown owner. |
| Per-transition move/recolor table (`move_finobj_to_tobefnz` no recolor) | **already exact** in the three `move_*` fns (allgc→finobj & tobefnz→allgc recolor `current_white()` when `is_sweep()`; finobj→tobefnz does not) | **COMPOSE.** Recolor rules copied verbatim; only unlink/link mechanics change (tombstone source slot + push dest tail). |
| Cohort counters model `finish_minor_collection` rotation | `FinalizerRegistry` already does `reallyold += old1; old1 = survival; survival = new` for `pending` | **COMPOSE.** OwnerVec rotation mirrors it after compaction (registry precedent claim already withdrawn in rev-3). |
| Pacer bytes exclude ownership storage | `allocate` charges `size_of::<GcBox<T>>()`; sweep refunds `header.size()` | **COMPOSE.** Unchanged mechanism; `owner_capacity_bytes()` diagnostic added for measurement. |
| `#[repr(u8)]` on `Color`/`GcAge` + 8-B `const` size assert (ex-W3) | header currently 24 B; 24-B runtime test | **COMPOSE.** Rides with Phase 5. |

## Part B — the pending_release / releasing decision: **COMPOSE, not REPLACE**

rev-3 specified two teardown/destruction mechanisms **before #260 existed**:
`pending_release` (a sixth heap structure owning dead-in-transit boxes) and a
`releasing: Cell<bool>` window that makes collection entry points inert during a
release drain. #260 then landed its own teardown machinery. The decision is
**COMPOSE** — the two designs address *different windows* and coexist cleanly:

1. **`pending_release` is genuinely required and #260 has no equivalent.**
   The intrusive sweep (`sweep_budgeted`) frees one box at a time by chasing
   `header.next` raw pointers — it holds **no `RefCell` borrow**, so freeing
   inline is safe. An `OwnerVec` is a `RefCell<Vec>`; the sweep scan must borrow
   it to read/tombstone slots, and `release_box` (which runs arbitrary payload
   `Drop`) **must not run under that borrow**. Therefore the scan tombstones dead
   slots and moves their single owning pointer into `pending_release` (a
   *different* `RefCell`, transient borrow), then a release phase drops all owner
   borrows and drains `pending_release` one box at a time. This restores the
   exactly-one-owner invariant *during* destruction (R2 finding 1) and is orthogonal
   to teardown. #260's `drop_all` frees directly from the owner lists and never
   populates `pending_release`.

2. **`releasing` folds into `collection_inert()`; it is a *different* window than
   `closed`.** `closed` is permanent ("teardown has begun"); `releasing` is
   transient ("a sweep/minor/full release drain is in flight"). The batching in
   (1) creates a hazard the old one-at-a-time sweep did not: while draining
   `pending_release`, dead peers A and B are both already tombstoned/absent from
   every owner vec, but B is still pending; A's `Drop` could start a nested
   collection that traces B as a root and then the outer drain frees it (R2
   finding 1, "more severe variants"). The mitigation: set `releasing` for the
   duration of every release drain and make `collection_inert()` return
   `paused || closed || releasing`. Because all entry points already funnel
   through `collection_inert()`, this single change makes them inert during the
   drain with **no per-entry-point edits** — the cleanest possible composition
   with #260. `releasing` is restored by an RAII drop guard (same shape as
   #260's `TearingDownReset`) so a panicking destructor cannot wedge the
   collector shut.

3. **#260's teardown owner stays, with one insertion.** `drop_all` keeps
   `closed`/`tearing_down`, the drain-until-stable loop, and the 10 000-pass
   panic. rev-3's step 1 ("drain `pending_release` first") is added at the top,
   because a sweep that panicked mid-release could have stranded boxes in
   `pending_release`; teardown must free them before zeroing accounting. rev-3's
   "clear grayagain second" is already done by #260's `clear_generation_cursors`
   call. So `drop_all` gains exactly one new first step and is otherwise #260's.

**Consequence for `remember_minor_revisit`:** it stays gated on `closed` only,
**not** `releasing`. A generational barrier fires on a *written* old object,
which is necessarily reachable from the running destructor, hence live, hence
never in `pending_release`; the spec's "barriers touch only colors/ages/grayagain,
no owner-vec access" holds. A `releasing`-gate here would be harmless but
unnecessary. Flagged for the B1/B2 churn tests to probe (barrier-during-release).

## Part C — generational-cursor consumers to delete or convert

Pointer-cursor cells today; all replaced by `usize` cohort counts inside the
relevant `OwnerVec`, or deleted:

| symbol | today | W2 disposition |
|---|---|---|
| `survival`, `old1`, `reallyold` (allgc cursors) | `Cell<Option<NonNull>>` boundaries in the allgc chain | **CONVERT** → `allgc.survival/old1/reallyold: usize` cohort counts (physical-index boundaries). |
| `finobjsur`, `finobjold1`, `finobjrold` (finobj cursors) | same for finobj | **CONVERT** → `finobj.survival/old1/reallyold: usize`. |
| `firstold1` | written in `sweep_young`/`move_tobefnz_to_allgc`, cursor-patched in `correct_generation_pointers`; **read only by one test assert** (`full_sweep_corrects_generation_cursors_when_cursor_object_is_freed:3958`) and by the deleted `correct_generation_pointers` | **DELETE.** Verified unread by any collector decision (spec §"every owner-list move", R2 finding 4). Changelog note. The `firstold1` local in `sweep_young_range` and its `Old1` tracking are deleted with it. |
| `sweep_prev_next` (`Cell<Option<NonNull<Cell<...>>>>`) | raw pointer to the live sweep `Cell` | **DELETE** → `sweep_index: Cell<usize>` + `sweep_watermark: Cell<usize>`. |
| `correct_generation_pointers` | patches all 7 cursors + calls `unlink_grayagain` | **DELETE.** Tombstones never shift indices, so no cursor patch exists. Its only surviving duty (grayagain-entry deletion) is called directly by the sweep-scan and move paths. |
| `set_all_cursors_to_head` (from `promote_all_to_old`) | sets all 7 cursors to list heads | **CONVERT** → `promote_all_to_old`: compact each OwnerVec, then `reallyold = slots.len(); old1 = survival = 0` (all live objects become the old prefix). |
| `clear_generation_cursors` (from `reset_all_ages`, `drop_all`) | nulls all 7 cursors + `clear_grayagain` | **CONVERT** → set all three counts on allgc+finobj to 0 + `clear_grayagain` (tobefnz counts are already 0). |
| `unlink_from_list`'s `sweep_prev_next` rewrite | rewrites cursor when the removed cell held it | **DELETE** (whole `unlink_from_list` deleted — see Part D). |

## Part D — list consumers to convert

| symbol | today | W2 disposition |
|---|---|---|
| `head`, `finobj`, `tobefnz` (`Cell<Option<NonNull>>`) | intrusive-list heads | **REPLACE** with `allgc/finobj/tobefnz: RefCell<OwnerVec>`. |
| `quarantined`, `uncollected` (`Cell<Option<NonNull>>` chained via `header.next`) | intrusive lists | **REPLACE** with `RefCell<Vec<NonNull<...>>>` (append-only, no tombstones/cohorts; drained only in `drop_all`). |
| `allocate` | build box, `header.next = head`, `head = box` | **CONVERT** → push `Some(ptr)` on `allgc.slots` (one amortized push replacing two `Cell` writes). |
| `allocate_uncollected` | prepend to `uncollected` via `header.next` | **CONVERT** → push on `uncollected` Vec. |
| `release_box` (quarantine arm) | poison + prepend to `quarantined` via `header.next` | **CONVERT** → poison + push on `quarantined` Vec. Non-quarantine arm (`Box::from_raw`) unchanged. |
| `for_each_header` / `for_each_list_header` | walk `header.next` on the 3 lists | **CONVERT** → iterate the 3 OwnerVec slot vecs, skip `None`. |
| `type_name_count` | walk 3 lists via `header.next` | **CONVERT** → iterate slots, skip `None`. |
| `allgc_cohort_stats` | cursor-compare walk of the allgc chain | **CONVERT** → index arithmetic on `allgc` cohort counts (count non-None per cohort range). |
| `allgc_count` | returns `objects.get()` | **UNCHANGED** (counter, not a walk). |
| `link_to_head`, `link_to_tail` | intrusive insert | **DELETE** → OwnerVec `push` (tail). |
| `unlink_from_list` | intrusive remove + cursor/grayagain fixups | **DELETE** → OwnerVec tombstone-at-slot + direct `unlink_grayagain`. |
| `move_allgc_to_finobj` / `move_finobj_to_tobefnz` / `move_tobefnz_to_allgc` | unlink + recolor + link | **CONVERT** → tombstone source slot (linear membership scan, cold path, density-bounded) + recolor per the M-table + push dest tail; `firstold1` special-case dropped. Public signatures + `-> bool` unchanged (lua-vm calls them at `state.rs`/`api.rs`). |
| `sweep_budgeted` | budgeted `sweep_prev_next` chain walk, inline free | **REPLACE** → two-phase slot scan over `slots[sweep_index..sweep_watermark]` → `pending_release` → drain. |
| `sweep_young_range` / `sweep_young` | two chain walks per list via `header.next` | **REPLACE** → slot-range scans of `allgc.slots[reallyold+old1..]`, `finobj.slots[reallyold+old1..]`, all of `tobefnz.slots`; two-phase release; compact + rotate cohorts at end. |
| `drop_all` / `drop_list` | drain 5 intrusive lists until stable | **CONVERT** → `drop_all` gains a `pending_release` drain first; `drop_list` becomes an OwnerVec/Vec `mem::take`-and-free. #260 drain-until-stable + pass-cap kept. |
| `start_cycle` repaint, `abort_cycle`, `reset_all_ages`, `promote_all_to_old` | `for_each_header` iterate | **CONVERT** via the new `for_each_header`. `start_cycle` also arms `sweep_index=0`/`sweep_watermark=len` at the atomic transition (in `run_atomic`), not at `start_cycle`. |
| `run_atomic` | sets `sweep_prev_next = &head` | **CONVERT** → `sweep_index=0; sweep_watermark = allgc.slots.len()`. Re-arm per phase entry for finobj/tobefnz. |

External consumers verified: only `move_*` (7 call sites in `lua-vm/src/state.rs`
+ `api.rs`) — signatures unchanged. No code outside `heap.rs` reads
`header.next`/cursors (grep of lua-vm/lua-types/lua-stdlib/lua-rs-runtime).

## Part E — tests to preserve unchanged vs translate

- **Preserve UNCHANGED (behavior pins):** `g3_baseline_child_freed_after_move_*`
  (the #263 baseline, pins inherited liveness), `grayagain_links_object_once`,
  `grayagain_list_carries_old1_until_old`, `grayagain_list_carries_touched2_
  until_old`, `full_sweep_unlinks_freed_grayagain_entries`,
  `cross_list_move_deletes_grayagain_entry`, `drop_all_clears_grayagain_before_
  freeing`, `minor_collect_frees_young_and_keeps_old`, `minor_collect_skips_
  untouched_old_root_scan_work`, `promote_and_reset_all_ages`, `allocate_
  uncollected_*`, `allocate_into_closed_heap_panics`, the #260 teardown tests.
  These call only public API + `allgc_count`/`grayagain_count`/`age`/stats, which
  are representation-independent — they must stay green with **no edits**.
- **Translate (cursor asserts → counter/index asserts):**
  `full_sweep_corrects_generation_cursors_when_cursor_object_is_freed` reads
  `heap.survival.get()/old1/reallyold/firstold1` (`Option<NonNull>`). After
  conversion these fields no longer exist; rewrite to assert the three allgc
  cohort counts are 0 after a full sweep frees everything, and drop the
  `firstold1` assert. `minor_sweep_uses_generation_cursors_to_skip_old_tail`
  uses only `last_sweep_stats` counts — **no edit**.
- **New kit tests (Phase 1 + Phase 6):** OwnerVec unit tests (append, tombstone,
  compact, B2 churn, density bound); M1/M2/M3 ×3 transitions; A1 extended;
  B1 budget-1 resumption; B2 adversarial churn; F1 FIFO sync; G3 graph-shaped
  (currently `#[ignore]`d as #263 baseline gap — port as-is); Q1 quarantine; U1
  uncollected; C1 cohort rotation.

## Part F — spec-vs-code divergences found in Phase 0

1. **rev-3 predates #260.** The whole `pending_release`/`releasing`/drain-until-
   stable teardown section of rev-3 is written as if teardown had no machinery;
   #260 built drain-until-stable, `closed`/`tearing_down`, `collection_inert`
   gating, and the pass-cap. Resolved by the COMPOSE decision in Part B (no
   design conflict — the pieces slot together).
2. **rev-3 header baseline is 40 B; landed is 24 B.** rev-3's "remove both
   links" is now "remove the one remaining link (`next`)"; the 40→8 narrative is
   really 24→8. The Phase-5 runtime assert changes 24→8 and the ex-W3 `const`
   8-B assert lands then.
3. **`firstold1` unread confirmed.** rev-3 asserts it; verified against the
   file — the only reader is one test assert (line 3958) and the to-be-deleted
   `correct_generation_pointers`. Deletion is behavior-neutral.
4. **`allgc_cohort_stats` already exists** as a cursor-walk (not mentioned by
   rev-3 by that name; rev-3 says "cohort stats become index arithmetic") — it
   converts to counter arithmetic. No behavior change (it is lua-cli telemetry).
5. No mechanism in rev-3 W2 **conflicts** with landed code; every one either
   composes as a fresh addition or converts a representation. The high-risk
   surface is exactly the collector spine (sweep + minor + moves + teardown),
   as rev-3 rated it MEDIUM-HIGH.
