# Lua 5.4 → Safe Rust: Port Strategy

Status: **draft for review.** Nothing in here is committed until you sign off on the load-bearing design decisions in §3.

## 1. Lay of the land

### 1.1 Source corpus

Reference: PUC-Rio Lua 5.4.7 (tag `v5.4.7`), built and verified locally at `reference/lua-5.4.7/`.

| Subsystem | LoC | Key files |
|---|---|---|
| VM core | ~5,000 | `lvm.c` (1899), `ldo.c` (1028), `lapi.c` (1463), `lstate.c` (445) |
| Parser / compiler | ~4,500 | `lparser.c` (1967), `lcode.c` (1874), `llex.c` (581), `lopcodes.{c,h}` |
| GC | ~2,200 | `lgc.c` (1743), `lmem.c` (215), `lstring.c` (274), `lfunc.c` (294) |
| Data structures | ~1,900 | `ltable.c` (995), `lobject.c` (602), `ltm.c` (271) |
| Bytecode I/O | ~580 | `ldump.c` (230), `lundump.c` (335) |
| Stdlib | ~7,000 | `lstrlib.c` (1874, inc. pattern matching), `lmathlib`, `liolib`, `lbaselib`, `ltablib`, `loslib`, `lutf8lib`, `lcorolib`, `lauxlib`, `loadlib`, `ldblib` |
| CLI | 688 | `lua.c` |
| Internal test hooks | 1,983 | `ltests.c` — **not in scope** |
| **Total in scope** | **~28,000** | |

Source counts include comments; effective code is materially less.

### 1.2 Test corpus

Reference: official PUC-Rio test suite at `reference/lua-5.4.7-tests/` (16,522 LoC of Lua, ~3,000 asserts across 33 files).

Driver is `all.lua`. Baseline run with `_U=true` (user-test mode, no internal hooks) completes in **~1 second** and ends in `final OK !!!`. That's our oracle.

`api.lua` (1543 LoC, 276 asserts) tests the C API via `T` and is unreachable from a Rust port — skipped. After exclusions, the runnable target is ~14k LoC of test code.

### 1.3 Data model recap (from `lobject.h`, `lstate.h`)

- `TValue`: 8-byte `Value` union + 1-byte tag. Tag bits: 0–3 type, 4–5 variant, 6 collectable flag.
- 8 base types; variant bits sub-type a few: number → int/float, string → short/long, function → Lua / light-C / C-closure.
- `lua_State`: per-thread state — stack, CallInfo linked list, hooks, error jump, openupvals list.
- `global_State`: shared — string-intern table, GC lists (allgc, finobj, gray, grayagain, weak, ephemeron, allweak, tobefnz, fixedgc, plus generational cohort markers), primitive metatables, panic fn, allocator. **GC is the gnarliest single subsystem.**

## 2. Goals and non-goals

### Goals
- Pass the official Lua 5.4.7 test suite (`_U=true` mode) end-to-end.
- Safe Rust as default — `unsafe` only where there is a documented invariant that the borrow checker genuinely cannot express.
- Rust-native embedding API for use from Rust applications.
- Publishable harness (`PORTING.md`, hooks, oracle scripts) as a first-class artifact, not an afterthought.

### Non-goals (initial scope)
- C ABI compatibility with existing C-Lua extensions. May be added later via a separate `lua-sys` shim crate.
- Loading precompiled `.luac` bytecode from C-Lua. Out of scope — our bytecode is internal.
- Drop-in replacement for OpenResty / Neovim / WoW. They ship LuaJIT or Lua 5.1, not 5.4.
- LuaJIT-level performance. Reference-Lua-level performance is the bar.
- `ltests.c` internal hooks (`T.testC`, etc.).

## 3. Design decisions

The eight load-bearing choices. **Each is a question for you to confirm or override before `PORTING.md` is written.**

### 3.1 C API: Rust-native first

| Option | Pros | Cons |
|---|---|---|
| Port C API verbatim (`lua_pushstring` etc.) | C extensions recompilable | Unsafe across the surface, ugly Rust |
| Pure Rust-native API | Clean, type-safe, ergonomic | No C-extension compat |
| **Rust-native now, `lua-sys` shim later** | Best of both | More work eventually |

**Recommendation: Rust-native now. FFI shim is a separate later artifact.**

### 3.2 `TValue` representation: Rust enum

```rust
enum LuaValue {
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(GcRef<LuaString>),
    Table(GcRef<LuaTable>),
    Function(GcRef<LuaClosure>),
    UserData(GcRef<LuaUserData>),
    LightUserData(*mut c_void),
    Thread(GcRef<LuaState>),
}
```

**Recommendation: Rust enum, not C-style tagged struct.** We don't need C ABI compat. Niche-optimized to ~16 bytes. Compiler enforces exhaustive matching.

### 3.3 Strings: custom byte-string intern pool

Lua strings are **byte-strings, not UTF-8**. They can contain arbitrary bytes including NUL. Short strings (≤40 bytes default) are interned in a hash table on `global_State`; long strings are individually allocated.

**Hard rule for `PORTING.md`:** **never** `String`, `&str`, `from_utf8`, or `to_string` for Lua string data. Use `&[u8]`, `Box<[u8]>`, `Rc<[u8]>`, or our own `LuaString` newtype.

**Recommendation: a `StringPool` owned by `global_State` keyed by content (`HashSet<Rc<[u8]>>` or similar), preserving the short/long distinction.**

### 3.4 GC: stub now, real port in dedicated Phase D

| Strategy | Effort | Test impact |
|---|---|---|
| **Stub: leak everything in Phase A/B/C** | Trivial | All non-GC tests pass; `gc.lua` / `gengc.lua` fail |
| `Rc<RefCell<…>>` + later cycle collector | Medium | Most tests pass; GC-observable behaviors diverge |
| Port Lua's incremental tri-color mark-and-sweep faithfully | High | `gc.lua` passes; faithful behavior |

**Recommendation: leak in A/B/C; port real incremental GC in Phase D as a dedicated subsystem effort.** Pretending we'll do GC "alongside" the VM is how rewrites die. Bracket it.

### 3.5 Stack pointers: indices, never references

C-Lua uses `StkIdRel`, a union that holds a `StkId` pointer normally and a `ptrdiff_t` offset during stack reallocation. In Rust, **every "pointer into the stack" must become an index.** The stack is a `Vec<StackValue>` that reallocates; you cannot hold a `&` or `&mut` into it across any operation that might grow it.

**Hard rule for `PORTING.md`:** internal APIs that took `StkId` take `StackIdx` (newtype `u32`). **No borrows held across stack-mutating calls.**

This is the single biggest *shape* change vs. C and the most common source of borrowck friction. Calling it out up front, in the spec, with a guardrail.

### 3.6 Coroutines: stub through D, stackful via `corosensei` in E

| Option | Effort | Faithfulness |
|---|---|---|
| **Stub: panic on `coroutine.create`** | Zero | Breaks `coroutine.lua`, parts of others |
| OS threads + channels | Low | Behavior-close; perf wrong |
| **Stackful via `corosensei` / `psm`** | Medium | Closest to C |
| Stackless: rewrite VM as suspendable SM | Very high | Cleanest, invasive |

**Recommendation: stub through Phase D; stackful via `corosensei` in Phase E.** `coroutine.lua` and `cstack.lua` are naturally bracketable.

### 3.7 Error handling: `longjmp` → `Result<T, LuaError>`

```rust
enum LuaError {
    Runtime(LuaValue),    // Lua errors carry an arbitrary value, typically a string
    Syntax(String),
    Memory,
    Error,                // error in error handling
    // ...
}
```

**Hard rule for `PORTING.md`:** every fallible internal function returns `Result<T, LuaError>`. Every C-side `luaG_runerror` / `luaD_throw` site → `return Err(...)`. The `?` operator handles propagation cleanly.

### 3.8 Closures and upvalues: `enum Open(StackIdx) | Closed(TValue)`

C-Lua's open upvalues point to a slot on a parent's stack and are "closed" (value copied in) when the parent returns. The naive Rust translation is `Rc<RefCell<TValue>>` for all upvalues, but that throws away the optimization.

```rust
enum UpVal {
    Open { thread: GcRef<LuaState>, idx: StackIdx },
    Closed(TValue),
}
```

**Recommendation: the enum form.** Every read goes through a match, but it's faithful and avoids unnecessary heap allocation for hot upvalues.

## 4. Phase plan

Strangler-fig: each phase lands a real, demonstrable "drop-in works on $N test files" milestone. No phase merges with another. Each begins by passing the previous phase's tests, ends by passing its own.

| Phase | Scope | Oracle (drop-in moment) | Est. agent-weeks |
|---|---|---|---|
| **A. Lexer + parser + bytecode emitter** | `llex`, `lparser`, `lcode`, `lopcodes`. No execution. | Byte-identical `.luac` output vs `luac -o` for a corpus of small Lua programs. | 1–2 |
| **B. VM + minimal data structures (no GC)** | `lvm`, `ldo`, `lobject`, `lstate`, `ltm`, basic `ltable`/`lstring`. Stub GC (leak), stub coroutines (panic), stub finalizers. | `print("hello")` end-to-end. Then `constructs.lua`, `locals.lua`, `closure.lua`, `vararg.lua`, `goto.lua`, `literals.lua` pass. | 2–3 |
| **C. Standard library (most)** | `lbaselib`, `lstrlib` (inc. pattern matching), `ltablib`, `lmathlib`, `lutf8lib`, `lauxlib`. Skip `io`, `os`, `coro`, `debug`. | `strings.lua`, `pm.lua`, `math.lua`, `sort.lua`, `bitwise.lua`, `tpack.lua`, `nextvar.lua`, `utf8.lua` pass. | 2 |
| **D. Real incremental GC + finalizers** | `lgc` proper. Replace leak-everything. | `gc.lua`, `gengc.lua`, weak-table tests in `events.lua` pass. | 1–2 |
| **E. Coroutines + I/O + remaining stdlib** | `lcorolib` via `corosensei`, `liolib`, `loslib`, partial `ldblib`. | `coroutine.lua`, `cstack.lua`, `files.lua`, `db.lua` pass. | 2 |
| **F. Long tail** | Whatever doesn't pass yet. | Full `all.lua` runs green in `_U=true` mode. | 1–2 |

**Total: ~10–13 weeks of agent-time.**

Public drop-in milestones (in addition to test files):
- Post-Phase C: Run [Penlight](https://lunarmodules.github.io/Penlight/)'s test suite.
- Post-Phase E: Run [LuaRocks](https://luarocks.org/) itself (the package manager, written in pure Lua).
- Post-Phase F: Run a real-world Lua project end-to-end (TBD — pandoc Lua filter is the leading candidate).

## 5. Repository layout (proposed)

```
lua-rs-port/
├── PORT_STRATEGY.md          <- this file
├── PORTING.md                <- agent-facing translation rules (to be written)
├── reference/
│   ├── lua-5.4.7/            <- canonical C source (built, working)
│   └── lua-5.4.7-tests/      <- official test suite
├── docs/
│   └── (design notes per phase)
├── harness/
│   ├── oracle/               <- diff drivers comparing C vs Rust output
│   └── hooks/                <- pre-commit, anti-rationalization gate, etc.
└── crates/                   <- Cargo workspace
    ├── lua-types/            <- TValue, GCObject, errors (Phase A foundation)
    ├── lua-lex/              <- llex
    ├── lua-parse/            <- lparser
    ├── lua-code/             <- lcode + lopcodes
    ├── lua-vm/               <- lvm + ldo + lstate (Phase B)
    ├── lua-stdlib/           <- baselib, strlib, tablib, etc. (Phase C)
    ├── lua-gc/               <- real GC (Phase D)
    ├── lua-coro/             <- coroutines (Phase E)
    └── lua-cli/              <- standalone interpreter
```

## 6. Harness architecture (sketch)

Mirrors the Bun PORTING.md / Anthropic cwc-long-running-agents pattern:

- **`PORTING.md`** — agent-facing translation rules. Source-pattern → target-pattern table; banned crates/idioms; hard rules on string types, stack indices, unsafe blocks; mandated per-file `PORT STATUS` trailer.
- **`harness/oracle/run-test.sh <test-file>`** — runs a test file against both C-Lua and our Rust binary, diffs stdout/exit code, writes JSON result.
- **`harness/oracle/test-results.json`** — defaults to FAIL. Flipped to PASS only by evidence-bearing writes.
- **`.claude/hooks/verify-gate.sh`** (PreToolUse) — blocks writes to `test-results.json` unless the agent has read the corresponding evidence file first.
- **`.claude/hooks/unsafe-budget.sh`** (Stop) — fails if `unsafe` block count grew beyond a per-crate budget. **This is the key guardrail against the 13k-unsafe-block Bun outcome.**
- **`.claude/hooks/commit-on-stop.sh`** — commits uncommitted work when the agent stops.

## 7. Open questions for review

Before `PORTING.md` is written, I need your sign-off (or pushback) on:

1. **All eight design decisions in §3.** Strongest opinions: enum `TValue`, leak-then-real-GC, indices not pointers, Rust-native API first.
2. **Phase ordering.** Anything you want to reorder, merge, or split?
3. **Scope of "safe Rust."** I'm proposing: `unsafe` allowed in `lua-gc` and `lua-coro` (stackful coroutines fundamentally need it), forbidden elsewhere, every block gated by an `unsafe-budget.sh` hook with a per-crate ceiling and a `// SAFETY: <why>` comment. Does that match your bar?
4. **Public artifact.** Is the goal a usable crate people would adopt, a research / case-study artifact, pure personal learning, or all three? Affects how much polish to put on the API.
5. **Drop-in target after Phase F.** Penlight + LuaRocks are obvious. Pandoc filters? A specific Lua project you'd want to demo?

## 8. Demo milestones — what "cool, it works" looks like per phase

Beyond `all.lua` passing, each phase has a concrete public-demo target. These are the moments to point at when describing the project externally.

### Post-Phase C — small-program demos

After stdlib lands, before real GC. Anything that doesn't allocate aggressively or use coroutines/io.

- **JSON round-trip:** run [dkjson](https://github.com/LuaDist/dkjson) on a real JSON file; verify byte-identical encode/decode against reference C-Lua.
- **Markdown parser:** run [Lunamark](https://github.com/jgm/lunamark) on a real `.md` file; diff HTML output.
- **[Penlight](https://lunarmodules.github.io/Penlight/) test suite:** the most-used pure-Lua stdlib extensions; ~hundreds of tests run.
- **A random Advent of Code solution** published in Lua. Real-world community code.

**Headline target for this phase: Penlight test suite passing.** Penlight is a known library; "we run it" is meaningful to anyone in the Lua ecosystem.

### Post-Phase D — allocation-heavy programs

GC online. Programs that churn through millions of allocations work correctly.

- **[busted](https://lunarmodules.github.io/busted/)** (pure-Lua test framework) running its own tests against our impl.
- **A brute-force solver** in 200 lines of Lua (Sudoku, n-queens) — sanity-checks GC under millions of intermediate values.
- **A Lua benchmark suite** (shootout-style) — confirm perf is within 2–3x of reference C-Lua. Not LuaJIT territory; we're not trying to be.

### Post-Phase E — the headline demos

Coroutines and io online. This is where it stops looking like a toy.

- **🥇 [LuaRocks](https://luarocks.org/) self-hosting.** The language's own package manager (written in Lua) running through our binary. `luarocks search inspect` or `luarocks list` end-to-end. **The canonical "actually works" moment.**
- **🥈 Pandoc Lua filter.** Write a real markdown-AST filter; route execution through our impl via `pandoc --lua-filter`. Production toolchain integration.
- **🥉 [Copas](https://lunarmodules.github.io/copas/) coroutine scheduler** stripped of its networking (we don't have sockets). Confirms coroutine semantics under load.

### Post-Phase F — pick-anything

Full test suite green; anything pure-Lua-5.4 is fair game.

- A deployed game's logic layer stripped of rendering — e.g. [Defold](https://defold.com/) scripts headless against a recorded input replay.
- A non-trivial published project the user picks (TBD).

### Acceptance criteria per demo

For each demo above, "passes" means:
1. **`./harness/oracle/diff-output.sh <prog>` exits 0** — byte-identical stdout, identical exit code vs reference C-Lua.
2. **No silent skips.** Every example actually executed.
3. **Recorded in `docs/DEMOS.md`** (autogenerated from `harness/oracle/results/*` at phase boundaries) with the command, an output excerpt, and the timestamp.

### Single-pitch tagline (for when this ships)

> "Lua 5.4, in safe Rust, runs LuaRocks."

Self-hosting a language's own toolchain through your reimplementation is the universally-recognized "this is real software" signal.

## 9. Next concrete steps (pending sign-off)

1. Pick the Phase-A oracle corpus (small Lua programs + their `luac -o` bytecode dumps).
2. Write `PORTING.md`.
3. Stub the Cargo workspace under `crates/` per §5.
4. Wire up the harness scripts under `harness/` and the hooks under `.claude/hooks/`.
5. Start Phase A.
