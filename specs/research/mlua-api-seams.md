# mlua API Seams: Where a Rust Lua Embedding Must Be Version-Aware

Mined from mlua `0.12.0-rc.1` (git `39d3201`,
[github.com/mlua-rs/mlua](https://github.com/mlua-rs/mlua)) and the published
`mlua-sys 0.6.8` FFI crate. mlua spans Lua 5.1/5.2/5.3/5.4/5.5 + LuaJIT + Luau
behind one Rust API via mutually-exclusive Cargo features, so its source is the
ready-made map of version-sensitive seams for the WebLua unified API.

Counts of `cfg(feature = ...)` version gates across `mlua/src` (this checkout
also carries the not-yet-released `lua55` and `luau` backends):

| feature | gate count | feature | gate count |
|---|---|---|---|
| `luau` | 387 | `lua52` | 37 |
| `lua55` | 100 | `lua51` | 36 |
| `lua54` | 87 | `luau-vector4` | 19 |
| `lua53` | 62 | `luajit52` | 8 |
| `luajit` | 43 | | |

(We target only the PUC-Rio line 5.1-5.5, so `luau`/LuaJIT gates are reference
material, not requirements. They do show that the *shape* of the seams below is
where every embedder ends up branching.)

---

## 1. Version-Sensitive Public API Surface (the checklist)

Each row is a seam where mlua's public surface or behavior branches on the Lua
version. "Driving dimension" is the upstream change that forces the branch;
URLs cite the upstream manual / source.

| # | API surface | What branches | Driving version dimension | Source |
|---|---|---|---|---|
| 1 | **`Integer` / `Number` type aliases** (`types.rs:27`) | `pub type Integer = ffi::lua_Integer` — width is **i32 in 5.1/5.2, i64 in 5.3/5.4/5.5**; `lua_Number` is always `c_double` on PUC builds | Integer subtype + 64-bit default added in 5.3; configurable width since 5.1 | [mlua-sys lua51/lua.rs:71-73](https://github.com/mlua-rs/mlua), [lua52:73-75](https://github.com/mlua-rs/mlua), [lua54:74](https://github.com/mlua-rs/mlua); [Lua 5.3 manual §8.1](https://www.lua.org/manual/5.3/manual.html#8.1) |
| 2 | **`Value::Integer` vs `Value::Number` distinction** (`value.rs:42-44`) | The two-variant split is only *meaningful* on 5.3+. On 5.1/5.2 the VM has one `number` type (double); `lua_isinteger` is **native in 5.3/5.4** but a **compat shim** for 5.1/5.2/Luau | Integer subtype introduced 5.3 | [mlua-sys lua53/lua.rs (native `lua_isinteger`)](https://github.com/mlua-rs/mlua) vs `lua51/compat.rs`; [Lua 5.3 §3.1 numeric constants](https://www.lua.org/manual/5.3/manual.html#3.1) |
| 3 | **`FromLua`/`IntoLua` for i64/u64/i128/usize…** (`conversion.rs:762` `lua_convert_int!`) | Macro pushes `Value::Integer` when `num_traits::cast` fits the version's `lua_Integer`, else falls back to `Value::Number`. On 5.1/5.2 (`lua_Integer = i32`) far more Rust ints silently become floats; "out of range" errors differ by version | Driven entirely by #1's width | [conversion.rs:762-840](https://github.com/mlua-rs/mlua) |
| 4 | **Integer fast-path on stack reads** (`conversion.rs:805` `from_stack`) | `lua_tointegerx` ok-flag path is taken on PUC; a separate `LUA_TINTEGER` path exists only `#[cfg(feature="luau")]` | Luau has a distinct integer tag; PUC does not | [conversion.rs:803-823](https://github.com/mlua-rs/mlua) |
| 5 | **String <-> number coercion** (`state.rs` `coerce_integer`/`coerce_number`, used by every numeric `FromLua`) | String->number auto-coercion rules and `tointegerx` semantics changed across 5.2->5.3->5.4; 5.4 tightened implicit float->integer in some contexts | Coercion + `__index` on strings evolved 5.3/5.4 | [Lua 5.4 manual §3.4.3](https://www.lua.org/manual/5.4/manual.html#3.4.3) |
| 6 | **`load()` / chunk mode (text vs binary)** (`chunk.rs:155` `ChunkMode`, `detect_mode`) | Text/binary auto-detect by bytecode signature; bytecode format is version-specific and **not portable across versions**. Luau adds a separate `Compiler`/`set_mode`/`compile` path (`chunk.rs:149,205,563`) | Bytecode header/format differs every minor version; `load` `mode` arg semantics ("t"/"b"/"bt") | [Lua 5.4 manual `load`](https://www.lua.org/manual/5.4/manual.html#pdf-load) |
| 7 | **`Function::dump(strip)`** (`function.rs:498`) | `#[cfg(not(feature="luau"))]` — exists for all PUC versions but emits **version-specific bytecode**; `strip` flag honored differently | `string.dump`/`lua_dump` bytecode format | [Lua 5.4 `string.dump`](https://www.lua.org/manual/5.4/manual.html#pdf-string.dump) |
| 8 | **Function environment: `set_environment` / `_ENV`** (`function.rs:388-405`) | `#[cfg(lua51/luajit/luau)]` uses `lua_setfenv`; `#[cfg(lua52/53/54/55)]` manipulates the `_ENV` upvalue. Same for reading the env in `info()` | `setfenv`/`getfenv` removed in 5.2, replaced by `_ENV` upvalue | [Lua 5.2 §8.1 incompatibilities](https://www.lua.org/manual/5.2/manual.html#8.1) |
| 9 | **GC options: incremental vs generational** (`state.rs:1148` `gc_set_mode`, `GcMode`, `GcIncParams`, `GcGenParams`) | Four distinct branches: 5.5 `LUA_GCPARAM`; 5.4 `LUA_GCINC`/`LUA_GCGEN` with pause/stepmul/stepsize; 5.3/5.2/5.1/jit only `LUA_GCSETPAUSE`/`LUA_GCSETSTEPMUL` (no generational); Luau `LUA_GCSETGOAL`. `GcMode::Generational` is meaningless pre-5.4 | Generational GC added 5.4; GC param API reshaped in 5.5 | [Lua 5.4 manual `collectgarbage`/§2.5.2](https://www.lua.org/manual/5.4/manual.html#2.5.2) |
| 10 | **`Thread::reset` (coroutine close)** (`thread.rs:370`) | `#[cfg(lua55/lua54/luau)]` only; within that, `lua_resetthread` (5.4 non-vendored) vs `lua_closethread` (5.5 / vendored 5.4). Returns a status to surface `__close` errors. **Absent entirely on 5.1/5.2/5.3** | `coroutine.close` + to-be-closed vars added 5.4 | [Lua 5.4 `lua_closethread`/`coroutine.close`](https://www.lua.org/manual/5.4/manual.html#lua_closethread) |
| 11 | **Coroutine library presence** (`stdlib.rs:8` `StdLib::COROUTINE`) | `#[cfg(lua55/54/53/52/luau)]` — the `coroutine` *library table* gate excludes 5.1 (coroutines existed in 5.1 but library packaging/`load_std_libs` differs) | Library layout changed 5.2 | [stdlib.rs:8-26](https://github.com/mlua-rs/mlua) |
| 12 | **`utf8` library** (`stdlib.rs:43` `StdLib::UTF8`) | `#[cfg(lua55/54/53/luau)]` — does not exist before 5.3 | `utf8` lib added 5.3 | [Lua 5.3 §6.5](https://www.lua.org/manual/5.3/manual.html#6.5) |
| 13 | **`bit32` / `bit` library** (`stdlib.rs:51` `StdLib::BIT`) | `#[cfg(lua52/luajit/luau)]` — 5.2-only (`bit32`) plus LuaJIT/Luau (`bit`); removed in 5.3 (use native operators) | `bit32` added 5.2, removed 5.3 | [Lua 5.3 §8.1](https://www.lua.org/manual/5.3/manual.html#8.1) |
| 14 | **`Error` enum variants** (`error.rs`) | `Error::GarbageCollectorError` is `#[cfg(lua53/lua52)]` only — the VM only surfaces a distinct `__gc`-error status in 5.2/5.3; folded elsewhere in 5.4. Error *message text* and traceback format also drift by version | `LUA_ERRGCMM` existed 5.2/5.3 only | [error.rs:49-53](https://github.com/mlua-rs/mlua); [Lua 5.4 §8.1 incompatibilities](https://www.lua.org/manual/5.4/manual.html#8.1) |
| 15 | **Warning system: `set_warning_function` / `warning`** (`state.rs:936,982`) | Built on `lua_warning` / `lua_setwarnf`, **added in 5.4**; method bodies must shim/no-op on older versions | Warning subsystem added 5.4 | [Lua 5.4 `lua_warning`](https://www.lua.org/manual/5.4/manual.html#lua_warning) |
| 16 | **Table length / `#` semantics** (`table.rs:589` `len` vs `raw_len`) | `len()` may invoke `__len`; `__len` on tables only honored from 5.2+. `raw_len`/`lua_rawlen` is the version-stable path mlua steers users toward | `__len` for tables added 5.2 | [Lua 5.2 §8.1](https://www.lua.org/manual/5.2/manual.html#8.1) |
| 17 | **`__pairs` metamethod** (iteration) | `__pairs` honored by `pairs()` in 5.2/5.3, **deprecated and removed in 5.4** — an embedder exposing `pairs`/`for_each` must branch | `__pairs` lifecycle 5.2 add / 5.4 remove | [Lua 5.4 §8.1](https://www.lua.org/manual/5.4/manual.html#8.1) |
| 18 | **`MAX_UPVALUES` constant** (`mlua-sys/lib.rs:25-35`) | 255 (5.2-5.5) / 60 (5.1/jit) / 200 (Luau) — affects closure/binding limits surfaced to the embedder | C-source `MAXUPVAL` differs | [mlua-sys lib.rs:25-35](https://github.com/mlua-rs/mlua) |
| 19 | **Registry / `RegistryKey`, named registry, app-data** (`state.rs:1907+`) | Functional everywhere but rides on `LUA_RIDX_*` indices and `luaL_ref` that shifted (`LUA_RIDX_MAINTHREAD` exists 5.2+). App-data/extra-space (`lua_getextraspace`) is 5.3+ | Registry pseudo-indices reorganized 5.2; extra-space 5.3 | [mlua-sys lua53/lua.rs `LUA_RIDX_*`](https://github.com/mlua-rs/mlua) |
| 20 | **`set_globals` / replacing `_G`** (`state.rs:1742`) | Meaningful via `_ENV` only on 5.2+; on 5.1 the global table is reached differently (`LUA_GLOBALSINDEX`) | `_ENV` model 5.2 | [Lua 5.2 §8.1](https://www.lua.org/manual/5.2/manual.html#8.1) |
| 21 | **`pcall`/`error` level + `xpcall` message handler arity** | `xpcall` extra-args + message-handler signature changed 5.2; integer `error` level semantics stable but traceback hook differs | 5.2 calling-convention changes | [Lua 5.2 §8.1](https://www.lua.org/manual/5.2/manual.html#8.1) |

---

## 2. How mlua Structures the Feature Gates (shared vs per-version)

mlua's whole strategy is **"normalize everything to one FFI namespace, then
write the high-level crate once against that namespace."**

1. **Mutually-exclusive version features, enforced at compile time.**
   `mlua-sys/build/main.rs` is a `cfg_if!` chain that ends in
   `compile_error!("You can enable only one of the features: lua54, lua53,
   lua52, lua51, luajit, luajit52, luau")`. You physically cannot build mlua
   with two Lua versions. Each arm links exactly one C library.

2. **One submodule per version, re-exported into a single `ffi::` namespace.**
   In `mlua-sys/src/lib.rs`:
   ```
   #[cfg(any(feature = "lua54", doc))] pub use lua54::*;
   #[cfg(any(feature = "lua53", doc))] pub use lua53::*;
   #[cfg(any(feature = "lua51", feature = "luajit", doc))] pub use lua51::*;
   ...
   ```
   Whichever feature is on *becomes* `ffi`. (Note 5.1 and LuaJIT share the
   `lua51` module.)

3. **A per-version `compat.rs` shim layer.** Older backends (`lua51/compat.rs`,
   `lua52/compat.rs`, `luau/compat.rs`) hand-implement the missing 5.3/5.4-shaped
   functions (e.g. `lua_isinteger`, `lua_tointegerx`, `luaL_*` helpers) so the
   upper crate can call a single uniform API. The compat layer is where "make an
   old version look like the newest" lives.

4. **High-level crate branches only at genuine divergences.** `mlua/src/*.rs`
   targets the unified `ffi` namespace and adds `#[cfg(feature = ...)]` *only*
   where behavior or surface truly differs (the 21 seams above). Hot spots:
   `state.rs` (107 gates — GC, libs, sandbox, warnings), `userdata.rs` (52),
   `debug.rs` (38), `function.rs`/`stdlib.rs` (30 each).

5. **Feature-gated public items carry `#[cfg_attr(docsrs, doc(cfg(...)))]`** so
   docs.rs renders which versions expose each method (e.g. `Thread::reset`,
   `StdLib::UTF8`).

Net: the gate density is *low in the type/value core* (the superset `Value`
enum is shared; only `Vector`/`Buffer` are gated, both Luau) and *high in
state/GC/stdlib/debug* — exactly the subsystems where Lua's C API changed
between minor versions.

---

## 3. The Hard Limitation: Compile-Time-Only Version Selection

**mlua cannot switch Lua versions at runtime, and cannot host two versions in
one binary.** Proof, directly from the source:

- `mlua-sys/build/main.rs` `compile_error!(...)` rejects any build with >1
  version feature. The crate links **one** C library (`-llua54` *or*
  `-llua53` …), chosen by Cargo features at compile time.
- `mlua-sys/src/lib.rs` does `pub use lua54::*` so the symbol `ffi::lua_pcall`
  resolves to *one* version's binding for the whole compilation. There is no
  vtable or dispatch — the version is baked into every call site.

**Why this is structural, not laziness:** mlua is a *binding*. The behavior
lives in C objects that share C global symbol names (`lua_pcall`, `lua_newstate`
…). You cannot statically link two C Luas into one binary without symbol
collisions, and even if you renamed them, the `lua_Integer` width (#1) changes
the ABI of nearly every function — there is no single Rust signature that
serves both i32-based 5.1 and i64-based 5.4. So the version *must* be a
compile-time monomorphization. A consumer who wants "5.3 and 5.4 in the same
process" has to compile two separate mlua-using crates/dylibs and bridge them.

**Why a pure-Rust implementation CAN multiplex at runtime — the WebLua
differentiator:** lua-rs has no C symbols and no foreign ABI. Each version is a
Rust module/backend behind a trait, so a single binary can hold the 5.1, 5.3,
and 5.4 VMs simultaneously and pick one *per `Lua::new(version)` call* at
runtime. For the WASM playground pitch this is the headline: **one `.wasm`
artifact, a dropdown that switches Lua versions live**, no recompile, no second
download. mlua *cannot* ship that artifact.

**The costs of runtime multiplexing (be honest about them):**
- **Binary / `.wasm` size.** Carrying N VM backends ~ N× the VM code (mitigable
  by aggressive sharing where versions overlap — lexer, most of the parser,
  table/string runtime — and by feature-gating backends out for size-sensitive
  embeds).
- **Superset `Value`.** A runtime-multiplexed `Value` must be the *union* of all
  versions' value spaces (e.g. integer subtype present for 5.3+, absent-but-
  representable for 5.1/5.2). That means either (a) one fat enum whose
  integer/float discipline is enforced *per active version* rather than by the
  type, or (b) a version tag carried alongside. mlua sidesteps this by letting
  the compile-time version *define* the enum; we pay for flexibility with a
  wider, version-checked value type.
- **No type-level version guarantee.** mlua can make `Thread::reset` literally
  not exist on 5.3 (`#[cfg]`). A runtime-multiplexed API must instead return a
  runtime `Error::VersionUnsupported`-style result for version-absent features —
  weaker than a compile error.
- **Dispatch cost.** A per-call `match version { ... }` or trait-object indirection
  on hot paths; negligible vs the upside but non-zero.

---

## 4. Lessons for the WebLua Unified API

### Seams that are genuinely unavoidable (must design for explicitly)
1. **Integer width & the integer/float distinction (#1-#5).** This is the
   deepest seam and it's a *value-representation* decision, not a config flag.
   It touches every `FromLua`/`IntoLua`, every arithmetic result, every
   `tostring`. Decide it first.
2. **GC option surface (#9, #10).** Generational GC and `coroutine.close`/
   to-be-closed simply do not exist before 5.4. Any GC-tuning or coroutine-close
   API must be version-conditional.
3. **`_ENV` vs `setfenv` (#8, #20).** The 5.1->5.2 environment model break is
   load-bearing for sandboxing — a primary WebLua/WASM use case.
4. **Stdlib roster (#11-#13).** `utf8` (5.3+), `bit32` (5.2-only). The set of
   libraries you can `open` is version-defined.
5. **Bytecode (load binary / `dump`) (#6, #7).** Non-portable across versions;
   text-mode load is the only version-stable path. For a playground, prefer
   text; treat binary chunks as version-pinned.

### Recommended shape: compile-time gating WITH optional runtime multiplexing
- **Adopt mlua's "one normalized core" discipline.** Share aggressively:
  lexer, parser, and the bulk of the runtime/table/string code are version-
  agnostic. Branch *only* at the 21 seams. mlua proves the gate density is low
  in the value core and concentrated in state/GC/stdlib/debug — mirror that.
- **Make version a backend behind a trait, not a Cargo feature, as the
  *default*.** This is the inversion vs mlua and the entire WebLua thesis: the
  embedding API is the product, the Lua version is a runtime-selectable backend.
  `Lua::builder().version(LuaVersion::V5_4).build()`.
- **Keep a compile-time feature to *exclude* backends** (`default-features =
  false`, `version-5_4` etc.) so size-sensitive embeds (small WASM, a server
  pinning one version) pay only for what they use. Runtime-multiplex when all
  backends are compiled in; degrade to a single backend (mlua-style, with the
  same ergonomics) when only one is.
- **Define `Value` as the superset, with version-checked construction.** One
  enum across versions; enforce the integer-subtype discipline against the
  *active backend's* rules at the boundary (coercion sites), not via separate
  types. This is the cost called out in §3 and it's the right trade for the
  pitch.
- **Version-absent features return a typed runtime error**
  (`Error::Unsupported { feature, version }`), not a panic — the runtime analog
  of mlua's `#[cfg]` removal. Document per-method version support the way mlua's
  `doc(cfg(...))` does, generated from the same backend trait.
- **Treat the upstream official test suites strictly as the oracle.** Each
  version's suite validates that backend's behavior; they do NOT dictate the
  unified API's internal structure (we are past the C-mirroring phase).

---

### Top 5 seams we MUST design for (priority order)
1. **Integer width + integer/float distinction** — i32 (5.1/5.2) vs i64 (5.3+),
   subtype native only 5.3+; pervades all numeric `FromLua`/`IntoLua`.
2. **GC mode surface** — generational + `coroutine.close`/`Thread::reset` are
   5.4-only; pre-5.4 has only incremental pause/stepmul.
3. **`_ENV` vs `setfenv` environment model** — the 5.1->5.2 break; central to
   sandboxing, WebLua's key use case.
4. **Stdlib roster** — `utf8` (5.3+), `bit32` (5.2-only), `coroutine` library
   packaging; the openable-library set is version-defined.
5. **Bytecode portability** — `load(binary)` / `Function::dump` formats are
   version-pinned; only text load is version-stable.
