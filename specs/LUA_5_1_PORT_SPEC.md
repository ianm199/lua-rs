# Implementation spec — Lua 5.1 (non-JIT) backend, with 5.2 as the bridge

Status: planning / deferred ("dessert"). Audience: whoever implements the 5.1
backend behind the unified embedding API.

Inputs this spec is built on (read them first):
- `specs/research/5.1-5.2-upstream.md` — the upstream delta map (number model,
  globals model, opcodes, stdlib, metamethods, GC, oracle pinning).
- `specs/research/lua-rs-current-seams.md` — where 5.4 assumptions are baked
  into the current code and where version seams naturally go.

Pins (the oracle): **Lua 5.1.5** and **Lua 5.2.4** — final point releases.
Tarballs: https://www.lua.org/ftp/lua-5.1.5.tar.gz ,
https://www.lua.org/ftp/lua-5.2.4.tar.gz .

One-line strategy: **5.1 is a second family, not a feature-flag.** Two pervasive,
observable axes force it — float-only numbers and function-environment globals.
5.2 keeps the float-only number model but already uses the modern `_ENV` globals
model, so 5.2 is the bridge: do 5.2 first to prove the float-only core in
isolation from the env model, then reach 5.1 by *subtraction plus legacy add-back*.

---

## 1. Why 5.1 is a SECOND family (the structural split)

The modern core (`lua-types`, `lua-vm`, `lua-parse`, GC, runtime API) is built on
two assumptions that the seam audit confirms are load-bearing and shared cleanly
across 5.3/5.4/5.5 but **broken by 5.1**:

1. **Dual int/float number model.** `LuaValue::Int(i64) | Float(f64)`
   (`lua-types/src/value.rs:13-24`, `:18`), with `arith.rs` integer overflow
   wrap, `//`, bitwise ops, and `math.type`. 5.1/5.2 have exactly **one** number
   type: `f64` (`LUA_NUMBER = double`, 5.1 manual §2.2,
   https://www.lua.org/manual/5.1/manual.html ; luaconf
   https://www.lua.org/source/5.1/luaconf.h.html ). There is no `Integer` value
   to produce, ever, and `math.type` must be **absent**.
2. **`_ENV`-as-upvalue globals.** The parser threads an `_ENV` upvalue through
   every chunk (`lua-parse/src/lib.rs:408 envn`, `:2254-2260`, `:4146-4222`); the
   VM resolves free names via `GETTABUP`/`SETTABUP` on it. 5.1 has **no `_ENV`**:
   globals live in a per-function *environment table* manipulated by
   `getfenv`/`setfenv`, and free names compile to the dedicated
   `OP_GETGLOBAL`/`OP_SETGLOBAL` ops (5.1 manual §2.9). 5.2 manual §8.1/§8.2
   removed both the ops and `set/getfenv`.

Each of these touches the value enum, arithmetic, coercion, table-keying,
string formatting, the lexer's number scanner, the parser's name resolution, the
VM dispatch, and large parts of stdlib. They are not localized. Therefore:

| Layer | Verdict for 5.1 |
|---|---|
| `lua-types` `LuaValue`/arith | **Cannot share with modern core.** Needs a float-only value model. This is the hard boundary — see §1a. |
| `lua-parse` name resolution / `_ENV` threading | **5.1-only branch.** Do not retrofit fenv into the shared `_ENV` parser (seams §`_ENV`, line 31). |
| `lua-code`/`lua-vm` opcode ISA + dispatch | **Separate 5.1 ISA + dispatch** (or lower-at-load into the modern table-access path; see §2.2). |
| GC engine (`lua-gc/src/heap.rs`) | **Mostly shareable.** Mark/sweep is version-invariant; only script-visible knobs and table-finalizer/ephemeron *eligibility* differ (seams §GC; §6 here). |
| Embedding API (`lua-rs-runtime`) | **Shared, version-invariant** — but its `Value` mirrors the int/float split (`lib.rs:1553`), so the i64↔f64 marshaling contract is the API-level cost (§1a). |
| Stdlib (`lua-stdlib`) | **Shared registry, gated bodies** — `LOADED_LIBS` is already data-driven (`init.rs:58-95`); 5.1 is a different *set* + a handful of changed bodies (§3). |

### 1a. Where the number-model boundary forces the split

The seam audit is explicit (lines 24, 69): the modern `LuaValue` is shared across
the modern family, and **5.1 must be a separate core** because the float-only
value model is a deep, shared-core assumption. Concretely the boundary lands at:

- **`LuaValue` itself.** A 5.1 backend either (a) reuses `LuaValue` but contracts
  that the `Int` variant is **never constructed** (every arithmetic/lex/coercion
  path forces `Float`), or (b) introduces a `LuaValue51` with a single `Number(f64)`.
  Recommendation: **option (a) — reuse the enum, ban `Int` by construction** — to
  keep the GC, table, and runtime layers genuinely shared. The ban is enforced at
  the producers (lexer, arith results, `tonumber`, loop counters, `math.floor`),
  not by a new type. This is the single most invasive rule of the whole port: it
  is a property that must hold across dozens of callsites, so it wants a
  **forbidden-pattern hook** ("no `LuaValue::Int` under the 5.1 backend") and a
  numeric-formatting kit (§4) as its mechanical guard.
- **String formatting.** `tostring(3.0)` is `"3"` in 5.1/5.2 (`%.14g`, no forced
  `.0`) vs `"3.0"` in 5.4. The current formatter is `%.14g`
  (`lua-vm/src/object.rs:511`) which is *correct for 5.1/5.2* — the 5.4 `.0`
  suffix is the added behavior to suppress under the legacy backend.
- **Lenient `%d`.** `string.format("%d", 3.5)` truncates in 5.1 but **errors** in
  5.4. Gate this per version.
- **Host marshaling (API contract).** An incoming host `i64`/`u64` must widen to
  `f64`, losing precision above 2^53; outgoing numbers are always `f64`. Pick one
  documented round/range rule for `f64 → i64` (truncate-toward-zero, error on
  non-integral/out-of-range) — a single source of truth, no fallback. This is a
  real semantic divergence the API surfaces (a `lossy` flag / documented
  contract), not papers over.

---

## 2. Concrete subsystems

### 2.1 Float-only number behavior
- Lexer number scanner: accept decimal floats, decimal ints (stored as `f64`),
  and **hex integers** (`0x1F`); **reject** the `p`/`P` binary exponent (that is
  5.2+), and reject `0b`/`//`/bitwise tokens. All numeric tokens lower to one
  `f64`.
- Arithmetic: `/`, `^`, `%` are always float (`%` via `fmod`/`luai_nummod`);
  `7 % 3 == 1` but as a float. No integer overflow, no `//`, no bitwise.
- `math.floor`/`ceil` return **floats**, not integers.
- Table keys: `t[3]` and `t[3.0]` hit the same slot — one number type already
  collapses; ensure float keys with integral value normalize to the array/int
  slot (the modern core already does this for the int subtype, so the behavior
  matches once everything is `f64`).
- `for i=1,3` loop counter is a float.
- `tonumber("3")` → float `3`; large literals lose precision past 2^53 (accept;
  no int path).

### 2.2 fenv / setfenv / getfenv + OP_GETGLOBAL bytecode
The 5.1 globals model is the second hard split. Two implementation options
(seam audit §4 / line 31):

- **Option A (separate 5.1 ISA):** compiler emits `OP_GETGLOBAL A Bx` /
  `OP_SETGLOBAL A Bx`; the VM has a 5.1 dispatch reading the *current closure's
  environment pointer*. Highest fidelity, most code.
- **Option B (lower at load into table access):** lower `OP_GETGLOBAL`/`SETGLOBAL`
  into the existing `_ENV`-style table-access path, where the "table" is the
  per-closure environment rather than an `_ENV` upvalue. Maximizes sharing (the
  bridge thesis) **but must still expose 5.1-correct observable `getfenv`/`setfenv`**
  — i.e. a closure can have a *different* environment than the global table, and
  `setfenv(f, t)` / `setfenv(0, t)` (current-thread) must work.

Recommendation: **Option B**, with a per-closure `env` field that the
lowered table-access reads, plus `getfenv`/`setfenv` mutating it. This reuses the
modern table/metamethod machinery and avoids a whole second VM body. Required
observable surface:
- `getfenv([f|level])` returns the env of function/level (running function if 0/absent).
- `setfenv(f, t)` sets env of a Lua function or stack level; `setfenv(0, t)` sets
  the running thread's env.
- C-function environments (`LUA_ENVIRONINDEX`) — only needed if a host registers
  C functions that read their env; can be a documented gap for v1 if no API
  consumer needs it. Note it, do not silently fake it.

`OP_LOADNIL` is the **range form** in 5.1 (`R(A)..R(B):=nil`); 5.2 changed it to
an `A B count` form. If Option B reuses the modern decoder, the 5.1 loader/compiler
must translate the range semantics correctly.

### 2.3 The 5.1 stdlib set
Restore-from-5.2-removed names plus arity/behavior changes (research §5):
- `loadstring(s)` (present); `load` takes a **reader function only** (string
  loading is `loadstring`'s job in 5.1).
- `unpack` as a **global** (5.2 moved it to `table.unpack`); `table.unpack` absent.
- `table.getn`/`table.setn`/`table.maxn`/`table.foreach`/`table.foreachi`
  (compat/deprecated, present).
- `module(name,...)`, `package.seeall`, `package.loaders` (5.2 renamed to
  `package.searchers`).
- `string.gfind` (compat alias of `gmatch`).
- `math.log(x)` takes **one argument** (no base); `math.log10`, `math.atan2`,
  `math.pow` present; `math.fmod` present, no `math.type`.
- `math.random`/`randomseed` use the **C `rand()`-based** algorithm — output bytes
  differ from 5.4's xoshiro256**; the oracle compares against `lua-5.1.5`, so do
  not reuse the 5.4 PRNG for the 5.1 backend.
- `gcinfo()` (deprecated holdover); `newproxy([bool|proxy])` (undocumented; the
  5.1 idiom for a userdata with a metatable so `__gc`/`__len` work — needed
  because tables can't carry them; see §2.4).
- `arg` table (main-chunk varargs + script args: `arg[0]`=script, `arg[1..n]`,
  `arg.n`).
- `xpcall(f, h)` — **cannot pass extra args** to `f` (5.2+ can).
- `coroutine.*` create/resume/yield/wrap/status/running (`running` returns nil in
  the main coroutine in 5.1).
- **Absent in 5.1:** `bit32` (5.2-only), native bitwise, `string.pack`/`utf8`,
  `<const>`/`<close>`, `goto`, `\x`/`\z`/hex-float, `math.type`.

`LOADED_LIBS` (`lua-stdlib/src/init.rs:58-95`) becomes a per-version table; gate
individual registrations and the few changed bodies (`math.log` arity, lenient
`%d`, `xpcall` arity, `os.execute` return shape) per backend — do **not** fork
whole modules.

### 2.4 Metamethod differences
- **`__len` on tables: does nothing in 5.1.** `#t` uses the primitive length and
  never consults `metatable(t).__len` (5.1 §2.8 vs 5.2 §2.4). Only `newproxy`
  userdata can intercept `#`. The `#` dispatch must branch on version. **This is
  the single most likely source of silent 5.1 test failures.**
- **No `__pairs`/`__ipairs`** (added 5.2; `__pairs` removed again in 5.4).
- **No `__gc` on tables** in 5.1 (userdata only) — table finalizers are 5.2+.
- `__lt`-derives-`__le` fallback is **kept** in 5.1/5.2 (5.4 dropped it).
- `__eq` semantics, `__index`/`__newindex`/`__call`/`__concat`/`__metatable`/
  `__mode` unchanged.
- Arithmetic metamethods always float-dispatched.

---

## 3. 5.2 as the stepping stone — and do it first

5.2 = **modern `_ENV` + goto, but float-only numbers.** It shares the globals
model with the modern core (the existing `_ENV`/closure/`GETTABUP` machinery is
reusable as-is) and shares the number model with 5.1. That makes it the ideal
place to land and validate the **float-only core in isolation** from the env
mess.

**Recommendation: yes, do 5.2 first.** Sequence:

1. **5.2 backend** = modern core minus the int subtype (force float-only, §1a/§2.1)
   plus the 5.2 stdlib surface (`table.unpack`, `bit32`, `__pairs`/`__ipairs`,
   `__len`/`__gc` on tables, hex floats, `goto`, `\x`/`\z` escapes,
   `package.searchers`). `_ENV`/goto/closures are **already in the modern
   parser/VM** — reuse them. This proves the float-only value model against a real
   oracle (5.2.4 ships a modern-shaped test bundle, §4) with minimal new surface.
2. **5.1 backend** = 5.2 *minus* `_ENV`/goto/`\x`/`\z`/hex-float/`bit32`, *plus*
   restore fenv globals + `OP_GETGLOBAL`/`SETGLOBAL` (or lowered, §2.2), restore
   the legacy stdlib names (§2.3), and flip `__len`/`__gc`/`__pairs` to
   userdata-only/absent (§2.4).

The shared seams between 5.2 and 5.1 are large (number core, most of stdlib,
lexer minus a few tokens). The genuine 5.1-only splits are narrow: globals
dispatch, `__len`/`__gc` table eligibility, the legacy opcode/`LOADNIL` shape,
and `newproxy`.

5.2's own divergences to honor (not just "modern minus int"): `goto`/`::label::`
grammar, `\x`/`\z` escapes and hex-float literals in the lexer, `bit32`
(operands mod 2^32, range stated `(-2^51,+2^51)`, funcs
`band/bor/bxor/bnot/btest/lshift/rshift/arshift/lrotate/rrotate/extract/replace`),
table `__gc` (must set before/at metatable assignment or never finalized),
ephemeron weak-key tables, and the "step/full collect doesn't restart a stopped
collector" GC rule (research §7).

---

## 4. Oracle plan

- **Pins:** `lua-5.1.5` and `lua-5.2.4`. Record tarball sha + build command in a
  per-version `source.toml`, mirroring the 5.4 reference pin. Build POSIX:
  `make linux`/`make macosx` → `src/lua` (behavioral oracle) + `src/luac`
  (structural oracle, **not** a product requirement — we are past C-mirroring;
  only behavioral parity matters, research §4/§8).
- **5.2 test suite:** 5.2.4 ships the modern-style bundle (`lua-5.2.4-tests`,
  https://www.lua.org/tests/ ) — `all.lua` driver + per-feature files, close to
  5.4's `testes/`. The existing harness wrapper adapts with minor edits → a real
  rung-6 full-suite parity run is achievable.
- **5.1 test suite is a DIFFERENT SHAPE.** 5.1 has **no official modern test
  bundle** of the `all.lua` form; the 5.1-era tests are older, smaller, and
  structured differently. Plan: a **curated 5.1 behavioral corpus** — per-feature
  `.lua` files run through the diff-stdout-and-exit-code behavioral oracle against
  `lua-5.1.5`. Do **not** down-port 5.4 `testes/` (they use int subtype, `goto`,
  `\u`, bitwise, `<const>` — they won't even load under 5.1/5.2). Build the
  corpora from the version-matched suites.
- **Iteration ladder (project convention):**
  - Rung 3 (`cargo test -p <crate> --lib`) for the float-only numeric core, the
    5.1 lexer, the fenv/`_ENV` globals path.
  - **Build two custom in-memory subsystem kits BEFORE paying the oracle loop**
    (the project's core fast-iteration strategy):
    (a) a **numeric-formatting kit** asserting `%.14g` parity for edge values
    (`3.0`, `1e15`, `0.1`, `-0.0`, `1/0`, `0/0`) against the 5.1/5.2 reference —
    also the mechanical guard for the "never construct `Int`" rule;
    (b) a **globals kit** exercising `getfenv`/`setfenv` sandbox idioms vs `_ENV`
    load-env idioms.
  - Rung 4: single version-matched file through the behavioral oracle.
  - Rung 6: full 5.2.4 suite; curated corpus for 5.1.5.
- **Backend selection seam:** add a version selector at engine construction
  (`Lua::new`/`try_new`/`with_hooks`, `lua-rs-runtime/src/lib.rs:725-735`) that
  picks the backend ISA/parser/stdlib set. The API types stay version-invariant
  (seam audit §Embedding API / line 37).

---

## 5. Effort / risk sizing (honest)

Relative to a **modern-family** version (5.3, which the seam audit sizes as:
opcode ISA + VM dispatch is the only hard axis, everything else easy/medium):

| Axis | 5.3 (modern sibling) | 5.2 | 5.1 |
|---|---|---|---|
| Value model | shared (no work) | **force float-only (pervasive)** | force float-only (shared with 5.2) |
| Globals | shared `_ENV` | shared `_ENV` (no work) | **fenv + legacy ops (new subsystem)** |
| Opcode ISA | second ISA (hard) | reuse modern + float | legacy `GETGLOBAL`/`LOADNIL` shape (or lower) |
| Stdlib | few branches | medium (bit32, unpack move) | **large legacy add-back** |
| Metamethods | shared | table `__len`/`__gc`/`__pairs` add | **`__len`/`__gc`/`__pairs` flip back** |
| Test oracle | reuse 5.4 suite shape | modern-shape suite ✔ | **no modern suite → curated corpus** |

**Sizing verdict:** 5.2 is roughly a *modern-sibling-sized* effort (one hard axis:
the pervasive float-only retrofit) and is the cheaper, lower-risk of the two —
do it first. 5.1 is **larger than any single modern-family version**: it pays the
float-only cost (shared with 5.2) **plus** an entirely new globals subsystem,
**plus** the largest legacy-stdlib surface, **plus** the metamethod flip, **plus**
the weakest oracle situation (hand-curated corpus, no drop-in suite).

**Biggest landmines (ranked):**
1. **The "never construct `Int`" invariant** — a cross-cutting property over
   dozens of producers; a single leaked `Int` corrupts `type()`/formatting/keying
   silently. Guard it mechanically (hook + numeric kit) from day one.
2. **`__len` on tables silently does nothing in 5.1** — defines-but-ignored is the
   classic silent test failure; the `#` dispatch must branch on backend.
3. **fenv observable semantics** — `setfenv(0, t)` / per-closure-env-≠-global-table
   must be *observable*, not just "globals work". Easy to fake-pass at first.
4. **Weak oracle for 5.1** — no `all.lua`; coverage depends on how good the curated
   corpus is, so it's easy to declare done with thin coverage. The corpus is the
   product artifact; budget for it.
5. **5.2/5.1 GC quirks** — ephemeron-vs-non-ephemeron weak keys, table-finalizer
   eligibility, "step doesn't restart a stopped collector" — oracle-visible in GC
   stress tests, easy to miss because the engine "works."
6. **PRNG divergence** — `math.random` bytes differ; must use the 5.1/5.2
   algorithm, not the 5.4 one, or every seeded-random test diffs.

---

## Appendix — citations
- Lua 5.1 manual: https://www.lua.org/manual/5.1/manual.html (§2.2 numbers,
  §2.8 `#`/`__len`, §2.9 environments, §7 compat names)
- Lua 5.2 manual: https://www.lua.org/manual/5.2/manual.html (§2.2 `_ENV`,
  §6.7 `bit32`, §8 incompatibilities)
- Lua 5.4 manual: https://www.lua.org/manual/5.4/manual.html
- 5.1.5 source: https://www.lua.org/source/5.1/ (`luaconf.h`, `lopcodes.h`,
  `lundump.c`)
- 5.2.4 source: https://www.lua.org/source/5.2/
- Tarballs: https://www.lua.org/ftp/lua-5.1.5.tar.gz ,
  https://www.lua.org/ftp/lua-5.2.4.tar.gz
- 5.2.4 test bundle: https://www.lua.org/tests/
- mlua (embedding-API prior art): https://github.com/mlua-rs/mlua
