# `#[derive(LuaUserData)]` field-exposure model: design background and decision

Design doc for a feature decision on **ianm199/lua-rs**. Written so a reader
who doesn't know the project, the players, or the prior discussion can weigh
in. Everything claimed about other codebases is quoted from source, with
links. Companion to [`SCOPE_NONSTATIC_DECISION.md`](SCOPE_NONSTATIC_DECISION.md),
which covers a *different* request from the same user; read that first if you
want the full cast and the strategic stakes.

---

## 1. The cast (short version)

Full version in `SCOPE_NONSTATIC_DECISION.md` §1. Briefly:

- **ianm199/lua-rs** — the project this doc is about. Pure-Rust Lua 5.4, no
  C bindings. Currently v0.0.9 (pre-1.0). https://github.com/ianm199/lua-rs.
- **CppCXY/lua-rs** — a *different*, unrelated pure-Rust Lua implementation
  (often called `luars` in its own docs). The direct competitor for the same
  niche. https://github.com/CppCXY/lua-rs. **This is the reference
  implementation Shatur is comparing us against in both issues below.**
- **mlua** — the long-standing Rust binding to *C* Lua. The ecosystem's
  shape-setter for the embedding API, but not pure-Rust.
  https://github.com/mlua-rs/mlua.
- **Shatur** — long-time Bevy ecosystem author (`bevy_replicon` ~609★,
  `bevy_enhanced_input` ~274★, `simgine` ~114★). Exploring a generic
  Lua-scripting-for-Bevy crate. He is a high-value adoption signal: winning
  him pulls in ~1,000 combined stars' worth of ecosystem gravity. He is
  currently on mlua and wants to move to a pure-Rust option. https://github.com/Shatur.

The strategic frame (from the companion doc): the pitch is "pure-Rust mlua."
Every place a real user hits friction that mlua or CppCXY don't have is a
place that pitch grows an asterisk. Both issues below are exactly that:
friction we have and CppCXY doesn't.

---

## 2. The two issues

Both filed by Shatur against ianm199/lua-rs on 2026-05-30, zero comments,
still open.

### Issue #56 — "`LuaUserData` derive requires `Clone`"

https://github.com/ianm199/lua-rs/issues/56

> It's problematic because it prevents me from creating an abstraction over
> `App`:
>
> ```rust
>     #[derive(LuaUserData, Clone)]
>     #[repr(transparent)]
>     struct ScriptApp {
>         app: App, // `App` is not clonable.
>     }
> ```
>
> CppCXY/lua-rs don't have this requirement.

### Issue #57 — "Structs without named fields"

https://github.com/ianm199/lua-rs/issues/57

> It would be nice to support this since it's common to write something like
> this:
>
> ```rust
>     #[derive(LuaUserData)]
>     #[repr(transparent)]
>     struct ScriptApp(App);
> ```
>
> CppCXY/lua-rs supports this.

`App` here is `bevy::App` — a large, non-`Clone` engine object. `ScriptApp`
is a newtype wrapper Shatur wants to expose to Lua as an opaque handle with
methods (`app:add_system(...)` etc.), **not** as a data record whose fields
Lua reads and writes.

---

## 3. Why these are one request, not two

Both issues are Shatur trying to do the same thing — wrap an opaque,
non-`Clone` Rust type as Lua userdata and hang methods off it — from two
syntactic directions:

- #56 is the **named-field** spelling: `struct ScriptApp { app: App }`.
- #57 is the **newtype/tuple** spelling: `struct ScriptApp(App)`.

In neither case does he want `app` mirrored into Lua as a readable/writable
value. The derive's insistence on doing that (or on rejecting the shape
outright) is the whole problem. Any fix should solve both coherently — they
are the same wall hit from two angles.

`#[repr(transparent)]` in both examples is a memory-layout attribute,
irrelevant to the derive's logic. The derive should neither require nor react
to it. Mentioned only because it appears in his code.

---

## 4. How ianm199/lua-rs's derive works today

Source: `crates/lua-rs-derive/src/lib.rs`. `#[derive(LuaUserData)]` generates
`impl UserData for T`. The relevant behavior:

**It auto-exposes every named field** (regardless of visibility), unless the
field carries `#[lua(skip)]`. For each exposed field it emits a getter that
**clones**:

```rust
// crates/lua-rs-derive/src/lib.rs:161
__m.add_field_method_get(#lua_name, |_, __this| {
    ::core::result::Result::Ok(::core::clone::Clone::clone(&__this.#ident))
});
```

and (unless `#[lua(readonly)]`) a setter that assigns by value:

```rust
// crates/lua-rs-derive/src/lib.rs:167
__m.add_field_method_set(#lua_name, |_, __this, __value: #ty| {
    __this.#ident = __value;
    ::core::result::Result::Ok(())
});
```

The runtime methods these call have these bounds (`crates/lua-rs-runtime/src/lib.rs:2057`):

```rust
fn add_field_method_get<R, F>(&mut self, name: &str, getter: F)
where R: IntoLuaMulti + 'static, F: Fn(&Lua, &T) -> Result<R> + 'static;

fn add_field_method_set<A, F>(&mut self, name: &str, setter: F)
where A: FromLuaMulti + 'static, F: Fn(&Lua, &mut T, A) -> Result<()> + 'static;
```

So **each auto-exposed field type picks up three hard bounds**:
`Clone` (from the getter's `clone()`), `IntoLua` (getter return), and
`FromLua` (setter arg). For `x: f64` all three hold and it's delightful
ergonomics — `v.x` just works with zero boilerplate. For `app: App` none
hold, and you get a compile error blaming `App: Clone`.

**`UserData` itself imposes no `Clone`** — it's just `'static`
(`crates/lua-rs-runtime/src/lib.rs:2015`):

```rust
pub trait UserData: 'static { /* ... */ }
```

So issue #56's "requires `Clone`" is entirely an artifact of auto-field
exposure cloning, **not** a trait bound. (Shatur added `#[derive(Clone)]` to
his struct hoping to satisfy it; that *also* fails — `App` isn't `Clone` —
and wouldn't have helped anyway, because the bound is on the field, not the
struct. That confusion is itself evidence the current default is a foot-gun
for handle types.)

**Tuple and unit structs are rejected outright** (`crates/lua-rs-derive/src/lib.rs:137`):

```rust
Fields::Named(named) => &named.named,
_ => {
    return Err(syn::Error::new_spanned(
        &input.ident,
        "LuaUserData currently supports only structs with named fields",
    ))
}
```

That is issue #57: `struct ScriptApp(App)` never compiles.

### One nuance that matters for the design: non-`Clone` fields *can* be
### exposed today — by reference, not value

The `#[lua_methods]` attribute already does reference-delegation: a method
returning `&T`/`&mut T` is exposed as a live sub-reference ("delegate") with
no clone (`crates/lua-rs-derive/src/lib.rs:324`–`359`,
`delegate`/`delegate_ref`). See `crates/lua-rs-runtime/examples/scope_delegate_macro.rs`
for a `Scene -> Entity -> Vec2` chain built this way. This means a *future*
`#[lua(delegate)]` field attribute could expose a non-`Clone` `UserData`
field by reference. It's additive and orthogonal to the default-policy
question below — but it proves the door to non-`Clone` field access isn't
permanently closed by today's design.

---

## 5. Source-level state of the art

### 5a. CppCXY/lua-rs — the reference Shatur cites

Source quoted from `crates/luars-derive/src/derive_userdata.rs` on `main`
and `docs/userdata/DeriveUserData.md`.

**Tuple/unit structs → opaque.** The field-extraction matches only named
fields; everything else falls through to a minimal impl:

```rust
Fields::Named(fields) => Some(&fields.named),
_ => None, // tuple or unit struct — no field export
```

`None` triggers `gen_minimal_impl()` — type name, method lookup, and
metamethods, but **no field access**. So `struct ScriptApp(App)` becomes an
opaque method-holder. That is exactly why Shatur says "CppCXY supports this"
in #57.

**Named fields → only `pub` ones are exposed.** The field loop:

```rust
let is_pub = matches!(field.vis, syn::Visibility::Public(_));
// ...
if skip || !is_pub {
    continue;
}
```

The doc states it plainly: *"Only `pub` fields are exposed to Lua. Private
fields are completely invisible."* So `struct ScriptApp { app: App }` with a
**private** `app` field exposes nothing, needs no `Clone`, and compiles. That
is exactly why Shatur says "CppCXY don't have this requirement" in #56.

**Field attributes:** `#[lua(skip)]`, `#[lua(readonly)]`,
`#[lua(name = "...")]`, `#[lua(iter)]` — a near-superset of ours.

**Auto-exposed field types are a fixed allowlist:** the integer/float types,
`bool`, `String`. Other public field types need a custom `Into<UdValue>`
impl or `#[lua(skip)]`. (We are more general: we expose *any* type satisfying
`Clone + IntoLua + FromLua`. That generality is worth keeping — see §8.)

**Net:** CppCXY's model is one coherent rule — *"expose `pub` named fields;
tuple/unit structs and private fields are opaque."* Both halves are why both
Shatur examples compile there and neither compiles here.

### 5b. mlua — the ecosystem shape-setter

mlua provides **no auto-field-exposure derive at all.** Its `mlua_derive`
crate derives `FromLua` (value conversion) and provides the `lua_module` /
`chunk` macros — but `UserData` is implemented by hand: you write
`add_fields` / `add_methods` yourself (https://docs.rs/mlua/latest/mlua/trait.UserData.html).
Field exposure is therefore 100% explicit and opt-in in mlua.

So on the "how do fields get exposed" axis the three projects sit at:

| Project | Default field exposure | Tuple/newtype structs |
|---|---|---|
| **mlua** | none — fully manual `add_fields` | n/a (no derive) |
| **CppCXY/lua-rs** | **`pub` named fields, auto** | **opaque (supported)** |
| **ianm199/lua-rs (today)** | **all named fields, auto** | **hard error** |

We are the outlier on *both* columns. CppCXY sits between mlua's "explicit"
and our "everything," using Rust visibility as the opt-in signal.

---

## 6. The hard constraint that shapes every option

A `proc_macro_derive` runs at **syntactic** expansion time. It sees the
tokens `app: App` — it cannot ask the type system "is `App: Clone`?"
(no specialization on stable; no trait-impl reflection in proc-macros).

Therefore the macro **cannot auto-skip fields based on whether their type is
`Clone`/marshalable.** Whatever set of fields it decides to expose, it
imposes the `Clone + IntoLua + FromLua` bounds on those field types
*unconditionally*. The only lever available is **which fields are exposed by
default, and what syntactic signal overrides that.** Every option below is a
different answer to that one question.

---

## 7. The options

### Option A — Minimal: support tuple/unit structs as opaque; leave named structs as-is

Principled framing: *the macro exposes fields by their name.* Named fields
have names → exposed (unchanged). Tuple/unit fields have no name → nothing to
expose → opaque handle (matches CppCXY's `gen_minimal_impl`).

- **#57** → works out of the box, zero annotation.
- **#56** → still needs a tweak: either switch to the newtype form (his own
  #57 example, now supported), or add `#[lua(skip)]` to the non-`Clone`
  field. Plus we'd improve the diagnostic so a non-`Clone` exposed field
  points at `#[lua(skip)]` instead of "Clone not satisfied."
- **Backward compat:** none broken. Purely additive.
- **Effort:** ~15 lines in `expand_derive` (add `Fields::Unnamed`/`Unit`
  arms producing empty field regs) + a test. Low risk.
- **Cost:** #56 isn't *fully* zero-annotation in the named-field form, and we
  stay the odd-one-out vs CppCXY on the named-field default. A user who tries
  exactly Shatur's #56 snippet still gets a compile error.

### Option B — CppCXY parity: expose only `pub` named fields + opaque tuple/unit structs (RECOMMENDED)

Adopt CppCXY's model wholesale: a field is auto-exposed iff it is a `pub`
named field without `#[lua(skip)]`; tuple/unit structs are opaque.

- **#56** (`app` is private) → works out of the box, zero annotation.
- **#57** (tuple struct) → works out of the box, zero annotation.
- **Both reporters' exact snippets compile**, matching the implementation
  they're comparing us to.
- **Mental model:** "public fields are scriptable, private fields are
  encapsulated" — mirrors Rust's own visibility and is arguably the correct
  *encapsulation/security* default for handing a Rust object to a script.
- **Backward compat:** **breaks structs that relied on exposing private
  fields** — and breaks them **silently** (the field just becomes `nil` in
  Lua; no compile error). In-repo, this hits `crates/lua-rs-derive/tests/derive.rs`
  (its `Vec2` has private `x`, `y`). `crates/lua-rs-derive/tests/full.rs` uses
  `pub x`/`pub y` and is unaffected. External blast radius is unknown but
  bounded by "pre-1.0, just-now acquiring users."
- **Effort:** ~25 lines — visibility filter in the field loop + the tuple/unit
  arms from Option A + update `derive.rs` test to `pub` + a tuple test +
  README/doc note. Low-medium risk.

### Option C — Fully opt-in: opaque by default everywhere, expose with `#[lua(field)]`

No field exposed unless explicitly annotated `#[lua(field)]`. This is closest
to mlua's "explicit only" philosophy (though mlua has no derive at all).

- **#56, #57** → both work zero-annotation.
- **Mental model:** maximally explicit, no surprises, no visibility magic.
- **Backward compat:** breaks *every* current auto-exposure user (also
  silently — fields become `nil`), and adds boilerplate to the common
  data-record case (`Vec2` needs `#[lua(field)] x` on every field).
- **Effort:** invert the default + annotate every field in every test/example.
- **Cost:** diverges from *both* references (CppCXY auto-exposes `pub`; mlua
  has no derive), and taxes the ergonomic case the derive exists to serve.

### Summary

| | #57 tuple | #56 named non-`Clone` | Backward compat | Matches CppCXY? | Effort |
|---|---|---|---|---|---|
| **A** Minimal | ✅ 0-annot | ⚠️ needs `skip`/newtype | ✅ no breaks | tuple only | ~15 LoC |
| **B** Pub-only (rec.) | ✅ 0-annot | ✅ 0-annot | ⚠️ silent break, private fields | ✅ both halves | ~25 LoC |
| **C** Opt-in | ✅ 0-annot | ✅ 0-annot | ❌ silent break, all fields | ❌ neither | high |

---

## 8. Recommendation: **B**, with two riders

Adopt CppCXY's model — `pub`-only named-field exposure plus opaque tuple/unit
structs. Reasons:

1. **It is the "larger change that actually matters."** Shatur is explicitly
   benchmarking us against CppCXY in both issues ("CppCXY don't have this
   requirement" / "CppCXY supports this"). Matching their derive model closes
   *both* issues with *zero* annotation and removes a standing reason for the
   single highest-value prospective adopter to stay on mlua. Option A leaves
   #56 as a paper cut against exactly his snippet.
2. **It's a proven model, not a guess.** Unlike the scope-API decision in the
   companion doc (where designing without Shatur's input was the main risk),
   here the design is already validated by the direct competitor's shipping
   code. Low design risk.
3. **The mental model is good on its own merits.** "Public fields are
   scriptable; private fields are encapsulated" is intuitive and is the right
   default when exposing a host object to script code.
4. **Pre-1.0 is the cheapest time.** The library is v0.0.9 and only now
   acquiring external users. The expose-all default gets *more* expensive to
   change with every new dependent. The blast radius is minimal today.

**Rider 1 — keep our generality, don't copy CppCXY's type allowlist.** CppCXY
only auto-exposes a fixed primitive+`String` allowlist. We expose any
`Clone + IntoLua + FromLua` type, which is strictly more flexible (a `pub`
field of a `Clone` userdata type works). Keep that; only borrow the `pub`
gate.

**Rider 2 — make the break as loud as possible.** A silent `nil` is the worst
failure mode. Mitigations: (a) a `CHANGELOG`/release-notes entry stating
"private fields are no longer exposed to Lua; mark fields `pub` or expose via
`#[lua_methods]`"; (b) optionally a `#[lua(field)]` escape hatch to force-expose
a *private* field for anyone who genuinely relied on it (keeps the door open
without weakening the default); (c) bump the version to signal the behavior
change.

If the silent-break risk is judged unacceptable even pre-1.0, fall back to
**Option A now** (purely additive, ships today, fully solves #57) and treat
the `pub`-only switch as a separate, announced change. But the recommendation
is B: the upside (winning Shatur, parity with the competitor, a better
default) outweighs a silent break in a pre-1.0 library with a near-empty
dependent set.

---

## 9. What the implementation looks like (sketch for B)

All in `crates/lua-rs-derive/src/lib.rs`, `expand_derive`:

```rust
let fields = match &input.data {
    Data::Struct(s) => match &s.fields {
        Fields::Named(named) => named.named.iter().collect::<Vec<_>>(),
        // NEW: tuple/unit structs are opaque — no field accessors,
        // just the UserData impl so #[lua(methods)]/metamethods attach.
        Fields::Unnamed(_) | Fields::Unit => Vec::new(),
    },
    _ => return Err(/* "LuaUserData supports only structs" */),
};

let mut field_regs = Vec::new();
for field in fields {
    let cfg = parse_field_cfg(field)?;
    // NEW: visibility gate — only pub fields auto-expose, unless an
    // explicit opt-in attribute (Rider 2) is present.
    let is_pub = matches!(field.vis, syn::Visibility::Public(_));
    if cfg.skip || (!is_pub && !cfg.force_field) {
        continue;
    }
    // ... existing getter/setter generation, unchanged ...
}
```

- `parse_field_cfg` already tolerates everything except the `ident.unwrap()`
  on tuple fields — but since tuple structs now produce an empty field list,
  that path is never hit. (If Rider 2's `#[lua(field)]` is added for tuple
  positional access later, revisit; not needed for #56/#57.)
- `#[lua(methods)]`, `#[lua_impl(...)]` metamethods, and the
  `__lua_register_methods` hook all key off the struct name and are
  unaffected — they work identically for tuple/unit structs.
- Tests: flip `crates/lua-rs-derive/tests/derive.rs` `Vec2` fields to `pub`;
  add a `tests/tuple_struct.rs` asserting `struct Handle(Inner)` derives,
  exposes no fields, and accepts `#[lua_methods]`; add a `non_clone_field`
  test asserting a struct with a private non-`Clone` field compiles.
- Docs: update the `lib.rs` module doc (currently says "exposes the struct's
  fields") to "exposes the struct's **public** fields"; add a CHANGELOG entry.

Estimate: ~25 LoC impl + ~60 LoC tests + doc/changelog. Half a day including
verification.

---

## 10. Pointers and reading list

- Issue #56: https://github.com/ianm199/lua-rs/issues/56
- Issue #57: https://github.com/ianm199/lua-rs/issues/57
- Our derive: `crates/lua-rs-derive/src/lib.rs` (getter clone at :161; tuple
  reject at :137; delegate machinery at :324)
- Our `UserData` trait: `crates/lua-rs-runtime/src/lib.rs:2015`; field-method
  bounds at :2057
- Our delegate example: `crates/lua-rs-runtime/examples/scope_delegate_macro.rs`
- CppCXY derive: https://github.com/CppCXY/lua-rs/blob/main/crates/luars-derive/src/derive_userdata.rs
- CppCXY derive docs: https://github.com/CppCXY/lua-rs/blob/main/docs/userdata/DeriveUserData.md
- mlua `UserData` (manual, no field derive): https://docs.rs/mlua/latest/mlua/trait.UserData.html
- mlua derive crate (FromLua etc.): https://github.com/mlua-rs/mlua/blob/main/mlua_derive/src/lib.rs
- Companion decision doc (same user, scope/`'static` request):
  [`SCOPE_NONSTATIC_DECISION.md`](SCOPE_NONSTATIC_DECISION.md)

---

## 11. TL;DR

- Issues #56 and #57 are one request — "wrap a non-`Clone`, opaque Rust type
  as Lua userdata with methods" — in two spellings (named struct vs newtype).
- Both fail on ianm199/lua-rs for the same root cause: the derive
  auto-exposes **every** named field by cloning it (forcing `Clone + IntoLua
  + FromLua` on each field type) and **hard-errors** on tuple/unit structs.
  The `Clone` requirement is from field-cloning, *not* from the `UserData`
  trait (which is just `'static`).
- The competitor Shatur cites, **CppCXY/lua-rs**, uses one coherent model
  that makes both of his snippets compile: **expose only `pub` named fields;
  tuple/unit structs and private fields are opaque.** Verified in their
  source (`is_pub = matches!(field.vis, Visibility::Public(_)); … continue;`
  and tuple → `gen_minimal_impl`). mlua has no field-exposure derive at all.
- **Recommendation: adopt CppCXY's model (Option B)** — `pub`-only field
  exposure + opaque tuple/unit structs. Closes both issues zero-annotation,
  matches the reference, is a better default, and is cheapest to do pre-1.0.
  The one real cost is a *silent* behavior change for anyone exposing private
  fields today (in-repo: one test); mitigate with loud release notes, an
  optional `#[lua(field)]` force-expose escape hatch, and a version bump.
- If the silent break is unacceptable even now, fall back to **Option A**
  (additive: tuple structs opaque, named structs unchanged) — ships today and
  fully fixes #57, but leaves #56 needing a one-word `#[lua(skip)]`.
