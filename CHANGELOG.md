# Changelog

All notable changes to `omniLua` are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed — embedding VM churn no longer retains io stream entries (#317)

The `io` library's `LStream` side table (file-handle userdata identity →
stream) lived in a process-wide `thread_local!`, so it outlived every VM
that filled it: each `Lua` instance registered its `stdin`/`stdout`/`stderr`
(plus any opened files) and never removed them. Retention per dropped VM
scaled with how rarely the allocator hands back a previously seen userdata
address — small on Linux (~30 B/iter in the leak canary), large on Windows
(~40–260 B/iter, the envelope 0.7.1 recorded). The table now lives on
`GlobalState` and dies with its VM; the leak canaries measure exactly zero
growth across all churn scenarios on Linux, and a new `io_registry_churn`
scenario pins the fix at the strict POSIX tolerance.

## [0.7.1] - 2026-07-21

Windows repair release: 0.7.0 could not be installed on current MSVC
toolchains, and the first-ever Windows run of the workspace test suite
surfaced two more Windows-only breakages, fixed here.

### Fixed — `cargo install omnilua-cli` links again on VS 18 / MSVC 14.5x (#308)

The CLI's `os.date` local-offset hook called `libc::localtime_s`/`gmtime_s`,
which are CRT header-inline names, not linker symbols; Visual Studio 18
toolchains (also GitHub's current `windows-latest` image) no longer resolve
them, so every install failed with LNK2019. The hook now calls the real
MSVCRT exports `_localtime64_s`/`_gmtime64_s`. The libc declarations are
reported upstream as rust-lang/libc#5299. The tm-differencing day math
behind the hook is now a pure function unit-tested on every platform.

### Fixed — Windows `os.execute`/`io.popen` never spawned (PR #313)

Both hooks hardcoded `/bin/sh -c`. On Windows they now run `%COMSPEC% /C`
with the command text passed raw — the CRT `system`/`_popen` contract —
plus a cross-platform `shell_hooks` integration test.

### Fixed — Windows `package.path`/`package.cpath` defaults were stubs (PR #311)

The Windows defaults are now byte-faithful per-version transcriptions of
each release's `luaconf.h` `_WIN32` branch, and `setprogdir` is implemented:
every `!` (`LUA_EXEC_DIR`) in the composed path — env-provided values
included, matching C's ordering — is replaced with the executable's
directory.

### CI / infrastructure (PRs #310, #312, #314)

- The release Windows gate now RUNS the workspace test suite on MSVC (it
  was build-only, which is how #308 shipped through five green releases);
  npm publish is gated on it too, closing the half-released-skew hole.
- New `windows-check.yml` manual dispatch to verify any branch on real MSVC.
- `Cargo.lock` is now committed; Windows gate builds with `--locked`.
- `size_class_histogram` example gated to macOS (its `malloc_good_size`
  probe target does not link elsewhere).
- New `docs/WINDOWS_DIVERGENCES.md` ledger: what is fixed, what is open
  (text-mode stdio), what remains unverified until the oracle can run on
  Windows.
- The first Windows suite runs also hardened the tests themselves: six
  POSIX-hardcoded loadlib expectations are now platform-aware (PR #316),
  and the embedding leak canary got a measured Windows byte envelope
  (PR #318) for a pre-existing, GC-reclaimable ~40-260 B/VM retention now
  tracked as #317 — the detached-allocation tripwire stays exact
  everywhere.

## [0.7.0] - 2026-07-15

### Breaking — filesystem host hooks now return `std::io::Result` (#301)

`FileOpenHook`, `FileRemoveHook`, and `FileRenameHook` changed from
`Result<_, LuaError>` to `std::io::Result<_>` (i.e. `Result<_, std::io::Error>`).
This is required for fidelity: only `std::io::Error` carries `raw_os_error()`,
and `io.open`/`os.remove`/`os.rename` must report the real errno as the third
return value the way C's `luaL_fileresult` does. A `LuaError` return could not
carry an errno, so the boundary was silently dropping it (see the Fixed note
below). Embedders installing these hooks must return `io::Error` (typically
straight from `std::fs`, which already carries the OS errno) instead of building
a `LuaError`. The wasm host ABI's `open_file` import gained an errno-carrying
failure convention (`id <= -2` encodes `errno = -id`; `-1` stays "failure, no
errno").

### Fixed — io/os error fidelity: real errno + clean strerror text (#301)

`io.open`, `os.remove`, and `os.rename` on failure returned errno **0** and a
verbose Rust-style message (`... (os error 2)`) where reference Lua returns the
real errno and the bare `strerror` text (`No such file or directory`, errno 2).
Root cause: the file-hook boundary re-wrapped the OS error as
`io::ErrorKind::Other`, discarding `raw_os_error()`. The hooks now propagate the
original `io::Error` end to end; the failure message is rendered as clean
`strerror`-shaped text; and the errno is reported only when the error actually
carries one (a non-OS failure — e.g. a sandbox "no filesystem hook registered" —
now yields the honest 2-value `(nil, msg)` instead of a fabricated errno 0).
Also fixed alongside: `os.remove` on a symlink now reports the real `unlink`
errno (mirroring C `remove(3)`) instead of a spurious `rmdir` `ENOTDIR`;
`io.input`/`io.output`/`io.lines` open-failure messages are version-gated
(Lua 5.1 uses the `argerror` form, 5.2+ the `cannot open file` form, with the
`luaL_where` location prefix), matching every reference binary 5.1–5.5;
`io.write`/`file:write` to a bad descriptor now RETURN the `luaL_fileresult`
failure tuple (`nil, "Bad file descriptor", 9`) instead of raising, with Lua
5.5's extra fourth value (total bytes written) appended per `g_write`; and
reads/writes against a wrong-mode handle report the real `EBADF` the OS yields.
On `wasm32-unknown-unknown` — where `std` has no `strerror` table and
`io::Error` `Display` renders "operation successful" — the errno message is
resolved from a target-independent POSIX `strerror` table so the wasm build's
`(nil, msg, errno)` matches a native build's.

### Fixed — `<const>` compile-time-constant folding (#300)

A `local x <const> = <constant-expr>` in Lua 5.4/5.5 is now **folded**: the
value is stored on the variable descriptor, the variable is marked a
compile-time constant, and no register/local and no upvalue are emitted for it —
references materialize the constant inline (`LOADI`/`LOADK`/`LOADFALSE`/…),
exactly as reference Lua does. Previously the port kept it as an ordinary local,
so `debug.getlocal`/`debug.getupvalue` reported a phantom local/upvalue that the
reference does not. Implemented on top of a faithful register-accounting refactor
(a real-local-register watermark decoupled from `nactvar`, mirroring C's
`luaY_nvarstack`/`reglevel`), which is byte-identical for code containing no
`<const>` constants. The 5.5 `global`-declaration barrier correctly shadows a
folded constant across enclosing/intermediate function levels.

### Fixed — Lua 5.5 per-frame `global`/local name resolution (#304)

The 5.5 `global x` declaration now shadows an enclosing regular local `x` at
every function level, matching reference Lua — the decision is made per frame
during name resolution (before any upvalue capture) instead of post-hoc, which
could not un-capture a variable. This corrects four divergences: a captured
local shadowed by a later `global x` (the headline; previously the nested
function saw the local, not the global), a `global function` name shadowed by an
inner local, a named vararg parameter (`function f(...x)`) shadowed by `global x`,
and `global function x` with an enclosing local (which previously left a phantom
upvalue). `_ENV`-shadowing globals now raise the reference error
(`_ENV is global when accessing variable '<name>'`). Pre-5.5 output is
byte-identical (the machinery is inert without `global` declarations).

### Fixed — LightC pointer identity + embedding-API stubs (#278)

`tostring`/`string.format("%p", …)`/`debug.debug`/the public
`Function::to_pointer()` now report the **same, real** address for a built-in
C function — previously three separate resolvers disagreed, and some returned a
tiny registry index instead of the function's code address. `luaL_tolstring`
display fidelity was corrected (light userdata renders `userdata`, `__name` is
honored on any reference value for 5.3+, numeric `__tostring` satisfies the
pushed-string contract on 5.2+), several embedding-API stubs were made
functional (`to_cfunction`, C-closure/userdata construction with GC pacing,
upvalue-count validation), and dead code from the de-port sweep was removed.

### Performance — UpVal representation shrink (#113)

`GcBox<UpVal>` shrank 56 → 48 bytes (three `Cell`s collapsed into one
`Cell<UpValState>` enum), crossing the libmalloc 64→48 size class: measured
**−16.7% closure_ops peak RSS**, wall-flat, +1.9% Ir (CPI-absorbed — see the
layout-shrink rule in `docs/MEASUREMENT_PROTOCOL.md`). The size-class histogram
analysis (#296) confirmed UpVal was the only GC object whose shrink both crosses
a malloc class and has the live population to move RSS.

## [0.6.0] - 2026-07-13

### Fixed — deterministic close (#260)

`close(state)` now does what `lua_close` does: runs close finalizers
(draining both queues, exactly-once per object) and then frees every heap
object deterministically before returning — previously `free_all_objects`
was a no-op and destruction rode on scope drop, so a lingering guard or
handle could delay teardown indefinitely. The heap gains a closed state
(allocation after close panics with a clear message; re-entrant allocation
from destructors during teardown is supported via drain-until-stable), and
the CLI now exits through the deterministic path. Weak handles die
immediately at close.

### Performance — GC header diet, Wave 1 (#113)

Every GC object's header shrank 40 → 24 bytes (the grayagain revisit list
moved from an intrusive per-object link to one heap-owned vector). Measured:
closure_ops peak RSS −11.8%, binarytrees ~−5%, compute-workload instruction
counts flat. Wave 2 (removing the second link) was built, adversarially
reviewed, measured RSS-negative on churn workloads, and closed unmerged
with full evidence — see `docs/PERF_EVIDENCE_113_W2_OWNERVEC_20260713.md`
and the new "Patterns from the owner-vector negative" section of
`docs/PERFORMANCE_PRINCIPLES.md`.

### Changed — GC guard-coverage panics are unconditional

The last two env-gated GC guard checks are now always-on, completing the
issue #249 → #251 → #253 arc. A guard-less `GcRef::downgrade` of a
heap-owned box (which would mint a `GcWeak` that upgrades forever, including
after sweep frees the target) and a guard-less `GcRef::account_buffer` on a
heap-owned box (whose pacer charge would be silently dropped) now **panic
unconditionally in every build** — previously they degraded silently unless
`OMNILUA_GC_STRICT_GUARD=1` was set. With all three guard checks (allocation
since #253, plus these two) always-on, the `OMNILUA_GC_STRICT_GUARD` env var
is retired; `harness/strict_guard_check.sh` remains the convenience runner
for the workspace GC gate. Detached (`Gc::new_uncollected`, process-lifetime)
boxes keep their legacy behavior.

## [0.5.0] - 2026-07-12

### Breaking — `LuaError` representation (#253)

`LuaError` gained two variants, `RuntimeMsg(Box<[u8]>)` and
`SyntaxMsg(Box<[u8]>)`: host-side error construction
(`LuaError::runtime(...)` and friends) now carries the message as owned
bytes instead of allocating a GC string, and the string is materialized
only when the error enters a VM. Two consequences for embedders:

- **Exhaustive matches on `LuaError` stop compiling** — add arms for the
  new variants, or better, switch to the stable accessors:
  `message_bytes()` / `message_lossy()` (never allocate, never panic) and
  `to_status()` for classification.
- **Wildcard matches like `Runtime(LuaValue::Str(s)) => ..., _ => ...`
  keep compiling but now take the `_` arm for host-constructed errors** —
  audit these; `message_bytes()` covers both representations.
- `into_value()` now documents (and enforces) that it requires an active
  heap for `*Msg` payloads; hosts that only need the text should use the
  accessors above.

Why: constructing an error with no VM in scope previously either required
an active `HeapGuard` or silently allocated a detached, never-freed box —
the last remnant of the issue #249 leak class. With the message carried as
bytes, **the guard-less allocation fallback is deleted entirely**: a
`GcRef::new` with no active heap now panics with an entry-path diagnostic
in every build, making that leak class unrepresentable. `lua_resume` and
`reset_thread` are now guard-self-sufficient as part of the same sweep.

### Fixed — weak-handle and guard soundness (#252)

`Heap` now lives behind `Rc` end to end: `HeapGuard` holds a strong
reference on the TLS stack (a guard can no longer dangle, with no
documented-contract caveats), and weak-reference heap identity
(`HeapRef`) holds a `Weak<Heap>` — a `GcWeak` that outlives its VM now
answers upgrade checks with `false` instead of touching freed memory.
One `unsafe` block removed from `lua-gc`. `Heap::new()` returns
`Rc<Heap>` (breaking only for direct `lua-gc` consumers; the embedding
API is unaffected).

### Docs

README and CLI crate README now describe omniLua accurately as an
AI-assisted C-to-Rust port of PUC-Rio Lua 5.4.7 extended to 5.1–5.5
(#248, wording per the reporter), and `specs/` gains the reviewed design
spec for the safe GcHeader diet (#113).

## [0.4.4] - 2026-07-12

### Fixed — GC ownership of bootstrap allocations (#249, #250)

Creating and dropping many `Lua` VMs no longer leaks. GC objects allocated
during VM bootstrap (registry, stdlib install, tagmethod tables, interned
strings) fell back to a detached allocation whenever no `HeapGuard` was
active — invisible to both sweep and `Heap::drop_all` — so each
`Lua::new()`/drop cycle leaked ~29KB for the life of the process, and every
`load(..).into_function()` leaked the chunk's `_ENV` upvalue. Bootstrap
allocations now live on a heap-owned never-swept list freed at heap
teardown, and `lua_vm::api::load` holds its own guard so chunk allocations
are collector-owned. Reported, diagnosed, and fix-designed by
@xyzshantaram.

### Added — strict-guard mode and embedding leak canaries (#251)

The bug class behind #249 is now mechanically detectable.
`OMNILUA_GC_STRICT_GUARD=1` turns the GC's silent no-active-heap fallback
arms (detached allocation, sweep-blind weak handles, dropped pacer charges)
into panics with backtraces; `harness/strict_guard_check.sh` runs the
workspace under it. A counting-global-allocator canary asserts net-zero
live bytes and a zero detached-allocation delta across VM, chunk,
coroutine, and callback churn. The strict sweep found and fixed three more
gaps: `Lua::with_state` (the embedding funnel) now activates the state's
heap — guard-less weak-cache handles created there could outlive their
swept targets — and `LuaRuntime::install_sandbox` and the hlua shim's
stdlib install run guarded. The last pre-heap per-VM leak (~112B) is gone:
`new_state` builds the `Heap` first and allocates its placeholders on the
heap-owned list. Bootstrap windows are RAII (`BootstrapScope`), sound with
no unsafe. Follow-ups: #252 (heap shared ownership), #253 (LuaError owned
message bytes).

## [0.4.3] - 2026-06-29

### Fixed — CLI startup banner

The `omnilua -v` / interactive REPL banner no longer claims the PUC-Rio copyright
and now reflects the Lua version actually selected. omniLua is an independent
implementation, so it names itself and its own version — following the LuaJIT
precedent of printing its own identity rather than the reference interpreter's:

```
omniLua 0.4.3 (Lua 5.4) -- pure-Rust Lua. https://github.com/ianm199/omnilua
```

Previously it printed a hardcoded `Lua 5.4.7  Copyright (C) 1994-2024 Lua.org,
PUC-Rio` regardless of `OMNILUA_VERSION`. The `Lua 5.x` language level remains a
substring of the banner for tooling that greps it.

## [0.4.2] - 2026-06-27

### Added — async host functions (`async` feature)

`Lua::create_async_function` registers a Rust `async` function callable from Lua;
`Function::call_async` / `Chunk::eval_async` / `exec_async` drive Lua from Rust
while awaiting host async functions. Built on the coroutine machinery (no VM/GC/
unsafe changes): an async function yields a per-function capability token
(light-userdata, unforgeable by Lua and version-invariant), and the driver awaits
the registered Rust future with no VM borrow held — rooting of the suspended
coroutine is inherited from the coroutine path. The genuine `coroutine.*`
primitives are captured at construction into the script-inaccessible registry, so
global reassignment cannot bypass or break async dispatch (important for the
untrusted-script case). Executor-agnostic and `!Send` (use a single-threaded
executor such as tokio's `LocalSet`); pure-std, no new dependencies; the `async`
feature enables `coroutine`. A future error propagates to the Rust caller (the
coroutine is left suspended); native-only (no wasm executor). Implements
`specs/embedding/async-integration.md`.

## [0.4.1] - 2026-06-27

### Added — serde integration (`LuaSerdeExt`)

`Lua::to_value` / `from_value` convert between any `Serialize`/`Deserialize` Rust
type and a Lua `Value`, mirroring `mlua`'s `LuaSerdeExt`. Pure-Rust and
feature-gated (`serde`), so it also builds for `wasm32-unknown-unknown` — which a
C-backed binding cannot. Host integers cross the version number-model seam via
`LossyIntPolicy`; `None`/`null` use a non-nil sentinel (`null()`) and sequences
carry an array-marker metatable (`array_metatable()`) so empty arrays round-trip
distinctly from empty maps. Conventions match `mlua` (externally tagged enums,
byte-safe strings). Also ships `specs/embedding/async-integration.md`, the design
spec for a future async lane (not implemented).

## [0.4.0] - 2026-06-26

### Added — embedding-API parity tier: host coroutines, registry keys, GC control, lazy iteration

Completes the `mlua`-shaped host surface so ported `mlua` code finds its idioms,
all driving the corresponding Lua builtins so behavior is identical to running
the same code purely in Lua (per-version nuances and provenance come for free).

- **Host-driven coroutines** (#230): `Lua::create_thread(Function)`,
  `Thread::resume::<A, R>(args)`, `Thread::status() -> ThreadStatus`. The host
  can create, step, and observe a coroutine without dropping into Lua-level
  `coroutine.*`. Provenance-checked to the parent instance.
- **Keyed registry** (#226): `Lua::create_registry_value` / `registry_value` /
  `remove_registry_value` with the anonymous `RegistryKey` token, alongside the
  existing named registry. Provenance-checked.
- **GC control surface** (#231): `Lua::gc() -> GcControl` with
  `collect`/`step(kb)`/`stop`/`restart`/`count() -> f64`/`is_running()`,
  matching `collectgarbage(...)` and its per-version option roster.
- **Lazy table iteration** (#232): `Table::pairs` (honors `__pairs` where the
  running version does — 5.2–5.5) and `Table::raw_pairs_iter`, yielding one
  pair at a time via the new `TablePairs` iterator instead of materializing a
  `Vec` (the eager `raw_pairs()` is unchanged).

### Fixed

- **`coroutine.resume(coroutine.running())` on the main thread** (#239) now
  reports `cannot resume non-suspended coroutine` (5.2–5.5) instead of
  `cannot resume dead coroutine`. The main thread is never stored in the thread
  registry, so the resume registry-miss path is special-cased to the
  version-aware non-suspended message, matching the reference.

### Added — multi-version capability seam (#234)

Makes the multi-version differentiator usable at the API boundary instead of
inert. A version-indexed **capability matrix** is the realized form of the
WebLua spec's `Backend` contract (per-version `enum Engine` backend structs are
deferred — the single versioned core already meets the goal).

- **`Feature` enum + `LuaVersion::supports`/`features`** — the capability matrix,
  whose authority is `ANALYSES/version_feature_matrix.tsv`, generated from the
  reference binaries (`specs/oracle/gen_feature_matrix.sh`); a test asserts
  `supports()` against it for every `(version, feature)` so it can't drift.
- **`Lua::supports(Feature)`** ANDs version capability with compile-time stdlib
  availability (a lean build reports `utf8`/`bit32` absent); `LuaVersion::supports`
  stays build-independent. `Feature`/`Unsupported` are re-exported.
- **Typed `Unsupported { feature, version }`** on the embedding `Error`
  (`as_unsupported`/`is_unsupported`) for host-API verbs that name a
  version-absent feature — e.g. `gc().is_running()` on pre-5.2 now returns a
  typed error, not a raw Lua "invalid option" message.
- Internal: the `utf8`/`bit32` registration gates now consume the matrix
  (single source), keeping the `#[cfg(feature)]` compile-time gate.

Gate: `cargo test --workspace` green, official 5.4 suite 44/44, the
multi-version oracle green across 5.1–5.5.

## [0.3.4] - 2026-06-24

### Added — feature-gated standard library for lean / sandboxed embeds

`omnilua` now lets an embedder compile out the sandbox-forbidden standard
libraries — `io`, `os`, `package`/`require`, `debug` (and optionally
`coroutine`, `utf8`, `bit32`) — so a build that runs Lua in a Redis-style
sandbox (e.g. Valdr/EdgeStash on `wasm32`) ships neither their code nor the
fs/loader/OS surface it forbids. `base`, `string`, `table`, and `math` are
always present.

- New Cargo features on `omnilua` and `lua-stdlib`, **all on by default** — a
  default build, LuaRocks, and the full official suite are unchanged. A lean
  build is `default-features = false`, re-enabling any subset (e.g.
  `features = ["os", "coroutine"]`).
- Each feature gates both the module's compilation and its registration in
  `luaL_openlibs`; `debug` implies `coroutine` (it introspects coroutine
  threads).
- Verified end-to-end: `cargo run -p omnilua --no-default-features --example
  sandbox_smoke` (core libs work, gated libs absent) and a lean
  `wasm32-unknown-unknown` check that strips the libraries from the bundle.
  No behavior change on the default profile (44/44 official 5.4 suite green).

## [0.3.3] - 2026-06-23

### Added — metamethod-free `Table` access on the embedding API

Exposes raw, metamethod-bypassing table access on `omnilua::Table`:
`raw_get` / `raw_set` / `raw_pairs` (raw iteration) plus `set_metatable` /
`get_metatable`. Needed by embedders that require metamethod-free table access
(the redis-rs-port mlua-exit scripting backend). No runtime behavior change to
the language; embedding-API surface only.

## [0.3.2] - 2026-06-22

Same library code as 0.3.1 (all five Lua versions at 100%); this patch unblocks
the **npm** publish. No runtime behavior change.

### Fixed (release harness only)

- The wasm package gate (`harness/check_wasm_package.sh`) asserted the old
  `io.tmpfile()` / `file:read` failure return of `false`; the reference-faithful
  value is `fail` (`nil`, via `luaL_pushfail`), which the 0.3.x runtime already
  returns. Updated the wasm Node + low-level smoke scenarios
  (`harness/wasm/smoke-scenario.mjs`, `crates/lua-wasm-smoke` — a `publish=false`
  test crate) to assert `nil`. No change to any published library crate.

## [0.3.1] - 2026-06-22

### Multi-version: all five versions now pass their full official suites

**omniLua now passes 100% of the official PUC-Rio Lua test suite for every
version it speaks — 5.1, 5.2, 5.3, 5.4, and 5.5** — measured against the
unmodified reference binaries under the identical stock harness (`ltests`-only
files, which the reference itself can't run standalone, excluded). This release
closes the remaining 5.1 and 5.3 gaps (5.1 rose from ~40% to 100%, 5.3 from
~74% to 100%) with zero change to the 5.4/5.5 baseline.

- **Lazy `load()` reader streaming** — a reentrant reader now feeds the lexer on
  demand (stopping at the first syntax error instead of draining the whole
  reader), and the duplicate `ZIO` types are unified. Fixes `load()` with a
  function reader and `load(io.lines(...))`. Also fixes a GC use-after-sweep when
  a reader runs `collectgarbage()` mid-parse, and `io.lines` arity per version.
  The public `omnilua::load()` signature is unchanged.
- **Lua 5.1 environment model** completed — per-thread global table (`l_gt`) for
  `setfenv(0)` inside coroutines, per-closure environments for closures with no
  `_ENV` upvalue, the implicit `arg` vararg table, the same-reference metamethod
  rule, and `getfenv` level/tail-call handling.
- **Debug/GC fidelity** — 5.1 "tail return" hook events and synthetic tail-call
  frames; 5.3 finalizer-frame (`CIST_FIN`) naming so `debug.getinfo` inside a
  `__gc` reports the metamethod (eliminating a `db.lua` hang); weak-value
  clearing ordered before finalizer resurrection; white proto-cache drop;
  suspended-coroutine cycle finalization; collect-time userdata finalizability
  on 5.1.
- **Parser/lexer/number-model gates** — 5.1 stack ceiling corrected to 65500
  (fixing an `xpcall`-over-stack-overflow hang), `"too many syntax levels"` /
  `LUAI_MAXUPVALUES=60` limits, control-byte token rendering, oversized-hex
  `tonumber`, float-only `next` key normalization, and the
  `%x`/`%u`/`%o`/`%q`/`%g`/`%s` conversions, each version-gated.

### Harness

- `harness/quick_file.sh` (8s-capped whole-file oracle check), `harness/gen_golden.sh`
  + committed golden vectors, the `dump_kit` / `error_wording_kit` in-process kits,
  and `harness/multiversion_diff_suite.sh` (per-version differential gate). The
  official **5.1** and **5.2.2** test suites are vendored under `reference/extra-tests/`.

## [0.3.0] - 2026-06-22

### Added — multi-version compatibility (5.1–5.5)

Substantial progress toward 1:1 reference parity on the older language versions,
measured against the unmodified PUC-Rio reference suites under the identical
stock harness. Official self-checking suite pass rates moved from
5.1 40% → **86%**, 5.2 54% → **100%**, 5.3 74% → **93%** (its real-bug floor),
with **5.4 and 5.5 held at 100%** (zero regression on the baseline throughout).

- **5.2 now passes its full official suite.** 5.3 reaches its real-bug floor
  (remaining failures are an `ltests`-dependent harness file and the
  cross-cutting lazy-reader item below).
- **Bytecode `string.dump`** emits the faithful per-version header for 5.1/5.2/5.3
  (previously always the 5.4 header); `load`/`undump` validate each accordingly.
- **Per-version error and traceback wording**: type-error attribution order,
  C-function name resolution (`?` on 5.1, `_G.`-qualified on 5.2), metamethod
  naming, and the numeric-for / arithmetic-on-string messages.
- **GC fidelity**: weak-value clearing ordered before finalizer resurrection,
  white proto-cache drop (C `traverseproto`), suspended-coroutine cycle
  finalization, and collect-time userdata finalizability for 5.1.
- **Lua 5.1 environment model**: per-thread `l_gt` (coroutine `setfenv(0)`),
  per-closure environments for closures without an `_ENV` upvalue, the implicit
  `arg` vararg table, the same-reference metamethod rule, and synthetic
  tail-call debug frames.
- **Number model** (5.1/5.2 float-only): oversized-hex `tonumber`, `next`
  resumption-key normalization, and the `%x`/`%u`/`%o`/`%q`/`%g`/`%s`
  `string.format` conversions.
- **Lexer/parser version gates**: `\u{}` / hex-float / hex-overflow handling,
  empty-statement rejection (5.1), `CALL` line attribution (5.2/5.3), and
  invalid-byte token naming.

### Fixed (also affected the 5.4/5.5 baseline)

- `string.gsub` now accepts a number returned from a function/table replacement.
- `string.format("%g"/"%e")` preserves the sign of negative zero.
- The `stack overflow` runtime error carries its `file:line:` location prefix.
- Arg-errors in the "value expected" path include the function name.

### Harness

- `harness/quick_file.sh` (8s-capped whole-file oracle check), `harness/gen_golden.sh`
  + committed golden vectors, the `dump_kit` / `error_wording_kit` in-process kits,
  and `harness/multiversion_diff_suite.sh` (per-version differential gate).
- Official **5.1** and **5.2.2** test suites vendored under `reference/extra-tests/`;
  `run_official_all.sh` wired for 5.1/5.2.

Methodology and the full per-wave log live in
`specs/followup/MULTIVERSION_COMPAT_AUDIT_2026_06_21.md`.

## [0.2.0] - 2026-06-13

### Fixed

- **GC use-after-sweep on error values escaping into Rust-held errors
  (#189).** A Lua error raised uncaught through `Lua::scope`, `Chunk::eval`,
  `Chunk::exec`, or `Function::call` carries its error *value* (the string from
  `error('boom')`, the table from `error({code=403})`) in the returned error.
  The value's only Lua-stack root was popped when the protected call's frame
  unwound, leaving it referenced solely by the Rust-side error — which the
  collector does not trace — so any collection before the embedder read the
  message swept it (a use-after-sweep, deterministically caught under
  `LUA_RS_GC_QUARANTINE`). The public boundaries now pin the error value in the
  external root set the moment they capture it, and release it when the error is
  dropped. The value is *preserved* as a real Lua value, so re-raising it
  through `pcall`/`xpcall` still returns the original table, exactly as before.

### Changed (breaking)

- **`omnilua::Error` is now a struct, not a type alias for `LuaError`.** It
  wraps the inner `LuaError` plus the GC root anchor described above. It is a
  drop-in replacement for nearly all uses: it `Deref`s to `LuaError` (so
  `message_lossy()`, `to_status()`, `into_value()`, and `Display` forward
  unchanged), implements `From<LuaError>` (so `?` and internally-constructed
  errors keep working), and implements `std::error::Error`. Code that
  pattern-matched a returned error against `LuaError::Runtime(..)` /
  `LuaError::Syntax(..)` should now match on `err.as_lua_error()` (or `kind()`).
  `lua_types::LuaError` remains re-exported as `omnilua::LuaError` and is
  unchanged.

## [0.1.0] - 2026-06-13

### Changed (rebrand to omniLua, 0.1.0)

- **Project renamed `lua-rs` → omniLua.** The wordmark, public docs, and the
  GitHub repository (`ianm199/lua-rs` → `ianm199/omnilua`) move to the new name;
  github.com redirects the old repo URL, but the GitHub Pages site moves to
  `ianm199.github.io/omnilua`.
- **Crate and package renames.** The embedding crate `lua-rs-runtime` →
  `omnilua` (the directory `crates/lua-rs-runtime/` is unchanged), the CLI crate
  `lua-cli` → `omnilua-cli` with the binary `lua-rs` → `omnilua`, and the npm
  package `lua-rs-wasm` → `omnilua`. Internal crates (`lua-vm`, `lua-gc`,
  `lua-types`, `lua-parse`, `lua-stdlib`, …) keep their names — they are
  implementation details, not published surfaces.
- **Version env var.** `OMNILUA_VERSION` is now the canonical way to select a
  Lua version on the CLI; `LUA_RS_VERSION` is still read as a fallback for
  compatibility.
- **Version bumped to 0.1.0** across the workspace for the first release under
  the new name.



- **gc/types** (T1, #113 rung 1): deleted the `UpVal` `RefCell<UpValState>`
  mirror — the Cell-tagged fields are the single source of truth
  (`CLOSED_TAG` sentinel as discriminant). UpVal 64→32 B, GcBox<UpVal>
  104→72 B; closure_ops process RSS −8.3%, heap bytes −7.9% with allocation
  count unchanged (the delta equals 100k upvalues × 32 B exactly).
- **gc** (T3b): lazy weak-token registration — the allocation hot path no
  longer inserts into the weak-handle validation table; tokens are minted at
  `GcRef::downgrade`, the only place they were ever consumed. Ir −2.6 to
  −3.7% on gc_pressure/concat_chain/binarytrees/table_hash_pressure (control
  exactly flat), peak live bytes −12.4%/−10.0% on the live-set rows.
- **bench** (T0): `instr-count.sh --branch-sim` (deterministic Bc/Bcm — the
  CPI arbiter; also corrected the header: the tool is cachegrind, not
  callgrind), `heap-diff.sh` (dhat alloc deltas between two commits),
  agent-safe `profile-hotspots.sh` (detached-watchdog fd fix), bash-3.2
  `set -u` fix. New `docs/MEASUREMENT_PROTOCOL.md` codifies the wall=Ir×CPI
  model and the frozen-baseline/interleave/arbiter discipline.

### Measured, recorded, deliberately not merged (sprint 2)

- **T2 setter family RESOLVED-NEGATIVE**: per-write branch counts (Bcm≈0)
  prove the 2x setter gap is safety/representation tax, not removable logic;
  our no-metatable fast path is already at branch-parity with C.
- **T4 safety-tax ablation** (branch `ablation/unchecked-stack`, never
  merges): removing ALL stack/table bounds checks and RefCell guards =
  5–15.5% of instructions, ~0% of reliable wall (perfectly predicted
  branches), and Ir ratios remain ≥1.9x C — the residual gap is
  representation/idiom. Recorded in `docs/PERFORMANCE_MODEL.md`; the unsafe
  budget stays at zero. `docs/GC_ALLOC_DESIGN_MEMO.md` ranks the remaining
  allocator levers (R2 concat string churn: 13.9M blocks/run, is next).

### Fixed

- **vm** (#139): Lua 5.1 order comparisons on mixed-type operands now raise
  `attempt to compare X with Y` before any metamethod lookup, matching
  reference 5.1.5 — `__lt`/`__le` are consulted only for same-Lua-type
  operands in 5.1 (Int/Float share the number tag); 5.2+ behavior is
  unchanged. One version gate at the cold `call_order_tm` choke point covers
  the register and immediate (LtI/LeI/GtI/GeI) compare paths with zero
  dispatch-loop cost.

### Changed

- **coroutine** (T2-B2): the per-resume panic-hook install/restore dance
  (3–4 heap allocations plus four global hook-lock operations per resume,
  there only to silence `LuaThreadClose` unwinds) is replaced by an
  install-once chaining hook gated on a thread-local suppress counter.
  coroutine_pingpong wall −32% (interleaved A/B min-ratio 0.674, Ir flat —
  the win is removed lock/alloc latency, not instructions); the stock matrix
  row is now 1.35x vs reference where the 2026-06-10 matrix recorded 2.00x
  even with PGO. Embedder note: install custom panic hooks before the first
  resume; later hooks displace the chain (documented on
  `ensure_chaining_panic_hook`).
- **vm** (T2-C2): `CallInfoFrame` flattened from a 2-variant tagged enum to a
  branch-free always-present-fields struct, and the per-frame hook trap moved
  to callstatus bit `CIST_TRAP`, matching C's layout discipline. CallInfo
  stays 72 bytes (now enforced by a compile-time assertion), every accessor
  is a plain field read with a `debug_assert!` frame-kind tripwire, zero new
  unsafe. call_return_shapes −8.6% / method_calls −4.4% wall on interleaved
  A/B with exactly flat Ir (the wall component is layout-entangled on
  macOS/arm64; the structural win is the deleted discriminant branch).
- **coroutine** (T2-B): the four remaining per-resume `Vec` buffers (resume
  args, results, parent open-upvalue slots, cross-thread flush) are pooled on
  `GlobalState` following the snapshot-pool pattern. Wall-neutral on pingpong
  (the dominant snapshot pole was already pooled in `04cd144`); removes
  allocator traffic for arg/result-heavy resume shapes.

### Docs

- `docs/ISSUE_BURNDOWN_SPEC.md` — plan-of-record for the 2026-06-11
  #139/#134/#113 sweep, every verdict evidence-backed: pretailcall
  `clear_stack_range` KEEP (rooting safety; the divergence from C is now
  documented at the callsite), T2-C frame micro-optimizations and T2-D
  `finish_get` diet both RESOLVED-NEGATIVE by the instruction-count arbiter
  (the latter because `GcRef`/`LuaValue` are `Copy` — there was no clone
  overhead to remove), and the `CallInfoFrame` union (64 B CallInfo, unsafe
  budget raise) explicitly NOT escalated. #113 retitled to the RSS
  object-diet backlog with a measured size table.

## [0.0.33] - 2026-06-10

### Fixed

- **gc** (#140): the exact-rooting audit closed a family of latent
  use-after-free bugs where objects the VM still used were swept because the
  root trace did not cover them. Four instances fixed: dead-key tombstones on
  nil'd entries (0.0.32-era, `9c5125c`), debug-API thread borrows silently
  un-rooting coroutines during collection (`debug.traceback(co)` segfaulted
  every release-profile run of db.lua), weak-table pruning skipping manually
  erased entries (freed keys stayed dereferenceable), and the stack tracer
  walking stale slots because C's atomic dead-slice clear was never ported.
  Stack rooting is now C-faithful: trace exactly `[0..top)`, nil the dead
  slice before every collect, `savestate` top fixups at Protect-origin
  checkpoints.

### Changed

- **vm/table** (W2.3 R2): table representation diet — the metatable slot is a
  single `Cell<Option<GcRef>>` (no borrow flag, no cached bool) and
  `TableInner.lastfree` packs to a `u32` sentinel, shrinking the table box
  144 → 128 bytes. binarytrees 0.928 vs pre-change; tracked regressed-minor:
  `table_field_index` 1.017 (RSS 0.856).

### Added

- **gc/harness** (#140): `LUA_RS_GC_QUARANTINE=1` — sweep parks dead objects
  with poisoned headers so any use-after-sweep dereference panics with a
  backtrace in a plain debug build; `harness/asan-stress.sh` — the rooting
  battery (quarantine, stress+quarantine, and ASAN configs) with CI gating on
  the quarantine configs; the official suite now also runs against the
  RELEASE-profile binary in `make test` (optimized cadence is how the
  traceback bug hid from the debug suite); debug builds assert that a
  coroutine mutably borrowed at collect time is covered by a parent snapshot.

## [0.0.32] - 2026-06-09

### Added

- **benchmarks/perf** (#134): added focused coverage for isolated table and
  global setter hot paths: `table_setfield_same`,
  `table_settable_string_key`, `table_seti_same`, and
  `global_settabup_same`.

### Changed

- **vm/table** (#134): accelerated stable table/global setter paths by adding
  existing-slot updates for short-string and integer keys, routing `SETFIELD`,
  string-key `SETTABLE`, `SETI`, and no-metatable `SETTABUP` through those
  paths, caching table metatable presence, and avoiding write-barrier work for
  non-collectable values.

## [0.0.31] - 2026-06-04

### Added

- **benchmarks/perf** (#134): expanded the benchmark matrix with bytecode and
  dispatch telemetry workloads: `numeric_mixed`, `bitwise_mixed`,
  `compare_immediates`, `loop_variants`, `call_return_shapes`, and
  `table_field_index`.

### Changed

- **parser/vm**: emit Lua 5.4 immediate and constant-pool opcode shapes for
  arithmetic, bitwise, shift, and equality expressions where the reference
  compiler uses them, while preserving the correct metamethod fallback opcodes.
- **vm/stdlib**: tighten the generic-for C-iterator path used by `ipairs`,
  avoiding the full generic call slow path when the iterator is already a C
  function and removing repeated positive stack-index resolution in
  `ipairs_aux`.

### Fixed

- **conformance**: the expanded perf packet exposed three version-sensitive
  official-suite edges, now fixed for Lua 5.4 while preserving Lua 5.5 oracle
  behavior: long `__call` chains no longer hit the 5.5-only cap on 5.4,
  stripped bytecode errors report `?:-1:` on 5.4, and high-index method calls
  keep their `method 'name'` attribution on 5.4.

### Docs

- Documented the final performance matrix, profile artifacts, and next packets
  for call frames, iterator dispatch, allocation/GC cadence, and upvalue-heavy
  closure workloads.

## [0.0.25] - 2026-06-01

### Fixed

- **debug** (#92): version-gated line-hook (`debug.sethook(f,"l")`) fidelity.
  Lua 5.5 folds the conditional `TEST`/`JMP` of an `if`/`elseif` onto the
  condition-expression line, so a multi-line `if/<cond>/then` no longer fires a
  separate `then`-line event (5.1–5.4 keep it). On 5.1/5.2/5.3 a numeric `for`
  now fires a line event on every iteration's back-edge — the legacy
  FORPREP-jumps-to-the-bottom-test loop shape — where 5.4/5.5 fire once per
  iteration. Verified byte-for-byte against the Lua 5.3.6 and 5.4.7 references.
- **lexer** (#105): Lua 5.1 quotes the special multi-char tokens (`<eof>`,
  `<name>`, `<number>`, `<string>`) in syntax-error messages
  (`'<name>' expected near '<eof>'`), matching 5.1's unconditional `LUA_QS`
  wrapping; 5.2+ leave them bare.

### Added

- **reference**: pinned upstream Lua 5.3.6 (with the 5.3.4 test bundle) as a
  secondary behavioral oracle for version-gated 5.1/5.2/5.3 work
  (`reference/lua-5.3.6/`, source committed, binaries built locally).

## [0.0.24] - 2026-06-01

### Fixed

- **vm/coroutine** (#97): a `__le` derived from `__lt` (the `LUA_COMPAT_LT_LE`
  fallback on 5.1–5.4) now negates correctly when the `__lt` metamethod yields,
  via the `CIST_LEQ` mark. Previously the comparison returned the inverted
  result across a yield.
- **vm** (#96): closures built in a loop over identical upvalues compare equal
  (`==`) on 5.2/5.3, matching reference closure caching; distinct on 5.1/5.4/5.5.
- **vm** (#94): Lua 5.5 named varargs `function f(...t)` share one storage object
  between `t` and `...`, so mutating `t` is observable through a later `...`
  (count follows `t.n`); preserved across `string.dump`/`load`.
- **parser** (#95): the `break`-outside-loop error message is now version-correct
  (5.1 `no loop to break`, 5.2/5.3 `<break> ... not inside a loop`, 5.4
  `break outside loop at line N`, 5.5 `break outside loop near 'break'`).

## [0.0.23] - 2026-06-01

### Fixed

- **Windows build** (#90): `lua-cli`'s `os.date`/`os.time` local-timezone hook
  used `libc::localtime_r` and read `tm_gmtoff`, neither of which exists in
  Windows' MSVCRT, so `lua-cli` failed to compile on Windows. The
  `local_offset_hook` is now `cfg`-split — the Unix path is unchanged; the
  Windows path derives the same offset (DST included) by decomposing the instant
  with `localtime_s` and `gmtime_s` and differencing the two wall clocks.

### CI

- Releases now gate on a `windows-build` job (`windows-latest`, MSVC) that
  `publish-crates` depends on, so a Windows compile break can no longer ship.

## [0.0.22] - 2026-06-01

### Added — Lua 5.1 and 5.2; one API now spans Lua 5.1–5.5

`lua-rs` now runs **Lua 5.1, 5.2, 5.3, 5.4, and 5.5** from a single embedding
API, selected per instance (`Lua::new_versioned(LuaVersion::V51 …)` /
`LUA_RS_VERSION=5.1…5.5`). Every version is verified against its unmodified
upstream reference binary; all five share one core, and the bytecode dispatch
loop carries no per-version cost (3/4/5 are byte-identical in benchmarks; the
only measurable delta is 5.1/5.2 integer-arithmetic, an inherent consequence of
their float-only number model).

- **5.2 (the bridge)**: float-only numbers on the modern `_ENV` globals model;
  `bit32`, compat-math, `module`; `//`/bitwise/`<const>` rejected; `math.type`/
  `utf8`/`string.pack` absent.
- **5.1 (the legacy family)**: float-only **plus fenv globals** —
  `getfenv`/`setfenv` over a per-closure environment; `__len`-on-tables inert;
  `loadstring`, global `unpack`, `table.getn`/`foreach`, `newproxy`, `gcinfo`,
  1-arg `math.log`; `goto`/`bit32`/`string.pack`/`utf8` absent. C-`rand()` PRNG
  sequence is a documented divergence (the contract — ranges/arg-errors — matches).

5.3 and 5.5 graduate from **alpha to beta**: their long tails are closed
(compat-math, bitwise string coercion, error wording, `global` declarations,
named varargs, `utf8.offset`, `collectgarbage` params, traceback fidelity).

### Fixed

- **Cross-version fidelity** (improves 2–3 versions at once): `_ENV[<relational>]`
  index codegen, arg-error `to '<fn>'` qualifier + offending value + location
  prefix, `print`→global `tostring` (5.1–5.3), `\u{}`/`utf8.char` ceilings,
  `string.unpack`/`format`/`pack` boundaries, `math.random` interval guards.
- **The trailing `[C]: in ?` traceback frame** on uncaught errors, cross-version
  (#79) — official `math.lua` now runs clean.
- **goto label scoping** (block-scoped on 5.2/5.3 vs function-wide on 5.4/5.5).
- **`__gc` finalizer error disposition** (propagate on 5.2/5.3, warn on 5.4/5.5)
  — and the previously-unwired `warn`/`@on`/`@off` subsystem now emits.
- A **panic** in the table downward-resize path (`index out of bounds`) — now
  clamps to the physical array length per upstream `luaH_resize`.

### Changed

- CI: the release perf-dashboard step's `/usr/bin/time` parsing is now portable
  to the Linux runner (#82).

### Docs

- `specs/MULTIVERSION_PLAYBOOK.md` — the reusable "how to add a Lua version"
  methodology (oracle/contract, adversarial-first, the iteration ladder, the
  version seam, the per-phase workflow).

## [0.0.21] - 2026-05-31

### Added — Lua 5.3 fidelity (toward #19)

The clear-cut Lua 5.3 long tail surfaced by the multi-version oracle sweep,
each fix verified against the upstream `lua5.3.6` reference binary and guarded
in `tests/multiversion_oracle.rs` (now 29 cases):

- **`LUA_COMPAT_MATHLIB` roster.** `math.atan2/cosh/sinh/tanh/pow/log10` are now
  present on 5.3 and 5.4, and `math.frexp/ldexp` on 5.3/5.4/5.5 — matching the
  default reference builds. (This also closed a latent 5.4 gap: those six
  functions were previously absent on 5.4, where the reference exposes them.)
- **String→integer coercion in core bitwise ops.** On 5.3, numeric strings
  coerce in `& | ~ << >>` (e.g. `"0xff" & 0xf0` → `240`); 5.4/5.5 keep raising.
  Non-integral numeric strings still report "number has no integer
  representation". This made the official `bitwise.lua` and `constructs.lua`
  byte-identical to the reference.
- **5.3-specific error wording.** Arithmetic on a non-coercible string now
  reports `attempt to perform arithmetic on a <type> value (<varinfo>)` with the
  correct `local`/`global`/`constant` qualifier; a non-number `for` bound reports
  `'for' <what> must be a number`. 5.4/5.5 wording is unchanged.

5.4 and 5.5 behavior is unaffected (`check.sh` 5.4=7/0, 5.5=10/0); the
compat-math roster, bitwise coercion, and error wording are all version-gated.

## [0.0.20] - 2026-05-31

### Fixed — reference-fidelity bugs surfaced by the multi-version oracle

Cross-version bugs found by diffing against the upstream reference binaries
(present before the multi-version work; they affected 5.3/5.4/5.5):

- `math.type` / `math.tointeger` now return `nil` (a `fail`), not `false`, so
  `== nil` guards and truthiness behave as the manual specifies (#76).
- `string.find` with a magic-character pattern and no explicit captures no
  longer returns a spurious trailing empty value — arity matches the reference
  (#77).
- `__le` is derived from `__lt` (`a <= b` ⇒ `not (b < a)`) on 5.1–5.4 (matching
  the default `LUA_COMPAT_LT_LE` reference builds) and raises on 5.5 (#78).
- Error-message fidelity: `bad argument` errors carry the `to '<fn>'` qualifier
  and `got no value` for absent args; length/concat/string-arithmetic errors
  carry the `(command line):N:` location prefix; arithmetic metamethod-failure
  messages report the correct operand types; `table.concat` reports `(table)`
  instead of leaking an internal byte-array (#79).

### Changed

- CI: the release workflow's npm `verify registry install` step now retries with
  backoff so registry propagation lag no longer fails an otherwise-successful
  publish (#80).

## [0.0.19] - 2026-05-31

### Added — multi-version support: Lua 5.3 and 5.5 (alpha)

A single embedding API can now run Lua **5.3** and **5.5** alongside the stable
**5.4**, selected per instance:

```rust
let lua = Lua::new_versioned(LuaVersion::V53); // or V55, or V54 (default)
```

Both share 5.4's mature core (VM, GC, number model, metatables, most of stdlib)
with version-specific behavior confined to a few cold-path seams; the bytecode
dispatch loop carries no per-version cost, so compute-bound code runs
identically across versions. The CLI selects a version with
`LUA_RS_VERSION=5.3|5.4|5.5`.

- **5.5**: contextual `global` (`LUA_COMPAT_GLOBAL`), block-scoped `global`
  declarations with strict undeclared-name checking, `<const>` globals,
  read-only numeric/generic for control variables, stored `global` initializers,
  round-trip float `tostring`, `table.create`.
- **5.3**: `bit32` library, string-in-arithmetic coercion to float, `warn` and
  `coroutine.close` absent, `<const>`/`<close>` attribute syntax rejected.

**Alpha caveat.** 5.3 and 5.5 are preliminary. Their headline features are
verified against the upstream reference binaries (`tests/multiversion_oracle.rs`),
but each has a documented long tail (e.g. 5.3 compat-math and error wording; 5.5
named varargs and the "global already defined" guard) — see `specs/`. Use 5.4
for production and treat 5.3/5.5 as experimental. Lua 5.1/5.2 are not yet
supported and refuse construction rather than masquerade as 5.4.

## [0.0.18] - 2026-05-30

### Added — sandboxing for untrusted Lua

Run untrusted scripts with bounded CPU and memory and no host access. Limits are
enforced on every thread (coroutines included) and are **uncatchable** — a script
cannot escape them with `pcall`/`xpcall`/`coroutine.resume`. A non-sandboxed
runtime pays zero overhead.

- **Rust:** `Lua::sandboxed(SandboxConfig)` returns the runtime plus a `Sandbox`
  handle (`tripped()` / `reset()`). `Lua::install_sandbox` and
  `LuaRuntime::install_sandbox` apply limits to an existing runtime.
- **CLI:** `--sandbox`, `--max-instructions=N`, `--max-memory=N[K|M|G]`.
- **WASM / JS:** `lua_rs_wasm_set_limits` / `lua_rs_wasm_last_trip` /
  `lua_rs_wasm_sandbox_reset`; the `lua-rs-wasm` JS wrapper adds `setLimits`,
  `lastTrip`, and `sandboxReset`.

Three controls: an instruction budget (aborts infinite loops and runaway
recursion), a memory ceiling (refuses oversize allocations before they happen,
plus per-interval sampling), and capability stripping (removes `os.execute`,
`io`, `load`, `require`, `debug`, … from `_G`). Design and threat model:
[docs/SANDBOXING_EXPLORATION.md](docs/SANDBOXING_EXPLORATION.md).

## [0.0.17] - 2026-05-30

### Changed — `#[derive(LuaUserData)]` field exposure (BREAKING)

**Private fields are no longer auto-exposed to Lua. Mark fields `pub` or use
`#[lua(field)]`.**

Rust visibility is now the scriptability boundary for the derive:

- **Public named fields** are auto-exposed to Lua, exactly as before
  (`obj.field` read/write, requiring `Clone + IntoLua + FromLua` on the field
  type).
- **Private named fields** are now opaque — invisible to Lua. Previously
  *every* named field was exposed regardless of visibility, which forced
  `Clone` (and Lua-marshaling) onto fields that were only ever meant to be
  internal. To keep a private field scriptable, either make it `pub` or
  annotate it `#[lua(field)]`.
- **Tuple/newtype and unit structs** (e.g. `struct Handle(App);`) now derive
  successfully and become **opaque userdata handles** — no field access, but
  full support for `#[lua(methods)]` and metamethods. Previously the derive
  rejected them with a compile error.

This makes both data-record structs and opaque engine/resource-handle structs
work without extra boilerplate, and lets a struct hold a non-`Clone` private
field (e.g. `app: bevy::App`) and still derive cleanly.

Closes [#56](https://github.com/ianm199/lua-rs/issues/56) and
[#57](https://github.com/ianm199/lua-rs/issues/57).

#### Migration

```rust
// Before: `x` and `y` were exposed to Lua because every field was.
#[derive(LuaUserData)]
struct Point { x: f64, y: f64 }

// After: mark the fields you want scriptable `pub`.
#[derive(LuaUserData)]
struct Point { pub x: f64, pub y: f64 }

// ...or force-expose a specific private field with `#[lua(field)]`:
#[derive(LuaUserData)]
struct Point {
    #[lua(field)]
    x: f64,
    pub y: f64,
}
```

If a previously-exposed field silently becomes `nil` in your Lua code after
upgrading, this is the cause: add `pub` or `#[lua(field)]` to that field.

### Added

- `#[lua(field)]` field attribute on `#[derive(LuaUserData)]` to force-expose a
  private field (escape hatch for the visibility change above).
- `#[derive(LuaUserData)]` support for tuple/newtype and unit structs as opaque
  userdata handles.
- Behavioral-parity oracle (`make parity` / `harness/parity_check.sh`): a golden
  diff of normalized stdout + exit code against reference C Lua 5.4.7, distinct
  from the existing no-crash gate. ([#60](https://github.com/ianm199/lua-rs/pull/60))

### Fixed

- `os.date` / `os.time` local-time handling and close-time (`<close>`
  to-be-closed variable) finalizers — two behavioral divergences from C Lua 5.4
  surfaced by the new parity oracle. Official-test conformance 24 → 27/33.
  ([#60](https://github.com/ianm199/lua-rs/pull/60))

### Performance

- GC pacer now charges table array/hash backing buffers, not just the `GcBox`
  header, so the collector's byte budget reflects real allocation.
  ([#58](https://github.com/ianm199/lua-rs/pull/58))
- Removed a redundant duplicate short-string intern table; short strings were
  interned twice and one copy was never read (−56% RSS, −54% wall on the
  `table_hash_pressure` benchmark). ([#62](https://github.com/ianm199/lua-rs/pull/62))
