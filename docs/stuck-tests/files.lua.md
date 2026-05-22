# Stuck: `reference/lua-c/testes/files.lua`

Stuck-skipped after Sonnet then Opus both ran. Most recent transcript: `harness/impl/mega-O2-D4-reference_lua_c_testes_files_l.transcript.jsonl` (~550 KB).

## Current failure

```
testing i/o
lua: pcall_k failed: Runtime: TODO(port): borrow split needed for f_seek
```

That's not a runtime bug — it's an unimplemented stub. `f_seek` (and friends) are intentionally returning a TODO string.

## What's actually wrong

`crates/lua-stdlib/src/io_lib.rs` has **five** file/IO entry points that all bail with the same `borrow split needed for ...` runtime error:

| Line | Function | Lua-facing |
|---|---|---|
| 1011 | `io_read` | `io.read(...)` |
| 1023 | `f_read` | `file:read(...)` |
| 1157 | `io_write` | `io.write(...)` (the cherry-pick wired this — verify) |
| 1193 | `f_seek` | `file:seek(...)` ← current blocker |
| 1215 | `f_setvbuf` | `file:setvbuf(...)` |

The TODO comment above each one (e.g. io_lib.rs:1188-1190) explains it:

> borrow split — cannot call seek on extracted `&mut dyn LuaFileOps` while state is also borrowed. Phase B fix: RefCell or StackIdx API.

Concrete shape of the problem: the file handle is stored in userdata behind `&mut LuaState`. To call any method on it (seek/read/write) you need `&mut self`-on-the-handle and `&mut state`-for-stack-manipulation simultaneously. Rust's borrow checker says no. The cherry-pick fix (`42ff10f`) **solved this for `io.open` / `io.output` / `io.write`** by installing `file_open_hook` / `file_remove_hook` on `GlobalState` — those don't need stack access during the call. Read/seek/setvbuf do need stack access (they push results), so the same trick doesn't apply.

## What past agents have tried

Only one agent commit ever landed for files.lua: `229c5b2 agent debug: reference/lua-c/testes/files.lua`. The freshest O2 opus run did NOT commit f_seek — it spent its budget implementing `load_filex` (loadfile/dofile) in `auxlib.rs`. Quoting its summary:

> **Root cause.** `crates/lua-stdlib/src/auxlib.rs:1050` — `load_filex` was a Phase-A stub. C-Lua's `luaL_loadfilex` returns a status code with an error string left on the stack on file errors; returning `Err` instead bubbled past `load_aux` so `loadfile(removed_file)` raised instead of returning `(nil, errmsg)`.
>
> **Fix.** Implemented `load_filex` for real: slurp via `std::fs::read`, strip BOM and shebang, feed bytes into `lua_vm::api::load` with chunkname `@<filename>`. On open failure pushes `"cannot open <filename>: <io-err>"` and returns `LUA_ERRFILE`.

That fix is **correct and useful** for `loadfile`, but `files.lua` testing I/O early triggers `f_seek` before reaching the `loadfile` tests. The agent's work didn't advance the test because the new failure is upstream of its fix.

## Suggested next move

Two real options:

1. **Wrap file handles in `RefCell`.** Change `LuaUserData` storage so a `LuaFileHandle` lives inside a `RefCell<Box<dyn LuaFileHandle>>`. Then `f_seek` can `try_borrow_mut()` independently of the `&mut LuaState` borrow. Mechanically straightforward; touches every file op site. ★ recommended.
2. **StackIdx-based API.** Add `LuaState::call_on_file_at(idx, fn)` that does the borrow split internally — extract file from registry, perform op, push result. Cleaner long-term but bigger surface change.

Either way, this is a focused, well-scoped piece of work (maybe 200 lines + tests). It's a great candidate for a single targeted Opus run with an explicit prompt: *"Replace TODO-borrow-split stubs in io_lib.rs by wrapping file handles in RefCell. Don't change loadfile/dofile."*

## Files most touched

`crates/lua-stdlib/src/io_lib.rs`, `crates/lua-stdlib/src/auxlib.rs`, `crates/lua-types/src/filehandle.rs` (new in cherry-pick), `crates/lua-cli/src/main.rs` (new in cherry-pick).
