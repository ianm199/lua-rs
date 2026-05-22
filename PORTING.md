# Lua C → Safe Rust Porting Guide

You are translating one Lua C file to Rust. Read this whole document before
writing any code. The goal of **Phase A** is a draft `.rs` next to the `.c`
that captures the logic — it does **not** need to compile. **Phase B** makes
it compile crate-by-crate. **Phase C+** makes the corresponding tests pass.

If you are an agent: every rule below is binding. **Flag and TODO over guess.**
Hooks enforce the hard rules; you will be stopped if you violate them.

## 1. Ground rules

- **File location.** For a C file `src/lparser.c`, the Rust port lives at
  `crates/lua-parse/src/parser.rs`. Crate assignment is in
  `ANALYSES/file_deps.txt`. `.h` files merge into the `.rs` that uses them —
  do not produce `.rs` mirrors of headers.
- **No `tokio`, `rayon`, `async-trait`, `futures`, `hyper`.** No `std::fs`,
  `std::net`, `std::process`. No `async fn`. Lua owns its own control flow.
- **No `String`, `&str`, `from_utf8*`, `.to_string()` for Lua string data.**
  Lua strings are byte-strings. Use `&[u8]`, `Vec<u8>`, `Box<[u8]>`, or our
  `LuaString` newtype. The only legitimate `&str` is for literal Rust-side
  identifiers (crate names, error tag names).
- **No raw pointers (`*const T`, `*mut T`) outside `crates/lua-gc/` and
  `crates/lua-coro/`.** A pointer-into-the-stack pattern in C becomes a
  `StackIdx` (newtype around `u32`) in Rust. No borrows held across stack-
  mutating operations.
- **No `unsafe` outside `lua-gc` and `lua-coro`.** Ceiling is in
  `harness/unsafe-budgets.toml`. If you genuinely need it, leave
  `// TODO(port): unsafe needed because <X>` and stop — do not write the
  `unsafe` block. A human raises the ceiling.
- **`anyhow`, `Box<dyn Error>`, `String` error messages — banned for Lua errors.**
  Use `LuaError`. See §6.
- **Match the C file's logical structure, not its line-by-line shape.** We
  are not Bun. Renaming for idiom, regrouping, and splitting are encouraged
  where they clarify intent. Reviewers diff *behavior* (via the oracle), not
  diff text.
- **Don't guess. Flag.** Use `TODO(port)`, `PORT NOTE`, `PERF(port)`. See §11.
- **Output trailer required.** Every `.rs` you produce ends with a `PORT STATUS`
  block. See §12. A missing trailer fails the `trailer-required.sh` hook.

## 2. The eight load-bearing design decisions

These were locked in `PORT_STRATEGY.md §3`. Restated here as rules you cannot
deviate from without escalation:

1. **Rust-native API.** No C-API parity. The user-facing type is `LuaState`
   with methods, not `lua_State` with free functions.
2. **`TValue` is a Rust enum** named `LuaValue`. Not a tagged C struct.
3. **Strings are interned byte-strings**, stored in `StringPool` on
   `GlobalState`. Use `LuaString` (newtype wrapping `Rc<[u8]>` or pool key).
4. **GC: leak in Phases A–C, port real incremental GC in Phase D.** In
   Phases A–C, `GcRef<T>` is implemented as `Rc<T>` and we accept cycles
   will leak. Phase D replaces this.
5. **Stack pointers → `StackIdx` indices.** Never hold a borrow across a
   stack-mutating call. The stack is `Vec<StackValue>` and reallocates.
6. **Coroutines stubbed in A–D.** `coroutine.create` panics with a clear
   message. Phase E adds stackful coroutines via `corosensei`.
7. **Errors are `Result<T, LuaError>`.** Every fallible internal fn returns
   it. No `unwrap()` outside test code and `main()`. See §6.
8. **Upvalues are `enum UpVal { Open { thread, idx }, Closed(LuaValue) }`.**
   Not `Rc<RefCell<LuaValue>>` for everything. Open upvalues stay on the
   stack until close.

## 3. Type map

### 3.1 Primitive C types

| C | Rust | Notes |
|---|---|---|
| `int` | `i32` | unless context demands otherwise |
| `unsigned int` | `u32` | |
| `lua_Integer` | `i64` | per `luaconf.h` default |
| `lua_Number` | `f64` | per `luaconf.h` default |
| `size_t` | `usize` | |
| `ptrdiff_t` | `isize` | |
| `char` | `u8` | Lua strings are bytes, not chars |
| `lu_byte` | `u8` | |
| `lu_mem` / `l_mem` | `usize` / `isize` | Lua's mem-counter types |
| `lua_State *` | `&mut LuaState` (param), `&LuaState` (immut), `GcRef<LuaState>` (thread value) | never raw |
| `global_State *` | accessed via `state.global()` | not a separate type at the API |
| `void *` (light userdata) | `*mut c_void` | one of the rare allowed raw-ptr cases; documented in `LuaValue::LightUserData` |
| `const char *` (C string) | `&CStr` if NUL-terminated; `&[u8]` otherwise | |

### 3.2 Lua value types

| C | Rust | Notes |
|---|---|---|
| `TValue` | `LuaValue` (enum) | see PORT_STRATEGY §3.2 for variants |
| `Value` (union) | not directly exposed | enum payload handles it |
| `StkId` (`StackValue *`) | `StackIdx` (`u32` newtype) | **never** a borrow |
| `TString *` | `GcRef<LuaString>` | byte-string, interned if short |
| `Table *` | `GcRef<LuaTable>` | hybrid array+hash internally |
| `Proto *` | `GcRef<LuaProto>` | function prototype |
| `Closure *` (any) | `GcRef<LuaClosure>` | enum variants for Lua/CCl/LightC |
| `UpVal *` | `GcRef<UpVal>` where `UpVal` is the enum from §2 #8 | |
| `Udata *` | `GcRef<LuaUserData>` | |
| `CallInfo *` | `CallInfoIdx` (`u32` newtype) | indices into `LuaState.call_stack: Vec<CallInfo>` |
| `GCObject *` | `GcRef<dyn Collectable>` (Phase A: `Rc<dyn Collectable>`) | |

### 3.3 Lua macros — `ttis*` family

In C, type checks are macros (`ttisnil(o)`, `ttisstring(o)`, etc.) over the
tag byte. In Rust, they become enum match patterns or `matches!`.

| C | Rust |
|---|---|
| `ttisnil(o)` | `matches!(o, LuaValue::Nil)` |
| `ttisnumber(o)` | `matches!(o, LuaValue::Int(_) \| LuaValue::Float(_))` |
| `ttisinteger(o)` | `matches!(o, LuaValue::Int(_))` |
| `ttisfloat(o)` | `matches!(o, LuaValue::Float(_))` |
| `ttisstring(o)` | `matches!(o, LuaValue::Str(_))` |
| `ttistable(o)` | `matches!(o, LuaValue::Table(_))` |
| `ttisfunction(o)` | `matches!(o, LuaValue::Function(_))` |
| `ivalue(o)` | `o.as_int().expect("not int")` — but prefer `if let LuaValue::Int(i) = o` |
| `fltvalue(o)` | `o.as_float().expect("not float")` |
| `tsvalue(o)` | `o.as_string().expect("not string")` returning `&GcRef<LuaString>` |
| `setnilvalue(o)` | `*o = LuaValue::Nil` |
| `setivalue(o, x)` | `*o = LuaValue::Int(x)` |

Full list in `ANALYSES/macros.tsv`.

## 4. C-pattern → Rust-pattern table

### 4.1 Function signatures

```c
// C: takes lua_State *L, returns int (count of values pushed)
static int luaB_print (lua_State *L);
```
```rust
// Rust: method on LuaState, returns Result<usize, LuaError>
fn print(state: &mut LuaState) -> Result<usize, LuaError>;
```

| C pattern | Rust pattern |
|---|---|
| `static int foo(lua_State *L, ...)` | `fn foo(state: &mut LuaState, ...) -> Result<usize, LuaError>` |
| `static void foo(lua_State *L, ...)` | `fn foo(state: &mut LuaState, ...) -> Result<(), LuaError>` if can error; else `fn foo(state: &mut LuaState, ...)` |
| `LUAI_FUNC void foo(...)` | `pub(crate) fn foo(...)` |
| `LUA_API int foo(...)` | `pub fn foo(...)` |
| `static inline ... foo(...)` | `#[inline] fn foo(...)` |

### 4.2 Error handling

```c
// C
luaG_runerror(L, "bad argument %d to '%s'", i, fname);
```
```rust
// Rust
return Err(LuaError::runtime(format_args!(
    "bad argument {} to '{}'", i, fname
)));
```

`format_args!` deferred — no allocation unless the error is realized. See
`ANALYSES/error_sites.tsv` for the mapping of every error site.

| C | Rust |
|---|---|
| `setjmp`/`longjmp` jump tables | gone. `?` operator handles propagation. |
| `luaD_throw(L, status)` | `return Err(LuaError::with_status(status))` |
| `luaG_runerror(L, fmt, ...)` | `return Err(LuaError::runtime(format_args!(fmt, ...)))` |
| `luaG_typeerror(L, o, "op")` | `return Err(LuaError::type_error(o, "op"))` |
| `lua_error(L)` | `return Err(LuaError::from_value(state.pop()))` |
| `lua_pcall` / `lua_pcallk` | `state.protected_call(...)` returning `Result<...>` |
| `lua_assert(x)` | `debug_assert!(x)` |
| `api_check(L, x, "msg")` | `debug_assert!(x, "msg")` |

### 4.3 Stack operations

The **`StackIdx`** rule (§2 #5) is the most-violated one. Watch carefully.

```c
// C: holds a pointer into the stack across a push (legal in C)
StkId o = L->top - 1;
luaO_pushfstring(L, "...", ...);  // may grow stack
setobj(L, o, &something);  // o may be dangling!
```

The C code is technically a bug too (it sometimes is), but Lua's discipline
catches most of these. **In Rust this pattern is impossible** because
`StackIdx` is a `u32`, not a borrow.

```rust
// Rust
let o = state.top_idx() - 1;
state.push_fstring(format_args!("..."))?;  // may grow stack
state.set_at(o, something);  // o is still valid as an index
```

| C | Rust |
|---|---|
| `L->top` | `state.top_idx()` / `state.set_top(...)` |
| `*L->top++` (push) | `state.push(value)` |
| `L->top--` (pop) | `state.pop()` returning `LuaValue` |
| `setobjs2s(L, o1, o2)` | `state.set_at(o1, state.get_at(o2).clone())` |
| `lua_pushinteger(L, x)` | `state.push(LuaValue::Int(x))` |
| `lua_pushlstring(L, s, n)` | `state.push_string(s)` |
| `lua_pushnil(L)` | `state.push(LuaValue::Nil)` |
| `api_incr_top(L)` | gone — `state.push()` already increments |

### 4.4 Table operations

| C | Rust |
|---|---|
| `luaH_new(L)` | `state.new_table()` returning `GcRef<LuaTable>` |
| `luaH_get(t, k)` | `t.get(k)` returning `LuaValue` (or `Nil` if absent) |
| `luaH_set(L, t, k)` | `t.set(state, k, v)?` |
| `luaH_resize(L, t, na, nh)` | `t.resize(state, na, nh)?` |
| `luaH_next(L, t, key)` | `t.next(key)` returning `Option<(LuaValue, LuaValue)>` |
| array part access (`t->array[i]`) | `t.array_get(i)` / `t.array_set(i, v)` |

### 4.5 GC and refcounting

In Phase A–C, `GcRef<T>` is `Rc<T>`. Phase D replaces with real GC. Either
way, the agent writes `GcRef<T>` and uses these idioms:

| C | Rust |
|---|---|
| `luaC_objbarrier(L, o, v)` | `state.gc().barrier(o, &v)` — no-op in Phases A–C |
| `luaC_step(L)` | `state.gc().step()` — no-op in Phases A–C |
| `luaC_fullgc(L, isemergency)` | `state.gc().full_collect()` |
| `setobj` (assigning a TValue) | `*dst = src.clone()` — `LuaValue: Clone` cheap for non-GC variants |

### 4.6 String operations

| C | Rust |
|---|---|
| `luaS_new(L, s)` | `state.intern_str(s)` — `s: &[u8]` |
| `luaS_newlstr(L, s, n)` | `state.intern_str(&s[..n])` |
| `getstr(ts)` | `ts.as_bytes()` returning `&[u8]` |
| `tsslen(ts)` | `ts.len()` |
| `luaS_eqlngstr(a, b)` | `a == b` (uses `PartialEq for LuaString`) |
| `luaS_hash(s, l, seed)` | `LuaString::hash_bytes(s, seed)` |

### 4.7 Bit operations

```c
i & (sz - 1)  // assumes sz is power of two
```
```rust
i & (sz - 1)  // same; document the invariant
// or: i % sz   if sz is not power-of-two
```

Lua uses lots of power-of-two table-size bit tricks. Keep them; document the
invariant with a one-line comment.

### 4.8 Switch / dispatch

Lua's VM is a giant `switch` over opcodes. In Rust:

```rust
match opcode {
    OpCode::Move => { /* ... */ }
    OpCode::LoadK => { /* ... */ }
    // ...
}
```

Use `match` over `OpCode` enum. **Do not** use computed-goto (the `ljumptab.h`
pattern). Modern compilers do this for us; manual computed-goto in Rust is
nightly-only and not worth the unsafety.

### 4.9 Bitfield-packed structs

Lua uses some bit-packed structs for compactness (e.g. flags bytes in
`Table.flags`, the `tt_` byte with type + variant + collectable bit).

| C pattern | Rust pattern |
|---|---|
| `lu_byte tt;` with bit fields | `Tag(u8)` newtype with `const fn` accessors |
| `t->flags & BIT_X` | `t.flags().has_x()` |
| `t->flags \|= BIT_X` | `t.flags_mut().set_x(true)` |

## 5. Banned patterns

A non-exhaustive list. The `forbidden-import.sh` hook enforces these.

```rust
// BANNED
use std::string::String;        // not for Lua data
fn foo(s: &str) { ... }         // not for Lua data
let s = String::from_utf8(bytes).unwrap();  // never
let s = format!("{}", ...);     // not for Lua errors; use LuaError::runtime
use tokio::*;
async fn ...;
use std::fs;
use std::net;
use std::process::Command;      // only allowed in lua-cli
unsafe { ... }                  // outside lua-gc/lua-coro
```

```rust
// ALLOWED but DISCOURAGED
.unwrap()                       // OK in tests and main(); flag elsewhere
.expect("msg")                  // same
panic!(...)                     // same; LuaError::Runtime preferred
```

## 6. Error handling — full rules

```rust
#[derive(Debug, Clone)]
pub enum LuaError {
    Runtime(LuaValue),       // arbitrary value, usually a string — matches C-Lua
    Syntax(LuaValue),        // parser errors
    Memory,                  // OOM
    Error,                   // error in error handling
    Yield,                   // not really an error; control flow
    File,                    // file I/O
    Gc,                      // GC error
}
```

- Every internal fallible fn returns `Result<T, LuaError>`.
- The error value is a `LuaValue` because Lua errors can be any value, not just strings. Most are strings.
- Never use `anyhow`, `thiserror::Error` derive with `String` payloads, or `Box<dyn Error>` for Lua errors. Lua errors must be a `LuaValue` payload to round-trip through `pcall`.

### 6.1 Canonical `LuaError` constructors

These are the only constructors the Translator should emit. Each builds the standard C-Lua error message verbatim so test snapshots match. See `ANALYSES/error_sites.tsv` for the full call-site mapping. All take `format_args!`-style lazy arguments where applicable — no allocation until the error is realized.

| Constructor | Message shape | Used at |
|---|---|---|
| `LuaError::runtime(args)` | `Runtime(LuaValue::String(...))` | generic `luaG_runerror` |
| `LuaError::syntax(args)` | `Syntax(LuaValue::String(...))` | parser errors |
| `LuaError::syntax_at(args, source, line)` | parser error with explicit location | `luaX_syntaxerror` / `luaX_lexerror` |
| `LuaError::type_error(v, op)` | `"attempt to <op> a <type> value"` | `luaG_typeerror` |
| `LuaError::call_error(v)` | `"attempt to call a <type> value"` | `luaG_callerror` |
| `LuaError::concat_error(p1, p2)` | `"attempt to concatenate a <type> value"` | `luaG_concaterror` |
| `LuaError::arith_error(p1, p2, msg)` | `"attempt to perform arithmetic on a <type> value"` | `luaG_opinterror` |
| `LuaError::int_overflow(p1, p2)` | `"number has no integer representation"` | `luaG_tointerror` |
| `LuaError::order_error(p1, p2)` | `"attempt to compare two <t> values"` / `"compare <t1> with <t2>"` | `luaG_ordererror` |
| `LuaError::for_error(v, what)` | `"bad 'for' <what> (number expected, got <type>)"` | `luaG_forerror` |
| `LuaError::arg_error(narg, msg)` | `"bad argument #N to '<fname>' (<msg>)"` | `luaL_argerror`, `luaL_argcheck` |
| `LuaError::type_arg_error(narg, expected, got)` | `"<expected> expected, got <type>"` | `luaL_typeerror`, `luaL_check*` |
| `LuaError::from_value(v)` | `Runtime(v)` — caller-supplied value | `lua_error`; special-case "not enough memory" → `Memory` |
| `LuaError::from_top(state)` | `Runtime(state.pop())` | `luaG_errormsg` |
| `LuaError::with_status(status)` | variant chosen by status code | direct `luaD_throw` ports |

These constructors live in `crates/lua-types/src/error.rs` (currently a stub — they land as Phase A's first deliberate write to that crate). All accept `format_args!` so they're zero-alloc on the happy path.

## 7. Naming and module layout

- Drop the `lua` / `lua_` / `luaB_` / `luaH_` / `luaS_` / `luaO_` etc.
  prefixes. The crate namespace replaces them.
  - `luaH_new` → `lua_vm::table::Table::new`
  - `luaS_newlstr` → `lua_vm::string::intern`
  - `luaB_print` → `lua_stdlib::base::print`
- Functions stay `snake_case`. Acronyms collapse: `lua_toJS` style is not
  a thing here, but: `toString` → `to_string` (only for non-Lua-data; Lua
  string conversion is `to_lua_string`).
- One C file becomes one or two `.rs` files in the appropriate crate.
  Headers (`*.h`) merge into the consuming `.rs`. See `ANALYSES/file_deps.txt`
  for the canonical assignment.

## 8. Lifetimes and ownership

Phase A discipline: when in doubt, prefer ownership transfer (`T`) over
borrowing (`&T` / `&mut T`). The borrow checker has a clear opinion about
what's safe; the cost of slightly-more-clones in Phase A is trivial vs. the
cost of restructuring later.

- `&LuaState` for read-only operations that don't push.
- `&mut LuaState` for anything that pushes, pops, or calls.
- **No `&LuaValue` across a stack-mutating call.** Clone or copy the value
  first. `LuaValue` is `Clone` and cheap for primitives.
- No struct with a `'a` lifetime parameter in Phase A unless you can defend
  it with a one-liner. Heap-allocate (`Box`, `Rc`) instead. Phase B can
  tighten lifetimes if profiling says we need to.

## 9. Macro translation

Lua headers are 50%+ macros. Translate the *call site*, not the macro
definition. ANALYSES/macros.tsv has the canonical mapping; below is the
shape.

| C macro form | Rust |
|---|---|
| Predicates: `ttisnil`, `iscollectable` | `matches!` or method |
| Accessors: `tsvalue(o)`, `ivalue(o)` | enum-match-or-method |
| Setters: `setivalue(o, x)` | direct assignment to enum variant |
| Casts: `cast(int, x)`, `cast_byte(x)` | `x as i32`, `x as u8` (or `try_from` for narrowing) |
| Bit ops: `lmod(s, size)` | `(s & (size - 1)) as usize` with invariant comment |
| Assertions: `lua_assert(x)`, `api_check(L, x, "msg")` | `debug_assert!(x)`, `debug_assert!(x, "msg")` |

If a macro has no clear equivalent, leave `// TODO(port): macro <name>` and
move on.

## 10. Lua's internal C-API patterns

A few patterns appear so often they get their own translation:

```c
// C: push a value onto the stack, then call a function on it
lua_pushliteral(L, "key");
lua_gettable(L, -2);
lua_call(L, 0, 1);
```

You will *not* see this in the Rust internals — we don't have a public C
API. Equivalent operations are direct method calls:

```rust
let v = state.get_field(table, b"key")?;
state.call(v, &[], 1)?;
```

If you encounter a `.c` file that's heavy on this style (i.e. `lbaselib.c`,
the standard library functions written against the C API), it gets a
special translation pattern: treat `lua_get*`/`lua_to*`/`lua_push*` calls as
operating on `state` and producing intermediate `LuaValue`s. The
`ANALYSES/c_apis.tsv` (built lazily as we hit these files) carries the
explicit mapping.

## 11. Flagging conventions

| Prefix | Meaning | Routes to |
|---|---|---|
| `// TODO(port): <reason>` | Unconfident translation, needs revisit | Phase B / human review |
| `// PORT NOTE: <note>` | Intentional non-faithful restructuring | Diff-time clarification |
| `// PERF(port): <c-idiom> — profile in Phase B` | Naive-idiom translation of perf-sensitive C; benchmark later | Phase B perf pass |
| `// SAFETY: <invariant>` | Required on every `unsafe` block (only in `lua-gc`/`lua-coro`) | Reviewer audit |

**The hardest discipline:** when faced with a translation you're unsure
about, **emit `TODO(port)` and stop**. Do not invent. Do not reach for
`unsafe`. Do not write `unwrap()` to silence a `Result`. Flagging is
infinitely better than wrong code.

## 12. Output format — PORT STATUS trailer

Every `.rs` file produced by the Translator role ends with:

```rust
// ──────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        src/<file>.c  (NNN lines, M functions)
//   target_crate:  lua-<crate>
//   confidence:    high | medium | low
//   todos:         N
//   port_notes:    M
//   unsafe_blocks: 0   (must be 0 outside lua-gc/lua-coro)
//   notes:         <one-line summary for Phase B>
// ──────────────────────────────────────────────────────────────────────────
```

- `confidence: low` = "logic probably wrong; re-read the C in Phase B."
- `confidence: medium` = "types/imports need fixing; logic should hold."
- `confidence: high` = "should compile with mechanical import fixes."
- `todos: N` must match the count of `TODO(port)` comments in the file.

The `trailer-required.sh` hook fails the agent if the trailer is missing or
malformed.

## 13. Don't translate

- Generated files like `ljumptab.h` (computed-goto dispatch table) — Rust's
  `match` compiles to the same thing; drop entirely.
- `ltests.c` — internal test hooks, not in scope (see PORT_STRATEGY §2).
- `#include` lines — `use` statements live at the top of the Rust file,
  driven by the crate map.
- `LUAI_DDEC` / `LUAI_DDEF` declaration macros — these are visibility
  controls; the Rust `pub`/`pub(crate)` system supersedes them.
- Compatibility shims for `LUA_COMPAT_5_3` etc. — we target 5.4 only.

## 14. Concrete checklist for a Translator task

1. Read this PORTING.md in full (it's prompt-cached).
2. Read the C file you've been assigned.
3. Look up cross-references in `ANALYSES/macros.tsv`, `ANALYSES/types.tsv`,
   `ANALYSES/error_sites.tsv`.
4. Identify the target crate from `ANALYSES/file_deps.txt`.
5. Produce the `.rs` file with the appropriate translation per rules above.
6. Emit a PORT STATUS trailer.
7. Commit. The `commit-on-stop.sh` hook does this automatically if you exit
   without committing.

If at any point you're unsure: **TODO(port) and stop.**
