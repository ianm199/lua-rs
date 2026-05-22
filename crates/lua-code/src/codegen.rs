//! Code generator for Lua 5.4.
//!
//! Ports `src/lcode.c` (1875 lines, ~60 functions) and the public interface
//! from `src/lcode.h`.
//!
//! Phase A: faithful logic translation — does not yet compile.
//!
//! TODO(port): FuncState.f is GcRef<LuaProto> per types.tsv but the code
//! generator mutates it directly.  Phase B must wrap LuaProto in
//! RefCell<LuaProto> (or similar) inside GcRef so that `fs.f.code`, `fs.f.k`,
//! etc. are mutable.  All `fs.f.<field>` accesses below assume direct mutability.
//!
//! TODO(port): LexState.L (the &mut LuaState back-pointer) was removed per
//! types.tsv.  Functions that call luaM_growvector / luaH_get / luaC_barrier /
//! luaO_rawarith carry an explicit `state: &mut LuaState` parameter here.
//! The caller (lua-parse) must thread it through.

// C: /* $Id: lcode.c $ */
// C: #define lcode_c
// C: #define LUA_CORE

use crate::opcodes::{
    Instruction, OpCode, OpMode,
    MAXARG_A, MAXARG_B, MAXARG_C, MAXARG_BX, MAXARG_AX, MAXARG_S_J,
    OFFSET_S_BX, OFFSET_S_J, OFFSET_S_C,
    NO_REG, MAXINDEXRK, LFIELDS_PER_FLUSH,
};

use lua_types::error::LuaError;
use lua_types::value::LuaValue;
use lua_types::string::LuaString;
use lua_types::gc::GcRef;
use lua_types::tagmethod::TagMethod;

// ─── PHASE B PLACEHOLDERS ─────────────────────────────────────────────────────
//
// The following types live in `lua-parse` (`FuncState`, `ExprDesc`, `ExprKind`,
// `LexState`) and `lua-vm` (`LuaState`, `LuaTable`).  Importing them from those
// crates would create a dependency cycle (`lua-vm` depends on `lua-code`), so
// we keep stub shapes here in Phase B.  The real definitions land when those
// crates fully exist; the field/method signatures below are inferred from the
// concrete usage in `codegen.rs`.

/// PHASE B: real definition lives in lua-types (`lua_types::proto::LuaProto`),
/// but it stores `Vec<lua_types::opcode::Instruction>` — a barebones u32 wrapper
/// without the `arg_a` / `set_opcode` / `is_in_top` / etc. methods that the
/// code generator calls.  Those methods live on the local
/// `crate::opcodes::Instruction` type.  Until the two `Instruction` types are
/// unified (Phase B), `FuncState` carries a local proto shape that uses the
/// methodful local `Instruction`.
pub struct LuaProto {
    pub numparams: u8,
    pub is_vararg: bool,
    pub maxstacksize: u8,
    pub k: Vec<LuaValue>,
    pub code: Vec<Instruction>,
    pub lineinfo: Vec<i8>,
    pub abslineinfo: Vec<AbsLineInfo>,
}

/// PHASE B: mirrors `lua_types::proto::AbsLineInfo`; kept local so it pairs
/// with the local `LuaProto` placeholder above.
#[derive(Clone, Copy)]
pub struct AbsLineInfo {
    pub pc: i32,
    pub line: i32,
}

/// PHASE B: real definition lives in lua-parse; placeholder shape inferred
/// from codegen usage.
pub struct FuncState {
    pub f: LuaProto,
    pub pc: i32,
    pub lasttarget: i32,
    pub previousline: i32,
    pub iwthabs: u8,
    pub nabslineinfo: i32,
    pub nactvar: u8,
    pub freereg: u8,
    pub nk: i32,
    pub needclose: bool,
}

/// PHASE B: real definition lives in lua-parse; placeholder shape inferred
/// from codegen usage.  Mirrors the C `expdesc` struct (a tagged kind plus a
/// payload union); the union becomes a plain struct here so every variant's
/// fields are addressable.
pub struct ExprDesc {
    pub k: ExprKind,
    pub t: i32,
    pub f: i32,
    pub u: ExprDescU,
}

impl Clone for ExprDesc {
    fn clone(&self) -> Self {
        ExprDesc {
            k: self.k,
            t: self.t,
            f: self.f,
            u: ExprDescU {
                info: self.u.info,
                ival: self.u.ival,
                nval: self.u.nval,
                strval: self.u.strval.clone(),
                var: self.u.var,
                ind: self.u.ind,
            },
        }
    }
}

impl ExprDesc {
    /// C: `static const expdesc ef = {VKINT, {0}, NO_JUMP, NO_JUMP};`
    /// PHASE B: a zero `VKINT` literal used as a synthetic operand by the
    /// constant-folding helpers in `prefix()`.
    pub fn const_zero() -> Self {
        todo!("PHASE B: ExprDesc::const_zero — real shape requires GcRef<LuaString> default")
    }
}

/// PHASE B: real definition lives in lua-parse; placeholder shape inferred
/// from codegen usage.  The C union becomes a struct: simultaneous fields are
/// harmless because Phase B will gate them by `ExprKind`.
pub struct ExprDescU {
    pub info: i32,
    pub ival: i64,
    pub nval: f64,
    pub strval: GcRef<LuaString>,
    pub var: ExprDescVar,
    pub ind: ExprDescInd,
}

/// PHASE B: real definition lives in lua-parse; placeholder shape inferred
/// from codegen usage.
#[derive(Clone, Copy)]
pub struct ExprDescVar {
    pub ridx: u8,
}

/// PHASE B: real definition lives in lua-parse; placeholder shape inferred
/// from codegen usage.
#[derive(Clone, Copy)]
pub struct ExprDescInd {
    pub t: u8,
    pub idx: i16,
}

/// PHASE B: real definition lives in lua-parse; placeholder shape inferred
/// from codegen usage.  Variant set comes from the `ExprKind::V*` literals
/// referenced in `codegen.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExprKind {
    VCall,
    VConst,
    VFalse,
    VIndexI,
    VIndexStr,
    VIndexUp,
    VIndexed,
    VJmp,
    VK,
    VKFlt,
    VKInt,
    VKStr,
    VLocal,
    VNil,
    VNonReloc,
    VReloc,
    VTrue,
    VUpVal,
    VVarArg,
}

/// PHASE B: real definition lives in lua-parse; placeholder shape inferred
/// from codegen usage.
pub struct LexState {
    pub t: LexToken,
    pub dyd: Dyndata,
    pub lastline: i32,
    pub h: GcRef<LuaTable>,
}

/// PHASE B: real definition lives in lua-parse; placeholder shape inferred
/// from codegen usage.
pub struct LexToken {
    pub token: i32,
}

/// PHASE B: real definition lives in lua-parse; placeholder shape inferred
/// from codegen usage.
pub struct Dyndata {
    pub actvar: Vec<ActvarEntry>,
}

/// PHASE B: real definition lives in lua-parse; placeholder shape inferred
/// from codegen usage.
pub struct ActvarEntry {
    pub k: LuaValue,
}

/// PHASE B: real definition lives in lua-vm; placeholder shape inferred from
/// codegen usage (only `equal_obj` and `raw_arith` are called here).
pub struct LuaState {
    _private: (),
}

impl LuaState {
    pub fn equal_obj(&self, _mt: Option<()>, _a: &LuaValue, _b: &LuaValue) -> bool {
        todo!("PHASE B: LuaState::equal_obj — lives in lua-vm")
    }

    pub fn raw_arith(
        &mut self,
        _op: i32,
        _a: &LuaValue,
        _b: &LuaValue,
    ) -> Result<LuaValue, LuaError> {
        todo!("PHASE B: LuaState::raw_arith — lives in lua-vm")
    }
}

/// PHASE B: the runtime table type lives in lua-vm; the existing
/// `lua_types::value::LuaTable` is a forward-decl with no methods.  We re-use
/// that type for `ls.h` so it can be `clone()`d into `LuaValue::Table`; the
/// `get` / `finish_set` operations are exposed here as free-function shims
/// (they will become real method calls in Phase B).
pub use lua_types::value::LuaTable;

/// PHASE B: real impl lives in lua-vm (`LuaTable::get`).
fn lex_h_get(_table: &GcRef<LuaTable>, _state: &LuaState, _key: &LuaValue) -> LuaValue {
    todo!("PHASE B: LuaTable::get — lives in lua-vm")
}

/// PHASE B: real impl lives in lua-vm (`LuaTable::finish_set`).
fn lex_h_finish_set(
    _table: &GcRef<LuaTable>,
    _state: &mut LuaState,
    _key: &LuaValue,
    _val: &LuaValue,
) -> Result<(), LuaError> {
    todo!("PHASE B: LuaTable::finish_set — lives in lua-vm")
}

/// PHASE B: real definition for `TagMethod::from_delta` lives in lua-vm; this
/// shim performs the offset math locally so codegen can keep its
/// `binopr2_tm`-style ports.
#[inline]
fn tag_method_from_delta(base: TagMethod, delta: i32) -> TagMethod {
    let raw = (base as i32 + delta) as u8;
    use TagMethod::*;
    match raw {
        0  => Index,
        1  => NewIndex,
        2  => Gc,
        3  => Mode,
        4  => Len,
        5  => Eq,
        6  => Add,
        7  => Sub,
        8  => Mul,
        9  => Mod,
        10 => Pow,
        11 => Div,
        12 => Idiv,
        13 => Band,
        14 => Bor,
        15 => Bxor,
        16 => Shl,
        17 => Shr,
        18 => Unm,
        19 => Bnot,
        20 => Lt,
        21 => Le,
        22 => Concat,
        23 => Call,
        24 => Close,
        _  => todo!("PHASE B: TagMethod::from_delta out-of-range"),
    }
}

/// PHASE B: real definition for `OpCode::from_delta` lives in lua-code/opcodes
/// but isn't wired up yet; this shim performs the offset cast.
#[inline]
fn opcode_from_delta(base: OpCode, delta: i32) -> OpCode {
    let raw = (base as i32 + delta) as u32;
    match OpCode::from_u32(raw) {
        Some(op) => op,
        None => todo!("PHASE B: OpCode::from_delta out-of-range"),
    }
}

// ─── Constants (lcode.h + local) ──────────────────────────────────────────────

// C: #define NO_JUMP (-1)
/// End-of-patch-list sentinel.  An invalid value both as an absolute address
/// and as a list link (would link an element to itself).
pub const NO_JUMP: i32 = -1;

// C: #define MAXREGS 255
/// Maximum number of registers in a Lua function (must fit in 8 bits).
const MAXREGS: i32 = 255;

// C: #define LIMLINEDIFF 0x80
/// Limit for difference between lines in relative line info.
const LIM_LINE_DIFF: i32 = 0x80;

// From macros.tsv: MAX_IWTH_ABS = 128
const MAX_IWTH_ABS: i32 = 128;

// From macros.tsv: ABS_LINE_INFO = -0x80
const ABS_LINE_INFO: i8 = -0x80i8;

// ─── BinOpr (lcode.h) ─────────────────────────────────────────────────────────
//
// C: /* grep "ORDER OPR" if you change these enums  (ORDER OP) */
// C: typedef enum BinOpr { OPR_ADD, OPR_SUB, ... OPR_NOBINOPR } BinOpr;
//
// ORDER OPR — discriminants must match `lcode.h` exactly because binopr2op /
// binopr2TM use arithmetic on them.

/// Binary operator kinds.  ORDER OPR must match `lcode.h`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum BinOpr {
    // arithmetic
    Add    = 0,
    Sub    = 1,
    Mul    = 2,
    Mod    = 3,
    Pow    = 4,
    Div    = 5,
    IDiv   = 6,
    // bitwise
    BAnd   = 7,
    BOr    = 8,
    BXor   = 9,
    Shl    = 10,
    Shr    = 11,
    // string
    Concat = 12,
    // comparison
    Eq     = 13,
    Lt     = 14,
    Le     = 15,
    Ne     = 16,
    Gt     = 17,
    Ge     = 18,
    // logical
    And    = 19,
    Or     = 20,
    // sentinel
    NoBinOpr = 21,
}

impl BinOpr {
    /// C: `#define foldbinop(op)  ((op) <= OPR_SHR)`
    /// True if operation is constant-foldable (arithmetic or bitwise).
    #[inline]
    pub fn is_foldable(self) -> bool {
        // C: foldbinop(op) ((op) <= OPR_SHR)
        (self as u8) <= BinOpr::Shr as u8
    }
}

// ─── UnOpr (lcode.h) ──────────────────────────────────────────────────────────
//
// C: typedef enum UnOpr { OPR_MINUS, OPR_BNOT, OPR_NOT, OPR_LEN, OPR_NOUNOPR } UnOpr;

/// Unary operator kinds.  ORDER OPR must match `lcode.h`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum UnOpr {
    Minus   = 0,
    BNot    = 1,
    Not     = 2,
    Len     = 3,
    NoUnOpr = 4,
}

// ─── ExprKind (from lparser.h, ORDER must match) ──────────────────────────────
//
// Defined in lua-parse but reproduced here for reference:
// VVOID=0, VNIL=1, VTRUE=2, VFALSE=3, VK=4, VKFLT=5, VKINT=6, VKSTR=7,
// VNONRELOC=8, VLOCAL=9, VUPVAL=10, VCONST=11, VINDEXED=12, VINDEXUP=13,
// VINDEXI=14, VINDEXSTR=15, VJMP=16, VRELOC=17, VCALL=18, VVARARG=19
//
// PORT NOTE: ExprKind is defined in lua-parse; this comment documents the
// ordering invariants used for range comparisons below.

// ─── Arithmetic operation codes (lua.h) ──────────────────────────────────────
//
// C: LUA_OPADD=0, LUA_OPSUB, LUA_OPMUL, LUA_OPMOD, LUA_OPPOW, LUA_OPDIV,
//    LUA_OPIDIV, LUA_OPBAND, LUA_OPBOR, LUA_OPBXOR, LUA_OPSHL, LUA_OPSHR,
//    LUA_OPCONCAT, LUA_OPUNM, LUA_OPBNOT, LUA_OPLEN, LUA_OPNOT
//
// PORT NOTE: used in constfolding / binopr2TM offset arithmetic.
// TODO(port): define ArithOp enum in lua-types and use it instead of raw i32.
const LUA_OPADD: i32  = 0;
const LUA_OPUNM: i32  = 13;

// ─── Semantic-error helper ────────────────────────────────────────────────────

// C: l_noret luaK_semerror (LexState *ls, const char *msg) {
//      ls->t.token = 0;
//      luaX_syntaxerror(ls, msg);
//    }
/// Raise a semantic error, suppressing the "near <token>" addendum.
/// Returns the error value; callers do `return Err(sem_error(ls, msg))`.
/// PORT NOTE: C declares this `l_noret`; Rust stable doesn't allow `Result<!,_>`
/// as a stable return type, so we return the `LuaError` directly.
pub(crate) fn sem_error(ls: &mut LexState, msg: &str) -> LuaError {
    ls.t.token = 0; // remove "near <token>" from final message
    LuaError::syntax(format_args!("{}", msg))
}

// ─── Numeral predicate ────────────────────────────────────────────────────────

// C: static int tonumeral (const expdesc *e, TValue *v) { ... }
/// If `e` is a numeric constant, fill `v` with its value and return true.
/// Returns false when `e` has pending jumps (not a plain constant).
fn tonumeral(e: &ExprDesc, v: Option<&mut LuaValue>) -> bool {
    // C: if (hasjumps(e)) return 0;
    if e.t != e.f {
        return false;
    }
    match e.k {
        ExprKind::VKInt => {
            if let Some(dst) = v {
                *dst = LuaValue::Int(e.u.ival);
            }
            true
        }
        ExprKind::VKFlt => {
            if let Some(dst) = v {
                *dst = LuaValue::Float(e.u.nval);
            }
            true
        }
        _ => false,
    }
}

// ─── Constant-expression helpers ─────────────────────────────────────────────

// C: static TValue *const2val (FuncState *fs, const expdesc *e) {
//      lua_assert(e->k == VCONST);
//      return &fs->ls->dyd->actvar.arr[e->u.info].k;
//    }
/// Return the compile-time constant value stored for a VCONST expression.
fn const2val(fs: &FuncState, ls: &LexState, e: &ExprDesc) -> LuaValue {
    debug_assert!(e.k == ExprKind::VConst);
    // C: &fs->ls->dyd->actvar.arr[e->u.info].k
    ls.dyd.actvar[e.u.info as usize].k.clone()
}

// C: int luaK_exp2const (FuncState *fs, const expdesc *e, TValue *v) { ... }
/// If `e` is a compile-time constant, copy its value into `v` and return true.
pub(crate) fn exp2const(fs: &FuncState, ls: &LexState, e: &ExprDesc, v: &mut LuaValue) -> bool {
    // C: if (hasjumps(e)) return 0;
    if e.t != e.f {
        return false;
    }
    match e.k {
        ExprKind::VFalse => { *v = LuaValue::Bool(false); true }
        ExprKind::VTrue  => { *v = LuaValue::Bool(true);  true }
        ExprKind::VNil   => { *v = LuaValue::Nil;          true }
        ExprKind::VKStr  => {
            // C: setsvalue(fs->ls->L, v, e->u.strval)
            *v = LuaValue::Str(e.u.strval.clone());
            true
        }
        ExprKind::VConst => {
            *v = const2val(fs, ls, e);
            true
        }
        _ => tonumeral(e, Some(v)),
    }
}

// ─── Previous-instruction helper ─────────────────────────────────────────────

// C: static Instruction *previousinstruction (FuncState *fs) {
//      static const Instruction invalidinstruction = ~(Instruction)0;
//      if (fs->pc > fs->lasttarget)
//        return &fs->f->code[fs->pc - 1];
//      else
//        return cast(Instruction*, &invalidinstruction);
//    }
//
// PORT NOTE: In C the "invalid" case returns a pointer to 0xFFFFFFFF which
// will never match any real opcode.  In Rust we return Option<usize>; None
// means "no accessible previous instruction".
fn previous_instruction_idx(fs: &FuncState) -> Option<usize> {
    if fs.pc > fs.lasttarget {
        Some((fs.pc - 1) as usize)
    } else {
        None
    }
}

// ─── OP_LOADNIL optimisation ──────────────────────────────────────────────────

// C: void luaK_nil (FuncState *fs, int from, int n) { ... }
/// Emit OP_LOADNIL for registers `from..from+n-1`, merging with the previous
/// OP_LOADNIL when the ranges are compatible.
pub(crate) fn nil(
    fs: &mut FuncState,
    ls: &mut LexState,
    from: i32,
    n: i32,
) -> Result<(), LuaError> {
    let mut from = from;
    let mut l = from + n - 1; // last register to set nil
    if let Some(prev_idx) = previous_instruction_idx(fs) {
        let prev = fs.f.code[prev_idx];
        if prev.opcode() == Some(OpCode::LoadNil) {
            // C: int pfrom = GETARG_A(*previous); int pl = pfrom + GETARG_B(*previous);
            let pfrom = prev.arg_a() as i32;
            let pl = pfrom + prev.arg_b() as i32;
            if (pfrom <= from && from <= pl + 1) || (from <= pfrom && pfrom <= l + 1) {
                if pfrom < from { from = pfrom; }
                if pl > l { l = pl; }
                fs.f.code[prev_idx].set_arg_a(from as u32);
                fs.f.code[prev_idx].set_arg_b((l - from) as u32);
                return Ok(());
            }
        }
    }
    // C: luaK_codeABC(fs, OP_LOADNIL, from, n - 1, 0);
    code_abc(fs, ls, OpCode::LoadNil, from as u32, (n - 1) as u32, 0)?;
    Ok(())
}

// ─── Jump list traversal ──────────────────────────────────────────────────────

// C: static int getjump (FuncState *fs, int pc) { ... }
fn getjump(fs: &FuncState, pc: i32) -> i32 {
    let offset = fs.f.code[pc as usize].arg_s_j();
    if offset == NO_JUMP {
        NO_JUMP
    } else {
        (pc + 1) + offset
    }
}

// C: static void fixjump (FuncState *fs, int pc, int dest) { ... }
fn fixjump(fs: &mut FuncState, ls: &LexState, pc: i32, dest: i32) -> Result<(), LuaError> {
    debug_assert!(dest != NO_JUMP);
    let offset = dest - (pc + 1);
    // C: if (!(-OFFSET_sJ <= offset && offset <= MAXARG_sJ - OFFSET_sJ))
    if !((-OFFSET_S_J <= offset) && (offset <= MAXARG_S_J as i32 - OFFSET_S_J)) {
        return Err(LuaError::syntax(format_args!("control structure too long")));
    }
    debug_assert!(fs.f.code[pc as usize].opcode() == Some(OpCode::Jmp));
    fs.f.code[pc as usize].set_arg_s_j(offset);
    Ok(())
}

// C: void luaK_concat (FuncState *fs, int *l1, int l2) { ... }
/// Concatenate jump-list `l2` into jump-list `l1`.
pub(crate) fn concat(
    fs: &mut FuncState,
    ls: &LexState,
    l1: &mut i32,
    l2: i32,
) -> Result<(), LuaError> {
    if l2 == NO_JUMP { return Ok(()); }
    if *l1 == NO_JUMP {
        *l1 = l2;
    } else {
        let mut list = *l1;
        loop {
            let next = getjump(fs, list);
            if next == NO_JUMP { break; }
            list = next;
        }
        fixjump(fs, ls, list, l2)?;
    }
    Ok(())
}

// C: int luaK_jump (FuncState *fs) { return codesJ(fs, OP_JMP, NO_JUMP, 0); }
/// Emit an unconditional jump.  Returns the pc of the jump instruction.
pub(crate) fn jump(fs: &mut FuncState, ls: &mut LexState) -> Result<i32, LuaError> {
    codes_j(fs, ls, OpCode::Jmp, NO_JUMP, 0)
}

// C: void luaK_ret (FuncState *fs, int first, int nret) { ... }
pub(crate) fn ret(
    fs: &mut FuncState,
    ls: &mut LexState,
    first: i32,
    nret: i32,
) -> Result<(), LuaError> {
    let op = match nret {
        0 => OpCode::Return0,
        1 => OpCode::Return1,
        _ => OpCode::Return,
    };
    code_abc(fs, ls, op, first as u32, (nret + 1) as u32, 0)?;
    Ok(())
}

// C: static int condjump (FuncState *fs, OpCode op, int A, int B, int C, int k) { ... }
fn condjump(
    fs: &mut FuncState,
    ls: &mut LexState,
    op: OpCode,
    a: u32,
    b: u32,
    c: u32,
    k: u32,
) -> Result<i32, LuaError> {
    code_abck(fs, ls, op, a, b, c, k)?;
    jump(fs, ls)
}

// C: int luaK_getlabel (FuncState *fs) { fs->lasttarget = fs->pc; return fs->pc; }
pub(crate) fn getlabel(fs: &mut FuncState) -> i32 {
    fs.lasttarget = fs.pc;
    fs.pc
}

// C: static Instruction *getjumpcontrol (FuncState *fs, int pc) { ... }
/// Return the index of the instruction "controlling" the jump at `pc`.
/// That is: if the previous instruction is a test, return its index;
/// otherwise return `pc` itself.
fn getjumpcontrol(fs: &FuncState, pc: i32) -> usize {
    let pi = pc as usize;
    if pi >= 1 {
        if let Some(prev_op) = fs.f.code[pi - 1].opcode() {
            // C: testTMode(GET_OPCODE(*(pi-1)))
            if crate::opcodes::test_t_mode(prev_op) {
                return pi - 1;
            }
        }
    }
    pi
}

// C: static int patchtestreg (FuncState *fs, int node, int reg) { ... }
fn patchtestreg(fs: &mut FuncState, node: i32, reg: u32) -> bool {
    let ctrl = getjumpcontrol(fs, node);
    if fs.f.code[ctrl].opcode() != Some(OpCode::TestSet) {
        return false;
    }
    if reg != NO_REG && reg != fs.f.code[ctrl].arg_b() {
        fs.f.code[ctrl].set_arg_a(reg);
    } else {
        // change to simple TEST; C: CREATE_ABCk(OP_TEST, GETARG_B(*i), 0, 0, GETARG_k(*i))
        let b = fs.f.code[ctrl].arg_b();
        let k = fs.f.code[ctrl].arg_k();
        fs.f.code[ctrl] = Instruction::abck(OpCode::Test, b, 0, 0, k);
    }
    true
}

// C: static void removevalues (FuncState *fs, int list) { ... }
fn removevalues(fs: &mut FuncState, list: i32) {
    let mut list = list;
    while list != NO_JUMP {
        patchtestreg(fs, list, NO_REG);
        list = getjump(fs, list);
    }
}

// C: static void patchlistaux (FuncState *fs, int list, int vtarget, int reg, int dtarget) { ... }
fn patchlistaux(
    fs: &mut FuncState,
    ls: &LexState,
    list: i32,
    vtarget: i32,
    reg: u32,
    dtarget: i32,
) -> Result<(), LuaError> {
    let mut list = list;
    while list != NO_JUMP {
        let next = getjump(fs, list);
        if patchtestreg(fs, list, reg) {
            fixjump(fs, ls, list, vtarget)?;
        } else {
            fixjump(fs, ls, list, dtarget)?;
        }
        list = next;
    }
    Ok(())
}

// C: void luaK_patchlist (FuncState *fs, int list, int target) { ... }
pub(crate) fn patchlist(
    fs: &mut FuncState,
    ls: &LexState,
    list: i32,
    target: i32,
) -> Result<(), LuaError> {
    debug_assert!(target <= fs.pc);
    patchlistaux(fs, ls, list, target, NO_REG, target)
}

// C: void luaK_patchtohere (FuncState *fs, int list) { ... }
pub(crate) fn patchtohere(
    fs: &mut FuncState,
    ls: &LexState,
    list: i32,
) -> Result<(), LuaError> {
    let hr = getlabel(fs);
    patchlist(fs, ls, list, hr)
}

// ─── Line information ─────────────────────────────────────────────────────────

// C: static void savelineinfo (FuncState *fs, Proto *f, int line) { ... }
fn savelineinfo(
    fs: &mut FuncState,
    state: &mut LuaState,
    line: i32,
) -> Result<(), LuaError> {
    let linedif_raw = line - fs.previousline;
    let pc = fs.pc - 1; // last instruction coded
    let need_abs = linedif_raw.abs() >= LIM_LINE_DIFF || {
        let over = fs.iwthabs as i32 >= MAX_IWTH_ABS;
        if !over { fs.iwthabs += 1; }
        over
    };
    let linedif: i8;
    if need_abs {
        // C: luaM_growvector(fs->ls->L, f->abslineinfo, fs->nabslineinfo, ...)
        // PERF(port): reserve_or_grow call omitted; Vec::push handles growth
        fs.f.abslineinfo.push(AbsLineInfo { pc, line });
        fs.nabslineinfo += 1;
        linedif = ABS_LINE_INFO;
        fs.iwthabs = 1;
    } else {
        linedif = linedif_raw as i8;
    }
    // C: luaM_growvector(fs->ls->L, f->lineinfo, pc, f->sizelineinfo, ...)
    // PERF(port): Vec::push / extend handles growth
    if fs.f.lineinfo.len() <= pc as usize {
        fs.f.lineinfo.resize(pc as usize + 1, 0i8);
    }
    fs.f.lineinfo[pc as usize] = linedif;
    fs.previousline = line;
    Ok(())
}

// C: static void removelastlineinfo (FuncState *fs) { ... }
fn removelastlineinfo(fs: &mut FuncState) {
    let pc = (fs.pc - 1) as usize;
    if fs.f.lineinfo[pc] != ABS_LINE_INFO {
        fs.previousline -= fs.f.lineinfo[pc] as i32;
        fs.iwthabs -= 1;
    } else {
        debug_assert_eq!(
            fs.f.abslineinfo[fs.nabslineinfo as usize - 1].pc,
            pc as i32
        );
        fs.nabslineinfo -= 1;
        // C: fs->iwthabs = MAXIWTHABS + 1; /* force next to be absolute */
        fs.iwthabs = (MAX_IWTH_ABS + 1) as u8;
    }
}

// C: static void removelastinstruction (FuncState *fs) { ... }
fn removelastinstruction(fs: &mut FuncState) {
    removelastlineinfo(fs);
    fs.pc -= 1;
}

// ─── Core instruction emission ────────────────────────────────────────────────

// C: int luaK_code (FuncState *fs, Instruction i) { ... }
/// Emit instruction `i`, grow code array as needed, save line info.
/// Returns the pc (index) of the new instruction.
pub(crate) fn code(
    fs: &mut FuncState,
    ls: &mut LexState,
    state: &mut LuaState,
    i: Instruction,
) -> Result<i32, LuaError> {
    // C: luaM_growvector(fs->ls->L, f->code, fs->pc, f->sizecode, ...)
    // PERF(port): Vec::push handles growth
    if fs.f.code.len() <= fs.pc as usize {
        fs.f.code.resize(fs.pc as usize + 1, Instruction::default());
    }
    fs.f.code[fs.pc as usize] = i;
    fs.pc += 1;
    // C: savelineinfo(fs, f, fs->ls->lastline);
    savelineinfo(fs, state, ls.lastline)?;
    Ok(fs.pc - 1)
}

// C: int luaK_codeABCk (FuncState *fs, OpCode o, int a, int b, int c, int k) { ... }
pub(crate) fn code_abck(
    fs: &mut FuncState,
    ls: &mut LexState,
    o: OpCode,
    a: u32,
    b: u32,
    c: u32,
    k: u32,
) -> Result<i32, LuaError> {
    // TODO(port): getOpMode assertion requires luaP_opmodes lookup
    debug_assert!(a <= MAXARG_A && b <= MAXARG_B && c <= MAXARG_C && (k & !1) == 0);
    // C: luaK_code(fs, CREATE_ABCk(o, a, b, c, k))
    // TODO(port): state needs to be threaded from caller; using dummy for now
    // TODO(port): code() needs &mut LuaState for savelineinfo/growvector
    let instr = Instruction::abck(o, a, b, c, k);
    // PORT NOTE: state is not available at this call site in all callers;
    // this is a known Phase B issue.  Callers that have state pass it in;
    // others will need refactoring.
    // For Phase A we call a version that skips growth:
    code_no_state(fs, ls, instr)
}

// C: int luaK_codeABx (FuncState *fs, OpCode o, int a, unsigned int bc) { ... }
pub(crate) fn code_abx(
    fs: &mut FuncState,
    ls: &mut LexState,
    o: OpCode,
    a: u32,
    bc: u32,
) -> Result<i32, LuaError> {
    debug_assert!(a <= MAXARG_A && bc <= MAXARG_BX);
    code_no_state(fs, ls, Instruction::abx(o, a, bc))
}

// C: static int codeAsBx (FuncState *fs, OpCode o, int a, int bc) { ... }
fn code_asbx(
    fs: &mut FuncState,
    ls: &mut LexState,
    o: OpCode,
    a: u32,
    bc: i32,
) -> Result<i32, LuaError> {
    let b = (bc + OFFSET_S_BX) as u32;
    debug_assert!(a <= MAXARG_A && b <= MAXARG_BX);
    code_no_state(fs, ls, Instruction::abx(o, a, b))
}

// C: static int codesJ (FuncState *fs, OpCode o, int sj, int k) { ... }
fn codes_j(
    fs: &mut FuncState,
    ls: &mut LexState,
    o: OpCode,
    sj: i32,
    k: u32,
) -> Result<i32, LuaError> {
    let j = (sj + OFFSET_S_J) as u32;
    debug_assert!(j <= MAXARG_S_J && (k & !1) == 0);
    code_no_state(fs, ls, Instruction::sj(o, j, k))
}

// C: static int codeextraarg (FuncState *fs, int a) { ... }
fn codeextraarg(
    fs: &mut FuncState,
    ls: &mut LexState,
    a: u32,
) -> Result<i32, LuaError> {
    debug_assert!(a <= MAXARG_AX);
    code_no_state(fs, ls, Instruction::ax(OpCode::ExtraArg, a))
}

// PORT NOTE: `code_no_state` is a Phase A shim — it records the instruction
// and bumps pc but skips the Vec growth check that requires &mut LuaState.
// Phase B replaces all call sites with `code(fs, ls, state, instr)`.
fn code_no_state(
    fs: &mut FuncState,
    ls: &LexState,
    i: Instruction,
) -> Result<i32, LuaError> {
    // PERF(port): In production, pre-allocate via reserve_or_grow; here we
    // rely on Vec's amortised growth.
    if fs.f.code.len() <= fs.pc as usize {
        fs.f.code.resize(fs.pc as usize + 1, Instruction::default());
    }
    fs.f.code[fs.pc as usize] = i;
    fs.pc += 1;
    // TODO(port): savelineinfo needs &mut LuaState for abslineinfo growth;
    // for now we do a simplified version that only handles relative deltas.
    let linedif_raw = ls.lastline - fs.previousline;
    let pc = (fs.pc - 1) as usize;
    if fs.f.lineinfo.len() <= pc {
        fs.f.lineinfo.resize(pc + 1, 0i8);
    }
    if linedif_raw.abs() < LIM_LINE_DIFF && (fs.iwthabs as i32) < MAX_IWTH_ABS {
        fs.f.lineinfo[pc] = linedif_raw as i8;
        fs.iwthabs += 1;
    } else {
        fs.f.lineinfo[pc] = ABS_LINE_INFO;
        fs.f.abslineinfo.push(AbsLineInfo { pc: pc as i32, line: ls.lastline });
        fs.nabslineinfo += 1;
        fs.iwthabs = 1;
    }
    fs.previousline = ls.lastline;
    Ok(fs.pc - 1)
}

// Thin wrapper — `luaK_codeABC` macro translates to this (k=0).
fn code_abc(
    fs: &mut FuncState,
    ls: &mut LexState,
    o: OpCode,
    a: u32,
    b: u32,
    c: u32,
) -> Result<i32, LuaError> {
    code_abck(fs, ls, o, a, b, c, 0)
}

// C: static int luaK_codek (FuncState *fs, int reg, int k) { ... }
fn codek(
    fs: &mut FuncState,
    ls: &mut LexState,
    reg: i32,
    k: i32,
) -> Result<i32, LuaError> {
    if k as u32 <= MAXARG_BX {
        code_abx(fs, ls, OpCode::LoadK, reg as u32, k as u32)
    } else {
        let p = code_abx(fs, ls, OpCode::LoadKX, reg as u32, 0)?;
        codeextraarg(fs, ls, k as u32)?;
        Ok(p)
    }
}

// ─── Stack-size bookkeeping ───────────────────────────────────────────────────

// C: void luaK_checkstack (FuncState *fs, int n) { ... }
pub(crate) fn checkstack(
    fs: &mut FuncState,
    ls: &LexState,
    n: i32,
) -> Result<(), LuaError> {
    let newstack = fs.freereg as i32 + n;
    if newstack > fs.f.maxstacksize as i32 {
        if newstack >= MAXREGS {
            return Err(LuaError::syntax(format_args!(
                "function or expression needs too many registers"
            )));
        }
        fs.f.maxstacksize = newstack as u8;
    }
    Ok(())
}

// C: void luaK_reserveregs (FuncState *fs, int n) { ... }
pub(crate) fn reserveregs(
    fs: &mut FuncState,
    ls: &LexState,
    n: i32,
) -> Result<(), LuaError> {
    checkstack(fs, ls, n)?;
    fs.freereg = (fs.freereg as i32 + n) as u8;
    Ok(())
}

// ─── Register freeing ─────────────────────────────────────────────────────────

// C: static void freereg (FuncState *fs, int reg) { ... }
// `nvarstack` is the number of locals on the register stack (luaY_nvarstack).
fn freereg(fs: &mut FuncState, reg: i32, nvarstack: i32) {
    if reg >= nvarstack {
        fs.freereg -= 1;
        debug_assert_eq!(reg, fs.freereg as i32);
    }
}

// C: static void freeregs (FuncState *fs, int r1, int r2) { ... }
fn freeregs(fs: &mut FuncState, r1: i32, r2: i32, nvarstack: i32) {
    if r1 > r2 {
        freereg(fs, r1, nvarstack);
        freereg(fs, r2, nvarstack);
    } else {
        freereg(fs, r2, nvarstack);
        freereg(fs, r1, nvarstack);
    }
}

// C: static void freeexp (FuncState *fs, expdesc *e) { ... }
fn freeexp(fs: &mut FuncState, e: &ExprDesc, nvarstack: i32) {
    if e.k == ExprKind::VNonReloc {
        freereg(fs, e.u.info, nvarstack);
    }
}

// C: static void freeexps (FuncState *fs, expdesc *e1, expdesc *e2) { ... }
fn freeexps(fs: &mut FuncState, e1: &ExprDesc, e2: &ExprDesc, nvarstack: i32) {
    let r1 = if e1.k == ExprKind::VNonReloc { e1.u.info } else { -1 };
    let r2 = if e2.k == ExprKind::VNonReloc { e2.u.info } else { -1 };
    freeregs(fs, r1, r2, nvarstack);
}

// ──────────────────────────────────────────────────────────────────────────
// ─── Constant pool management ────────────────────────────────────────────────

// C: static int addk (FuncState *fs, TValue *key, TValue *v) { ... }
fn addk(
    fs: &mut FuncState,
    ls: &mut LexState,
    state: &mut LuaState,
    key: LuaValue,
    v: LuaValue,
) -> Result<i32, LuaError> {
    let idx = lex_h_get(&ls.h, state, &key);
    let mut k: i32;
    if let LuaValue::Int(ki) = idx {
        k = ki as i32;
        if k >= 0
            && k < fs.nk
            && (k as usize) < fs.f.k.len()
            && fs.f.k[k as usize].type_tag() == v.type_tag()
            && state.equal_obj(None, &fs.f.k[k as usize], &v)
        {
            return Ok(k);
        }
    }
    k = fs.nk;
    let val = LuaValue::Int(k as i64);
    lex_h_finish_set(&ls.h, state, &key, &val)?;
    while fs.f.k.len() <= k as usize {
        fs.f.k.push(LuaValue::Nil);
    }
    fs.f.k[k as usize] = v.clone();
    fs.nk += 1;
    // C: luaC_barrier(L, f, v); — no-op in Phase A-C
    Ok(k)
}

fn string_k(
    fs: &mut FuncState,
    ls: &mut LexState,
    state: &mut LuaState,
    s: GcRef<LuaString>,
) -> Result<i32, LuaError> {
    let o = LuaValue::Str(s);
    addk(fs, ls, state, o.clone(), o)
}

fn int_k(
    fs: &mut FuncState,
    ls: &mut LexState,
    state: &mut LuaState,
    n: i64,
) -> Result<i32, LuaError> {
    let o = LuaValue::Int(n);
    addk(fs, ls, state, o.clone(), o)
}

// C: static int luaK_numberK (FuncState *fs, lua_Number r) { ... }
fn number_k(
    fs: &mut FuncState,
    ls: &mut LexState,
    state: &mut LuaState,
    r: f64,
) -> Result<i32, LuaError> {
    let o = LuaValue::Float(r);
    // TODO(port): luaV_flttointeger — check if r has integer value exactly
    let ik_opt: Option<i64> = if r.fract() == 0.0 && r.abs() < i64::MAX as f64 {
        Some(r as i64)
    } else {
        None
    };
    if ik_opt.is_none() {
        return addk(fs, ls, state, o.clone(), o);
    }
    let ik = ik_opt.unwrap();
    // build alternate key: r + r * 2^(-52) so floats don't collide with ints
    // C: const int nbm = l_floatatt(MANT_DIG); /* 53 for f64 */
    // C: const lua_Number q = ldexp(1.0, -nbm + 1);
    let q = f64::EPSILON; // 2^(-52)
    let alt_key = if ik == 0 { q } else { r + r * q };
    let kv = LuaValue::Float(alt_key);
    addk(fs, ls, state, kv, o)
}

fn bool_f(
    fs: &mut FuncState,
    ls: &mut LexState,
    state: &mut LuaState,
) -> Result<i32, LuaError> {
    let o = LuaValue::Bool(false);
    addk(fs, ls, state, o.clone(), o)
}

fn bool_t(
    fs: &mut FuncState,
    ls: &mut LexState,
    state: &mut LuaState,
) -> Result<i32, LuaError> {
    let o = LuaValue::Bool(true);
    addk(fs, ls, state, o.clone(), o)
}

fn nil_k(
    fs: &mut FuncState,
    ls: &mut LexState,
    state: &mut LuaState,
) -> Result<i32, LuaError> {
    let v = LuaValue::Nil;
    // C: sethvalue(fs->ls->L, &k, fs->ls->h); — use the scanner table as key for nil
    let k = LuaValue::Table(ls.h.clone());
    addk(fs, ls, state, k, v)
}

// ─── Signed-range helpers ─────────────────────────────────────────────────────

#[inline]
fn fits_c(i: i64) -> bool {
    (i as u64).wrapping_add(OFFSET_S_C as u64) <= MAXARG_C as u64
}

#[inline]
fn fits_bx(i: i64) -> bool {
    (-OFFSET_S_BX as i64) <= i && i <= (MAXARG_BX as i64 - OFFSET_S_BX as i64)
}

// ─── Integer/float literal emission ──────────────────────────────────────────

// C: void luaK_int (FuncState *fs, int reg, lua_Integer i) { ... }
pub(crate) fn int_const(
    fs: &mut FuncState,
    ls: &mut LexState,
    state: &mut LuaState,
    reg: i32,
    i: i64,
) -> Result<(), LuaError> {
    if fits_bx(i) {
        code_asbx(fs, ls, OpCode::LoadI, reg as u32, i as i32)?;
    } else {
        let k = int_k(fs, ls, state, i)?;
        codek(fs, ls, reg, k)?;
    }
    Ok(())
}

// C: static void luaK_float (FuncState *fs, int reg, lua_Number f) { ... }
fn float_const(
    fs: &mut FuncState,
    ls: &mut LexState,
    state: &mut LuaState,
    reg: i32,
    f: f64,
) -> Result<(), LuaError> {
    // TODO(port): luaV_flttointeger — simplified
    let fi_opt: Option<i64> = if f.fract() == 0.0 && f.abs() < i64::MAX as f64 {
        Some(f as i64)
    } else {
        None
    };
    if let Some(fi) = fi_opt {
        if fits_bx(fi) {
            code_asbx(fs, ls, OpCode::LoadF, reg as u32, fi as i32)?;
            return Ok(());
        }
    }
    let k = number_k(fs, ls, state, f)?;
    codek(fs, ls, reg, k)?;
    Ok(())
}

// ─── const2exp ────────────────────────────────────────────────────────────────

fn const2exp(v: &LuaValue, e: &mut ExprDesc) {
    match v {
        LuaValue::Int(i)         => { e.k = ExprKind::VKInt; e.u.ival = *i; }
        LuaValue::Float(n)       => { e.k = ExprKind::VKFlt; e.u.nval = *n; }
        LuaValue::Bool(false)    => { e.k = ExprKind::VFalse; }
        LuaValue::Bool(true)     => { e.k = ExprKind::VTrue; }
        LuaValue::Nil            => { e.k = ExprKind::VNil; }
        LuaValue::Str(s)         => { e.k = ExprKind::VKStr; e.u.strval = s.clone(); }
        _  => { debug_assert!(false, "const2exp: unexpected value"); }
    }
}

// ─── Multi-return / single-return fixups ─────────────────────────────────────

pub(crate) fn setreturns(fs: &mut FuncState, e: &mut ExprDesc, nresults: i32) {
    let pc_idx = e.u.info as usize;
    if e.k == ExprKind::VCall {
        fs.f.code[pc_idx].set_arg_c((nresults + 1) as u32);
    } else {
        debug_assert_eq!(e.k, ExprKind::VVarArg);
        fs.f.code[pc_idx].set_arg_c((nresults + 1) as u32);
        fs.f.code[pc_idx].set_arg_a(fs.freereg as u32);
        fs.freereg += 1;
    }
}

fn str2_k(
    fs: &mut FuncState,
    ls: &mut LexState,
    state: &mut LuaState,
    e: &mut ExprDesc,
) -> Result<(), LuaError> {
    debug_assert_eq!(e.k, ExprKind::VKStr);
    e.u.info = string_k(fs, ls, state, e.u.strval.clone())?;
    e.k = ExprKind::VK;
    Ok(())
}

pub(crate) fn setoneret(fs: &mut FuncState, e: &mut ExprDesc) {
    if e.k == ExprKind::VCall {
        debug_assert_eq!(fs.f.code[e.u.info as usize].arg_c(), 2);
        e.k = ExprKind::VNonReloc;
        e.u.info = fs.f.code[e.u.info as usize].arg_a() as i32;
    } else if e.k == ExprKind::VVarArg {
        fs.f.code[e.u.info as usize].set_arg_c(2);
        e.k = ExprKind::VReloc;
    }
}

// ─── Discharge ────────────────────────────────────────────────────────────────

pub(crate) fn dischargevars(
    fs: &mut FuncState,
    ls: &mut LexState,
    state: &mut LuaState,
    e: &mut ExprDesc,
) -> Result<(), LuaError> {
    match e.k {
        ExprKind::VConst => {
            let v = const2val(fs, ls, e);
            const2exp(&v, e);
        }
        ExprKind::VLocal => {
            let r = e.u.var.ridx as i32;
            e.u.info = r;
            e.k = ExprKind::VNonReloc;
        }
        ExprKind::VUpVal => {
            let info = e.u.info as u32;
            e.u.info = code_abc(fs, ls, OpCode::GetUpVal, 0, info, 0)?;
            e.k = ExprKind::VReloc;
        }
        ExprKind::VIndexUp => {
            let t = e.u.ind.t as u32;
            let idx = e.u.ind.idx as u32;
            e.u.info = code_abc(fs, ls, OpCode::GetTabUp, 0, t, idx)?;
            e.k = ExprKind::VReloc;
        }
        ExprKind::VIndexI => {
            let nv = fs.nactvar as i32;
            freereg(fs, e.u.ind.t as i32, nv);
            let t = e.u.ind.t as u32;
            let idx = e.u.ind.idx as u32;
            e.u.info = code_abc(fs, ls, OpCode::GetI, 0, t, idx)?;
            e.k = ExprKind::VReloc;
        }
        ExprKind::VIndexStr => {
            let nv = fs.nactvar as i32;
            freereg(fs, e.u.ind.t as i32, nv);
            let t = e.u.ind.t as u32;
            let idx = e.u.ind.idx as u32;
            e.u.info = code_abc(fs, ls, OpCode::GetField, 0, t, idx)?;
            e.k = ExprKind::VReloc;
        }
        ExprKind::VIndexed => {
            let nv = fs.nactvar as i32;
            freeregs(fs, e.u.ind.t as i32, e.u.ind.idx as i32, nv);
            let t = e.u.ind.t as u32;
            let idx = e.u.ind.idx as u32;
            e.u.info = code_abc(fs, ls, OpCode::GetTable, 0, t, idx)?;
            e.k = ExprKind::VReloc;
        }
        ExprKind::VVarArg | ExprKind::VCall => { setoneret(fs, e); }
        _ => {}
    }
    Ok(())
}

fn discharge2reg(
    fs: &mut FuncState,
    ls: &mut LexState,
    state: &mut LuaState,
    e: &mut ExprDesc,
    reg: i32,
) -> Result<(), LuaError> {
    dischargevars(fs, ls, state, e)?;
    match e.k {
        ExprKind::VNil   => { nil(fs, ls, reg, 1)?; }
        ExprKind::VFalse => { code_abc(fs, ls, OpCode::LoadFalse, reg as u32, 0, 0)?; }
        ExprKind::VTrue  => { code_abc(fs, ls, OpCode::LoadTrue, reg as u32, 0, 0)?; }
        ExprKind::VKStr  => {
            // PORT NOTE: str2_k then fall through to VK
            str2_k(fs, ls, state, e)?;
            codek(fs, ls, reg, e.u.info)?;
        }
        ExprKind::VK    => { codek(fs, ls, reg, e.u.info)?; }
        ExprKind::VKFlt => { float_const(fs, ls, state, reg, e.u.nval)?; }
        ExprKind::VKInt => { int_const(fs, ls, state, reg, e.u.ival)?; }
        ExprKind::VReloc => {
            fs.f.code[e.u.info as usize].set_arg_a(reg as u32);
        }
        ExprKind::VNonReloc => {
            if reg != e.u.info {
                code_abc(fs, ls, OpCode::Move, reg as u32, e.u.info as u32, 0)?;
            }
        }
        _ => {
            debug_assert_eq!(e.k, ExprKind::VJmp);
            return Ok(());
        }
    }
    e.u.info = reg;
    e.k = ExprKind::VNonReloc;
    Ok(())
}

fn discharge2anyreg(
    fs: &mut FuncState,
    ls: &mut LexState,
    state: &mut LuaState,
    e: &mut ExprDesc,
) -> Result<(), LuaError> {
    if e.k != ExprKind::VNonReloc {
        reserveregs(fs, ls, 1)?;
        let r = fs.freereg as i32 - 1;
        discharge2reg(fs, ls, state, e, r)?;
    }
    Ok(())
}

fn code_loadbool(
    fs: &mut FuncState,
    ls: &mut LexState,
    a: i32,
    op: OpCode,
) -> Result<i32, LuaError> {
    getlabel(fs);
    code_abc(fs, ls, op, a as u32, 0, 0)
}

fn need_value(fs: &FuncState, list: i32) -> bool {
    let mut list = list;
    while list != NO_JUMP {
        let ctrl = getjumpcontrol(fs, list);
        if fs.f.code[ctrl].opcode() != Some(OpCode::TestSet) {
            return true;
        }
        list = getjump(fs, list);
    }
    false
}

fn exp2reg(
    fs: &mut FuncState,
    ls: &mut LexState,
    state: &mut LuaState,
    e: &mut ExprDesc,
    reg: i32,
) -> Result<(), LuaError> {
    discharge2reg(fs, ls, state, e, reg)?;
    if e.k == ExprKind::VJmp {
        let info = e.u.info;
        concat(fs, ls, &mut e.t, info)?;
    }
    if e.t != e.f {
        let mut p_f = NO_JUMP;
        let mut p_t = NO_JUMP;
        if need_value(fs, e.t) || need_value(fs, e.f) {
            let fj = if e.k == ExprKind::VJmp { NO_JUMP } else { jump(fs, ls)? };
            p_f = code_loadbool(fs, ls, reg, OpCode::LFalseSkip)?;
            p_t = code_loadbool(fs, ls, reg, OpCode::LoadTrue)?;
            patchtohere(fs, ls, fj)?;
        }
        let final_pc = getlabel(fs);
        patchlistaux(fs, ls, e.f, final_pc, reg as u32, p_f)?;
        patchlistaux(fs, ls, e.t, final_pc, reg as u32, p_t)?;
    }
    e.f = NO_JUMP;
    e.t = NO_JUMP;
    e.u.info = reg;
    e.k = ExprKind::VNonReloc;
    Ok(())
}

pub(crate) fn exp2nextreg(
    fs: &mut FuncState,
    ls: &mut LexState,
    state: &mut LuaState,
    e: &mut ExprDesc,
) -> Result<(), LuaError> {
    let nv = fs.nactvar as i32;
    dischargevars(fs, ls, state, e)?;
    freeexp(fs, e, nv);
    reserveregs(fs, ls, 1)?;
    let r = fs.freereg as i32 - 1;
    exp2reg(fs, ls, state, e, r)
}

pub(crate) fn exp2anyreg(
    fs: &mut FuncState,
    ls: &mut LexState,
    state: &mut LuaState,
    e: &mut ExprDesc,
) -> Result<i32, LuaError> {
    dischargevars(fs, ls, state, e)?;
    if e.k == ExprKind::VNonReloc {
        if e.t == e.f { return Ok(e.u.info); }
        if e.u.info >= fs.nactvar as i32 {
            let reg = e.u.info;
            exp2reg(fs, ls, state, e, reg)?;
            return Ok(e.u.info);
        }
    }
    exp2nextreg(fs, ls, state, e)?;
    Ok(e.u.info)
}

pub(crate) fn exp2anyregup(
    fs: &mut FuncState,
    ls: &mut LexState,
    state: &mut LuaState,
    e: &mut ExprDesc,
) -> Result<(), LuaError> {
    if e.k != ExprKind::VUpVal || e.t != e.f {
        exp2anyreg(fs, ls, state, e)?;
    }
    Ok(())
}

pub(crate) fn exp2val(
    fs: &mut FuncState,
    ls: &mut LexState,
    state: &mut LuaState,
    e: &mut ExprDesc,
) -> Result<(), LuaError> {
    if e.t != e.f { exp2anyreg(fs, ls, state, e)?; }
    else { dischargevars(fs, ls, state, e)?; }
    Ok(())
}

fn exp2_k(
    fs: &mut FuncState,
    ls: &mut LexState,
    state: &mut LuaState,
    e: &mut ExprDesc,
) -> Result<bool, LuaError> {
    if e.t != e.f { return Ok(false); }
    let info: i32 = match e.k {
        ExprKind::VTrue  => bool_t(fs, ls, state)?,
        ExprKind::VFalse => bool_f(fs, ls, state)?,
        ExprKind::VNil   => nil_k(fs, ls, state)?,
        ExprKind::VKInt  => int_k(fs, ls, state, e.u.ival)?,
        ExprKind::VKFlt  => number_k(fs, ls, state, e.u.nval)?,
        ExprKind::VKStr  => string_k(fs, ls, state, e.u.strval.clone())?,
        ExprKind::VK     => e.u.info,
        _                => return Ok(false),
    };
    if info as u32 <= MAXINDEXRK {
        e.k = ExprKind::VK;
        e.u.info = info;
        return Ok(true);
    }
    Ok(false)
}

fn exp2_rk(
    fs: &mut FuncState,
    ls: &mut LexState,
    state: &mut LuaState,
    e: &mut ExprDesc,
) -> Result<bool, LuaError> {
    if exp2_k(fs, ls, state, e)? { return Ok(true); }
    exp2anyreg(fs, ls, state, e)?;
    Ok(false)
}

fn code_abrk(
    fs: &mut FuncState,
    ls: &mut LexState,
    state: &mut LuaState,
    o: OpCode,
    a: u32,
    b: u32,
    ec: &mut ExprDesc,
) -> Result<(), LuaError> {
    let k = if exp2_rk(fs, ls, state, ec)? { 1u32 } else { 0u32 };
    code_abck(fs, ls, o, a, b, ec.u.info as u32, k)?;
    Ok(())
}

pub(crate) fn storevar(
    fs: &mut FuncState,
    ls: &mut LexState,
    state: &mut LuaState,
    var: &ExprDesc,
    ex: &mut ExprDesc,
) -> Result<(), LuaError> {
    let nv = fs.nactvar as i32;
    match var.k {
        ExprKind::VLocal => {
            freeexp(fs, ex, nv);
            exp2reg(fs, ls, state, ex, var.u.var.ridx as i32)?;
            return Ok(());
        }
        ExprKind::VUpVal => {
            let e = exp2anyreg(fs, ls, state, ex)?;
            code_abc(fs, ls, OpCode::SetUpVal, e as u32, var.u.info as u32, 0)?;
        }
        ExprKind::VIndexUp => {
            let mut ex = ex.clone();
            code_abrk(fs, ls, state, OpCode::SetTabUp,
                var.u.ind.t as u32, var.u.ind.idx as u32, &mut ex)?;
        }
        ExprKind::VIndexI => {
            let mut ex = ex.clone();
            code_abrk(fs, ls, state, OpCode::SetI,
                var.u.ind.t as u32, var.u.ind.idx as u32, &mut ex)?;
        }
        ExprKind::VIndexStr => {
            let mut ex = ex.clone();
            code_abrk(fs, ls, state, OpCode::SetField,
                var.u.ind.t as u32, var.u.ind.idx as u32, &mut ex)?;
        }
        ExprKind::VIndexed => {
            let mut ex = ex.clone();
            code_abrk(fs, ls, state, OpCode::SetTable,
                var.u.ind.t as u32, var.u.ind.idx as u32, &mut ex)?;
        }
        _ => { debug_assert!(false, "storevar: invalid var kind"); }
    }
    freeexp(fs, ex, nv);
    Ok(())
}

pub(crate) fn self_(
    fs: &mut FuncState,
    ls: &mut LexState,
    state: &mut LuaState,
    e: &mut ExprDesc,
    key: &mut ExprDesc,
) -> Result<(), LuaError> {
    let nv = fs.nactvar as i32;
    exp2anyreg(fs, ls, state, e)?;
    let ereg = e.u.info;
    freeexp(fs, e, nv);
    e.u.info = fs.freereg as i32;
    e.k = ExprKind::VNonReloc;
    reserveregs(fs, ls, 2)?;
    code_abrk(fs, ls, state, OpCode::Self_, e.u.info as u32, ereg as u32, key)?;
    freeexp(fs, key, nv);
    Ok(())
}

fn negatecondition(fs: &mut FuncState, e: &ExprDesc) {
    let ctrl = getjumpcontrol(fs, e.u.info);
    let k = fs.f.code[ctrl].arg_k() ^ 1;
    fs.f.code[ctrl].set_arg_k(k);
}

fn jumponcond(
    fs: &mut FuncState,
    ls: &mut LexState,
    state: &mut LuaState,
    e: &mut ExprDesc,
    cond: bool,
) -> Result<i32, LuaError> {
    if e.k == ExprKind::VReloc {
        let ie = fs.f.code[e.u.info as usize];
        if ie.opcode() == Some(OpCode::Not) {
            removelastinstruction(fs);
            let b = ie.arg_b();
            return condjump(fs, ls, OpCode::Test, b, 0, 0, !cond as u32);
        }
    }
    let nv = fs.nactvar as i32;
    discharge2anyreg(fs, ls, state, e)?;
    freeexp(fs, e, nv);
    condjump(fs, ls, OpCode::TestSet, NO_REG, e.u.info as u32, 0, cond as u32)
}

pub(crate) fn goiftrue(
    fs: &mut FuncState,
    ls: &mut LexState,
    state: &mut LuaState,
    e: &mut ExprDesc,
) -> Result<(), LuaError> {
    dischargevars(fs, ls, state, e)?;
    let pc: i32 = match e.k {
        ExprKind::VJmp => { negatecondition(fs, e); e.u.info }
        ExprKind::VK | ExprKind::VKFlt | ExprKind::VKInt | ExprKind::VKStr | ExprKind::VTrue => NO_JUMP,
        _ => jumponcond(fs, ls, state, e, false)?,
    };
    concat(fs, ls, &mut e.f, pc)?;
    patchtohere(fs, ls, e.t)?;
    e.t = NO_JUMP;
    Ok(())
}

pub(crate) fn goiffalse(
    fs: &mut FuncState,
    ls: &mut LexState,
    state: &mut LuaState,
    e: &mut ExprDesc,
) -> Result<(), LuaError> {
    dischargevars(fs, ls, state, e)?;
    let pc: i32 = match e.k {
        ExprKind::VJmp => e.u.info,
        ExprKind::VNil | ExprKind::VFalse => NO_JUMP,
        _ => jumponcond(fs, ls, state, e, true)?,
    };
    concat(fs, ls, &mut e.t, pc)?;
    patchtohere(fs, ls, e.f)?;
    e.f = NO_JUMP;
    Ok(())
}

fn codenot(
    fs: &mut FuncState,
    ls: &mut LexState,
    state: &mut LuaState,
    e: &mut ExprDesc,
) -> Result<(), LuaError> {
    match e.k {
        ExprKind::VNil | ExprKind::VFalse => { e.k = ExprKind::VTrue; }
        ExprKind::VK | ExprKind::VKFlt | ExprKind::VKInt | ExprKind::VKStr | ExprKind::VTrue => {
            e.k = ExprKind::VFalse;
        }
        ExprKind::VJmp => { negatecondition(fs, e); }
        ExprKind::VReloc | ExprKind::VNonReloc => {
            let nv = fs.nactvar as i32;
            discharge2anyreg(fs, ls, state, e)?;
            freeexp(fs, e, nv);
            e.u.info = code_abc(fs, ls, OpCode::Not, 0, e.u.info as u32, 0)?;
            e.k = ExprKind::VReloc;
        }
        _ => { debug_assert!(false, "codenot: cannot happen"); }
    }
    let temp = e.f; e.f = e.t; e.t = temp;
    removevalues(fs, e.f);
    removevalues(fs, e.t);
    Ok(())
}

// ─── Predicate helpers ────────────────────────────────────────────────────────

// C: static int isKstr (FuncState *fs, expdesc *e) { ... }
fn is_kstr(fs: &FuncState, e: &ExprDesc) -> bool {
    e.k == ExprKind::VK
        && e.t == e.f
        && (e.u.info as u32) <= MAXARG_B
        && matches!(fs.f.k[e.u.info as usize], LuaValue::Str(ref s) if s.is_short())
}

// C: static int isKint (expdesc *e) { ... }
fn is_kint(e: &ExprDesc) -> bool {
    e.k == ExprKind::VKInt && e.t == e.f
}

// C: static int isCint (expdesc *e) { ... }
fn is_cint(e: &ExprDesc) -> bool {
    is_kint(e) && (e.u.ival as u64) <= (MAXARG_C as u64)
}

// C: static int isSCint (expdesc *e) { ... }
fn is_scint(e: &ExprDesc) -> bool {
    is_kint(e) && fits_c(e.u.ival)
}

// C: static int isSCnumber (expdesc *e, int *pi, int *isfloat) { ... }
fn is_scnumber(e: &ExprDesc, pi: &mut i32, isfloat: &mut bool) -> bool {
    let i: i64;
    if e.k == ExprKind::VKInt {
        i = e.u.ival;
    } else if e.k == ExprKind::VKFlt {
        // TODO(port): luaV_flttointeger
        if e.u.nval.fract() == 0.0 && e.u.nval.abs() < i64::MAX as f64 {
            i = e.u.nval as i64;
            *isfloat = true;
        } else {
            return false;
        }
    } else {
        return false;
    }
    if e.t == e.f && fits_c(i) {
        // C: *pi = int2sC(cast_int(i))
        *pi = (i as i32) + OFFSET_S_C;
        true
    } else {
        false
    }
}

// ─── Indexed expression ───────────────────────────────────────────────────────

// C: void luaK_indexed (FuncState *fs, expdesc *t, expdesc *k) { ... }
pub(crate) fn indexed(
    fs: &mut FuncState,
    ls: &mut LexState,
    state: &mut LuaState,
    t: &mut ExprDesc,
    k: &mut ExprDesc,
) -> Result<(), LuaError> {
    if k.k == ExprKind::VKStr {
        str2_k(fs, ls, state, k)?;
    }
    debug_assert!(
        t.t == t.f
            && (t.k == ExprKind::VLocal
                || t.k == ExprKind::VNonReloc
                || t.k == ExprKind::VUpVal)
    );
    if t.k == ExprKind::VUpVal && !is_kstr(fs, k) {
        exp2anyreg(fs, ls, state, t)?;
    }
    if t.k == ExprKind::VUpVal {
        let temp = t.u.info;
        debug_assert!(is_kstr(fs, k));
        t.u.ind.t = temp as u8;
        t.u.ind.idx = k.u.info as i16;
        t.k = ExprKind::VIndexUp;
    } else {
        t.u.ind.t = if t.k == ExprKind::VLocal {
            t.u.var.ridx
        } else {
            t.u.info as u8
        };
        if is_kstr(fs, k) {
            t.u.ind.idx = k.u.info as i16;
            t.k = ExprKind::VIndexStr;
        } else if is_cint(k) {
            t.u.ind.idx = k.u.ival as i16;
            t.k = ExprKind::VIndexI;
        } else {
            t.u.ind.idx = exp2anyreg(fs, ls, state, k)? as i16;
            t.k = ExprKind::VIndexed;
        }
    }
    Ok(())
}

// ─── Constant folding ─────────────────────────────────────────────────────────

// C: static int validop (int op, TValue *v1, TValue *v2) { ... }
fn validop(op: i32, v1: &LuaValue, v2: &LuaValue) -> bool {
    // C: LUA_OPBAND..LUA_OPSHR require integer conversion
    // C: LUA_OPDIV / LUA_OPIDIV / LUA_OPMOD forbid zero divisor
    // TODO(port): use ArithOp enum once defined in lua-types
    match op {
        7 | 8 | 9 | 10 | 11 | 14 => {
            // bitwise ops: both operands must be convertible to integer
            // TODO(port): luaV_tointegerns
            matches!(v1, LuaValue::Int(_)) && matches!(v2, LuaValue::Int(_))
        }
        5 | 6 | 3 => {
            // LUA_OPDIV, LUA_OPIDIV, LUA_OPMOD: no zero divisor
            match v2 {
                LuaValue::Int(0) => false,
                LuaValue::Float(f) if *f == 0.0 => false,
                _ => true,
            }
        }
        _ => true,
    }
}

// C: static int constfolding (FuncState *fs, int op, expdesc *e1, const expdesc *e2) { ... }
fn constfolding(
    fs: &FuncState,
    ls: &LexState,
    state: &mut LuaState,
    op: i32,
    e1: &mut ExprDesc,
    e2: &ExprDesc,
) -> Result<bool, LuaError> {
    let mut v1 = LuaValue::Nil;
    let mut v2 = LuaValue::Nil;
    if !tonumeral(e1, Some(&mut v1)) || !tonumeral(e2, Some(&mut v2)) || !validop(op, &v1, &v2) {
        return Ok(false);
    }
    // C: luaO_rawarith(fs->ls->L, op, &v1, &v2, &res);
    // TODO(port): state.raw_arith(op, v1, v2)
    let res = state.raw_arith(op, &v1, &v2)?;
    match res {
        LuaValue::Int(i) => {
            e1.k = ExprKind::VKInt;
            e1.u.ival = i;
        }
        LuaValue::Float(n) => {
            if n.is_nan() || n == 0.0 {
                return Ok(false); // folds neither NaN nor 0.0
            }
            e1.k = ExprKind::VKFlt;
            e1.u.nval = n;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

// ─── OPR → OpCode / TagMethod converters ─────────────────────────────────────

// C: l_sinline OpCode binopr2op (BinOpr opr, BinOpr baser, OpCode base) { ... }
// ORDER OPR - ORDER OP: opr and op must have the same relative ordering.
#[inline]
fn binopr2op(opr: BinOpr, baser: BinOpr, base: OpCode) -> OpCode {
    debug_assert!(opr as u8 >= baser as u8);
    let delta = (opr as u8 - baser as u8) as i32;
    // TODO(port): OpCode::from_u8 or similar cast needed; using transmute-equivalent
    // SAFETY: not applicable (no unsafe); Phase B must implement OpCode::from_i32
    opcode_from_delta(base, delta)
}

// C: l_sinline OpCode unopr2op (UnOpr opr) { ... }
#[inline]
fn unopr2op(opr: UnOpr) -> OpCode {
    // C: cast(OpCode, (cast_int(opr) - cast_int(OPR_MINUS)) + cast_int(OP_UNM))
    let delta = opr as i32 - UnOpr::Minus as i32;
    opcode_from_delta(OpCode::Unm, delta)
}

// C: l_sinline TMS binopr2TM (BinOpr opr) { ... }
#[inline]
fn binopr2_tm(opr: BinOpr) -> TagMethod {
    // C: cast(TMS, (cast_int(opr) - cast_int(OPR_ADD)) + cast_int(TM_ADD))
    let delta = opr as i32 - BinOpr::Add as i32;
    debug_assert!(delta >= 0);
    tag_method_from_delta(TagMethod::Add, delta)
}

// ─── Binary / unary code emission ────────────────────────────────────────────

// C: static void codeunexpval (FuncState *fs, OpCode op, expdesc *e, int line) { ... }
fn codeunexpval(
    fs: &mut FuncState,
    ls: &mut LexState,
    state: &mut LuaState,
    op: OpCode,
    e: &mut ExprDesc,
    line: i32,
) -> Result<(), LuaError> {
    let nv = fs.nactvar as i32;
    let r = exp2anyreg(fs, ls, state, e)?;
    freeexp(fs, e, nv);
    e.u.info = code_abc(fs, ls, op, 0, r as u32, 0)?;
    e.k = ExprKind::VReloc;
    fixline(fs, ls, line)?;
    Ok(())
}

// C: static void finishbinexpval (FuncState *fs, expdesc *e1, expdesc *e2,
//      OpCode op, int v2, int flip, int line, OpCode mmop, TMS event) { ... }
fn finishbinexpval(
    fs: &mut FuncState,
    ls: &mut LexState,
    state: &mut LuaState,
    e1: &mut ExprDesc,
    e2: &ExprDesc,
    op: OpCode,
    v2: i32,
    flip: bool,
    line: i32,
    mmop: OpCode,
    event: TagMethod,
) -> Result<(), LuaError> {
    let nv = fs.nactvar as i32;
    let v1 = exp2anyreg(fs, ls, state, e1)? as u32;
    let pc = code_abck(fs, ls, op, 0, v1, v2 as u32, 0)?;
    let e2_clone = e2.clone();
    freeexps(fs, e1, &e2_clone, nv);
    e1.u.info = pc;
    e1.k = ExprKind::VReloc;
    fixline(fs, ls, line)?;
    code_abck(fs, ls, mmop, v1, v2 as u32, event as u32, flip as u32)?;
    fixline(fs, ls, line)?;
    Ok(())
}

// C: static void codebinexpval (FuncState *fs, BinOpr opr, expdesc *e1, expdesc *e2, int line) { ... }
fn codebinexpval(
    fs: &mut FuncState,
    ls: &mut LexState,
    state: &mut LuaState,
    opr: BinOpr,
    e1: &mut ExprDesc,
    e2: &mut ExprDesc,
    line: i32,
) -> Result<(), LuaError> {
    let op = binopr2op(opr, BinOpr::Add, OpCode::Add);
    let v2 = exp2anyreg(fs, ls, state, e2)? as i32;
    let e2c = e2.clone();
    finishbinexpval(fs, ls, state, e1, &e2c, op, v2, false, line, OpCode::MmBin, binopr2_tm(opr))
}

// C: static void codebini (FuncState *fs, OpCode op, expdesc *e1, expdesc *e2, int flip, int line, TMS event) { ... }
fn codebini(
    fs: &mut FuncState,
    ls: &mut LexState,
    state: &mut LuaState,
    op: OpCode,
    e1: &mut ExprDesc,
    e2: &ExprDesc,
    flip: bool,
    line: i32,
    event: TagMethod,
) -> Result<(), LuaError> {
    // C: int v2 = int2sC(cast_int(e2->u.ival));
    debug_assert_eq!(e2.k, ExprKind::VKInt);
    let v2 = e2.u.ival as i32 + OFFSET_S_C; // int2sC
    let e2c = e2.clone();
    finishbinexpval(fs, ls, state, e1, &e2c, op, v2, flip, line, OpCode::MmBinI, event)
}

// C: static void codebinK (FuncState *fs, BinOpr opr, expdesc *e1, expdesc *e2, int flip, int line) { ... }
fn codebin_k(
    fs: &mut FuncState,
    ls: &mut LexState,
    state: &mut LuaState,
    opr: BinOpr,
    e1: &mut ExprDesc,
    e2: &ExprDesc,
    flip: bool,
    line: i32,
) -> Result<(), LuaError> {
    let event = binopr2_tm(opr);
    let v2 = e2.u.info;
    let op = binopr2op(opr, BinOpr::Add, OpCode::AddK);
    let e2c = e2.clone();
    finishbinexpval(fs, ls, state, e1, &e2c, op, v2, flip, line, OpCode::MmBinK, event)
}

// C: static int finishbinexpneg (FuncState *fs, expdesc *e1, expdesc *e2, OpCode op, int line, TMS event) { ... }
fn finishbinexpneg(
    fs: &mut FuncState,
    ls: &mut LexState,
    state: &mut LuaState,
    e1: &mut ExprDesc,
    e2: &ExprDesc,
    op: OpCode,
    line: i32,
    event: TagMethod,
) -> Result<bool, LuaError> {
    if !is_kint(e2) { return Ok(false); }
    let i2 = e2.u.ival;
    if !(fits_c(i2) && fits_c(-i2)) { return Ok(false); }
    let v2 = i2 as i32;
    let e2c = e2.clone();
    // C: finishbinexpval(fs, e1, e2, op, int2sC(-v2), 0, line, OP_MMBINI, event)
    finishbinexpval(fs, ls, state, e1, &e2c, op, (-v2) + OFFSET_S_C, false, line, OpCode::MmBinI, event)?;
    // C: SETARG_B(fs->f->code[fs->pc - 1], int2sC(v2))
    let prev_pc = (fs.pc - 1) as usize;
    fs.f.code[prev_pc].set_arg_b((v2 + OFFSET_S_C) as u32);
    Ok(true)
}

fn swapexps(e1: &mut ExprDesc, e2: &mut ExprDesc) {
    // C: expdesc temp = *e1; *e1 = *e2; *e2 = temp;
    core::mem::swap(e1, e2);
}

fn codebinno_k(
    fs: &mut FuncState,
    ls: &mut LexState,
    state: &mut LuaState,
    opr: BinOpr,
    e1: &mut ExprDesc,
    e2: &mut ExprDesc,
    flip: bool,
    line: i32,
) -> Result<(), LuaError> {
    if flip { swapexps(e1, e2); }
    codebinexpval(fs, ls, state, opr, e1, e2, line)
}

fn codearith(
    fs: &mut FuncState,
    ls: &mut LexState,
    state: &mut LuaState,
    opr: BinOpr,
    e1: &mut ExprDesc,
    e2: &mut ExprDesc,
    flip: bool,
    line: i32,
) -> Result<(), LuaError> {
    if tonumeral(e2, None) && exp2_k(fs, ls, state, e2)? {
        codebin_k(fs, ls, state, opr, e1, e2, flip, line)
    } else {
        codebinno_k(fs, ls, state, opr, e1, e2, flip, line)
    }
}

fn codecommutative(
    fs: &mut FuncState,
    ls: &mut LexState,
    state: &mut LuaState,
    op: BinOpr,
    e1: &mut ExprDesc,
    e2: &mut ExprDesc,
    line: i32,
) -> Result<(), LuaError> {
    let mut flip = false;
    if tonumeral(e1, None) {
        swapexps(e1, e2);
        flip = true;
    }
    if op == BinOpr::Add && is_scint(e2) {
        codebini(fs, ls, state, OpCode::AddI, e1, e2, flip, line, TagMethod::Add)
    } else {
        codearith(fs, ls, state, op, e1, e2, flip, line)
    }
}

fn codebitwise(
    fs: &mut FuncState,
    ls: &mut LexState,
    state: &mut LuaState,
    opr: BinOpr,
    e1: &mut ExprDesc,
    e2: &mut ExprDesc,
    line: i32,
) -> Result<(), LuaError> {
    let mut flip = false;
    if e1.k == ExprKind::VKInt {
        swapexps(e1, e2);
        flip = true;
    }
    if e2.k == ExprKind::VKInt && exp2_k(fs, ls, state, e2)? {
        codebin_k(fs, ls, state, opr, e1, e2, flip, line)
    } else {
        codebinno_k(fs, ls, state, opr, e1, e2, flip, line)
    }
}

fn codeorder(
    fs: &mut FuncState,
    ls: &mut LexState,
    state: &mut LuaState,
    opr: BinOpr,
    e1: &mut ExprDesc,
    e2: &mut ExprDesc,
) -> Result<(), LuaError> {
    let nv = fs.nactvar as i32;
    let mut im = 0i32;
    let mut isfloat = false;
    let (r1, r2, op);
    if is_scnumber(e2, &mut im, &mut isfloat) {
        r1 = exp2anyreg(fs, ls, state, e1)? as i32;
        r2 = im;
        op = binopr2op(opr, BinOpr::Lt, OpCode::LtI);
    } else if is_scnumber(e1, &mut im, &mut isfloat) {
        r1 = exp2anyreg(fs, ls, state, e2)? as i32;
        r2 = im;
        op = binopr2op(opr, BinOpr::Lt, OpCode::GtI);
    } else {
        r1 = exp2anyreg(fs, ls, state, e1)? as i32;
        r2 = exp2anyreg(fs, ls, state, e2)? as i32;
        op = binopr2op(opr, BinOpr::Lt, OpCode::Lt);
    }
    let e1c = e1.clone();
    let e2c = e2.clone();
    freeregs(fs, e1c.u.info, e2c.u.info, nv);
    e1.u.info = condjump(fs, ls, op, r1 as u32, r2 as u32, isfloat as u32, 1)?;
    e1.k = ExprKind::VJmp;
    Ok(())
}

fn codeeq(
    fs: &mut FuncState,
    ls: &mut LexState,
    state: &mut LuaState,
    opr: BinOpr,
    e1: &mut ExprDesc,
    e2: &mut ExprDesc,
) -> Result<(), LuaError> {
    let nv = fs.nactvar as i32;
    let mut im = 0i32;
    let mut isfloat = false;
    if e1.k != ExprKind::VNonReloc {
        debug_assert!(matches!(e1.k, ExprKind::VK | ExprKind::VKInt | ExprKind::VKFlt));
        swapexps(e1, e2);
    }
    let r1 = exp2anyreg(fs, ls, state, e1)? as u32;
    let (op, r2): (OpCode, u32);
    if is_scnumber(e2, &mut im, &mut isfloat) {
        op = OpCode::EqI;
        r2 = im as u32;
    } else if exp2_rk(fs, ls, state, e2)? {
        op = OpCode::EqK;
        r2 = e2.u.info as u32;
    } else {
        op = OpCode::Eq;
        r2 = exp2anyreg(fs, ls, state, e2)? as u32;
    }
    let e1c = e1.clone();
    let e2c = e2.clone();
    freeregs(fs, e1c.u.info, e2c.u.info as i32, nv);
    let eq_k = (opr == BinOpr::Eq) as u32;
    e1.u.info = condjump(fs, ls, op, r1, r2, isfloat as u32, eq_k)?;
    e1.k = ExprKind::VJmp;
    Ok(())
}

// ─── Prefix / infix / posfix ─────────────────────────────────────────────────

// C: void luaK_prefix (FuncState *fs, UnOpr opr, expdesc *e, int line) { ... }
pub(crate) fn prefix(
    fs: &mut FuncState,
    ls: &mut LexState,
    state: &mut LuaState,
    opr: UnOpr,
    e: &mut ExprDesc,
    line: i32,
) -> Result<(), LuaError> {
    // C: static const expdesc ef = {VKINT, {0}, NO_JUMP, NO_JUMP};
    let ef = ExprDesc::const_zero();
    dischargevars(fs, ls, state, e)?;
    match opr {
        UnOpr::Minus | UnOpr::BNot => {
            // C: if (constfolding(fs, opr + LUA_OPUNM, e, &ef)) break;
            let op_code = LUA_OPUNM + opr as i32;
            if constfolding(fs, ls, state, op_code, e, &ef)? {
                return Ok(());
            }
            codeunexpval(fs, ls, state, unopr2op(opr), e, line)?;
        }
        UnOpr::Len => {
            codeunexpval(fs, ls, state, OpCode::Len, e, line)?;
        }
        UnOpr::Not => {
            codenot(fs, ls, state, e)?;
        }
        UnOpr::NoUnOpr => { debug_assert!(false, "prefix: NoUnOpr"); }
    }
    Ok(())
}

// C: void luaK_infix (FuncState *fs, BinOpr op, expdesc *v) { ... }
pub(crate) fn infix(
    fs: &mut FuncState,
    ls: &mut LexState,
    state: &mut LuaState,
    op: BinOpr,
    v: &mut ExprDesc,
) -> Result<(), LuaError> {
    dischargevars(fs, ls, state, v)?;
    match op {
        BinOpr::And => { goiftrue(fs, ls, state, v)?; }
        BinOpr::Or  => { goiffalse(fs, ls, state, v)?; }
        BinOpr::Concat => { exp2nextreg(fs, ls, state, v)?; }
        BinOpr::Add | BinOpr::Sub | BinOpr::Mul | BinOpr::Div | BinOpr::IDiv
        | BinOpr::Mod | BinOpr::Pow | BinOpr::BAnd | BinOpr::BOr | BinOpr::BXor
        | BinOpr::Shl | BinOpr::Shr => {
            if !tonumeral(v, None) {
                exp2anyreg(fs, ls, state, v)?;
            }
        }
        BinOpr::Eq | BinOpr::Ne => {
            if !tonumeral(v, None) {
                exp2_rk(fs, ls, state, v)?;
            }
        }
        BinOpr::Lt | BinOpr::Le | BinOpr::Gt | BinOpr::Ge => {
            let mut dummy1 = 0i32;
            let mut dummy2 = false;
            if !is_scnumber(v, &mut dummy1, &mut dummy2) {
                exp2anyreg(fs, ls, state, v)?;
            }
        }
        _ => { debug_assert!(false, "infix: invalid op"); }
    }
    Ok(())
}

// C: static void codeconcat (FuncState *fs, expdesc *e1, expdesc *e2, int line) { ... }
fn codeconcat(
    fs: &mut FuncState,
    ls: &mut LexState,
    state: &mut LuaState,
    e1: &mut ExprDesc,
    e2: &ExprDesc,
    line: i32,
) -> Result<(), LuaError> {
    let nv = fs.nactvar as i32;
    if let Some(ie2_idx) = previous_instruction_idx(fs) {
        if fs.f.code[ie2_idx].opcode() == Some(OpCode::Concat) {
            let n = fs.f.code[ie2_idx].arg_b() as i32;
            debug_assert_eq!(e1.u.info + 1, fs.f.code[ie2_idx].arg_a() as i32);
            // C: freeexp(fs, e2)
            let e2c = e2.clone(); freeexp(fs, &e2c, nv);
            fs.f.code[ie2_idx].set_arg_a(e1.u.info as u32);
            fs.f.code[ie2_idx].set_arg_b((n + 1) as u32);
            return Ok(());
        }
    }
    // C: luaK_codeABC(fs, OP_CONCAT, e1->u.info, 2, 0)
    code_abc(fs, ls, OpCode::Concat, e1.u.info as u32, 2, 0)?;
    let e2c = e2.clone(); freeexp(fs, &e2c, nv);
    fixline(fs, ls, line)?;
    Ok(())
}

// C: void luaK_posfix (FuncState *fs, BinOpr opr, expdesc *e1, expdesc *e2, int line) { ... }
pub(crate) fn posfix(
    fs: &mut FuncState,
    ls: &mut LexState,
    state: &mut LuaState,
    opr: BinOpr,
    e1: &mut ExprDesc,
    e2: &mut ExprDesc,
    line: i32,
) -> Result<(), LuaError> {
    dischargevars(fs, ls, state, e2)?;
    // C: if (foldbinop(opr) && constfolding(fs, opr + LUA_OPADD, e1, e2))
    if opr.is_foldable() {
        let op_code = LUA_OPADD + opr as i32;
        if constfolding(fs, ls, state, op_code, e1, e2)? {
            return Ok(());
        }
    }
    match opr {
        BinOpr::And => {
            debug_assert_eq!(e1.t, NO_JUMP);
            concat(fs, ls, &mut e2.f, e1.f)?;
            *e1 = e2.clone();
        }
        BinOpr::Or => {
            debug_assert_eq!(e1.f, NO_JUMP);
            concat(fs, ls, &mut e2.t, e1.t)?;
            *e1 = e2.clone();
        }
        BinOpr::Concat => {
            exp2nextreg(fs, ls, state, e2)?;
            let e2c = e2.clone();
            codeconcat(fs, ls, state, e1, &e2c, line)?;
        }
        BinOpr::Add | BinOpr::Mul => {
            codecommutative(fs, ls, state, opr, e1, e2, line)?;
        }
        BinOpr::Sub => {
            if finishbinexpneg(fs, ls, state, e1, e2, OpCode::AddI, line, TagMethod::Sub)? {
                return Ok(());
            }
            codearith(fs, ls, state, opr, e1, e2, false, line)?;
        }
        BinOpr::Div | BinOpr::IDiv | BinOpr::Mod | BinOpr::Pow => {
            codearith(fs, ls, state, opr, e1, e2, false, line)?;
        }
        BinOpr::BAnd | BinOpr::BOr | BinOpr::BXor => {
            codebitwise(fs, ls, state, opr, e1, e2, line)?;
        }
        BinOpr::Shl => {
            if is_scint(e1) {
                swapexps(e1, e2);
                codebini(fs, ls, state, OpCode::ShlI, e1, e2, true, line, TagMethod::Shl)?;
            } else if finishbinexpneg(fs, ls, state, e1, e2, OpCode::ShrI, line, TagMethod::Shl)? {
                // coded as (r1 >> -I)
            } else {
                codebinexpval(fs, ls, state, opr, e1, e2, line)?;
            }
        }
        BinOpr::Shr => {
            if is_scint(e2) {
                codebini(fs, ls, state, OpCode::ShrI, e1, e2, false, line, TagMethod::Shr)?;
            } else {
                codebinexpval(fs, ls, state, opr, e1, e2, line)?;
            }
        }
        BinOpr::Eq | BinOpr::Ne => {
            codeeq(fs, ls, state, opr, e1, e2)?;
        }
        BinOpr::Gt | BinOpr::Ge => {
            // C: '(a > b)' <=> '(b < a)'
            swapexps(e1, e2);
            let mapped = if opr == BinOpr::Gt { BinOpr::Lt } else { BinOpr::Le };
            codeorder(fs, ls, state, mapped, e1, e2)?;
        }
        BinOpr::Lt | BinOpr::Le => {
            codeorder(fs, ls, state, opr, e1, e2)?;
        }
        _ => { debug_assert!(false, "posfix: invalid op"); }
    }
    Ok(())
}

// ─── Line fixup ───────────────────────────────────────────────────────────────

// C: void luaK_fixline (FuncState *fs, int line) { ... }
pub(crate) fn fixline(
    fs: &mut FuncState,
    ls: &LexState,
    line: i32,
) -> Result<(), LuaError> {
    removelastlineinfo(fs);
    // savelineinfo re-records with the corrected line
    // PORT NOTE: simplified — uses the ls.lastline override
    let pc = (fs.pc - 1) as usize;
    let linedif_raw = line - fs.previousline;
    if fs.f.lineinfo.len() <= pc {
        fs.f.lineinfo.resize(pc + 1, 0i8);
    }
    if linedif_raw.abs() < LIM_LINE_DIFF && (fs.iwthabs as i32) < MAX_IWTH_ABS {
        fs.f.lineinfo[pc] = linedif_raw as i8;
        fs.iwthabs += 1;
    } else {
        fs.f.lineinfo[pc] = ABS_LINE_INFO;
        fs.f.abslineinfo.push(AbsLineInfo { pc: pc as i32, line });
        fs.nabslineinfo += 1;
        fs.iwthabs = 1;
    }
    fs.previousline = line;
    Ok(())
}

// ─── NEWTABLE / SETLIST helpers ───────────────────────────────────────────────

// C: void luaK_settablesize (FuncState *fs, int pc, int ra, int asize, int hsize) { ... }
pub(crate) fn settablesize(
    fs: &mut FuncState,
    pc: i32,
    ra: i32,
    asize: i32,
    hsize: i32,
) {
    // C: int rb = (hsize != 0) ? luaO_ceillog2(hsize) + 1 : 0;
    let rb = if hsize != 0 {
        (hsize as u32).next_power_of_two().trailing_zeros() as i32 + 1
    } else {
        0
    };
    let extra = asize / (MAXARG_C as i32 + 1);
    let rc = asize % (MAXARG_C as i32 + 1);
    let k = if extra > 0 { 1u32 } else { 0u32 };
    fs.f.code[pc as usize] = Instruction::abck(OpCode::NewTable, ra as u32, rb as u32, rc as u32, k);
    fs.f.code[pc as usize + 1] = Instruction::ax(OpCode::ExtraArg, extra as u32);
}

// C: void luaK_setlist (FuncState *fs, int base, int nelems, int tostore) { ... }
pub(crate) fn setlist(
    fs: &mut FuncState,
    ls: &mut LexState,
    base: i32,
    nelems: i32,
    tostore: i32,
) -> Result<(), LuaError> {
    debug_assert!(tostore != 0 && tostore as u32 <= LFIELDS_PER_FLUSH);
    let tostore = if tostore == -1 { 0i32 } else { tostore }; // LUA_MULTRET → 0
    if nelems <= MAXARG_C as i32 {
        code_abc(fs, ls, OpCode::SetList, base as u32, tostore as u32, nelems as u32)?;
    } else {
        let extra = nelems / (MAXARG_C as i32 + 1);
        let nelems_lo = nelems % (MAXARG_C as i32 + 1);
        code_abck(fs, ls, OpCode::SetList, base as u32, tostore as u32, nelems_lo as u32, 1)?;
        codeextraarg(fs, ls, extra as u32)?;
    }
    fs.freereg = (base + 1) as u8;
    Ok(())
}

// ─── Jump-chain optimisation ──────────────────────────────────────────────────

// C: static int finaltarget (Instruction *code, int i) { ... }
fn finaltarget(code: &[Instruction], mut i: i32) -> i32 {
    // avoid infinite loops: cap at 100 iterations
    for _ in 0..100 {
        let pc = code[i as usize];
        if pc.opcode() != Some(OpCode::Jmp) { break; }
        i += pc.arg_s_j() + 1;
    }
    i
}

// C: void luaK_finish (FuncState *fs) { ... }
pub(crate) fn finish(
    fs: &mut FuncState,
    ls: &LexState,
) -> Result<(), LuaError> {
    for i in 0..fs.pc {
        let opcode = fs.f.code[i as usize].opcode();
        // C: lua_assert(i == 0 || isOT(*(pc - 1)) == isIT(*pc));
        debug_assert!(
            i == 0
                || fs.f.code[(i - 1) as usize].is_out_top()
                    == fs.f.code[i as usize].is_in_top()
        );
        match opcode {
            Some(OpCode::Return0) | Some(OpCode::Return1) => {
                if !fs.needclose && !fs.f.is_vararg {
                    continue; // no extra work
                }
                // change to OP_RETURN to do the extra work
                fs.f.code[i as usize].set_opcode(OpCode::Return);
                // FALLTHROUGH — handled by next match arm below
                let pc = &mut fs.f.code[i as usize];
                if fs.needclose { pc.set_arg_k(1); }
                if fs.f.is_vararg { pc.set_arg_c(fs.f.numparams as u32 + 1); }
            }
            Some(OpCode::Return) | Some(OpCode::TailCall) => {
                let pc = &mut fs.f.code[i as usize];
                if fs.needclose { pc.set_arg_k(1); }
                if fs.f.is_vararg { pc.set_arg_c(fs.f.numparams as u32 + 1); }
            }
            Some(OpCode::Jmp) => {
                let target = {
                    let code_slice = &fs.f.code[..];
                    finaltarget(code_slice, i)
                };
                fixjump(fs, ls, i, target)?;
            }
            _ => {}
        }
    }
    Ok(())
}

// PORT STATUS
//   source:        src/lcode.c  (1875 lines, ~60 functions)
//   target_crate:  lua-code
//   confidence:    medium
//   todos:         15
//   port_notes:    8
//   unsafe_blocks: 0
//   notes:         All ~60 C functions translated.  FuncState.f needs
//                  RefCell<LuaProto> for mutation in Phase B.  code_no_state()
//                  is a Phase A shim; replace with code(state) in Phase B.
//                  OpCode::from_delta / TagMethod::from_delta need Phase B impls.
//                  ArithOp enum pending in lua-types (raw i32 used here).
//                  luaV_flttointeger inlined as fract()==0.0 (approximate).
//                  sem_error returns LuaError directly (not Result<!,_>).
//                  LuaString::is_short() needed in lua-types for is_kstr().
// ──────────────────────────────────────────────────────────────────────────
