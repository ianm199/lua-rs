# WebLua — Unified Multi-Version Embedding API Spec

> Codename **WebLua** (NOT final). The product is the embedding API; the Lua
> language version is a *backend*. Host Rust code must look **identical** across
> Lua 5.1 / 5.2 / 5.3 / 5.4 / 5.5 except where the semantics genuinely differ —
> and at those points the divergence must be explicit, typed, and discoverable,
> never silent.

Status: design spec. Supersedes nothing yet; it is the target the existing
single-version 5.4 runtime (`crates/lua-rs-runtime`) evolves toward.

This spec is grounded in five research documents in `specs/research/`:
`mlua-api-seams.md`, `lua-rs-current-seams.md`, `5.3-upstream-delta.md`,
`5.5-upstream-delta.md`, `5.1-5.2-upstream.md`. Every external behavioral claim
traces to those (which in turn cite lua.org and upstream source); inline URLs
are repeated here only where they are load-bearing for an API decision.

External references used directly:
- Lua manuals: <https://www.lua.org/manual/5.1/>, `/5.2/`, `/5.3/`, `/5.4/`, `/5.5/manual.html>`
- Upstream source tags: <https://github.com/lua/lua/tags>
- mlua (compile-time multi-version embedding prior art): <https://github.com/mlua-rs/mlua>

---

## 0. The thesis, stated as constraints

Three working assumptions, each turned into a hard API constraint:

1. **The embedding API is the product, the version is a backend.** → No public
   type in the API may be parameterized by, or named after, a Lua version.
   `Lua`, `Value`, `Table`, `Function`, `AnyUserData`, `LuaError`, `Scope`,
   `FromLua`/`IntoLua` are all version-invariant. The version lives *below* the
   API, selected at construction.

2. **Share code aggressively; split only at genuine divergences.** The codebase
   audit (`lua-rs-current-seams.md`) confirms the divergence surface is *narrow*:
   value core, arithmetic, table/metamethod machinery, GC engine, the `_ENV`
   model, and the entire embedding API are already version-invariant across the
   modern family. The genuine splits are the **bytecode ISA + VM dispatch**, a
   thin shell of **parser grammar** (attributes, `global` decl), **stdlib roster**,
   **GC knobs**, and — for 5.1/5.2 — the **number model** and **globals model**.

3. **Upstream official suites are the ORACLE, not the structure.** Each version's
   suite validates *that backend's* behavior; none dictates the unified API's
   internal shape. We are past the C-mirroring phase. Internal bytecode need not
   match upstream; only observable behavior (and, where we opt in, `string.dump`
   bytes as a structural oracle) must.

---

## 1. Version selection — runtime enum *and* compile-time gating (BOTH)

### 1.1 Decision

**Adopt all three of: a runtime `LuaVersion` enum (default), a compile-time
feature set that *excludes* backends, and NO version type-parameter on public
types.** Concretely:

- **Runtime selection is the default and the headline.** A binary that compiles
  in multiple backends picks the version per `Lua` instance at runtime:
  ```rust
  let lua = Lua::builder().version(LuaVersion::V5_4).build()?;
  ```
  This is the WASM-playground differentiator: **one `.wasm` artifact, a dropdown
  that switches Lua 5.1/5.3/5.4/5.5 live, no recompile, no second download.**
  mlua *structurally cannot* ship this — it links one C library and bakes the
  version into every call site via `pub use lua54::*`
  (`mlua-api-seams.md` §3). lua-rs has no C symbols and no foreign ABI, so every
  backend is just a Rust module behind a seam and they coexist in one binary.

- **Compile-time features only *subtract*.** Cargo features gate backends *out*
  for size-sensitive embeds (small WASM, a server pinning one version):
  ```toml
  # all backends (default) — full runtime multiplexing
  weblua = "x"
  # slim single-version build — only 5.4 code is compiled in
  weblua = { version = "x", default-features = false, features = ["lua54"] }
  ```
  This inverts mlua's model: mlua's features are *mutually exclusive and required*
  (its `build.rs` `compile_error!`s on >1). Ours are *additive and optional*; the
  default is "all on." With exactly one feature on, the backend dispatch collapses
  to a single arm and the design degrades gracefully to mlua-style ergonomics with
  zero runtime version cost.

- **No type parameter.** We reject `Lua<V5_4>` / `Lua<V: Version>`. Rationale in
  §1.3.

### 1.2 The monomorphic-instance rule (the key invariant)

> **One `Lua` instance is monomorphic in version. Multiplexing happens *across*
> instances, never within one.**

A given `Lua` value is bound to exactly one `LuaVersion` for its entire life.
`lua.version()` is fixed at `build()`. Every handle (`Table`, `Function`,
`AnyUserData`, …) carries provenance back to its parent instance (§4.2) and is
therefore implicitly version-tagged by that parent. You never mix a 5.3 `Table`
into a 5.4 `Lua`; the handle-provenance check (already present in the codebase
as the Scope-invalidation machinery) rejects it as a clean runtime error.

A playground hosting 5.1/5.3/5.4/5.5 simultaneously holds **four `Lua`
instances** and routes each chunk to the matching one. This keeps every backend
internally consistent (a 5.1 backend only ever sees float-only values; a 5.4
backend only ever sees its dual-subtype values) while the *host-facing* API is
identical across all four.

### 1.3 Why a runtime enum and NOT a type parameter `Lua<V>`

A type parameter (`Lua<V5_4>`) would buy compile-time "this method doesn't exist
on 5.3" guarantees, mirroring mlua's `#[cfg]` removal. We reject it because:

- **It destroys runtime multiplexing.** `Lua<V5_1>` and `Lua<V5_4>` are different
  types; you cannot store a heterogeneous set of them in `Vec<Lua>` or behind one
  `dyn` for the playground without erasing the parameter anyway — at which point
  the parameter bought nothing and cost ergonomics.
- **It poisons every downstream signature.** Host code, derive macros, and
  `FromLua`/`IntoLua` impls would all have to be generic over `V` or pick one,
  re-introducing exactly the per-version source forking the thesis forbids.
- **The guarantee it buys is weak in practice.** The sharp divergences (number
  model, `math.random` stream, error-message wording) are *behavioral*, not
  *surface* — a type parameter cannot encode "5.1 widens i64 to f64 lossily." So
  it pays a large ergonomic tax for a guarantee that doesn't cover the hazards
  that actually bite.

Instead, **version-absent features return a typed runtime error**
(`LuaError::Unsupported { feature, version }`, §3.4) — the runtime analog of
mlua's `#[cfg]`. Per-method version support is documented and machine-derivable
from the backend trait (so docs.rs can render a support matrix the way mlua's
`doc(cfg(...))` does).

### 1.4 Construction surface

```rust
#[derive(Copy, Clone, PartialEq, Eq, Debug, Hash)]
#[non_exhaustive]
pub enum LuaVersion {
    V5_1,
    V5_2,
    V5_3,
    V5_4,   // the implemented baseline today
    V5_5,
}

impl LuaVersion {
    /// Family-level numeric model. The single sharpest behavioral axis.
    pub fn number_model(self) -> NumberModel {
        match self {
            LuaVersion::V5_1 | LuaVersion::V5_2 => NumberModel::FloatOnly,
            LuaVersion::V5_3 | LuaVersion::V5_4 | LuaVersion::V5_5 => NumberModel::DualSubtype,
        }
    }
    pub fn as_str(self) -> &'static str { /* "Lua 5.4" etc. for _VERSION */ }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum NumberModel { FloatOnly, DualSubtype }

pub struct LuaBuilder { /* version, hooks, opened libs, gc opts, sandbox env */ }

impl LuaBuilder {
    pub fn version(self, v: LuaVersion) -> Self;
    pub fn hooks(self, hooks: HostHooks) -> Self;
    pub fn std_libs(self, set: StdLibSet) -> Self;        // §5.3
    pub fn build(self) -> Result<Lua>;                     // fails if backend not compiled in
}

impl Lua {
    /// Back-compat shorthand. Defaults to the latest fully-supported version.
    pub fn new() -> Self;                                  // == builder().version(default()).build().unwrap()
    pub fn builder() -> LuaBuilder;
    pub fn version(&self) -> LuaVersion;                   // fixed for this instance's life
}
```

`build()` returns `Err(LuaError::Unsupported { feature: "backend", version })`
when the requested version's backend was feature-gated out — the one place
compile-time gating becomes a runtime error, and the only honest way to keep the
API uniform whether or not a backend is present.

---

## 2. The `Value` boundary — superset marshaling currency + the number-model seam

### 2.1 `Value` is marshaling currency only

`Value` is the **host↔Lua marshaling currency**, NOT the engine's internal
representation. Each backend keeps its own internal value type (the 5.4 VM's
tagged value, a future 5.1 VM's float-only value). `Value` is the *union* of all
versions' value spaces, produced only at the API boundary when a value crosses
into host Rust and consumed only when a host value crosses into Lua.

The codebase already has exactly this shape
(`lua-rs-runtime/src/lib.rs:1553`) — confirmed by audit:

```rust
pub enum Value {
    Nil,
    Boolean(bool),
    Integer(i64),          // present even though 5.1/5.2 have no integer subtype
    Number(f64),
    String(LuaString),
    Table(Table),
    Function(Function),
    UserData(AnyUserData),
    LightUserData(*mut c_void),
    Thread(Thread),
}
```

`Integer(i64)` and `Number(f64)` are *both* variants of the one superset enum.
This is the right shape (`lua-rs-current-seams.md` confirms it is already
version-invariant). The discipline that makes it correct across versions is
enforced **at the marshaling seam against the active backend's `NumberModel`**,
not by the type — exactly the cost called out in `mlua-api-seams.md` §3.

### 2.2 The engine-aware number-model marshaling seam (the sharpest hazard)

This is the single most dangerous seam in the whole design. 5.1/5.2 are
**float-only**: one `number` type, every numeric value is `f64`, there is no
integer subtype and `math.type` does not exist
(`5.1-5.2-upstream.md` §1). 5.3/5.4/5.5 have the **dual subtype**. So the i64/f64
boundary behaves differently per instance, and a naive "just store i64" silently
corrupts a 5.1 program.

**The contract is owned by one function on the active backend, and there is a
single source of truth — no fallback.**

#### 2.2.1 Host → Lua (ingest): `i64` crossing into a float-only engine

```rust
/// How a host integer that has no exact f64 representation is handled when it
/// crosses into a FloatOnly (5.1/5.2) engine. There is no integer Lua value to
/// receive it, so it MUST widen to f64; values with magnitude > 2^53 lose
/// precision. We surface this rather than paper over it.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum LossyIntPolicy {
    /// Default. Widen to f64 silently when exact; ERROR on inexact widening.
    /// "Correct or loud" — never silently lossy.
    ErrorOnInexact,
    /// Opt-in: widen to f64 even when inexact, recording it happened.
    Truncate,
}
```

The backend's marshaling seam (replacing the current `Value::to_raw_for_lua`):

```rust
impl Value {
    /// Lower a host Value into the active backend's internal representation.
    /// THE version-aware ingest seam. Single source of truth for i64 ingest.
    fn lower_into(&self, backend: &dyn Backend) -> Result<RawLuaValue> {
        match (self, backend.number_model()) {
            // Dual-subtype engines: i64 is a first-class Lua integer. Exact round-trip.
            (Value::Integer(i), NumberModel::DualSubtype) => Ok(RawLuaValue::Int(*i)),

            // Float-only engines: there is NO integer Lua value. Must widen.
            (Value::Integer(i), NumberModel::FloatOnly) => {
                let f = *i as f64;
                if (f as i64) == *i {
                    Ok(RawLuaValue::Float(f))          // exact: 0..=2^53 round-trips
                } else {
                    match backend.lossy_int_policy() {
                        LossyIntPolicy::ErrorOnInexact => Err(LuaError::LossyIntConversion {
                            value: *i, version: backend.version(),
                        }),
                        LossyIntPolicy::Truncate => Ok(RawLuaValue::Float(f)),
                    }
                }
            }

            (Value::Number(f), _) => Ok(RawLuaValue::Float(*f)),
            // ... non-numeric variants are version-invariant ...
        }
    }
}
```

Key points:
- On a **dual-subtype** engine, `Value::Integer(i64)` round-trips exactly as a
  Lua integer — no loss, no branch.
- On a **float-only** engine, `Value::Integer` *must* become `RawLuaValue::Float`.
  The `i as f64 as i64 == i` check is the canonical exactness test (covers the
  full `±2^53` exact range). Inexact widening is, by default, a typed error — not
  a silent truncation. This honors the global rule "missing/ambiguous data is a
  bug to surface, not paper over with a fallback."

#### 2.2.2 Lua → Host (egress): float-only number crossing back out

Under a float-only backend every Lua number is `f64`, so it surfaces as
`Value::Number(f64)` — **never** `Value::Integer`. Host code asking for an
`i64` via `FromLua` therefore goes through one documented rule:

```rust
impl FromLua for i64 {
    fn from_lua(v: Value, lua: &Lua) -> Result<i64> {
        match v {
            // Dual-subtype path: an actual Lua integer.
            Value::Integer(i) => Ok(i),
            // Float-only path (and float-valued numbers on any backend):
            // accept ONLY if the float is exactly integral and in range.
            // truncate-toward-zero is NOT applied; non-integral is an error.
            Value::Number(f) if f.fract() == 0.0 && f >= i64::MIN as f64 && f < 2f64.powi(63)
                => Ok(f as i64),
            Value::Number(_) => Err(LuaError::FromLuaConversion {
                from: "number", to: "integer",
                why: Some("number has no integer representation".into()),
            }),
            other => Err(/* type error */),
        }
    }
}
```

This is one source of truth for "Lua number → host i64": *exactly integral and
in range, else error*. No silent truncation, no fallback. It matches what a 5.3+
engine does internally for `math.tointeger`/`%d`, and gives float-only backends a
single coherent rule rather than a per-call-site guess.

> The float-only backends also differ in *observable Lua-level* number behavior
> the host never marshals but a script sees: `tostring(3.0)` is `"3"` in 5.1/5.2
> vs `"3.0"` in 5.4 (`5.1-5.2-upstream.md` §1); `string.format("%d", 3.5)` is
> lenient-truncating in 5.1 but an error in 5.4. Those live *inside* the backend
> (its `tostring`/`string.format` impl), not in the `Value` seam — the seam only
> governs the host boundary. They are listed in the divergence registry (§3.4).

### 2.3 Why a superset enum and not a per-version `Value`

A per-version `Value` would force host code and `FromLua`/`IntoLua` to be generic
over the version (the `Lua<V>` problem from §1.3) or to fork. The superset enum
keeps one `Value`, one set of `FromLua`/`IntoLua` impls, and pushes the only real
cost — version-checked number discipline — to the single ingest/egress seam above.
That is the right trade for the unified-API pitch.

---

## 3. What stays version-invariant (confirmed against the codebase audit)

The audit (`lua-rs-current-seams.md`, "Embedding API (the product)" row,
`lua-rs-runtime/src/lib.rs`) confirms these encode **no** Lua version today and
must stay that way:

| Area | Types | Status per audit |
|---|---|---|
| State handle | `Lua` (`:283`), `HostHooks` (`:144`) | invariant; version is added *below*, at construction |
| Value currency | `Value` (`:1553`) | invariant superset (§2) |
| Handles | `Table` (`:1617`), `Function` (`:1667`), `LuaString` (`:1720`), `AnyUserData` (`:1744`), `Thread` | invariant; RAII-rooted, provenance-tagged |
| UserData + derive | `UserData`/`UserDataMethods` (`:2015,2029`), `#[derive(LuaUserData)]`, `#[lua_methods]`, `#[lua_impl(...)]` | **invariant** — this is the #56/#57 work; see §3.1 |
| Scope / non-`'static` | `Lua::scope` (`:454`) | invariant; the borrow-lending model is version-agnostic |
| Conversions | `FromLua`/`IntoLua`/`FromLuaMulti`/`IntoLuaMulti` (`:2496-2512`) | invariant *signatures*; only the numeric impls consult the backend at the seam (§2.2) |
| Error model | `LuaError` | invariant *enum shape*; gains `Unsupported`/`LossyIntConversion` variants (§3.4) and a per-backend message table (§3.3) |

### 3.1 UserData + derive: fully version-invariant

UserData is bound to Rust types via real Lua metatables built once per `TypeId`
and shared (`lua-rs-runtime/src/lib.rs` module docs). Nothing in this mechanism
is version-coupled:

- Metatables, `__index`/`__newindex` composition (field → method → raw escape
  hatch), `getmetatable`/`setmetatable` reflection — all present in 5.1→5.5.
- The derive surface (`#[derive(LuaUserData)]`, `#[lua_methods]`,
  `#[lua_impl(Display, PartialEq, PartialOrd)]` wiring `__tostring`/`__eq`/
  `__lt`/`__le`) targets metamethods that exist in every version.

**One backend-internal caveat the derive does NOT expose to the host:** `__lt`
emulates `__le` in 5.1/5.2/5.3 but was removed in 5.4 (`5.3-upstream-delta.md`
§6; `5.1-5.2-upstream.md` §6). So a userdata that defines only `__lt` answers
`a <= b` in 5.3 but errors in 5.4. The derive *registers the same metamethods*
regardless; the *dispatch fallback* lives in the backend's comparison path. Host
code and the derive macro are unchanged across versions — the divergence is
entirely below the API, which is the whole point.

### 3.2 Scope, handles, non-`'static` borrows

`Lua::scope` lends a non-`'static` borrow for one call and invalidates any escaped
reference on scope exit (clean runtime error, not UB). This machinery is
version-agnostic and *also* supplies the handle-provenance check that enforces
the monomorphic-instance rule (§1.2): a handle remembers its parent `Lua`, so
using it against a different-version instance fails the same way an escaped scope
ref does.

### 3.3 Error model: invariant shape, per-backend message table

`LuaError`'s *Rust enum shape* is invariant. But error-message **text** is a
genuine divergence the oracle asserts on: 5.4 invested in richer variable-name
annotations than 5.3 (`5.3-upstream-delta.md` §8), `'for' limit` wording was
reworded, `5.5` replaces a `nil` error object with a string
(`5.5-upstream-delta.md` §6). Therefore:

- The error *value* a host catches (`LuaError::Runtime { message, traceback }`)
  has a uniform Rust shape.
- The *message string* inside it is produced by the active backend from a
  **per-backend message table**, because the version-matched official suite
  (`errors.lua` for that version) is the oracle for those exact strings. This is a
  per-backend table, never one shared table.

### 3.4 Typed divergence: `Unsupported` + a machine-readable divergence registry

```rust
#[non_exhaustive]
pub enum LuaError {
    // ... existing variants ...
    /// A feature that does not exist on the active backend was requested.
    /// Runtime analog of mlua's #[cfg] removal.
    Unsupported { feature: &'static str, version: LuaVersion },
    /// A host i64 could not be represented exactly in a FloatOnly backend
    /// and the policy is ErrorOnInexact.
    LossyIntConversion { value: i64, version: LuaVersion },
}
```

Every version-divergent point is registered once in a backend-trait table so the
support matrix is derivable (and renderable on docs.rs). The registry, distilled
from the deltas, is:

| Feature | Present on | Absent → `Unsupported` on | Source |
|---|---|---|---|
| integer subtype / `math.type` | 5.3, 5.4, 5.5 | 5.1, 5.2 | `5.1-5.2-upstream.md` §1 |
| `_ENV` / `load(..., env)` sandbox | 5.2, 5.3, 5.4, 5.5 | 5.1 (uses `setfenv`/`getfenv`) | `5.1-5.2-upstream.md` §2 |
| `setfenv`/`getfenv` | 5.1 | 5.2+ | `5.1-5.2-upstream.md` §2 |
| `goto`/labels | 5.2+ | 5.1 | `5.1-5.2-upstream.md` §3 |
| native bitwise `& \| ~ << >>`, `//` | 5.3+ | 5.1, 5.2 | `5.1-5.2-upstream.md` §3 |
| `bit32` library | 5.2 only | 5.1, 5.3, 5.4, 5.5 | `5.3-upstream-delta.md` §5; `5.1-5.2-upstream.md` §5 |
| `utf8` library | 5.3+ | 5.1, 5.2 | `5.3-upstream-delta.md` §5 |
| `string.pack`/`unpack`/`packsize` | 5.3+ | 5.1, 5.2 | `5.1-5.2-upstream.md` §5 |
| `<const>`/`<close>` attributes, `__close`, `coroutine.close`, `warn` | 5.4+ | 5.1, 5.2, 5.3 | `5.3-upstream-delta.md` §3,§6 |
| generational GC, `collectgarbage("generational"/"incremental")` | 5.4 (5.5 reshapes via `"param"`) | 5.1, 5.2, 5.3 | `5.3-upstream-delta.md` §7; `5.5-upstream-delta.md` §5 |
| `global` keyword + declared-globals scope model | 5.5 | 5.1–5.4 | `5.5-upstream-delta.md` §1 |
| named vararg tables `function f(a, ...t)`, `table.create` | 5.5 | 5.1–5.4 | `5.5-upstream-delta.md` §3,§5 |
| `__len`/`__gc` on tables | 5.2+ | 5.1 (userdata-only) | `5.1-5.2-upstream.md` §6 |
| `__lt`-emulates-`__le` fallback | 5.1, 5.2, 5.3 | (removed) 5.4, 5.5 | `5.3-upstream-delta.md` §6 |

Behavioral divergences (same call, different result — not absence) are flagged
the same way but resolved inside the backend rather than via `Unsupported`:
`math.random` stream/algorithm, `for`-loop integer wraparound (5.3 wraps, 5.4+
counts), decimal-literal overflow (5.3 wraps, 5.4 → float), `print` routing
through overridable `tostring` (5.3) vs hardwired (5.4), `io.lines` arity (1 vs
4), `utf8` surrogate acceptance, finalizer-error propagation (5.3 errors, 5.4
warns), `tostring(3.0)` (`5.1-5.2-upstream.md`, `5.3-upstream-delta.md`,
`5.5-upstream-delta.md`).

---

## 4. Internal dispatch shape

### 4.1 `enum Engine` over `Box<dyn Backend>`

**Recommendation: a closed `enum Engine` whose variants are the compiled-in
backends, with a `Backend` trait used for the shared seam *contract* but
dispatched through the enum, not stored as `dyn`.**

```rust
enum Engine {
    #[cfg(feature = "lua51")] V51(v51::Engine),
    #[cfg(feature = "lua52")] V52(v52::Engine),
    #[cfg(feature = "lua53")] V53(v53::Engine),
    #[cfg(feature = "lua54")] V54(v54::Engine),   // the implemented baseline
    #[cfg(feature = "lua55")] V55(v55::Engine),
}
```

Rationale (vs `Box<dyn Backend>`):

1. **The set of backends is closed and known at compile time.** There is no
   open-ended plugin story — we ship a fixed family (5.1–5.5). A closed enum
   models that exactly; `dyn` models an open set we don't have.
2. **Feature-gating falls out for free.** Each variant is `#[cfg]`-gated, so a
   slim single-version build compiles to a single-variant enum the optimizer
   reduces to a no-op dispatch — recovering mlua-class performance with no code
   change. A `Box<dyn>` would still carry vtable indirection even when one backend
   is compiled in.
3. **No allocation, no vtable on the hot path.** The VM dispatch loop is the
   hottest code in the system; a `match self.engine` the optimizer can see
   through beats an opaque vtable call. The per-call `match version` cost flagged
   in `mlua-api-seams.md` §3 is real but negligible, and the enum keeps it
   inline-able.
4. **Variants can hold version-specific concrete state** (a 5.4 `Engine` holds
   the 5.4 opcode tables; a 5.1 `Engine` holds the function-environment globals
   machinery) without boxing or type erasure.

The `Backend` trait still exists — it defines the seam contract
(`number_model()`, `lossy_int_policy()`, `lower_into`/`raise_from`, `open_libs`,
`error_message_table`, `gc_surface`, the divergence registry) — but `Engine`
dispatches to each variant's inherent impl. Trait-as-contract, enum-as-dispatch.

This also matches what the code already wants: the audit shows `lua-vm` carries
its own opcode enum and dispatch, and recommends *consolidating the duplicated
`OpCode` first, then adding a second dispatch body that reuses the
version-invariant helpers* (`lua-rs-current-seams.md`, "VM dispatch loop" row).
An `enum Engine` with per-variant dispatch bodies sharing the value/arith/
metamethod helpers is precisely that shape.

### 4.2 How handles bind to their parent instance

Handles (`Table`, `Function`, `AnyUserData`, `LuaString`, `Thread`) are already
RAII roots into a specific `Lua`'s state (`root_raw_in_state`,
`lua-rs-runtime/src/lib.rs:1578`+). The binding rules:

- A handle holds provenance back to its parent `Lua` instance (the existing
  rooting/`Rc<LuaInner>` linkage). Because an instance is monomorphic in version
  (§1.2), the handle is transitively version-tagged by its parent — **we do not
  add a version field to every handle**; provenance already carries it.
- Using a handle against a *different* `Lua` instance is rejected by the same
  provenance/Scope-invalidation check that already guards escaped scope refs
  (§3.2). Cross-version handle mixing is therefore a clean `LuaError`, never UB
  and never a silently-wrong result.
- The `Engine` lives inside `LuaInner` (alongside the existing `state`,
  `userdata_metatables`, etc.). Every handle operation routes through
  `lua.with_state(...)`, which now also has the `Engine` in scope for any
  version-divergent step.

---

## 5. Concrete Rust API sketches

The governing property: **the common cases are byte-for-byte identical across
versions; only genuinely-divergent cases name the version.**

### 5.1 Construction

```rust
// Identical-shape construction, version is the only difference.
let lua54 = Lua::new();                                   // latest, the back-compat path
let lua53 = Lua::builder().version(LuaVersion::V5_3).build()?;
let lua51 = Lua::builder().version(LuaVersion::V5_1).build()?;

// Playground: four instances, one binary. Route a chunk to the chosen backend.
fn run_in(version: LuaVersion, src: &str) -> Result<String> {
    let lua = Lua::builder().version(version).build()?;
    lua.load(src).eval::<String>()                        // SAME call for every version
}
```

### 5.2 Load / eval / globals — identical across versions

```rust
// load + eval: identical signature on every backend.
let sum: i64 = lua.load("return 1 + 2").eval()?;          // 5.3/5.4/5.5: integer 3
// On a 5.1/5.2 instance the SAME code returns 3.0 internally; FromLua<i64>
// accepts it iff exactly integral (§2.2.2) — so this line still yields 3.

// globals: identical.
lua.globals().set("greeting", "hi")?;
let g: String = lua.globals().get("greeting")?;

// calling a Lua function: identical.
let f: Function = lua.load("return function(x) return x * 2 end").eval()?;
let y: i64 = f.call(21)?;                                  // 42 on every backend
```

### 5.3 Stdlib roster selection (version-defined set, uniform API)

```rust
let lua = Lua::builder()
    .version(LuaVersion::V5_3)
    .std_libs(StdLibSet::ALL)          // resolves to the 5.3 roster: includes bit32+utf8, excludes warn/coroutine.close
    .build()?;

// Requesting a lib the backend lacks is a typed error, not a panic:
let lua51 = Lua::builder().version(LuaVersion::V5_1).build()?;
match lua51.load("return utf8.len('x')").eval::<i64>() {
    Err(LuaError::Unsupported { feature: "utf8", version: LuaVersion::V5_1 }) => { /* expected */ }
    _ => {}
}
```

`StdLibSet::ALL` is *resolved against the active version's roster* — it means
"every library this version ships," not a fixed list. The host writes the same
`std_libs(StdLibSet::ALL)` for every version; the backend supplies the set
(`bit32` only on 5.2/5.3, `utf8` only on 5.3+, `table.create` only on 5.5, etc.,
per the §3.4 registry).

### 5.4 UserData + derive — identical across versions

```rust
#[derive(LuaUserData, PartialEq, PartialOrd)]
#[lua(methods)]
#[lua_impl(Display, PartialEq, PartialOrd)]
struct Vec2 { pub x: f64, pub y: f64 }

#[lua_methods]
impl Vec2 {
    pub fn length(&self) -> f64 { (self.x * self.x + self.y * self.y).sqrt() }
    pub fn scale(&mut self, k: f64) { self.x *= k; self.y *= k; }
}

// SAME registration on a 5.1 and a 5.5 instance:
let v = lua.create_userdata(Vec2 { x: 3.0, y: 4.0 })?;
lua.globals().set("v", v)?;
let len: f64 = lua.load("return v:length()").eval()?;      // 5.0 everywhere
```

The `#[lua_impl(PartialOrd)]`-generated `__lt`/`__le` register identically; the
only difference is the *backend's* `<=` fallback (5.1/5.2/5.3 derive `<=` from
`__lt`; 5.4/5.5 require `__le`). Host code never sees it.

### 5.5 Version-divergent cases — the only places version is named

```rust
// (a) Number model — the host opts a float-only backend into lossy ingest.
let lua51 = Lua::builder()
    .version(LuaVersion::V5_1)
    .lossy_int_policy(LossyIntPolicy::Truncate)            // explicit opt-in
    .build()?;
lua51.globals().set("big", 9_007_199_254_740_993_i64)?;    // > 2^53
// Default (ErrorOnInexact) would return LossyIntConversion here instead.

// (b) GC tuning — surface differs; gate behind a version check.
match lua.version() {
    LuaVersion::V5_4 => { lua.gc().set_mode(GcMode::Generational)?; }
    LuaVersion::V5_5 => { lua.gc().set_param(GcParam::Pause, 200)?; } // 5.5 "param" model
    _ => { lua.gc().set_incremental(/*pause*/200, /*stepmul*/100)?; } // 5.1–5.3
}
// Calling the wrong one is a typed error, never a silent no-op:
//   lua53.gc().set_mode(GcMode::Generational) -> Err(Unsupported { "generational", V5_3 })

// (c) Sandboxing — _ENV (5.2+) vs setfenv (5.1). Both expressible; the host
// asks for "a sandbox" and the backend supplies the mechanism.
let sandbox: Table = lua.create_table()?;
lua.load(untrusted_src).set_environment(sandbox)?.exec()?;  // 5.2+: custom _ENV; 5.1: setfenv under the hood
// set_environment on a 5.1 instance routes to setfenv; on 5.2+ to the _ENV upvalue.
```

For (c), `set_environment` is the *uniform* host verb; the backend chooses `_ENV`
(5.2+) or `setfenv` (5.1) internally — the host writes one line for both. This is
the pattern for every divergence that has a uniform *intent* but a divergent
*mechanism*: expose the intent, hide the mechanism, name the version only when
the *intent itself* doesn't exist (then `Unsupported`).

---

## 6. Build order implied by this spec (not a schedule, a dependency order)

From the audit's "easy vs hard" and the bridge thesis:

1. **Consolidate the duplicated `OpCode`** (`lua-code` vs `lua-vm` stub) to one
   owner and make `lua-vm` depend on `lua-code` — prerequisite for any ISA seam
   (`lua-rs-current-seams.md`, "Opcode set (VM side) — DUPLICATE").
2. **Introduce `enum Engine` + `Backend` trait** with the single 5.4 variant;
   wire `LuaBuilder::version`, `Lua::version()`, the `Value` marshaling seam
   (§2.2), and `Unsupported`/`LossyIntConversion`. This is pure refactor —
   behavior-preserving, gated by the 5.4 oracle staying green.
3. **5.3 backend** as a configured modern-family variant: gate `<const>`/`<close>`
   parsing off, restore `for`-wrap + decimal-literal-wrap + `__le`-via-`__lt`,
   swap `math.random`, add `bit32`, restrict GC/stdlib roster, per-backend 5.3
   message table. Oracle: `lua-5.3.4-tests` vs `lua-5.3.6` (`5.3-upstream-delta.md`).
4. **5.5 backend**: `global` keyword + declared-global scope model, read-only
   `for` var, named vararg tables, `ivABC` operand mode + `OP_GETVARG`/`OP_ERRNNIL`,
   `table.create`, `collectgarbage("param", ...)` (`5.5-upstream-delta.md`).
5. **5.2 backend** (float-only core on the modern `_ENV` path) then **5.1** (5.2
   minus `_ENV`/goto/escapes, plus `setfenv`/`getfenv` + legacy stdlib) — the
   bridge thesis. These are a *separate core* for the number+globals model, not a
   parameter of the modern core (`5.1-5.2-upstream.md` §0, §9;
   `lua-rs-current-seams.md` closing section).

---

## 7. The three most consequential decisions (summary)

1. **Runtime `LuaVersion` enum as the default selector, compile-time features that
   only *subtract* backends, and NO `Lua<V>` type parameter.** One `.wasm` switches
   versions live (the playground differentiator mlua cannot ship); slim builds gate
   backends out and collapse to mlua-class single-version performance. One `Lua`
   instance is monomorphic in version; multiplexing is across instances.

2. **`Value` is a superset marshaling currency, and the i64↔f64 number-model
   crossing is owned by one engine-aware seam with a single source of truth and no
   fallback.** On dual-subtype engines (5.3/5.4/5.5) `Value::Integer(i64)`
   round-trips exactly; on float-only engines (5.1/5.2) it must widen to `f64` and
   inexact widening is a typed `LossyIntConversion` error by default, never a silent
   truncation.

3. **`enum Engine` (closed, `#[cfg]`-gated variants) for internal dispatch, with a
   `Backend` trait as the seam *contract* — not `Box<dyn>`.** Handles bind to their
   parent instance via the existing RAII-root/provenance machinery (no per-handle
   version field), and the same Scope-invalidation check that guards escaped
   references rejects cross-version handle mixing as a clean error.
