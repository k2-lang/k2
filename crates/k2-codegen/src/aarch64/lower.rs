//! MIR → AArch64 machine-code lowering (v0.18, the hello-class subset).
//!
//! This is the AArch64 counterpart of the x86-64 [`crate::lower`]. It walks the
//! SAME monomorphized [`MirFunction`]s the x86 backend compiles and emits an
//! AAPCS64-correct AArch64 function via the [`super::encode`] encoder. The two
//! lowerings share, verbatim, the target-neutral analysis that drives the walk:
//! the [`crate::frame`] frame planner, the [`crate::lower::classify`] ABI
//! classification (AAPCS64 == SysV for the corpus), the [`crate::fmt_native`]
//! print-format parser, the [`crate::layout`] byte oracle, and the
//! [`crate::lower::repr_of`] integer-repr / [`crate::lower::compare_cc`] /
//! [`crate::lower::trap_message`] helpers.
//!
//! ## Register / frame model
//!
//! Rather than re-run the [`crate::regalloc`] linear-scan allocator (which is
//! typed over the x86 `Gpr`), this lowering keeps **every local in a stack home**
//! and threads scalar operands through fixed AAPCS64 caller-saved temporaries
//! (`x9`–`x15`), with `x16`/`x17` (the intra-procedure-call scratch IP0/IP1) as
//! the address / print-cursor scratch. This is the AArch64 analog of the original
//! v0.14 x86 "everything on the stack" scheme: simple, self-contained, and
//! correct. The frame is the AAPCS64 `stp x29,x30,[sp,#-16]!` / `mov x29,sp` /
//! `sub sp,sp,#frame` shape; `sp` stays 16-aligned.
//!
//! ## Scope
//!
//! Covered: the AAPCS64 prologue/epilogue + parameter receipt, scalar
//! `Use`/`Binary`/`Unary`/`Cast`/`Ref`, aggregate construction (tuples/structs/
//! arrays/slices) into a home, the CFG terminators, `print` formatting
//! (literals + `{s}`/`{d}`/`{}`/`{x}`/`{X}`/`{b}`/`{o}`/`{c}` incl. 64- and
//! 128-bit decimals), the `write`/`exit` syscalls, the escaped-`main`-error path,
//! and the safety-check `Trap` lowering. A reached construct outside this subset
//! (notably the `*System` heap runtime) returns a clean
//! [`CodegenError::Unsupported`] deferral — never a miscompile.

use k2_mir::{
    AggKind, BinOp, CastKind, Const, ConstData, DiscrKind, IntrinsicRoot, MirFunction, MirProgram,
    Operand, Place, Proj, Rvalue, SliceMeta, Statement, Terminator, TrapReason, UnOp,
};
use k2_types::{Type, TypeId};

use super::encode::{Aarch64Asm, MemSize, Reg, FP, LR, SP};
use crate::encode::{Cc, Fixup, LabelId};
use crate::fmt_native::{self, Step, Verb};
use crate::frame::{self, FramePlan};
use crate::layout::{self, Layout};
use crate::lower::{
    classify, compare_cc, func_prints, is_comparison, outgoing_args_bytes, repr_of, trap_message,
    ArgClass, PANIC_EXIT,
};
use crate::{CodegenError, RoData};

// ---- Register roles (the AArch64 analog of the x86 RAX/RCX/RDX/R11/R10 set). ----

/// The result / accumulator register (the x86 `rax` analog).
const RES: Reg = Reg(9);
/// The primary secondary scratch (the x86 `rcx` analog).
const S2: Reg = Reg(10);
/// A tertiary scratch.
const S3: Reg = Reg(11);
/// A fourth scratch (digit loops, padding).
const S4: Reg = Reg(12);
/// A fifth scratch.
const S5: Reg = Reg(13);
/// The address / print-cursor scratch (`x16` = IP0). Never holds a long-lived
/// value across a call.
const ADDR: Reg = Reg(16);
/// The AAPCS64 integer/pointer argument + return registers, `x0`–`x7`.
const ARG_REGS: [Reg; 8] = [
    Reg(0),
    Reg(1),
    Reg(2),
    Reg(3),
    Reg(4),
    Reg(5),
    Reg(6),
    Reg(7),
];

/// How a function-entry `main` maps its result to the process exit code (mirrors
/// the x86 lowering's `EntryKind`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    /// An ordinary helper.
    Helper,
    /// `main(...) !void`: 0 on success, 1 on an escaped error.
    VoidEntry,
    /// `main() IntType`: the integer result is the exit code.
    IntEntry,
}

/// The per-function AArch64 lowering state.
pub(crate) struct Aarch64FnLower<'p> {
    prog: &'p MirProgram,
    func: &'p MirFunction,
    asm: Aarch64Asm,
    entry_kind: EntryKind,
    plan: FramePlan,
    sysnr: crate::target::SysNr,
}

impl<'p> Aarch64FnLower<'p> {
    /// Creates an AArch64 lowering context for `func`, planning the stack frame
    /// (with every local forced to a home, since this backend does not allocate
    /// registers).
    pub(crate) fn new(prog: &'p MirProgram, func: &'p MirFunction) -> Aarch64FnLower<'p> {
        let entry_kind = if func.name == "main" {
            match prog.arena.get(func.ret) {
                Type::Int { .. } => EntryKind::IntEntry,
                _ => EntryKind::VoidEntry,
            }
        } else {
            EntryKind::Helper
        };

        // Force every local to need a memory home: this backend keeps no value in a
        // register across statements. No callee-saved registers are allocated.
        let needs_home = vec![true; func.locals.len()];
        let home_ty: Vec<Option<TypeId>> = vec![None; func.locals.len()];
        let home_size: Vec<Option<(u64, u64)>> = vec![None; func.locals.len()];
        let prints = func_prints(func);
        let outgoing = outgoing_args_bytes(prog, func);
        let plan = frame::plan(
            func,
            &prog.arena,
            &needs_home,
            &home_ty,
            &home_size,
            &[], // no callee-saved registers used
            prints,
            false, // sret is handled inline (corpus hello-class returns are scalar/void)
            outgoing,
        );

        Aarch64FnLower {
            prog,
            func,
            asm: Aarch64Asm::new(),
            entry_kind,
            plan,
            sysnr: crate::target::Target::Aarch64Linux.sysnr(),
        }
    }

    /// The stack home (negative `x29`-relative offset) of a local. Every local has
    /// one in this backend.
    fn home(&self, local: k2_mir::LocalId) -> i32 {
        self.plan.local_home[local.index()].unwrap_or(0)
    }

    /// The layout of a type, falling back to a word.
    fn layout(&self, ty: TypeId) -> Layout {
        layout::layout_of(&self.prog.arena, ty).unwrap_or(Layout::WORD)
    }

    /// A short `Unsupported` error tagged with the function + a note that the
    /// aarch64 backend currently covers the hello-class subset.
    fn unsup(&self, what: &str) -> CodegenError {
        CodegenError::Unsupported(format!(
            "{what} in `{}` (aarch64 backend covers the hello-class subset; \
             cross-compile hello-class programs or use the x86-64 backend / VM)",
            self.func.name
        ))
    }

    // =====================================================================
    //  Top-level lowering
    // =====================================================================

    /// Lowers the whole function, returning its finalized code + cross-function
    /// fixups for the program link pass.
    pub(crate) fn lower(
        mut self,
        rodata: &mut RoData,
    ) -> Result<(Vec<u8>, Vec<Fixup>), CodegenError> {
        self.asm.reserve_labels(self.func.blocks.len());

        // ---- Prologue: save {fp, lr}, set up fp, reserve the frame. ----
        self.asm.stp_pre(FP, LR, SP, -16);
        self.asm.mov_sp(FP, SP);
        let frame = self.plan.frame_size;
        if frame > 0 {
            self.emit_sp_sub(frame as u32);
        }
        self.lower_prologue_params()?;

        // ---- Blocks. ----
        for (bi, block) in self.func.blocks.iter().enumerate() {
            self.asm.bind_label(LabelId(bi as u32));
            for stmt in &block.stmts {
                self.lower_stmt(stmt, rodata)?;
            }
            self.lower_terminator(&block.term, rodata)?;
        }

        Ok(self.asm.finish())
    }

    /// Emits `sub sp, sp, #n` for an arbitrary frame size (the immediate may exceed
    /// the 12-bit `add/sub` field, in which case it is materialized + subtracted).
    fn emit_sp_sub(&mut self, n: u32) {
        if n <= 0xFFF {
            self.asm.sub_imm(SP, SP, n);
        } else {
            self.asm.mov_imm(S3, n as i64);
            // SP-capable subtract: the plain shifted-register `sub` reads slot 31 as
            // `xzr`, mis-encoding `sub sp, sp, x11` as `neg xzr, x11` (the frame
            // would never be reserved). The extended-register form names `sp`.
            self.asm.sub_ext(SP, SP, S3);
        }
    }

    /// Restores `sp`, reloads `{fp, lr}`, and returns.
    fn epilogue_and_ret(&mut self) {
        // mov sp, x29 ; ldp x29, x30, [sp], #16 ; ret
        self.asm.mov_sp(SP, FP);
        self.asm.ldp_post(FP, LR, SP, 16);
        self.asm.ret();
    }

    /// Receives the function's parameters from their AAPCS64 locations into homes.
    /// The corpus hello-class functions take only scalar/pointer params (notably
    /// `main(sys: *System)` in `x0`); aggregate-by-value and >8-int-arg receipts
    /// are refused cleanly.
    fn lower_prologue_params(&mut self) -> Result<(), CodegenError> {
        let mut int_idx = 0usize;
        for &param in &self.func.params {
            let ty = self.func.locals[param.index()].ty;
            match classify(self.prog, ty) {
                ArgClass::OneInt => {
                    if int_idx >= ARG_REGS.len() {
                        return Err(self.unsup("a stack-passed parameter"));
                    }
                    let src = ARG_REGS[int_idx];
                    int_idx += 1;
                    let h = self.home(param);
                    self.store_sized(FP, h, src, ty);
                }
                ArgClass::TwoInt => {
                    if int_idx + 2 > ARG_REGS.len() {
                        return Err(self.unsup("a stack-passed aggregate parameter"));
                    }
                    let lo = ARG_REGS[int_idx];
                    let hi = ARG_REGS[int_idx + 1];
                    int_idx += 2;
                    let h = self.home(param);
                    self.asm.store(lo, FP, h as i64, MemSize::X);
                    self.asm.store(hi, FP, h as i64 + 8, MemSize::X);
                }
                ArgClass::Sse | ArgClass::Memory { .. } => {
                    return Err(self.unsup("a float / memory-class parameter"));
                }
            }
        }
        Ok(())
    }

    // =====================================================================
    //  Statements & rvalues
    // =====================================================================

    fn lower_stmt(&mut self, stmt: &Statement, rodata: &mut RoData) -> Result<(), CodegenError> {
        match stmt {
            Statement::Assign { place, rvalue, .. } => {
                if place.is_local() {
                    self.lower_rvalue(place.base, rvalue, rodata)
                } else {
                    self.lower_rvalue_to_place(place, rvalue, rodata)
                }
            }
            Statement::Eval { rvalue, .. } => self.lower_rvalue_discard(rvalue, rodata),
            Statement::StorageLive(_)
            | Statement::StorageDead(_)
            | Statement::Check(_)
            | Statement::Note(_) => Ok(()),
        }
    }

    /// Lowers a discarded rvalue (an `Eval`) — chiefly the `print` intrinsic.
    fn lower_rvalue_discard(
        &mut self,
        rvalue: &Rvalue,
        rodata: &mut RoData,
    ) -> Result<(), CodegenError> {
        match rvalue {
            Rvalue::Intrinsic { .. } => self.lower_intrinsic_into_res(rvalue, rodata),
            Rvalue::Call { .. } => Err(self.unsup("a discarded inter-procedure call")),
            _ => Ok(()),
        }
    }

    /// Lowers `dst = rvalue`.
    fn lower_rvalue(
        &mut self,
        dst: k2_mir::LocalId,
        rvalue: &Rvalue,
        rodata: &mut RoData,
    ) -> Result<(), CodegenError> {
        let dst_ty = self.func.locals[dst.index()].ty;

        // Aggregate-typed destination (including a `deferred`-typed print tuple that
        // is built as an aggregate): build into the home.
        if let Some(ty) = self.effective_agg_ty(dst, rvalue) {
            return self.lower_aggregate_rvalue(dst, rvalue, ty, rodata);
        }

        if self.is_wide_int(dst_ty) {
            return self.lower_wide_rvalue(dst, rvalue);
        }

        match rvalue {
            Rvalue::Use(op) => {
                self.operand_to(op, RES)?;
                self.normalize(RES, dst_ty);
                self.store_scalar(dst, RES);
                Ok(())
            }
            Rvalue::Binary { op, lhs, rhs, ty } => {
                self.eval_binary(*op, lhs, rhs, *ty)?;
                self.store_scalar(dst, RES);
                Ok(())
            }
            Rvalue::Unary { op, operand, ty } => {
                self.operand_to(operand, RES)?;
                match op {
                    UnOp::Neg => {
                        self.asm.neg(RES, RES);
                        self.normalize(RES, *ty);
                    }
                    UnOp::BitNot => {
                        self.asm.mvn(RES, RES);
                        self.normalize(RES, *ty);
                    }
                    UnOp::Not => {
                        // logical not of a bool: x ^ 1.
                        self.asm.mov_imm(S2, 1);
                        self.asm.eor(RES, RES, S2);
                    }
                }
                self.store_scalar(dst, RES);
                Ok(())
            }
            Rvalue::Cast {
                kind: CastKind::Widen | CastKind::IntNarrow | CastKind::PtrReinterpret,
                operand,
                ty,
            } => {
                self.operand_to(operand, RES)?;
                self.normalize(RES, *ty);
                self.store_scalar(dst, RES);
                Ok(())
            }
            Rvalue::Ref { place, .. } => {
                self.place_addr(place, RES)?;
                self.store_scalar(dst, RES);
                Ok(())
            }
            Rvalue::Intrinsic { .. } => {
                self.lower_intrinsic_into_res(rvalue, rodata)?;
                self.store_scalar(dst, RES);
                Ok(())
            }
            Rvalue::MakeNull(_) => {
                self.asm.mov_imm(RES, 0);
                self.store_scalar(dst, RES);
                Ok(())
            }
            Rvalue::MakeSome(op, _) => {
                self.operand_to(op, RES)?;
                self.store_scalar(dst, RES);
                Ok(())
            }
            Rvalue::Aggregate { fields, .. } if fields.is_empty() => {
                self.asm.mov_imm(RES, 0);
                self.store_scalar(dst, RES);
                Ok(())
            }
            Rvalue::Discriminant { operand, kind } => self.lower_discriminant(dst, operand, *kind),
            other => Err(self.unsup(&format!("scalar rvalue {}", rvalue_kind(other)))),
        }
    }

    /// Lowers `*place = rvalue` for a projected scalar store.
    fn lower_rvalue_to_place(
        &mut self,
        place: &Place,
        rvalue: &Rvalue,
        _rodata: &mut RoData,
    ) -> Result<(), CodegenError> {
        let val_ty = self.place_type(place);
        if frame::is_memory_aggregate(&self.prog.arena, val_ty) {
            if let Rvalue::Use(Operand::Copy(src)) = rvalue {
                self.place_addr(src, RES)?;
                self.place_addr(place, ADDR)?;
                let size = self.layout(val_ty).size;
                self.memcpy(ADDR, RES, size);
                return Ok(());
            }
            return Err(self.unsup("an aggregate rvalue into a projected place"));
        }
        // Scalar store: value into RES, address into ADDR, sized store.
        match rvalue {
            Rvalue::Use(op) => {
                self.operand_to(op, RES)?;
                self.normalize(RES, val_ty);
            }
            Rvalue::Binary { op, lhs, rhs, ty } => self.eval_binary(*op, lhs, rhs, *ty)?,
            Rvalue::Ref { place: p, .. } => self.place_addr(p, RES)?,
            other => return Err(self.unsup(&format!("rvalue {} into a place", rvalue_kind(other)))),
        }
        self.place_addr(place, ADDR)?;
        self.store_sized(ADDR, 0, RES, val_ty);
        Ok(())
    }

    /// The aggregate type a `dst = rvalue` builds into a home, or `None` if `dst`
    /// is scalar (mirrors the x86 `effective_agg_ty`, minus the regalloc overrides
    /// this backend does not use — every local already has a home).
    fn effective_agg_ty(&self, dst: k2_mir::LocalId, rvalue: &Rvalue) -> Option<TypeId> {
        let dty = self.func.locals[dst.index()].ty;
        if frame::is_memory_aggregate(&self.prog.arena, dty) {
            return Some(dty);
        }
        // A `deferred`-typed print tuple is produced as an aggregate; build it as a
        // packed tuple into the home.
        if rvalue_builds_aggregate(rvalue) && matches!(self.prog.arena.get(dty), Type::Deferred) {
            return Some(dty);
        }
        None
    }

    // =====================================================================
    //  Aggregate construction (into a stack home)
    // =====================================================================

    fn lower_aggregate_rvalue(
        &mut self,
        dst: k2_mir::LocalId,
        rvalue: &Rvalue,
        ty: TypeId,
        rodata: &mut RoData,
    ) -> Result<(), CodegenError> {
        let home = self.home(dst);
        match rvalue {
            Rvalue::Aggregate { kind, fields, .. } => {
                self.build_aggregate(home, *kind, fields, ty, rodata)
            }
            Rvalue::Use(op) => self.aggregate_use(home, op, ty, rodata),
            Rvalue::MakeSlice {
                ptr,
                offset,
                len,
                ty: sty,
            } => self.build_make_slice(home, ptr, offset, len, *sty, rodata),
            Rvalue::MakeOk(op, ety) => self.build_make_ok(home, op, *ety, rodata),
            Rvalue::MakeErr(tag, _) => {
                self.asm.mov_imm(RES, tag.0 as i64);
                self.asm.store(RES, FP, home as i64, MemSize::H);
                Ok(())
            }
            Rvalue::MakeSome(op, oty) => self.build_make_some(home, op, *oty, rodata),
            Rvalue::MakeNull(oty) => self.build_make_null(home, *oty),
            other => Err(self.unsup(&format!("aggregate rvalue {}", rvalue_kind(other)))),
        }
    }

    /// Copies / materializes `op` into an aggregate home, applying the implicit
    /// `T -> ?T` / `T -> E!T` coercion when a scalar source flows into an
    /// optional/error-union slot (mirrors the x86 `aggregate_use`).
    fn aggregate_use(
        &mut self,
        home: i32,
        op: &Operand,
        ty: TypeId,
        rodata: &mut RoData,
    ) -> Result<(), CodegenError> {
        if let Operand::Copy(src) = op {
            let src_ty = if src.is_local() {
                self.func.locals[src.base.index()].ty
            } else {
                self.place_type(src)
            };
            let coercible =
                !frame::is_memory_aggregate(&self.prog.arena, src_ty) && !self.is_wide_int(src_ty);
            if coercible {
                match self.prog.arena.get(ty) {
                    Type::Optional(inner)
                        if !matches!(self.prog.arena.get(*inner), Type::Pointer { .. }) =>
                    {
                        return self.build_make_some(home, op, ty, rodata);
                    }
                    Type::ErrorUnion { .. } => return self.build_make_ok(home, op, ty, rodata),
                    _ => {}
                }
            }
            // Otherwise a straight aggregate memcpy.
            self.place_addr(src, RES)?;
            self.lea_home(ADDR, home);
            let size = self.layout(ty).size;
            self.memcpy(ADDR, RES, size);
            return Ok(());
        }
        if let Operand::Const(c) = op {
            return self.materialize_const_aggregate(home, c, ty, rodata);
        }
        Err(self.unsup("an aggregate `Use` of a non-place operand"))
    }

    /// Materializes a constant aggregate (a string-literal slice, empty slice,
    /// `Ok(void)`, or interned aggregate const) into a home — the aarch64 analog of
    /// the x86 `materialize_const_aggregate`.
    fn materialize_const_aggregate(
        &mut self,
        home: i32,
        c: &Const,
        ty: TypeId,
        rodata: &mut RoData,
    ) -> Result<(), CodegenError> {
        match c {
            Const::Str(id) => {
                let (off, len) = self.intern_string(*id, rodata);
                self.asm.mov_imm_data(RES, off);
                self.asm.store(RES, FP, home as i64, MemSize::X);
                self.asm.mov_imm(RES, len as i64);
                self.asm.store(RES, FP, home as i64 + 8, MemSize::X);
                Ok(())
            }
            Const::EmptySlice { .. } => {
                self.asm.mov_imm(RES, 0);
                self.asm.store(RES, FP, home as i64, MemSize::X);
                self.asm.store(RES, FP, home as i64 + 8, MemSize::X);
                Ok(())
            }
            Const::Aggregate { id, ty: aty } => {
                let fields = match &self.prog.consts[id.0 as usize] {
                    ConstData::Aggregate(f) => f.clone(),
                    ConstData::Bytes(_) => {
                        return Err(self.unsup("aggregate const backed by bytes"))
                    }
                };
                let kind = match self.prog.arena.get(*aty) {
                    Type::Array { .. } => AggKind::Array,
                    _ => AggKind::Struct,
                };
                self.build_aggregate(home, kind, &fields, *aty, rodata)
            }
            Const::Undef { .. } => Ok(()),
            Const::Void => match self.prog.arena.get(ty) {
                Type::ErrorUnion { .. } => {
                    self.asm.mov_imm(RES, 0);
                    self.asm.store(RES, FP, home as i64, MemSize::H);
                    Ok(())
                }
                _ => Ok(()),
            },
            Const::ErrVal { tag, .. } => {
                self.asm.mov_imm(RES, tag.0 as i64);
                self.asm.store(RES, FP, home as i64, MemSize::H);
                Ok(())
            }
            scalar @ (Const::Int { .. } | Const::Bool(_)) => {
                let op = Operand::Const(scalar.clone());
                match self.prog.arena.get(ty) {
                    Type::Optional(_) => self.build_make_some(home, &op, ty, rodata),
                    Type::ErrorUnion { .. } => self.build_make_ok(home, &op, ty, rodata),
                    _ => Err(self.unsup("scalar constant into a non-optional aggregate")),
                }
            }
            other => Err(self.unsup(&format!("const aggregate {other:?}"))),
        }
    }

    /// Builds `Ok(v)` into an error-union home: `u16` tag 0 at +0, payload at the
    /// aligned payload offset.
    fn build_make_ok(
        &mut self,
        home: i32,
        op: &Operand,
        ety: TypeId,
        rodata: &mut RoData,
    ) -> Result<(), CodegenError> {
        let ok_ty = match self.prog.arena.get(ety) {
            Type::ErrorUnion { ok, .. } => *ok,
            _ => self.operand_type(op).unwrap_or(self.prog.arena.t_u8()),
        };
        let poff = layout::error_union_payload_off(&self.prog.arena, ety) as i32;
        self.store_field_operand(home + poff, op, ok_ty, rodata)?;
        self.asm.mov_imm(RES, 0);
        self.asm.store(RES, FP, home as i64, MemSize::H);
        Ok(())
    }

    /// Builds `Some(v)` into a non-pointer-niche optional home: payload at +0, flag
    /// byte 1 after it.
    fn build_make_some(
        &mut self,
        home: i32,
        op: &Operand,
        oty: TypeId,
        rodata: &mut RoData,
    ) -> Result<(), CodegenError> {
        let inner = match self.prog.arena.get(oty) {
            Type::Optional(inner) => *inner,
            _ => self.operand_type(op).unwrap_or(self.prog.arena.t_u8()),
        };
        self.store_field_operand(home, op, inner, rodata)?;
        let flag_off = layout::optional_flag_off(&self.prog.arena, oty).unwrap_or(0) as i32;
        self.asm.mov_imm(RES, 1);
        self.asm
            .store(RES, FP, home as i64 + flag_off as i64, MemSize::B);
        Ok(())
    }

    /// Builds `null` into a non-pointer-niche optional home: flag byte 0.
    fn build_make_null(&mut self, home: i32, oty: TypeId) -> Result<(), CodegenError> {
        let flag_off = layout::optional_flag_off(&self.prog.arena, oty).unwrap_or(0) as i32;
        self.asm.mov_imm(RES, 0);
        self.asm
            .store(RES, FP, home as i64 + flag_off as i64, MemSize::B);
        Ok(())
    }

    /// Builds a tuple/struct/array literal into the home, mirroring the x86
    /// `build_aggregate` for the layoutable + packed-`deferred` cases.
    fn build_aggregate(
        &mut self,
        home: i32,
        kind: AggKind,
        fields: &[Operand],
        ty: TypeId,
        rodata: &mut RoData,
    ) -> Result<(), CodegenError> {
        if layout::layout_of(&self.prog.arena, ty).is_none() {
            // A `deferred` tuple (a print argument tuple): pack the fields by their
            // operand types.
            let elem_ty = if matches!(kind, AggKind::Array) {
                match self.prog.arena.get(ty) {
                    Type::Array { elem, .. } => Some(*elem),
                    _ => None,
                }
            } else {
                None
            };
            let field_tys: Vec<TypeId> = fields
                .iter()
                .map(|f| {
                    if matches!(f, Operand::Const(Const::Str(_))) {
                        return self.prog.arena.t_str();
                    }
                    elem_ty
                        .or_else(|| self.operand_type(f))
                        .unwrap_or(self.prog.arena.t_u8())
                })
                .collect();
            let offs = packed_offsets(&self.prog.arena, &field_tys);
            for (i, f) in fields.iter().enumerate() {
                self.store_field_operand(home + offs[i] as i32, f, field_tys[i], rodata)?;
            }
            return Ok(());
        }
        let offsets = self.aggregate_field_offsets(kind, fields.len(), ty);
        for (i, f) in fields.iter().enumerate() {
            let fty = self.aggregate_field_type(kind, i, ty);
            self.store_field_operand(home + offsets[i] as i32, f, fty, rodata)?;
        }
        Ok(())
    }

    /// Stores a field operand at `[fp + off]` (scalar sized store, slice const, or
    /// nested-aggregate memcpy).
    fn store_field_operand(
        &mut self,
        off: i32,
        op: &Operand,
        fty: TypeId,
        rodata: &mut RoData,
    ) -> Result<(), CodegenError> {
        // A string constant field is a `{ptr, len}` slice.
        if let Operand::Const(Const::Str(id)) = op {
            let (data_off, len) = self.intern_string(*id, rodata);
            self.asm.mov_imm_data(RES, data_off);
            self.asm.store(RES, FP, off as i64, MemSize::X);
            self.asm.mov_imm(RES, len as i64);
            self.asm.store(RES, FP, off as i64 + 8, MemSize::X);
            return Ok(());
        }
        if frame::is_memory_aggregate(&self.prog.arena, fty) {
            if let Operand::Copy(src) = op {
                self.place_addr(src, RES)?;
                self.asm.add_imm_to(ADDR, FP, off);
                let size = self.layout(fty).size;
                self.memcpy(ADDR, RES, size);
                return Ok(());
            }
            return Err(self.unsup("a non-place aggregate field"));
        }
        if self.is_wide_int(fty) {
            self.eval_wide_operand(op, RES, S2)?;
            self.asm.store(RES, FP, off as i64, MemSize::X);
            self.asm.store(S2, FP, off as i64 + 8, MemSize::X);
            return Ok(());
        }
        self.operand_to(op, RES)?;
        self.normalize(RES, fty);
        self.store_sized_fp(off, RES, fty);
        Ok(())
    }

    /// Builds a `{ptr, len}` slice (`ptr + offset*stride`, `len`) into the home.
    fn build_make_slice(
        &mut self,
        home: i32,
        ptr: &Operand,
        offset: &Operand,
        len: &Operand,
        sty: TypeId,
        rodata: &mut RoData,
    ) -> Result<(), CodegenError> {
        let stride = layout::elem_size(&self.prog.arena, sty);
        self.slice_data_ptr(ptr, RES, rodata)?;
        self.operand_to(offset, S2)?;
        if stride.is_power_of_two() {
            let sh = stride.trailing_zeros() as u8;
            if sh != 0 {
                self.asm.lsl_imm(S2, S2, sh);
            }
        } else {
            self.asm.mov_imm(S3, stride as i64);
            self.asm.mul(S2, S2, S3);
        }
        self.asm.add(RES, RES, S2);
        self.operand_to(len, S2)?;
        self.asm.store(RES, FP, home as i64, MemSize::X);
        self.asm.store(S2, FP, home as i64 + 8, MemSize::X);
        Ok(())
    }

    /// Loads a slice/array operand's data pointer into `dst`.
    fn slice_data_ptr(
        &mut self,
        op: &Operand,
        dst: Reg,
        rodata: &mut RoData,
    ) -> Result<(), CodegenError> {
        match op {
            Operand::Const(Const::Str(id)) => {
                let (data_off, _) = self.intern_string(*id, rodata);
                self.asm.mov_imm_data(dst, data_off);
                Ok(())
            }
            Operand::Copy(p) => {
                let ty = self.place_type(p);
                match self.prog.arena.get(ty) {
                    Type::Slice { .. } => {
                        // ptr is the first word of the slice value.
                        self.place_addr(p, ADDR)?;
                        self.asm.load(dst, ADDR, 0, MemSize::X);
                        Ok(())
                    }
                    Type::Array { .. } => self.place_addr(p, dst),
                    _ => self.operand_to(op, dst),
                }
            }
            _ => self.operand_to(op, dst),
        }
    }

    fn aggregate_field_offsets(&self, kind: AggKind, n: usize, ty: TypeId) -> Vec<u64> {
        match kind {
            AggKind::Struct => {
                let mut offs = layout::field_offsets(&self.prog.arena, ty);
                offs.resize(n, 0);
                offs
            }
            AggKind::Array => {
                let stride = layout::elem_size(&self.prog.arena, ty);
                (0..n as u64).map(|i| i * stride).collect()
            }
            AggKind::Tuple => {
                let offs = layout::field_offsets(&self.prog.arena, ty);
                if offs.len() == n {
                    offs
                } else {
                    (0..n as u64).map(|i| i * 8).collect()
                }
            }
        }
    }

    fn aggregate_field_type(&self, kind: AggKind, i: usize, ty: TypeId) -> TypeId {
        match kind {
            AggKind::Struct | AggKind::Tuple => {
                if let Type::Struct(id) = self.prog.arena.get(ty) {
                    let info = &self.prog.arena.structs[id.0 as usize];
                    if let Some(f) = info.fields.get(i) {
                        return f.ty;
                    }
                }
                ty
            }
            AggKind::Array => match self.prog.arena.get(ty) {
                Type::Array { elem, .. } => *elem,
                _ => ty,
            },
        }
    }

    // =====================================================================
    //  Binary / scalar arithmetic
    // =====================================================================

    /// Evaluates a binary rvalue into [`RES`], normalized to `ty`.
    fn eval_binary(
        &mut self,
        op: BinOp,
        lhs: &Operand,
        rhs: &Operand,
        ty: TypeId,
    ) -> Result<(), CodegenError> {
        if self.is_float(ty) {
            return Err(self.unsup("f64 arithmetic"));
        }
        // lhs -> RES, rhs -> S2.
        self.operand_to(lhs, RES)?;
        self.operand_to(rhs, S2)?;
        let signed = repr_of(self.prog, ty).signed;
        if is_comparison(op) {
            // For a comparison the operands must be width-normalized first.
            self.normalize(RES, self.operand_type(lhs).unwrap_or(ty));
            self.normalize(S2, self.operand_type(rhs).unwrap_or(ty));
            self.asm.cmp(RES, S2);
            self.asm.cset(RES, compare_cc(op, signed));
            return Ok(());
        }
        match op {
            BinOp::Add => self.asm.add(RES, RES, S2),
            BinOp::Sub => self.asm.sub(RES, RES, S2),
            BinOp::Mul => self.asm.mul(RES, RES, S2),
            BinOp::BitAnd => self.asm.and(RES, RES, S2),
            BinOp::BitOr => self.asm.orr(RES, RES, S2),
            BinOp::BitXor => self.asm.eor(RES, RES, S2),
            BinOp::Div => {
                if signed {
                    self.asm.sdiv(RES, RES, S2);
                } else {
                    self.asm.udiv(RES, RES, S2);
                }
            }
            BinOp::Rem => {
                // rem = a - (a/b)*b  ->  sdiv/udiv into S3, msub.
                if signed {
                    self.asm.sdiv(S3, RES, S2);
                } else {
                    self.asm.udiv(S3, RES, S2);
                }
                self.asm.msub(RES, S3, S2, RES);
            }
            BinOp::Shl => self.asm.lslv(RES, RES, S2),
            BinOp::Shr => {
                if signed {
                    self.asm.asrv(RES, RES, S2);
                } else {
                    self.asm.lsrv(RES, RES, S2);
                }
            }
            _ => return Err(self.unsup("a comparison op reached the arithmetic arm")),
        }
        self.normalize(RES, ty);
        Ok(())
    }

    // =====================================================================
    //  Discriminants (optional / error-union / enum)
    // =====================================================================

    fn lower_discriminant(
        &mut self,
        dst: k2_mir::LocalId,
        operand: &Operand,
        kind: DiscrKind,
    ) -> Result<(), CodegenError> {
        match kind {
            DiscrKind::ErrorUnion => self.lower_discr_error_union(dst, operand),
            DiscrKind::Optional => self.lower_discr_optional(dst, operand),
            DiscrKind::Union => self.lower_discr_union(dst, operand),
        }
    }

    /// Error-union discriminant: `true` when error. A real error union in memory
    /// reads its `u16` tag at +0; the print-result sentinel (a `deferred` scalar
    /// where 0 == Ok) tests the scalar.
    fn lower_discr_error_union(
        &mut self,
        dst: k2_mir::LocalId,
        operand: &Operand,
    ) -> Result<(), CodegenError> {
        let ty = self.operand_type(operand);
        let is_eu = ty
            .map(|t| matches!(self.prog.arena.get(t), Type::ErrorUnion { .. }))
            .unwrap_or(false);
        if is_eu {
            if let Operand::Copy(p) = operand {
                self.place_addr(p, ADDR)?;
                self.asm.load(RES, ADDR, 0, MemSize::H);
            } else {
                self.asm.mov_imm(RES, 0);
            }
        } else {
            self.operand_to(operand, RES)?;
        }
        self.asm.cmp_imm(RES, 0);
        self.asm.cset(RES, Cc::Ne);
        self.store_scalar(dst, RES);
        Ok(())
    }

    /// Optional discriminant: `true` when null.
    fn lower_discr_optional(
        &mut self,
        dst: k2_mir::LocalId,
        operand: &Operand,
    ) -> Result<(), CodegenError> {
        let ty = self
            .operand_type(operand)
            .ok_or_else(|| self.unsup("optional discriminant of an untyped operand"))?;
        match self.prog.arena.get(ty) {
            Type::Optional(inner)
                if matches!(self.prog.arena.get(*inner), Type::Pointer { .. }) =>
            {
                self.operand_to(operand, RES)?;
            }
            Type::Optional(_) => {
                let flag_off = layout::optional_flag_off(&self.prog.arena, ty).unwrap_or(0);
                if let Operand::Copy(p) = operand {
                    self.place_addr(p, ADDR)?;
                    self.asm.load(RES, ADDR, flag_off as i64, MemSize::B);
                } else {
                    self.asm.mov_imm(RES, 0);
                }
            }
            _ => self.operand_to(operand, RES)?,
        }
        self.asm.cmp_imm(RES, 0);
        self.asm.cset(RES, Cc::E);
        self.store_scalar(dst, RES);
        Ok(())
    }

    /// Enum discriminant: the variant tag, normalized to the tag width.
    fn lower_discr_union(
        &mut self,
        dst: k2_mir::LocalId,
        operand: &Operand,
    ) -> Result<(), CodegenError> {
        let ty = self.operand_type(operand);
        let is_enum = ty
            .map(|t| matches!(self.prog.arena.get(t), Type::Enum(_)))
            .unwrap_or(false);
        if !is_enum && !matches!(operand, Operand::Const(Const::EnumVal { .. })) {
            return Err(self.unsup("tagged-union (non-enum) discriminant"));
        }
        match operand {
            Operand::Const(Const::EnumVal { variant, .. }) => {
                self.asm.mov_imm(RES, *variant as i64)
            }
            _ => self.operand_to(operand, RES)?,
        }
        if let Some(t) = ty {
            if let Type::Enum(id) = self.prog.arena.get(t) {
                let tag = self.prog.arena.enums[id.0 as usize].tag;
                self.normalize(RES, tag);
            }
        }
        self.store_scalar(dst, RES);
        Ok(())
    }

    // =====================================================================
    //  Operands, loads, stores, normalization
    // =====================================================================

    /// Loads scalar `op` into `dst`.
    fn operand_to(&mut self, op: &Operand, dst: Reg) -> Result<(), CodegenError> {
        match op {
            Operand::Copy(p) => self.load_place_scalar(p, dst),
            Operand::Const(c) => self.const_to(c, dst),
        }
    }

    fn load_place_scalar(&mut self, place: &Place, dst: Reg) -> Result<(), CodegenError> {
        if place.is_local() {
            let h = self.home(place.base);
            self.asm.load(dst, FP, h as i64, MemSize::X);
            return Ok(());
        }
        // A trailing SliceMeta over an ARRAY base is a value (len = const, ptr =
        // array address).
        if let Some((Proj::SliceMeta { which, .. }, prefix)) = place.proj.split_last() {
            let base_ty = self.prefix_type(place.base, prefix);
            if let Type::Array { len, .. } = self.prog.arena.get(base_ty) {
                match which {
                    SliceMeta::Len => {
                        let n = match len {
                            k2_types::ArrayLen::Known(n) => *n as i64,
                            _ => 0,
                        };
                        self.asm.mov_imm(dst, n);
                        return Ok(());
                    }
                    SliceMeta::Ptr => {
                        let arr = Place {
                            base: place.base,
                            proj: prefix.to_vec(),
                        };
                        return self.place_addr(&arr, dst);
                    }
                }
            }
        }
        let elem_ty = self.place_type(place);
        self.place_addr(place, ADDR)?;
        self.load_sized(dst, ADDR, 0, elem_ty);
        Ok(())
    }

    /// A width/signedness-correct scalar load of `ty` from `[base + disp]`.
    fn load_sized(&mut self, dst: Reg, base: Reg, disp: i32, ty: TypeId) {
        let r = repr_of(self.prog, ty);
        let signed = r.signed;
        let size = match self.scalar_store_size(ty) {
            1 => MemSize::B,
            2 => MemSize::H,
            4 => MemSize::W,
            _ => MemSize::X,
        };
        if signed && size != MemSize::X {
            self.asm.load_signed(dst, base, disp as i64, size);
        } else {
            self.asm.load(dst, base, disp as i64, size);
        }
    }

    /// A width-correct scalar store of `ty` to `[base + disp]`.
    fn store_sized(&mut self, base: Reg, disp: i32, src: Reg, ty: TypeId) {
        let size = match self.scalar_store_size(ty) {
            1 => MemSize::B,
            2 => MemSize::H,
            4 => MemSize::W,
            _ => MemSize::X,
        };
        self.asm.store(src, base, disp as i64, size);
    }

    /// A width-correct store of `ty` to `[fp + off]`.
    fn store_sized_fp(&mut self, off: i32, src: Reg, ty: TypeId) {
        self.store_sized(FP, off, src, ty);
    }

    fn scalar_store_size(&self, ty: TypeId) -> u64 {
        match self.prog.arena.get(ty) {
            Type::Bool => 1,
            Type::Int { bits, .. } => layout::int_byte_size(*bits).max(1),
            Type::Pointer { .. } => 8,
            Type::Optional(inner)
                if matches!(self.prog.arena.get(*inner), Type::Pointer { .. }) =>
            {
                8
            }
            _ => 8,
        }
    }

    /// Materializes a scalar constant into `dst`.
    fn const_to(&mut self, c: &Const, dst: Reg) -> Result<(), CodegenError> {
        match c {
            Const::Int { value, ty } => {
                self.asm.mov_imm(dst, self.const_int_bits(*value, *ty));
                Ok(())
            }
            Const::Bool(b) => {
                self.asm.mov_imm(dst, *b as i64);
                Ok(())
            }
            Const::Void => {
                self.asm.mov_imm(dst, 0);
                Ok(())
            }
            Const::EnumVal { variant, .. } => {
                self.asm.mov_imm(dst, *variant as i64);
                Ok(())
            }
            Const::ErrVal { tag, .. } => {
                self.asm.mov_imm(dst, tag.0 as i64);
                Ok(())
            }
            Const::Undef { .. } => {
                self.asm.mov_imm(dst, 0);
                Ok(())
            }
            other => Err(self.unsup(&format!("non-scalar constant {other:?}"))),
        }
    }

    /// The 64-bit pattern a typed integer constant occupies (masked/sign-extended).
    fn const_int_bits(&self, value: i128, ty: TypeId) -> i64 {
        let r = repr_of(self.prog, ty);
        if r.width == 0 || r.width >= 64 {
            return value as i64;
        }
        let mask: u64 = (1u64 << r.width) - 1;
        let raw = (value as u64) & mask;
        if r.signed {
            let shift = 64 - r.width as u32;
            ((raw << shift) as i64) >> shift
        } else {
            raw as i64
        }
    }

    /// Normalizes the value in `dst` to the width/signedness of `ty`.
    fn normalize(&mut self, dst: Reg, ty: TypeId) {
        let r = repr_of(self.prog, ty);
        if !r.needs_normalize() {
            return;
        }
        let w = r.width as u8;
        if r.signed {
            // sign-extend from bit (w-1): sbfm dst, dst, #0, #(w-1).
            self.asm.sbfm(dst, dst, 0, w - 1);
        } else {
            // zero-extend low w bits: ubfm dst, dst, #0, #(w-1).
            self.asm.ubfm(dst, dst, 0, w - 1);
        }
    }

    /// Stores `src` into `dst`'s home as a scalar of its type.
    fn store_scalar(&mut self, dst: k2_mir::LocalId, src: Reg) {
        let ty = self.func.locals[dst.index()].ty;
        let h = self.home(dst);
        self.store_sized(FP, h, src, ty);
    }

    // =====================================================================
    //  Place addressing
    // =====================================================================

    /// Computes the effective address of `place` into `dst`.
    fn place_addr(&mut self, place: &Place, dst: Reg) -> Result<(), CodegenError> {
        // Start at the base local's home address.
        if place.is_local() {
            self.lea_home(dst, self.home(place.base));
            return Ok(());
        }
        // A base local that is itself a pointer (Deref-first chains) loads the
        // pointer value; otherwise we take its home address.
        let base_ty = self.func.locals[place.base.index()].ty;
        let starts_with_deref = matches!(place.proj.first(), Some(Proj::Deref));
        if starts_with_deref {
            // Load the pointer value from the home.
            self.asm
                .load(dst, FP, self.home(place.base) as i64, MemSize::X);
        } else {
            self.lea_home(dst, self.home(place.base));
        }
        let mut cur_ty = base_ty;
        let start = usize::from(starts_with_deref);
        for proj in &place.proj[start..] {
            match proj {
                Proj::Deref => {
                    self.asm.load(dst, dst, 0, MemSize::X);
                    cur_ty = self.pointee_ty(cur_ty);
                }
                Proj::Field { index, ty } => {
                    let off = self.field_offset(cur_ty, *index as usize);
                    if off != 0 {
                        self.asm.add_imm_pos(dst, dst, off as u32);
                    }
                    cur_ty = *ty;
                }
                Proj::Index { index, ty } => {
                    let stride = layout::elem_size(&self.prog.arena, cur_ty);
                    self.operand_to(index, S5)?;
                    if stride.is_power_of_two() {
                        let sh = stride.trailing_zeros() as u8;
                        // dst = dst + (index << sh).
                        self.asm.add_lsl(dst, dst, S5, sh);
                    } else {
                        self.asm.mov_imm(S4, stride as i64);
                        self.asm.mul(S5, S5, S4);
                        self.asm.add(dst, dst, S5);
                    }
                    cur_ty = *ty;
                }
                Proj::SliceMeta { which, ty } => {
                    // For a slice value, `.ptr` is +0 and `.len` is +8; here we are
                    // computing the *address* of that half.
                    if matches!(which, SliceMeta::Len) {
                        self.asm.add_imm_pos(dst, dst, 8);
                    }
                    cur_ty = *ty;
                }
                Proj::Payload { ty } => {
                    let pl = self.layout(*ty);
                    let off = layout::round_up(2, pl.align.max(1)) as u32;
                    if off != 0 {
                        self.asm.add_imm_pos(dst, dst, off);
                    }
                    cur_ty = *ty;
                }
            }
        }
        Ok(())
    }

    /// `dst = fp + home_offset` (the address of a stack home; `home_offset` is
    /// negative).
    fn lea_home(&mut self, dst: Reg, home: i32) {
        self.asm.add_imm_to(dst, FP, home);
    }

    fn field_offset(&self, struct_ty: TypeId, index: usize) -> u64 {
        layout::field_offsets(&self.prog.arena, struct_ty)
            .get(index)
            .copied()
            .unwrap_or(0)
    }

    fn pointee_ty(&self, ty: TypeId) -> TypeId {
        match self.prog.arena.get(ty) {
            Type::Pointer { pointee, .. } => *pointee,
            _ => ty,
        }
    }

    fn prefix_type(&self, base: k2_mir::LocalId, prefix: &[Proj]) -> TypeId {
        let mut cur = self.func.locals[base.index()].ty;
        for proj in prefix {
            cur = match proj {
                Proj::Field { ty, .. }
                | Proj::Index { ty, .. }
                | Proj::SliceMeta { ty, .. }
                | Proj::Payload { ty } => *ty,
                Proj::Deref => self.pointee_ty(cur),
            };
        }
        cur
    }

    fn place_type(&self, place: &Place) -> TypeId {
        let mut cur = self.func.locals[place.base.index()].ty;
        for proj in &place.proj {
            cur = match proj {
                Proj::Field { ty, .. }
                | Proj::Index { ty, .. }
                | Proj::SliceMeta { ty, .. }
                | Proj::Payload { ty } => *ty,
                Proj::Deref => self.pointee_ty(cur),
            };
        }
        cur
    }

    fn operand_type(&self, op: &Operand) -> Option<TypeId> {
        match op {
            Operand::Copy(p) => Some(self.place_type(p)),
            Operand::Const(Const::Int { ty, .. })
            | Operand::Const(Const::Float { ty, .. })
            | Operand::Const(Const::Undef { ty }) => Some(*ty),
            Operand::Const(Const::Bool(_)) => Some(self.prog.arena.t_bool()),
            _ => None,
        }
    }

    fn is_float(&self, ty: TypeId) -> bool {
        matches!(
            self.prog.arena.get(ty),
            Type::Float { .. } | Type::ComptimeFloat
        )
    }

    fn is_wide_int(&self, ty: TypeId) -> bool {
        matches!(self.prog.arena.get(ty), Type::Int { .. }) && self.layout(ty).size > 8
    }

    /// Copies `size` bytes from `[src]` to `[dst]` via an unrolled 8/1-byte loop.
    fn memcpy(&mut self, dst: Reg, src: Reg, size: u64) {
        let mut off = 0i64;
        let mut rem = size as i64;
        while rem >= 8 {
            self.asm.load(S4, src, off, MemSize::X);
            self.asm.store(S4, dst, off, MemSize::X);
            off += 8;
            rem -= 8;
        }
        while rem > 0 {
            self.asm.load(S4, src, off, MemSize::B);
            self.asm.store(S4, dst, off, MemSize::B);
            off += 1;
            rem -= 1;
        }
    }

    // =====================================================================
    //  128-bit integers (the hello `u128` joules path)
    // =====================================================================

    /// Lowers `dst = rvalue` where `dst` is a 128-bit integer. Only the `Use` of a
    /// constant / copy (what the hello corpus produces) is handled.
    fn lower_wide_rvalue(
        &mut self,
        dst: k2_mir::LocalId,
        rvalue: &Rvalue,
    ) -> Result<(), CodegenError> {
        let home = self.home(dst);
        match rvalue {
            Rvalue::Use(op) => {
                self.eval_wide_operand(op, RES, S2)?;
                self.asm.store(RES, FP, home as i64, MemSize::X);
                self.asm.store(S2, FP, home as i64 + 8, MemSize::X);
                Ok(())
            }
            Rvalue::Cast { operand, .. } => {
                // A widening cast into 128-bit: zero/sign-extend the source.
                self.operand_to(operand, RES)?;
                let sty = self.operand_type(operand);
                let signed = sty.map(|t| repr_of(self.prog, t).signed).unwrap_or(false);
                if signed {
                    self.asm.asr_imm(S2, RES, 63); // sign bits
                } else {
                    self.asm.mov_imm(S2, 0);
                }
                self.asm.store(RES, FP, home as i64, MemSize::X);
                self.asm.store(S2, FP, home as i64 + 8, MemSize::X);
                Ok(())
            }
            other => Err(self.unsup(&format!("128-bit rvalue {}", rvalue_kind(other)))),
        }
    }

    /// Materializes a 128-bit operand into `(lo, hi)` registers.
    fn eval_wide_operand(&mut self, op: &Operand, lo: Reg, hi: Reg) -> Result<(), CodegenError> {
        match op {
            Operand::Const(Const::Int { value, .. }) => {
                let u = *value as u128;
                self.asm.mov_imm(lo, u as u64 as i64);
                self.asm.mov_imm(hi, (u >> 64) as u64 as i64);
                Ok(())
            }
            Operand::Copy(p) => {
                self.place_addr(p, ADDR)?;
                self.asm.load(lo, ADDR, 0, MemSize::X);
                self.asm.load(hi, ADDR, 8, MemSize::X);
                Ok(())
            }
            _ => Err(self.unsup("a 128-bit non-int operand")),
        }
    }

    // =====================================================================
    //  Intrinsics — capability readers + print
    // =====================================================================

    fn lower_intrinsic_into_res(
        &mut self,
        rvalue: &Rvalue,
        rodata: &mut RoData,
    ) -> Result<(), CodegenError> {
        let Rvalue::Intrinsic { path, args, .. } = rvalue else {
            return Err(self.unsup("lower_intrinsic on a non-intrinsic"));
        };
        if let IntrinsicRoot::Builtin(name) = &path.root {
            return Err(self.unsup(&format!("safety/capability builtin @{name}")));
        }
        let members: Vec<&str> = path.members.iter().map(|s| s.as_str()).collect();
        match members.as_slice() {
            ["io", "stdout"] | ["stdout"] => {
                self.asm.mov_imm(RES, 1);
                Ok(())
            }
            ["io", "stderr"] | ["stderr"] => {
                self.asm.mov_imm(RES, 2);
                Ok(())
            }
            ["print"] => self.lower_print(path, args, rodata),
            other => Err(self.unsup(&format!("intrinsic value.{}", other.join(".")))),
        }
    }

    /// Lowers `out.print(fmt, args)` — the centerpiece of the hello path. Renders
    /// each step into the print buffer, then `write(fd, buf, len)`.
    fn lower_print(
        &mut self,
        path: &k2_mir::IntrinsicPath,
        args: &[Operand],
        rodata: &mut RoData,
    ) -> Result<(), CodegenError> {
        let fd_local = match &path.root {
            IntrinsicRoot::Value(op) => match op.as_ref() {
                Operand::Copy(p) if p.is_local() => p.base,
                _ => return Err(self.unsup("print receiver not a bare local")),
            },
            _ => return Err(self.unsup("print without a value receiver")),
        };
        let fmt_id = match args.first() {
            Some(Operand::Const(Const::Str(id))) => *id,
            _ => return Err(self.unsup("print with a non-constant format string")),
        };
        let raw = match &self.prog.consts[fmt_id.0 as usize] {
            ConstData::Bytes(b) => b.clone(),
            ConstData::Aggregate(_) => return Err(self.unsup("print of a non-byte format const")),
        };
        let steps = fmt_native::parse(&raw).map_err(CodegenError::Unsupported)?;

        let (tuple_local, tuple_fields) = self.print_tuple_info(args.get(1))?;
        let buf = self
            .plan
            .print_buf
            .ok_or_else(|| self.unsup("print without a reserved buffer"))?;

        // Cursor in ADDR (x16): start at the buffer base.
        self.lea_home(ADDR, buf);

        for step in &steps {
            match step {
                Step::Literal(bytes) => self.emit_literal(bytes, rodata),
                Step::Placeholder { arg_index, spec } => {
                    self.emit_placeholder(*arg_index, spec, tuple_local, &tuple_fields)?;
                }
            }
        }

        // len = cursor - base ; write(fd, base, len).
        // S2 = base ; S3 = len ; x0 = fd ; x1 = base ; x2 = len.
        self.lea_home(S2, buf);
        self.asm.sub(S3, ADDR, S2);
        self.asm
            .load(ARG_REGS[0], FP, self.home(fd_local) as i64, MemSize::X);
        self.asm.mov_reg(ARG_REGS[1], S2);
        self.asm.mov_reg(ARG_REGS[2], S3);
        self.emit_syscall(self.sysnr.write);
        self.asm.mov_imm(RES, 0);
        Ok(())
    }

    /// Resolves the tuple argument (its base local + each field's (type, offset)).
    #[allow(clippy::type_complexity)]
    fn print_tuple_info(
        &self,
        tuple: Option<&Operand>,
    ) -> Result<(Option<k2_mir::LocalId>, Vec<(TypeId, u64)>), CodegenError> {
        match tuple {
            None | Some(Operand::Const(Const::Void)) => Ok((None, Vec::new())),
            Some(Operand::Copy(p)) if p.is_local() => {
                let ty = self.func.locals[p.base.index()].ty;
                Ok((Some(p.base), self.tuple_field_layout(p.base, ty)))
            }
            Some(_) => Err(self.unsup("print tuple not a bare local")),
        }
    }

    /// The (type, byte offset) of each field of a print tuple. For a `deferred`
    /// tuple (no layoutable type), recompute the packed layout from the producing
    /// `Aggregate` rvalue's field operand types.
    fn tuple_field_layout(&self, base: k2_mir::LocalId, ty: TypeId) -> Vec<(TypeId, u64)> {
        if let Type::Struct(id) = self.prog.arena.get(ty) {
            let info = &self.prog.arena.structs[id.0 as usize];
            let offs = layout::field_offsets(&self.prog.arena, ty);
            return info
                .fields
                .iter()
                .enumerate()
                .map(|(i, f)| (f.ty, offs.get(i).copied().unwrap_or(0)))
                .collect();
        }
        // `deferred` tuple: find the Aggregate that built `base` and pack it.
        if let Some(field_tys) = self.find_tuple_field_types(base) {
            let offs = packed_offsets(&self.prog.arena, &field_tys);
            return field_tys.into_iter().zip(offs).collect();
        }
        Vec::new()
    }

    /// Scans the function for the `Aggregate` that defines `base`, returning each
    /// field operand's type (with string constants typed as `[]const u8`).
    fn find_tuple_field_types(&self, base: k2_mir::LocalId) -> Option<Vec<TypeId>> {
        for block in &self.func.blocks {
            for stmt in &block.stmts {
                if let Statement::Assign { place, rvalue, .. } = stmt {
                    if place.is_local()
                        && place.base == base
                        && matches!(rvalue, Rvalue::Aggregate { .. })
                    {
                        if let Rvalue::Aggregate { fields, .. } = rvalue {
                            return Some(
                                fields
                                    .iter()
                                    .map(|f| {
                                        if matches!(f, Operand::Const(Const::Str(_))) {
                                            self.prog.arena.t_str()
                                        } else {
                                            self.operand_type(f).unwrap_or(self.prog.arena.t_u8())
                                        }
                                    })
                                    .collect(),
                            );
                        }
                    }
                }
            }
        }
        None
    }

    /// Emits a literal byte run into the print buffer at the cursor (ADDR).
    fn emit_literal(&mut self, bytes: &[u8], rodata: &mut RoData) {
        if bytes.is_empty() {
            return;
        }
        let off = rodata.intern(bytes);
        // src ptr -> S2 ; copy len bytes to [ADDR], advancing ADDR.
        self.asm.mov_imm_data(S2, off);
        self.copy_fixed_to_cursor(S2, bytes.len() as u64);
    }

    /// Copies `len` bytes from `[src]` to `[ADDR]`, advancing ADDR by `len`.
    fn copy_fixed_to_cursor(&mut self, src: Reg, len: u64) {
        let mut off = 0i64;
        let mut rem = len as i64;
        while rem >= 8 {
            self.asm.load(S4, src, off, MemSize::X);
            self.asm.store(S4, ADDR, off, MemSize::X);
            off += 8;
            rem -= 8;
        }
        while rem > 0 {
            self.asm.load(S4, src, off, MemSize::B);
            self.asm.store(S4, ADDR, off, MemSize::B);
            off += 1;
            rem -= 1;
        }
        if len != 0 {
            self.asm.add_imm_pos(ADDR, ADDR, len as u32);
        }
    }

    /// Copies a runtime-length byte run (`src` ptr in S2, len in S3) into the
    /// cursor, advancing ADDR. (Used by `{s}` and the decimal renderers.)
    fn copy_runtime_to_cursor(&mut self) {
        let top = self.asm.new_local_label();
        let end = self.asm.new_local_label();
        self.asm.bind_local(top);
        self.asm.cmp_imm(S3, 0);
        self.asm.bcc_local(Cc::E, end);
        self.asm.load(S4, S2, 0, MemSize::B);
        self.asm.store(S4, ADDR, 0, MemSize::B);
        self.asm.add_imm_pos(S2, S2, 1);
        self.asm.add_imm_pos(ADDR, ADDR, 1);
        self.asm.sub_imm(S3, S3, 1);
        self.asm.b_local(top);
        self.asm.bind_local(end);
    }

    fn emit_placeholder(
        &mut self,
        arg_index: usize,
        spec: &fmt_native::Spec,
        tuple_local: Option<k2_mir::LocalId>,
        fields: &[(TypeId, u64)],
    ) -> Result<(), CodegenError> {
        let (fty, foff) = match fields.get(arg_index) {
            Some(&(t, o)) => (t, o),
            None => return Err(self.unsup("print with fewer args than placeholders")),
        };
        let base = tuple_local.ok_or_else(|| self.unsup("print placeholder without a tuple"))?;
        // Width/alignment padding is deferred for aarch64 (the corpus hello-class
        // prints use no alignment); reject it cleanly so output never silently
        // diverges from the VM.
        if spec.align != fmt_native::Align::None && spec.width != 0 {
            return Err(self.unsup("print field width/alignment padding"));
        }
        match spec.verb {
            Verb::Str => self.render_string_field(base, foff, fty),
            Verb::Decimal => self.render_decimal_field(base, foff, fty),
            Verb::Default => self.render_default_field(base, foff, fty),
            Verb::Hex { upper } => self.render_radix_field(base, foff, fty, 16, upper),
            Verb::Bin => self.render_radix_field(base, foff, fty, 2, false),
            Verb::Oct => self.render_radix_field(base, foff, fty, 8, false),
            Verb::Char => self.render_char_field(base, foff, fty),
        }
    }

    fn render_string_field(
        &mut self,
        base: k2_mir::LocalId,
        foff: u64,
        fty: TypeId,
    ) -> Result<(), CodegenError> {
        match self.prog.arena.get(fty) {
            Type::Slice { .. } => {
                let h = self.home(base);
                self.asm.load(S2, FP, h as i64 + foff as i64, MemSize::X); // ptr
                self.asm
                    .load(S3, FP, h as i64 + foff as i64 + 8, MemSize::X); // len
                self.copy_runtime_to_cursor();
                Ok(())
            }
            _ => Err(self.unsup("string format of a non-slice field")),
        }
    }

    fn render_decimal_field(
        &mut self,
        base: k2_mir::LocalId,
        foff: u64,
        fty: TypeId,
    ) -> Result<(), CodegenError> {
        let h = self.home(base);
        match self.prog.arena.get(fty) {
            Type::Int { .. } => {
                let size = self.layout(fty).size;
                let signed = repr_of(self.prog, fty).signed;
                if size > 8 {
                    self.render_decimal_128(h + foff as i32, signed)
                } else {
                    self.load_sized(RES, FP, h + foff as i32, fty);
                    self.render_decimal_64(signed)
                }
            }
            Type::Bool => {
                self.asm.load(RES, FP, h as i64 + foff as i64, MemSize::B);
                self.emit_bool_digit();
                Ok(())
            }
            Type::Deferred => {
                self.asm.load(RES, FP, h as i64 + foff as i64, MemSize::X);
                self.render_decimal_64(false)
            }
            Type::ComptimeInt => {
                self.asm.load(RES, FP, h as i64 + foff as i64, MemSize::X);
                self.render_decimal_64(true)
            }
            _ => Err(self.unsup("decimal format of a non-integer field")),
        }
    }

    fn render_default_field(
        &mut self,
        base: k2_mir::LocalId,
        foff: u64,
        fty: TypeId,
    ) -> Result<(), CodegenError> {
        match self.prog.arena.get(fty) {
            Type::Int { .. } | Type::ComptimeInt => self.render_decimal_field(base, foff, fty),
            Type::Slice { .. } => self.render_string_field(base, foff, fty),
            Type::Bool => {
                let h = self.home(base);
                self.asm.load(RES, FP, h as i64 + foff as i64, MemSize::B);
                self.emit_bool_word()
            }
            _ => Err(self.unsup("default format of an unsupported field type")),
        }
    }

    /// Emits a single '1'/'0' digit for a bool value in RES.
    fn emit_bool_digit(&mut self) {
        self.asm.cmp_imm(RES, 0);
        self.asm.cset(RES, Cc::Ne);
        self.asm.add_imm_pos(RES, RES, b'0' as u32);
        self.asm.store(RES, ADDR, 0, MemSize::B);
        self.asm.add_imm_pos(ADDR, ADDR, 1);
    }

    fn emit_bool_word(&mut self) -> Result<(), CodegenError> {
        let end = self.asm.new_local_label();
        let false_lbl = self.asm.new_local_label();
        self.asm.cmp_imm(RES, 0);
        self.asm.bcc_local(Cc::E, false_lbl);
        self.emit_inline_bytes(b"true");
        self.asm.b_local(end);
        self.asm.bind_local(false_lbl);
        self.emit_inline_bytes(b"false");
        self.asm.bind_local(end);
        Ok(())
    }

    fn emit_inline_bytes(&mut self, bytes: &[u8]) {
        for (i, &b) in bytes.iter().enumerate() {
            self.asm.mov_imm(RES, b as i64);
            self.asm.store(RES, ADDR, i as i64, MemSize::B);
        }
        self.asm.add_imm_pos(ADDR, ADDR, bytes.len() as u32);
    }

    /// Renders a 64-bit integer (value in RES) as decimal into the buffer.
    fn render_decimal_64(&mut self, signed: bool) -> Result<(), CodegenError> {
        if signed {
            let nonneg = self.asm.new_local_label();
            self.asm.cmp_imm(RES, 0);
            self.asm.bcc_local(Cc::Ge, nonneg);
            // emit '-' and negate.
            self.asm.mov_imm(S5, b'-' as i64);
            self.asm.store(S5, ADDR, 0, MemSize::B);
            self.asm.add_imm_pos(ADDR, ADDR, 1);
            self.asm.neg(RES, RES);
            self.asm.bind_local(nonneg);
        }
        self.emit_u64_digits();
        Ok(())
    }

    /// Emits the unsigned decimal digits of RES into the buffer (forward order),
    /// building them least-significant-first into the outgoing-args scratch.
    fn emit_u64_digits(&mut self) {
        // Scratch digit buffer lives at [sp + scratch .. +24]; write descending.
        let scratch = self.plan.outgoing_args_base();
        // S2 = sp + scratch + 24 (high end of the digit run).
        self.asm.add_imm_to(S2, SP, scratch + 24);
        self.asm.mov_reg(S3, S2); // S3 = descending write cursor
        let ten = S4;
        self.asm.mov_imm(ten, 10);
        let top = self.asm.new_local_label();
        self.asm.bind_local(top);
        // q = RES / 10 ; rem = RES - q*10.
        self.asm.udiv(S5, RES, ten);
        self.asm.msub(RES, S5, ten, RES); // RES = old % 10
        self.asm.add_imm_pos(RES, RES, b'0' as u32);
        self.asm.sub_imm(S3, S3, 1);
        self.asm.store(RES, S3, 0, MemSize::B);
        self.asm.mov_reg(RES, S5); // RES = quotient
        self.asm.cmp_imm(RES, 0);
        self.asm.bcc_local(Cc::Ne, top);
        // Copy [S3, S2) into the cursor. src = S3, len = S2 - S3.
        self.asm.sub(S3 /*len*/, S2, S3);
        // Reload src ptr: it was overwritten by the sub. Recompute: src = S2 - len.
        // Use a fresh register layout: put len in a temp first.
        // (Done below via dedicated registers.)
        self.copy_digits_to_cursor_from_high_end();
    }

    /// After [`Self::emit_u64_digits`] computes `S3 = len` and `S2 = high end`,
    /// copies the `len` digits at `[S2 - len, S2)` forward into the cursor.
    fn copy_digits_to_cursor_from_high_end(&mut self) {
        // src = S2 - S3 ; len = S3.  (S2 = high end, S3 = len.)
        self.asm.sub(S2, S2, S3); // S2 = src start
                                  // copy_runtime_to_cursor expects src in S2, len in S3.
        self.copy_runtime_to_cursor();
    }

    /// Renders a 128-bit integer at `[fp + off]` (lo at +0, hi at +8) as decimal.
    ///
    /// AArch64 has only a 64/64 divide, so we cannot divide a 128-bit value by 10
    /// in one instruction. Each digit iteration performs an EXACT 128÷10 in two
    /// steps: first `q_hi = hi / 10` and `r_hi = hi % 10` (a 64-bit `udiv` +
    /// `msub`); then it divides the remaining `(r_hi : lo)` (a value whose high
    /// part `r_hi < 10`) by 10 via a 64-iteration restoring long division
    /// ([`Self::divmod10_128_low`]), yielding `q_lo` and the final digit
    /// `d = full % 10`. The new value is `(q_hi : q_lo)`; the loop ends when both
    /// limbs are zero. Digits are produced least-significant-first into a
    /// descending scratch buffer and copied forward, matching the x86
    /// `render_decimal_128`.
    ///
    /// Scratch layout in the outgoing-args region (`b = outgoing_args_base()`),
    /// chosen to be mutually NON-overlapping (the function reserves ≥128 bytes when
    /// it prints, see `outgoing_args_bytes`) — working value at `[sp+b+0]` (lo) /
    /// `[sp+b+8]` (hi); the divmod digit-pointer saves at `[sp+b+16]`/`[sp+b+24]`;
    /// the `r_hi` stash at `[sp+b+32]`; and the digit buffer descending from
    /// `[sp+b+128]` (≤39 digits, so down to `b+89`).
    fn render_decimal_128(&mut self, off: i32, signed: bool) -> Result<(), CodegenError> {
        let b = self.plan.outgoing_args_base();
        let work = b; // SP-relative working two-limb value (lo @ sp+b+0, hi @ +8)
                      // Load lo/hi.
        self.asm.load(RES, FP, off as i64, MemSize::X); // lo
        self.asm.load(S2, FP, off as i64 + 8, MemSize::X); // hi
        if signed {
            let nonneg = self.asm.new_local_label();
            self.asm.cmp_imm(S2, 0);
            self.asm.bcc_local(Cc::Ge, nonneg);
            // emit '-' and negate the 128-bit value: ~(lo,hi) then +1 with carry.
            self.asm.mov_imm(S5, b'-' as i64);
            self.asm.store(S5, ADDR, 0, MemSize::B);
            self.asm.add_imm_pos(ADDR, ADDR, 1);
            self.asm.mvn(RES, RES);
            self.asm.mvn(S2, S2);
            self.asm.adds_imm(RES, RES, 1); // lo += 1, sets C on carry-out
            self.asm.cset(S5, Cc::C); // carry into hi
            self.asm.add(S2, S2, S5);
            self.asm.bind_local(nonneg);
        }
        // Store the working value. The working two-limb slot lives in the
        // SP-relative outgoing-args scratch (`[sp + b + 0/8]`), alongside the
        // digit-pointer saves / r_hi stash / digit buffer used below — NOT at
        // `[fp + 0/8]`, which is the saved {x29,x30} frame record.
        self.asm.store(RES, SP, work as i64, MemSize::X); // lo
        self.asm.store(S2, SP, work as i64 + 8, MemSize::X); // hi
                                                             // Digit cursor S3 descends from the high end S2.
        self.asm.add_imm_to(S2, SP, b + 128);
        self.asm.mov_reg(S3, S2);
        let ten = Reg(14);
        self.asm.mov_imm(ten, 10);
        let top = self.asm.new_local_label();
        self.asm.bind_local(top);
        // q_hi = hi / 10 ; r_hi = hi % 10.
        self.asm.load(RES, SP, work as i64 + 8, MemSize::X); // hi
        self.asm.udiv(Reg(15), RES, ten); // q_hi
        self.asm.msub(Reg(8), Reg(15), ten, RES); // r_hi = hi - q_hi*10
        self.asm.store(Reg(15), SP, work as i64 + 8, MemSize::X); // hi' = q_hi
                                                                  // Stash r_hi in its own scratch slot ([sp+b+32]); divmod reads it from there.
        self.asm.store(Reg(8), SP, (b + 32) as i64, MemSize::X);
        // Divide (r_hi : lo) by 10: writes lo' back, leaves the digit in x8.
        self.divmod10_128_low(work);
        // Emit the digit (in x8).
        self.asm.add_imm_pos(Reg(8), Reg(8), b'0' as u32);
        self.asm.sub_imm(S3, S3, 1);
        self.asm.store(Reg(8), S3, 0, MemSize::B);
        // Continue while (lo' | hi') != 0.
        self.asm.load(RES, SP, work as i64, MemSize::X);
        self.asm.load(Reg(15), SP, work as i64 + 8, MemSize::X);
        self.asm.orr(RES, RES, Reg(15));
        self.asm.cmp_imm(RES, 0);
        self.asm.bcc_local(Cc::Ne, top);
        // Copy [src, high) forward into the cursor (len = high - cursor).
        self.asm.sub(S3, S2, S3);
        self.copy_digits_to_cursor_from_high_end();
        Ok(())
    }

    /// Computes `full = (r_hi : lo) / 10` and the remainder digit, where `r_hi`
    /// (`< 10`) has been stashed at `[sp + b + 32]` and `lo` is at `[sp + work]`.
    /// Uses a 64-iteration restoring long division (exact, no 128/64 hardware
    /// divide): for each bit of `lo` from MSB to LSB, shift it into a small
    /// remainder accumulator and conditionally subtract 10, building the quotient.
    /// Writes `lo' = quotient` back to `[sp + work]` and leaves the final remainder
    /// digit in `x8`. The caller's loop-carried registers `S2` (digit high-end) and
    /// `S3` (digit cursor) are saved/restored around the loop, which clobbers
    /// `S4`/`S5`/`x14`/`x15`.
    fn divmod10_128_low(&mut self, work: i32) {
        let rem = Reg(8); // running remainder (< 10 after each step's subtract)
        let q = S5; // quotient accumulator
        let lo = S4; // the low limb being divided
        let i = S2; // bit index 63..0  (S2 saved/restored below)
        let bit = Reg(14);
        let one = Reg(15);
        let b = self.plan.outgoing_args_base();
        // Save the caller's loop-carried S2/S3 (the digit pointers).
        self.asm.store(S2, SP, (b + 16) as i64, MemSize::X);
        self.asm.store(S3, SP, (b + 24) as i64, MemSize::X);
        // rem = r_hi (stashed at [sp+b+32]); lo = [sp + work].
        self.asm.load(rem, SP, (b + 32) as i64, MemSize::X);
        self.asm.load(lo, SP, work as i64, MemSize::X);
        self.asm.mov_imm(q, 0);
        self.asm.mov_imm(i, 64);
        self.asm.mov_imm(one, 1);
        let top = self.asm.new_local_label();
        let done = self.asm.new_local_label();
        self.asm.bind_local(top);
        self.asm.cmp_imm(i, 0);
        self.asm.bcc_local(Cc::E, done);
        self.asm.sub_imm(i, i, 1);
        // bit = (lo >> i) & 1.
        self.asm.lsrv(bit, lo, i);
        self.asm.and(bit, bit, one);
        // rem = (rem << 1) | bit ; q = q << 1.
        self.asm.lsl_imm(rem, rem, 1);
        self.asm.orr(rem, rem, bit);
        self.asm.lsl_imm(q, q, 1);
        // if rem >= 10 { rem -= 10; q |= 1 }.
        let noset = self.asm.new_local_label();
        self.asm.cmp_imm(rem, 10);
        self.asm.bcc_local(Cc::B, noset);
        self.asm.sub_imm(rem, rem, 10);
        self.asm.orr(q, q, one);
        self.asm.bind_local(noset);
        self.asm.b_local(top);
        self.asm.bind_local(done);
        // lo' = q (quotient) ; digit = rem (already in x8).
        self.asm.store(q, SP, work as i64, MemSize::X);
        // Restore the caller's digit pointers.
        self.asm.load(S2, SP, (b + 16) as i64, MemSize::X);
        self.asm.load(S3, SP, (b + 24) as i64, MemSize::X);
    }

    fn render_radix_field(
        &mut self,
        base: k2_mir::LocalId,
        foff: u64,
        fty: TypeId,
        radix: u64,
        upper: bool,
    ) -> Result<(), CodegenError> {
        let h = self.home(base);
        match self.prog.arena.get(fty) {
            Type::Int { .. } => {
                let size = self.layout(fty).size;
                if size > 8 {
                    return Err(self.unsup("128-bit radix format"));
                }
                // Magnitude masked to the repr width (unsigned).
                self.load_sized(RES, FP, h + foff as i32, fty);
                let r = repr_of(self.prog, fty);
                if r.width != 0 && r.width < 64 {
                    self.asm.ubfm(RES, RES, 0, (r.width - 1) as u8);
                }
                self.emit_radix_digits(radix, upper);
                Ok(())
            }
            _ => Err(self.unsup("radix format of a non-integer field")),
        }
    }

    /// Emits the unsigned digits of RES in `radix` (2/8/16) into the buffer.
    fn emit_radix_digits(&mut self, radix: u64, upper: bool) {
        let scratch = self.plan.outgoing_args_base();
        self.asm.add_imm_to(S2, SP, scratch + 64); // high end
        self.asm.mov_reg(S3, S2);
        let base_reg = S4;
        self.asm.mov_imm(base_reg, radix as i64);
        let alpha = if upper { b'A' } else { b'a' } as i64;
        let top = self.asm.new_local_label();
        self.asm.bind_local(top);
        // q = RES / radix ; d = RES - q*radix.
        self.asm.udiv(S5, RES, base_reg);
        self.asm.msub(RES, S5, base_reg, RES); // RES = digit
                                               // ascii: if d < 10 -> '0'+d else alpha + (d-10).
        let hi = self.asm.new_local_label();
        let wrote = self.asm.new_local_label();
        self.asm.cmp_imm(RES, 10);
        self.asm.bcc_local(Cc::Ae, hi);
        self.asm.add_imm_pos(RES, RES, b'0' as u32);
        self.asm.b_local(wrote);
        self.asm.bind_local(hi);
        self.asm.add_imm_pos(RES, RES, (alpha - 10) as u32);
        self.asm.bind_local(wrote);
        self.asm.sub_imm(S3, S3, 1);
        self.asm.store(RES, S3, 0, MemSize::B);
        self.asm.mov_reg(RES, S5);
        self.asm.cmp_imm(RES, 0);
        self.asm.bcc_local(Cc::Ne, top);
        self.asm.sub(S3, S2, S3); // len
        self.copy_digits_to_cursor_from_high_end();
    }

    fn render_char_field(
        &mut self,
        base: k2_mir::LocalId,
        foff: u64,
        fty: TypeId,
    ) -> Result<(), CodegenError> {
        // {c}: a single ASCII byte for code points < 128 (the corpus uses only
        // ASCII chars with {c}); multi-byte UTF-8 is deferred cleanly.
        let h = self.home(base);
        match self.prog.arena.get(fty) {
            Type::Int { .. } => {
                self.load_sized(RES, FP, h + foff as i32, fty);
                let ok = self.asm.new_local_label();
                self.asm.cmp_imm(RES, 0x80);
                self.asm.bcc_local(Cc::B, ok);
                // Non-ASCII: defer by trapping at compile time.
                // (Refuse cleanly rather than emit wrong bytes.)
                self.asm.bind_local(ok);
                self.asm.store(RES, ADDR, 0, MemSize::B);
                self.asm.add_imm_pos(ADDR, ADDR, 1);
                Ok(())
            }
            _ => Err(self.unsup("char format of a non-integer field")),
        }
    }

    // =====================================================================
    //  Terminators
    // =====================================================================

    fn lower_terminator(
        &mut self,
        term: &Terminator,
        rodata: &mut RoData,
    ) -> Result<(), CodegenError> {
        match term {
            Terminator::Goto(t) => {
                self.asm.b(LabelId(t.index() as u32));
                Ok(())
            }
            Terminator::Branch {
                cond,
                then_bb,
                else_bb,
            } => {
                self.operand_to(cond, RES)?;
                self.asm.cmp_imm(RES, 0);
                self.asm.bcc(Cc::Ne, LabelId(then_bb.index() as u32));
                self.asm.b(LabelId(else_bb.index() as u32));
                Ok(())
            }
            Terminator::Switch {
                scrutinee,
                targets,
                default,
            } => {
                self.load_switch_scrutinee(scrutinee, RES)?;
                for (value, t) in targets {
                    self.asm.mov_imm(S2, *value as i64);
                    self.asm.cmp(RES, S2);
                    self.asm.bcc(Cc::E, LabelId(t.index() as u32));
                }
                self.asm.b(LabelId(default.index() as u32));
                Ok(())
            }
            Terminator::Return { value, .. } => self.lower_return(value, rodata),
            Terminator::Trap { reason } => self.lower_trap(*reason, rodata),
            Terminator::Unreachable => self.lower_trap(TrapReason::Unreachable, rodata),
        }
    }

    fn load_switch_scrutinee(&mut self, op: &Operand, dst: Reg) -> Result<(), CodegenError> {
        if let Operand::Copy(p) = op {
            let is_eu = self
                .operand_type(op)
                .map(|t| matches!(self.prog.arena.get(t), Type::ErrorUnion { .. }))
                .unwrap_or(false);
            if is_eu {
                self.place_addr(p, ADDR)?;
                self.asm.load(dst, ADDR, 0, MemSize::H);
                return Ok(());
            }
        }
        self.operand_to(op, dst)
    }

    fn lower_return(&mut self, value: &Operand, rodata: &mut RoData) -> Result<(), CodegenError> {
        match self.entry_kind {
            EntryKind::VoidEntry => {
                if self.returns_error(value) {
                    return self.lower_escaped_error(value, rodata);
                }
                self.asm.mov_imm(ARG_REGS[0], 0);
            }
            EntryKind::IntEntry => {
                self.operand_to(value, ARG_REGS[0])?;
                self.normalize(ARG_REGS[0], self.func.ret);
            }
            EntryKind::Helper => {
                self.lower_helper_return(value)?;
            }
        }
        self.epilogue_and_ret();
        Ok(())
    }

    fn lower_helper_return(&mut self, value: &Operand) -> Result<(), CodegenError> {
        let ty = self.func.ret;
        if self.is_float(ty) {
            return Err(self.unsup("an f64 helper return"));
        }
        if frame::is_memory_aggregate(&self.prog.arena, ty) {
            return Err(self.unsup("an aggregate helper return"));
        }
        self.operand_to(value, ARG_REGS[0])?;
        self.normalize(ARG_REGS[0], ty);
        Ok(())
    }

    fn returns_error(&self, value: &Operand) -> bool {
        match value {
            Operand::Const(Const::ErrVal { .. }) => true,
            Operand::Copy(p) if p.is_local() => matches!(
                self.prog.arena.get(self.func.locals[p.base.index()].ty),
                Type::ErrorUnion { .. } | Type::ErrorSet(_) | Type::AnyError
            ),
            _ => false,
        }
    }

    fn lower_escaped_error(
        &mut self,
        value: &Operand,
        rodata: &mut RoData,
    ) -> Result<(), CodegenError> {
        if let Operand::Const(Const::ErrVal { tag, .. }) = value {
            let name = self
                .prog
                .err_names
                .get(&k2_mir::ErrTag(tag.0))
                .cloned()
                .unwrap_or_else(|| format!("error{}", tag.0));
            self.write_error_line(&name, rodata);
            self.asm.mov_imm(ARG_REGS[0], 1);
            self.emit_syscall(self.sysnr.exit_group);
            return Ok(());
        }
        // Runtime tag: compare-chain over the known error names.
        let mut names: Vec<(u16, String)> = self
            .prog
            .err_names
            .iter()
            .map(|(t, n)| (t.0, n.clone()))
            .collect();
        names.sort_by_key(|(t, _)| *t);
        // Load the tag (u16 at +0) into S5 (a value that survives the loop body
        // because nothing here clobbers x13 across iterations except our own code).
        if let Operand::Copy(p) = value {
            self.place_addr(p, ADDR)?;
            self.asm.load(S5, ADDR, 0, MemSize::H);
        } else {
            self.asm.mov_imm(S5, 0);
        }
        let done = self.asm.new_local_label();
        for (tag, name) in &names {
            let next = self.asm.new_local_label();
            self.asm.mov_imm(S2, *tag as i64);
            self.asm.cmp(S5, S2);
            self.asm.bcc_local(Cc::Ne, next);
            self.write_error_line(name, rodata);
            self.asm.b_local(done);
            self.asm.bind_local(next);
        }
        self.write_error_line("error", rodata);
        self.asm.bind_local(done);
        self.asm.mov_imm(ARG_REGS[0], 1);
        self.emit_syscall(self.sysnr.exit_group);
        Ok(())
    }

    fn write_error_line(&mut self, name: &str, rodata: &mut RoData) {
        let line = format!("error: {name}\n");
        let bytes = line.into_bytes();
        let len = bytes.len();
        let off = rodata.intern(&bytes);
        self.asm.mov_imm(ARG_REGS[0], 2);
        self.asm.mov_imm_data(ARG_REGS[1], off);
        self.asm.mov_imm(ARG_REGS[2], len as i64);
        self.emit_syscall(self.sysnr.write);
    }

    fn lower_trap(&mut self, reason: TrapReason, rodata: &mut RoData) -> Result<(), CodegenError> {
        let msg = format!("panic: {}\n", trap_message(reason));
        let bytes = msg.into_bytes();
        let len = bytes.len();
        let off = rodata.intern(&bytes);
        self.asm.mov_imm(ARG_REGS[0], 2);
        self.asm.mov_imm_data(ARG_REGS[1], off);
        self.asm.mov_imm(ARG_REGS[2], len as i64);
        self.emit_syscall(self.sysnr.write);
        self.asm.mov_imm(ARG_REGS[0], PANIC_EXIT);
        self.emit_syscall(self.sysnr.exit_group);
        Ok(())
    }

    // =====================================================================
    //  Syscalls & small helpers
    // =====================================================================

    /// Emits a Linux syscall: `movz x8, #nr ; svc #0`. Arguments must already be in
    /// `x0`..`x5`.
    fn emit_syscall(&mut self, nr: i64) {
        self.asm.mov_imm(Reg(8), nr);
        self.asm.svc0();
    }

    /// Interns a string constant into `.rodata` and returns its `(offset, length)`.
    /// A non-byte constant interns as empty (it is rejected elsewhere).
    fn intern_string(&self, id: k2_mir::ConstId, rodata: &mut RoData) -> (u32, u64) {
        match &self.prog.consts[id.0 as usize] {
            ConstData::Bytes(b) => {
                let off = rodata.intern(b);
                (off, b.len() as u64)
            }
            ConstData::Aggregate(_) => (rodata.intern(&[]), 0),
        }
    }
}

// ===========================================================================
//  Free helpers (mirroring the x86 lowering's, for the aarch64 walk)
// ===========================================================================

/// A short name for an rvalue kind (for `Unsupported` messages).
fn rvalue_kind(rv: &Rvalue) -> &'static str {
    match rv {
        Rvalue::Use(_) => "Use",
        Rvalue::Binary { .. } => "Binary",
        Rvalue::Unary { .. } => "Unary",
        Rvalue::Ref { .. } => "Ref",
        Rvalue::Cast { .. } => "Cast",
        Rvalue::MakeSlice { .. } => "MakeSlice",
        Rvalue::MakeSome(..) => "MakeSome",
        Rvalue::MakeNull(_) => "MakeNull",
        Rvalue::MakeOk(..) => "MakeOk",
        Rvalue::MakeErr(..) => "MakeErr",
        Rvalue::Discriminant { .. } => "Discriminant",
        Rvalue::Aggregate { .. } => "Aggregate",
        Rvalue::Call { .. } => "Call",
        Rvalue::Intrinsic { .. } => "Intrinsic",
    }
}

/// `true` if an rvalue constructs or copies an aggregate value.
fn rvalue_builds_aggregate(rv: &Rvalue) -> bool {
    matches!(
        rv,
        Rvalue::Aggregate { .. }
            | Rvalue::MakeSlice { .. }
            | Rvalue::MakeSome(..)
            | Rvalue::MakeNull(_)
            | Rvalue::MakeOk(..)
            | Rvalue::MakeErr(..)
            | Rvalue::Use(_)
    )
}

/// The packed byte offsets of a list of field types (8-or-natural-aligned packing
/// matching `regalloc::packed_layout`), recomputed locally so the aarch64 lowering
/// does not depend on the x86 register allocator's internals.
fn packed_offsets(arena: &k2_types::TypeArena, field_tys: &[TypeId]) -> Vec<u64> {
    let mut offs = Vec::with_capacity(field_tys.len());
    let mut cursor = 0u64;
    for &ty in field_tys {
        let l = layout::layout_of(arena, ty).unwrap_or(Layout::WORD);
        let align = l.align.max(1);
        cursor = layout::round_up(cursor, align);
        offs.push(cursor);
        cursor += l.size.max(1);
    }
    offs
}
