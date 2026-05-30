# Implementation Spec — Adding Lua 5.3 and 5.5 to the lua-rs (5.4) codebase

Status: design spec. Audience: implementers + the harness.
Inputs: `specs/research/5.3-upstream-delta.md`, `specs/research/5.5-upstream-delta.md`,
`specs/research/lua-rs-current-seams.md` (the codebase audit). All external facts are
cited in those research files; this spec cites them by section rather than re-deriving.

Codebase root for all `file:line` references:
`/Users/ianmclaughlin/PycharmProjects/rustExperiments/lua-rs-port/.claude/worktrees/git-issues/crates/`.

---

## 0. TL;DR

- **Architecture: ONE parameterized "modern core" (5.3 / 5.4 / 5.5 selected by config), not separate cores.** The
  expensive-to-duplicate layers (`LuaValue`, arithmetic/coercion, table+metamethod machinery, GC engine, the `_ENV`
  model, and the entire `lua-rs-runtime` embedding API) are already version-invariant across the modern family per
  the seam audit (`lua-rs-current-seams.md` §"One parameterized modern core?"). The only genuinely divergent layer is
  the **bytecode ISA** (compiler emit + VM dispatch), wrapped in a thin shell of grammar/stdlib/GC-knob gating. We split
  only at the ISA; everything else is shared and gated by a single `LuaVersion` enum threaded from `Lua::new`.
- **5.1 stays a separate core** (float-only value model, `fenv` globals) — out of scope here, but the seams are flagged
  so it can branch cleanly later.
- **Implement 5.3 FIRST.** Strongest single reason: **5.3 is subtractive and frozen** — it is a strict simplification of
  the language we already run (remove `<const>`/`<close>`, restore old semantics, gate stdlib), with a stable
  20-year-old oracle. 5.5 requires *adding* a genuinely stateful global-declaration scope model and a new instruction
  operand mode (`ivABC`) — net-new parser/codegen machinery that is far better built *after* the version-config seams
  have been shaken out by the cheaper 5.3 backend.

---

## 1. Architecture decision: one parameterized modern core

### 1.1 Decision

**One core, parameterized by a `LuaVersion` enum, splitting only at the ISA.** This is dictated by the codebase, not
preference: the seam audit (`lua-rs-current-seams.md`) found that `LuaValue` (`lua-types/src/value.rs:13`), arithmetic
and coercion (`lua-types/src/arith.rs`, `lua-vm/src/vm.rs` helpers), table/metamethod dispatch (`lua-vm/src/vm.rs:893-985`),
the GC engine (`lua-gc/src/heap.rs`), the `_ENV`-as-upvalue globals model (`lua-parse`), and the full embedding API
(`lua-rs-runtime/src/lib.rs`) carry **zero** version coupling across 5.3/5.4/5.5. Forking a whole second core would
duplicate all of that for no divergence. The only place "share aggressively" hits a wall is the instruction set.

### 1.2 The version selector

Add one enum, owned by `lua-types` (the lowest shared crate so every layer can read it without a dep cycle):

```rust
// lua-types/src/version.rs  (new)
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum LuaVersion { Lua53, Lua54, Lua55 }
impl LuaVersion {
    pub fn version_str(self) -> &'static [u8] { /* b"Lua 5.3" | b"Lua 5.4" | b"Lua 5.5" */ }
    pub fn luac_version_byte(self) -> u8 { /* 0x53 | 0x54 | 0x55 */ }
}
```

Threaded from the embedding API entry points (`lua-rs-runtime/src/lib.rs` `Lua::new`/`try_new`/`with_hooks`,
`:725-735`) — add `Lua::new_with_version(LuaVersion)` keeping `Lua::new()` defaulting to `Lua54` for source/ABI
back-compat. The version flows into: `LexState`/`FuncState` (parser), the ISA selector (compiler + VM), the stdlib
opener table, and the `collectgarbage` option matcher. **No version appears in any embedding-API type** — confirmed
version-invariant (`lua-rs-current-seams.md` row "Embedding API").

### 1.3 Per-crate seam placement

| Crate | Seam shape | Cost |
|---|---|---|
| **lua-types** | New `LuaVersion` enum (single source of truth). `LuaValue`, arith: **no seam** — shared. | Trivial |
| **lua-lex** | Keyword table becomes per-version **data** (`&[&[u8]]`) instead of constants (`lua-lex/src/lib.rs:157-265`). 5.3↔5.4 identical; **5.5 adds `global`** (+ `LUA_COMPAT_GLOBAL` demotion switch). Numeric/string literal lexing shared, except **5.3 decimal-literal overflow wraps** (a 5.3-only branch in `read_numeral`). | Low (5.3) / Med (5.5) |
| **lua-parse** | Gate the 5.4-only `<const>`/`<close>` attribute productions + to-be-closed emission (`lua-parse/src/lib.rs:261,2201,2642`) behind `version >= 5.4`. **5.5 adds**: `global` statement + stateful scope resolver, read-only for-var marking, named vararg tables. Bulk of recursive-descent grammar shared. | Low (5.3) / High (5.5) |
| **lua-code** | **Primary seam.** `OpCode`/`OP_MODES`/`Instruction` bit layout (`lua-code/src/opcodes.rs:87,337,484`) is 5.4-specific. Put each ISA behind a module (`isa::v53` / `isa::v54` / `isa::v55`) or an `Isa` trait owning the opcode enum, mode table, instruction encode/decode, and codegen emit. | High |
| **lua-vm** | **Pre-req: consolidate the duplicate `OpCode` enum first** (`lua-vm/src/vm.rs:45-120` is a Phase-B stub copy; `lua-vm` does not even depend on `lua-code`). Then dispatch: a thin second/third `execute_*` body per ISA reusing the version-neutral helpers (arith/concat/compare/metamethod-chain), **or** a generic loop over the `Isa` trait. Plus `dump.rs`/`undump.rs` `LUAC_VERSION` parameterization (`dump.rs:31`, `undump.rs:41`). | High |
| **lua-stdlib** | Already a clean data seam: `LOADED_LIBS` opener table (`lua-stdlib/src/init.rs:58-95`) becomes per-version; individual function registrations gated inside each `open_*`. A few divergent bodies (`math.random`, `print`/`tostring`, `io.lines`, `utf8`) get `if version` branches at the call site — do not fork whole modules. `_VERSION` string + `collectgarbage` option set (`base.rs:24,392`) per version. | Low–Med |
| **lua-gc** | Engine shared (mark/sweep is version-invariant). Only the **script-visible knobs** differ — gated in `lua-stdlib/base.rs`, not in `lua-gc`. `__gc` non-function handling + finalizer-error propagation differ (5.3 ignores/propagates; 5.4+ calls/warns) — a small flag read in the finalizer path. | Low |
| **lua-coro** | `coroutine.close` is 5.4+ only (tied to to-be-closed); gate its registration. Otherwise shared. | Trivial |

**Consolidation gate (do this before any version work):** collapse the two `OpCode` definitions
(`lua-code/src/opcodes.rs:87` and the stub `lua-vm/src/vm.rs:45`) to one owner in `lua-code`, make `lua-vm` depend on
`lua-code`, and route VM decode through the shared definition. Building a version seam on top of a duplicated enum
doubles every ISA change. This is a prerequisite refactor, oracle-gated against the existing 5.4 suite (no behavior
change expected).

---

## 2. Lua 5.3 — concrete change list

Authoritative delta: `5.3-upstream-delta.md` (master table, items #1–#30). 5.3 is **subtractive**: remove 5.4 additions
and restore older semantics.

### 2.1 Per-crate changes

**lua-lex**
- 5.3-only branch in `read_numeral`: a decimal integer literal that overflows `i64::MAX` **wraps** to an integer
  (5.4 reads it as a float). Hex literals wrap in both. (delta #2)
- Keyword set otherwise identical — no token changes.

**lua-parse**
- Gate `<const>`/`<close>` attribute parsing behind `version >= 5.4`; under 5.3 these are a **syntax error** (`<` parses
  as an unexpected token). (delta #7)
- Omit all to-be-closed machinery (`insidetbc`, `Tbc`/`Close` emission) under 5.3. (delta #8)
- 5.3 is *more permissive* on the goto same-name-label-in-enclosing-block rule — relax that check under 5.3. (delta #6)

**lua-code / lua-vm (ISA `v53`)**
- Restore **wrapping** numeric-`for` over integers: `idx = intop(+, idx, step)` then compare `idx <= limit`, instead of
  5.4's precomputed unsigned iteration count. This is the headline behavioral delta. Test against
  `for i=math.maxinteger-1,math.maxinteger` and large-step cases. (delta #1)
- The 47-opcode RK-encoded ISA is an **internal** choice — our VM is from-scratch and need not byte-match. We may reuse
  the existing 5.4-style internal opcodes for the 5.3 backend **as long as observable behavior matches**; only the
  `string.dump` byte format is observable (structural oracle). **Recommendation: keep the shared internal ISA for 5.3
  behavior, and only emit a 5.3-shaped `string.dump` header byte `0x53`** (delta #24); full 47-opcode byte-faithful
  dump is deferred unless a precompiled-chunk parity test demands it. This collapses most of the "High" ISA cost for 5.3.
- `string.dump` header version byte → `0x53`; `_VERSION` → `"Lua 5.3"`. (delta #24, #25)

**lua-types / vm arith**
- String→number coercion must happen in the **core arithmetic/bitwise path** under 5.3 (`"1"+"2"` → integer; `"3" & 5`
  works in core), vs 5.4 routing through string-library metamethods. Implement as a `version` flag on the arith fast
  path; the error message origin differs accordingly. (delta #3, §8 wording)
- Reinstate `__le`-via-`__lt` fallback under 5.3 (`a <= b` ≡ `not (b < a)` when `__le` absent). (delta #4)

**lua-gc (finalizer path) / lua-stdlib base**
- 5.3 **ignores** a `__gc` that isn't a function (5.4 calls any value). (delta #5)
- 5.3 finalizer errors **propagate** (`LUA_ERRGCMM`); 5.4 emits a warning. (delta #19)

**lua-stdlib (per-version openers + bodies)**
- **Add `bit32` library** (default-on in 5.3, removed in 5.4). Note: `bit32.*` masks to **32 bits**, distinct from the
  64-bit native `&`/`|`/`~`/`<<`/`>>`. Budget its own small impl + tests. (delta #11, risk #5)
- **`math.random`**: swap to a 5.3 algorithm — C-`rand()`-style float in `[0,1)`; `math.randomseed(x)` requires an arg
  (no entropy seed); `math.random(0)` is an **error** ("interval is empty"), not 5.4's full-range integer. Stream parity
  with C `rand()` is platform-defined — ship a documented deterministic 5.3-compatible generator and treat exact-value
  `math.lua` asserts as known-divergence (risk #1). (delta #13, #14, #15)
- **math compat aliases** present by default: `atan2 cosh sinh tanh pow frexp ldexp log10` (compat-on in 5.3). (delta #16)
- **`print`** routes through the overridable global `tostring` under 5.3 (hardwired in 5.4). (delta #20)
- **`io.lines`** returns **1** value under 5.3 (4 in 5.4). (delta #21)
- **`utf8`** decoders accept surrogates by default under 5.3 (rejected in 5.4). (delta #22)
- **`ipairs`** honors `__ipairs` under 5.3 (removed in 5.4). (delta #12)
- **`coroutine.close`**, **`warn`** absent; **`loadstring`** present (alias of `load`). (delta #9, #10, #30)
- **`collectgarbage`**: reject `"incremental"`/`"generational"`; accept `setpause`/`setstepmul` as real tuning. (delta #17)
- **Error-message wording**: maintain a **5.3-specific** message table (shorter, fewer name annotations) keyed off the
  5.3.4 `errors.lua` spec — not shared with 5.4. (delta #29, §8)

### 2.2 Effort & risk (5.3)

- **Effort: Medium.** Mostly subtraction + stdlib gating + a handful of semantic restorations. No new parser grammar,
  no new opcode operand mode, no new scope model. The `bit32` lib and the per-version error table are the only sizable
  net-new pieces.
- **Risk: Low–Medium.** Main risks: (1) `math.random` stream non-parity (mitigated by quarantining exact-value asserts);
  (2) string-coercion-in-core touches the hot arith path (regression risk to 5.4 — guard behind the version flag and
  re-run the 5.4 suite); (3) per-version error wording is fiddly but mechanical.

---

## 3. Lua 5.5 — concrete change list

Authoritative delta: `5.5-upstream-delta.md`. 5.5 is **additive**: a smaller raw delta from 5.4 but the additions land
in the hardest layers (parser/codegen).

### 3.1 Per-crate changes

**lua-lex**
- Add `Global` token (`TK_GLOBAL`, between `function` and `goto`). Honor `LUA_COMPAT_GLOBAL` (default-on upstream): a
  compat switch that **demotes** `global` back to an ordinary identifier. This is a second behavior axis to test.
  (5.5 §1a)

**lua-parse — the hard part**
- New `global` statement + the **stateful scope-resolution model** (§1c): every chunk starts with an implicit
  `global *`; an explicit `global` decl **voids** it for that scope, after which **every free name must be declared**
  (local/upvalue/global) or it is a **compile-time error**; `global *` (optionally `<const>`) re-enables global-by-default.
  Track a per-scope mode flag + the declared-global set + their attributes. `<close>` is local-only (`global X <close>`
  is a compile error). (5.5 §1b, §1c)
- **Read-only for control variable**: mark the numeric/generic loop var `const`; assignment to it is a compile error
  (`check_readonly`). Integer overflow semantics are **unchanged from 5.4** (no wrap) — existing count logic carries over.
  (5.5 §2)
- **Named vararg tables**: `function f(a, ...t)` binds varargs to a named table param (`parlist`, kind `RDKVAVAR`);
  indexing it compiles to `OP_GETVARG`. Plain `...` still works. (5.5 §3)

**lua-code / lua-vm (ISA `v55`) — broad bytecode divergence**
- Opcode count **83 → 85**: add `OP_GETVARG` (`R[A] := R[B][R[C]]`) and `OP_ERRNNIL`
  (`raise error if R[A] ~= nil`, powers the undeclared-global compile-time check). (5.5 §4)
- **`OP_SHRI`/`OP_SHLI` order swapped** — any opcode-indexed table must be regenerated for 5.5, not diffed.
- **New `ivABC` operand mode** (`vC(10)`/`vB(6)`); `OP_NEWTABLE`/`OP_SETLIST` switch to it. The instruction decoder +
  codegen must add this mode. (5.5 §4)
- `LUAC_VERSION` → `0x55`; dump/undump caveats (writer called one extra time). (5.5 §4)
- As with 5.3, the internal ISA need only be **behaviorally** faithful; byte-faithful `string.dump` is a structural-oracle
  concern. However the **new language features (named varargs via GETVARG, the global-check via ERRNNIL) require real
  codegen + VM support** regardless of dump faithfulness — this is net-new VM work, unlike 5.3.

**lua-stdlib**
- **`table.create(nseq [, nrec])`** — NEW (preallocation). (5.5 §5)
- **`utf8.offset`** also returns the final byte position. (5.5 §5)
- **`collectgarbage`**: `"incremental"`/`"generational"` no longer take tuning params; tuning moves to a new
  `collectgarbage("param", ...)`; GC params changed. (5.5 §5)
- **Float `tostring`/`%g`** prints enough digits to **round-trip exactly** (vs 5.4 `%.14g`). Observable in `math.lua`.
- math compat aliases still gated by `LUA_COMPAT_MATHLIB` (off by default — same as 5.4). No library removals.
- `_VERSION` → `"Lua 5.5"`.

**lua-vm / pcall-error path**
- A `nil` error object is **replaced by a string message** (`error(nil)` through `pcall` yields a string). (5.5 §6)
- `__call` metamethod chain capped at **15 objects**. (5.5 §6)

### 3.2 Effort & risk (5.5)

- **Effort: High.** The raw delta table is shorter than 5.3's, but the work concentrates in parser/codegen/VM: a
  stateful scope resolver with compile-time errors, a new instruction operand mode, two new opcodes, named-vararg codegen.
  These are exactly the layers the embedding-API vision wants to *share*, so 5.5 forces a real version-split at
  lex/parse/codegen (`5.5-upstream-delta.md` risk #3), not just a stdlib shim.
- **Risk: Medium–High.** (1) The global-decl scope model is genuinely stateful with subtle nesting/voiding rules + a
  `LUA_COMPAT_GLOBAL` second axis (risk #2). (2) `ivABC` + reordered opcodes mean a "diff against 5.4" assumption
  silently corrupts decode (risk #1). (3) Oracle logistics: test suite ships separately/lightly-versioned; GitHub mirror
  uses root-level layout vs tarball `src/`; default-on compat flag means the stock binary won't reject `global` as an
  identifier (risk #5).

---

## 4. Oracle plan (per version)

Integrates with the existing harness: `harness/source.toml`, `harness/run_official_test.sh`,
`harness/run_official_all.sh`, `harness/parity_check.sh`, reference binary built into `reference/`. The current 5.4
oracle pins `lua-5.4.7` and runs `reference/lua-c/testes/*.lua`.

### 4.1 Make `source.toml` multi-version

Today `source.toml` has a single `[source]`/`[build]`/`[tests]`. Extend to per-version tables (e.g.
`[source.lua53]`, `[source.lua54]`, `[source.lua55]`), each pinning its own tarball/commit, build command, reference
binary path, and test-suite directory. The runner scripts take a `--version` (or `LUA_VERSION` env) selecting the
reference binary and test dir, and the lua-rs CLI gets a `--lua-version` flag so `run_official_test.sh` runs the
matching backend. Parity-diff (`parity_check.sh`) stays the same shape: same input → C binary vs lua-rs binary, diff
stdout + exit code — now parameterized by version on **both** sides.

### 4.2 Lua 5.3 oracle

- **Pin reference binary: `lua-5.3.6`** (final 5.3.x, latest-patched). Source
  `https://www.lua.org/ftp/lua-5.3.6.tar.gz`; build `make macosx` → `src/lua` + `src/luac`. Keep default `luaconf.h`
  compat flags **on** (gives `math.atan2`, `__ipairs`, `loadstring`). (`5.3-upstream-delta.md` "ORACLE for 5.3")
- **Test suite: `lua-5.3.4-tests`** (`https://www.lua.org/tests/lua-5.3.4-tests.tar.gz`) — **there is no 5.3.6 tarball**
  (404). Run the 5.3.4 suite against the 5.3.6 binary and our 5.3 backend; behavioral diffs 5.3.4→5.3.6 are bug fixes,
  not language changes. **Record this deliberate version skew in `source.toml`** (risk #2).
- 28 `.lua` files. Run under `LC_ALL=C`. Quarantine/skip: `code.lua` (internal opcode disasm — needs `ltests`/`testC`,
  irrelevant to our from-scratch ISA), C-API-heavy `api.lua`/`db.lua` (run pure-Lua subsets only), exact-value random
  asserts in `math.lua` (RNG ≠ C `rand()` — keep statistical/range asserts), and gate `big.lua`/`verybig.lua` behind a
  slow tier. Unique temp names for `files.lua`/`main.lua`.

### 4.3 Lua 5.5 oracle

- **Pin reference binary: `lua-5.5.0`** (released 22 Dec 2025; GitHub tag `v5.5.0`, commit
  `a5522f06d2679b8f18534fd6a9968f7eb539dc31`). Source `https://www.lua.org/ftp/lua-5.5.0.tar.gz`. Note the GitHub
  mirror lays sources at the **repo root**, the tarball under `src/` — pin the tarball layout to match the existing
  5.4 flow. **Build two binaries**: compat-on (default `LUA_COMPAT_GLOBAL`) and **all-compat-off** (to oracle the
  `global`-as-keyword behavior). Decide per-test which build is the oracle. (`5.5-upstream-delta.md` "ORACLE for 5.5")
- **Test suite: the `testes/` tree at GitHub tag `v5.5.0`** (ships separately from the release tarball, lightly
  versioned — pin tests to the exact tag you build). 34 `.lua` files. Most-relevant-to-delta files: `locals.lua`
  (global decls, `<const>`), `attrib.lua`, `vararg.lua` (named vararg tables), `code.lua`, `nextvar.lua`/`gc.lua`/
  `gengc.lua`, `errors.lua` (nil error object), `math.lua` (float round-trip), `utf8.lua`.
- Freeze caveat: a future 5.5.x maintenance release may patch behavior; pin the test bundle and reference build together.

### 4.4 Cross-version harness invariant

A change to the shared core must re-run **all three** version oracles before landing — the version flag means a 5.3 or
5.5 edit can regress 5.4. Add a `run_official_all.sh --version {53,54,55}` matrix gate to the between-commit ladder.

---

## 5. Recommendation: implement 5.3 FIRST

**Implement 5.3 first, then 5.5.**

Evidence, weighed:

1. **Subtractive-and-frozen beats additive-and-stateful (the decisive factor).** 5.3 is a strict simplification of the
   language we already run: remove `<const>`/`<close>`, restore old `for`/coercion/`__le` semantics, gate the stdlib,
   add `bit32`. **No new parser grammar, no new opcode operand mode, no new scope model.** 5.5's headline change — the
   `global`-declaration scope model — is genuinely *stateful* compile-time machinery (implicit `global *`, voided by an
   explicit decl, re-enabled by `global *`, with undeclared-name compile errors), plus the new `ivABC` operand mode and
   two new opcodes (`5.5-upstream-delta.md` risks #1–#3). That is net-new work in the hardest layers.

2. **5.3 best shakes out the version-config machinery at lower risk.** The whole point of going first is to validate the
   `LuaVersion` seam, the multi-version `source.toml`, the per-version stdlib opener table, the per-version error table,
   and the three-way oracle matrix — *before* betting them on 5.5's stateful parser. 5.3 exercises every seam (lexer
   literal branch, parser gating, stdlib gating, GC-knob gating, dump header, `_VERSION`) without forcing the riskiest
   parser/codegen rewrite. Build the chassis on the easy version, then run the hard version through it.

3. **Freeze status.** 5.3 has been frozen for ~5 years and the oracle is stable (modulo the documented 5.3.4-tests
   skew). 5.5.0 is release-grade but only ~5 months old (released 22 Dec 2025) with a separately-distributed,
   lightly-versioned test suite and a future-maintenance-release tail — slightly more oracle logistics risk.

4. **Counter-argument (smallest forward delta), and why it loses.** 5.5 has the *smaller raw delta table* from 5.4 and
   is the "forward" direction, which argues for doing it first. But raw-delta-size is the wrong metric: 5.5's small delta
   lands entirely in parser/codegen/VM (the expensive layers), while 5.3's larger delta is mostly cheap stdlib/semantic
   gating. **Near-term value/install base also favors 5.3** — 5.3 has a far larger deployed embedding base today than a
   5-month-old 5.5. So both effort-risk and install-base point the same way.

**Single strongest reason:** 5.3 is **subtractive and frozen** — it lets us build and prove the entire version-config
machinery against a stable, lower-risk target before paying for 5.5's net-new stateful global-declaration parser and
new instruction operand mode.

Sequencing: (0) consolidate the duplicate `OpCode` enum and land `LuaVersion`; (1) ship 5.3 end-to-end through the new
multi-version oracle; (2) ship 5.5, reusing the now-proven seams, focusing effort on the global-decl scope model,
`ivABC`, and the two new opcodes.
