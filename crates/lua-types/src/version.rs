//! `LuaVersion` — the single source of truth for which Lua language version a
//! runtime instance speaks.
//!
//! This lives in `lua-types`, the lowest shared crate, so every layer above
//! (parser, compiler, VM, stdlib, runtime) can name the version without a
//! dependency cycle. Per the multi-version architecture decision
//! (`specs/MULTIVERSION_ARCHITECTURE_DECISION.md` §4, §5), the version is a
//! *backend selector* threaded from construction; it never appears in a public
//! embedding-API type.

/// The numeric model a version uses for Lua numbers.
///
/// This is the single sharpest behavioral axis across versions: 5.1/5.2 are
/// float-only (one `number` type, every value an `f64`, no `math.type`), while
/// 5.3/5.4/5.5 carry the dual integer/float subtype.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NumberModel {
    /// One `number` type; every numeric value is an `f64`. Lua 5.1/5.2.
    FloatOnly,
    /// Distinct integer (`i64`) and float (`f64`) subtypes. Lua 5.3/5.4/5.5.
    Dual,
}

/// Which Lua language version a runtime instance speaks.
///
/// `Default` is [`LuaVersion::V54`] — the version this codebase currently
/// implements end-to-end — so that `Lua::new()` and any other defaulted
/// construction keeps the existing 5.4 behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LuaVersion {
    /// Lua 5.1 — float-only, `fenv`-based globals. Deferred (separate core).
    V51,
    /// Lua 5.2 — float-only, modern `_ENV` globals. Deferred (separate core).
    V52,
    /// Lua 5.3 — dual subtype, modern `_ENV`. Deferred.
    V53,
    /// Lua 5.4 — the implemented baseline today.
    V54,
    /// Lua 5.5 — dual subtype, declared-globals scope model. Deferred.
    V55,
}

impl Default for LuaVersion {
    fn default() -> Self {
        LuaVersion::V54
    }
}

impl LuaVersion {
    /// The family-level numeric model for this version.
    pub fn number_model(self) -> NumberModel {
        match self {
            LuaVersion::V51 | LuaVersion::V52 => NumberModel::FloatOnly,
            LuaVersion::V53 | LuaVersion::V54 | LuaVersion::V55 => NumberModel::Dual,
        }
    }

    /// The `_VERSION` global string for this version (e.g. `"Lua 5.4"`).
    pub fn version_str(self) -> &'static str {
        match self {
            LuaVersion::V51 => "Lua 5.1",
            LuaVersion::V52 => "Lua 5.2",
            LuaVersion::V53 => "Lua 5.3",
            LuaVersion::V54 => "Lua 5.4",
            LuaVersion::V55 => "Lua 5.5",
        }
    }

    /// The `LUAC_VERSION` byte written into a `luac`/`string.dump` header for
    /// this version. Upstream encodes the version as `(major << 4) | minor`,
    /// e.g. 5.4 → `0x54`.
    pub fn luac_version_byte(self) -> u8 {
        match self {
            LuaVersion::V51 => 0x51,
            LuaVersion::V52 => 0x52,
            LuaVersion::V53 => 0x53,
            LuaVersion::V54 => 0x54,
            LuaVersion::V55 => 0x55,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_v54() {
        assert_eq!(LuaVersion::default(), LuaVersion::V54);
    }

    #[test]
    fn number_model_split() {
        assert_eq!(LuaVersion::V51.number_model(), NumberModel::FloatOnly);
        assert_eq!(LuaVersion::V52.number_model(), NumberModel::FloatOnly);
        assert_eq!(LuaVersion::V53.number_model(), NumberModel::Dual);
        assert_eq!(LuaVersion::V54.number_model(), NumberModel::Dual);
        assert_eq!(LuaVersion::V55.number_model(), NumberModel::Dual);
    }

    #[test]
    fn version_str_and_byte() {
        assert_eq!(LuaVersion::V54.version_str(), "Lua 5.4");
        assert_eq!(LuaVersion::V54.luac_version_byte(), 0x54);
        assert_eq!(LuaVersion::V53.version_str(), "Lua 5.3");
        assert_eq!(LuaVersion::V53.luac_version_byte(), 0x53);
    }
}

// ──────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        (foundation — multi-version seam, not ported from .c)
//   target_crate:  lua-types
//   confidence:    high
//   todos:         0
//   port_notes:    0
//   unsafe_blocks: 0
//   notes:         LuaVersion + NumberModel. Default = V54 preserves the
//                  existing single-version behavior. Only V54 has a backend.
// ──────────────────────────────────────────────────────────────────────────
