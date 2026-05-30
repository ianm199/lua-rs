//! Lua parser — translates the token stream produced by the lexer into
//! bytecode prototypes (`LuaProto`).
//!
//! # C source
//! `reference/lua-5.4.7/src/lparser.c` (1968 lines, 95 functions)
//!
//! # Design notes (Phase A)
//! * `BlockCnt` and `LhsAssign` form intrusive linked lists in C via raw
//!   pointers to stack-allocated nodes. In Rust they become
//!   `Option<Box<...>>` chains; `enter_block` pushes, `leave_block` pops.
//! * `FuncState.prev` similarly uses `Option<Box<FuncState>>`.
//! * `FuncState.f` is `Box<LuaProto>` during compilation (owned, mutably
//!   accessible). types.tsv maps it to `GcRef<LuaProto>` but interior-
//!   mutability via `Rc<RefCell<...>>` would be too noisy; Phase B can
//!   switch. PORT NOTE: FuncState.f is Box<LuaProto>, not GcRef<LuaProto>.
//! * `LexState` is logically defined in `lua-lex`; a minimal stub is declared
//!   here for Phase A. Phase B will replace with `lua_lex::LexState` once
//!   inter-crate deps are wired.
//! * Cross-crate calls to `lua_code::luaK_*` and `lua_lex::luaX_*` are
//!   written as qualified paths and will resolve in Phase B.
//! * `LuaState` is from `lua-vm`; referenced here as an unresolved import.

use lua_types::{AbsLineInfo, GcRef, LuaError, LuaString, LuaValue, LuaProto, UpvalDesc, LocalVar};

// TODO(port): these imports resolve in Phase B when inter-crate deps land.
// use lua_vm::LuaState;
// use lua_code::{self, UnOpr, BinOpr, OpCode};

// ── Token kind constants ────────────────────────────────────────────────────
// TODO(port): replace with lua_lex::TokenKind enum when lua-lex lands.

pub type TokenKind = i32;
pub const TK_AND: TokenKind = 257;
pub const TK_BREAK: TokenKind = 258;
pub const TK_DO: TokenKind = 259;
pub const TK_ELSE: TokenKind = 260;
pub const TK_ELSEIF: TokenKind = 261;
pub const TK_END: TokenKind = 262;
pub const TK_FALSE: TokenKind = 263;
pub const TK_FOR: TokenKind = 264;
pub const TK_FUNCTION: TokenKind = 265;
pub const TK_GOTO: TokenKind = 266;
pub const TK_IF: TokenKind = 267;
pub const TK_IN: TokenKind = 268;
pub const TK_LOCAL: TokenKind = 269;
pub const TK_NIL: TokenKind = 270;
pub const TK_NOT: TokenKind = 271;
pub const TK_OR: TokenKind = 272;
pub const TK_REPEAT: TokenKind = 273;
pub const TK_RETURN: TokenKind = 274;
pub const TK_THEN: TokenKind = 275;
pub const TK_TRUE: TokenKind = 276;
pub const TK_UNTIL: TokenKind = 277;
pub const TK_WHILE: TokenKind = 278;
pub const TK_IDIV: TokenKind = 279;
pub const TK_CONCAT: TokenKind = 280;
pub const TK_DOTS: TokenKind = 281;
pub const TK_EQ: TokenKind = 282;
pub const TK_GE: TokenKind = 283;
pub const TK_LE: TokenKind = 284;
pub const TK_NE: TokenKind = 285;
pub const TK_SHL: TokenKind = 286;
pub const TK_SHR: TokenKind = 287;
pub const TK_DBCOLON: TokenKind = 288;
pub const TK_EOS: TokenKind = 289;
pub const TK_FLT: TokenKind = 290;
pub const TK_INT: TokenKind = 291;
pub const TK_NAME: TokenKind = 292;
pub const TK_STRING: TokenKind = 293;

// ── Parser constants ────────────────────────────────────────────────────────

const MAX_VARS: i32 = 200;

const NO_JUMP: i32 = -1;

const UNARY_PRIORITY: i32 = 12;

const LUA_MULTRET: i32 = -1;

const MAX_UPVAL: u8 = 255;

/// TODO(port): should come from lua_types::opcode constants.
const MAXARG_BX: i32 = (1 << 17) - 1;

const LFIELDS_PER_FLUSH: i32 = 50;

// ── Variable kind constants ─────────────────────────────────────────────────
// macros.tsv maps these to VarKind enum variants.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum VarKind {
    Reg = 0,
    Const = 1,
    ToBeClosed = 2,
    CompileTimeConst = 3,
}

impl VarKind {
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => VarKind::Reg,
            1 => VarKind::Const,
            2 => VarKind::ToBeClosed,
            3 => VarKind::CompileTimeConst,
            _ => VarKind::Reg,
        }
    }
    pub fn as_u8(self) -> u8 { self as u8 }
}

// ── ExprKind ────────────────────────────────────────────────────────────────

/// Variants correspond exactly to the C enum in lparser.h.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExprKind {
    Void,       // VVOID: empty expression list
    Nil,        // VNIL: constant nil
    True,       // VTRUE: constant true
    False,      // VFALSE: constant false
    K,          // VK: constant in k[]; info = index
    KFlt,       // VKFLT: float constant; u.nval
    KInt,       // VKINT: integer constant; u.ival
    KStr,       // VKSTR: string constant; u.strval
    NonReloc,   // VNONRELOC: value in fixed register; info = reg
    Local,      // VLOCAL: local variable; u.var.ridx, u.var.vidx
    UpVal,      // VUPVAL: upvalue; info = upvalue index
    Const,      // VCONST: compile-time const; info = absolute actvar index
    Indexed,    // VINDEXED: indexed by reg key; u.ind.t, u.ind.idx
    IndexUp,    // VINDEXUP: indexed upvalue; u.ind.t, u.ind.idx
    IndexI,     // VINDEXI: indexed by int; u.ind.t, u.ind.idx
    IndexStr,   // VINDEXSTR: indexed by string; u.ind.t, u.ind.idx
    Jmp,        // VJMP: test/comparison; info = jump instruction pc
    Reloc,      // VRELOC: result in any register; info = instruction pc
    Call,       // VCALL: function call; info = instruction pc
    VarArg,     // VVARARG: vararg; info = instruction pc
}

impl ExprKind {
    #[inline]
    pub fn has_mult_ret(self) -> bool {
        matches!(self, ExprKind::Call | ExprKind::VarArg)
    }

    #[inline]
    pub fn is_var(self) -> bool {
        matches!(
            self,
            ExprKind::Local
                | ExprKind::UpVal
                | ExprKind::Const
                | ExprKind::Indexed
                | ExprKind::IndexUp
                | ExprKind::IndexI
                | ExprKind::IndexStr
        )
    }

    #[inline]
    pub fn is_indexed(self) -> bool {
        matches!(
            self,
            ExprKind::Indexed | ExprKind::IndexUp | ExprKind::IndexI | ExprKind::IndexStr
        )
    }
}

// ── ExprPayload ─────────────────────────────────────────────────────────────

/// PORT NOTE: C uses a union; all arms share memory. Rust keeps all fields in
///   one struct for Phase A simplicity. Phase B may refactor to a proper enum.
#[derive(Debug, Clone, Default)]
pub struct ExprPayload {
    pub ival: i64,
    pub nval: f64,
    pub strval: Option<GcRef<LuaString>>,
    pub info: i32,
    pub ind_idx: i16,
    pub ind_t: u8,
    pub var_ridx: u8,
    pub var_vidx: u16,
}

// ── ExprDesc ────────────────────────────────────────────────────────────────

/// Field `t`/`f` are patch-lists for short-circuit boolean evaluation.
#[derive(Debug, Clone)]
pub struct ExprDesc {
    pub k: ExprKind,
    pub u: ExprPayload,
    pub t: i32,
    pub f: i32,
}

impl Default for ExprDesc {
    fn default() -> Self {
        ExprDesc { k: ExprKind::Void, u: ExprPayload::default(), t: NO_JUMP, f: NO_JUMP }
    }
}

// ── VarDesc ─────────────────────────────────────────────────────────────────

/// PORT NOTE: C uses a union (vd fields + k for const value). Rust keeps all
///   fields in a struct. The `const_val` field is only meaningful when
///   `kind == VarKind::CompileTimeConst`.
#[derive(Debug, Clone)]
pub struct VarDesc {
    pub kind: VarKind,
    pub ridx: u8,
    pub pidx: i16,
    pub name: Option<GcRef<LuaString>>,
    pub const_val: LuaValue,
}

impl Default for VarDesc {
    fn default() -> Self {
        VarDesc {
            kind: VarKind::Reg,
            ridx: 0,
            pidx: 0,
            name: None,
            const_val: LuaValue::Nil,
        }
    }
}

// ── LabelDesc ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct LabelDesc {
    pub name: Option<GcRef<LuaString>>,
    pub pc: i32,
    pub line: i32,
    pub nactvar: u8,
    pub close: bool,
}

// ── DynData ─────────────────────────────────────────────────────────────────

/// C stored C-style dynamic arrays (arr/n/size); Rust uses Vec.
#[derive(Debug, Default)]
pub struct DynData {
    pub actvar: Vec<VarDesc>,
    pub gt: Vec<LabelDesc>,
    pub label: Vec<LabelDesc>,
}

// ── BlockCnt ────────────────────────────────────────────────────────────────

/// In C: stack-allocated, chained via raw `*previous` pointer.
/// In Rust: heap-allocated in an `Option<Box<BlockCnt>>` chain on FuncState.
#[derive(Debug)]
pub struct BlockCnt {
    pub previous: Option<Box<BlockCnt>>,
    pub firstlabel: i32,
    pub firstgoto: i32,
    pub nactvar: u8,
    pub upval: bool,
    pub isloop: bool,
    pub insidetbc: bool,
}

// ── FuncState ───────────────────────────────────────────────────────────────

/// In C: stack-allocated in `body()`, chained via raw `*prev` pointer.
/// In Rust: heap-allocated via `Option<Box<FuncState>>` in LexState.
#[derive(Debug)]
pub struct FuncState {
    /// PORT NOTE: types.tsv maps this to GcRef<LuaProto>; we use Box<LuaProto>
    ///   during compilation to avoid RefCell overhead. close_func hands it to
    ///   the GC/parent at close time.
    pub f: Box<LuaProto>,
    pub prev: Option<Box<FuncState>>,
    pub bl: Option<Box<BlockCnt>>,
    pub pc: i32,
    pub lasttarget: i32,
    pub previousline: i32,
    pub nk: i32,
    pub np: i32,
    pub nabslineinfo: i32,
    pub firstlocal: i32,
    pub firstlabel: i32,
    pub ndebugvars: i16,
    pub nactvar: u8,
    pub nups: u8,
    pub freereg: u8,
    pub iwthabs: u8,
    pub needclose: bool,
    /// Current `ls.lastline` value, mirrored on every `sync_from_lex`.
    /// Used by `emit_inst` to attribute the line to the just-consumed token
    /// (matching lua-c's `savelineinfo(fs, f, fs->ls->lastline)`), instead
    /// of whatever `line` the caller threaded down. The threaded `line`
    /// param is preserved only for explicit overrides (luaK_fixline-style).
    pub last_token_line: i32,
}

// ── ConsControl ─────────────────────────────────────────────────────────────

/// PORT NOTE: C stores `expdesc *t` as a pointer to the caller's expdesc.
///   Rust stores a copy of the table descriptor; callers must sync back
///   if they mutate it. Phase B may restructure.
#[derive(Debug)]
pub struct ConsControl {
    pub v: ExprDesc,
    pub t: ExprDesc,
    pub nh: i32,
    pub na: i32,
    pub tostore: i32,
}

// ── LhsAssign ───────────────────────────────────────────────────────────────

/// In C: stack-allocated, chained via raw `*prev`. In Rust: `Option<Box<...>>`.
#[derive(Debug)]
pub struct LhsAssign {
    pub prev: Option<Box<LhsAssign>>,
    pub v: ExprDesc,
}

// ── Unary / binary operator enums ───────────────────────────────────────────
// TODO(port): unify with lua_code::UnOpr / BinOpr when lua-code lands.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOpr {
    Minus,    // OPR_MINUS
    BNot,     // OPR_BNOT
    Not,      // OPR_NOT
    Len,      // OPR_LEN
    NoUnOpr,  // OPR_NOUNOPR
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOpr {
    Add,     // OPR_ADD
    Sub,     // OPR_SUB
    Mul,     // OPR_MUL
    Mod,     // OPR_MOD
    Pow,     // OPR_POW
    Div,     // OPR_DIV
    IDiv,    // OPR_IDIV
    BAnd,    // OPR_BAND
    BOr,     // OPR_BOR
    BXor,    // OPR_BXOR
    Shl,     // OPR_SHL
    Shr,     // OPR_SHR
    Concat,  // OPR_CONCAT
    Eq,      // OPR_EQ
    Lt,      // OPR_LT
    Le,      // OPR_LE
    Ne,      // OPR_NE
    Gt,      // OPR_GT
    Ge,      // OPR_GE
    And,     // OPR_AND
    Or,      // OPR_OR
    NoBinOpr, // OPR_NOBINOPR
}

/// Indexed by BinOpr discriminant (0 = Add, ... 20 = Or).
const PRIORITY: [(u8, u8); 21] = [
    (10, 10), (10, 10),       // Add, Sub
    (11, 11), (11, 11),       // Mul, Mod
    (14, 13),                 // Pow (right-associative)
    (11, 11), (11, 11),       // Div, IDiv
    (6, 6), (4, 4), (5, 5),  // BAnd, BOr, BXor
    (7, 7), (7, 7),           // Shl, Shr
    (9, 8),                   // Concat (right-associative)
    (3, 3), (3, 3), (3, 3),  // Eq, Lt, Le
    (3, 3), (3, 3), (3, 3),  // Ne, Gt, Ge
    (2, 2), (1, 1),           // And, Or
];

// TODO_ARCH(phase-b-reconcile): re-exporting canonical OpCode from lua-code.
pub use lua_code::opcodes::OpCode;

// ── Minimal LexState stub ───────────────────────────────────────────────────
// PORT NOTE: In C, LexState is defined in llex.h (→ lua-lex crate).
//   We declare a minimal stub here for Phase A so function bodies can be
//   written. Phase B will replace with `lua_lex::LexState` and remove this.

/// Semantic info attached to a token.
#[derive(Debug, Clone, Default)]
pub struct TokenValue {
    pub r: f64,
    pub i: i64,
    pub ts: Option<GcRef<LuaString>>,
}

#[derive(Debug, Clone, Default)]
pub struct LexToken {
    pub token: TokenKind,
    pub seminfo: TokenValue,
}

/// PORT NOTE: This is a Phase A stub. In Phase B, `LexState` lives in
///   `lua-lex` and `lua-parse` imports it. `FuncState` will move here
///   or be passed separately. The `fs` field creates a circular-crate
///   dependency that Phase B must resolve (likely: both live in one crate).
pub struct LexState {
    pub current: i32,
    pub linenumber: i32,
    pub lastline: i32,
    pub t: LexToken,
    pub lookahead: LexToken,
    pub fs: Option<Box<FuncState>>,
    pub dyd: DynData,
    pub source: Option<GcRef<LuaString>>,
    pub envn: Option<GcRef<LuaString>>,
    /// Underlying lexer state that owns the ZIO stream and lex buffer.
    /// The parser drives the lexer by calling `lex_next` / `lex_lookahead`,
    /// which forward to `lua_lex::next` / `lua_lex::lookahead` on this inner
    /// state and then mirror the resulting token into `self.t` / `self.lookahead`.
    pub lex: lua_lex::LexState,
    /// Parser recursion depth for C-Lua's `enterlevel` / `leavelevel` guard.
    pub recursion_depth: u32,
}

const PARSER_MAX_C_CALLS: u32 = 200;

fn enter_level(ls: &mut LexState) -> Result<(), LuaError> {
    ls.recursion_depth += 1;
    if ls.recursion_depth >= PARSER_MAX_C_CALLS {
        Err(LuaError::syntax(format_args!("C stack overflow")))
    } else {
        Ok(())
    }
}

fn leave_level(ls: &mut LexState) {
    ls.recursion_depth = ls.recursion_depth.saturating_sub(1);
}

/// Advance the lexer one token and mirror the resulting state into the
/// parser's outer `LexState` fields. This is the canonical replacement for the
/// Phase A `// TODO(port): lua_lex::next(ls, state)?;` stubs.
fn lex_next(ls: &mut LexState, state: &mut LuaState) -> Result<(), LuaError> {
    lua_lex::next(state, &mut ls.lex)?;
    sync_from_lex(ls);
    Ok(())
}

/// Populate the lookahead token and mirror lexer state. Replaces the
/// `// TODO(port): lua_lex::lookahead(ls, state)?` stub.
fn lex_lookahead(ls: &mut LexState, state: &mut LuaState) -> Result<TokenKind, LuaError> {
    let kind = lua_lex::lookahead(state, &mut ls.lex)?;
    sync_from_lex(ls);
    Ok(kind)
}

/// Copy lexer-side current/line/token/lookahead values back into the parser's
/// outer LexState. Used after every `lua_lex::next` / `lua_lex::lookahead`.
fn sync_from_lex(ls: &mut LexState) {
    ls.current = ls.lex.current;
    ls.linenumber = ls.lex.linenumber;
    ls.lastline = ls.lex.lastline;
    ls.t = LexToken {
        token: ls.lex.t.kind,
        seminfo: local_token_value(&ls.lex.t.value),
    };
    ls.lookahead = LexToken {
        token: ls.lex.lookahead.kind,
        seminfo: local_token_value(&ls.lex.lookahead.value),
    };
    // Mirror lastline into the active FuncState so emit_inst can read it
    // without needing access to LexState. This matches lua-c's
    // `savelineinfo(fs, f, fs->ls->lastline)` semantics.
    if let Some(fs) = ls.fs.as_mut() {
        fs.last_token_line = ls.lastline;
    }
}

// TODO_ARCH(phase-b-reconcile): re-exporting canonical LuaState from lua-vm.
pub use lua_vm::state::LuaState;

// ── Minimal inline codegen (Phase A bootstrap) ──────────────────────────────
//
// The full code generator lives in `lua-code` but operates on its own
// placeholder `FuncState` / `ExprDesc` types (see `lua-code/src/codegen.rs`
// "PHASE B PLACEHOLDERS"), so it cannot yet be called from `lua-parse` with
// the real types defined here. Until that reconciliation lands, the parser
// emits the small subset of bytecode required to execute simple programs
// (global lookup + function call + string literal arg) directly, using the
// shared `Instruction` encoding from `lua-code::opcodes`.
//
// These helpers mirror the behaviour of the C codegen functions they replace
// (`luaK_codeABC`, `luaK_stringK`, `luaK_dischargevars` for the VINDEXUP
// case, `luaK_exp2nextreg` for the VKSTR case). Phase B should delete this
// section once lua-code is reachable from lua-parse with unified types.

fn emit_inst(fs: &mut FuncState, line: i32, inst: lua_code::opcodes::Instruction) -> i32 {
    const MAX_IWTH_ABS: i32 = 128;
    const LIM_LINE_DIFF: i32 = 0x80;
    const ABS_LINE_INFO: i8 = -0x80i8;
    let pc = fs.pc as usize;
    if fs.f.code.len() <= pc {
        fs.f.code.resize(pc + 1, lua_types::opcode::Instruction::default());
    }
    fs.f.code[pc] = lua_types::opcode::Instruction::new(inst.0);
    if fs.f.lineinfo.len() <= pc {
        fs.f.lineinfo.resize(pc + 1, 0i8);
    }
    let linedif_raw = line - fs.previousline;
    let need_abs = linedif_raw.abs() >= LIM_LINE_DIFF || {
        let over = fs.iwthabs as i32 >= MAX_IWTH_ABS;
        if !over { fs.iwthabs += 1; }
        over
    };
    if need_abs {
        fs.f.abslineinfo.push(AbsLineInfo { pc: pc as i32, line });
        fs.nabslineinfo += 1;
        fs.f.lineinfo[pc] = ABS_LINE_INFO;
        fs.iwthabs = 1;
    } else {
        fs.f.lineinfo[pc] = linedif_raw as i8;
    }
    fs.previousline = line;
    let result = fs.pc;
    fs.pc += 1;
    result
}

fn add_k_value(fs: &mut FuncState, v: LuaValue) -> i32 {
    let idx = fs.nk;
    if (fs.f.k.len() as i32) <= idx {
        fs.f.k.resize((idx + 1) as usize, LuaValue::Nil);
    }
    fs.f.k[idx as usize] = v;
    fs.nk += 1;
    idx
}

fn add_k_string(fs: &mut FuncState, s: GcRef<LuaString>) -> i32 {
    for (i, k) in fs.f.k.iter().take(fs.nk as usize).enumerate() {
        if let LuaValue::Str(existing) = k {
            if GcRef::ptr_eq(existing, &s) {
                return i as i32;
            }
        }
    }
    add_k_value(fs, LuaValue::Str(s))
}

fn bump_maxstack(fs: &mut FuncState, n: u8) {
    if fs.f.maxstacksize < n {
        fs.f.maxstacksize = n;
    }
}

fn reserve_reg(fs: &mut FuncState) -> Result<u8, LuaError> {
    if fs.freereg == u8::MAX {
        return Err(LuaError::syntax(format_args!(
            "function or expression needs too many registers"
        )));
    }
    let r = fs.freereg;
    fs.freereg += 1;
    bump_maxstack(fs, fs.freereg);
    Ok(r)
}

fn reserve_regs(fs: &mut FuncState, n: i32) -> Result<(), LuaError> {
    let newstack = fs.freereg as i32 + n;
    if newstack >= 255 {
        return Err(LuaError::syntax(format_args!(
            "function or expression needs too many registers"
        )));
    }
    fs.freereg = newstack as u8;
    bump_maxstack(fs, fs.freereg);
    Ok(())
}

/// Free `reg` if it sits above the active-local watermark.
///
/// Mirrors C's `freereg` from `lcode.c`: registers below `nactvar` belong to
/// declared locals and must not be popped; temporaries above that watermark
/// are freed by decrementing `fs.freereg`.
fn cg_free_reg(fs: &mut FuncState, reg: i32) {
    if reg >= fs.nactvar as i32 {
        debug_assert_eq!(reg, fs.freereg as i32 - 1);
        fs.freereg = fs.freereg.saturating_sub(1);
    }
}

/// Free the temporary register held by `e` if any.
///
/// Mirrors C's `freeexp` from `lcode.c`: only `VNONRELOC` carries a concrete
/// register that may need releasing.
fn cg_free_exp(fs: &mut FuncState, e: &ExprDesc) {
    if e.k == ExprKind::NonReloc {
        cg_free_reg(fs, e.u.info);
    }
}

/// Free temporary registers held by `e1` and `e2`, releasing the higher
/// register first so the LIFO invariant on `fs.freereg` holds.
///
/// Mirrors C's `freeexps` from `lcode.c`.
fn cg_free_exps(fs: &mut FuncState, e1: &ExprDesc, e2: &ExprDesc) {
    let r1 = if e1.k == ExprKind::NonReloc { e1.u.info } else { -1 };
    let r2 = if e2.k == ExprKind::NonReloc { e2.u.info } else { -1 };
    if r1 > r2 {
        cg_free_reg(fs, r1);
        cg_free_reg(fs, r2);
    } else {
        cg_free_reg(fs, r2);
        cg_free_reg(fs, r1);
    }
}

/// Constant-folding `luaK_posfix` for arithmetic binary operators where both
/// operands are already numeric literals (`KInt` / `KFlt`). Mirrors the
/// `constfolding` branch in C's `luaK_posfix`: when both operands are
/// numerals, the result is computed at compile time and stored back into
/// `e1`. Non-foldable arithmetic / bitwise binops fall through to the
/// two-register emit path (`OP_ADD` ... `OP_SHR`) plus an `OP_MMBIN`
/// metamethod-dispatch instruction. `Concat` is delegated to
/// `cg_emit_concat`; comparisons to `cg_emit_order` / `cg_emit_eq`;
/// `And` / `Or` short-circuit jumps to `cg_concat`.
fn cg_posfix_fold(
    fs: &mut FuncState,
    op: BinOpr,
    e1: &mut ExprDesc,
    e2: &mut ExprDesc,
    line: i32,
) -> Result<(), LuaError> {
    // Lua C records line info at emit time from `ls->lastline`. By the time
    // postfix code runs, the RHS has already been parsed, so discharging RHS
    // indexed expressions must use the current token line, not the saved
    // operator line. The operator line is still used below for the binop/MMBIN
    // instructions themselves.
    let rhs_line = fs.last_token_line;
    cg_discharge_vars(fs, rhs_line, e2)?;

    let promote = |k: ExprKind, u: &ExprPayload| -> Option<f64> {
        match k {
            ExprKind::KInt => Some(u.ival as f64),
            ExprKind::KFlt => Some(u.nval),
            _ => None,
        }
    };

    let foldable = e1.t == NO_JUMP && e1.f == NO_JUMP
        && e2.t == NO_JUMP && e2.f == NO_JUMP;

    if foldable {
    if let (ExprKind::KInt, ExprKind::KInt) = (e1.k, e2.k) {
        let a = e1.u.ival;
        let b = e2.u.ival;
        let r: Option<i64> = match op {
            BinOpr::Add => Some(a.wrapping_add(b)),
            BinOpr::Sub => Some(a.wrapping_sub(b)),
            BinOpr::Mul => Some(a.wrapping_mul(b)),
            BinOpr::Mod if b != 0 => Some(a.rem_euclid(b)),
            BinOpr::IDiv if b != 0 => Some(a.div_euclid(b)),
            BinOpr::BAnd => Some(a & b),
            BinOpr::BOr  => Some(a | b),
            BinOpr::BXor => Some(a ^ b),
            _ => None,
        };
        if let Some(v) = r {
            e1.k = ExprKind::KInt;
            e1.u.ival = v;
            return Ok(());
        }
    }
    if let (Some(a), Some(b)) = (promote(e1.k, &e1.u), promote(e2.k, &e2.u)) {
        let r: Option<f64> = match op {
            BinOpr::Add => Some(a + b),
            BinOpr::Sub => Some(a - b),
            BinOpr::Mul => Some(a * b),
            BinOpr::Div => Some(a / b),
            BinOpr::Pow => Some(a.powf(b)),
            _ => None,
        };
        if let Some(v) = r {
            if v.is_finite() {
                e1.k = ExprKind::KFlt;
                e1.u.nval = v;
                return Ok(());
            }
        }
    }
    }

    if matches!(op, BinOpr::Lt | BinOpr::Le) {
        return cg_emit_order(fs, op, e1, e2, line);
    }

    if matches!(op, BinOpr::Gt | BinOpr::Ge) {
        let swap_op = if matches!(op, BinOpr::Gt) { BinOpr::Lt } else { BinOpr::Le };
        std::mem::swap(e1, e2);
        return cg_emit_order(fs, swap_op, e1, e2, line);
    }

    if matches!(op, BinOpr::Eq | BinOpr::Ne) {
        return cg_emit_eq(fs, op, e1, e2, line);
    }

    if matches!(op, BinOpr::And) {
        debug_assert_eq!(e1.t, NO_JUMP);
        cg_concat(fs, &mut e2.f, e1.f)?;
        *e1 = e2.clone();
        return Ok(());
    }

    if matches!(op, BinOpr::Or) {
        debug_assert_eq!(e1.f, NO_JUMP);
        cg_concat(fs, &mut e2.t, e1.t)?;
        *e1 = e2.clone();
        return Ok(());
    }

    if matches!(op, BinOpr::Concat) {
        return cg_emit_concat(fs, e1, e2, line);
    }

    let (opcode, event) = match op {
        BinOpr::Add  => (lua_code::opcodes::OpCode::Add,  lua_types::tagmethod::TagMethod::Add),
        BinOpr::Sub  => (lua_code::opcodes::OpCode::Sub,  lua_types::tagmethod::TagMethod::Sub),
        BinOpr::Mul  => (lua_code::opcodes::OpCode::Mul,  lua_types::tagmethod::TagMethod::Mul),
        BinOpr::Mod  => (lua_code::opcodes::OpCode::Mod,  lua_types::tagmethod::TagMethod::Mod),
        BinOpr::Pow  => (lua_code::opcodes::OpCode::Pow,  lua_types::tagmethod::TagMethod::Pow),
        BinOpr::Div  => (lua_code::opcodes::OpCode::Div,  lua_types::tagmethod::TagMethod::Div),
        BinOpr::IDiv => (lua_code::opcodes::OpCode::IDiv, lua_types::tagmethod::TagMethod::Idiv),
        BinOpr::BAnd => (lua_code::opcodes::OpCode::BAnd, lua_types::tagmethod::TagMethod::Band),
        BinOpr::BOr  => (lua_code::opcodes::OpCode::BOr,  lua_types::tagmethod::TagMethod::Bor),
        BinOpr::BXor => (lua_code::opcodes::OpCode::BXor, lua_types::tagmethod::TagMethod::Bxor),
        BinOpr::Shl  => (lua_code::opcodes::OpCode::Shl,  lua_types::tagmethod::TagMethod::Shl),
        BinOpr::Shr  => (lua_code::opcodes::OpCode::Shr,  lua_types::tagmethod::TagMethod::Shr),
        BinOpr::Concat | BinOpr::Eq | BinOpr::Lt | BinOpr::Le | BinOpr::Ne
        | BinOpr::Gt | BinOpr::Ge | BinOpr::And | BinOpr::Or | BinOpr::NoBinOpr => {
            unreachable!("cg_posfix_fold reached opcode match with non-arith op {:?}", op)
        }
    };

    cg_discharge_vars(fs, line, e1)?;
    cg_discharge_vars(fs, line, e2)?;
    let v2 = cg_exp_to_any_reg(fs, line, e2)?;
    let v1 = cg_exp_to_any_reg(fs, line, e1)?;

    let inst = lua_code::opcodes::Instruction::abck(opcode, 0, v1 as u32, v2 as u32, 0);
    let pc = emit_inst(fs, line, inst);
    cg_free_exps(fs, e1, e2);
    e1.u.info = pc;
    e1.k = ExprKind::Reloc;

    let mm_inst = lua_code::opcodes::Instruction::abck(
        lua_code::opcodes::OpCode::MmBin,
        v1 as u32,
        v2 as u32,
        event as u32,
        0,
    );
    emit_inst(fs, line, mm_inst);
    Ok(())
}

/// Mirrors C's `codeorder` from `lcode.c` for relational binops (`<`, `<=`,
/// `>`, `>=`). Emits a comparison opcode (with `k = 1`) followed by an
/// `OP_JMP` with offset `NO_JUMP`; the resulting `VJMP` expression carries
/// the jump's pc in `e1.u.info` so the surrounding control-flow logic can
/// patch it. When one operand is a small-integer literal that fits the
/// signed-C field, the immediate forms (`OP_LTI` / `OP_GTI`) are used;
/// otherwise both operands are discharged to registers and the register
/// form (`OP_LT`) is emitted.
fn cg_emit_order(
    fs: &mut FuncState,
    op: BinOpr,
    e1: &mut ExprDesc,
    e2: &mut ExprDesc,
    line: i32,
) -> Result<(), LuaError> {
    debug_assert!(matches!(op, BinOpr::Lt | BinOpr::Le));
    let is_le = matches!(op, BinOpr::Le);
    let (op_imm_e2, op_imm_e1, op_reg) = if is_le {
        (
            lua_code::opcodes::OpCode::LeI,
            lua_code::opcodes::OpCode::GeI,
            lua_code::opcodes::OpCode::Le,
        )
    } else {
        (
            lua_code::opcodes::OpCode::LtI,
            lua_code::opcodes::OpCode::GtI,
            lua_code::opcodes::OpCode::Lt,
        )
    };
    let (r1, r2, cmp_op) = if let Some(im) = cg_sc_int(e2) {
        let r1 = cg_exp_to_any_reg(fs, line, e1)?;
        (r1, im, op_imm_e2)
    } else if let Some(im) = cg_sc_int(e1) {
        let r1 = cg_exp_to_any_reg(fs, line, e2)?;
        (r1, im, op_imm_e1)
    } else {
        let r2 = cg_exp_to_any_reg(fs, line, e2)?;
        let r1 = cg_exp_to_any_reg(fs, line, e1)?;
        (r1, r2, op_reg)
    };
    cg_free_exps(fs, e1, e2);
    let cmp = lua_code::opcodes::Instruction::abck(
        cmp_op,
        r1 as u32,
        r2 as u32,
        0,
        1,
    );
    emit_inst(fs, line, cmp);
    let jmp_arg = (NO_JUMP + lua_code::opcodes::OFFSET_S_J) as u32;
    let jmp = lua_code::opcodes::Instruction::sj(
        lua_code::opcodes::OpCode::Jmp,
        jmp_arg,
        0,
    );
    let jmp_pc = emit_inst(fs, line, jmp);
    e1.u.info = jmp_pc;
    e1.k = ExprKind::Jmp;
    Ok(())
}

/// Mirrors C's `codeeq` from `lcode.c` for the equality binops (`==`, `~=`).
/// Emits an `OP_EQ` (or its `OP_EQI` immediate form when the right operand
/// is a small-integer literal) followed by an `OP_JMP` whose pc is stored
/// in `e1.u.info`; `e1.k` becomes `VJMP`. The `k` bit selects between `==`
/// (k=1) and `~=` (k=0) so the same opcode pair handles both operators.
///
/// The Phase-A bootstrap deliberately omits the constant-table (`OP_EQK`)
/// fast path used by C; both operands fall back to register form when no
/// signed-C immediate fits. Correctness is unchanged.
fn cg_emit_eq(
    fs: &mut FuncState,
    op: BinOpr,
    e1: &mut ExprDesc,
    e2: &mut ExprDesc,
    line: i32,
) -> Result<(), LuaError> {
    debug_assert!(matches!(op, BinOpr::Eq | BinOpr::Ne));
    if e1.k != ExprKind::NonReloc {
        std::mem::swap(e1, e2);
    }
    let r1 = cg_exp_to_any_reg(fs, line, e1)?;
    let (r2, cmp_op) = if let Some(im) = cg_sc_int(e2) {
        (im, lua_code::opcodes::OpCode::EqI)
    } else {
        let r = cg_exp_to_any_reg(fs, line, e2)?;
        (r, lua_code::opcodes::OpCode::Eq)
    };
    cg_free_exps(fs, e1, e2);
    let k_bit = if matches!(op, BinOpr::Eq) { 1 } else { 0 };
    let cmp = lua_code::opcodes::Instruction::abck(
        cmp_op,
        r1 as u32,
        r2 as u32,
        0,
        k_bit,
    );
    emit_inst(fs, line, cmp);
    let jmp_pc = cg_jump(fs, line);
    e1.u.info = jmp_pc;
    e1.k = ExprKind::Jmp;
    Ok(())
}

/// Mirrors C's `previousinstruction` from `lcode.c`: returns the index of the
/// last emitted instruction, but only when `pc` is past `lasttarget` (i.e. the
/// previous instruction is reachable without crossing a jump label). Used by
/// peephole merges such as the `OP_CONCAT` chain fold.
fn previous_instruction_idx(fs: &FuncState) -> Option<usize> {
    if fs.pc > fs.lasttarget {
        Some((fs.pc - 1) as usize)
    } else {
        None
    }
}

/// Mirrors C's `codeconcat` from `lcode.c`. The left operand `e1` has
/// already been placed on the stack by `cg_infix`'s `OPR_CONCAT` arm
/// (`luaK_exp2nextreg`); here we only push `e2` onto the next register and
/// emit (or fold into) the `OP_CONCAT`. When the previous instruction is
/// itself an `OP_CONCAT` whose `A` register is exactly `e1.u.info + 1`,
/// the chain is merged by widening that instruction's `B` field;
/// otherwise a fresh `OP_CONCAT A=e1.u.info, B=2` is emitted. In both
/// branches the temporary register holding `e2` is freed.
fn cg_emit_concat(
    fs: &mut FuncState,
    e1: &mut ExprDesc,
    e2: &mut ExprDesc,
    line: i32,
) -> Result<(), LuaError> {
    cg_exp_to_next_reg(fs, line, e2)?;

    if let Some(prev_idx) = previous_instruction_idx(fs) {
        let prev = lua_code::opcodes::Instruction(fs.f.code[prev_idx].0);
        if prev.opcode() == Some(lua_code::opcodes::OpCode::Concat) {
            let n = prev.arg_b();
            debug_assert_eq!(e1.u.info + 1, prev.arg_a() as i32);
            cg_free_exp(fs, e2);
            let mut updated = prev;
            updated.set_arg_a(e1.u.info as u32);
            updated.set_arg_b(n + 1);
            fs.f.code[prev_idx] = lua_types::opcode::Instruction::new(updated.0);
            return Ok(());
        }
    }

    let inst = lua_code::opcodes::Instruction::abck(
        lua_code::opcodes::OpCode::Concat,
        e1.u.info as u32,
        2,
        0,
        0,
    );
    emit_inst(fs, line, inst);
    cg_free_exp(fs, e2);
    Ok(())
}

/// Mirrors C's `luaK_prefix` from `lcode.c`. Discharges `e`, then for
/// `Minus` / `BNot` / `Len` emits the unary opcode via `codeunexpval`
/// (place operand in a register, emit `OP_UNM` / `OP_BNOT` / `OP_LEN`
/// with `A` left as 0 so the result is relocatable). Constant folding
/// for `Minus` / `BNot` is skipped here; the runtime falls back to the
/// register form, matching C semantics (just less efficient). `Not`
/// is routed through `cg_codenot`, which performs literal folding,
/// JMP-condition flipping, or emits `OP_NOT` for register operands.
fn cg_prefix(
    fs: &mut FuncState,
    op: UnOpr,
    e: &mut ExprDesc,
    line: i32,
) -> Result<(), LuaError> {
    cg_discharge_vars(fs, line, e)?;
    let opcode = match op {
        UnOpr::Minus => lua_code::opcodes::OpCode::Unm,
        UnOpr::BNot  => lua_code::opcodes::OpCode::BNot,
        UnOpr::Len   => lua_code::opcodes::OpCode::Len,
        UnOpr::Not   => return cg_codenot(fs, line, e),
        UnOpr::NoUnOpr => return Ok(()),
    };
    let r = cg_exp_to_any_reg(fs, line, e)?;
    cg_free_exp(fs, e);
    let inst = lua_code::opcodes::Instruction::abck(opcode, 0, r as u32, 0, 0);
    let pc = emit_inst(fs, line, inst);
    e.u.info = pc;
    e.k = ExprKind::Reloc;
    Ok(())
}

/// Return the pc of the test instruction that controls the jump at `pc`,
/// or `pc` itself if the jump is unconditional.
///
/// Mirrors C's `getjumpcontrol` from `lcode.c`: when `pc >= 1` and the
/// preceding opcode has the T-mode bit set (i.e. it's a test that is always
/// paired with a following `OP_JMP`), the control lives at `pc - 1`.
fn cg_get_jump_control(fs: &FuncState, pc: i32) -> i32 {
    if pc >= 1 {
        let prev = cg_inst_at(fs, pc - 1);
        if let Some(op) = prev.opcode() {
            if lua_code::opcodes::test_t_mode(op) {
                return pc - 1;
            }
        }
    }
    pc
}

/// Patch the destination register of a `TESTSET` that controls the jump at
/// `node`. If the control isn't a `TESTSET`, returns `false`. With `reg ==
/// NO_REG` (or when `reg` already equals B), the instruction is rewritten to
/// a plain `OP_TEST` (preserving the original `k` bit) — the test no longer
/// produces a value.
///
/// Mirrors C's `patchtestreg` from `lcode.c`.
fn cg_patch_test_reg(fs: &mut FuncState, node: i32, reg: u32) -> bool {
    let ctrl_pc = cg_get_jump_control(fs, node);
    let mut inst = cg_inst_at(fs, ctrl_pc);
    if inst.opcode() != Some(lua_code::opcodes::OpCode::TestSet) {
        return false;
    }
    let b = inst.arg_b();
    let k = inst.arg_k();
    if reg != lua_code::opcodes::NO_REG && reg != b {
        inst.set_arg_a(reg);
        cg_set_inst_at(fs, ctrl_pc, inst);
    } else {
        let test = lua_code::opcodes::Instruction::abck(
            lua_code::opcodes::OpCode::Test,
            b,
            0,
            0,
            k,
        );
        cg_set_inst_at(fs, ctrl_pc, test);
    }
    true
}

/// Walk the jump-list rooted at `list` and strip every `TESTSET` of its
/// destination register, leaving plain `OP_TEST`s behind. Used after
/// `not <expr>` swaps `e.t` / `e.f`: any pending value-producing tests in
/// the new lists would write the unnegated value, which is wrong.
///
/// Mirrors C's `removevalues` from `lcode.c`.
fn cg_remove_values(fs: &mut FuncState, list: i32) {
    let mut list = list;
    while list != NO_JUMP {
        let next = cg_get_jump(fs, list);
        cg_patch_test_reg(fs, list, lua_code::opcodes::NO_REG);
        list = next;
    }
}

/// Mirrors C's `codenot` from `lcode.c`. Handles constant folding for `not`
/// (nil/false → true; any other constant → false), flips the condition bit
/// of a jump-result expression, or emits `OP_NOT` for in-register operands.
/// After negation, `e.t` and `e.f` are swapped (the old true-exit list now
/// fires when the negated value is false, and vice versa) and any
/// value-producing tests in the new lists are downgraded to plain tests via
/// `cg_remove_values`.
fn cg_codenot(fs: &mut FuncState, line: i32, e: &mut ExprDesc) -> Result<(), LuaError> {
    match e.k {
        ExprKind::Nil | ExprKind::False => {
            e.k = ExprKind::True;
        }
        ExprKind::K
        | ExprKind::KFlt
        | ExprKind::KInt
        | ExprKind::KStr
        | ExprKind::True => {
            e.k = ExprKind::False;
        }
        ExprKind::Jmp => {
            cg_negate_condition(fs, e);
        }
        ExprKind::Reloc | ExprKind::NonReloc => {
            let reg = cg_exp_to_any_reg(fs, line, e)?;
            cg_free_exp(fs, e);
            let inst = lua_code::opcodes::Instruction::abck(
                lua_code::opcodes::OpCode::Not,
                0,
                reg as u32,
                0,
                0,
            );
            let pc = emit_inst(fs, line, inst);
            e.u.info = pc;
            e.k = ExprKind::Reloc;
        }
        _ => debug_assert!(false, "cg_codenot: unexpected ExprKind {:?}", e.k),
    }
    std::mem::swap(&mut e.f, &mut e.t);
    cg_remove_values(fs, e.f);
    cg_remove_values(fs, e.t);
    Ok(())
}

/// Emit OP_JMP with NO_JUMP offset; return its pc.
///
/// Mirrors C's `luaK_jump`.
fn cg_jump(fs: &mut FuncState, line: i32) -> i32 {
    let jmp_arg = (NO_JUMP + lua_code::opcodes::OFFSET_S_J) as u32;
    let jmp = lua_code::opcodes::Instruction::sj(
        lua_code::opcodes::OpCode::Jmp,
        jmp_arg,
        0,
    );
    emit_inst(fs, line, jmp)
}

/// Read an instruction word from `fs.f.code` wrapped in the methodful
/// `lua_code::opcodes::Instruction` so accessor helpers are available.
fn cg_inst_at(fs: &FuncState, pc: i32) -> lua_code::opcodes::Instruction {
    lua_code::opcodes::Instruction(fs.f.code[pc as usize].0)
}

/// Store an instruction word into `fs.f.code` from a methodful
/// `lua_code::opcodes::Instruction`.
fn cg_set_inst_at(fs: &mut FuncState, pc: i32, inst: lua_code::opcodes::Instruction) {
    fs.f.code[pc as usize] = lua_types::opcode::Instruction::new(inst.0);
}

/// Return the absolute pc that the jump at `pc` targets, or `NO_JUMP` if the
/// jump's offset field is still the sentinel.
///
/// Mirrors C's `getjump` from `lcode.c`.
fn cg_get_jump(fs: &FuncState, pc: i32) -> i32 {
    let offset = cg_inst_at(fs, pc).arg_s_j();
    if offset == NO_JUMP { NO_JUMP } else { (pc + 1) + offset }
}

/// Patch the jump at `pc` to land at absolute `dest`.
///
/// Mirrors C's `fixjump` from `lcode.c`.
fn cg_fix_jump(fs: &mut FuncState, pc: i32, dest: i32) -> Result<(), LuaError> {
    debug_assert!(dest != NO_JUMP);
    let offset = dest - (pc + 1);
    let max = lua_code::opcodes::MAXARG_S_J as i32 - lua_code::opcodes::OFFSET_S_J;
    let min = -lua_code::opcodes::OFFSET_S_J;
    if offset < min || offset > max {
        return Err(LuaError::syntax(format_args!("control structure too long")));
    }
    let mut inst = cg_inst_at(fs, pc);
    inst.set_arg_s_j(offset);
    cg_set_inst_at(fs, pc, inst);
    Ok(())
}

/// Record `fs.pc` as a jump label and return it.
///
/// Mirrors C's `luaK_getlabel` from `lcode.c`.
fn cg_get_label(fs: &mut FuncState) -> i32 {
    fs.lasttarget = fs.pc;
    fs.pc
}

/// Concatenate jump-list `l2` onto the tail of `*l1`.
///
/// Mirrors C's `luaK_concat` from `lcode.c`.
fn cg_concat(fs: &mut FuncState, l1: &mut i32, l2: i32) -> Result<(), LuaError> {
    if l2 == NO_JUMP { return Ok(()); }
    if *l1 == NO_JUMP { *l1 = l2; return Ok(()); }
    let mut list = *l1;
    loop {
        let next = cg_get_jump(fs, list);
        if next == NO_JUMP { break; }
        list = next;
    }
    cg_fix_jump(fs, list, l2)
}

/// Patch every jump in the singly-linked list rooted at `list` to land at
/// absolute pc `target`.
///
/// Mirrors C's `luaK_patchlist`, which delegates to `patchlistaux(fs, list,
/// target, NO_REG, target)`: every `TESTSET` controller in the list gets
/// rewritten to a plain `OP_TEST` (the value-producing destination register
/// is no longer wanted at a fall-through target), and every jump is fixed to
/// `target`.
fn cg_patch_list(fs: &mut FuncState, list: i32, target: i32) -> Result<(), LuaError> {
    cg_patch_list_aux(fs, list, target, lua_code::opcodes::NO_REG, target)
}

/// Patch every jump in `list` to land at the current `fs.pc`.
///
/// Mirrors C's `luaK_patchtohere` from `lcode.c`.
fn cg_patch_to_here(fs: &mut FuncState, list: i32) -> Result<(), LuaError> {
    let target = cg_get_label(fs);
    cg_patch_list(fs, list, target)
}

/// Flip the `k` (condition) bit of the test instruction that immediately
/// precedes `e`'s JMP. After this, the jump fires on the opposite truth
/// value of the original comparison.
///
/// Mirrors C's `negatecondition` from `lcode.c`.
fn cg_negate_condition(fs: &mut FuncState, e: &ExprDesc) {
    let pc = e.u.info - 1;
    let mut inst = cg_inst_at(fs, pc);
    let k = inst.arg_k();
    inst.set_arg_k(k ^ 1);
    cg_set_inst_at(fs, pc, inst);
}

/// Arrange for control to fall through when `e` is true and to jump (via the
/// patch list rooted at `e.f`) when `e` is false. After this call `e.t` has
/// been patched to the current pc and `e.f` holds the false-exit list.
///
/// Mirrors C's `luaK_goiftrue` from `lcode.c`. `VJMP` (comparison results)
/// negate the condition so the jump fires on false; literal-true forms emit
/// no jump; any other kind is forced into a register and tested with
/// `OP_TESTSET` via `cg_jump_on_cond`.
fn cg_go_if_true(fs: &mut FuncState, line: i32, e: &mut ExprDesc) -> Result<(), LuaError> {
    cg_discharge_vars(fs, line, e)?;
    let pc: i32 = match e.k {
        ExprKind::Jmp => {
            cg_negate_condition(fs, e);
            e.u.info
        }
        ExprKind::K | ExprKind::KFlt | ExprKind::KInt | ExprKind::KStr | ExprKind::True => {
            NO_JUMP
        }
        _ => cg_jump_on_cond(fs, line, e, 0)?,
    };
    cg_concat(fs, &mut e.f, pc)?;
    cg_patch_to_here(fs, e.t)?;
    e.t = NO_JUMP;
    Ok(())
}

/// Mirror of `cg_go_if_true` for false short-circuit (`or` operator and
/// `while not <cond>` shaped control flow). Falls through when `e` is false
/// and jumps when true. After this call `e.f` has been patched to the
/// current pc and `e.t` holds the true-exit list.
///
/// Mirrors C's `luaK_goiffalse` from `lcode.c`.
fn cg_go_if_false(fs: &mut FuncState, line: i32, e: &mut ExprDesc) -> Result<(), LuaError> {
    cg_discharge_vars(fs, line, e)?;
    let pc: i32 = match e.k {
        ExprKind::Jmp => e.u.info,
        ExprKind::Nil | ExprKind::False => NO_JUMP,
        _ => cg_jump_on_cond(fs, line, e, 1)?,
    };
    cg_concat(fs, &mut e.t, pc)?;
    cg_patch_to_here(fs, e.f)?;
    e.f = NO_JUMP;
    Ok(())
}

/// Emit `OP_TESTSET R[NO_REG], R[e.info], cond` followed by an `OP_JMP` so
/// control transfers to the jump's patch list when `e`'s truth value equals
/// `cond`. Returns the pc of the emitted jump so the caller can append it
/// to the appropriate exit list.
///
/// Mirrors C's `jumponcond` from `lcode.c`. The `OP_NOT` peephole that C
/// applies for `VRELOC` operands is intentionally skipped for the Phase-A
/// bootstrap; correctness is unaffected and the optimisation can land with
/// the codegen reconciliation pass.
fn cg_jump_on_cond(
    fs: &mut FuncState,
    line: i32,
    e: &mut ExprDesc,
    cond: u8,
) -> Result<i32, LuaError> {
    let reg = cg_exp_to_any_reg(fs, line, e)?;
    cg_free_exp(fs, e);
    let test = lua_code::opcodes::Instruction::abck(
        lua_code::opcodes::OpCode::TestSet,
        lua_code::opcodes::NO_REG,
        reg as u32,
        0,
        cond as u32,
    );
    emit_inst(fs, line, test);
    Ok(cg_jump(fs, line))
}

/// First half of `luaK_posfix`: pre-process the left operand `v` of a binary
/// operator before the right operand is parsed. Mirrors C's `luaK_infix`
/// from `lcode.c`. The codegen reconciliation has not yet routed parser
/// calls through `lua_code::infix`, so this lives in the parser file
/// alongside the other `cg_*` helpers.
///
/// For `And`/`Or` the operand is converted into a short-circuit form (jump
/// list closed via `cg_go_if_true` / `cg_go_if_false`). For `Concat` it is
/// pushed onto the next register. Other arithmetic, bitwise, and comparison
/// operators rely on `cg_posfix_fold` to discharge their operands after the
/// right-hand side is known, so `cg_infix` only calls `cg_discharge_vars`
/// for them.
fn cg_infix(
    fs: &mut FuncState,
    op: BinOpr,
    v: &mut ExprDesc,
    line: i32,
) -> Result<(), LuaError> {
    match op {
        BinOpr::And => cg_go_if_true(fs, line, v),
        BinOpr::Or => cg_go_if_false(fs, line, v),
        BinOpr::Concat => cg_exp_to_next_reg(fs, line, v),
        BinOpr::Add | BinOpr::Sub | BinOpr::Mul | BinOpr::Div | BinOpr::IDiv
        | BinOpr::Mod | BinOpr::Pow
        | BinOpr::BAnd | BinOpr::BOr | BinOpr::BXor
        | BinOpr::Shl | BinOpr::Shr
        | BinOpr::Eq | BinOpr::Ne
        | BinOpr::Lt | BinOpr::Le | BinOpr::Gt | BinOpr::Ge => {
            if matches!(v.k, ExprKind::KInt | ExprKind::KFlt)
                && v.t == NO_JUMP && v.f == NO_JUMP
            {
                cg_discharge_vars(fs, line, v)
            } else {
                cg_exp_to_any_reg(fs, line, v).map(|_| ())
            }
        }
        _ => cg_discharge_vars(fs, line, v),
    }
}

/// Mirrors C's `isSCint` from `lcode.c` (a restriction of `isSCnumber` to
/// the integer case): returns `Some(int2sC(ival))` if `e` is a `VKINT`
/// literal whose value fits the signed-C 8-bit operand field, else `None`.
/// The returned byte is already pre-encoded with the `OFFSET_sC` bias so
/// the caller can drop it straight into an `sC` argument slot.
fn cg_sc_int(e: &ExprDesc) -> Option<u8> {
    if !matches!(e.k, ExprKind::KInt) {
        return None;
    }
    if e.t != NO_JUMP || e.f != NO_JUMP {
        return None;
    }
    let biased = (e.u.ival as u64).wrapping_add(lua_code::opcodes::OFFSET_S_C as u64);
    if biased <= lua_code::opcodes::MAXARG_C as u64 {
        Some(biased as u8)
    } else {
        None
    }
}

/// Minimal `luaK_exp2anyreg`: ensure `e` ends up in *some* register. If `e`
/// is already `VNONRELOC` and its register is at or above `nactvar`, keep it
/// there; otherwise discharge to the next free register.
fn cg_exp_to_any_reg(
    fs: &mut FuncState,
    line: i32,
    e: &mut ExprDesc,
) -> Result<u8, LuaError> {
    cg_discharge_vars(fs, line, e)?;
    if e.k == ExprKind::NonReloc {
        if e.t == NO_JUMP && e.f == NO_JUMP {
            return Ok(e.u.info as u8);
        }
        if e.u.info >= fs.nactvar as i32 {
            cg_exp_to_reg(fs, line, e, e.u.info as u8)?;
            return Ok(e.u.info as u8);
        }
    }
    cg_exp_to_next_reg(fs, line, e)?;
    Ok(e.u.info as u8)
}

/// Minimal `luaK_dischargevars` covering the cases the parser bootstrap can
/// produce: `VLOCAL`, `VUpVal`, `VIndexUp`, `VKStr`. Other variants are left
/// untouched. Returns Ok(()) on success.
fn cg_discharge_vars(
    fs: &mut FuncState,
    line: i32,
    e: &mut ExprDesc,
) -> Result<(), LuaError> {
    match e.k {
        ExprKind::Local => {
            e.u.info = e.u.var_ridx as i32;
            e.k = ExprKind::NonReloc;
        }
        ExprKind::UpVal => {
            let inst = lua_code::opcodes::Instruction::abck(
                lua_code::opcodes::OpCode::GetUpVal,
                0,
                e.u.info as u32,
                0,
                0,
            );
            let pc = emit_inst(fs, line, inst);
            e.u.info = pc;
            e.k = ExprKind::Reloc;
        }
        ExprKind::IndexUp => {
            let inst = lua_code::opcodes::Instruction::abck(
                lua_code::opcodes::OpCode::GetTabUp,
                0,
                e.u.ind_t as u32,
                e.u.ind_idx as u32,
                0,
            );
            let pc = emit_inst(fs, line, inst);
            e.u.info = pc;
            e.k = ExprKind::Reloc;
        }
        ExprKind::IndexI => {
            cg_free_reg_if_temp(fs, e.u.ind_t as i32);
            let inst = lua_code::opcodes::Instruction::abck(
                lua_code::opcodes::OpCode::GetI,
                0,
                e.u.ind_t as u32,
                e.u.ind_idx as u32,
                0,
            );
            let pc = emit_inst(fs, line, inst);
            e.u.info = pc;
            e.k = ExprKind::Reloc;
        }
        ExprKind::IndexStr => {
            cg_free_reg_if_temp(fs, e.u.ind_t as i32);
            let inst = lua_code::opcodes::Instruction::abck(
                lua_code::opcodes::OpCode::GetField,
                0,
                e.u.ind_t as u32,
                e.u.ind_idx as u32,
                0,
            );
            let pc = emit_inst(fs, line, inst);
            e.u.info = pc;
            e.k = ExprKind::Reloc;
        }
        ExprKind::Indexed => {
            let t_reg = e.u.ind_t as i32;
            let idx_reg = e.u.ind_idx as i32;
            if idx_reg > t_reg {
                cg_free_reg_if_temp(fs, idx_reg);
                cg_free_reg_if_temp(fs, t_reg);
            } else {
                cg_free_reg_if_temp(fs, t_reg);
                cg_free_reg_if_temp(fs, idx_reg);
            }
            let inst = lua_code::opcodes::Instruction::abck(
                lua_code::opcodes::OpCode::GetTable,
                0,
                e.u.ind_t as u32,
                e.u.ind_idx as u32,
                0,
            );
            let pc = emit_inst(fs, line, inst);
            e.u.info = pc;
            e.k = ExprKind::Reloc;
        }
        ExprKind::VarArg | ExprKind::Call => {
            cg_set_one_ret(fs, e);
        }
        _ => {}
    }
    Ok(())
}

/// result. For a Call this leaves the already-emitted instruction alone (it
/// was emitted with `ARG_C = 2`, i.e. exactly one result) and reclassifies
/// `e` as `NonReloc` pointing at the result register (the Call's `ARG_A`).
/// For a VarArg this patches `ARG_C = 2` and leaves `e` as `Reloc` so the
/// caller can place the single result into a destination register.
fn cg_set_one_ret(fs: &mut FuncState, e: &mut ExprDesc) {
    if e.k == ExprKind::Call {
        let pc_idx = e.u.info as usize;
        let lc = lua_code::opcodes::Instruction(fs.f.code[pc_idx].0);
        debug_assert_eq!(lc.arg_c(), 2);
        e.u.info = lc.arg_a() as i32;
        e.k = ExprKind::NonReloc;
    } else if e.k == ExprKind::VarArg {
        let pc_idx = e.u.info as usize;
        let mut lc = lua_code::opcodes::Instruction(fs.f.code[pc_idx].0);
        lc.set_arg_c(2);
        fs.f.code[pc_idx] = lua_types::opcode::Instruction::new(lc.0);
        e.k = ExprKind::Reloc;
    }
}

/// by `var`. Handles VLocal (move into register), VUpVal (OP_SETUPVAL),
/// VIndexUp (OP_SETTABUP), VIndexI/IndexStr/Indexed (OP_SETI/SETFIELD/SETTABLE).
fn cg_storevar(
    fs: &mut FuncState,
    line: i32,
    var: &ExprDesc,
    ex: &mut ExprDesc,
) -> Result<(), LuaError> {
    match var.k {
        ExprKind::Local => {
            cg_free_exp(fs, ex);
            cg_exp_to_reg(fs, line, ex, var.u.var_ridx as u8)?;
            return Ok(());
        }
        ExprKind::UpVal => {
            let e_reg = cg_exp_to_any_reg(fs, line, ex)?;
            let inst = lua_code::opcodes::Instruction::abck(
                lua_code::opcodes::OpCode::SetUpVal,
                e_reg as u32,
                var.u.info as u32,
                0,
                0,
            );
            emit_inst(fs, line, inst);
        }
        ExprKind::IndexUp => {
            cg_store_abrk(fs, line, lua_code::opcodes::OpCode::SetTabUp,
                var.u.ind_t as u32, var.u.ind_idx as u32, ex)?;
        }
        ExprKind::IndexI => {
            cg_store_abrk(fs, line, lua_code::opcodes::OpCode::SetI,
                var.u.ind_t as u32, var.u.ind_idx as u32, ex)?;
        }
        ExprKind::IndexStr => {
            cg_store_abrk(fs, line, lua_code::opcodes::OpCode::SetField,
                var.u.ind_t as u32, var.u.ind_idx as u32, ex)?;
        }
        ExprKind::Indexed => {
            cg_store_abrk(fs, line, lua_code::opcodes::OpCode::SetTable,
                var.u.ind_t as u32, var.u.ind_idx as u32, ex)?;
        }
        _ => {
            return Err(LuaError::syntax(format_args!(
                "internal: cg_storevar: invalid var kind {:?}", var.k
            )));
        }
    }
    cg_free_exp(fs, ex);
    Ok(())
}

/// Helper for cg_storevar: emit an ABRK-form store. Mirrors C's `codeABRK`
/// for the SetTabUp/SetI/SetField/SetTable family. When `ex` is a constant
/// the K bit is set; otherwise the value is forced into a register.
fn cg_store_abrk(
    fs: &mut FuncState,
    line: i32,
    op: lua_code::opcodes::OpCode,
    a: u32,
    b: u32,
    ex: &mut ExprDesc,
) -> Result<(), LuaError> {
    let c_reg = cg_exp_to_any_reg(fs, line, ex)?;
    let inst = lua_code::opcodes::Instruction::abck(op, a, b, c_reg as u32, 0);
    emit_inst(fs, line, inst);
    Ok(())
}

/// Mirrors C's `discharge2reg` from `lcode.c`: places the value described by
/// `e` into `reg`. For `Jmp` this is a no-op (the caller — `cg_exp_to_reg` —
/// is responsible for stitching the jump into `e.t` and emitting the
/// LoadTrue / LFalseSkip pair if a concrete value is needed).
fn cg_discharge_to_reg(
    fs: &mut FuncState,
    line: i32,
    e: &mut ExprDesc,
    reg: u8,
) -> Result<(), LuaError> {
    cg_discharge_vars(fs, line, e)?;
    match e.k {
        ExprKind::Jmp => {
            return Ok(());
        }
        ExprKind::NonReloc => {
            if e.u.info as u8 != reg {
                let inst = lua_code::opcodes::Instruction::abck(
                    lua_code::opcodes::OpCode::Move,
                    reg as u32,
                    e.u.info as u32,
                    0, 0,
                );
                emit_inst(fs, line, inst);
            }
        }
        ExprKind::Reloc => {
            let pc = e.u.info as usize;
            let mut lc = lua_code::opcodes::Instruction(fs.f.code[pc].0);
            lc.set_arg_a(reg as u32);
            fs.f.code[pc] = lua_types::opcode::Instruction::new(lc.0);
        }
        ExprKind::Nil => {
            let inst = lua_code::opcodes::Instruction::abck(
                lua_code::opcodes::OpCode::LoadNil, reg as u32, 0, 0, 0,
            );
            emit_inst(fs, line, inst);
        }
        ExprKind::True => {
            let inst = lua_code::opcodes::Instruction::abck(
                lua_code::opcodes::OpCode::LoadTrue, reg as u32, 0, 0, 0,
            );
            emit_inst(fs, line, inst);
        }
        ExprKind::False => {
            let inst = lua_code::opcodes::Instruction::abck(
                lua_code::opcodes::OpCode::LoadFalse, reg as u32, 0, 0, 0,
            );
            emit_inst(fs, line, inst);
        }
        ExprKind::KInt => {
            let i = e.u.ival;
            let max = lua_code::opcodes::MAXARG_BX as i64 - lua_code::opcodes::OFFSET_S_BX as i64;
            let min = -(lua_code::opcodes::OFFSET_S_BX as i64);
            if i >= min && i <= max {
                let bx = (i as i32 + lua_code::opcodes::OFFSET_S_BX) as u32;
                let inst = lua_code::opcodes::Instruction::abx(
                    lua_code::opcodes::OpCode::LoadI, reg as u32, bx,
                );
                emit_inst(fs, line, inst);
            } else {
                let k_idx = add_k_value(fs, LuaValue::Int(i));
                let inst = lua_code::opcodes::Instruction::abx(
                    lua_code::opcodes::OpCode::LoadK, reg as u32, k_idx as u32,
                );
                emit_inst(fs, line, inst);
            }
        }
        ExprKind::KFlt => {
            let f = e.u.nval;
            let max = lua_code::opcodes::MAXARG_BX as i64 - lua_code::opcodes::OFFSET_S_BX as i64;
            let min = -(lua_code::opcodes::OFFSET_S_BX as i64);
            let fi_opt: Option<i64> = if f.fract() == 0.0 && f.abs() < i64::MAX as f64 {
                Some(f as i64)
            } else {
                None
            };
            if let Some(fi) = fi_opt.filter(|fi| *fi >= min && *fi <= max) {
                let bx = (fi as i32 + lua_code::opcodes::OFFSET_S_BX) as u32;
                let inst = lua_code::opcodes::Instruction::abx(
                    lua_code::opcodes::OpCode::LoadF, reg as u32, bx,
                );
                emit_inst(fs, line, inst);
            } else {
                let k_idx = add_k_value(fs, LuaValue::Float(f));
                let inst = lua_code::opcodes::Instruction::abx(
                    lua_code::opcodes::OpCode::LoadK, reg as u32, k_idx as u32,
                );
                emit_inst(fs, line, inst);
            }
        }
        ExprKind::KStr => {
            let s = e.u.strval.clone()
                .ok_or_else(|| LuaError::syntax(format_args!("internal: VKStr with no strval")))?;
            let k_idx = add_k_string(fs, s);
            let inst = lua_code::opcodes::Instruction::abx(
                lua_code::opcodes::OpCode::LoadK,
                reg as u32,
                k_idx as u32,
            );
            emit_inst(fs, line, inst);
        }
        ExprKind::K => {
            let inst = lua_code::opcodes::Instruction::abx(
                lua_code::opcodes::OpCode::LoadK,
                reg as u32,
                e.u.info as u32,
            );
            emit_inst(fs, line, inst);
        }
        _ => {
            return Err(LuaError::syntax(format_args!(
                "internal: cg_discharge_to_reg cannot discharge {:?}", e.k
            )));
        }
    }
    e.u.info = reg as i32;
    e.k = ExprKind::NonReloc;
    Ok(())
}

/// Mirrors C's `need_value` from `lcode.c`: walks the jump-list `list` and
/// returns true if any controlling instruction is *not* an `OP_TESTSET`,
/// meaning a concrete LoadTrue / LFalseSkip pair must be emitted to provide
/// the value at the fallthrough.
fn cg_need_value(fs: &FuncState, list: i32) -> bool {
    let mut list = list;
    while list != NO_JUMP {
        let ctrl_pc = cg_get_jump_control(fs, list);
        let ctrl = cg_inst_at(fs, ctrl_pc);
        if ctrl.opcode() != Some(lua_code::opcodes::OpCode::TestSet) {
            return true;
        }
        list = cg_get_jump(fs, list);
    }
    false
}

/// Mirrors C's `code_loadbool` from `lcode.c`: records `fs.pc` as a jump
/// label, then emits the requested LoadTrue / LoadFalse / LFalseSkip
/// instruction and returns its pc.
fn cg_code_loadbool(fs: &mut FuncState, line: i32, reg: i32, op: lua_code::opcodes::OpCode) -> i32 {
    cg_get_label(fs);
    let inst = lua_code::opcodes::Instruction::abck(op, reg as u32, 0, 0, 0);
    emit_inst(fs, line, inst)
}

/// Mirrors C's `patchlistaux` from `lcode.c`: walks the jump-list `list`,
/// rewriting `TESTSET` controllers to write `reg` (and routing them to
/// `vtarget`) and leaving plain tests to fall through to `dtarget`.
fn cg_patch_list_aux(
    fs: &mut FuncState,
    list: i32,
    vtarget: i32,
    reg: u32,
    dtarget: i32,
) -> Result<(), LuaError> {
    let mut list = list;
    while list != NO_JUMP {
        let next = cg_get_jump(fs, list);
        if cg_patch_test_reg(fs, list, reg) {
            cg_fix_jump(fs, list, vtarget)?;
        } else {
            cg_fix_jump(fs, list, dtarget)?;
        }
        list = next;
    }
    Ok(())
}

/// Discharge `e` into the specific register `reg`. Mirrors C's `exp2reg`
/// from `lcode.c`: delegates to `cg_discharge_to_reg`, then folds the jump
/// at `e.u.info` into `e.t` (when `e` is itself a test) and patches any
/// pending `e.t` / `e.f` jump-lists. When the lists actually need a value
/// (i.e. any controller isn't a `TESTSET`), emits the LFalseSkip / LoadTrue
/// pair around which the jumps land.
fn cg_exp_to_reg(
    fs: &mut FuncState,
    line: i32,
    e: &mut ExprDesc,
    reg: u8,
) -> Result<(), LuaError> {
    cg_discharge_to_reg(fs, line, e, reg)?;
    if e.k == ExprKind::Jmp {
        let info = e.u.info;
        cg_concat(fs, &mut e.t, info)?;
    }
    if e.t != e.f {
        let mut p_f = NO_JUMP;
        let mut p_t = NO_JUMP;
        if cg_need_value(fs, e.t) || cg_need_value(fs, e.f) {
            let fj = if e.k == ExprKind::Jmp {
                NO_JUMP
            } else {
                cg_jump(fs, line)
            };
            p_f = cg_code_loadbool(fs, line, reg as i32, lua_code::opcodes::OpCode::LFalseSkip);
            p_t = cg_code_loadbool(fs, line, reg as i32, lua_code::opcodes::OpCode::LoadTrue);
            cg_patch_to_here(fs, fj)?;
        }
        let final_pc = cg_get_label(fs);
        cg_patch_list_aux(fs, e.f, final_pc, reg as u32, p_f)?;
        cg_patch_list_aux(fs, e.t, final_pc, reg as u32, p_t)?;
    }
    e.f = NO_JUMP;
    e.t = NO_JUMP;
    e.u.info = reg as i32;
    e.k = ExprKind::NonReloc;
    Ok(())
}

/// Like `cg_free_reg`, but only acts when the index actually belongs to a
/// temporary register (one above `fs.nactvar`). Used by indexed-get
/// dischargers, which may operate on either a temp result or a local.
fn cg_free_reg_if_temp(fs: &mut FuncState, reg: i32) {
    if reg >= fs.nactvar as i32 {
        debug_assert!(reg < fs.freereg as i32);
        if reg == fs.freereg as i32 - 1 {
            fs.freereg -= 1;
        }
    }
}

/// Mirrors C's `luaK_exp2nextreg` from `lcode.c`: discharge variable forms,
/// free any temp held by `e`, reserve the next register, then call
/// `cg_exp_to_reg` to place the value (handling `Jmp` and pending
/// `e.t` / `e.f` jump-lists through the shared `exp2reg` path).
fn cg_exp_to_next_reg(
    fs: &mut FuncState,
    line: i32,
    e: &mut ExprDesc,
) -> Result<(), LuaError> {
    cg_discharge_vars(fs, line, e)?;
    cg_free_exp(fs, e);
    let reg = reserve_reg(fs)?;
    cg_exp_to_reg(fs, line, e, reg)
}

/// it produces `nresults` values (or LUA_MULTRET when `nresults == -1`).
fn cg_set_returns(fs: &mut FuncState, e: &mut ExprDesc, nresults: i32) {
    let pc_idx = e.u.info as usize;
    let mut lc = lua_code::opcodes::Instruction(fs.f.code[pc_idx].0);
    if e.k == ExprKind::Call {
        lc.set_arg_c((nresults + 1) as u32);
    } else {
        debug_assert_eq!(e.k, ExprKind::VarArg);
        lc.set_arg_c((nresults + 1) as u32);
        lc.set_arg_a(fs.freereg as u32);
        fs.freereg += 1;
    }
    fs.f.code[pc_idx] = lua_types::opcode::Instruction::new(lc.0);
}

/// `OP_JMP` instructions to the final landing pc. Capped at 100 hops to
/// avoid infinite loops on malformed code.
fn cg_final_target(fs: &FuncState, mut i: i32) -> i32 {
    for _ in 0..100 {
        let inst = cg_inst_at(fs, i);
        if inst.opcode() != Some(lua_code::opcodes::OpCode::Jmp) {
            break;
        }
        i += inst.arg_s_j() + 1;
    }
    i
}

///
/// Patches `OP_RETURN`/`OP_RETURN0`/`OP_RETURN1`/`OP_TAILCALL` to record the
/// vararg signature (so the VM can roll back `ci->func` on return) and the
/// `needclose` flag (so it closes pending upvalues). Also resolves chained
/// `OP_JMP` jumps to their final target.
fn cg_finish(fs: &mut FuncState) {
    use lua_code::opcodes::OpCode;
    let needclose = fs.needclose;
    let is_vararg = fs.f.is_vararg;
    let numparams = fs.f.numparams as u32;
    let pc_end = fs.pc;
    for i in 0..pc_end {
        let mut inst = cg_inst_at(fs, i);
        match inst.opcode() {
            Some(OpCode::Return0) | Some(OpCode::Return1) => {
                if !(needclose || is_vararg) {
                    continue;
                }
                inst.set_opcode(OpCode::Return);
                if needclose {
                    inst.set_arg_k(1);
                }
                if is_vararg {
                    inst.set_arg_c(numparams + 1);
                }
                cg_set_inst_at(fs, i, inst);
            }
            Some(OpCode::Return) | Some(OpCode::TailCall) => {
                if needclose {
                    inst.set_arg_k(1);
                }
                if is_vararg {
                    inst.set_arg_c(numparams + 1);
                }
                cg_set_inst_at(fs, i, inst);
            }
            Some(OpCode::Jmp) => {
                let target = cg_final_target(fs, i);
                let _ = cg_fix_jump(fs, i, target);
            }
            _ => {}
        }
    }
}

/// based on `nret`. `first` is the first result register; `nret` is the
/// number of values to return (`LUA_MULTRET` for "all values on top").
fn cg_emit_return(fs: &mut FuncState, line: i32, first: i32, nret: i32) {
    let op = match nret {
        0 => lua_code::opcodes::OpCode::Return0,
        1 => lua_code::opcodes::OpCode::Return1,
        _ => lua_code::opcodes::OpCode::Return,
    };
    let inst = lua_code::opcodes::Instruction::abck(
        op,
        first as u32,
        (nret + 1) as u32,
        0,
        0,
    );
    emit_inst(fs, line, inst);
}

// ── Free functions ──────────────────────────────────────────────────────────

// (Both defined later in this file; Rust has no forward declarations.)

// ── §1 Error helpers ────────────────────────────────────────────────────────

/// Constructs a syntax error for a missing expected token.
/// In Rust, `l_noret` becomes returning `LuaError`; callers use
/// `return Err(error_expected(...))`.
fn error_expected(ls: &mut LexState, token: TokenKind) -> LuaError {
    let tok_str = lua_lex::token2str(&ls.lex, token);
    let mut msg: Vec<u8> = Vec::with_capacity(tok_str.len() + 10);
    msg.extend_from_slice(&tok_str);
    msg.extend_from_slice(b" expected");
    lua_lex::syntax_error(&mut ls.lex, &msg)
}

/// Constructs a compile-time limit-exceeded syntax error.
fn error_limit(fs: &FuncState, limit: i32, what: &str) -> LuaError {
    let line = fs.f.linedefined;
    if line == 0 {
        LuaError::syntax(format_args!(
            "too many {} (limit is {}) in main function", what, limit
        ))
    } else {
        LuaError::syntax(format_args!(
            "too many {} (limit is {}) in function at line {}", what, limit, line
        ))
    }
}

fn check_limit(fs: &FuncState, v: i32, l: i32, what: &str) -> Result<(), LuaError> {
    if v > l {
        return Err(error_limit(fs, l, what));
    }
    Ok(())
}

// ── §2 Basic parse utilities ─────────────────────────────────────────────────

/// If the current token matches `c`, consume it and return true.
fn test_next(ls: &mut LexState, state: &mut LuaState, c: TokenKind) -> Result<bool, LuaError> {
    if ls.t.token == c {
        lex_next(ls, state)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

fn check(ls: &mut LexState, c: TokenKind) -> Result<(), LuaError> {
    if ls.t.token != c {
        return Err(error_expected(ls, c));
    }
    Ok(())
}

fn check_next(ls: &mut LexState, state: &mut LuaState, c: TokenKind) -> Result<(), LuaError> {
    check(ls, c)?;
    lex_next(ls, state)?;
    Ok(())
}

/// Expects TK_NAME, returns the name string, advances.
fn str_check_name(ls: &mut LexState, state: &mut LuaState) -> Result<GcRef<LuaString>, LuaError> {
    check(ls, TK_NAME)?;
    let ts = ls.t.seminfo.ts.clone()
        .ok_or_else(|| LuaError::syntax(format_args!("name expected")))?;
    lex_next(ls, state)?;
    Ok(ts)
}

fn init_exp(e: &mut ExprDesc, k: ExprKind, i: i32) {
    e.f = NO_JUMP;
    e.t = NO_JUMP;
    e.k = k;
    e.u.info = i;
}

fn codestring(e: &mut ExprDesc, s: GcRef<LuaString>) {
    e.f = NO_JUMP;
    e.t = NO_JUMP;
    e.k = ExprKind::KStr;
    e.u.strval = Some(s);
}

fn codename(ls: &mut LexState, state: &mut LuaState, e: &mut ExprDesc) -> Result<(), LuaError> {
    let name = str_check_name(ls, state)?;
    codestring(e, name);
    Ok(())
}

// ── §3 Variable handling ─────────────────────────────────────────────────────

/// Registers a local variable in the proto's debug-info locvars array.
/// Returns the index in locvars (= fs->ndebugvars before increment).
fn register_local_var(
    _ls: &mut LexState,
    _state: &mut LuaState,
    fs: &mut FuncState,
    varname: GcRef<LuaString>,
) -> Result<i32, LuaError> {
    // In Rust, Vec grows automatically; just push a placeholder if needed.
    let idx = fs.ndebugvars as usize;
    while fs.f.locvars.len() <= idx {
        fs.f.locvars.push(LocalVar {
            varname: varname.clone(), // placeholder; overwritten below
            startpc: 0,
            endpc: 0,
        });
    }
    fs.f.locvars[idx].varname = varname;
    fs.f.locvars[idx].startpc = fs.pc;
    let result = fs.ndebugvars as i32;
    fs.ndebugvars += 1;
    Ok(result)
}

/// Creates a new local variable entry in dyd.actvar.
/// Returns the variable's index relative to fs->firstlocal.
fn new_local_var(
    ls: &mut LexState,
    _state: &mut LuaState,
    name: GcRef<LuaString>,
) -> Result<i32, LuaError> {
    let fs = ls.fs.as_ref().unwrap();
    let n = ls.dyd.actvar.len() as i32;
    let first_local = fs.firstlocal;
    check_limit(fs, n + 1 - first_local, MAX_VARS, "local variables")?;

    let mut var = VarDesc::default();
    var.kind = VarKind::Reg;
    var.name = Some(name);
    ls.dyd.actvar.push(var);
    let result = ls.dyd.actvar.len() as i32 - 1 - first_local;
    Ok(result)
}

/// Returns a reference to the VarDesc at index `fs->firstlocal + vidx`.
fn get_local_var_desc<'a>(ls: &'a LexState, fs: &FuncState, vidx: i32) -> &'a VarDesc {
    &ls.dyd.actvar[(fs.firstlocal + vidx) as usize]
}

fn get_local_var_desc_mut(ls: &mut LexState, first_local: i32, vidx: i32) -> &mut VarDesc {
    &mut ls.dyd.actvar[(first_local + vidx) as usize]
}

/// Converts a compiler-index level to its register number.
fn reg_level(ls: &LexState, fs: &FuncState, nvar: i32) -> i32 {
    let mut nvar = nvar;
    while nvar > 0 {
        nvar -= 1;
        let vd = get_local_var_desc(ls, fs, nvar);
        if vd.kind != VarKind::CompileTimeConst {
            return vd.ridx as i32 + 1;
        }
    }
    0
}

/// Returns the number of variables currently occupying registers.
/// LUAI_FUNC visibility.
pub fn nvarstack(ls: &LexState, fs: &FuncState) -> i32 {
    reg_level(ls, fs, fs.nactvar as i32)
}

fn init_var(ls: &LexState, fs: &FuncState, e: &mut ExprDesc, vidx: i32) {
    e.f = NO_JUMP;
    e.t = NO_JUMP;
    e.k = ExprKind::Local;
    e.u.var_vidx = vidx as u16;
    e.u.var_ridx = get_local_var_desc(ls, fs, vidx).ridx;
}

/// Raises an error if expression `e` describes a read-only variable.
fn check_readonly(ls: &mut LexState, state: &mut LuaState, e: &ExprDesc) -> Result<(), LuaError> {
    let varname: Option<GcRef<LuaString>> = {
        let fs = ls.fs.as_ref().unwrap();
        match e.k {
            ExprKind::Const => {
                ls.dyd.actvar[e.u.info as usize].name.clone()
            }
            ExprKind::Local => {
                let vd = get_local_var_desc(ls, fs, e.u.var_vidx as i32);
                if vd.kind != VarKind::Reg {
                    vd.name.clone()
                } else {
                    None
                }
            }
            ExprKind::UpVal => {
                let up = &fs.f.upvalues[e.u.info as usize];
                if VarKind::from_u8(up.kind) != VarKind::Reg {
                    up.name.clone()
                } else {
                    None
                }
            }
            _ => None,
        }
    };
    if let Some(vname) = varname {
        // luaX_syntaxerror; route through `syntax_error` here for parity so
        // constructs.lua's checkload(":1: attempt to assign...") matches.
        let _ = state;
        let msg = format!(
            "attempt to assign to const variable '{}'",
            String::from_utf8_lossy(vname.as_bytes())
        );
        return Err(lua_lex::syntax_error(&mut ls.lex, msg.as_bytes()));
    }
    Ok(())
}

/// Starts the scope for the last `nvars` created variables.
fn adjust_local_vars(ls: &mut LexState, state: &mut LuaState, nvars: i32) -> Result<(), LuaError> {
    // Extract needed data to avoid borrow conflict with ls.fs and ls.dyd
    let first_local = ls.fs.as_ref().unwrap().firstlocal;
    let nactvar_start = ls.fs.as_ref().unwrap().nactvar as i32;
    let mut reglevel_val = {
        let fs = ls.fs.as_ref().unwrap();
        reg_level(ls, fs, fs.nactvar as i32)
    };

    for i in 0..nvars {
        let vidx = nactvar_start + i;
        ls.fs.as_mut().unwrap().nactvar += 1;
        let var_name = ls.dyd.actvar[(first_local + vidx) as usize].name.clone();
        ls.dyd.actvar[(first_local + vidx) as usize].ridx = reglevel_val as u8;
        reglevel_val += 1;
        if let Some(vn) = var_name {
            let mut fs_box = ls.fs.take().unwrap();
            let pidx_result = register_local_var(ls, state, &mut fs_box, vn);
            ls.fs = Some(fs_box);
            let pidx = pidx_result?;
            ls.dyd.actvar[(first_local + vidx) as usize].pidx = pidx as i16;
        } else {
            // TODO(port): variable has no name — shouldn't happen in valid source
        }
    }
    Ok(())
}

/// Closes scope for all variables above `tolevel`, updating their endpc.
fn remove_vars(ls: &mut LexState, fs: &mut FuncState, tolevel: i32) {
    //
    // C just decrements a length counter; the underlying array memory is
    // untouched and the subsequent loop reads from it freely. A Rust
    // `truncate` would actually free the entries, leaving the loop reading
    // out-of-range and silently writing every iteration's endpc to
    // `locvars[0]` (via the `unwrap_or(0)` fallback below). Defer the
    // truncate until after the loop walks each soon-to-be-removed entry.
    let delta = fs.nactvar as i32 - tolevel;
    while fs.nactvar as i32 > tolevel {
        fs.nactvar -= 1;
        let nactvar = fs.nactvar as i32;
        let vd_kind = {
            let first_local = fs.firstlocal;
            ls.dyd.actvar.get((first_local + nactvar) as usize)
                .map(|v| v.kind)
                .unwrap_or(VarKind::Reg)
        };
        if vd_kind != VarKind::CompileTimeConst {
            let vd_pidx = {
                let first_local = fs.firstlocal;
                ls.dyd.actvar.get((first_local + nactvar) as usize)
                    .map(|v| v.pidx)
                    .unwrap_or(0)
            };
            if let Some(lv) = fs.f.locvars.get_mut(vd_pidx as usize) {
                lv.endpc = fs.pc;
            }
        }
    }
    if delta > 0 {
        let new_len = ls.dyd.actvar.len().saturating_sub(delta as usize);
        ls.dyd.actvar.truncate(new_len);
    }
}

// ── §4 Upvalue handling ──────────────────────────────────────────────────────

/// Returns the index of an upvalue named `name`, or -1 if not found.
fn search_upvalue(fs: &FuncState, name: &GcRef<LuaString>) -> i32 {
    for (i, up) in fs.f.upvalues.iter().enumerate() {
        if up.name.as_ref().map_or(false, |n| GcRef::ptr_eq(n, name)) {
            return i as i32;
        }
    }
    -1
}

/// Grows upvalues array and returns index of the new slot.
fn alloc_upvalue(fs: &mut FuncState) -> Result<usize, LuaError> {
    if fs.nups as i32 + 1 > MAX_UPVAL as i32 {
        return Err(error_limit(fs, MAX_UPVAL as i32, "upvalues"));
    }
    let idx = fs.nups as usize;
    while fs.f.upvalues.len() <= idx {
        fs.f.upvalues.push(UpvalDesc { name: None, instack: false, idx: 0, kind: 0 });
    }
    fs.nups += 1;
    Ok(idx)
}

/// Adds a new upvalue descriptor and returns its index.
fn new_upvalue(
    ls: &LexState,
    fs: &mut FuncState,
    name: GcRef<LuaString>,
    v: &ExprDesc,
) -> Result<i32, LuaError> {
    let idx = alloc_upvalue(fs)?;
    let kind: u8 = if v.k == ExprKind::Local {
        let prev = fs.prev.as_deref().expect("upvalue capture requires enclosing FuncState");
        get_local_var_desc(ls, prev, v.u.var_vidx as i32).kind.as_u8()
    } else {
        let prev = fs.prev.as_deref().expect("upvalue chain requires enclosing FuncState");
        prev.f.upvalues[v.u.info as usize].kind
    };
    let up = &mut fs.f.upvalues[idx];
    if v.k == ExprKind::Local {
        up.instack = true;
        up.idx = v.u.var_ridx;
    } else {
        up.instack = false;
        up.idx = v.u.info as u8;
    }
    up.kind = kind;
    up.name = Some(name);
    Ok(fs.nups as i32 - 1)
}

/// Searches for a local variable named `n`. Returns ExprKind as i32 or -1.
fn searchvar(
    ls: &LexState,
    fs: &FuncState,
    n: &GcRef<LuaString>,
    var: &mut ExprDesc,
) -> i32 {
    let mut i = fs.nactvar as i32 - 1;
    while i >= 0 {
        let vd = get_local_var_desc(ls, fs, i);
        if vd.name.as_ref().map_or(false, |nm| GcRef::ptr_eq(nm, n)) {
            if vd.kind == VarKind::CompileTimeConst {
                init_exp(var, ExprKind::Const, fs.firstlocal + i);
            } else {
                init_var(ls, fs, var, i);
            }
            return var.k as i32; // PORT NOTE: encoding ExprKind as i32 for C compat
        }
        i -= 1;
    }
    -1
}

/// Marks the block where the variable at `level` was defined as having an upvalue.
fn markupval(fs: &mut FuncState, level: i32) {
    let mut current = fs.bl.as_deref_mut();
    while let Some(b) = current {
        if (b.nactvar as i32) <= level {
            b.upval = true;
            break;
        }
        current = b.previous.as_deref_mut();
    }
    fs.needclose = true;
}

fn marktobeclosed(fs: &mut FuncState) {
    if let Some(bl) = fs.bl.as_mut() {
        bl.upval = true;
        bl.insidetbc = true;
    }
    fs.needclose = true;
}

// ── §5 Variable resolution ───────────────────────────────────────────────────

/// Recursively finds variable `n` in `fs` and its enclosing functions.
/// If not found at any level, sets var->k = VVOID (global).
fn singlevaraux(
    ls: &LexState,
    fs: Option<&mut FuncState>,
    n: &GcRef<LuaString>,
    var: &mut ExprDesc,
    base: bool,
) -> Result<(), LuaError> {
    match fs {
        None => {
            init_exp(var, ExprKind::Void, 0);
        }
        Some(fs) => {
            let v = searchvar(ls, fs, n, var);
            if v >= 0 {
                if v == ExprKind::Local as i32 && !base {
                    markupval(fs, var.u.var_vidx as i32);
                }
            } else {
                let idx = search_upvalue(fs, n);
                let final_idx = if idx < 0 {
                    singlevaraux(ls, fs.prev.as_deref_mut(), n, var, false)?;
                    if var.k == ExprKind::Local || var.k == ExprKind::UpVal {
                        new_upvalue(ls, fs, n.clone(), var)?
                    } else {
                        return Ok(());
                    }
                } else {
                    idx
                };
                init_exp(var, ExprKind::UpVal, final_idx);
            }
        }
    }
    Ok(())
}

/// Finds the variable named by the next TK_NAME token.
fn singlevar(ls: &mut LexState, state: &mut LuaState, var: &mut ExprDesc) -> Result<(), LuaError> {
    let varname = str_check_name(ls, state)?;
    let mut fs_box = ls.fs.take();
    let recurse_result = singlevaraux(ls, fs_box.as_deref_mut(), &varname, var, true);
    ls.fs = fs_box;
    recurse_result?;
    if var.k == ExprKind::Void {
        let envn = ls.envn.clone().expect("envn must be set when resolving globals");
        let mut env_var = ExprDesc::default();
        let mut fs_box = ls.fs.take();
        let r = singlevaraux(ls, fs_box.as_deref_mut(), &envn, &mut env_var, true);
        ls.fs = fs_box;
        r?;
        debug_assert!(env_var.k != ExprKind::Void, "_ENV must resolve");
        let line = ls.lastline;
        let fs = ls.fs.as_mut().unwrap();
        cg_exp_to_any_reg_up(fs, line, &mut env_var)?;
        let mut key = ExprDesc::default();
        codestring(&mut key, varname);
        cg_indexed(fs, line, &mut env_var, &mut key)?;
        *var = env_var;
    }
    Ok(())
}

fn adjust_assign(
    ls: &mut LexState,
    _state: &mut LuaState,
    nvars: i32,
    nexps: i32,
    e: &mut ExprDesc,
) -> Result<(), LuaError> {
    let needed = nvars - nexps;
    let line = ls.lastline;
    let fs = ls.fs.as_mut().unwrap();
    if e.k.has_mult_ret() {
        let extra = if needed + 1 < 0 { 0 } else { needed + 1 };
        cg_set_returns(fs, e, extra);
    } else {
        if e.k != ExprKind::Void {
            cg_exp_to_next_reg(fs, line, e)?;
        }
        if needed > 0 {
            let from = fs.freereg as i32;
            cg_emit_nil(fs, line, from, needed);
        }
    }
    if needed > 0 {
        for _ in 0..needed {
            reserve_reg(fs)?;
        }
    } else {
        fs.freereg = (fs.freereg as i32 + needed) as u8;
    }
    Ok(())
}

/// Emits `OP_NEWTABLE` followed by the required `OP_EXTRAARG` slot. The two
/// instructions are written as placeholders; `cg_settablesize` later patches
/// them with the final array/hash sizes. Returns the pc of `OP_NEWTABLE`.
fn cg_emit_newtable(fs: &mut FuncState, line: i32) -> i32 {
    let newtable = lua_code::opcodes::Instruction::abck(
        lua_code::opcodes::OpCode::NewTable, 0, 0, 0, 0,
    );
    let pc = emit_inst(fs, line, newtable);
    let extra = lua_code::opcodes::Instruction::ax(
        lua_code::opcodes::OpCode::ExtraArg, 0,
    );
    emit_inst(fs, line, extra);
    pc
}

/// Patches a previously-emitted `OP_NEWTABLE`/`OP_EXTRAARG` pair with the
/// final array size (`asize`) and hash size (`hsize`). Mirrors
/// `luaK_settablesize` from `lcode.c`.
fn cg_settablesize(fs: &mut FuncState, pc: i32, ra: i32, asize: i32, hsize: i32) {
    let rb = if hsize != 0 {
        (hsize as u32).next_power_of_two().trailing_zeros() as i32 + 1
    } else {
        0
    };
    let maxc = lua_code::opcodes::MAXARG_C as i32 + 1;
    let extra = asize / maxc;
    let rc = asize % maxc;
    let k = if extra > 0 { 1u32 } else { 0u32 };
    let newtable = lua_code::opcodes::Instruction::abck(
        lua_code::opcodes::OpCode::NewTable,
        ra as u32, rb as u32, rc as u32, k,
    );
    fs.f.code[pc as usize] = lua_types::opcode::Instruction::new(newtable.0);
    let extra_inst = lua_code::opcodes::Instruction::ax(
        lua_code::opcodes::OpCode::ExtraArg, extra as u32,
    );
    fs.f.code[pc as usize + 1] = lua_types::opcode::Instruction::new(extra_inst.0);
}

/// Emits `OP_SETLIST` for `tostore` elements starting at `base+1`, with
/// `nelems` already-stored elements preceding them. `tostore == -1` means
/// `LUA_MULTRET` (encoded as 0 in the B field). Also resets `fs.freereg`
/// to `base + 1`, mirroring `luaK_setlist`.
fn cg_setlist(fs: &mut FuncState, line: i32, base: i32, nelems: i32, tostore: i32) {
    let maxc = lua_code::opcodes::MAXARG_C as i32;
    let tostore_arg = if tostore == LUA_MULTRET { 0 } else { tostore };
    if nelems <= maxc {
        let inst = lua_code::opcodes::Instruction::abck(
            lua_code::opcodes::OpCode::SetList,
            base as u32, tostore_arg as u32, nelems as u32, 0,
        );
        emit_inst(fs, line, inst);
    } else {
        let extra = nelems / (maxc + 1);
        let nelems_lo = nelems % (maxc + 1);
        let inst = lua_code::opcodes::Instruction::abck(
            lua_code::opcodes::OpCode::SetList,
            base as u32, tostore_arg as u32, nelems_lo as u32, 1,
        );
        emit_inst(fs, line, inst);
        let extra_inst = lua_code::opcodes::Instruction::ax(
            lua_code::opcodes::OpCode::ExtraArg, extra as u32,
        );
        emit_inst(fs, line, extra_inst);
    }
    fs.freereg = (base + 1) as u8;
}

/// Converts a table-and-key expression pair into the appropriate `VINDEX*`
/// variant. Mirrors `luaK_indexed` from `lcode.c`. Assumes `t` is already a
/// value-producing form (`VLOCAL`, `VNONRELOC`, or `VUPVAL`) and that any
/// short-string key has already been promoted to a `VKSTR` constant index.
fn cg_indexed(fs: &mut FuncState, line: i32, t: &mut ExprDesc, k: &mut ExprDesc) -> Result<(), LuaError> {
    if k.k == ExprKind::KStr {
        let s = k.u.strval.clone()
            .ok_or_else(|| LuaError::syntax(format_args!("internal: VKStr with no strval")))?;
        let k_idx = add_k_string(fs, s);
        k.u.info = k_idx;
        k.k = ExprKind::K;
    }
    let k_is_kstr = k.k == ExprKind::K
        && k.u.info >= 0
        && (k.u.info as u32) <= lua_code::opcodes::MAXARG_B;
    if t.k == ExprKind::UpVal && !k_is_kstr {
        cg_exp_to_any_reg(fs, line, t)?;
    }
    if t.k == ExprKind::UpVal {
        let temp = t.u.info as u8;
        t.u.ind_t = temp;
        t.u.ind_idx = k.u.info as i16;
        t.k = ExprKind::IndexUp;
        return Ok(());
    }
    let t_reg = match t.k {
        ExprKind::Local => t.u.var_ridx,
        ExprKind::NonReloc => t.u.info as u8,
        _ => return Err(LuaError::syntax(format_args!(
            "internal: cg_indexed on non-register table kind {:?}", t.k
        ))),
    };
    t.u.ind_t = t_reg;
    if k.k == ExprKind::K && k_is_kstr {
        t.u.ind_idx = k.u.info as i16;
        t.k = ExprKind::IndexStr;
    } else if k.k == ExprKind::KInt && cg_fits_int_key(k.u.ival) {
        t.u.ind_idx = k.u.ival as i16;
        t.k = ExprKind::IndexI;
    } else {
        cg_exp_to_any_reg(fs, line, k)?;
        t.u.ind_idx = k.u.info as i16;
        t.k = ExprKind::Indexed;
    }
    Ok(())
}

fn cg_fits_int_key(i: i64) -> bool {
    i >= 0 && (i as u32) <= lua_code::opcodes::MAXARG_C
}

/// Emits OP_SELF, converting `e:key(...)` into the equivalent of `(e.key)(e, ...)`.
/// Leaves `e` as VNONRELOC pointing at the function register (base); the self
/// register is `base + 1`. `key` must be a string expression (VKStr).
fn cg_self(
    fs: &mut FuncState,
    line: i32,
    e: &mut ExprDesc,
    key: &mut ExprDesc,
) -> Result<(), LuaError> {
    cg_exp_to_any_reg(fs, line, e)?;
    let ereg = e.u.info;
    cg_free_exp(fs, e);
    let base = fs.freereg as i32;
    e.u.info = base;
    e.k = ExprKind::NonReloc;
    reserve_regs(fs, 2)?;
    let key_str = key.u.strval.clone()
        .ok_or_else(|| LuaError::syntax(format_args!(
            "internal: cg_self expected VKStr key, got {:?}", key.k
        )))?;
    let k_idx = add_k_string(fs, key_str);
    let (c_arg, k_flag) = if (k_idx as u32) <= lua_code::opcodes::MAXINDEXRK {
        (k_idx as u32, 1u32)
    } else {
        key.k = ExprKind::K;
        key.u.info = k_idx;
        cg_exp_to_any_reg(fs, line, key)?;
        (key.u.info as u32, 0u32)
    };
    let inst = lua_code::opcodes::Instruction::abck(
        lua_code::opcodes::OpCode::Self_,
        base as u32,
        ereg as u32,
        c_arg,
        k_flag,
    );
    emit_inst(fs, line, inst);
    cg_free_exp(fs, key);
    Ok(())
}

/// Minimal `luaK_exp2anyregup`: if `e` is an upvalue or constant, leave it as
/// is; otherwise discharge it into some register.
fn cg_exp_to_any_reg_up(fs: &mut FuncState, line: i32, e: &mut ExprDesc) -> Result<(), LuaError> {
    if matches!(e.k, ExprKind::UpVal | ExprKind::K) {
        return Ok(());
    }
    cg_exp_to_any_reg(fs, line, e)?;
    Ok(())
}

/// Minimal `luaK_nil`: emits a LoadNil instruction filling `n` consecutive
/// registers starting at `from` with `nil`. Does not perform the C
/// optimization that merges with a preceding LoadNil.
fn cg_emit_nil(fs: &mut FuncState, line: i32, from: i32, n: i32) {
    let inst = lua_code::opcodes::Instruction::abck(
        lua_code::opcodes::OpCode::LoadNil,
        from as u32,
        (n - 1) as u32,
        0,
        0,
    );
    emit_inst(fs, line, inst);
}

// ── §6 Label / goto management ───────────────────────────────────────────────

fn jumpscopeerror(ls: &LexState, gt_idx: usize) -> LuaError {
    let gt = &ls.dyd.gt[gt_idx];
    let line = gt.line;
    let gt_name_bytes: &[u8] = gt.name.as_ref().map(|n| n.as_bytes()).unwrap_or(b"");
    let gt_name = String::from_utf8_lossy(gt_name_bytes);
    let varname_bytes: &[u8] = ls.fs.as_ref()
        .and_then(|fs| {
            let vidx = gt.nactvar as i32;
            if (fs.firstlocal + vidx) >= 0 && ((fs.firstlocal + vidx) as usize) < ls.dyd.actvar.len() {
                let vd = get_local_var_desc(ls, fs, vidx);
                vd.name.as_ref().map(|n| n.as_bytes())
            } else {
                None
            }
        })
        .unwrap_or(b"");
    let varname = String::from_utf8_lossy(varname_bytes);
    LuaError::syntax(format_args!(
        "<goto {}> at line {} jumps into the scope of local '{}'", gt_name, line, varname
    ))
}

/// Resolves goto at index `g` to `label`, removing it from pending list.
fn solvegoto(
    ls: &mut LexState,
    _state: &mut LuaState,
    g: usize,
    label_pc: i32,
    label_nactvar: u8,
) -> Result<(), LuaError> {
    if ls.dyd.gt[g].nactvar < label_nactvar {
        return Err(jumpscopeerror(ls, g));
    }
    let gt_pc = ls.dyd.gt[g].pc;
    cg_patch_list(ls.fs.as_mut().unwrap(), gt_pc, label_pc)?;
    ls.dyd.gt.remove(g);
    Ok(())
}

/// Searches for an active label with the given name in the current function.
fn findlabel(ls: &LexState, name: &GcRef<LuaString>) -> Option<usize> {
    let first = ls.fs.as_ref().unwrap().firstlabel as usize;
    for i in first..ls.dyd.label.len() {
        let lb = &ls.dyd.label[i];
        if lb.name.as_ref().map_or(false, |n| GcRef::ptr_eq(n, name)) {
            return Some(i);
        }
    }
    None
}

/// Adds a new label/goto entry; returns its index.
fn new_label_entry(
    ls: &mut LexState,
    _state: &mut LuaState,
    is_goto: bool,
    name: GcRef<LuaString>,
    line: i32,
    pc: i32,
) -> Result<usize, LuaError> {
    let nactvar = ls.fs.as_ref().unwrap().nactvar;
    let entry = LabelDesc { name: Some(name), pc, line, nactvar, close: false };
    let list = if is_goto { &mut ls.dyd.gt } else { &mut ls.dyd.label };
    let n = list.len();
    list.push(entry);
    Ok(n)
}

fn new_goto_entry(
    ls: &mut LexState,
    state: &mut LuaState,
    name: GcRef<LuaString>,
    line: i32,
    pc: i32,
) -> Result<usize, LuaError> {
    new_label_entry(ls, state, true, name, line, pc)
}

/// Resolves all pending gotos that match label `lb`.
/// Returns true if any goto needed close.
fn solvegotos(ls: &mut LexState, state: &mut LuaState, lb_idx: usize) -> Result<bool, LuaError> {
    let lb_name = ls.dyd.label[lb_idx].name.clone();
    let lb_pc = ls.dyd.label[lb_idx].pc;
    let lb_nactvar = ls.dyd.label[lb_idx].nactvar;
    let first_goto = ls.fs.as_ref().unwrap().bl.as_ref().map_or(0, |b| b.firstgoto) as usize;

    let mut i = first_goto;
    let mut needs_close = false;
    while i < ls.dyd.gt.len() {
        let gt_name = ls.dyd.gt[i].name.clone();
        let names_match = lb_name.as_ref().and_then(|ln| gt_name.as_ref().map(|gn| GcRef::ptr_eq(ln, gn))).unwrap_or(false);
        if names_match {
            needs_close |= ls.dyd.gt[i].close;
            // solvegoto removes element i, so don't increment i
            solvegoto(ls, state, i, lb_pc, lb_nactvar)?;
        } else {
            i += 1;
        }
    }
    Ok(needs_close)
}

/// Creates a new label; resolves pending gotos. Returns true if CLOSE emitted.
fn createlabel(
    ls: &mut LexState,
    state: &mut LuaState,
    name: GcRef<LuaString>,
    line: i32,
    last: bool,
) -> Result<bool, LuaError> {
    let label_pc = cg_get_label(ls.fs.as_mut().unwrap());
    let l = new_label_entry(ls, state, false, name, line, label_pc)?;
    if last {
        let bl_nactvar = ls.fs.as_ref().unwrap().bl.as_ref().map_or(0, |b| b.nactvar);
        ls.dyd.label[l].nactvar = bl_nactvar;
    }
    let needs_close = solvegotos(ls, state, l)?;
    if needs_close {
        let nstack = nvarstack(ls, ls.fs.as_ref().unwrap()) as u32;
        let inst = lua_code::opcodes::Instruction::abck(
            lua_code::opcodes::OpCode::Close,
            nstack,
            0,
            0,
            0,
        );
        emit_inst(ls.fs.as_mut().unwrap(), line, inst);
        return Ok(true);
    }
    Ok(false)
}

/// Adjusts pending gotos to outer block level when leaving a block.
fn movegotosout(ls: &mut LexState, bl_firstgoto: usize, bl_nactvar: u8, bl_upval: bool) {
    let _ = ls.fs.as_ref().unwrap();
    let first_goto = bl_firstgoto;
    let _n_gt = ls.dyd.gt.len();

    for i in first_goto..ls.dyd.gt.len() {
        let _gt_nactvar = ls.dyd.gt[i].nactvar;
        // TODO(port): compute reg_level properly using ls+fs
        if bl_upval {
            ls.dyd.gt[i].close = true;
        }
        ls.dyd.gt[i].nactvar = bl_nactvar;
    }
}

/// Pushes a new block scope onto fs->bl.
fn enter_block(ls: &mut LexState, isloop: bool) {
    let firstlabel = ls.dyd.label.len() as i32;
    let firstgoto = ls.dyd.gt.len() as i32;
    let insidetbc = ls.fs.as_ref()
        .and_then(|f| f.bl.as_ref())
        .map_or(false, |b| b.insidetbc);
    let fs = ls.fs.as_mut().unwrap();
    let nactvar = fs.nactvar;
    let new_bl = Box::new(BlockCnt {
        previous: fs.bl.take(),
        firstlabel,
        firstgoto,
        nactvar,
        upval: false,
        isloop,
        insidetbc,
    });
    fs.bl = Some(new_bl);
    debug_assert!(fs.freereg as i32 == {
        // TODO(port): nvarstack(ls, fs) -- circular borrow
        fs.freereg as i32 // placeholder assertion
    });
}

fn undef_goto(ls: &LexState, gt_idx: usize) -> LuaError {
    let gt = &ls.dyd.gt[gt_idx];
    let line = gt.line;
    let name_bytes: &[u8] = gt.name.as_ref().map(|n| n.as_bytes()).unwrap_or(b"");
    if name_bytes == b"break" {
        LuaError::syntax(format_args!("break outside loop at line {}", line))
    } else {
        let name_str = String::from_utf8_lossy(name_bytes);
        LuaError::syntax(format_args!("no visible label '{}' for <goto> at line {}", name_str, line))
    }
}

/// Pops the innermost block scope, emitting CLOSE if needed.
fn leave_block(ls: &mut LexState, state: &mut LuaState) -> Result<(), LuaError> {
    // Snapshot block fields without popping; createlabel below relies on
    // fs->bl still pointing at this (loop) block so solvegotos can read
    // fs->bl->firstgoto.
    let (bl_nactvar, bl_isloop, bl_upval, bl_firstgoto, bl_firstlabel) = {
        let bl = ls
            .fs
            .as_ref()
            .unwrap()
            .bl
            .as_ref()
            .expect("leave_block: no current block");
        (bl.nactvar, bl.isloop, bl.upval, bl.firstgoto, bl.firstlabel)
    };

    let stklevel = reg_level(ls, ls.fs.as_ref().unwrap(), bl_nactvar as i32);
    let mut fs_box = ls.fs.take().unwrap();
    remove_vars(ls, &mut fs_box, bl_nactvar as i32);
    debug_assert!(bl_nactvar == fs_box.nactvar);
    ls.fs = Some(fs_box);

    let hasclose = if bl_isloop {
        let break_str = state.intern_str(b"break")?;
        createlabel(ls, state, break_str, 0, false)?
    } else {
        false
    };

    // Now pop the block off fs.bl, restoring its previous link.
    let mut bl_box = ls.fs.as_mut().unwrap().bl.take().unwrap();
    let previous = bl_box.previous.take();
    ls.fs.as_mut().unwrap().bl = previous;

    let has_prev_block = ls.fs.as_ref().unwrap().bl.is_some();
    if !hasclose && has_prev_block && bl_upval {
        // Use `lastline` so the OP_CLOSE attributes to the block's terminating
        // token (END/UNTIL) rather than whatever the parser has peeked to next.
        // Mirrors lua-c's `savelineinfo(fs, f, fs->ls->lastline)`.
        let line = ls.lastline;
        let inst = lua_code::opcodes::Instruction::abck(
            lua_code::opcodes::OpCode::Close,
            stklevel as u32,
            0,
            0,
            0,
        );
        emit_inst(ls.fs.as_mut().unwrap(), line, inst);
    }
    ls.fs.as_mut().unwrap().freereg = stklevel as u8;

    ls.dyd.label.truncate(bl_firstlabel as usize);

    if has_prev_block {
        movegotosout(ls, bl_firstgoto as usize, bl_nactvar, bl_upval);
    } else {
        if (bl_firstgoto as usize) < ls.dyd.gt.len() {
            return Err(undef_goto(ls, bl_firstgoto as usize));
        }
    }
    Ok(())
}

// ── §7 Proto management ──────────────────────────────────────────────────────

/// Adds a new prototype slot to the current function's proto list.
/// Returns a mutable reference to the new prototype.
fn add_prototype(ls: &mut LexState, _state: &mut LuaState) -> Result<Box<LuaProto>, LuaError> {
    let np = ls.fs.as_ref().unwrap().np as usize;
    // TODO(port): allocate via state.gc().new_proto() in Phase B
    let new_proto = Box::new(LuaProto::placeholder());
    while ls.fs.as_ref().unwrap().f.p.len() <= np {
        ls.fs
            .as_mut()
            .unwrap()
            .f
            .p
            .push(GcRef::new(LuaProto::placeholder()));
    }
    ls.fs.as_mut().unwrap().np += 1;
    Ok(new_proto)
}

/// Emits OP_CLOSURE in the parent function and fixes up v.
fn codeclosure(ls: &mut LexState, _state: &mut LuaState, v: &mut ExprDesc) -> Result<(), LuaError> {
    let line = ls.lastline;
    let mut child = ls.fs.take().expect("codeclosure: no current FuncState");
    let result = (|| -> Result<(), LuaError> {
        let parent = child.prev.as_mut().expect(
            "codeclosure: child FuncState has no parent (called outside body()?)",
        );
        let bx = (parent.np - 1) as u32;
        let inst = lua_code::opcodes::Instruction::abx(
            lua_code::opcodes::OpCode::Closure,
            0,
            bx,
        );
        let pc = emit_inst(parent, line, inst);
        init_exp(v, ExprKind::Reloc, pc);
        cg_exp_to_next_reg(parent, line, v)
    })();
    ls.fs = Some(child);
    result
}

/// Installs `new_fs` as the current FuncState, pushing old one as `prev`.
fn open_func(ls: &mut LexState, _state: &mut LuaState, mut new_fs: FuncState) -> Result<(), LuaError> {
    new_fs.prev = ls.fs.take();

    let f = &mut new_fs.f;
    new_fs.pc = 0;
    new_fs.previousline = f.linedefined;
    new_fs.iwthabs = 0;
    new_fs.lasttarget = 0;
    new_fs.freereg = 0;
    new_fs.nk = 0;
    new_fs.nabslineinfo = 0;
    new_fs.np = 0;
    new_fs.nups = 0;
    new_fs.ndebugvars = 0;
    new_fs.nactvar = 0;
    new_fs.needclose = false;

    new_fs.firstlocal = ls.dyd.actvar.len() as i32;
    new_fs.firstlabel = ls.dyd.label.len() as i32;
    new_fs.bl = None;

    new_fs.f.source = ls.source.clone();
    new_fs.f.maxstacksize = 2;


    ls.fs = Some(Box::new(new_fs));

    enter_block(ls, false);
    Ok(())
}

/// Finalizes and pops the current FuncState.
/// Returns the completed LuaProto.
fn close_func(ls: &mut LexState, state: &mut LuaState) -> Result<Box<LuaProto>, LuaError> {
    {
        let first = {
            let fs = ls.fs.as_ref().unwrap();
            nvarstack(ls, fs)
        };
        let line = ls.lastline;
        let fs = ls.fs.as_mut().unwrap();
        let inst = lua_code::opcodes::Instruction::abck(
            lua_code::opcodes::OpCode::Return0,
            first as u32,
            1,
            0,
            0,
        );
        emit_inst(fs, line, inst);
    }
    leave_block(ls, state)?;
    debug_assert!(ls.fs.as_ref().unwrap().bl.is_none());

    //                     and needclose, and resolve JMP chains to final target.
    cg_finish(ls.fs.as_mut().unwrap());

    {
        let fs = ls.fs.as_mut().unwrap();
        let pc = fs.pc as usize;
        let nabslineinfo = fs.nabslineinfo as usize;
        let nk = fs.nk as usize;
        let np = fs.np as usize;
        let ndebugvars = fs.ndebugvars as usize;
        let nups = fs.nups as usize;
        fs.f.code.truncate(pc);
        fs.f.lineinfo.truncate(pc);
        fs.f.abslineinfo.truncate(nabslineinfo);
        fs.f.k.truncate(nk);
        fs.f.p.truncate(np);
        fs.f.locvars.truncate(ndebugvars);
        fs.f.upvalues.truncate(nups);
    }

    let mut fs_box = ls.fs.take().unwrap();
    ls.fs = fs_box.prev.take();

    Ok(fs_box.f)
}

// ── §8 Grammar rules — block / statement lists ───────────────────────────────

/// Returns true if the current token can end a block.
fn block_follow(ls: &LexState, withuntil: bool) -> bool {
    match ls.t.token {
        TK_ELSE | TK_ELSEIF | TK_END | TK_EOS => true,
        TK_UNTIL => withuntil,
        _ => false,
    }
}

fn statlist(ls: &mut LexState, state: &mut LuaState) -> Result<(), LuaError> {
    while !block_follow(ls, true) {
        if ls.t.token == TK_RETURN {
            statement(ls, state)?;
            return Ok(());
        }
        statement(ls, state)?;
    }
    Ok(())
}

/// Handles '.' NAME or ':' NAME field selection.
fn fieldsel(ls: &mut LexState, state: &mut LuaState, v: &mut ExprDesc) -> Result<(), LuaError> {
    let line = ls.lastline;
    cg_exp_to_any_reg_up(ls.fs.as_mut().unwrap(), line, v)?;
    lex_next(ls, state)?; // skip '.' or ':'
    let mut key = ExprDesc::default();
    codename(ls, state, &mut key)?;
    cg_indexed(ls.fs.as_mut().unwrap(), line, v, &mut key)?;
    Ok(())
}

/// Handles '[' expr ']' indexing.
fn yindex(ls: &mut LexState, state: &mut LuaState, v: &mut ExprDesc) -> Result<(), LuaError> {
    lex_next(ls, state)?;
    expr(ls, state, v)?;
    // TODO(port): lua_code::exp_to_val(ls.fs.as_mut().unwrap(), v)?;
    check_next(ls, state, b']' as TokenKind)?;
    Ok(())
}

// ── §9 Constructor rules ─────────────────────────────────────────────────────

fn recfield(ls: &mut LexState, state: &mut LuaState, cc: &mut ConsControl) -> Result<(), LuaError> {
    let reg = ls.fs.as_ref().unwrap().freereg as i32;
    let mut key = ExprDesc::default();
    let mut val = ExprDesc::default();
    if ls.t.token == TK_NAME {
        let fs = ls.fs.as_ref().unwrap();
        check_limit(fs, cc.nh, i32::MAX, "items in a constructor")?;
        codename(ls, state, &mut key)?;
    } else {
        yindex(ls, state, &mut key)?;
    }
    cc.nh += 1;
    check_next(ls, state, b'=' as TokenKind)?;
    let mut tab = cc.t.clone();
    let line = ls.lastline;
    cg_indexed(ls.fs.as_mut().unwrap(), line, &mut tab, &mut key)?;
    expr(ls, state, &mut val)?;
    cg_storevar(ls.fs.as_mut().unwrap(), line, &tab, &mut val)?;
    ls.fs.as_mut().unwrap().freereg = reg as u8;
    Ok(())
}

fn closelistfield(ls: &mut LexState, state: &mut LuaState, cc: &mut ConsControl) -> Result<(), LuaError> {
    let _ = state;
    if cc.v.k == ExprKind::Void {
        return Ok(());
    }
    let line = ls.lastline;
    cg_exp_to_next_reg(ls.fs.as_mut().unwrap(), line, &mut cc.v)?;
    cc.v.k = ExprKind::Void;
    if cc.tostore == LFIELDS_PER_FLUSH {
        let t_info = cc.t.u.info;
        cg_setlist(ls.fs.as_mut().unwrap(), line, t_info, cc.na, cc.tostore);
        cc.na += cc.tostore;
        cc.tostore = 0;
    }
    Ok(())
}

fn lastlistfield(ls: &mut LexState, state: &mut LuaState, cc: &mut ConsControl) -> Result<(), LuaError> {
    let _ = state;
    if cc.tostore == 0 {
        return Ok(());
    }
    let t_info = cc.t.u.info;
    let line = ls.lastline;
    if cc.v.k.has_mult_ret() {
        cg_set_returns(ls.fs.as_mut().unwrap(), &mut cc.v, LUA_MULTRET);
        cg_setlist(ls.fs.as_mut().unwrap(), line, t_info, cc.na, LUA_MULTRET);
        cc.na -= 1;
    } else {
        if cc.v.k != ExprKind::Void {
            cg_exp_to_next_reg(ls.fs.as_mut().unwrap(), line, &mut cc.v)?;
        }
        cg_setlist(ls.fs.as_mut().unwrap(), line, t_info, cc.na, cc.tostore);
    }
    cc.na += cc.tostore;
    Ok(())
}

fn listfield(ls: &mut LexState, state: &mut LuaState, cc: &mut ConsControl) -> Result<(), LuaError> {
    expr(ls, state, &mut cc.v)?;
    cc.tostore += 1;
    Ok(())
}

fn field(ls: &mut LexState, state: &mut LuaState, cc: &mut ConsControl) -> Result<(), LuaError> {
    match ls.t.token {
        TK_NAME => {
            let next_is_eq = lex_lookahead(ls, state)? == b'=' as TokenKind;
            if !next_is_eq {
                listfield(ls, state, cc)?;
            } else {
                recfield(ls, state, cc)?;
            }
        }
        c if c == b'[' as TokenKind => {
            recfield(ls, state, cc)?;
        }
        _ => {
            listfield(ls, state, cc)?;
        }
    }
    Ok(())
}

fn constructor(ls: &mut LexState, state: &mut LuaState, t: &mut ExprDesc) -> Result<(), LuaError> {
    let line = ls.lastline;
    let pc = cg_emit_newtable(ls.fs.as_mut().unwrap(), line);

    let freereg = ls.fs.as_ref().unwrap().freereg as i32;
    init_exp(t, ExprKind::NonReloc, freereg);
    reserve_regs(ls.fs.as_mut().unwrap(), 1)?;

    let mut cc = ConsControl {
        v: ExprDesc::default(),
        t: t.clone(),
        nh: 0,
        na: 0,
        tostore: 0,
    };

    check_next(ls, state, b'{' as TokenKind)?;
    loop {
        debug_assert!(cc.v.k == ExprKind::Void || cc.tostore > 0);
        if ls.t.token == b'}' as TokenKind {
            break;
        }
        closelistfield(ls, state, &mut cc)?;
        field(ls, state, &mut cc)?;
        if !test_next(ls, state, b',' as TokenKind)?
            && !test_next(ls, state, b';' as TokenKind)?
        {
            break;
        }
    }
    check_match(ls, state, b'}' as TokenKind, b'{' as TokenKind, line)?;
    lastlistfield(ls, state, &mut cc)?;

    let t_info = t.u.info;
    cg_settablesize(ls.fs.as_mut().unwrap(), pc, t_info, cc.na, cc.nh);
    Ok(())
}

// ── §10 Parameter list and function body ─────────────────────────────────────

fn setvararg(fs: &mut FuncState, _state: &mut LuaState, nparams: i32) -> Result<(), LuaError> {
    fs.f.is_vararg = true;
    let inst = lua_code::opcodes::Instruction::abck(
        lua_code::opcodes::OpCode::VarArgPrep,
        nparams as u32,
        0, 0, 0,
    );
    let line = fs.previousline;
    emit_inst(fs, line, inst);
    Ok(())
}

fn parlist(ls: &mut LexState, state: &mut LuaState) -> Result<(), LuaError> {
    let mut nparams: i32 = 0;
    let mut isvararg = false;
    if ls.t.token != b')' as TokenKind {
        loop {
            match ls.t.token {
                TK_NAME => {
                    let name = str_check_name(ls, state)?;
                    new_local_var(ls, state, name)?;
                    nparams += 1;
                }
                TK_DOTS => {
                    lex_next(ls, state)?;
                    isvararg = true;
                }
                _ => {
                    return Err(LuaError::syntax(format_args!("<name> or '...' expected")));
                }
            }
            if isvararg || !test_next(ls, state, b',' as TokenKind)? {
                break;
            }
        }
    }
    adjust_local_vars(ls, state, nparams)?;
    let numparams = ls.fs.as_ref().unwrap().nactvar;
    ls.fs.as_mut().unwrap().f.numparams = numparams;
    if isvararg {
        setvararg(ls.fs.as_mut().unwrap(), state, numparams as i32)?;
    }
    let nactvar = ls.fs.as_ref().unwrap().nactvar as i32;
    reserve_regs(ls.fs.as_mut().unwrap(), nactvar)?;
    Ok(())
}

fn check_match(
    ls: &mut LexState,
    state: &mut LuaState,
    what: TokenKind,
    who: TokenKind,
    where_line: i32,
) -> Result<(), LuaError> {
    if !test_next(ls, state, what)? {
        if where_line == ls.linenumber {
            return Err(error_expected(ls, what));
        } else {
            let what_str = lua_lex::token2str(&ls.lex, what);
            let who_str = lua_lex::token2str(&ls.lex, who);
            let mut msg: Vec<u8> = Vec::new();
            msg.extend_from_slice(&what_str);
            msg.extend_from_slice(b" expected (to close ");
            msg.extend_from_slice(&who_str);
            use std::io::Write as _;
            let _ = write!(msg, " at line {})", where_line);
            return Err(lua_lex::syntax_error(&mut ls.lex, &msg));
        }
    }
    Ok(())
}

fn body(
    ls: &mut LexState,
    state: &mut LuaState,
    e: &mut ExprDesc,
    ismethod: bool,
    line: i32,
) -> Result<(), LuaError> {
    let new_proto = add_prototype(ls, state)?;
    let mut new_fs = FuncState {
        f: new_proto,
        prev: None,
        bl: None,
        pc: 0,
        lasttarget: 0,
        previousline: line,
        nk: 0,
        np: 0,
        nabslineinfo: 0,
        firstlocal: 0,
        firstlabel: 0,
        ndebugvars: 0,
        nactvar: 0,
        nups: 0,
        freereg: 0,
        iwthabs: 0,
        needclose: false,
        last_token_line: ls.lastline,
    };
    new_fs.f.linedefined = line;
    open_func(ls, state, new_fs)?;

    check_next(ls, state, b'(' as TokenKind)?;
    if ismethod {
        let self_str = state.intern_str(b"self")?;
        new_local_var(ls, state, self_str)?;
        adjust_local_vars(ls, state, 1)?;
    }
    parlist(ls, state)?;
    check_next(ls, state, b')' as TokenKind)?;
    statlist(ls, state)?;
    ls.fs.as_mut().unwrap().f.lastlinedefined = ls.linenumber;
    check_match(ls, state, TK_END, TK_FUNCTION, line)?;
    codeclosure(ls, state, e)?;
    let inner_proto = close_func(ls, state)?;
    let parent = ls.fs.as_mut().expect("body: close_func left no parent FuncState");
    let slot = (parent.np - 1) as usize;
    if parent.f.p.len() <= slot {
        parent.f.p.resize_with(slot + 1, || GcRef::new(LuaProto::placeholder()));
    }
    parent.f.p[slot] = GcRef::new(*inner_proto);
    Ok(())
}

// ── §11 Expression list and function arguments ────────────────────────────────

fn explist(ls: &mut LexState, state: &mut LuaState, v: &mut ExprDesc) -> Result<i32, LuaError> {
    let mut n = 1;
    expr(ls, state, v)?;
    while test_next(ls, state, b',' as TokenKind)? {
        let line = ls.lastline;
        cg_exp_to_next_reg(ls.fs.as_mut().unwrap(), line, v)?;
        expr(ls, state, v)?;
        n += 1;
    }
    Ok(n)
}

fn funcargs(ls: &mut LexState, state: &mut LuaState, f: &mut ExprDesc) -> Result<(), LuaError> {
    let mut args = ExprDesc::default();
    // BEFORE consuming, so the OP_CALL/etc emissions attribute to the call site.
    // errors.lua tests `a\n(\n23)` expects error at line of `(`, not line of `a`.
    let line = ls.linenumber;
    match ls.t.token {
        c if c == b'(' as TokenKind => {
            lex_next(ls, state)?; // skip '('
            if ls.t.token == b')' as TokenKind {
                args.k = ExprKind::Void;
            } else {
                explist(ls, state, &mut args)?;
                if args.k.has_mult_ret() {
                    // Call/VarArg to produce LUA_MULTRET so all of its return
                    // values become arguments to the enclosing call.
                    cg_set_returns(ls.fs.as_mut().unwrap(), &mut args, LUA_MULTRET);
                }
            }
            check_match(ls, state, b')' as TokenKind, b'(' as TokenKind, line)?;
        }
        c if c == b'{' as TokenKind => {
            constructor(ls, state, &mut args)?;
        }
        TK_STRING => {
            let s = ls.t.seminfo.ts.clone()
                .ok_or_else(|| LuaError::syntax(format_args!("string expected")))?;
            codestring(&mut args, s);
            lex_next(ls, state)?;
        }
        _ => {
            return Err(LuaError::syntax(format_args!("function arguments expected")));
        }
    }
    debug_assert!(f.k == ExprKind::NonReloc);
    let base = f.u.info;
    let nparams: i32 = if args.k.has_mult_ret() {
        // TODO(port): luaK_setmultret for VVarArg / VCall args; only single
        // non-multret args are supported by the bootstrap codegen.
        LUA_MULTRET
    } else {
        if args.k != ExprKind::Void {
            cg_exp_to_next_reg(ls.fs.as_mut().unwrap(), line, &mut args)?;
        }
        ls.fs.as_ref().unwrap().freereg as i32 - (base + 1)
    };
    let call_inst = lua_code::opcodes::Instruction::abck(
        lua_code::opcodes::OpCode::Call,
        base as u32,
        (nparams + 1) as u32,
        2,
        0,
    );
    let call_pc = emit_inst(ls.fs.as_mut().unwrap(), line, call_inst);
    init_exp(f, ExprKind::Call, call_pc);
    ls.fs.as_mut().unwrap().freereg = base as u8 + 1;
    Ok(())
}

// ── §12 Expression parsing ────────────────────────────────────────────────────

fn primaryexp(ls: &mut LexState, state: &mut LuaState, v: &mut ExprDesc) -> Result<(), LuaError> {
    match ls.t.token {
        c if c == b'(' as TokenKind => {
            let line = ls.lastline;
            lex_next(ls, state)?;
            expr(ls, state, v)?;
            check_match(ls, state, b')' as TokenKind, b'(' as TokenKind, line)?;
            cg_discharge_vars(ls.fs.as_mut().unwrap(), line, v)?;
        }
        TK_NAME => {
            singlevar(ls, state, v)?;
        }
        _ => {
            return Err(lua_lex::syntax_error(&mut ls.lex, b"unexpected symbol"));
        }
    }
    Ok(())
}

fn suffixedexp(ls: &mut LexState, state: &mut LuaState, v: &mut ExprDesc) -> Result<(), LuaError> {
    primaryexp(ls, state, v)?;
    loop {
        match ls.t.token {
            c if c == b'.' as TokenKind => {
                fieldsel(ls, state, v)?;
            }
            c if c == b'[' as TokenKind => {
                let mut key = ExprDesc::default();
                let line = ls.lastline;
                cg_exp_to_any_reg_up(ls.fs.as_mut().unwrap(), line, v)?;
                yindex(ls, state, &mut key)?;
                cg_indexed(ls.fs.as_mut().unwrap(), line, v, &mut key)?;
            }
            c if c == b':' as TokenKind => {
                let mut key = ExprDesc::default();
                lex_next(ls, state)?;
                codename(ls, state, &mut key)?;
                let line = ls.lastline;
                cg_self(ls.fs.as_mut().unwrap(), line, v, &mut key)?;
                funcargs(ls, state, v)?;
            }
            c if c == b'(' as TokenKind || c == TK_STRING || c == b'{' as TokenKind => {
                let line = ls.lastline;
                cg_exp_to_next_reg(ls.fs.as_mut().unwrap(), line, v)?;
                funcargs(ls, state, v)?;
            }
            _ => return Ok(()),
        }
    }
}

fn simpleexp(ls: &mut LexState, state: &mut LuaState, v: &mut ExprDesc) -> Result<(), LuaError> {
    match ls.t.token {
        TK_FLT => {
            init_exp(v, ExprKind::KFlt, 0);
            v.u.nval = ls.t.seminfo.r;
        }
        TK_INT => {
            init_exp(v, ExprKind::KInt, 0);
            v.u.ival = ls.t.seminfo.i;
        }
        TK_STRING => {
            let s = ls.t.seminfo.ts.clone()
                .ok_or_else(|| LuaError::syntax(format_args!("string value missing")))?;
            codestring(v, s);
        }
        TK_NIL => {
            init_exp(v, ExprKind::Nil, 0);
        }
        TK_TRUE => {
            init_exp(v, ExprKind::True, 0);
        }
        TK_FALSE => {
            init_exp(v, ExprKind::False, 0);
        }
        TK_DOTS => {
            let is_vararg = ls.fs.as_ref().unwrap().f.is_vararg;
            if !is_vararg {
                return Err(LuaError::syntax(format_args!(
                    "cannot use '...' outside a vararg function"
                )));
            }
            let line = ls.lastline;
            let inst = lua_code::opcodes::Instruction::abck(
                lua_code::opcodes::OpCode::VarArg,
                0,
                0,
                1,
                0,
            );
            let pc = emit_inst(ls.fs.as_mut().unwrap(), line, inst);
            init_exp(v, ExprKind::VarArg, pc);
        }
        c if c == b'{' as TokenKind => {
            constructor(ls, state, v)?;
            return Ok(());
        }
        TK_FUNCTION => {
            lex_next(ls, state)?;
            let line = ls.lastline;
            body(ls, state, v, false, line)?;
            return Ok(());
        }
        _ => {
            suffixedexp(ls, state, v)?;
            return Ok(());
        }
    }
    lex_next(ls, state)?;
    Ok(())
}

fn getunopr(op: TokenKind) -> UnOpr {
    match op {
        TK_NOT => UnOpr::Not,
        c if c == b'-' as TokenKind => UnOpr::Minus,
        c if c == b'~' as TokenKind => UnOpr::BNot,
        c if c == b'#' as TokenKind => UnOpr::Len,
        _ => UnOpr::NoUnOpr,
    }
}

fn getbinopr(op: TokenKind) -> BinOpr {
    match op {
        c if c == b'+' as TokenKind => BinOpr::Add,
        c if c == b'-' as TokenKind => BinOpr::Sub,
        c if c == b'*' as TokenKind => BinOpr::Mul,
        c if c == b'%' as TokenKind => BinOpr::Mod,
        c if c == b'^' as TokenKind => BinOpr::Pow,
        c if c == b'/' as TokenKind => BinOpr::Div,
        TK_IDIV => BinOpr::IDiv,
        c if c == b'&' as TokenKind => BinOpr::BAnd,
        c if c == b'|' as TokenKind => BinOpr::BOr,
        c if c == b'~' as TokenKind => BinOpr::BXor,
        TK_SHL => BinOpr::Shl,
        TK_SHR => BinOpr::Shr,
        TK_CONCAT => BinOpr::Concat,
        TK_NE => BinOpr::Ne,
        TK_EQ => BinOpr::Eq,
        c if c == b'<' as TokenKind => BinOpr::Lt,
        TK_LE => BinOpr::Le,
        c if c == b'>' as TokenKind => BinOpr::Gt,
        TK_GE => BinOpr::Ge,
        TK_AND => BinOpr::And,
        TK_OR => BinOpr::Or,
        _ => BinOpr::NoBinOpr,
    }
}

/// Parses a sub-expression with operators of priority > `limit`.
/// Returns the first untreated (lower-priority) operator.
fn subexpr(
    ls: &mut LexState,
    state: &mut LuaState,
    v: &mut ExprDesc,
    limit: i32,
) -> Result<BinOpr, LuaError> {
    enter_level(ls)?;

    let uop = getunopr(ls.t.token);
    if uop != UnOpr::NoUnOpr {
        // so this is the operator's own line, not the prior token's.
        let line = ls.linenumber;
        lex_next(ls, state)?; // skip unary operator
        subexpr(ls, state, v, UNARY_PRIORITY)?;
        cg_prefix(ls.fs.as_mut().unwrap(), uop, v, line)?;
    } else {
        simpleexp(ls, state, v)?;
    }

    let mut op = getbinopr(ls.t.token);
    while op != BinOpr::NoBinOpr && PRIORITY[op as usize].0 as i32 > limit {
        let mut v2 = ExprDesc::default();
        // errors.lua's `lineerror` cases check that runtime arith errors are
        // attributed to the operator's line, not the operand's.
        let line = ls.linenumber;
        lex_next(ls, state)?;
        cg_infix(ls.fs.as_mut().unwrap(), op, v, line)?;
        let nextop = subexpr(ls, state, &mut v2, PRIORITY[op as usize].1 as i32)?;
        cg_posfix_fold(ls.fs.as_mut().unwrap(), op, v, &mut v2, line)?;
        op = nextop;
    }

    leave_level(ls);
    Ok(op)
}

fn expr(ls: &mut LexState, state: &mut LuaState, v: &mut ExprDesc) -> Result<(), LuaError> {
    subexpr(ls, state, v, 0)?;
    Ok(())
}

// ── §13 Statement rules ───────────────────────────────────────────────────────

fn block(ls: &mut LexState, state: &mut LuaState) -> Result<(), LuaError> {
    enter_block(ls, false);
    statlist(ls, state)?;
    leave_block(ls, state)?;
    Ok(())
}

/// Checks and fixes register/upvalue conflicts in multi-assignment.
///
/// When a non-indexed LHS variable `v` also appears as the table or key in an
/// indexed LHS variable, the indexed entry must be redirected to a copy made
/// before any assignments occur. For an upvalue table that becomes a register
/// copy, the ExprKind is changed from IndexUp to IndexStr so cg_storevar emits
/// SETFIELD (register table) instead of SETTABUP (upvalue table).
fn check_conflict(
    ls: &mut LexState,
    _state: &mut LuaState,
    lh: &mut LhsAssign,
    v: &ExprDesc,
) -> Result<(), LuaError> {
    let extra = ls.fs.as_ref().unwrap().freereg as i32;
    let line = ls.lastline;
    let mut conflict = false;

    conflict |= check_one_lhs_entry(&mut lh.v, v, extra);
    let mut prev = lh.prev.as_deref_mut();
    while let Some(node) = prev {
        conflict |= check_one_lhs_entry(&mut node.v, v, extra);
        prev = node.prev.as_deref_mut();
    }

    if conflict {
        let fs = ls.fs.as_mut().unwrap();
        let inst = if v.k == ExprKind::Local {
            lua_code::opcodes::Instruction::abck(
                lua_code::opcodes::OpCode::Move,
                extra as u32, v.u.var_ridx as u32, 0, 0,
            )
        } else {
            lua_code::opcodes::Instruction::abck(
                lua_code::opcodes::OpCode::GetUpVal,
                extra as u32, v.u.info as u32, 0, 0,
            )
        };
        emit_inst(fs, line, inst);
        reserve_regs(fs, 1)?;
    }
    Ok(())
}

fn check_one_lhs_entry(entry: &mut ExprDesc, v: &ExprDesc, extra: i32) -> bool {
    if !entry.k.is_indexed() {
        return false;
    }
    let mut found = false;
    if entry.k == ExprKind::IndexUp {
        if v.k == ExprKind::UpVal && entry.u.ind_t == v.u.info as u8 {
            found = true;
            entry.k = ExprKind::IndexStr;
            entry.u.ind_t = extra as u8;
        }
    } else {
        if v.k == ExprKind::Local && entry.u.ind_t == v.u.var_ridx {
            found = true;
            entry.u.ind_t = extra as u8;
        }
        if entry.k == ExprKind::Indexed
            && v.k == ExprKind::Local
            && entry.u.ind_idx == v.u.var_ridx as i16
        {
            found = true;
            entry.u.ind_idx = extra as i16;
        }
    }
    found
}

fn restassign(
    ls: &mut LexState,
    state: &mut LuaState,
    lh: &mut LhsAssign,
    nvars: i32,
) -> Result<(), LuaError> {
    if !lh.v.k.is_var() {
        return Err(lua_lex::syntax_error(&mut ls.lex, b"syntax error"));
    }
    check_readonly(ls, state, &lh.v.clone())?;

    if test_next(ls, state, b',' as TokenKind)? {
        let mut nv_assign = LhsAssign {
            prev: None, // We don't link here — Phase B restructures
            v: ExprDesc::default(),
        };
        suffixedexp(ls, state, &mut nv_assign.v)?;
        if !nv_assign.v.k.is_indexed() {
            check_conflict(ls, state, lh, &nv_assign.v.clone())?;
        }
        enter_level(ls)?;
        restassign(ls, state, &mut nv_assign, nvars + 1)?;
        leave_level(ls);
    } else {
        let mut e = ExprDesc::default();
        check_next(ls, state, b'=' as TokenKind)?;
        let nexps = explist(ls, state, &mut e)?;
        if nexps != nvars {
            adjust_assign(ls, state, nvars, nexps, &mut e)?;
        } else {
            let line = ls.lastline;
            let fs = ls.fs.as_mut().unwrap();
            cg_set_one_ret(fs, &mut e);
            cg_storevar(fs, line, &lh.v, &mut e)?;
            return Ok(());
        }
    }
    let line = ls.lastline;
    let fs = ls.fs.as_mut().unwrap();
    let freereg = fs.freereg as i32 - 1;
    let mut e = ExprDesc::default();
    init_exp(&mut e, ExprKind::NonReloc, freereg);
    cg_storevar(fs, line, &lh.v, &mut e)?;
    Ok(())
}

/// Parses a condition expression; returns its 'exit when false' patch list.
fn cond(ls: &mut LexState, state: &mut LuaState) -> Result<i32, LuaError> {
    let mut v = ExprDesc::default();
    expr(ls, state, &mut v)?;
    if v.k == ExprKind::Nil {
        v.k = ExprKind::False;
    }
    let line = ls.lastline;
    cg_go_if_true(ls.fs.as_mut().unwrap(), line, &mut v)?;
    Ok(v.f)
}

fn gotostat(ls: &mut LexState, state: &mut LuaState) -> Result<(), LuaError> {
    let line = ls.lastline;
    let name = str_check_name(ls, state)?;
    let lb = findlabel(ls, &name);
    if lb.is_none() {
        let pc = cg_jump(ls.fs.as_mut().unwrap(), line);
        new_goto_entry(ls, state, name, line, pc)?;
    } else {
        let lb_idx = lb.unwrap();
        let lb_pc = ls.dyd.label[lb_idx].pc;
        let lb_nactvar = ls.dyd.label[lb_idx].nactvar;
        let lblevel = reg_level(ls, ls.fs.as_ref().unwrap(), lb_nactvar as i32);
        let cur_nvarstack = {
            let fs = ls.fs.as_ref().unwrap();
            nvarstack(ls, fs)
        };
        if cur_nvarstack > lblevel {
            let inst = lua_code::opcodes::Instruction::abck(
                lua_code::opcodes::OpCode::Close,
                lblevel as u32,
                0,
                0,
                0,
            );
            emit_inst(ls.fs.as_mut().unwrap(), line, inst);
        }
        let jpc = cg_jump(ls.fs.as_mut().unwrap(), line);
        cg_patch_list(ls.fs.as_mut().unwrap(), jpc, lb_pc)?;
    }
    Ok(())
}

fn breakstat(ls: &mut LexState, state: &mut LuaState) -> Result<(), LuaError> {
    let line = ls.lastline;
    lex_next(ls, state)?;
    let break_str = state.intern_str(b"break")?;
    let pc = cg_jump(ls.fs.as_mut().unwrap(), line);
    new_goto_entry(ls, state, break_str, line, pc)?;
    Ok(())
}

fn checkrepeated(ls: &LexState, name: &GcRef<LuaString>) -> Result<(), LuaError> {
    if let Some(lb_idx) = findlabel(ls, name) {
        let name_str = String::from_utf8_lossy(name.as_bytes());
        let line = ls.dyd.label[lb_idx].line;
        return Err(LuaError::syntax(format_args!(
            "label '{}' already defined on line {}", name_str, line
        )));
    }
    Ok(())
}

fn labelstat(
    ls: &mut LexState,
    state: &mut LuaState,
    name: GcRef<LuaString>,
    line: i32,
) -> Result<(), LuaError> {
    check_next(ls, state, TK_DBCOLON)?;
    while ls.t.token == b';' as TokenKind || ls.t.token == TK_DBCOLON {
        statement(ls, state)?;
    }
    checkrepeated(ls, &name)?;
    let is_last = block_follow(ls, false);
    createlabel(ls, state, name, line, is_last)?;
    Ok(())
}

fn whilestat(ls: &mut LexState, state: &mut LuaState, line: i32) -> Result<(), LuaError> {
    lex_next(ls, state)?;
    let whileinit = cg_get_label(ls.fs.as_mut().unwrap());
    let condexit = cond(ls, state)?;
    enter_block(ls, true);
    check_next(ls, state, TK_DO)?;
    block(ls, state)?;
    // Use `lastline` (line of the just-parsed body's last token) rather than
    // `linenumber` (which has already advanced to END) so the back-jump's
    // line attribution matches lua-c's bytecode and the line hook does not
    // spuriously fire for the END line on every iteration.
    let back = cg_jump(ls.fs.as_mut().unwrap(), ls.lastline);
    cg_patch_list(ls.fs.as_mut().unwrap(), back, whileinit)?;
    check_match(ls, state, TK_END, TK_WHILE, line)?;
    leave_block(ls, state)?;
    cg_patch_to_here(ls.fs.as_mut().unwrap(), condexit)?;
    Ok(())
}

fn repeatstat(ls: &mut LexState, state: &mut LuaState, line: i32) -> Result<(), LuaError> {
    let repeat_init = cg_get_label(ls.fs.as_mut().unwrap());
    enter_block(ls, true);
    enter_block(ls, false);
    lex_next(ls, state)?;
    statlist(ls, state)?;
    check_match(ls, state, TK_UNTIL, TK_REPEAT, line)?;
    let condexit = cond(ls, state)?;

    let bl2_upval = ls.fs.as_ref().unwrap().bl.as_ref().unwrap().upval;
    let bl2_nactvar = ls.fs.as_ref().unwrap().bl.as_ref().unwrap().nactvar as i32;
    leave_block(ls, state)?;

    let mut condexit = condexit;
    if bl2_upval {
        let exit = cg_jump(ls.fs.as_mut().unwrap(), line);
        cg_patch_to_here(ls.fs.as_mut().unwrap(), condexit)?;
        let close_level = reg_level(ls, ls.fs.as_ref().unwrap(), bl2_nactvar) as u32;
        let close_inst = lua_code::opcodes::Instruction::abck(
            lua_code::opcodes::OpCode::Close,
            close_level,
            0,
            0,
            0,
        );
        emit_inst(ls.fs.as_mut().unwrap(), line, close_inst);
        condexit = cg_jump(ls.fs.as_mut().unwrap(), line);
        cg_patch_to_here(ls.fs.as_mut().unwrap(), exit)?;
    }
    cg_patch_list(ls.fs.as_mut().unwrap(), condexit, repeat_init)?;
    leave_block(ls, state)?;
    Ok(())
}

/// Parse an expression and emit it to the next register.
fn exp1(ls: &mut LexState, state: &mut LuaState) -> Result<(), LuaError> {
    let mut e = ExprDesc::default();
    expr(ls, state, &mut e)?;
    let line = ls.lastline;
    cg_exp_to_next_reg(ls.fs.as_mut().unwrap(), line, &mut e)?;
    debug_assert!(e.k == ExprKind::NonReloc);
    Ok(())
}

fn fixforjump(fs: &mut FuncState, pc: i32, dest: i32, back: bool) -> Result<(), LuaError> {
    let mut offset = dest - (pc + 1);
    if back {
        offset = -offset;
    }
    if offset > MAXARG_BX {
        return Err(LuaError::syntax(format_args!("control structure too long")));
    }
    let raw = fs.f.code[pc as usize].0;
    let mut inst = lua_code::opcodes::Instruction(raw);
    inst.set_arg_bx(offset as u32);
    fs.f.code[pc as usize] = lua_types::opcode::Instruction::new(inst.0);
    Ok(())
}

fn forbody(
    ls: &mut LexState,
    state: &mut LuaState,
    base: i32,
    line: i32,
    nvars: i32,
    isgen: bool,
) -> Result<(), LuaError> {
    check_next(ls, state, TK_DO)?;
    let prep_op = if isgen { OpCode::TForPrep } else { OpCode::ForPrep };
    let prep = {
        let fs = ls.fs.as_mut().unwrap();
        let inst = lua_code::opcodes::Instruction::abx(prep_op, base as u32, 0);
        emit_inst(fs, line, inst)
    };

    enter_block(ls, false);
    adjust_local_vars(ls, state, nvars)?;
    reserve_regs(ls.fs.as_mut().unwrap(), nvars)?;
    block(ls, state)?;
    leave_block(ls, state)?;

    let label_pc = ls.fs.as_ref().unwrap().pc;
    fixforjump(ls.fs.as_mut().unwrap(), prep, label_pc, false)?;

    if isgen {
        let fs = ls.fs.as_mut().unwrap();
        let inst = lua_code::opcodes::Instruction::abck(
            OpCode::TForCall, base as u32, 0, nvars as u32, 0,
        );
        emit_inst(fs, line, inst);
    }
    let loop_op = if isgen { OpCode::TForLoop } else { OpCode::ForLoop };
    let endfor = {
        let fs = ls.fs.as_mut().unwrap();
        let inst = lua_code::opcodes::Instruction::abx(loop_op, base as u32, 0);
        emit_inst(fs, line, inst)
    };
    fixforjump(ls.fs.as_mut().unwrap(), endfor, prep + 1, true)?;
    Ok(())
}

fn fornum(
    ls: &mut LexState,
    state: &mut LuaState,
    varname: GcRef<LuaString>,
    line: i32,
) -> Result<(), LuaError> {
    let base = ls.fs.as_ref().unwrap().freereg as i32;
    let for_state_str = state.intern_str(b"(for state)")?;
    new_local_var(ls, state, for_state_str.clone())?;
    new_local_var(ls, state, for_state_str.clone())?;
    new_local_var(ls, state, for_state_str)?;
    new_local_var(ls, state, varname)?;
    check_next(ls, state, b'=' as TokenKind)?;
    exp1(ls, state)?; // initial value
    check_next(ls, state, b',' as TokenKind)?;
    exp1(ls, state)?; // limit
    if test_next(ls, state, b',' as TokenKind)? {
        exp1(ls, state)?; // optional step
    } else {
        let fs = ls.fs.as_mut().unwrap();
        let reg = fs.freereg as u32;
        let bx = (1i32 + lua_code::opcodes::OFFSET_S_BX) as u32;
        let inst = lua_code::opcodes::Instruction::abx(
            lua_code::opcodes::OpCode::LoadI, reg, bx,
        );
        emit_inst(fs, line, inst);
        reserve_regs(fs, 1)?;
    }
    adjust_local_vars(ls, state, 3)?; // control variables
    forbody(ls, state, base, line, 1, false)?;
    Ok(())
}

fn forlist(
    ls: &mut LexState,
    state: &mut LuaState,
    indexname: GcRef<LuaString>,
) -> Result<(), LuaError> {
    let mut nvars: i32 = 5; // gen, state, control, toclose, 'indexname'
    let base = ls.fs.as_ref().unwrap().freereg as i32;
    let for_state_str = state.intern_str(b"(for state)")?;
    new_local_var(ls, state, for_state_str.clone())?;
    new_local_var(ls, state, for_state_str.clone())?;
    new_local_var(ls, state, for_state_str.clone())?;
    new_local_var(ls, state, for_state_str)?;
    new_local_var(ls, state, indexname)?;
    while test_next(ls, state, b',' as TokenKind)? {
        let extra_name = str_check_name(ls, state)?;
        new_local_var(ls, state, extra_name)?;
        nvars += 1;
    }
    check_next(ls, state, TK_IN)?;
    // After `in`, linenumber is the line of the operand — used for the
    // for-in control instructions so runtime errors point at the operand,
    // not the `in` keyword. errors.lua:401 depends on this.
    let line = ls.linenumber;
    let mut e = ExprDesc::default();
    let nexps = explist(ls, state, &mut e)?;
    adjust_assign(ls, state, 4, nexps, &mut e)?;
    adjust_local_vars(ls, state, 4)?;
    marktobeclosed(ls.fs.as_mut().unwrap()); // last control var must be closed
    // TODO(port): lua_code::check_stack(ls.fs.as_mut().unwrap(), 3)?;
    forbody(ls, state, base, line, nvars - 4, true)?;
    Ok(())
}

fn forstat(ls: &mut LexState, state: &mut LuaState, line: i32) -> Result<(), LuaError> {
    enter_block(ls, true); // scope for loop and control variables
    lex_next(ls, state)?;
    let varname = str_check_name(ls, state)?;
    match ls.t.token {
        c if c == b'=' as TokenKind => fornum(ls, state, varname, line)?,
        c if c == b',' as TokenKind || c == TK_IN => forlist(ls, state, varname)?,
        _ => {
            return Err(LuaError::syntax(format_args!("'=' or 'in' expected")));
        }
    }
    check_match(ls, state, TK_END, TK_FOR, line)?;
    leave_block(ls, state)?; // loop scope ('break' jumps to this point)
    Ok(())
}

fn test_then_block(
    ls: &mut LexState,
    state: &mut LuaState,
    escapelist: &mut i32,
) -> Result<(), LuaError> {
    lex_next(ls, state)?;
    let mut v = ExprDesc::default();
    expr(ls, state, &mut v)?;
    check_next(ls, state, TK_THEN)?;

    let jf: i32;
    if ls.t.token == TK_BREAK {
        let line = ls.lastline;
        cg_go_if_false(ls.fs.as_mut().unwrap(), line, &mut v)?;
        lex_next(ls, state)?; // skip 'break'
        enter_block(ls, false);
        let break_str = state.intern_str(b"break")?;
        new_goto_entry(ls, state, break_str, line, v.t)?;
        while test_next(ls, state, b';' as TokenKind)? {}
        if block_follow(ls, false) {
            leave_block(ls, state)?;
            return Ok(());
        } else {
            jf = cg_jump(ls.fs.as_mut().unwrap(), ls.linenumber);
        }
    } else {
        let line = ls.lastline;
        cg_go_if_true(ls.fs.as_mut().unwrap(), line, &mut v)?;
        enter_block(ls, false);
        jf = v.f;
    }

    statlist(ls, state)?;
    leave_block(ls, state)?;

    if ls.t.token == TK_ELSE || ls.t.token == TK_ELSEIF {
        let line = ls.lastline;
        let j = cg_jump(ls.fs.as_mut().unwrap(), line);
        cg_concat(ls.fs.as_mut().unwrap(), escapelist, j)?;
    }
    cg_patch_to_here(ls.fs.as_mut().unwrap(), jf)?;
    Ok(())
}

fn ifstat(ls: &mut LexState, state: &mut LuaState, line: i32) -> Result<(), LuaError> {
    let mut escapelist = NO_JUMP;
    test_then_block(ls, state, &mut escapelist)?; // IF cond THEN block
    while ls.t.token == TK_ELSEIF {
        test_then_block(ls, state, &mut escapelist)?;
    }
    if test_next(ls, state, TK_ELSE)? {
        block(ls, state)?;
    }
    check_match(ls, state, TK_END, TK_IF, line)?;
    cg_patch_to_here(ls.fs.as_mut().unwrap(), escapelist)?;
    Ok(())
}

fn localfunc(ls: &mut LexState, state: &mut LuaState) -> Result<(), LuaError> {
    let mut b = ExprDesc::default();
    let _fvar = ls.fs.as_ref().unwrap().nactvar as i32;
    let name = str_check_name(ls, state)?;
    new_local_var(ls, state, name)?;
    adjust_local_vars(ls, state, 1)?; // enter its scope
    let line = ls.lastline;
    body(ls, state, &mut b, false, line)?;
    let _pc = ls.fs.as_ref().unwrap().pc;
    // TODO(port): local_debug_info(ls, ls.fs.as_mut().unwrap(), fvar).map(|lv| lv.startpc = pc);
    Ok(())
}

/// Whether the active Lua version supports `<const>`/`<close>` local-variable
/// attributes. True for 5.4 and 5.5; false for 5.1/5.2/5.3.
fn version_has_attributes(state: &LuaState) -> bool {
    use lua_types::LuaVersion;
    matches!(
        state.global().lua_version,
        LuaVersion::V54 | LuaVersion::V55
    )
}

/// Parses an optional '<const>' or '<close>' attribute.
///
/// The `<const>`/`<close>` attribute syntax is a Lua 5.4 addition (5.4 §8.1,
/// `specs/research/5.3-upstream-delta.md` delta #7). Under Lua 5.3 the `<`
/// after a local name is not an attribute opener at all — it is left
/// unconsumed so the surrounding statement reports it as an unexpected symbol,
/// exactly as the 5.3 parser does. Versions are gated on
/// [`lua_types::LuaVersion`], read from the state set at construction.
fn getlocalattribute(ls: &mut LexState, state: &mut LuaState) -> Result<VarKind, LuaError> {
    if !version_has_attributes(state) {
        return Ok(VarKind::Reg);
    }
    if test_next(ls, state, b'<' as TokenKind)? {
        let attr_name = str_check_name(ls, state)?;
        check_next(ls, state, b'>' as TokenKind)?;
        let bytes = attr_name.as_bytes();
        if bytes == b"const" {
            return Ok(VarKind::Const);
        } else if bytes == b"close" {
            return Ok(VarKind::ToBeClosed);
        } else {
            let name_str = String::from_utf8_lossy(bytes);
            return Err(LuaError::syntax(format_args!(
                "unknown attribute '{}'", name_str
            )));
        }
    }
    Ok(VarKind::Reg)
}

fn checktoclose(ls: &mut LexState, _state: &mut LuaState, level: i32) -> Result<(), LuaError> {
    if level != -1 {
        marktobeclosed(ls.fs.as_mut().unwrap());
        let rl = reg_level(ls, ls.fs.as_ref().unwrap(), level);
        let line = ls.lastline;
        let inst = lua_code::opcodes::Instruction::abck(
            lua_code::opcodes::OpCode::Tbc,
            rl as u32,
            0,
            0,
            0,
        );
        emit_inst(ls.fs.as_mut().unwrap(), line, inst);
    }
    Ok(())
}

fn localstat(ls: &mut LexState, state: &mut LuaState) -> Result<(), LuaError> {
    let mut toclose: i32 = -1;
    let mut nvars: i32 = 0;
    let mut vidx: i32;
    loop {
        let name = str_check_name(ls, state)?;
        vidx = new_local_var(ls, state, name)?;
        let kind = getlocalattribute(ls, state)?;
        get_local_var_desc_mut(ls, ls.fs.as_ref().unwrap().firstlocal, vidx).kind = kind;
        if kind == VarKind::ToBeClosed {
            if toclose != -1 {
                return Err(LuaError::syntax(format_args!(
                    "multiple to-be-closed variables in local list"
                )));
            }
            toclose = ls.fs.as_ref().unwrap().nactvar as i32 + nvars;
        }
        nvars += 1;
        if !test_next(ls, state, b',' as TokenKind)? {
            break;
        }
    }
    let nexps: i32;
    let mut e = ExprDesc::default();
    if test_next(ls, state, b'=' as TokenKind)? {
        nexps = explist(ls, state, &mut e)?;
    } else {
        e.k = ExprKind::Void;
        nexps = 0;
    }
    let first_local = ls.fs.as_ref().unwrap().firstlocal;
    let last_vd_kind = ls.dyd.actvar[(first_local + vidx) as usize].kind;
    if nvars == nexps
        && last_vd_kind == VarKind::Const
    {
        // TODO(port): let is_const = lua_code::exp_to_const(ls.fs.as_mut().unwrap(), &mut e, &mut var_k)?;
        let is_const = false; // placeholder
        if is_const {
            ls.dyd.actvar[(first_local + vidx) as usize].kind = VarKind::CompileTimeConst;
            adjust_local_vars(ls, state, nvars - 1)?;
            ls.fs.as_mut().unwrap().nactvar += 1;
        } else {
            adjust_assign(ls, state, nvars, nexps, &mut e)?;
            adjust_local_vars(ls, state, nvars)?;
        }
    } else {
        adjust_assign(ls, state, nvars, nexps, &mut e)?;
        adjust_local_vars(ls, state, nvars)?;
    }
    checktoclose(ls, state, toclose)?;
    Ok(())
}

/// Parses a function name (NAME {'.' NAME} [':' NAME]). Returns ismethod.
fn funcname(ls: &mut LexState, state: &mut LuaState, v: &mut ExprDesc) -> Result<bool, LuaError> {
    let mut ismethod = false;
    singlevar(ls, state, v)?;
    while ls.t.token == b'.' as TokenKind {
        fieldsel(ls, state, v)?;
    }
    if ls.t.token == b':' as TokenKind {
        ismethod = true;
        fieldsel(ls, state, v)?;
    }
    Ok(ismethod)
}

fn funcstat(ls: &mut LexState, state: &mut LuaState, line: i32) -> Result<(), LuaError> {
    lex_next(ls, state)?;
    let mut v = ExprDesc::default();
    let mut b = ExprDesc::default();
    let ismethod = funcname(ls, state, &mut v)?;
    body(ls, state, &mut b, ismethod, line)?;
    check_readonly(ls, state, &v.clone())?;
    let fs = ls.fs.as_mut().unwrap();
    cg_storevar(fs, line, &v, &mut b)?;
    // TODO(port): lua_code::fix_line(ls.fs.as_mut().unwrap(), line);
    Ok(())
}

fn exprstat(ls: &mut LexState, state: &mut LuaState) -> Result<(), LuaError> {
    let mut v_assign = LhsAssign { prev: None, v: ExprDesc::default() };
    suffixedexp(ls, state, &mut v_assign.v)?;
    if ls.t.token == b'=' as TokenKind || ls.t.token == b',' as TokenKind {
        restassign(ls, state, &mut v_assign, 1)?;
    } else {
        if v_assign.v.k != ExprKind::Call {
            return Err(lua_lex::syntax_error(&mut ls.lex, b"syntax error"));
        }
        let info = v_assign.v.u.info as usize;
        let fs = ls.fs.as_mut().unwrap();
        let mut lc = lua_code::opcodes::Instruction(fs.f.code[info].0);
        lc.set_arg_c(1);
        fs.f.code[info] = lua_types::opcode::Instruction::new(lc.0);
    }
    Ok(())
}

fn retstat(ls: &mut LexState, state: &mut LuaState) -> Result<(), LuaError> {
    let mut first = {
        let fs = ls.fs.as_ref().unwrap();
        nvarstack(ls, fs)
    };
    let mut nret: i32;
    if block_follow(ls, true) || ls.t.token == b';' as TokenKind {
        nret = 0;
    } else {
        let mut e = ExprDesc::default();
        nret = explist(ls, state, &mut e)?;
        if e.k.has_mult_ret() {
            cg_set_returns(ls.fs.as_mut().unwrap(), &mut e, LUA_MULTRET);
            if e.k == ExprKind::Call && nret == 1 {
                let insidetbc = ls.fs.as_ref().unwrap().bl.as_ref().map_or(false, |b| b.insidetbc);
                if !insidetbc {
                    let fs = ls.fs.as_mut().unwrap();
                    let info = e.u.info as usize;
                    let mut lc = lua_code::opcodes::Instruction(fs.f.code[info].0);
                    lc.set_opcode(lua_code::opcodes::OpCode::TailCall);
                    fs.f.code[info] = lua_types::opcode::Instruction::new(lc.0);
                }
            }
            nret = LUA_MULTRET;
        } else {
            let line = ls.lastline;
            if nret == 1 {
                first = cg_exp_to_any_reg(ls.fs.as_mut().unwrap(), line, &mut e)? as i32;
            } else {
                cg_exp_to_next_reg(ls.fs.as_mut().unwrap(), line, &mut e)?;
            }
        }
    }
    let line = ls.lastline;
    cg_emit_return(ls.fs.as_mut().unwrap(), line, first, nret);
    test_next(ls, state, b';' as TokenKind)?;
    Ok(())
}

/// Top-level statement dispatcher.
fn statement(ls: &mut LexState, state: &mut LuaState) -> Result<(), LuaError> {
    // This is the line of the current keyword (for/while/if/...), captured
    // BEFORE consuming. Used both for error messages on unmatched blocks
    // AND for runtime-error line attribution on control-flow instructions
    // (FORPREP, etc). errors.lua's lineerror tests depend on this.
    let line = ls.linenumber;
    enter_level(ls)?;
    match ls.t.token {
        c if c == b';' as TokenKind => {
            lex_next(ls, state)?;
        }
        TK_IF => {
            ifstat(ls, state, line)?;
        }
        TK_WHILE => {
            whilestat(ls, state, line)?;
        }
        TK_DO => {
            lex_next(ls, state)?; // skip DO
            block(ls, state)?;
            check_match(ls, state, TK_END, TK_DO, line)?;
        }
        TK_FOR => {
            forstat(ls, state, line)?;
        }
        TK_REPEAT => {
            repeatstat(ls, state, line)?;
        }
        TK_FUNCTION => {
            funcstat(ls, state, line)?;
        }
        TK_LOCAL => {
            lex_next(ls, state)?; // skip LOCAL
            if test_next(ls, state, TK_FUNCTION)? {
                localfunc(ls, state)?;
            } else {
                localstat(ls, state)?;
            }
        }
        TK_DBCOLON => {
            lex_next(ls, state)?; // skip '::'
            let name = str_check_name(ls, state)?;
            labelstat(ls, state, name, line)?;
        }
        TK_RETURN => {
            lex_next(ls, state)?; // skip RETURN
            retstat(ls, state)?;
        }
        TK_BREAK => {
            breakstat(ls, state)?;
        }
        TK_GOTO => {
            lex_next(ls, state)?; // skip 'goto'
            gotostat(ls, state)?;
        }
        _ => {
            exprstat(ls, state)?;
        }
    }
    debug_assert!(
        ls.fs.as_ref().unwrap().f.maxstacksize >= ls.fs.as_ref().unwrap().freereg
            && ls.fs.as_ref().unwrap().freereg as i32
                >= nvarstack(ls, ls.fs.as_ref().unwrap())
    );
    let nv = nvarstack(ls, ls.fs.as_ref().unwrap());
    ls.fs.as_mut().unwrap().freereg = nv as u8;
    leave_level(ls);
    Ok(())
}

// ── §14 Main function and entry point ────────────────────────────────────────

/// Compiles the main chunk (always a vararg function with _ENV upvalue).
fn mainfunc(ls: &mut LexState, state: &mut LuaState, main_fs: FuncState) -> Result<Box<LuaProto>, LuaError> {
    open_func(ls, state, main_fs)?;

    setvararg(ls.fs.as_mut().unwrap(), state, 0)?;

    let env_name = ls.envn.clone();
    {
        let idx = alloc_upvalue(ls.fs.as_mut().unwrap())?;
        let up = &mut ls.fs.as_mut().unwrap().f.upvalues[idx];
        up.instack = true;
        up.idx = 0;
        up.kind = VarKind::Reg.as_u8();
        up.name = env_name.clone();
    }

    lex_next(ls, state)?;

    statlist(ls, state)?;

    check(ls, TK_EOS)?;

    close_func(ls, state)
}

///                           const char *name, int firstchar)
/// Top-level entry point: parses a chunk and returns the main LClosure.
/// LUAI_FUNC visibility.
///
/// PORT NOTE: In C, returns `LClosure *` (a GC object). In Rust (Phase A),
///   we return `Box<LuaProto>` since we don't have GcRef<LuaLClosure> ready.
///   Phase B will wrap this in a proper LuaLClosure / GcRef.
pub fn parse(
    state: &mut LuaState,
    dyd: DynData,
    source: &[u8],
    name: &[u8],
    firstchar: i32,
) -> Result<Box<LuaProto>, LuaError> {
    let source_str = state.intern_str(name)?;
    let envn_str = state.intern_str(lua_lex::LUA_ENV)?;

    let rest_bytes: Vec<u8> = source.iter().skip(1).copied().collect();
    let z = lua_lex::ZIO::from_bytes(rest_bytes);

    let lex_ls = lua_lex::LexState {
        current: firstchar,
        linenumber: 1,
        lastline: 1,
        t: lua_lex::Token::eos(),
        lookahead: lua_lex::Token::eos(),
        fs: None,
        z,
        buff: lua_lex::LexBuffer::new(),
        h: None,
        long_str_anchor: std::collections::HashMap::new(),
        dyd: None,
        source: source_str.clone(),
        envn: envn_str.clone(),
    };

    let mut lexstate = LexState {
        current: lex_ls.current,
        linenumber: lex_ls.linenumber,
        lastline: lex_ls.lastline,
        t: LexToken::default(),
        lookahead: LexToken::default(),
        fs: None,
        dyd,
        source: Some(source_str.clone()),
        envn: Some(lex_ls.envn.clone()),
        lex: lex_ls,
        recursion_depth: 0,
    };
    //   `mainfunc`; it does NOT pre-read the first token. `mainfunc` itself
    //   issues the initial `luaX_next` once its prelude (open_func, vararg
    //   marker, _ENV upvalue) is in place.

    let mut main_proto = Box::new(LuaProto::placeholder());
    main_proto.source = Some(source_str);
    main_proto.is_vararg = true;
    let main_fs = FuncState {
        f: main_proto,
        prev: None,
        bl: None,
        pc: 0,
        lasttarget: 0,
        previousline: 0,
        nk: 0,
        np: 0,
        nabslineinfo: 0,
        firstlocal: 0,
        firstlabel: 0,
        ndebugvars: 0,
        nactvar: 0,
        nups: 0,
        freereg: 0,
        iwthabs: 0,
        needclose: false,
        last_token_line: 0,
    };

    mainfunc(&mut lexstate, state, main_fs)
}

/// Convert a `lua_lex::TokenValue` into the local `parse::TokenValue` flat shape.
///
/// The parser's local `LexState` predates the lex-side enum and uses a flat
/// (r, i, ts) record; this picks out whichever variant the lexer produced.
fn local_token_value(v: &lua_lex::TokenValue) -> TokenValue {
    match v {
        lua_lex::TokenValue::None => TokenValue::default(),
        lua_lex::TokenValue::Float(r) => TokenValue { r: *r, i: 0, ts: None },
        lua_lex::TokenValue::Int(i) => TokenValue { r: 0.0, i: *i, ts: None },
        lua_lex::TokenValue::Str(s) => TokenValue { r: 0.0, i: 0, ts: Some(s.clone()) },
    }
}

// ──────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        src/lparser.c  (1968 lines, 95 functions)
//   target_crate:  lua-parse
//   confidence:    medium
//   todos:         184
//   port_notes:    14
//   unsafe_blocks: 0
//   notes:         All 95 functions translated with correct logical structure.
//                  184 TODO(port) stubs for cross-crate calls (lua_code::*,
//                  lua_lex::*, lua_vm state allocation). Key design choices:
//                  BlockCnt/LhsAssign use Option<Box<...>> chains; FuncState
//                  uses Box<LuaProto> (not GcRef) for mutable access during
//                  build. singlevaraux FuncState.prev chain traversal (upvalue
//                  capture across closures) is a known TODO — needs recursive
//                  descent through fs.prev without double-mutable-borrow.
//                  LexState is a local stub — Phase B must unify with
//                  lua_lex::LexState and add lua-lex as a dep. markupval
//                  BlockCnt chain traversal also needs Phase B restructure.
//                  rustc check: only E0432 (unresolved lua_types import) —
//                  expected Phase A name-resolution error; no syntax errors.
// ──────────────────────────────────────────────────────────────────────────
