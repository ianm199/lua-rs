//! Phase-D `Trace` implementations for GC-rooted types defined in this
//! crate. Types in `lua-types` (LuaValue, LuaString, UpVal) have their
//! Trace impls in `lua-types/src/trace_impls.rs` because of Rust's orphan
//! rule.
//!
//! Each impl below is a `todo!("phase-d: trace X")` stub. The
//! panic-driven mega-loop surfaces each one when a runtime path triggers
//! `Heap::full_collect`. Each agent works on ONE type — no family
//! expansion (Trace impls have subtle invariants).
//!
//! Implementation guidance for agents:
//!   1. Read the type definition; enumerate every field
//!   2. For every `Gc<T>`, `GcRef<T>`, or container (Vec/Option/HashMap)
//!      thereof, call `m.mark(field)` or `field.trace(m)` appropriately
//!   3. Skip non-GC fields (primitives, `String`, `Vec<u8>`)
//!   4. Skip "intentionally not traced" fields (weak refs)
//!   5. Reference `reference/lua-5.4.7/src/lgc.c`'s `reallymarkobject`

use lua_gc::{Marker, Trace};
use crate::state::{LuaState, GlobalState};
use crate::string::{LuaStringImpl, LuaUserDataImpl};
use crate::table::LuaTable as VmLuaTable;

/// Phase-B internal richer LuaString. The byte buffer is a Rust `Rc<[u8]>`
/// (not GC-managed); no fields to mark.
impl Trace for LuaStringImpl {
    fn trace(&self, _m: &mut Marker) {}
}

/// Phase-B internal userdata. Both `metatable` and `uv` are currently
/// `Option<()>` / `Vec<()>` stubs — no GC edges to walk yet. Becomes
/// real when userdata machinery lands post-D-1.
impl Trace for LuaUserDataImpl {
    fn trace(&self, _m: &mut Marker) {}
}

/// Phase-B internal LuaTable (separate from lua-types::LuaTable
/// placeholder).
impl Trace for VmLuaTable {
    fn trace(&self, m: &mut Marker) {
        for slot in self.array.iter() {
            slot.trace(m);
        }
        for entry in self.node.iter() {
            entry.key.trace(m);
            entry.value.trace(m);
        }
        if let Some(mt) = self.metatable.as_ref() {
            mt.trace(m);
        }
    }
}

impl Trace for LuaState {
    fn trace(&self, m: &mut Marker) {
        // C: `traversethread` in lgc.c walks the live portion of the stack
        // (`stack..top`) and the open-upvalue list. Slots past `top` are
        // dead and must not be visited.
        let top = self.top.0 as usize;
        let end = top.min(self.stack.len());
        for slot in &self.stack[..end] {
            slot.val.trace(m);
        }

        for uv in self.openupval.iter() {
            uv.trace(m);
        }

        // PORT NOTE: `global` (Rc<RefCell<GlobalState>>) is reached from the
        // heap's root via GlobalState::trace; tracing it from each thread
        // would re-enter the root and is explicitly excluded.
        // PORT NOTE: `call_info` entries carry pc offsets and stack indices
        // but no direct GcRef fields. The active closure is reached through
        // the stack slot at `ci.func`, already covered by the stack walk.
        // PORT NOTE: `tbclist` holds StackIdx values only; the to-be-closed
        // objects themselves live on the stack and are traced there.
    }
}

impl Trace for GlobalState {
    fn trace(&self, m: &mut Marker) {
        // C: `restartcollection` in lgc.c marks mainthread, l_registry, the
        // per-type metatables, and pending finalizers. We expand the set to
        // include preallocated short strings (memerrmsg, tmname[]) and the
        // open-upvalue thread list, both of which the panic-driven Phase-D
        // mega-loop expects to see at the root.

        self.l_registry.trace(m);

        // PORT NOTE (phase-b-reconcile): The lua-types LuaTable placeholder is
        // storage-less, so `globals` and `loaded` cannot live inside the registry
        // table (see `init_registry`). They are kept as direct GlobalState fields
        // and must be traced explicitly as roots; once the placeholder reconciles
        // with vm::LuaTable, these become reachable via `l_registry` and the two
        // lines below disappear.
        self.globals.trace(m);
        self.loaded.trace(m);

        if let Some(t) = &self.mainthread {
            t.trace(m);
        }

        self.main_thread_value.trace(m);

        for entry in self.threads.values() {
            entry.state.trace(m);
            entry.value.trace(m);
        }

        for slot in self.mt.iter() {
            if let Some(t) = slot {
                t.trace(m);
            }
        }

        for s in self.tmname.iter() {
            s.trace(m);
        }

        self.memerrmsg.trace(m);

        for th in self.twups.iter() {
            th.trace(m);
        }

        // The short-string intern cache holds `GcRef<LuaString>` values that
        // callers (parser, stdlib) reuse by pointer-equality across
        // `intern_str` calls. C-Lua treats this as a weak table cleared during
        // the atomic weak-table pass (`clearbykeys`); we have no incremental
        // weak-sweep yet, so leaving these untraced would leave the HashMap
        // with dangling `Gc<LuaString>` entries after the very next collect.
        // Trace them as strong roots until the weak-sweep machinery lands.
        for s in self.interned_lt.values() {
            s.trace(m);
        }
        for row in self.strcache.iter() {
            for s in row.iter() {
                s.trace(m);
            }
        }

        // Pending finalizers are NOT traced here — that's what lets the mark
        // phase distinguish "still reachable from the user program" from
        // "only kept alive by the finalizer registry". `collect_via_heap`'s
        // post-mark hook checks each entry against the visited set; an
        // unvisited entry is moved to `to_be_finalized` and explicitly
        // marked there so it survives the sweep.
        //
        // `to_be_finalized` IS traced as a strong root: tables in this list
        // are awaiting their `__gc` call but are otherwise dead, and the
        // table (plus its descendants) must survive long enough for the
        // finalizer to run.
        for t in self.to_be_finalized.iter() {
            t.trace(m);
        }

        // PORT NOTE: `strt` (the internal LuaStringImpl intern table) is a
        // weak table in C; entries are cleared during the atomic weak-table
        // pass (`clearbykeys`), not marked as roots. The current port has no
        // incremental weak-sweep, but `strt` is keyed by byte-content rather
        // than by `Gc` identity, so a dangling entry there is silently
        // recreated by the next `intern_str` — no UAF, unlike `interned_lt`.
        // PORT NOTE: `fixedgc` holds objects pre-marked fixed/black at
        // allocation (`luaC_fix`); the mark phase never re-visits them, and
        // `dyn Collectable` does not implement `Trace` here.
        // PORT NOTE: `allgc`, `finobj`, `gray`, `grayagain`, `tobefnz`,
        // `weak`, `ephemeron`, `allweak` are GC bookkeeping lists owned by
        // `heap` — they are the universe of allocated objects, not roots.
    }
}
