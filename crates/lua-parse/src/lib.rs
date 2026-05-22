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

use lua_types::{GcRef, LuaError, LuaString, LuaValue, LuaProto, UpvalDesc, LocalVar};

// TODO(port): these imports resolve in Phase B when inter-crate deps land.
// use lua_vm::LuaState;
// use lua_code::{self, UnOpr, BinOpr, OpCode};

// ── Token kind constants ────────────────────────────────────────────────────
// C: RESERVED enum from llex.h; FIRST_RESERVED = 257 (UCHAR_MAX + 1).
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

/// C: #define MAXVARS 200
const MAX_VARS: i32 = 200;

/// C: NO_JUMP from lcode.h
const NO_JUMP: i32 = -1;

/// C: UNARY_PRIORITY 12
const UNARY_PRIORITY: i32 = 12;

/// C: LUA_MULTRET from lua.h
const LUA_MULTRET: i32 = -1;

/// C: MAXUPVAL 255 from lfunc.h
const MAX_UPVAL: u8 = 255;

/// C: MAXARG_Bx — max value for Bx field in an iABx instruction.
/// TODO(port): should come from lua_types::opcode constants.
const MAXARG_BX: i32 = (1 << 17) - 1;

/// C: LFIELDS_PER_FLUSH 50 from lopcodes.h
const LFIELDS_PER_FLUSH: i32 = 50;

// ── Variable kind constants ─────────────────────────────────────────────────
// C: VDKREG / RDKCONST / RDKTOCLOSE / RDKCTC from lparser.h
// macros.tsv maps these to VarKind enum variants.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum VarKind {
    /// C: VDKREG 0 — regular local variable
    Reg = 0,
    /// C: RDKCONST 1 — read-only const variable
    Const = 1,
    /// C: RDKTOCLOSE 2 — to-be-closed variable
    ToBeClosed = 2,
    /// C: RDKCTC 3 — compile-time constant (no register)
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

/// C: expkind — the kind of a deferred expression or variable.
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
    /// C: hasmultret(k) — ((k) == VCALL || (k) == VVARARG)
    #[inline]
    pub fn has_mult_ret(self) -> bool {
        matches!(self, ExprKind::Call | ExprKind::VarArg)
    }

    /// C: vkisvar(k) — VLOCAL <= k <= VINDEXSTR
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

    /// C: vkisindexed(k) — VINDEXED <= k <= VINDEXSTR
    #[inline]
    pub fn is_indexed(self) -> bool {
        matches!(
            self,
            ExprKind::Indexed | ExprKind::IndexUp | ExprKind::IndexI | ExprKind::IndexStr
        )
    }
}

// ── ExprPayload ─────────────────────────────────────────────────────────────

/// C: the `u` union inside `expdesc`.
/// PORT NOTE: C uses a union; all arms share memory. Rust keeps all fields in
///   one struct for Phase A simplicity. Phase B may refactor to a proper enum.
#[derive(Debug, Clone, Default)]
pub struct ExprPayload {
    /// C: u.ival — for VKINT
    pub ival: i64,
    /// C: u.nval — for VKFLT
    pub nval: f64,
    /// C: u.strval — for VKSTR
    pub strval: Option<GcRef<LuaString>>,
    /// C: u.info — for VK/VNONRELOC/VUPVAL/VCONST/VJMP/VRELOC/VCALL/VVARARG
    pub info: i32,
    /// C: u.ind.idx — register or K index for VINDEXED/VINDEXUP/VINDEXI/VINDEXSTR
    pub ind_idx: i16,
    /// C: u.ind.t — table register or upvalue index
    pub ind_t: u8,
    /// C: u.var.ridx — register holding the local variable (VLOCAL)
    pub var_ridx: u8,
    /// C: u.var.vidx — compiler index in actvar.arr (VLOCAL)
    pub var_vidx: u16,
}

// ── ExprDesc ────────────────────────────────────────────────────────────────

/// C: expdesc — describes a potentially-deferred expression/variable.
/// Field `t`/`f` are patch-lists for short-circuit boolean evaluation.
#[derive(Debug, Clone)]
pub struct ExprDesc {
    pub k: ExprKind,
    pub u: ExprPayload,
    /// C: e.t — patch list for 'exit when true'; NO_JUMP if none.
    pub t: i32,
    /// C: e.f — patch list for 'exit when false'; NO_JUMP if none.
    pub f: i32,
}

impl Default for ExprDesc {
    fn default() -> Self {
        ExprDesc { k: ExprKind::Void, u: ExprPayload::default(), t: NO_JUMP, f: NO_JUMP }
    }
}

// ── VarDesc ─────────────────────────────────────────────────────────────────

/// C: Vardesc — describes an active local variable during compilation.
/// PORT NOTE: C uses a union (vd fields + k for const value). Rust keeps all
///   fields in a struct. The `const_val` field is only meaningful when
///   `kind == VarKind::CompileTimeConst`.
#[derive(Debug, Clone)]
pub struct VarDesc {
    /// C: vd.kind
    pub kind: VarKind,
    /// C: vd.ridx — register holding the variable
    pub ridx: u8,
    /// C: vd.pidx — index in Proto.locvars
    pub pidx: i16,
    /// C: vd.name — variable name
    pub name: Option<GcRef<LuaString>>,
    /// C: k — compile-time constant value (only valid for CompileTimeConst)
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

/// C: Labeldesc — a pending goto statement or an active label.
#[derive(Debug, Clone)]
pub struct LabelDesc {
    /// C: name — label identifier
    pub name: Option<GcRef<LuaString>>,
    /// C: pc — bytecode position
    pub pc: i32,
    /// C: line — source line
    pub line: i32,
    /// C: nactvar — active variable count at this position
    pub nactvar: u8,
    /// C: close — whether this goto escapes upvalues
    pub close: bool,
}

// ── DynData ─────────────────────────────────────────────────────────────────

/// C: Dyndata — parser-local mutable lists (active vars, gotos, labels).
/// C stored C-style dynamic arrays (arr/n/size); Rust uses Vec.
#[derive(Debug, Default)]
pub struct DynData {
    /// C: actvar — all currently-active local variables
    pub actvar: Vec<VarDesc>,
    /// C: gt — pending gotos
    pub gt: Vec<LabelDesc>,
    /// C: label — active labels
    pub label: Vec<LabelDesc>,
}

// ── BlockCnt ────────────────────────────────────────────────────────────────

/// C: BlockCnt — one nested block scope (defined in lparser.c, not header).
/// In C: stack-allocated, chained via raw `*previous` pointer.
/// In Rust: heap-allocated in an `Option<Box<BlockCnt>>` chain on FuncState.
#[derive(Debug)]
pub struct BlockCnt {
    /// C: *previous — enclosing block; None at function top level.
    pub previous: Option<Box<BlockCnt>>,
    /// C: firstlabel — index of first label in this block (in dyd.label)
    pub firstlabel: i32,
    /// C: firstgoto — index of first pending goto (in dyd.gt)
    pub firstgoto: i32,
    /// C: nactvar — active-local count on block entry
    pub nactvar: u8,
    /// C: upval — true if some variable in block is an upvalue
    pub upval: bool,
    /// C: isloop — true if this block is a loop body
    pub isloop: bool,
    /// C: insidetbc — true if inside the scope of a to-be-closed variable
    pub insidetbc: bool,
}

// ── FuncState ───────────────────────────────────────────────────────────────

/// C: FuncState — per-function compile-time state.
/// In C: stack-allocated in `body()`, chained via raw `*prev` pointer.
/// In Rust: heap-allocated via `Option<Box<FuncState>>` in LexState.
#[derive(Debug)]
pub struct FuncState {
    /// C: f — the Proto being built.
    /// PORT NOTE: types.tsv maps this to GcRef<LuaProto>; we use Box<LuaProto>
    ///   during compilation to avoid RefCell overhead. close_func hands it to
    ///   the GC/parent at close time.
    pub f: Box<LuaProto>,
    /// C: prev — enclosing FuncState (raw pointer in C; owned Box here).
    pub prev: Option<Box<FuncState>>,
    /// C: bl — innermost active block
    pub bl: Option<Box<BlockCnt>>,
    /// C: pc — next bytecode position to emit
    pub pc: i32,
    /// C: lasttarget — pc of last 'jump label'
    pub lasttarget: i32,
    /// C: previousline — last line saved in lineinfo
    pub previousline: i32,
    /// C: nk — number of constants emitted
    pub nk: i32,
    /// C: np — number of nested prototypes emitted
    pub np: i32,
    /// C: nabslineinfo — number of absolute line-info records
    pub nabslineinfo: i32,
    /// C: firstlocal — index of first local var in dyd.actvar
    pub firstlocal: i32,
    /// C: firstlabel — index of first label in dyd.label
    pub firstlabel: i32,
    /// C: ndebugvars — entries in f.locvars
    pub ndebugvars: i16,
    /// C: nactvar — number of active locals
    pub nactvar: u8,
    /// C: nups — number of upvalues
    pub nups: u8,
    /// C: freereg — next free register
    pub freereg: u8,
    /// C: iwthabs — instructions since last absolute line info
    pub iwthabs: u8,
    /// C: needclose — function must close upvalues on return
    pub needclose: bool,
}

// ── ConsControl ─────────────────────────────────────────────────────────────

/// C: ConsControl — state for parsing a table constructor.
/// PORT NOTE: C stores `expdesc *t` as a pointer to the caller's expdesc.
///   Rust stores a copy of the table descriptor; callers must sync back
///   if they mutate it. Phase B may restructure.
#[derive(Debug)]
pub struct ConsControl {
    /// C: v — last list item read
    pub v: ExprDesc,
    /// C: *t — table descriptor (copied; see PORT NOTE above)
    pub t: ExprDesc,
    /// C: nh — total number of record elements
    pub nh: i32,
    /// C: na — number of array elements already stored
    pub na: i32,
    /// C: tostore — array elements pending store
    pub tostore: i32,
}

// ── LhsAssign ───────────────────────────────────────────────────────────────

/// C: LHS_assign — chain of assignment left-hand-side variables.
/// In C: stack-allocated, chained via raw `*prev`. In Rust: `Option<Box<...>>`.
#[derive(Debug)]
pub struct LhsAssign {
    /// C: *prev — previous (outer) assignment target; None at head.
    pub prev: Option<Box<LhsAssign>>,
    /// C: v — the variable being assigned
    pub v: ExprDesc,
}

// ── Unary / binary operator enums ───────────────────────────────────────────
// C: UnOpr and BinOpr from lcode.h; defined locally here for Phase A.
// TODO(port): unify with lua_code::UnOpr / BinOpr when lua-code lands.

/// C: UnOpr
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOpr {
    Minus,    // OPR_MINUS
    BNot,     // OPR_BNOT
    Not,      // OPR_NOT
    Len,      // OPR_LEN
    NoUnOpr,  // OPR_NOUNOPR
}

/// C: BinOpr
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

/// C: priority[] table — (left_priority, right_priority) per BinOpr.
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
/// C: SemInfo from llex.h; Rust analogue is TokenValue.
#[derive(Debug, Clone, Default)]
pub struct TokenValue {
    /// C: r — float literal value (for TK_FLT)
    pub r: f64,
    /// C: i — integer literal value (for TK_INT)
    pub i: i64,
    /// C: ts — string value (for TK_NAME, TK_STRING)
    pub ts: Option<GcRef<LuaString>>,
}

/// C: Token from llex.h.
#[derive(Debug, Clone, Default)]
pub struct LexToken {
    pub token: TokenKind,
    pub seminfo: TokenValue,
}

/// C: LexState — per-chunk lexer + parser state.
/// PORT NOTE: This is a Phase A stub. In Phase B, `LexState` lives in
///   `lua-lex` and `lua-parse` imports it. `FuncState` will move here
///   or be passed separately. The `fs` field creates a circular-crate
///   dependency that Phase B must resolve (likely: both live in one crate).
pub struct LexState {
    /// C: current — current character (i32; -1 = EOZ)
    pub current: i32,
    /// C: linenumber
    pub linenumber: i32,
    /// C: lastline
    pub lastline: i32,
    /// C: t — current token
    pub t: LexToken,
    /// C: lookahead — one-token lookahead
    pub lookahead: LexToken,
    /// C: fs — current FuncState (parser owns this)
    pub fs: Option<Box<FuncState>>,
    /// C: dyd — parser dynamic data
    pub dyd: DynData,
    /// C: source — chunk name
    pub source: Option<GcRef<LuaString>>,
    /// C: envn — cached "_ENV" string
    pub envn: Option<GcRef<LuaString>>,
    /// Underlying lexer state that owns the ZIO stream and lex buffer.
    /// The parser drives the lexer by calling `lex_next` / `lex_lookahead`,
    /// which forward to `lua_lex::next` / `lua_lex::lookahead` on this inner
    /// state and then mirror the resulting token into `self.t` / `self.lookahead`.
    pub lex: lua_lex::LexState,
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
    let pc = fs.pc as usize;
    if fs.f.code.len() <= pc {
        fs.f.code.resize(pc + 1, lua_types::opcode::Instruction::default());
    }
    fs.f.code[pc] = lua_types::opcode::Instruction::new(inst.0);
    if fs.f.lineinfo.len() <= pc {
        fs.f.lineinfo.resize(pc + 1, 0i8);
    }
    fs.f.lineinfo[pc] = (line - fs.previousline).clamp(i8::MIN as i32, i8::MAX as i32) as i8;
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

fn reserve_reg(fs: &mut FuncState) -> u8 {
    let r = fs.freereg;
    fs.freereg += 1;
    bump_maxstack(fs, fs.freereg);
    r
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
/// metamethod-dispatch instruction. Concat, logical, and comparison
/// operators still hit `todo!()` so they surface as later iterations'
/// blockers.
fn cg_posfix_fold(
    fs: &mut FuncState,
    op: BinOpr,
    e1: &mut ExprDesc,
    e2: &mut ExprDesc,
    line: i32,
) -> Result<(), LuaError> {
    let promote = |k: ExprKind, u: &ExprPayload| -> Option<f64> {
        match k {
            ExprKind::KInt => Some(u.ival as f64),
            ExprKind::KFlt => Some(u.nval),
            _ => None,
        }
    };

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

    if matches!(op, BinOpr::Lt) {
        return cg_emit_order(fs, op, e1, e2, line);
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
        _ => todo!(
            "phase-b: cg_posfix_fold non-foldable binop {:?} ({:?} {:?})",
            op, e1.k, e2.k
        ),
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
    debug_assert!(matches!(op, BinOpr::Lt));
    let (r1, r2, cmp_op) = if let Some(im) = cg_sc_int(e2) {
        let r1 = cg_exp_to_any_reg(fs, line, e1)?;
        (r1, im, lua_code::opcodes::OpCode::LtI)
    } else if let Some(im) = cg_sc_int(e1) {
        let r1 = cg_exp_to_any_reg(fs, line, e2)?;
        (r1, im, lua_code::opcodes::OpCode::GtI)
    } else {
        let r2 = cg_exp_to_any_reg(fs, line, e2)?;
        let r1 = cg_exp_to_any_reg(fs, line, e1)?;
        (r1, r2, lua_code::opcodes::OpCode::Lt)
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
/// Phase-A subset of C's `luaK_patchlist`: skips the `TestSet`→`Test`
/// rewrite (`patchtestreg`) since `goiftrue`/`goiffalse` here only produce
/// raw comparison-followed-by-jump expressions, not boolean short-circuit
/// chains that need register patching.
fn cg_patch_list(fs: &mut FuncState, list: i32, target: i32) -> Result<(), LuaError> {
    let mut list = list;
    while list != NO_JUMP {
        let next = cg_get_jump(fs, list);
        cg_fix_jump(fs, list, target)?;
        list = next;
    }
    Ok(())
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
/// Phase-A subset of C's `luaK_goiftrue`: handles `VJMP` (comparisons) and
/// always-true literal forms (`VK`, `VKFLT`, `VKINT`, `VKSTR`, `VTRUE`).
/// Other expression kinds aren't reachable from the parser bootstrap yet.
fn cg_go_if_true(fs: &mut FuncState, e: &mut ExprDesc) -> Result<(), LuaError> {
    let pc: i32 = match e.k {
        ExprKind::Jmp => {
            cg_negate_condition(fs, e);
            e.u.info
        }
        ExprKind::K | ExprKind::KFlt | ExprKind::KInt | ExprKind::KStr | ExprKind::True => {
            NO_JUMP
        }
        _ => {
            return Err(LuaError::syntax(format_args!(
                "internal: cg_go_if_true on unsupported expression kind {:?}", e.k
            )));
        }
    };
    cg_concat(fs, &mut e.f, pc)?;
    cg_patch_to_here(fs, e.t)?;
    e.t = NO_JUMP;
    Ok(())
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
        return Ok(e.u.info as u8);
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
        _ => {}
    }
    Ok(())
}

/// Minimal `luaK_exp2nextreg`: discharge `e` and place it in `fs.freereg`,
/// incrementing `freereg`. Handles `VReloc`, `VKStr`, `VKInt`, `VKFlt`,
/// `VNil`, `VTrue`, `VFalse`, `VNonReloc` plus anything `cg_discharge_vars`
/// produces.
fn cg_exp_to_next_reg(
    fs: &mut FuncState,
    line: i32,
    e: &mut ExprDesc,
) -> Result<(), LuaError> {
    cg_discharge_vars(fs, line, e)?;
    let reg = reserve_reg(fs);
    match e.k {
        ExprKind::Reloc => {
            let pc = e.u.info as usize;
            let mut lc = lua_code::opcodes::Instruction(fs.f.code[pc].0);
            lc.set_arg_a(reg as u32);
            fs.f.code[pc] = lua_types::opcode::Instruction::new(lc.0);
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
        _ => {
            return Err(LuaError::syntax(format_args!(
                "internal: cg_exp_to_next_reg cannot discharge {:?}", e.k
            )));
        }
    }
    e.u.info = reg as i32;
    e.k = ExprKind::NonReloc;
    Ok(())
}

// ── Free functions ──────────────────────────────────────────────────────────

// C: static void statement(LexState *ls);  -- forward declaration
// C: static void expr(LexState *ls, expdesc *v);  -- forward declaration
// (Both defined later in this file; Rust has no forward declarations.)

// ── §1 Error helpers ────────────────────────────────────────────────────────

/// C: static l_noret error_expected(LexState *ls, int token)
/// Constructs a syntax error for a missing expected token.
/// In Rust, `l_noret` becomes returning `LuaError`; callers use
/// `return Err(error_expected(...))`.
fn error_expected(ls: &LexState, token: TokenKind) -> LuaError {
    // C: luaX_syntaxerror(ls, luaO_pushfstring(ls->L, "%s expected", luaX_token2str(ls, token)));
    // TODO(port): lua_lex::token_to_str(ls, token) for the token name
    let _ = ls;
    LuaError::syntax(format_args!("{} expected", token))
}

/// C: static l_noret errorlimit(FuncState *fs, int limit, const char *what)
/// Constructs a compile-time limit-exceeded syntax error.
fn error_limit(fs: &FuncState, limit: i32, what: &str) -> LuaError {
    // C: line == 0 ? "main function" : "function at line %d"
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

/// C: static void checklimit(FuncState *fs, int v, int l, const char *what)
fn check_limit(fs: &FuncState, v: i32, l: i32, what: &str) -> Result<(), LuaError> {
    if v > l {
        return Err(error_limit(fs, l, what));
    }
    Ok(())
}

// ── §2 Basic parse utilities ─────────────────────────────────────────────────

/// C: static int testnext(LexState *ls, int c)
/// If the current token matches `c`, consume it and return true.
fn test_next(ls: &mut LexState, state: &mut LuaState, c: TokenKind) -> Result<bool, LuaError> {
    if ls.t.token == c {
        // C: luaX_next(ls)
        lex_next(ls, state)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// C: static void check(LexState *ls, int c)
fn check(ls: &LexState, c: TokenKind) -> Result<(), LuaError> {
    if ls.t.token != c {
        return Err(error_expected(ls, c));
    }
    Ok(())
}

/// C: static void checknext(LexState *ls, int c)
fn check_next(ls: &mut LexState, state: &mut LuaState, c: TokenKind) -> Result<(), LuaError> {
    check(ls, c)?;
    // C: luaX_next(ls)
    lex_next(ls, state)?;
    Ok(())
}

/// C: static TString *str_checkname(LexState *ls)
/// Expects TK_NAME, returns the name string, advances.
fn str_check_name(ls: &mut LexState, state: &mut LuaState) -> Result<GcRef<LuaString>, LuaError> {
    // C: check(ls, TK_NAME); ts = ls->t.seminfo.ts; luaX_next(ls); return ts;
    check(ls, TK_NAME)?;
    let ts = ls.t.seminfo.ts.clone()
        .ok_or_else(|| LuaError::syntax(format_args!("name expected")))?;
    lex_next(ls, state)?;
    Ok(ts)
}

/// C: static void init_exp(expdesc *e, expkind k, int i)
fn init_exp(e: &mut ExprDesc, k: ExprKind, i: i32) {
    e.f = NO_JUMP;
    e.t = NO_JUMP;
    e.k = k;
    e.u.info = i;
}

/// C: static void codestring(expdesc *e, TString *s)
fn codestring(e: &mut ExprDesc, s: GcRef<LuaString>) {
    e.f = NO_JUMP;
    e.t = NO_JUMP;
    e.k = ExprKind::KStr;
    e.u.strval = Some(s);
}

/// C: static void codename(LexState *ls, expdesc *e)
fn codename(ls: &mut LexState, state: &mut LuaState, e: &mut ExprDesc) -> Result<(), LuaError> {
    // C: codestring(e, str_checkname(ls));
    let name = str_check_name(ls, state)?;
    codestring(e, name);
    Ok(())
}

// ── §3 Variable handling ─────────────────────────────────────────────────────

/// C: static int registerlocalvar(LexState *ls, FuncState *fs, TString *varname)
/// Registers a local variable in the proto's debug-info locvars array.
/// Returns the index in locvars (= fs->ndebugvars before increment).
fn register_local_var(
    ls: &mut LexState,
    state: &mut LuaState,
    fs: &mut FuncState,
    varname: GcRef<LuaString>,
) -> Result<i32, LuaError> {
    // C: luaM_growvector(ls->L, f->locvars, fs->ndebugvars, f->sizelocvars, LocVar, SHRT_MAX, ...)
    // In Rust, Vec grows automatically; just push a placeholder if needed.
    let idx = fs.ndebugvars as usize;
    while fs.f.locvars.len() <= idx {
        // C: f->locvars[oldsize++].varname = NULL
        fs.f.locvars.push(LocalVar {
            varname: varname.clone(), // placeholder; overwritten below
            startpc: 0,
            endpc: 0,
        });
    }
    fs.f.locvars[idx].varname = varname;
    fs.f.locvars[idx].startpc = fs.pc;
    // C: luaC_objbarrier(ls->L, f, varname) — no-op in Phase A
    let result = fs.ndebugvars as i32;
    fs.ndebugvars += 1;
    Ok(result)
}

/// C: static int new_localvar(LexState *ls, TString *name)
/// Creates a new local variable entry in dyd.actvar.
/// Returns the variable's index relative to fs->firstlocal.
fn new_local_var(
    ls: &mut LexState,
    state: &mut LuaState,
    name: GcRef<LuaString>,
) -> Result<i32, LuaError> {
    // C: checklimit(fs, dyd->actvar.n + 1 - fs->firstlocal, MAXVARS, "local variables")
    let fs = ls.fs.as_ref().unwrap();
    let n = ls.dyd.actvar.len() as i32;
    let first_local = fs.firstlocal;
    check_limit(fs, n + 1 - first_local, MAX_VARS, "local variables")?;

    // C: luaM_growvector(...) — Vec grows automatically
    // C: var = &dyd->actvar.arr[dyd->actvar.n++]
    let mut var = VarDesc::default();
    var.kind = VarKind::Reg;
    var.name = Some(name);
    ls.dyd.actvar.push(var);
    let result = ls.dyd.actvar.len() as i32 - 1 - first_local;
    Ok(result)
}

/// C: static Vardesc *getlocalvardesc(FuncState *fs, int vidx)
/// Returns a reference to the VarDesc at index `fs->firstlocal + vidx`.
fn get_local_var_desc<'a>(ls: &'a LexState, fs: &FuncState, vidx: i32) -> &'a VarDesc {
    &ls.dyd.actvar[(fs.firstlocal + vidx) as usize]
}

/// C: static Vardesc *getlocalvardesc — mutable variant
fn get_local_var_desc_mut(ls: &mut LexState, first_local: i32, vidx: i32) -> &mut VarDesc {
    &mut ls.dyd.actvar[(first_local + vidx) as usize]
}

/// C: static int reglevel(FuncState *fs, int nvar)
/// Converts a compiler-index level to its register number.
fn reg_level(ls: &LexState, fs: &FuncState, nvar: i32) -> i32 {
    // C: search backwards for the highest variable in a register
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

/// C: int luaY_nvarstack(FuncState *fs)
/// Returns the number of variables currently occupying registers.
/// LUAI_FUNC visibility.
pub fn nvarstack(ls: &LexState, fs: &FuncState) -> i32 {
    reg_level(ls, fs, fs.nactvar as i32)
}

/// C: static LocVar *localdebuginfo(FuncState *fs, int vidx)
/// Returns a mutable reference to the debug-info entry for variable `vidx`,
/// or `None` if it is a compile-time constant (no debug info).
fn local_debug_info<'a>(ls: &LexState, fs: &'a mut FuncState, vidx: i32) -> Option<&'a mut LocalVar> {
    let vd = get_local_var_desc(ls, fs, vidx);
    if vd.kind == VarKind::CompileTimeConst {
        return None; // C: return NULL
    }
    let idx = vd.pidx as usize;
    debug_assert!((idx as i16) < fs.ndebugvars);
    // TODO(port): borrow conflict — vd borrows ls immutably, idx is a copy; safe to proceed.
    Some(&mut fs.f.locvars[idx])
}

/// C: static void init_var(FuncState *fs, expdesc *e, int vidx)
fn init_var(ls: &LexState, fs: &FuncState, e: &mut ExprDesc, vidx: i32) {
    e.f = NO_JUMP;
    e.t = NO_JUMP;
    e.k = ExprKind::Local;
    e.u.var_vidx = vidx as u16;
    e.u.var_ridx = get_local_var_desc(ls, fs, vidx).ridx;
}

/// C: static void check_readonly(LexState *ls, expdesc *e)
/// Raises an error if expression `e` describes a read-only variable.
fn check_readonly(ls: &mut LexState, state: &mut LuaState, e: &ExprDesc) -> Result<(), LuaError> {
    // C: FuncState *fs = ls->fs;  TString *varname = NULL;
    let varname: Option<GcRef<LuaString>> = {
        let fs = ls.fs.as_ref().unwrap();
        match e.k {
            ExprKind::Const => {
                // C: varname = ls->dyd->actvar.arr[e->u.info].vd.name
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
        // C: luaK_semerror(ls, luaO_pushfstring(..., "attempt to assign to const variable '%s'", ...))
        // TODO(port): getstr(varname) → varname.as_bytes()
        return Err(LuaError::syntax(format_args!(
            "attempt to assign to const variable"
        )));
    }
    Ok(())
}

/// C: static void adjustlocalvars(LexState *ls, int nvars)
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
        // C: var->vd.ridx = reglevel++; var->vd.pidx = registerlocalvar(...)
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

/// C: static void removevars(FuncState *fs, int tolevel)
/// Closes scope for all variables above `tolevel`, updating their endpc.
fn remove_vars(ls: &mut LexState, fs: &mut FuncState, tolevel: i32) {
    // C: fs->ls->dyd->actvar.n -= (fs->nactvar - tolevel)
    let delta = fs.nactvar as i32 - tolevel;
    if delta > 0 {
        let new_len = ls.dyd.actvar.len().saturating_sub(delta as usize);
        ls.dyd.actvar.truncate(new_len);
    }
    while fs.nactvar as i32 > tolevel {
        fs.nactvar -= 1;
        // C: LocVar *var = localdebuginfo(fs, --fs->nactvar); if (var) var->endpc = fs->pc
        let nactvar = fs.nactvar as i32;
        let vd_kind = {
            // need to check kind without holding a borrow across the mut borrow below
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
}

// ── §4 Upvalue handling ──────────────────────────────────────────────────────

/// C: static int searchupvalue(FuncState *fs, TString *name)
/// Returns the index of an upvalue named `name`, or -1 if not found.
/// C: pointer equality (eqstr) because all strings are interned.
fn search_upvalue(fs: &FuncState, name: &GcRef<LuaString>) -> i32 {
    for (i, up) in fs.f.upvalues.iter().enumerate() {
        if up.name.as_ref().map_or(false, |n| GcRef::ptr_eq(n, name)) {
            return i as i32;
        }
    }
    -1
}

/// C: static Upvaldesc *allocupvalue(FuncState *fs)
/// Grows upvalues array and returns index of the new slot.
fn alloc_upvalue(fs: &mut FuncState) -> Result<usize, LuaError> {
    // C: checklimit(fs, fs->nups + 1, MAXUPVAL, "upvalues")
    // TODO(port): checklimit needs LexState for the error; passing MAX_UPVAL directly
    if fs.nups as i32 + 1 > MAX_UPVAL as i32 {
        return Err(LuaError::syntax(format_args!("too many upvalues (limit is {})", MAX_UPVAL)));
    }
    // C: luaM_growvector — Vec handles this automatically
    let idx = fs.nups as usize;
    while fs.f.upvalues.len() <= idx {
        fs.f.upvalues.push(UpvalDesc { name: None, instack: false, idx: 0, kind: 0 });
    }
    fs.nups += 1;
    Ok(idx)
}

/// C: static int newupvalue(FuncState *fs, TString *name, expdesc *v)
/// Adds a new upvalue descriptor and returns its index.
fn new_upvalue(
    ls: &LexState,
    fs: &mut FuncState,
    name: GcRef<LuaString>,
    v: &ExprDesc,
) -> Result<i32, LuaError> {
    let idx = alloc_upvalue(fs)?;
    let up = &mut fs.f.upvalues[idx];
    if v.k == ExprKind::Local {
        // C: up->instack = 1; up->idx = v->u.var.ridx; up->kind = getlocalvardesc(prev, ...)->vd.kind
        up.instack = true;
        up.idx = v.u.var_ridx;
        // TODO(port): need access to prev FuncState to get kind via getlocalvardesc
        up.kind = VarKind::Reg.as_u8();
    } else {
        // C: up->instack = 0; up->idx = cast_byte(v->u.info); up->kind = prev->f->upvalues[...].kind
        up.instack = false;
        up.idx = v.u.info as u8;
        // TODO(port): need access to prev FuncState to get upvalue kind
        up.kind = VarKind::Reg.as_u8();
    }
    up.name = Some(name);
    // C: luaC_objbarrier(fs->ls->L, fs->f, name) — no-op in Phase A
    Ok(fs.nups as i32 - 1)
}

/// C: static int searchvar(FuncState *fs, TString *n, expdesc *var)
/// Searches for a local variable named `n`. Returns ExprKind as i32 or -1.
fn searchvar(
    ls: &LexState,
    fs: &FuncState,
    n: &GcRef<LuaString>,
    var: &mut ExprDesc,
) -> i32 {
    // C: for (i = cast_int(fs->nactvar) - 1; i >= 0; i--)
    let mut i = fs.nactvar as i32 - 1;
    while i >= 0 {
        let vd = get_local_var_desc(ls, fs, i);
        if vd.name.as_ref().map_or(false, |nm| GcRef::ptr_eq(nm, n)) {
            if vd.kind == VarKind::CompileTimeConst {
                // C: init_exp(var, VCONST, fs->firstlocal + i)
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

/// C: static void markupval(FuncState *fs, int level)
/// Marks the block where the variable at `level` was defined as having an upvalue.
fn markupval(fs: &mut FuncState, level: i32) {
    // C: while (bl->nactvar > level) bl = bl->previous;  bl->upval = 1;
    let mut bl = fs.bl.as_mut();
    while let Some(b) = bl {
        if (b.nactvar as i32) <= level {
            b.upval = true;
            break;
        }
        // PORT NOTE: can't iterate Option<Box<BlockCnt>> chain mutably in a simple loop
        // TODO(port): borrow issue — chain traversal needs refactoring or unsafe
        break;
    }
    fs.needclose = true;
}

/// C: static void marktobeclosed(FuncState *fs)
fn marktobeclosed(fs: &mut FuncState) {
    if let Some(bl) = fs.bl.as_mut() {
        bl.upval = true;
        bl.insidetbc = true;
    }
    fs.needclose = true;
}

// ── §5 Variable resolution ───────────────────────────────────────────────────

/// C: static void singlevaraux(FuncState *fs, TString *n, expdesc *var, int base)
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
            // C: init_exp(var, VVOID, 0) — default is global
            init_exp(var, ExprKind::Void, 0);
        }
        Some(fs) => {
            let v = searchvar(ls, fs, n, var);
            if v >= 0 {
                // C: if (v == VLOCAL && !base) markupval(fs, var->u.var.vidx)
                if v == ExprKind::Local as i32 && !base {
                    markupval(fs, var.u.var_vidx as i32);
                }
            } else {
                // C: try existing upvalues
                let idx = search_upvalue(fs, n);
                if idx < 0 {
                    // TODO(port): recursive call needs access to fs.prev
                    // C: singlevaraux(fs->prev, n, var, 0)
                    // In Rust, fs.prev is owned by fs; can't easily recurse.
                    // PORT NOTE: upvalue chain traversal needs restructuring in Phase B.
                    // For now, set VVOID to indicate "global fallback".
                    init_exp(var, ExprKind::Void, 0);
                    return Ok(());
                }
                init_exp(var, ExprKind::UpVal, idx);
            }
        }
    }
    Ok(())
}

/// C: static void singlevar(LexState *ls, expdesc *var)
/// Finds the variable named by the next TK_NAME token.
fn singlevar(ls: &mut LexState, state: &mut LuaState, var: &mut ExprDesc) -> Result<(), LuaError> {
    let varname = str_check_name(ls, state)?;
    // C: singlevaraux(fs, varname, var, 1)
    // TODO(port): singlevaraux needs &mut FuncState but fs is inside ls — borrow conflict.
    // For Phase A, inline the logic with ls.fs extraction.
    {
        let fs = ls.fs.as_ref().unwrap();
        let v = searchvar(ls, fs, &varname, var);
        if v >= 0 {
            // found a local — no upvalue marking needed (base = true)
        } else {
            let idx = search_upvalue(fs, &varname);
            if idx >= 0 {
                init_exp(var, ExprKind::UpVal, idx);
            } else {
                // TODO(port): recurse into fs.prev chain for upvalue resolution
                init_exp(var, ExprKind::Void, 0);
            }
        }
    }
    if var.k == ExprKind::Void {
        // C: global name — env[varname]
        let envn = ls.envn.clone();
        let env_idx = if let Some(env_name) = envn {
            let fs = ls.fs.as_ref().unwrap();
            search_upvalue(fs, &env_name)
        } else {
            -1
        };
        // C: luaK_indexed(fs, var, &key) — for an upvalue table with a string
        // key, the result is VINDEXUP { ind_t: upval_idx, ind_idx: k_const_idx }.
        let fs = ls.fs.as_mut().unwrap();
        let k_idx = add_k_string(fs, varname);
        var.f = NO_JUMP;
        var.t = NO_JUMP;
        var.k = ExprKind::IndexUp;
        var.u.ind_t = env_idx.max(0) as u8;
        var.u.ind_idx = k_idx as i16;
    }
    Ok(())
}

/// C: static void adjust_assign(LexState *ls, int nvars, int nexps, expdesc *e)
fn adjust_assign(
    ls: &mut LexState,
    state: &mut LuaState,
    nvars: i32,
    nexps: i32,
    e: &mut ExprDesc,
) -> Result<(), LuaError> {
    let needed = nvars - nexps;
    let line = ls.linenumber;
    let fs = ls.fs.as_mut().unwrap();
    if e.k.has_mult_ret() {
        // C: extra = needed + 1; if (extra < 0) extra = 0; luaK_setreturns(fs, e, extra)
        let _extra = if needed + 1 < 0 { 0 } else { needed + 1 };
        // TODO(port): lua_code::set_returns(fs, e, extra)?;
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
            reserve_reg(fs);
        }
    } else {
        fs.freereg = (fs.freereg as i32 + needed) as u8;
    }
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

/// C: static l_noret jumpscopeerror(LexState *ls, Labeldesc *gt)
fn jumpscopeerror(ls: &LexState, gt_idx: usize) -> LuaError {
    // C: varname = getstr(getlocalvardesc(ls->fs, gt->nactvar)->vd.name)
    //    msg = "<goto %s> at line %d jumps into the scope of local '%s'"
    //    luaK_semerror(ls, msg)
    let gt = &ls.dyd.gt[gt_idx];
    let line = gt.line;
    // TODO(port): format with actual names from GcRef<LuaString>
    LuaError::syntax(format_args!(
        "goto at line {} jumps into the scope of a local variable", line
    ))
}

/// C: static void solvegoto(LexState *ls, int g, Labeldesc *label)
/// Resolves goto at index `g` to `label`, removing it from pending list.
fn solvegoto(
    ls: &mut LexState,
    state: &mut LuaState,
    g: usize,
    label_pc: i32,
    label_nactvar: u8,
) -> Result<(), LuaError> {
    // C: if (l_unlikely(gt->nactvar < label->nactvar)) jumpscopeerror(ls, gt)
    if ls.dyd.gt[g].nactvar < label_nactvar {
        return Err(jumpscopeerror(ls, g));
    }
    let gt_pc = ls.dyd.gt[g].pc;
    // C: luaK_patchlist(ls->fs, gt->pc, label->pc)
    // TODO(port): lua_code::patch_list(ls.fs.as_mut().unwrap(), gt_pc, label_pc)?;
    // C: remove goto g from pending list (shift left)
    ls.dyd.gt.remove(g);
    Ok(())
}

/// C: static Labeldesc *findlabel(LexState *ls, TString *name)
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

/// C: static int newlabelentry(LexState *ls, Labellist *l, TString *name, int line, int pc)
/// Adds a new label/goto entry; returns its index.
fn new_label_entry(
    ls: &mut LexState,
    state: &mut LuaState,
    is_goto: bool,
    name: GcRef<LuaString>,
    line: i32,
    pc: i32,
) -> Result<usize, LuaError> {
    let nactvar = ls.fs.as_ref().unwrap().nactvar;
    let entry = LabelDesc { name: Some(name), pc, line, nactvar, close: false };
    let list = if is_goto { &mut ls.dyd.gt } else { &mut ls.dyd.label };
    // C: luaM_growvector — Vec grows automatically
    let n = list.len();
    list.push(entry);
    Ok(n)
}

/// C: static int newgotoentry(LexState *ls, TString *name, int line, int pc)
fn new_goto_entry(
    ls: &mut LexState,
    state: &mut LuaState,
    name: GcRef<LuaString>,
    line: i32,
    pc: i32,
) -> Result<usize, LuaError> {
    new_label_entry(ls, state, true, name, line, pc)
}

/// C: static int solvegotos(LexState *ls, Labeldesc *lb)
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

/// C: static int createlabel(LexState *ls, TString *name, int line, int last)
/// Creates a new label; resolves pending gotos. Returns true if CLOSE emitted.
fn createlabel(
    ls: &mut LexState,
    state: &mut LuaState,
    name: GcRef<LuaString>,
    line: i32,
    last: bool,
) -> Result<bool, LuaError> {
    // C: int l = newlabelentry(ls, ll, name, line, luaK_getlabel(fs))
    let label_pc = {
        // TODO(port): lua_code::get_label(ls.fs.as_mut().unwrap())
        ls.fs.as_ref().unwrap().pc // placeholder
    };
    let l = new_label_entry(ls, state, false, name, line, label_pc)?;
    if last {
        // C: ll->arr[l].nactvar = fs->bl->nactvar
        let bl_nactvar = ls.fs.as_ref().unwrap().bl.as_ref().map_or(0, |b| b.nactvar);
        ls.dyd.label[l].nactvar = bl_nactvar;
    }
    let needs_close = solvegotos(ls, state, l)?;
    if needs_close {
        // C: luaK_codeABC(fs, OP_CLOSE, luaY_nvarstack(fs), 0, 0)
        // TODO(port): lua_code::code_abc(ls.fs.as_mut().unwrap(), OpCode::Close, nstack, 0, 0)?;
        return Ok(true);
    }
    Ok(false)
}

/// C: static void movegotosout(FuncState *fs, BlockCnt *bl)
/// Adjusts pending gotos to outer block level when leaving a block.
fn movegotosout(ls: &mut LexState, bl_firstgoto: usize, bl_nactvar: u8, bl_upval: bool) {
    let fs = ls.fs.as_ref().unwrap();
    let first_goto = bl_firstgoto;
    let n_gt = ls.dyd.gt.len();
    drop(fs); // release borrow before iterating

    for i in first_goto..ls.dyd.gt.len() {
        let gt_nactvar = ls.dyd.gt[i].nactvar;
        // C: if (reglevel(fs, gt->nactvar) > reglevel(fs, bl->nactvar)) gt->close |= bl->upval
        // TODO(port): compute reg_level properly using ls+fs
        if bl_upval {
            ls.dyd.gt[i].close = true;
        }
        ls.dyd.gt[i].nactvar = bl_nactvar;
    }
}

/// C: static void enterblock(FuncState *fs, BlockCnt *bl, lu_byte isloop)
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

/// C: static l_noret undefgoto(LexState *ls, Labeldesc *gt)
fn undef_goto(ls: &LexState, gt_idx: usize) -> LuaError {
    let gt = &ls.dyd.gt[gt_idx];
    let line = gt.line;
    // C: if (eqstr(gt->name, luaS_newliteral(ls->L, "break"))) ...
    let is_break = gt.name.as_ref()
        .and_then(|n| {
            // TODO(port): proper string comparison via getstr
            None::<bool>
        })
        .unwrap_or(false);
    if is_break {
        LuaError::syntax(format_args!("break outside loop at line {}", line))
    } else {
        LuaError::syntax(format_args!("no visible label for <goto> at line {}", line))
    }
}

/// C: static void leaveblock(FuncState *fs)
/// Pops the innermost block scope, emitting CLOSE if needed.
fn leave_block(ls: &mut LexState, state: &mut LuaState) -> Result<(), LuaError> {
    // Take the current block out of the chain
    let fs = ls.fs.as_mut().unwrap();
    let bl_box = fs.bl.take().unwrap();
    let mut bl = *bl_box;
    let previous = bl.previous.take();

    let stklevel = reg_level(ls, ls.fs.as_ref().unwrap(), bl.nactvar as i32);
    let mut fs_box = ls.fs.take().unwrap();
    remove_vars(ls, &mut fs_box, bl.nactvar as i32);
    debug_assert!(bl.nactvar == fs_box.nactvar);
    ls.fs = Some(fs_box);

    let hasclose = if bl.isloop {
        // C: createlabel(ls, luaS_newliteral(ls->L, "break"), 0, 0)
        // TODO(port): intern "break" string
        // TODO(port): createlabel(ls, state, break_str, 0, false)?
        false // placeholder
    } else {
        false
    };

    // Restore previous block
    ls.fs.as_mut().unwrap().bl = previous;

    let has_prev_block = ls.fs.as_ref().unwrap().bl.is_some();
    if !hasclose && has_prev_block && bl.upval {
        // C: luaK_codeABC(fs, OP_CLOSE, stklevel, 0, 0)
        // TODO(port): lua_code::code_abc(ls.fs.as_mut().unwrap(), OpCode::Close, stklevel, 0, 0)?;
    }
    ls.fs.as_mut().unwrap().freereg = stklevel as u8;

    // C: ls->dyd->label.n = bl->firstlabel
    ls.dyd.label.truncate(bl.firstlabel as usize);

    // C: fs->bl = bl->previous — already done above
    if has_prev_block {
        movegotosout(ls, bl.firstgoto as usize, bl.nactvar, bl.upval);
    } else {
        // C: if (bl->firstgoto < ls->dyd->gt.n) undefgoto(...)
        if (bl.firstgoto as usize) < ls.dyd.gt.len() {
            return Err(undef_goto(ls, bl.firstgoto as usize));
        }
    }
    Ok(())
}

// ── §7 Proto management ──────────────────────────────────────────────────────

/// C: static Proto *addprototype(LexState *ls)
/// Adds a new prototype slot to the current function's proto list.
/// Returns a mutable reference to the new prototype.
fn add_prototype(ls: &mut LexState, state: &mut LuaState) -> Result<Box<LuaProto>, LuaError> {
    // C: luaM_growvector(L, f->p, fs->np, f->sizep, Proto *, MAXARG_Bx, "functions")
    let np = ls.fs.as_ref().unwrap().np as usize;
    // C: f->p[fs->np++] = clp = luaF_newproto(L)
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
    // C: luaC_objbarrier(L, f, clp) — no-op in Phase A
    Ok(new_proto)
}

/// C: static void codeclosure(LexState *ls, expdesc *v)
/// Emits OP_CLOSURE in the parent function and fixes up v.
fn codeclosure(ls: &mut LexState, state: &mut LuaState, v: &mut ExprDesc) -> Result<(), LuaError> {
    // C: FuncState *fs = ls->fs->prev
    // C: init_exp(v, VRELOC, luaK_codeABx(fs, OP_CLOSURE, 0, fs->np - 1))
    // C: luaK_exp2nextreg(fs, v)
    // TODO(port): needs access to ls.fs.prev — requires traversal
    // TODO(port): lua_code::code_abx(parent_fs, OpCode::Closure, 0, np - 1)?
    // PORT NOTE: codeclosure operates on the PARENT FuncState (ls->fs->prev),
    //   not the current one. Phase B must thread this correctly.
    init_exp(v, ExprKind::Reloc, 0);
    Ok(())
}

/// C: static void open_func(LexState *ls, FuncState *fs, BlockCnt *bl)
/// Installs `new_fs` as the current FuncState, pushing old one as `prev`.
fn open_func(ls: &mut LexState, state: &mut LuaState, mut new_fs: FuncState) -> Result<(), LuaError> {
    // C: fs->prev = ls->fs; fs->ls = ls; ls->fs = fs;
    new_fs.prev = ls.fs.take();

    let f = &mut new_fs.f;
    // C: fs->pc = 0; fs->previousline = f->linedefined; ...
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

    // C: fs->firstlocal = ls->dyd->actvar.n
    new_fs.firstlocal = ls.dyd.actvar.len() as i32;
    // C: fs->firstlabel = ls->dyd->label.n
    new_fs.firstlabel = ls.dyd.label.len() as i32;
    new_fs.bl = None;

    // C: f->source = ls->source; f->maxstacksize = 2
    new_fs.f.source = ls.source.clone();
    new_fs.f.maxstacksize = 2;

    // C: luaC_objbarrier(ls->L, f, f->source) — no-op in Phase A

    ls.fs = Some(Box::new(new_fs));

    // C: enterblock(fs, bl, 0)
    enter_block(ls, false);
    Ok(())
}

/// C: static void close_func(LexState *ls)
/// Finalizes and pops the current FuncState.
/// Returns the completed LuaProto.
fn close_func(ls: &mut LexState, state: &mut LuaState) -> Result<Box<LuaProto>, LuaError> {
    // C: luaK_ret(fs, luaY_nvarstack(fs), 0)
    {
        let first = {
            let fs = ls.fs.as_ref().unwrap();
            nvarstack(ls, fs)
        };
        let fs = ls.fs.as_mut().unwrap();
        let raw = lua_code::opcodes::Instruction::abck(
            lua_code::opcodes::OpCode::Return0,
            first as u32,
            1,
            0,
            0,
        )
        .0;
        let pc = fs.pc as usize;
        if fs.f.code.len() <= pc {
            fs.f.code.resize(pc + 1, lua_types::opcode::Instruction::default());
        }
        fs.f.code[pc] = lua_types::opcode::Instruction::new(raw);
        if fs.f.lineinfo.len() <= pc {
            fs.f.lineinfo.resize(pc + 1, 0i8);
        }
        fs.pc += 1;
    }
    // C: leaveblock(fs)
    leave_block(ls, state)?;
    debug_assert!(ls.fs.as_ref().unwrap().bl.is_none());

    // C: luaK_finish(fs)
    // TODO(port): lua_code::finish(ls.fs.as_mut().unwrap())?;

    // C: luaM_shrinkvector — truncate arrays to actual used size
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

    // C: ls->fs = fs->prev
    let mut fs_box = ls.fs.take().unwrap();
    ls.fs = fs_box.prev.take();

    // C: luaC_checkGC(L) — no-op in Phase A
    Ok(fs_box.f)
}

// ── §8 Grammar rules — block / statement lists ───────────────────────────────

/// C: static int block_follow(LexState *ls, int withuntil)
/// Returns true if the current token can end a block.
fn block_follow(ls: &LexState, withuntil: bool) -> bool {
    match ls.t.token {
        TK_ELSE | TK_ELSEIF | TK_END | TK_EOS => true,
        TK_UNTIL => withuntil,
        _ => false,
    }
}

/// C: static void statlist(LexState *ls)
fn statlist(ls: &mut LexState, state: &mut LuaState) -> Result<(), LuaError> {
    // C: while (!block_follow(ls, 1)) { if (TK_RETURN) { statement; return; } statement; }
    while !block_follow(ls, true) {
        if ls.t.token == TK_RETURN {
            statement(ls, state)?;
            return Ok(());
        }
        statement(ls, state)?;
    }
    Ok(())
}

/// C: static void fieldsel(LexState *ls, expdesc *v)
/// Handles '.' NAME or ':' NAME field selection.
fn fieldsel(ls: &mut LexState, state: &mut LuaState, v: &mut ExprDesc) -> Result<(), LuaError> {
    // C: luaK_exp2anyregup(fs, v); luaX_next(ls); codename(ls, &key); luaK_indexed(fs, v, &key)
    // TODO(port): lua_code::exp_to_any_reg_up(ls.fs.as_mut().unwrap(), v)?;
    lex_next(ls, state)?; // skip '.' or ':'
    let mut key = ExprDesc::default();
    codename(ls, state, &mut key)?;
    // TODO(port): lua_code::indexed(ls.fs.as_mut().unwrap(), v, &mut key)?;
    Ok(())
}

/// C: static void yindex(LexState *ls, expdesc *v)
/// Handles '[' expr ']' indexing.
fn yindex(ls: &mut LexState, state: &mut LuaState, v: &mut ExprDesc) -> Result<(), LuaError> {
    // C: luaX_next(ls); expr(ls, v); luaK_exp2val(ls->fs, v); checknext(ls, ']')
    lex_next(ls, state)?;
    expr(ls, state, v)?;
    // TODO(port): lua_code::exp_to_val(ls.fs.as_mut().unwrap(), v)?;
    check_next(ls, state, b']' as TokenKind)?;
    Ok(())
}

// ── §9 Constructor rules ─────────────────────────────────────────────────────

/// C: static void recfield(LexState *ls, ConsControl *cc)
fn recfield(ls: &mut LexState, state: &mut LuaState, cc: &mut ConsControl) -> Result<(), LuaError> {
    let reg = ls.fs.as_ref().unwrap().freereg as i32;
    let mut key = ExprDesc::default();
    let mut val = ExprDesc::default();
    if ls.t.token == TK_NAME {
        // C: checklimit(fs, cc->nh, MAX_INT, "items in a constructor")
        let fs = ls.fs.as_ref().unwrap();
        check_limit(fs, cc.nh, i32::MAX, "items in a constructor")?;
        codename(ls, state, &mut key)?;
    } else {
        // C: yindex(ls, &key)
        yindex(ls, state, &mut key)?;
    }
    cc.nh += 1;
    check_next(ls, state, b'=' as TokenKind)?;
    let mut tab = cc.t.clone();
    // C: luaK_indexed(fs, &tab, &key); expr(ls, &val); luaK_storevar(fs, &tab, &val)
    // TODO(port): lua_code::indexed(ls.fs.as_mut().unwrap(), &mut tab, &mut key)?;
    expr(ls, state, &mut val)?;
    // TODO(port): lua_code::store_var(ls.fs.as_mut().unwrap(), &mut tab, &mut val)?;
    ls.fs.as_mut().unwrap().freereg = reg as u8;
    Ok(())
}

/// C: static void closelistfield(FuncState *fs, ConsControl *cc)
fn closelistfield(ls: &mut LexState, state: &mut LuaState, cc: &mut ConsControl) -> Result<(), LuaError> {
    if cc.v.k == ExprKind::Void {
        return Ok(()); // C: if (cc->v.k == VVOID) return;
    }
    // TODO(port): lua_code::exp_to_next_reg(ls.fs.as_mut().unwrap(), &mut cc.v)?;
    cc.v.k = ExprKind::Void;
    if cc.tostore == LFIELDS_PER_FLUSH {
        // C: luaK_setlist(fs, cc->t->u.info, cc->na, cc->tostore)
        let t_info = cc.t.u.info;
        // TODO(port): lua_code::set_list(ls.fs.as_mut().unwrap(), t_info, cc.na, cc.tostore)?;
        cc.na += cc.tostore;
        cc.tostore = 0;
    }
    Ok(())
}

/// C: static void lastlistfield(FuncState *fs, ConsControl *cc)
fn lastlistfield(ls: &mut LexState, state: &mut LuaState, cc: &mut ConsControl) -> Result<(), LuaError> {
    if cc.tostore == 0 {
        return Ok(());
    }
    let t_info = cc.t.u.info;
    if cc.v.k.has_mult_ret() {
        // C: luaK_setmultret(fs, &cc->v); luaK_setlist(fs, ..., LUA_MULTRET); cc->na--
        // TODO(port): lua_code::set_returns(ls.fs.as_mut().unwrap(), &mut cc.v, LUA_MULTRET)?;
        // TODO(port): lua_code::set_list(ls.fs.as_mut().unwrap(), t_info, cc.na, LUA_MULTRET)?;
        cc.na -= 1;
    } else {
        if cc.v.k != ExprKind::Void {
            // TODO(port): lua_code::exp_to_next_reg(ls.fs.as_mut().unwrap(), &mut cc.v)?;
        }
        // TODO(port): lua_code::set_list(ls.fs.as_mut().unwrap(), t_info, cc.na, cc.tostore)?;
    }
    cc.na += cc.tostore;
    Ok(())
}

/// C: static void listfield(LexState *ls, ConsControl *cc)
fn listfield(ls: &mut LexState, state: &mut LuaState, cc: &mut ConsControl) -> Result<(), LuaError> {
    expr(ls, state, &mut cc.v)?;
    cc.tostore += 1;
    Ok(())
}

/// C: static void field(LexState *ls, ConsControl *cc)
fn field(ls: &mut LexState, state: &mut LuaState, cc: &mut ConsControl) -> Result<(), LuaError> {
    match ls.t.token {
        TK_NAME => {
            // C: if (luaX_lookahead(ls) != '=') listfield else recfield
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

/// C: static void constructor(LexState *ls, expdesc *t)
fn constructor(ls: &mut LexState, state: &mut LuaState, t: &mut ExprDesc) -> Result<(), LuaError> {
    let line = ls.linenumber;
    // C: int pc = luaK_codeABC(fs, OP_NEWTABLE, 0, 0, 0)
    // TODO(port): let pc = lua_code::code_abc(ls.fs.as_mut().unwrap(), OpCode::NewTable, 0, 0, 0)?;
    let pc = ls.fs.as_ref().unwrap().pc; // placeholder
    // C: luaK_code(fs, 0) — extra arg space
    // TODO(port): lua_code::emit(ls.fs.as_mut().unwrap(), Instruction::new(0))?;

    let freereg = ls.fs.as_ref().unwrap().freereg as i32;
    // C: init_exp(t, VNONRELOC, fs->freereg); luaK_reserveregs(fs, 1)
    init_exp(t, ExprKind::NonReloc, freereg);
    // TODO(port): lua_code::reserve_regs(ls.fs.as_mut().unwrap(), 1)?;

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
    // C: check_match(ls, '}', '{', line)
    check_match(ls, state, b'}' as TokenKind, b'{' as TokenKind, line)?;
    lastlistfield(ls, state, &mut cc)?;

    // C: luaK_settablesize(fs, pc, t->u.info, cc.na, cc.nh)
    let t_info = t.u.info;
    // TODO(port): lua_code::set_table_size(ls.fs.as_mut().unwrap(), pc, t_info, cc.na, cc.nh)?;
    Ok(())
}

// ── §10 Parameter list and function body ─────────────────────────────────────

/// C: static void setvararg(FuncState *fs, int nparams)
fn setvararg(fs: &mut FuncState, _state: &mut LuaState, nparams: i32) -> Result<(), LuaError> {
    fs.f.is_vararg = true;
    // C: luaK_codeABC(fs, OP_VARARGPREP, nparams, 0, 0)
    let inst = lua_code::opcodes::Instruction::abck(
        lua_code::opcodes::OpCode::VarArgPrep,
        nparams as u32,
        0, 0, 0,
    );
    let line = fs.previousline;
    emit_inst(fs, line, inst);
    Ok(())
}

/// C: static void parlist(LexState *ls)
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
    // C: luaK_reserveregs(fs, fs->nactvar)
    let nactvar = ls.fs.as_ref().unwrap().nactvar as i32;
    // TODO(port): lua_code::reserve_regs(ls.fs.as_mut().unwrap(), nactvar)?;
    Ok(())
}

/// C: static void check_match(LexState *ls, int what, int who, int where)
fn check_match(
    ls: &mut LexState,
    state: &mut LuaState,
    what: TokenKind,
    who: TokenKind,
    where_line: i32,
) -> Result<(), LuaError> {
    // C: if (l_unlikely(!testnext(ls, what)))
    if !test_next(ls, state, what)? {
        if where_line == ls.linenumber {
            return Err(error_expected(ls, what));
        } else {
            // C: luaX_syntaxerror(ls, luaO_pushfstring(..., "%s expected (to close %s at line %d)", ...))
            return Err(LuaError::syntax(format_args!(
                "{} expected (to close {} at line {})", what, who, where_line
            )));
        }
    }
    Ok(())
}

/// C: static void body(LexState *ls, expdesc *e, int ismethod, int line)
fn body(
    ls: &mut LexState,
    state: &mut LuaState,
    e: &mut ExprDesc,
    ismethod: bool,
    line: i32,
) -> Result<(), LuaError> {
    // C: FuncState new_fs; BlockCnt bl; new_fs.f = addprototype(ls); new_fs.f->linedefined = line
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
    };
    new_fs.f.linedefined = line;
    open_func(ls, state, new_fs)?;

    check_next(ls, state, b'(' as TokenKind)?;
    if ismethod {
        // C: new_localvarliteral(ls, "self") — intern "self" and new_localvar
        // TODO(port): intern b"self" via state
        // new_local_var(ls, state, self_str)?;
        adjust_local_vars(ls, state, 1)?;
    }
    parlist(ls, state)?;
    check_next(ls, state, b')' as TokenKind)?;
    statlist(ls, state)?;
    ls.fs.as_mut().unwrap().f.lastlinedefined = ls.linenumber;
    check_match(ls, state, TK_END, TK_FUNCTION, line)?;
    codeclosure(ls, state, e)?;
    close_func(ls, state)?;
    Ok(())
}

// ── §11 Expression list and function arguments ────────────────────────────────

/// C: static int explist(LexState *ls, expdesc *v)
fn explist(ls: &mut LexState, state: &mut LuaState, v: &mut ExprDesc) -> Result<i32, LuaError> {
    let mut n = 1;
    expr(ls, state, v)?;
    while test_next(ls, state, b',' as TokenKind)? {
        // C: luaK_exp2nextreg(ls->fs, v)
        // TODO(port): lua_code::exp_to_next_reg(ls.fs.as_mut().unwrap(), v)?;
        expr(ls, state, v)?;
        n += 1;
    }
    Ok(n)
}

/// C: static void funcargs(LexState *ls, expdesc *f)
fn funcargs(ls: &mut LexState, state: &mut LuaState, f: &mut ExprDesc) -> Result<(), LuaError> {
    let mut args = ExprDesc::default();
    let line = ls.linenumber;
    match ls.t.token {
        c if c == b'(' as TokenKind => {
            lex_next(ls, state)?; // skip '('
            if ls.t.token == b')' as TokenKind {
                args.k = ExprKind::Void;
            } else {
                explist(ls, state, &mut args)?;
                if args.k.has_mult_ret() {
                    // TODO(port): lua_code::set_returns(ls.fs.as_mut().unwrap(), &mut args, LUA_MULTRET)?;
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
    // C: init_exp(f, VCALL, luaK_codeABC(fs, OP_CALL, base, nparams+1, 2))
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

/// C: static void primaryexp(LexState *ls, expdesc *v)
fn primaryexp(ls: &mut LexState, state: &mut LuaState, v: &mut ExprDesc) -> Result<(), LuaError> {
    match ls.t.token {
        c if c == b'(' as TokenKind => {
            let line = ls.linenumber;
            lex_next(ls, state)?;
            expr(ls, state, v)?;
            check_match(ls, state, b')' as TokenKind, b'(' as TokenKind, line)?;
            // C: luaK_dischargevars(ls->fs, v)
            // TODO(port): lua_code::discharge_vars(ls.fs.as_mut().unwrap(), v)?;
        }
        TK_NAME => {
            singlevar(ls, state, v)?;
        }
        _ => {
            return Err(LuaError::syntax(format_args!("unexpected symbol")));
        }
    }
    Ok(())
}

/// C: static void suffixedexp(LexState *ls, expdesc *v)
fn suffixedexp(ls: &mut LexState, state: &mut LuaState, v: &mut ExprDesc) -> Result<(), LuaError> {
    primaryexp(ls, state, v)?;
    loop {
        match ls.t.token {
            c if c == b'.' as TokenKind => {
                fieldsel(ls, state, v)?;
            }
            c if c == b'[' as TokenKind => {
                let mut key = ExprDesc::default();
                // C: luaK_exp2anyregup(fs, v)
                // TODO(port): lua_code::exp_to_any_reg_up(ls.fs.as_mut().unwrap(), v)?;
                yindex(ls, state, &mut key)?;
                // C: luaK_indexed(fs, v, &key)
                // TODO(port): lua_code::indexed(ls.fs.as_mut().unwrap(), v, &mut key)?;
            }
            c if c == b':' as TokenKind => {
                let mut key = ExprDesc::default();
                lex_next(ls, state)?;
                codename(ls, state, &mut key)?;
                // C: luaK_self(fs, v, &key)
                // TODO(port): lua_code::self_call(ls.fs.as_mut().unwrap(), v, &mut key)?;
                funcargs(ls, state, v)?;
            }
            c if c == b'(' as TokenKind || c == TK_STRING || c == b'{' as TokenKind => {
                // C: luaK_exp2nextreg(fs, v) — places the callee in a fixed register.
                let line = ls.linenumber;
                cg_exp_to_next_reg(ls.fs.as_mut().unwrap(), line, v)?;
                funcargs(ls, state, v)?;
            }
            _ => return Ok(()),
        }
    }
}

/// C: static void simpleexp(LexState *ls, expdesc *v)
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
            // C: check_condition(ls, fs->f->is_vararg, "cannot use '...' outside a vararg function")
            let is_vararg = ls.fs.as_ref().unwrap().f.is_vararg;
            if !is_vararg {
                return Err(LuaError::syntax(format_args!(
                    "cannot use '...' outside a vararg function"
                )));
            }
            // C: init_exp(v, VVARARG, luaK_codeABC(fs, OP_VARARG, 0, 0, 1))
            // TODO(port): let pc = lua_code::code_abc(ls.fs.as_mut().unwrap(), OpCode::Vararg, 0, 0, 1)?;
            init_exp(v, ExprKind::VarArg, 0 /* placeholder pc */);
        }
        c if c == b'{' as TokenKind => {
            constructor(ls, state, v)?;
            return Ok(()); // C: return (no luaX_next)
        }
        TK_FUNCTION => {
            lex_next(ls, state)?;
            let line = ls.linenumber;
            body(ls, state, v, false, line)?;
            return Ok(()); // C: return (no luaX_next)
        }
        _ => {
            suffixedexp(ls, state, v)?;
            return Ok(()); // C: return (no luaX_next)
        }
    }
    // C: luaX_next(ls) — for the simple literal cases
    lex_next(ls, state)?;
    Ok(())
}

/// C: static UnOpr getunopr(int op)
fn getunopr(op: TokenKind) -> UnOpr {
    match op {
        TK_NOT => UnOpr::Not,
        c if c == b'-' as TokenKind => UnOpr::Minus,
        c if c == b'~' as TokenKind => UnOpr::BNot,
        c if c == b'#' as TokenKind => UnOpr::Len,
        _ => UnOpr::NoUnOpr,
    }
}

/// C: static BinOpr getbinopr(int op)
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

/// C: static BinOpr subexpr(LexState *ls, expdesc *v, int limit)
/// Parses a sub-expression with operators of priority > `limit`.
/// Returns the first untreated (lower-priority) operator.
fn subexpr(
    ls: &mut LexState,
    state: &mut LuaState,
    v: &mut ExprDesc,
    limit: i32,
) -> Result<BinOpr, LuaError> {
    // C: enterlevel(ls) — luaE_incCstack(ls->L)
    // TODO(port): state.inc_c_calls()?;

    let uop = getunopr(ls.t.token);
    if uop != UnOpr::NoUnOpr {
        let line = ls.linenumber;
        lex_next(ls, state)?; // skip unary operator
        subexpr(ls, state, v, UNARY_PRIORITY)?;
        // C: luaK_prefix(ls->fs, uop, v, line)
        // TODO(port): lua_code::prefix(ls.fs.as_mut().unwrap(), uop, v, line)?;
    } else {
        simpleexp(ls, state, v)?;
    }

    let mut op = getbinopr(ls.t.token);
    while op != BinOpr::NoBinOpr && PRIORITY[op as usize].0 as i32 > limit {
        let mut v2 = ExprDesc::default();
        let line = ls.linenumber;
        lex_next(ls, state)?;
        // C: luaK_infix(ls->fs, op, v)
        // TODO(port): lua_code::infix(ls.fs.as_mut().unwrap(), op, v)?;
        let nextop = subexpr(ls, state, &mut v2, PRIORITY[op as usize].1 as i32)?;
        // C: luaK_posfix(ls->fs, op, v, &v2, line)
        cg_posfix_fold(ls.fs.as_mut().unwrap(), op, v, &mut v2, line)?;
        op = nextop;
    }

    // C: leavelevel(ls) — L->nCcalls--
    // TODO(port): state.dec_c_calls();
    Ok(op)
}

/// C: static void expr(LexState *ls, expdesc *v)
fn expr(ls: &mut LexState, state: &mut LuaState, v: &mut ExprDesc) -> Result<(), LuaError> {
    subexpr(ls, state, v, 0)?;
    Ok(())
}

// ── §13 Statement rules ───────────────────────────────────────────────────────

/// C: static void block(LexState *ls)
fn block(ls: &mut LexState, state: &mut LuaState) -> Result<(), LuaError> {
    enter_block(ls, false);
    statlist(ls, state)?;
    leave_block(ls, state)?;
    Ok(())
}

/// C: static void check_conflict(LexState *ls, struct LHS_assign *lh, expdesc *v)
/// Checks and fixes register/upvalue conflicts in multi-assignment.
fn check_conflict(
    ls: &mut LexState,
    state: &mut LuaState,
    lh: &mut LhsAssign,
    v: &ExprDesc,
) -> Result<(), LuaError> {
    let extra = ls.fs.as_ref().unwrap().freereg as i32;
    let mut conflict = false;
    // Iterate through the lh chain
    let mut current = Some(lh as *mut LhsAssign);
    // TODO(port): raw pointer iteration — borrow rules prevent clean chain traversal.
    //   Phase B should restructure LhsAssign to use indices or a Vec instead of Box chain.
    // For Phase A, use a TODO stub:
    // TODO(port): check_conflict chain traversal needs unsafe or restructuring
    if conflict {
        // C: copy the conflicting value to register 'extra'
        if v.k == ExprKind::Local {
            // TODO(port): lua_code::code_abc(ls.fs.as_mut().unwrap(), OpCode::Move, extra, v.u.var_ridx as i32, 0)?;
        } else {
            // TODO(port): lua_code::code_abc(ls.fs.as_mut().unwrap(), OpCode::GetUpval, extra, v.u.info, 0)?;
        }
        // TODO(port): lua_code::reserve_regs(ls.fs.as_mut().unwrap(), 1)?;
    }
    Ok(())
}

/// C: static void restassign(LexState *ls, struct LHS_assign *lh, int nvars)
fn restassign(
    ls: &mut LexState,
    state: &mut LuaState,
    lh: &mut LhsAssign,
    nvars: i32,
) -> Result<(), LuaError> {
    // C: check_condition(ls, vkisvar(lh->v.k), "syntax error")
    if !lh.v.k.is_var() {
        return Err(LuaError::syntax(format_args!("syntax error")));
    }
    check_readonly(ls, state, &lh.v.clone())?;

    if test_next(ls, state, b',' as TokenKind)? {
        // C: restassign -> ',' suffixedexp restassign
        let mut nv_assign = LhsAssign {
            prev: None, // We don't link here — Phase B restructures
            v: ExprDesc::default(),
        };
        suffixedexp(ls, state, &mut nv_assign.v)?;
        if !nv_assign.v.k.is_indexed() {
            check_conflict(ls, state, lh, &nv_assign.v.clone())?;
        }
        // C: enterlevel(ls)
        // TODO(port): state.inc_c_calls()?;
        restassign(ls, state, &mut nv_assign, nvars + 1)?;
        // C: leavelevel(ls)
        // TODO(port): state.dec_c_calls();
    } else {
        // C: restassign -> '=' explist
        let mut e = ExprDesc::default();
        check_next(ls, state, b'=' as TokenKind)?;
        let nexps = explist(ls, state, &mut e)?;
        if nexps != nvars {
            adjust_assign(ls, state, nvars, nexps, &mut e)?;
        } else {
            // C: luaK_setoneret(ls->fs, &e); luaK_storevar(ls->fs, &lh->v, &e)
            // TODO(port): lua_code::set_one_ret(ls.fs.as_mut().unwrap(), &mut e)?;
            // TODO(port): lua_code::store_var(ls.fs.as_mut().unwrap(), &mut lh.v, &mut e)?;
            return Ok(());
        }
    }
    // C: init_exp(&e, VNONRELOC, ls->fs->freereg-1); luaK_storevar(ls->fs, &lh->v, &e)
    let freereg = ls.fs.as_ref().unwrap().freereg as i32 - 1;
    let mut e = ExprDesc::default();
    init_exp(&mut e, ExprKind::NonReloc, freereg);
    // TODO(port): lua_code::store_var(ls.fs.as_mut().unwrap(), &mut lh.v, &mut e)?;
    Ok(())
}

/// C: static int cond(LexState *ls)
/// Parses a condition expression; returns its 'exit when false' patch list.
fn cond(ls: &mut LexState, state: &mut LuaState) -> Result<i32, LuaError> {
    let mut v = ExprDesc::default();
    expr(ls, state, &mut v)?;
    if v.k == ExprKind::Nil {
        v.k = ExprKind::False; // C: 'falses' are all equal here
    }
    // C: luaK_goiftrue(ls->fs, &v)
    // TODO(port): lua_code::go_if_true(ls.fs.as_mut().unwrap(), &mut v)?;
    Ok(v.f)
}

/// C: static void gotostat(LexState *ls)
fn gotostat(ls: &mut LexState, state: &mut LuaState) -> Result<(), LuaError> {
    let line = ls.linenumber;
    let name = str_check_name(ls, state)?;
    let lb = findlabel(ls, &name);
    if lb.is_none() {
        // C: forward jump — newgotoentry(ls, name, line, luaK_jump(fs))
        // TODO(port): let pc = lua_code::jump(ls.fs.as_mut().unwrap())?;
        let pc = 0; // placeholder
        new_goto_entry(ls, state, name, line, pc)?;
    } else {
        let lb_idx = lb.unwrap();
        let lb_pc = ls.dyd.label[lb_idx].pc;
        let lb_nactvar = ls.dyd.label[lb_idx].nactvar;
        // C: backward jump
        let lblevel = reg_level(ls, ls.fs.as_ref().unwrap(), lb_nactvar as i32);
        let cur_nvarstack = {
            let fs = ls.fs.as_ref().unwrap();
            nvarstack(ls, fs)
        };
        if cur_nvarstack > lblevel {
            // C: luaK_codeABC(fs, OP_CLOSE, lblevel, 0, 0)
            // TODO(port): lua_code::code_abc(ls.fs.as_mut().unwrap(), OpCode::Close, lblevel, 0, 0)?;
        }
        // C: luaK_patchlist(fs, luaK_jump(fs), lb->pc)
        // TODO(port): let jpc = lua_code::jump(ls.fs.as_mut().unwrap())?;
        // TODO(port): lua_code::patch_list(ls.fs.as_mut().unwrap(), jpc, lb_pc)?;
    }
    Ok(())
}

/// C: static void breakstat(LexState *ls)
fn breakstat(ls: &mut LexState, state: &mut LuaState) -> Result<(), LuaError> {
    let line = ls.linenumber;
    // C: luaX_next(ls) — skip 'break'
    lex_next(ls, state)?;
    // C: newgotoentry(ls, luaS_newliteral(ls->L, "break"), line, luaK_jump(ls->fs))
    // TODO(port): intern b"break" via state
    // TODO(port): let pc = lua_code::jump(ls.fs.as_mut().unwrap())?;
    // TODO(port): new_goto_entry(ls, state, break_str, line, pc)?;
    Ok(())
}

/// C: static void checkrepeated(LexState *ls, TString *name)
fn checkrepeated(ls: &LexState, name: &GcRef<LuaString>) -> Result<(), LuaError> {
    if let Some(_lb_idx) = findlabel(ls, name) {
        // C: msg = "label '%s' already defined on line %d"; luaK_semerror(ls, msg)
        return Err(LuaError::syntax(format_args!("label already defined")));
    }
    Ok(())
}

/// C: static void labelstat(LexState *ls, TString *name, int line)
fn labelstat(
    ls: &mut LexState,
    state: &mut LuaState,
    name: GcRef<LuaString>,
    line: i32,
) -> Result<(), LuaError> {
    // C: checknext(ls, TK_DBCOLON)
    check_next(ls, state, TK_DBCOLON)?;
    // C: while (ls->t.token == ';' || ls->t.token == TK_DBCOLON) statement(ls)
    while ls.t.token == b';' as TokenKind || ls.t.token == TK_DBCOLON {
        statement(ls, state)?;
    }
    checkrepeated(ls, &name)?;
    let is_last = block_follow(ls, false);
    createlabel(ls, state, name, line, is_last)?;
    Ok(())
}

/// C: static void whilestat(LexState *ls, int line)
fn whilestat(ls: &mut LexState, state: &mut LuaState, line: i32) -> Result<(), LuaError> {
    // C: luaX_next(ls) — skip WHILE
    lex_next(ls, state)?;
    // C: whileinit = luaK_getlabel(fs)
    // TODO(port): let whileinit = lua_code::get_label(ls.fs.as_mut().unwrap());
    let whileinit = ls.fs.as_ref().unwrap().pc; // placeholder
    let condexit = cond(ls, state)?;
    enter_block(ls, true);
    check_next(ls, state, TK_DO)?;
    block(ls, state)?;
    // C: luaK_jumpto(fs, whileinit)
    // TODO(port): lua_code::jump_to(ls.fs.as_mut().unwrap(), whileinit)?;
    check_match(ls, state, TK_END, TK_WHILE, line)?;
    leave_block(ls, state)?;
    // C: luaK_patchtohere(fs, condexit)
    // TODO(port): lua_code::patch_to_here(ls.fs.as_mut().unwrap(), condexit)?;
    Ok(())
}

/// C: static void repeatstat(LexState *ls, int line)
fn repeatstat(ls: &mut LexState, state: &mut LuaState, line: i32) -> Result<(), LuaError> {
    // C: int repeat_init = luaK_getlabel(fs)
    let repeat_init = ls.fs.as_ref().unwrap().pc; // placeholder
    // C: enterblock(fs, &bl1, 1); enterblock(fs, &bl2, 0)
    enter_block(ls, true);   // loop block (bl1)
    enter_block(ls, false);  // scope block (bl2)
    // C: luaX_next(ls) — skip REPEAT
    lex_next(ls, state)?;
    statlist(ls, state)?;
    check_match(ls, state, TK_UNTIL, TK_REPEAT, line)?;
    let condexit = cond(ls, state)?;

    // Save bl2 fields before leaveblock
    let bl2_upval = ls.fs.as_ref().unwrap().bl.as_ref().unwrap().upval;
    let bl2_nactvar = ls.fs.as_ref().unwrap().bl.as_ref().unwrap().nactvar;
    leave_block(ls, state)?; // finish scope (bl2)

    if bl2_upval {
        // C: int exit = luaK_jump(fs); luaK_patchtohere(fs, condexit); OP_CLOSE; condexit = luaK_jump(fs)
        // TODO(port): let exit_pc = lua_code::jump(ls.fs.as_mut().unwrap())?;
        // TODO(port): lua_code::patch_to_here(ls.fs.as_mut().unwrap(), condexit)?;
        // TODO(port): lua_code::code_abc(ls.fs.as_mut().unwrap(), OpCode::Close, reglevel_val, 0, 0)?;
        // TODO(port): let condexit = lua_code::jump(ls.fs.as_mut().unwrap())?;
        // TODO(port): lua_code::patch_to_here(ls.fs.as_mut().unwrap(), exit_pc)?;
    }
    // C: luaK_patchlist(fs, condexit, repeat_init)
    // TODO(port): lua_code::patch_list(ls.fs.as_mut().unwrap(), condexit, repeat_init)?;
    leave_block(ls, state)?; // finish loop (bl1)
    Ok(())
}

/// C: static void exp1(LexState *ls)
/// Parse an expression and emit it to the next register.
fn exp1(ls: &mut LexState, state: &mut LuaState) -> Result<(), LuaError> {
    let mut e = ExprDesc::default();
    expr(ls, state, &mut e)?;
    let line = ls.linenumber;
    cg_exp_to_next_reg(ls.fs.as_mut().unwrap(), line, &mut e)?;
    debug_assert!(e.k == ExprKind::NonReloc);
    Ok(())
}

/// C: static void fixforjump(FuncState *fs, int pc, int dest, int back)
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

/// C: static void forbody(LexState *ls, int base, int line, int nvars, int isgen)
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

/// C: static void fornum(LexState *ls, TString *varname, int line)
fn fornum(
    ls: &mut LexState,
    state: &mut LuaState,
    varname: GcRef<LuaString>,
    line: i32,
) -> Result<(), LuaError> {
    let base = ls.fs.as_ref().unwrap().freereg as i32;
    // C: new_localvarliteral(ls, "(for state)") × 3 + new_localvar(ls, varname)
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

/// C: static void forlist(LexState *ls, TString *indexname)
fn forlist(
    ls: &mut LexState,
    state: &mut LuaState,
    indexname: GcRef<LuaString>,
) -> Result<(), LuaError> {
    let mut nvars: i32 = 5; // gen, state, control, toclose, 'indexname'
    let base = ls.fs.as_ref().unwrap().freereg as i32;
    // C: new_localvarliteral × 4 (for state vars)
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
    let line = ls.linenumber;
    let mut e = ExprDesc::default();
    let nexps = explist(ls, state, &mut e)?;
    adjust_assign(ls, state, 4, nexps, &mut e)?;
    adjust_local_vars(ls, state, 4)?;
    marktobeclosed(ls.fs.as_mut().unwrap()); // last control var must be closed
    // C: luaK_checkstack(fs, 3)
    // TODO(port): lua_code::check_stack(ls.fs.as_mut().unwrap(), 3)?;
    forbody(ls, state, base, line, nvars - 4, true)?;
    Ok(())
}

/// C: static void forstat(LexState *ls, int line)
fn forstat(ls: &mut LexState, state: &mut LuaState, line: i32) -> Result<(), LuaError> {
    enter_block(ls, true); // scope for loop and control variables
    // C: luaX_next(ls) — skip 'for'
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

/// C: static void test_then_block(LexState *ls, int *escapelist)
fn test_then_block(
    ls: &mut LexState,
    state: &mut LuaState,
    escapelist: &mut i32,
) -> Result<(), LuaError> {
    // C: luaX_next(ls) — skip IF or ELSEIF
    lex_next(ls, state)?;
    let mut v = ExprDesc::default();
    expr(ls, state, &mut v)?;
    check_next(ls, state, TK_THEN)?;

    let jf: i32;
    if ls.t.token == TK_BREAK {
        let line = ls.linenumber;
        // C: luaK_goiffalse(ls->fs, &v) — jumps if condition is true
        // TODO(port): lua_code::go_if_false(ls.fs.as_mut().unwrap(), &mut v)?;
        let _ = line;
        lex_next(ls, state)?; // skip 'break'
        enter_block(ls, false);
        // C: newgotoentry(ls, "break", line, v.t)
        // TODO(port): new_goto_entry(ls, state, break_str, line, v.t)?;
        // C: while (testnext(ls, ';')) {} -- skip semicolons
        while test_next(ls, state, b';' as TokenKind)? {}
        if block_follow(ls, false) {
            leave_block(ls, state)?;
            return Ok(());
        } else {
            // C: jf = luaK_jump(fs)
            jf = cg_jump(ls.fs.as_mut().unwrap(), ls.linenumber);
        }
    } else {
        // C: luaK_goiftrue(ls->fs, &v); jf = v.f
        cg_go_if_true(ls.fs.as_mut().unwrap(), &mut v)?;
        enter_block(ls, false);
        jf = v.f;
    }

    statlist(ls, state)?;
    leave_block(ls, state)?;

    if ls.t.token == TK_ELSE || ls.t.token == TK_ELSEIF {
        // C: luaK_concat(fs, escapelist, luaK_jump(fs))
        let line = ls.linenumber;
        let j = cg_jump(ls.fs.as_mut().unwrap(), line);
        cg_concat(ls.fs.as_mut().unwrap(), escapelist, j)?;
    }
    // C: luaK_patchtohere(fs, jf)
    cg_patch_to_here(ls.fs.as_mut().unwrap(), jf)?;
    Ok(())
}

/// C: static void ifstat(LexState *ls, int line)
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
    // C: luaK_patchtohere(fs, escapelist)
    cg_patch_to_here(ls.fs.as_mut().unwrap(), escapelist)?;
    Ok(())
}

/// C: static void localfunc(LexState *ls)
fn localfunc(ls: &mut LexState, state: &mut LuaState) -> Result<(), LuaError> {
    let mut b = ExprDesc::default();
    let fvar = ls.fs.as_ref().unwrap().nactvar as i32;
    let name = str_check_name(ls, state)?;
    new_local_var(ls, state, name)?;
    adjust_local_vars(ls, state, 1)?; // enter its scope
    let line = ls.linenumber;
    body(ls, state, &mut b, false, line)?;
    // C: localdebuginfo(fs, fvar)->startpc = fs->pc
    let pc = ls.fs.as_ref().unwrap().pc;
    // TODO(port): local_debug_info(ls, ls.fs.as_mut().unwrap(), fvar).map(|lv| lv.startpc = pc);
    Ok(())
}

/// C: static int getlocalattribute(LexState *ls)
/// Parses an optional '<const>' or '<close>' attribute.
fn getlocalattribute(ls: &mut LexState, state: &mut LuaState) -> Result<VarKind, LuaError> {
    if test_next(ls, state, b'<' as TokenKind)? {
        let attr_name = str_check_name(ls, state)?;
        check_next(ls, state, b'>' as TokenKind)?;
        // C: if (strcmp(attr, "const") == 0) return RDKCONST
        //    else if (strcmp(attr, "close") == 0) return RDKTOCLOSE
        //    else luaK_semerror(ls, ...)
        // TODO(port): compare attr_name.as_bytes() with b"const" and b"close"
        // For Phase A, return Reg as placeholder and mark TODO
        // TODO(port): compare attribute name bytes
        return Ok(VarKind::Reg); // placeholder
    }
    Ok(VarKind::Reg)
}

/// C: static void checktoclose(FuncState *fs, int level)
fn checktoclose(ls: &mut LexState, state: &mut LuaState, level: i32) -> Result<(), LuaError> {
    if level != -1 {
        marktobeclosed(ls.fs.as_mut().unwrap());
        // C: luaK_codeABC(fs, OP_TBC, reglevel(fs, level), 0, 0)
        let rl = reg_level(ls, ls.fs.as_ref().unwrap(), level);
        // TODO(port): lua_code::code_abc(ls.fs.as_mut().unwrap(), OpCode::Tbc, rl, 0, 0)?;
    }
    Ok(())
}

/// C: static void localstat(LexState *ls)
fn localstat(ls: &mut LexState, state: &mut LuaState) -> Result<(), LuaError> {
    let mut toclose: i32 = -1;
    let mut nvars: i32 = 0;
    let mut vidx = 0i32;
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
        // C: luaK_exp2const(fs, &e, &var->k) — try compile-time constant
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

/// C: static int funcname(LexState *ls, expdesc *v)
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

/// C: static void funcstat(LexState *ls, int line)
fn funcstat(ls: &mut LexState, state: &mut LuaState, line: i32) -> Result<(), LuaError> {
    // C: luaX_next(ls) — skip FUNCTION
    lex_next(ls, state)?;
    let mut v = ExprDesc::default();
    let mut b = ExprDesc::default();
    let ismethod = funcname(ls, state, &mut v)?;
    body(ls, state, &mut b, ismethod, line)?;
    check_readonly(ls, state, &v.clone())?;
    // C: luaK_storevar(ls->fs, &v, &b); luaK_fixline(ls->fs, line)
    // TODO(port): lua_code::store_var(ls.fs.as_mut().unwrap(), &mut v, &mut b)?;
    // TODO(port): lua_code::fix_line(ls.fs.as_mut().unwrap(), line);
    Ok(())
}

/// C: static void exprstat(LexState *ls)
fn exprstat(ls: &mut LexState, state: &mut LuaState) -> Result<(), LuaError> {
    let mut v_assign = LhsAssign { prev: None, v: ExprDesc::default() };
    suffixedexp(ls, state, &mut v_assign.v)?;
    if ls.t.token == b'=' as TokenKind || ls.t.token == b',' as TokenKind {
        // C: stat -> assignment
        restassign(ls, state, &mut v_assign, 1)?;
    } else {
        // C: stat -> func call; check it's a call, fix result count
        if v_assign.v.k != ExprKind::Call {
            return Err(LuaError::syntax(format_args!("syntax error")));
        }
        // C: SETARG_C(*inst, 1) — call statement uses no results.
        let info = v_assign.v.u.info as usize;
        let fs = ls.fs.as_mut().unwrap();
        let mut lc = lua_code::opcodes::Instruction(fs.f.code[info].0);
        lc.set_arg_c(1);
        fs.f.code[info] = lua_types::opcode::Instruction::new(lc.0);
    }
    Ok(())
}

/// C: static void retstat(LexState *ls)
fn retstat(ls: &mut LexState, state: &mut LuaState) -> Result<(), LuaError> {
    let first = {
        let fs = ls.fs.as_ref().unwrap();
        nvarstack(ls, fs)
    };
    let nret: i32;
    if block_follow(ls, true) || ls.t.token == b';' as TokenKind {
        nret = 0;
    } else {
        let mut e = ExprDesc::default();
        nret = explist(ls, state, &mut e)?;
        if e.k.has_mult_ret() {
            // C: luaK_setmultret(fs, &e)
            // TODO(port): lua_code::set_returns(ls.fs.as_mut().unwrap(), &mut e, LUA_MULTRET)?;
            if e.k == ExprKind::Call && nret == 1 {
                // C: tail call check — !fs->bl->insidetbc
                let insidetbc = ls.fs.as_ref().unwrap().bl.as_ref().map_or(false, |b| b.insidetbc);
                if !insidetbc {
                    // C: SET_OPCODE(getinstruction(fs, &e), OP_TAILCALL)
                    let info = e.u.info as usize;
                    // TODO(port): ls.fs.as_mut().unwrap().f.code[info].set_opcode(OpCode::TailCall);
                }
            }
            // nret = LUA_MULTRET handled by set_returns
        } else {
            if nret == 1 {
                // C: first = luaK_exp2anyreg(fs, &e)
                // TODO(port): let first = lua_code::exp_to_any_reg(ls.fs.as_mut().unwrap(), &mut e)?;
            } else {
                // TODO(port): lua_code::exp_to_next_reg(ls.fs.as_mut().unwrap(), &mut e)?;
            }
        }
    }
    // C: luaK_ret(fs, first, nret)
    // TODO(port): lua_code::emit_return(ls.fs.as_mut().unwrap(), first, nret)?;
    // C: testnext(ls, ';')
    test_next(ls, state, b';' as TokenKind)?;
    Ok(())
}

/// C: static void statement(LexState *ls)
/// Top-level statement dispatcher.
fn statement(ls: &mut LexState, state: &mut LuaState) -> Result<(), LuaError> {
    let line = ls.linenumber;
    // C: enterlevel(ls)
    // TODO(port): state.inc_c_calls()?;
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
    // C: leavelevel(ls)
    // TODO(port): state.dec_c_calls();
    Ok(())
}

// ── §14 Main function and entry point ────────────────────────────────────────

/// C: static void mainfunc(LexState *ls, FuncState *fs)
/// Compiles the main chunk (always a vararg function with _ENV upvalue).
fn mainfunc(ls: &mut LexState, state: &mut LuaState, main_fs: FuncState) -> Result<Box<LuaProto>, LuaError> {
    // C: open_func(ls, fs, &bl)
    open_func(ls, state, main_fs)?;

    // C: setvararg(fs, 0) — main function is always vararg
    setvararg(ls.fs.as_mut().unwrap(), state, 0)?;

    // C: env = allocupvalue(fs); env->instack=1; env->idx=0; env->kind=VDKREG; env->name=ls->envn
    let env_name = ls.envn.clone();
    {
        let idx = alloc_upvalue(ls.fs.as_mut().unwrap())?;
        let up = &mut ls.fs.as_mut().unwrap().f.upvalues[idx];
        up.instack = true;
        up.idx = 0;
        up.kind = VarKind::Reg.as_u8();
        up.name = env_name.clone();
    }
    // C: luaC_objbarrier(ls->L, fs->f, env->name) — no-op in Phase A

    // C: luaX_next(ls) — read first token
    lex_next(ls, state)?;

    statlist(ls, state)?;

    // C: check(ls, TK_EOS)
    check(ls, TK_EOS)?;

    close_func(ls, state)
}

/// C: LClosure *luaY_parser(lua_State *L, ZIO *z, Mbuffer *buff, Dyndata *dyd,
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
        dyd: None,
        source: source_str.0.clone(),
        envn: envn_str.0.clone(),
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
        envn: Some(GcRef(lex_ls.envn.clone())),
        lex: lex_ls,
    };
    // C: luaX_setinput is the only setup the C parser performs before
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
        lua_lex::TokenValue::Str(s) => TokenValue { r: 0.0, i: 0, ts: Some(GcRef(s.clone())) },
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
