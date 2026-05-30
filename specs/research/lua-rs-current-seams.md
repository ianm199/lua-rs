# lua-rs (5.4) version-coupling seam inventory

Audit date: 2026-05-30. Codebase: `crates/` at worktree `git-issues`.
Goal: locate where Lua 5.4 assumptions are baked in, and where
version-parameterization seams would naturally go to host a second modern
family member (5.3 or 5.5) **in the same build**.

All `file:line` references are to files under
`/Users/ianmclaughlin/PycharmProjects/rustExperiments/lua-rs-port/.claude/worktrees/git-issues/crates/`.

External references cited inline:
- Lua 5.4 manual: https://www.lua.org/manual/5.4/
- Lua 5.3 manual: https://www.lua.org/manual/5.3/
- Lua 5.4 vs 5.3 incompatibilities: https://www.lua.org/manual/5.4/manual.html#8
- Upstream source tags: https://github.com/lua/lua/tags
- mlua (embedding-API prior art): https://github.com/mlua-rs/mlua

---

## Seam inventory

| Axis | file:line | How version-coupled | Proposed seam |
|---|---|---|---|
| Value enum (int/float split) | `lua-types/src/value.rs:13-24` | **Low/none.** `LuaValue` has separate `Int(i64)`/`Float(f64)` variants. The dual int/float number model is identical in 5.3 and 5.4 (integers added in 5.3 per https://www.lua.org/manual/5.3/manual.html#8). 5.1/5.2 had float-only — those would need a separate value model. | Keep `LuaValue` shared across the **modern family (5.3/5.4/5.5)**. No seam needed for 5.3↔5.4. Defer the float-only model to the separate-core 5.1 work. |
| Integer subtype width | `lua-types/src/value.rs:18` (`Int(i64)`), `lua-types/src/arith.rs` | Fixed `i64`. Both 5.3 and 5.4 default to 64-bit integers. | None. Shared. |
| Arithmetic semantics | `lua-types/src/arith.rs:5-19` (`ArithOp`), `lua-vm/src/vm.rs` arith helpers (`idiv`, `mod`, `shiftl`, `flt_to_integer`) | **Low.** Integer overflow wraps, `//` floor-div, bitwise ops, `math.type` — all identical 5.3↔5.4. | None for modern family. |
| Opcode set (compiler side) | `lua-code/src/opcodes.rs:87-198` (`OpCode`, 83 ops), `:337` (`OP_MODES`) | **High and 5.4-specific.** This is the 5.4 bytecode ISA (`LoadI/LoadF`, `*K`/`*I` immediate-arith ops, `MmBin*`, `TForPrep`, `VarArgPrep`, `Tbc`). 5.3 has a *different, smaller* opcode set with RK-encoded operands and no immediate-arith ops (upstream `lopcodes.h` differs per tag, https://github.com/lua/lua/tags). | **Primary seam.** Make `OpCode`/`OP_MODES`/`Instruction` field sizes a per-version module behind a trait or a `mod v53 / mod v54`. The `Instruction` newtype (`opcodes.rs:484`) bit layout (7-bit op, `SIZE_A=8`, `POS_K`) is also 5.4-specific — 5.3 uses 6-bit op + RK bit. |
| Opcode set (VM side) — **DUPLICATE** | `lua-vm/src/vm.rs:45-120` (a second `OpCode` enum) + `:184` numeric dispatch match | **High.** `lua-vm` does **not** depend on `lua-code` (`lua-vm/Cargo.toml` has no `lua-code`); it carries its **own copy** of the 5.4 opcode enum (a leftover Phase-B stub, see comment at `vm.rs:31`) and decodes via a hand-written `match (raw & 0x7F)`. Two opcode definitions must stay in lockstep today. | **Consolidate first**, then parameterize. Collapse the two `OpCode` enums to one owner (`lua-code`), have `lua-vm` depend on it, *then* make it version-generic. Doing the version seam on top of a duplicated enum doubles the work. |
| VM dispatch loop | `lua-vm/src/vm.rs:execute` (~line 813 `loop`), big `match` on decoded op | **High.** The interpreter body hard-codes 5.4 opcode handlers and 5.4 stack/`L->top` conventions (`is_in_top`/`is_out_top`, `opcodes.rs:670-684`). | Either (a) a second dispatch fn `execute_v53` sharing the value/table/metamethod helpers (which are version-invariant), or (b) a generic loop parameterized over an `Isa` trait. Given the helpers (arith, concat, comparisons, metamethod chaining at `vm.rs:893-985`) are already version-neutral, a **second thin dispatch body reusing shared helpers** is the lower-risk path. |
| Bytecode dump/undump header | `lua-vm/src/dump.rs:31-33` (`LUAC_VERSION` from `504`), `lua-vm/src/undump.rs:41,812` (`LUAC_VERSION=0x54`, version check) | **High** but localized. `LUAC_VERSION` byte, format byte, and proto layout are version-specific (5.3 proto serializes differently). | Parameterize `LUAC_VERSION` + proto reader/writer per version. Low effort once opcode/proto seams exist. Only matters if you load/save precompiled chunks; source loading is unaffected. |
| `_ENV` / globals | `lua-parse/src/lib.rs:408` (`envn`), `:2254-2260`, `:4146-4222` (main chunk gets `_ENV` upvalue); `lua-vm/src/debug.rs:50-51,1016-1028` | **Low.** `_ENV`-as-upvalue is the 5.2/5.3/5.4/5.5 model — fully shared across the modern family (https://www.lua.org/manual/5.4/manual.html#2.2). | **No seam for modern family.** For deferred **5.1**: 5.1 has no `_ENV`; it uses per-function environments (`getfenv`/`setfenv`) and a globals upvalue is absent. That is a parser+VM divergence — note it as a 5.1-only branch, do not try to retrofit into the shared parser. |
| Lexer tokens / keywords | `lua-lex/src/lib.rs:157-265` (`FIRST_RESERVED=257`, `NUM_RESERVED`, ORDER RESERVED keyword table) | **Low.** Modern family keyword set is stable. `goto` exists since 5.2; integer/hex-float literals, `//`, `<<`/`>>`, `~` all present in 5.3 and 5.4. | Largely shared. Only divergence is reserved-word *set* — gate the keyword table as data (a `&[&[u8]]` per version) rather than constants if/when a version adds a keyword (none between 5.3 and 5.4). |
| Parser grammar features | `lua-parse/src/lib.rs:261` (`insidetbc`), `:2201,2642` (to-be-closed `<close>`), attribute syntax `<const>`/`<close>` | **Medium, 5.4-only.** `<const>` and `<close>` attributes and to-be-closed variables are **new in 5.4** (https://www.lua.org/manual/5.4/manual.html#3.3.7). A 5.3 parser must *reject* them; integer `for` with float-detection also differs subtly. | Gate the attribute-parsing productions and the `Tbc`/`Close` emission behind a version flag in `LexState`/`FuncState`. Most of the recursive-descent grammar is shared. |
| Stdlib registration | `lua-stdlib/src/init.rs:58-95` (`LOADED_LIBS` static table → `open_libs`) | **Low/medium.** A single static array of `(name, opener)`. Per-version library *sets* differ only slightly (e.g. `math.atan`/`math.log` arity, removed `bit32`, `os`/`io` stable). | **Clean seam already.** Make `LOADED_LIBS` a per-version table (or filter), and gate individual function registrations inside each `open_*`. This is the cheapest axis to parameterize — it is already data-driven. |
| Stdlib function bodies | `lua-stdlib/src/math_lib.rs`, `string_lib.rs`, `os_lib.rs`, etc. | **Low.** Behavior of most functions is identical 5.3↔5.4. A handful differ (`math.random` algorithm/range — 5.4 uses xoshiro256\*\* and `math.random(0)`; `string.format %s` on non-strings; `print`/`tostring` on floats). | Per-function `if version` branches at the few divergent call sites; do not fork whole modules. |
| GC observable surface | `lua-stdlib/src/base.rs:66-76,392-396` (`collectgarbage` opts incl. `"generational"`/`"incremental"`), `lua-gc/src/heap.rs:461+` (incremental state machine) | **Medium, 5.4-only at the API surface.** Generational mode and the `collectgarbage("incremental"/"generational")` options + new `setpause`/`setstepmul` param semantics are **5.4 additions** (https://www.lua.org/manual/5.4/manual.html#2.5). The engine internals (mark/sweep) are version-invariant. | Gate the `collectgarbage` option *strings/params* per version in `base.rs`. The collector itself stays shared; only the script-visible knobs change. |
| Embedding API (the product) | `lua-rs-runtime/src/lib.rs`: `Lua` (`:283`), `Scope` (`:454`), `Value` (`:1553`), `Table` (`:1617`), `Function` (`:1667`), `LuaString` (`:1720`), `AnyUserData` (`:1744`), `UserData`/`UserDataMethods` (`:2015,2029`), `FromLua`/`IntoLua`/`FromLuaMulti`/`IntoLuaMulti` (`:2496-2512`), `HostHooks` (`:144`) | **Already version-invariant.** None of these types encode a Lua version. `Value` (`:1553`) mirrors the same int/float split and works for any modern version. The only 5.4 coupling is in **doc comments** (`:12,17`), not types. | **No seam in the API.** This confirms the strategic bet: the embedding API is the product, the version is a backend. The seam belongs *below* `lua-rs-runtime`, at engine construction (`Lua::new`/`try_new`/`with_hooks`, `:725-735`) — add a version selector there that picks the backend ISA/parser/stdlib set. |
| Float→string formatting | `lua-vm/src/object.rs:511` (`%.14g`), `:20,628,716` | **Low.** `LUAI_NUMFFORMAT` is `%.14g` in both 5.3 and 5.4. | None for modern family. (Would change for older floats-as-`%.14g` vs 5.1 `%.14g` — same anyway.) |

---

## What's easy vs hard

**Easy (data-driven, already a clean seam):**
- Stdlib library set + per-function gating (`init.rs:58`, individual `open_*`). Just data + a few `if version` branches.
- `collectgarbage` option surface (`base.rs:392`).
- Bytecode header `LUAC_VERSION` (`dump.rs`/`undump.rs`) — trivial constant swap, only matters for precompiled chunks.
- Lexer keyword table — nearly identical; turn constants into per-version data only if a keyword diverges.

**Medium:**
- Parser: gate the 5.4-only `<const>`/`<close>` attribute productions and to-be-closed handling (`lua-parse/src/lib.rs:261,2201,2642`). The bulk of the grammar is shared.
- A few divergent stdlib bodies (`math.random`, format edge cases).

**Hard (the real cost):**
- The **opcode ISA + VM dispatch**. 5.3 and 5.4 have genuinely different bytecode instruction sets and instruction encodings, so the compiler's `OpCode`/`OP_MODES`/`Instruction` (`lua-code/src/opcodes.rs`) and the interpreter dispatch (`lua-vm/src/vm.rs:execute`) must each grow a second variant. This is the one axis where "share aggressively" hits a wall — the instruction sets do not overlap cleanly.
- **Pre-req before that:** the duplicated `OpCode` enum (`lua-code/src/opcodes.rs:87` vs the stub `lua-vm/src/vm.rs:45`) must be consolidated to one owner first. `lua-vm` currently doesn't even depend on `lua-code`.

---

## One parameterized "modern core" or separate cores? (from the actual code)

**One parameterized modern core, split only at the ISA.** The code supports this:

- The expensive-to-duplicate pieces — `LuaValue` (`value.rs:13`), arithmetic/coercion (`arith.rs`, `vm.rs` helpers), table/metamethod machinery (`vm.rs:893-985`), GC engine (`lua-gc/src/heap.rs`), the `_ENV` model (`lua-parse`), and the entire embedding API (`lua-rs-runtime`) — are **already version-invariant** across the 5.3/5.4/5.5 family.
- The genuinely divergent piece is narrow: the **bytecode ISA** (compiler emit + VM dispatch) plus a thin shell of grammar attributes, stdlib gating, and GC knobs.

So the natural shape is: **one shared core crate-set, with the ISA behind a seam** (a `mod v53 / mod v54` or an `Isa` trait owning `OpCode`/`OP_MODES`/`Instruction` and a matching dispatch body), selected at `Lua::new`. A second *whole* core would needlessly fork value/GC/runtime code that has zero version divergence — contradicted directly by what's in `value.rs`, `arith.rs`, and `lua-rs-runtime/src/lib.rs`.

**5.1 is the exception.** 5.1 diverges on the *value model* (no integer subtype) and *globals model* (`fenv`, no `_ENV`) — both deep, shared-core assumptions. 5.1 should be a **separate core**, not a parameter of the modern core. The seams above note where 5.1 would break the shared assumptions (`value.rs:18`, the `_ENV` parser path) so the deferred 5.1 work can branch cleanly rather than retrofit.
