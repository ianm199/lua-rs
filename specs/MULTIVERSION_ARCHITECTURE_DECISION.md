# Multi-Version Architecture Decision — testing the working assumptions

Status: decision doc. Consolidates and adjudicates the three port specs
(`WEBLUA_MULTIVERSION_API_SPEC.md`, `LUA_5_3_AND_5_5_PORT_SPEC.md`,
`LUA_5_1_PORT_SPEC.md`) and the five research files in `specs/research/`
(`mlua-api-seams.md`, `lua-rs-current-seams.md`, `5.3-upstream-delta.md`,
`5.5-upstream-delta.md`, `5.1-5.2-upstream.md`).

Purpose: take the project's six working assumptions and return a verdict on each
— CONFIRMED / REFUTED / NUANCED — backed by specific evidence already gathered,
then commit to a sequencing, a tradeoffs table, the key API decisions, and the
first concrete steps.

External anchors (load-bearing only; full citations live in the research files):
Lua manuals <https://www.lua.org/manual/5.1/> … `/5.5/manual.html`; upstream
tags <https://github.com/lua/lua/tags>; mlua <https://github.com/mlua-rs/mlua>.

---

## 0. The six assumptions, at a glance

| # | Assumption | Verdict |
|---|---|---|
| a | Number-model boundary (float-only vs dual) is the primary axis → ~2 cores, not 5 or 1 | **CONFIRMED** |
| b | A single parameterized "modern core" hosts 5.3/5.4/5.5 cleanly via config seams | **NUANCED** (true everywhere except the ISA, which is a real fork) |
| c | No upfront shell-extraction; the right seams emerge from adding the 2nd version | **NUANCED** (one cheap prerequisite refactor is mandatory first) |
| d | 5.5 vs 5.3 as the easiest/best FIRST target | **REFUTED** (5.3 first; 5.5 is the hard one) |
| e | Runtime version multiplexing (one binary, switchable) is feasible and worth it | **CONFIRMED** |
| f | UserData / derive / Scope are version-invariant and transfer for free | **CONFIRMED** (one below-API caveat: `__lt`-as-`__le`) |

---

## 1. Assumption-by-assumption verdicts

### (a) Number model is the primary axis → ~2 cores — **CONFIRMED**

The codebase audit and the upstream deltas converge: the **dual int/float number
model** (`LuaValue::Int(i64) | Float(f64)`, `lua-types/src/value.rs:13-24`) is
identical across 5.3/5.4/5.5 and **broken only by 5.1/5.2**, which are float-only
(`LUA_NUMBER = double`, one `number` type, no `math.type`)
(`lua-rs-current-seams.md` rows "Value enum"/"Integer subtype"; `5.1-5.2-upstream.md`
§1). mlua independently confirms this is *the* deepest seam: "Integer width & the
integer/float distinction … Decide it first" (`mlua-api-seams.md` §4, seams #1–#5).

The audit's closing section states it directly: the modern family shares the
value core; "**5.1 is the exception … should be a separate core, not a parameter
of the modern core**" (`lua-rs-current-seams.md` §"One parameterized modern core?").
This yields exactly the predicted two-core partition: a **modern core
{5.3, 5.4, 5.5}** on the dual subtype, and a **legacy core {5.1, 5.2}** on
float-only. Five separate cores would fork the version-invariant value/GC/runtime
layers for no divergence; one monolithic core cannot absorb the float-only value
model without the "never construct `Int`" hazard pervading every modern callsite.
Two cores is the structurally-forced answer.

One refinement the evidence forces: the boundary is **number model *plus* globals
model**, and the two do not coincide. 5.2 is float-only (legacy number model) but
already uses modern `_ENV` globals (`5.1-5.2-upstream.md` §2; `LUA_5_1_PORT_SPEC.md`
§3). So 5.2 is the *bridge* that sits on the legacy-core side of the number axis
but the modern side of the globals axis. The two-core split is still correct; 5.2
just proves the float-only core in isolation before 5.1 adds the globals fork.

### (b) One parameterized modern core hosts 5.3/5.4/5.5 cleanly — **NUANCED**

True for everything **except the bytecode ISA**, which is a genuine fork, not a
config flag. The audit confirms `LuaValue`, arithmetic/coercion, table/metamethod
dispatch (`lua-vm/src/vm.rs:893-985`), the GC engine (`lua-gc/src/heap.rs`), the
`_ENV` model, and the entire embedding API carry **zero** version coupling across
the modern family (`lua-rs-current-seams.md` §"One parameterized modern core?";
`LUA_5_3_AND_5_5_PORT_SPEC.md` §1.1). Those are shared, parameterized by one
`LuaVersion` enum threaded from `Lua::new`.

But "config seam" undersells the ISA. The compiler `OpCode`/`OP_MODES`/`Instruction`
(`lua-code/src/opcodes.rs:87,337,484`) and the VM dispatch loop (`lua-vm/src/vm.rs`
`execute`) are 5.4-specific and **do not overlap cleanly** with 5.3 (6-bit op +
RK encoding) or 5.5 (opcode count 83→85, reordered `OP_SHRI`/`OP_SHLI`, new `ivABC`
operand mode, `OP_GETVARG`/`OP_ERRNNIL`) (`lua-rs-current-seams.md` "Opcode set"
rows; `5.5-upstream-delta.md` §4). The audit names this "the one axis where 'share
aggressively' hits a wall."

The escape hatch that keeps this NUANCED rather than REFUTED: because we are past
C-mirroring, the **internal** ISA need only be *behaviorally* faithful — only
`string.dump` bytes are an observable structural oracle. So 5.3 can reuse the
shared internal opcodes and only emit a `0x53` dump header
(`LUA_5_3_AND_5_5_PORT_SPEC.md` §2.1), collapsing most of 5.3's ISA cost. 5.5
cannot dodge it: named varargs (`OP_GETVARG`) and the undeclared-global check
(`OP_ERRNNIL`) require real net-new codegen + VM support regardless of dump
faithfulness (`5.5-upstream-delta.md` §4). Verdict: one shared core is right; the
ISA is a true seam (cheap for 5.3, expensive for 5.5), not a mere parameter.

### (c) No upfront shell-extraction; seams emerge empirically — **NUANCED**

Mostly endorsed, with one mandatory exception. The specs agree the *grammar /
stdlib / GC-knob* seams should emerge from adding the second version rather than
being speculatively extracted — the recommended path is "a second thin dispatch
body reusing shared helpers," and the embedding API needs **no seam at all**
(`lua-rs-current-seams.md` rows "VM dispatch", "Embedding API"). To that extent the
assumption holds: do not pre-build five abstract backends before the second
version teaches you where the real seams are.

The exception is non-negotiable and both specs flag it as a *prerequisite*: the
duplicated `OpCode` enum must be consolidated **before** any version work.
`lua-vm` does not depend on `lua-code`; it carries its own Phase-B stub copy of
the 5.4 opcode enum (`lua-vm/src/vm.rs:45-120` vs `lua-code/src/opcodes.rs:87`),
so two definitions must stay in lockstep (`lua-rs-current-seams.md` "Opcode set
(VM side) — DUPLICATE"; `LUA_5_3_AND_5_5_PORT_SPEC.md` §1.3 "Consolidation gate").
"Building a version seam on top of a duplicated enum doubles every ISA change."
This is not shell-extraction (it adds no abstraction); it removes an existing
duplication and is oracle-gated as behavior-preserving against the current 5.4
suite. So: emergent seams — yes; one cheap consolidation refactor up front — also
yes.

### (d) 5.5 as the easiest/best FIRST target — **REFUTED**

The evidence points the opposite way: **5.3 first.** The decisive factor is
subtractive-and-frozen vs additive-and-stateful. 5.3 is a strict simplification of
the language we already run — remove `<const>`/`<close>`, restore old `for`/
coercion/`__le` semantics, gate the stdlib, add `bit32` — with **no new parser
grammar, no new opcode operand mode, no new scope model**, against a 20-year-stable
oracle (`LUA_5_3_AND_5_5_PORT_SPEC.md` §0, §5). 5.5's headline change is a genuinely
*stateful* compile-time global-declaration scope model (implicit `global *`, voided
by an explicit decl, re-enabled by `global *`, undeclared-name compile errors),
plus the `ivABC` operand mode and two new opcodes — net-new work in the hardest
layers (parser/codegen/VM) (`5.5-upstream-delta.md` §1,§4, risks #1–#3).

The only argument for 5.5-first is its smaller *raw* delta table from 5.4, but that
metric is wrong: 5.5's small delta lands entirely in the expensive layers, while
5.3's larger delta is mostly cheap stdlib/semantic gating
(`LUA_5_3_AND_5_5_PORT_SPEC.md` §5.4). Install base agrees — 5.3 has a far larger
deployed embedding base than a ~5-month-old 5.5 (released 22 Dec 2025). Doing 5.3
first also *shakes out the version-config machinery* (the `LuaVersion` seam, multi-
version `source.toml`, per-version stdlib/error tables, the 3-way oracle matrix) at
low risk before betting it on 5.5's stateful parser. Assumption d is refuted; the
correct first target is 5.3.

### (e) Runtime version multiplexing is feasible and worth it — **CONFIRMED**

This is the pure-Rust differentiator and it is structurally real. mlua **cannot**
do it: its `build.rs` `compile_error!`s on more than one version feature, it links
one C library, and `pub use lua54::*` bakes the version into every call site; worse,
`lua_Integer` width changes the ABI of nearly every function so "no single Rust
signature serves both i32-based 5.1 and i64-based 5.4" (`mlua-api-seams.md` §3).
lua-rs has **no C symbols and no foreign ABI** — each backend is a Rust module
behind a seam, so one binary can hold the 5.1/5.3/5.4/5.5 VMs and pick one per
`Lua` instance at runtime (`mlua-api-seams.md` §3 "Why a pure-Rust implementation
CAN multiplex"; `WEBLUA_MULTIVERSION_API_SPEC.md` §1.1). The payoff is the WASM
playground headline: **one `.wasm`, a dropdown that switches versions live**, no
recompile, no second download.

"Worth it" survives the honest cost accounting because the costs are bounded and
opt-out: (1) binary/`.wasm` size ~ N× the *VM* code, mitigated by aggressive
sharing and by compile-time features that *subtract* backends for size-sensitive
embeds; (2) a superset `Value` whose number discipline is enforced per-active-
backend at the marshaling seam; (3) version-absent features become a typed runtime
`Unsupported` error instead of a compile error; (4) negligible per-call dispatch
cost (`mlua-api-seams.md` §3; `WEBLUA_MULTIVERSION_API_SPEC.md` §1.1, §4.1). The
`enum Engine` with `#[cfg]`-gated variants makes the single-backend build collapse
to mlua-class performance, so multiplexing is upside-only when off. Confirmed.

### (f) UserData / derive / Scope are version-invariant — **CONFIRMED** (one caveat)

The audit places `UserData`/`UserDataMethods` (`lua-rs-runtime/src/lib.rs:2015,2029`),
the derive surface (`#[derive(LuaUserData)]`, `#[lua_methods]`, `#[lua_impl(...)]`),
and `Lua::scope` (`:454`) squarely in the version-invariant column — none encode a
Lua version (`lua-rs-current-seams.md` "Embedding API" row;
`WEBLUA_MULTIVERSION_API_SPEC.md` §3, §3.1, §3.2). UserData binds Rust types via
real metatables keyed by `TypeId`; the metamethods the derive targets
(`__index`/`__newindex`/`__tostring`/`__eq`/`__lt`/`__le`/`getmetatable`) exist in
every version 5.1→5.5. Scope's borrow-lending and the handle-provenance check are
version-agnostic, and that same provenance machinery enforces the monomorphic-
instance rule for free (no per-handle version field needed)
(`WEBLUA_MULTIVERSION_API_SPEC.md` §4.2).

The one caveat is **entirely below the API**: `__lt` emulates `__le` in 5.1/5.2/5.3
but was removed in 5.4+, so a userdata defining only `__lt` answers `a <= b` in 5.3
but errors in 5.4 (`5.3-upstream-delta.md` §6; `5.1-5.2-upstream.md` §6). The derive
*registers the same metamethods regardless*; the fallback lives in the backend's
comparison path. Host code and the macro are unchanged across versions, so the
transfer-for-free claim holds at the API surface. Confirmed.

---

## 2. Recommended sequencing

Dependency order (a prerequisite, then easy→hard within each core):

| Step | Work | Why here |
|---|---|---|
| **0** | **Consolidate the duplicate `OpCode`** to one owner in `lua-code`; make `lua-vm` depend on it; route VM decode through the shared def. Land the `LuaVersion` enum in `lua-types`. Oracle-gated as behavior-preserving on the 5.4 suite. | Prerequisite (assumption c). Building any ISA seam on a duplicated enum doubles all future ISA work. |
| **1** | **5.3** — the cheap modern sibling. Gate off `<const>`/`<close>`; restore `for`-wrap, decimal-literal-wrap, `__le`-via-`__lt`, string-coercion-in-core; swap `math.random`; add `bit32`; per-version stdlib/GC roster; `0x53` dump header + 5.3 error table. | Subtractive + frozen + largest install base (assumption d). Proves the entire version-config chassis at lowest risk. |
| **2** | **5.5** — the hard modern sibling. Stateful `global`-decl scope model (+ `LUA_COMPAT_GLOBAL` axis), read-only `for` var, named vararg tables, `ivABC` operand mode, `OP_GETVARG`/`OP_ERRNNIL`, `table.create`, `collectgarbage("param", …)`, round-trip float `tostring`. | Reuses the now-proven seams; spends the net-new parser/codegen/VM budget once. |
| **3** | **5.2** — the float-only bridge. Modern `_ENV`/goto/closures reused as-is; force float-only value model (the "never construct `Int`" invariant + numeric-formatting kit); 5.2 stdlib (`bit32`, `table.unpack`, `__len`/`__gc`/`__pairs` on tables, hex floats, `\x`/`\z`, `package.searchers`). | First member of the *second* core. Proves float-only in isolation from the globals fork, against a modern-shaped 5.2.4 test bundle. |
| **4** | **5.1** — last, the "dessert." 5.2 minus `_ENV`/goto/escapes/`bit32`, plus restore `fenv` globals (lower `OP_GETGLOBAL`/`SETGLOBAL` into the table-access path + observable `getfenv`/`setfenv`), legacy stdlib add-back, flip `__len`/`__gc`/`__pairs` to userdata-only/absent, `newproxy`. | Largest single effort: float-only **plus** a new globals subsystem **plus** the biggest legacy-stdlib surface **plus** the metamethod flip **plus** the weakest oracle (hand-curated corpus, no `all.lua`). |

Rationale in one line: build the chassis on the easy version (5.3), spend the
hard modern budget once (5.5), then open the second core on its easy member (5.2)
before paying for 5.1's globals fork last.

---

## 3. Consolidated tradeoffs table

| Axis | 5.3 | 5.5 | 5.2 | 5.1 |
|---|---|---|---|---|
| Core | modern | modern | legacy (float-only) | legacy (float-only) |
| Value model | shared (no work) | shared | **force float-only (pervasive)** | float-only (shared w/ 5.2) |
| Globals | shared `_ENV` | shared `_ENV` | shared `_ENV` (no work) | **fenv + legacy ops (new subsystem)** |
| Parser/grammar | gate off `<const>`/`<close>` (low) | **stateful global-decl scope (high)** | reuse modern + goto (low) | legacy name resolution branch (med) |
| Opcode ISA | reuse internal, `0x53` dump (low) | **`ivABC` + 2 new ops, reordered (high)** | reuse modern (low) | legacy `GETGLOBAL`/`LOADNIL` shape or lower (med) |
| Stdlib | bit32, math aliases, gated bodies (low–med) | table.create, GC param, round-trip float (low–med) | bit32, unpack move (med) | **large legacy add-back (high)** |
| Metamethods | restore `__le`-via-`__lt` (low) | shared w/ 5.4 (none) | add table `__len`/`__gc`/`__pairs` | **flip `__len`/`__gc`/`__pairs` back (med)** |
| GC knobs | reject gen/inc params (low) | `collectgarbage("param")` reshape (low) | "step doesn't restart" rule (low) | gcinfo/quirks (low) |
| Oracle | reuse 5.4 suite shape; 5.3.4-tests skew | v5.5.0 testes, two compat builds | modern-shape 5.2.4 bundle ✔ | **no modern suite → curated corpus** |
| Net effort | **Medium** | **High** | modern-sibling-sized | **largest of all** |
| Net risk | Low–Med | Med–High | Med | High |

Top landmines (ranked, from the specs): (1) the "never construct `Int`" invariant
across dozens of producers — guard with a forbidden-pattern hook + numeric kit from
day one; (2) `__len` on tables silently does nothing in 5.1; (3) observable `fenv`
semantics (`setfenv(0,t)`, per-closure env ≠ global table); (4) weak 5.1 oracle;
(5) 5.5's stateful scope model + `LUA_COMPAT_GLOBAL` second axis; (6) `math.random`
PRNG divergence per version.

---

## 4. Key API decisions

1. **Selection = runtime `LuaVersion` enum (default) + compile-time features that
   only *subtract* backends + NO `Lua<V>` type parameter.** Runtime selection is
   the headline (one `.wasm`, live switch). Features invert mlua's model: additive
   and optional, default "all on," collapsing to a single-variant `enum Engine` —
   mlua-class perf — when one backend is compiled in. A type parameter is rejected:
   it destroys multiplexing (can't hold a heterogeneous set), poisons every
   downstream signature, and the sharp divergences are *behavioral* not *surface*
   so it buys a weak guarantee at a large ergonomic tax
   (`WEBLUA_MULTIVERSION_API_SPEC.md` §1.1–1.3).

2. **One `Lua` instance is monomorphic in version; multiplexing is *across*
   instances.** A playground holds four `Lua` instances and routes each chunk to
   the matching one. Handles carry provenance to their parent instance (existing
   RAII-root machinery), so they are transitively version-tagged with no per-handle
   field, and cross-version handle mixing is rejected by the same Scope-invalidation
   check that guards escaped refs (`WEBLUA_MULTIVERSION_API_SPEC.md` §1.2, §4.2).

3. **`Value` is a superset marshaling currency; the i64↔f64 crossing is owned by one
   engine-aware seam, single source of truth, no fallback.** On dual-subtype engines
   `Value::Integer(i64)` round-trips exactly; on float-only engines it must widen to
   `f64`, and inexact widening (|i| > 2^53) is a typed `LossyIntConversion` error by
   default (`LossyIntPolicy::{ErrorOnInexact, Truncate}`), never silent truncation.
   Lua→host `i64` accepts a float only when exactly integral and in range
   (`WEBLUA_MULTIVERSION_API_SPEC.md` §2.1–2.2). This honors the global "no fallback"
   rule.

4. **Internal dispatch = closed `enum Engine` with `#[cfg]`-gated variants; `Backend`
   trait is the seam *contract*, not `Box<dyn>`.** The backend set is closed and
   known at compile time; the enum models that, feature-gating falls out for free, no
   vtable/alloc on the hot VM loop, and variants hold version-specific concrete state
   (`WEBLUA_MULTIVERSION_API_SPEC.md` §4.1).

5. **Version-absent features → typed `LuaError::Unsupported { feature, version }`;**
   a machine-readable divergence registry makes the support matrix docs.rs-renderable.
   Error *message text* comes from a **per-backend** table (the version-matched
   `errors.lua` is the oracle for those exact strings), never one shared table
   (`WEBLUA_MULTIVERSION_API_SPEC.md` §3.3–3.4).

6. **Uniform intent, hidden mechanism for divergent-but-equivalent operations.**
   `set_environment` is one host verb routing to `_ENV` (5.2+) or `setfenv` (5.1);
   `StdLibSet::ALL` resolves against the active version's roster; GC tuning is one
   surface that gates internally. Name the version only when the *intent itself*
   doesn't exist, and then return `Unsupported`
   (`WEBLUA_MULTIVERSION_API_SPEC.md` §5.3, §5.5).

---

## 5. First concrete steps

1. **Add `LuaVersion` to `lua-types`** (lowest shared crate; no dep cycle) with
   `number_model()`, `version_str()`, `luac_version_byte()`. Thread it from the
   embedding entry points (`lua-rs-runtime/src/lib.rs` `Lua::new`/`try_new`/
   `with_hooks`, `:725-735`); keep `Lua::new()` defaulting to 5.4 for back-compat.
   No version appears in any embedding-API type.
2. **Consolidate the duplicate `OpCode`** (`lua-code/src/opcodes.rs:87` vs the stub
   `lua-vm/src/vm.rs:45-120`): one owner in `lua-code`, `lua-vm` depends on it, VM
   decode routes through the shared definition. Oracle-gate: full 5.4 suite stays
   green (behavior-preserving).
3. **Introduce `enum Engine` + `Backend` trait with the single 5.4 variant.** Wire
   `LuaBuilder::version`, `Lua::version()`, the `Value` marshaling seam, and the
   `Unsupported`/`LossyIntConversion` error variants. Pure refactor, 5.4-oracle-gated.
4. **Make `harness/source.toml` multi-version** (`[source.lua53]`/`[source.lua54]`/…),
   add a `--version` flag to the runner scripts and a `--lua-version` flag to the
   lua-rs CLI, and add a `run_official_all.sh --version {53,54,55}` matrix gate so a
   shared-core edit re-runs all version oracles before landing.
5. **Pin the 5.3 oracle:** reference binary `lua-5.3.6`
   (<https://www.lua.org/ftp/lua-5.3.6.tar.gz>, compat flags on), test suite
   `lua-5.3.4-tests` (<https://www.lua.org/tests/lua-5.3.4-tests.tar.gz> — no 5.3.6
   tarball exists; record the deliberate skew in `source.toml`). Quarantine
   `code.lua`/C-API-heavy files and exact-value RNG asserts.
6. **Begin the 5.3 backend** behind the new seam, easiest axes first (stdlib roster,
   `_VERSION`, dump header byte), then the semantic restorations (`for`-wrap,
   `__le`-via-`__lt`, string-coercion-in-core), guarding every shared-path change
   behind the version flag and re-running the 5.4 suite after each.
