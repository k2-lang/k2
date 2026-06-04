//! MIR -> x86-64 machine-code lowering for the v0.14 subset.
//!
//! Each [`MirFunction`] is lowered to a self-contained block of machine code
//! through a simple stack-machine scheme (correctness over speed — register
//! allocation is v0.15):
//!
//! * Every MIR local gets one 8-byte `[rbp - 8*(i+1)]` stack slot.
//! * Every rvalue computes into **RAX** (LHS / accumulator), using **RCX** for a
//!   binary's right-hand operand, then stores RAX into the destination slot.
//! * Each result is normalized to its type's width (sign-extend for signed
//!   sub-64 types, AND-mask for unsigned ones) so narrow integers wrap exactly
//!   as the VM does.
//! * Terminators become `jmp`/`jcc`/compare-chains/`leave;ret`/syscalls.
//!
//! Two intrinsic shapes are recognized so a program can produce observable
//! output: `value(_).io.stdout()`/`.io.stderr()` yields a writer **fd token**
//! (the constant `1` or `2`), and `value(fd).print(str, .{})` of a fixed byte
//! string emits a `write(fd, ptr, len)` syscall. The overflow/narrow safety
//! intrinsics that guard a `Trap` are lowered to width-exact boolean predicates,
//! so a Debug build's checks are reproduced faithfully.
//!
//! Anything outside the subset is rejected up-front with a
//! [`CodegenError::Unsupported`] message (the gate runs before any byte is
//! emitted), so a non-subset program fails cleanly instead of miscompiling — the
//! VM path via `k2c run` remains available.

use k2_mir::{
    BinOp, CastKind, Const, ConstData, DiscrKind, IntrinsicRoot, MirFunction, MirProgram, Operand,
    Rvalue, Statement, Terminator, TrapReason, UnOp,
};
use k2_types::{IntBits, Type, TypeId};

use crate::encode::{Asm, Cc, LabelId};
use crate::reg::{Gpr, ARG_REGS};
use crate::{CodegenError, RoData};

/// The Linux x86-64 syscall numbers the backend emits.
mod sys {
    /// `write(fd, buf, count)`.
    pub const WRITE: i64 = 1;
    /// `exit(code)`.
    pub const EXIT: i64 = 60;
}

/// The process exit code a panic/trap maps to, matching the VM's `134`.
pub const PANIC_EXIT: i64 = 134;

/// The integer representation of a scalar: its bit width and signedness. Mirrors
/// the VM's `IntRepr` so narrowing/sign-extension match byte-for-byte.
#[derive(Clone, Copy)]
struct Repr {
    /// The bit width (`8`/`16`/`32`/`64`; `0` for an unsized `comptime_int`,
    /// treated as full-width with no masking).
    width: u16,
    /// `true` for a signed integer.
    signed: bool,
}

impl Repr {
    /// `true` if a result of this repr needs width normalization (a sub-64 fixed
    /// width). Width 0 (comptime) and width 64 need none.
    fn needs_normalize(self) -> bool {
        self.width != 0 && self.width < 64
    }
}

/// Resolves the [`Repr`] of a type. Non-integer scalars (`bool`, pointers,
/// opaque capability handles) are treated as full-width unsigned values, which
/// is correct for the threading/equality/branch logic the subset performs.
fn repr_of(prog: &MirProgram, ty: TypeId) -> Repr {
    match prog.arena.get(ty) {
        Type::Int { signed, bits } => Repr {
            width: match bits {
                IntBits::Fixed(n) => *n,
                IntBits::Usize | IntBits::Isize => 64,
            },
            signed: *signed,
        },
        // `bool` is a 0/1 byte value; treat as width 0 (no masking needed — it is
        // always produced as a clean 0/1).
        _ => Repr {
            width: 0,
            signed: false,
        },
    }
}

/// How the program-entry `main` maps its result to the process exit code.
#[derive(Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    /// Not `main` — an ordinary helper whose `Return` is a SysV value return.
    Helper,
    /// `main(...) !void`: the result is an error union. The exit code is 0 on the
    /// void/Ok path and 1 when an error escapes — identical to the VM's rule.
    VoidEntry,
    /// `main() IntType`: the result is an integer the `_start` shim propagates
    /// directly as the process exit code (the natural native convention, used by
    /// the acceptance "compute an integer and exit with it" programs).
    IntEntry,
}

/// The per-function lowering state.
pub struct FnLower<'p> {
    prog: &'p MirProgram,
    func: &'p MirFunction,
    asm: Asm,
    /// How this function's `Return` maps to RAX (see [`EntryKind`]).
    entry_kind: EntryKind,
}

impl<'p> FnLower<'p> {
    /// Creates a lowering context for `func`.
    pub fn new(prog: &'p MirProgram, func: &'p MirFunction) -> FnLower<'p> {
        let entry_kind = if func.name == "main" {
            // The return slot is `func.ret`. An error-union (`!void`) return maps
            // to the 0/1 exit-code rule; a plain integer return propagates its
            // value as the exit code.
            match prog.arena.get(func.ret) {
                Type::Int { .. } => EntryKind::IntEntry,
                _ => EntryKind::VoidEntry,
            }
        } else {
            EntryKind::Helper
        };
        FnLower {
            prog,
            func,
            asm: Asm::new(),
            entry_kind,
        }
    }

    /// The byte displacement of `local`'s stack slot relative to RBP. Slot `i`
    /// (local `i`) lives at `[rbp - 8*(i+1)]`, so local 0 (the return slot) is at
    /// `[rbp - 8]`.
    fn slot(&self, local: k2_mir::LocalId) -> i32 {
        -8 * (local.index() as i32 + 1)
    }

    /// The 16-byte-aligned frame size, in bytes, for this function. One 8-byte
    /// slot per local; rounded up so RSP stays 16-aligned at call sites (the
    /// prologue's `push rbp` then `sub rsp, frame` leaves RSP 16-aligned, and the
    /// body never pushes, so every `call` is correctly aligned with no fixup).
    fn frame_size(&self) -> i32 {
        let raw = 8 * self.func.locals.len();
        let aligned = (raw + 15) & !15;
        aligned as i32
    }

    /// Lowers the whole function, returning its finalized code + cross-function
    /// fixups (calls / rodata pointers) for the program layout pass to patch.
    pub fn lower(
        mut self,
        rodata: &mut RoData,
    ) -> Result<(Vec<u8>, Vec<crate::encode::Fixup>), CodegenError> {
        // One label per basic block; bound when the block is emitted.
        self.asm.reserve_labels(self.func.blocks.len());

        // ---- Prologue: set up the frame and spill parameters. ----
        self.asm.push(Gpr::Rbp);
        self.asm.mov_rr(Gpr::Rbp, Gpr::Rsp);
        let frame = self.frame_size();
        if frame > 0 {
            self.asm.sub_rsp_imm(frame);
        }
        // Spill each parameter register into its local's slot. Params are
        // LocalId(0..) right after the return slot? No: params follow the layout
        // `func.params` gives — store ARG_REGS[k] into params[k]'s slot.
        for (k, param) in self.func.params.iter().enumerate() {
            if k < ARG_REGS.len() {
                let disp = self.slot(*param);
                self.asm.mov_store(disp, ARG_REGS[k]);
            }
        }

        // ---- Emit every block in order (entry is BlockId(0)). ----
        for (bi, block) in self.func.blocks.iter().enumerate() {
            self.asm.bind_label(LabelId(bi as u32));
            for stmt in &block.stmts {
                self.lower_stmt(stmt, rodata)?;
            }
            self.lower_terminator(&block.term, rodata)?;
        }

        Ok(self.asm.finish())
    }

    // ---------------------------------------------------------------------
    //  Operands & places
    // ---------------------------------------------------------------------

    /// Loads `op` into `dst`. Only bare-local copies and constants are accepted;
    /// the subset gate rejects projected places elsewhere.
    fn operand_to(&mut self, op: &Operand, dst: Gpr) -> Result<(), CodegenError> {
        match op {
            Operand::Copy(p) => {
                if !p.proj.is_empty() {
                    return Err(CodegenError::Unsupported(format!(
                        "projected place read in `{}`",
                        self.func.name
                    )));
                }
                let disp = self.slot(p.base);
                self.asm.mov_load(dst, disp);
                Ok(())
            }
            Operand::Const(c) => self.const_to(c, dst),
        }
    }

    /// Materializes a scalar constant into `dst`. Strings/aggregates are not
    /// scalar operands and are rejected (the gate handles the print-string path
    /// specially before reaching here).
    fn const_to(&mut self, c: &Const, dst: Gpr) -> Result<(), CodegenError> {
        match c {
            Const::Int { value, ty } => {
                let imm = self.const_int_bits(*value, *ty);
                self.asm.mov_ri(dst, imm);
                Ok(())
            }
            Const::Bool(b) => {
                self.asm.mov_ri(dst, *b as i64);
                Ok(())
            }
            Const::Void => {
                // A `()` value never feeds an arithmetic op; load 0 as a harmless
                // placeholder (e.g. `return ()` in a void-returning helper).
                self.asm.mov_ri(dst, 0);
                Ok(())
            }
            other => Err(CodegenError::Unsupported(format!(
                "non-scalar constant {other:?} in `{}`",
                self.func.name
            ))),
        }
    }

    /// The 64-bit pattern a typed integer constant occupies in a register: the
    /// value masked/sign-extended to its type width, then placed in the low bits.
    /// Matches the VM's masked `Value::int` so a stored narrow constant compares
    /// equal across the two backends.
    fn const_int_bits(&self, value: i128, ty: TypeId) -> i64 {
        let r = repr_of(self.prog, ty);
        if r.width == 0 || r.width >= 64 {
            return value as i64;
        }
        let mask: u64 = if r.width >= 64 {
            u64::MAX
        } else {
            (1u64 << r.width) - 1
        };
        let raw = (value as u64) & mask;
        if r.signed {
            // Sign-extend from the type width into the full 64 bits.
            let shift = 64 - r.width as u32;
            ((raw << shift) as i64) >> shift
        } else {
            raw as i64
        }
    }

    /// Normalizes the value in `dst` to the width/signedness of `ty`. For an
    /// unsigned sub-64 type this ANDs off the high bits; for a signed one it
    /// sign-extends from the type width. A 64-bit or comptime type is a no-op.
    fn normalize(&mut self, dst: Gpr, ty: TypeId) {
        let r = repr_of(self.prog, ty);
        if !r.needs_normalize() {
            return;
        }
        if r.signed {
            match r.width {
                8 => self.asm.movsx8(dst, dst),
                16 => self.asm.movsx16(dst, dst),
                32 => self.asm.movsxd(dst, dst),
                // Other signed widths (e.g. i7) fall back to a shift pair.
                w => self.normalize_signed_other(dst, w),
            }
        } else if r.width == 32 {
            // Unsigned 32-bit: a `REX.W 81 /4 0xFFFFFFFF` AND-mask is *wrong* —
            // the imm32 sign-extends to `0xFFFF_FFFF_FFFF_FFFF`, masking nothing
            // (a no-op). Instead emit a 32-bit-operand `mov dst32, dst32`, which
            // the CPU defines to zero the upper 32 bits, exactly masking to
            // `2^32 - 1`.
            self.asm.mov_rr32(dst, dst);
        } else {
            // Unsigned 8/16: mask to 2^w - 1. The masks 0xFF / 0xFFFF are positive
            // i32 values, so `REX.W 81 /4 id` does not sign-extend and is correct.
            let mask: u32 = (1u32 << r.width) - 1;
            self.asm.and_ri(dst, mask as i32);
        }
    }

    /// Sign-extends `dst` from a non-byte/-word/-dword signed width `w` (e.g.
    /// `i7`) via a shift-left / arithmetic-shift-right pair. Rare in practice.
    fn normalize_signed_other(&mut self, dst: Gpr, w: u16) {
        let shift = (64 - w) as i64;
        // mov rcx, shift; shl dst, cl; sar dst, cl
        self.asm.mov_ri(Gpr::Rcx, shift);
        self.asm.shl_cl(dst);
        self.asm.mov_ri(Gpr::Rcx, shift);
        self.asm.sar_cl(dst);
    }

    /// Stores RAX into `local`'s stack slot.
    fn store_rax(&mut self, local: k2_mir::LocalId) {
        let disp = self.slot(local);
        self.asm.mov_store(disp, Gpr::Rax);
    }

    // ---------------------------------------------------------------------
    //  Statements
    // ---------------------------------------------------------------------

    /// Lowers one statement.
    fn lower_stmt(&mut self, stmt: &Statement, rodata: &mut RoData) -> Result<(), CodegenError> {
        match stmt {
            Statement::Assign { place, rvalue, .. } => {
                if !place.proj.is_empty() {
                    return Err(CodegenError::Unsupported(format!(
                        "projected assignment in `{}`",
                        self.func.name
                    )));
                }
                self.lower_rvalue(place.base, rvalue, rodata)
            }
            // Evaluate for effect (e.g. a `print` intrinsic whose `Ok` result is
            // discarded). Route to a scratch destination — local 0 is fine to
            // overwrite transiently because an `Eval` never observes it, but to
            // be safe we compute into RAX and drop it without storing.
            Statement::Eval { rvalue, .. } => self.lower_rvalue_discard(rvalue, rodata),
            // Storage / check / note are advisory or already realized into
            // branches by the splitter; nothing to emit.
            Statement::StorageLive(_)
            | Statement::StorageDead(_)
            | Statement::Check(_)
            | Statement::Note(_) => Ok(()),
        }
    }

    /// Lowers an rvalue whose result is discarded (an `Eval` statement). Side
    /// effects (a `print` syscall) still happen; the result value is left in RAX
    /// and ignored.
    fn lower_rvalue_discard(
        &mut self,
        rvalue: &Rvalue,
        rodata: &mut RoData,
    ) -> Result<(), CodegenError> {
        // The only effectful discarded rvalue in the subset is an intrinsic
        // (stdout/print). Compute it into RAX without a destination store.
        match rvalue {
            Rvalue::Intrinsic { .. } => {
                self.lower_intrinsic_into_rax(rvalue, rodata)?;
                Ok(())
            }
            // A pure discarded rvalue has no observable effect; skip it.
            _ => Ok(()),
        }
    }

    // ---------------------------------------------------------------------
    //  Rvalues
    // ---------------------------------------------------------------------

    /// Lowers `dst = rvalue`.
    fn lower_rvalue(
        &mut self,
        dst: k2_mir::LocalId,
        rvalue: &Rvalue,
        rodata: &mut RoData,
    ) -> Result<(), CodegenError> {
        let dst_ty = self.func.locals[dst.index()].ty;
        match rvalue {
            Rvalue::Use(op) => {
                self.operand_to(op, Gpr::Rax)?;
                self.normalize(Gpr::Rax, dst_ty);
                self.store_rax(dst);
                Ok(())
            }
            Rvalue::Binary { op, lhs, rhs, ty } => self.lower_binary(dst, *op, lhs, rhs, *ty),
            Rvalue::Unary { op, operand, ty } => self.lower_unary(dst, *op, operand, *ty),
            Rvalue::Cast { kind, operand, ty } => self.lower_cast(dst, *kind, operand, *ty),
            Rvalue::Call { func, args, ty } => self.lower_call(dst, *func, args, *ty),
            Rvalue::Intrinsic { .. } => {
                self.lower_intrinsic_into_rax(rvalue, rodata)?;
                self.store_rax(dst);
                Ok(())
            }
            // An empty tuple/struct aggregate (the `print` argument tuple `.{}` /
            // a unit value): it carries no data the subset reads, so storing a
            // zero placeholder is sufficient and keeps the slot defined.
            Rvalue::Aggregate { fields, .. } if fields.is_empty() => {
                self.asm.mov_ri(Gpr::Rax, 0);
                self.store_rax(dst);
                Ok(())
            }
            // The discriminant of the `print` result's error union. The subset
            // represents an `Ok` result as the sentinel `0` and an error as a
            // nonzero tag, so "is error" is exactly `value != 0`. This drives the
            // `try`-desugared branch (success vs. the statically-dead error path).
            Rvalue::Discriminant { operand, kind } => self.lower_discriminant(dst, operand, *kind),
            other => Err(CodegenError::Unsupported(format!(
                "rvalue {} in `{}`",
                rvalue_kind(other),
                self.func.name
            ))),
        }
    }

    /// Lowers a binary op `dst = lhs OP rhs : ty`.
    fn lower_binary(
        &mut self,
        dst: k2_mir::LocalId,
        op: BinOp,
        lhs: &Operand,
        rhs: &Operand,
        ty: TypeId,
    ) -> Result<(), CodegenError> {
        use BinOp::*;
        // Comparisons read signedness from the operands, not the bool result.
        if is_comparison(op) {
            return self.lower_compare(dst, op, lhs, rhs);
        }
        // LHS -> RAX, RHS -> RCX.
        self.operand_to(lhs, Gpr::Rax)?;
        self.operand_to(rhs, Gpr::Rcx)?;
        match op {
            Add => self.asm.add_rr(Gpr::Rax, Gpr::Rcx),
            Sub => self.asm.sub_rr(Gpr::Rax, Gpr::Rcx),
            Mul => self.asm.imul_rr(Gpr::Rax, Gpr::Rcx),
            BitAnd => self.asm.and_rr(Gpr::Rax, Gpr::Rcx),
            BitOr => self.asm.or_rr(Gpr::Rax, Gpr::Rcx),
            BitXor => self.asm.xor_rr(Gpr::Rax, Gpr::Rcx),
            Div => {
                // Select signed vs unsigned division by the *result* type's
                // signedness. Signed: sign-extend RAX into RDX:RAX (`cqo`) then
                // `idiv rcx`. Unsigned: zero the high half (`xor rdx, rdx`) then
                // `div rcx` — a signed `idiv` would misread a high-bit-set u64
                // dividend as negative (wrong quotient, or a `#DE` fault on the
                // MIN/-1 bit pattern). Quotient lands in RAX either way.
                if repr_of(self.prog, ty).signed {
                    self.asm.cqo();
                    self.asm.idiv_r(Gpr::Rcx);
                } else {
                    self.asm.zero_rdx();
                    self.asm.div_r(Gpr::Rcx);
                }
            }
            Rem => {
                // Same signed/unsigned split as `Div`; the remainder lands in RDX.
                if repr_of(self.prog, ty).signed {
                    self.asm.cqo();
                    self.asm.idiv_r(Gpr::Rcx);
                } else {
                    self.asm.zero_rdx();
                    self.asm.div_r(Gpr::Rcx);
                }
                // Remainder is in RDX; move it to RAX for the uniform store.
                self.asm.mov_rr(Gpr::Rax, Gpr::Rdx);
            }
            Shl => {
                // Count is in RCX already (the shift uses CL).
                self.asm.shl_cl(Gpr::Rax);
            }
            Shr => {
                let r = repr_of(self.prog, ty);
                if r.signed {
                    self.asm.sar_cl(Gpr::Rax);
                } else {
                    self.asm.shr_cl(Gpr::Rax);
                }
            }
            // Comparisons handled above.
            Eq | Ne | Lt | Le | Gt | Ge => unreachable!("comparison routed to lower_compare"),
        }
        self.normalize(Gpr::Rax, ty);
        self.store_rax(dst);
        Ok(())
    }

    /// Lowers a comparison `dst = lhs CMP rhs` producing a 0/1 bool.
    fn lower_compare(
        &mut self,
        dst: k2_mir::LocalId,
        op: BinOp,
        lhs: &Operand,
        rhs: &Operand,
    ) -> Result<(), CodegenError> {
        // The comparison signedness comes from the operands' type. Use the LHS
        // operand's type (operands of a comparison share a type in the corpus).
        let signed = self.operand_signed(lhs) || self.operand_signed(rhs);
        self.operand_to(lhs, Gpr::Rax)?;
        self.operand_to(rhs, Gpr::Rcx)?;
        self.asm.cmp_rr(Gpr::Rax, Gpr::Rcx);
        let cc = compare_cc(op, signed);
        self.asm.setcc_al(cc);
        self.asm.movzx_al(Gpr::Rax);
        self.store_rax(dst);
        Ok(())
    }

    /// `true` if `op`'s source type is a signed integer (drives signed vs
    /// unsigned comparison condition codes / right shifts).
    fn operand_signed(&self, op: &Operand) -> bool {
        match op {
            Operand::Copy(p) if p.proj.is_empty() => {
                repr_of(self.prog, self.func.locals[p.base.index()].ty).signed
            }
            Operand::Const(Const::Int { ty, .. }) => repr_of(self.prog, *ty).signed,
            _ => false,
        }
    }

    /// Lowers a unary op `dst = OP operand : ty`.
    fn lower_unary(
        &mut self,
        dst: k2_mir::LocalId,
        op: UnOp,
        operand: &Operand,
        ty: TypeId,
    ) -> Result<(), CodegenError> {
        self.operand_to(operand, Gpr::Rax)?;
        match op {
            UnOp::Neg => {
                self.asm.neg_r(Gpr::Rax);
                self.normalize(Gpr::Rax, ty);
            }
            UnOp::BitNot => {
                self.asm.not_r(Gpr::Rax);
                self.normalize(Gpr::Rax, ty);
            }
            UnOp::Not => {
                // Boolean negation: flip the low bit of a 0/1 value.
                self.asm.xor_ri(Gpr::Rax, 1);
            }
        }
        self.store_rax(dst);
        Ok(())
    }

    /// Lowers an integer/pointer cast `dst = cast.kind operand : ty`.
    fn lower_cast(
        &mut self,
        dst: k2_mir::LocalId,
        kind: CastKind,
        operand: &Operand,
        ty: TypeId,
    ) -> Result<(), CodegenError> {
        match kind {
            CastKind::Widen | CastKind::IntNarrow | CastKind::PtrReinterpret => {
                self.operand_to(operand, Gpr::Rax)?;
                // Normalize to the target type's width; the paired NarrowFits
                // check (when present) already trapped a lossy narrow as a branch.
                self.normalize(Gpr::Rax, ty);
                self.store_rax(dst);
                Ok(())
            }
            CastKind::IntToFloat | CastKind::FloatToInt => Err(CodegenError::Unsupported(format!(
                "float cast in `{}`",
                self.func.name
            ))),
        }
    }

    /// Lowers `dst = discr.kind operand`, producing a `bool`. For the subset, an
    /// error union / optional value is represented by the sentinel `0` when it is
    /// `Ok`/`Some` (the `print` result) and a nonzero tag otherwise, so the
    /// `ErrorUnion`/`Optional` discriminant is `operand != 0`. A tagged-union
    /// discriminant (a raw variant index) is out of the v0.14 subset.
    fn lower_discriminant(
        &mut self,
        dst: k2_mir::LocalId,
        operand: &Operand,
        kind: DiscrKind,
    ) -> Result<(), CodegenError> {
        match kind {
            DiscrKind::ErrorUnion | DiscrKind::Optional => {
                self.operand_to(operand, Gpr::Rax)?;
                // bool = (operand != 0): cmp against 0 via test, setne.
                self.asm.test_rr(Gpr::Rax, Gpr::Rax);
                self.asm.setcc_al(Cc::Ne);
                self.asm.movzx_al(Gpr::Rax);
                self.store_rax(dst);
                Ok(())
            }
            DiscrKind::Union => Err(CodegenError::Unsupported(format!(
                "tagged-union discriminant in `{}`",
                self.func.name
            ))),
        }
    }

    /// Lowers a direct call `dst = func(args)` under the System V AMD64 ABI.
    /// Arguments go in `rdi, rsi, rdx, rcx, r8, r9`; the result is in RAX. The
    /// body performs no in-body pushes, so RSP stays 16-aligned from the prologue
    /// and the `call` site is correctly aligned with no extra work.
    fn lower_call(
        &mut self,
        dst: k2_mir::LocalId,
        func: k2_mir::FnId,
        args: &[Operand],
        ty: TypeId,
    ) -> Result<(), CodegenError> {
        if args.len() > ARG_REGS.len() {
            return Err(CodegenError::Unsupported(format!(
                "call with {} args (>6) in `{}`",
                args.len(),
                self.func.name
            )));
        }
        // Evaluate each argument into RAX, then move it into its arg register.
        // Operands are bare locals/consts (no nested call), so moving into the
        // arg register cannot clobber a not-yet-evaluated argument.
        for (k, arg) in args.iter().enumerate() {
            self.operand_to(arg, Gpr::Rax)?;
            let reg = ARG_REGS[k];
            if reg != Gpr::Rax {
                self.asm.mov_rr(reg, Gpr::Rax);
            }
        }
        self.asm.call_fn(func);
        // Result in RAX; normalize to the call type and store.
        self.normalize(Gpr::Rax, ty);
        self.store_rax(dst);
        Ok(())
    }

    // ---------------------------------------------------------------------
    //  Intrinsics (stdout/stderr writer + print + safety predicates)
    // ---------------------------------------------------------------------

    /// Lowers a recognized intrinsic, leaving its result value in RAX. The
    /// recognized shapes are the stdout/stderr writer (yields an fd token), the
    /// fixed-string `print` (emits a `write` syscall, yields an Ok sentinel), and
    /// the overflow/narrow safety predicates (yield a width-exact bool).
    fn lower_intrinsic_into_rax(
        &mut self,
        rvalue: &Rvalue,
        rodata: &mut RoData,
    ) -> Result<(), CodegenError> {
        let Rvalue::Intrinsic { path, args, .. } = rvalue else {
            unreachable!("lower_intrinsic_into_rax called on non-intrinsic");
        };
        // Builtin-rooted safety predicates.
        if let IntrinsicRoot::Builtin(name) = &path.root {
            return self.lower_safety_predicate(name, args);
        }
        // Value-rooted capability methods.
        let members: Vec<&str> = path.members.iter().map(|s| s.as_str()).collect();
        match members.as_slice() {
            ["io", "stdout"] | ["stdout"] => {
                self.asm.mov_ri(Gpr::Rax, 1);
                Ok(())
            }
            ["io", "stderr"] | ["stderr"] => {
                self.asm.mov_ri(Gpr::Rax, 2);
                Ok(())
            }
            ["print"] => self.lower_print(path, args, rodata),
            other => Err(CodegenError::Unsupported(format!(
                "intrinsic value.{} in `{}`",
                other.join("."),
                self.func.name
            ))),
        }
    }

    /// Lowers `value(fd).print(str, .{})` of a fixed byte string with an empty
    /// argument tuple and no format placeholders into a `write(fd, ptr, len)`
    /// syscall, leaving an `Ok`-sentinel (`0`) in RAX. The fd is the writer-token
    /// receiver; the string bytes are interned into `.rodata` and the pointer is
    /// loaded by a `DataRef` hole the layout pass patches to the absolute vaddr.
    fn lower_print(
        &mut self,
        path: &k2_mir::IntrinsicPath,
        args: &[Operand],
        rodata: &mut RoData,
    ) -> Result<(), CodegenError> {
        // The receiver carries the fd token (1 or 2). It is either a bare local
        // holding the stdout/stderr token or, rarely, an inline operand.
        let fd_disp = match &path.root {
            IntrinsicRoot::Value(op) => match op.as_ref() {
                Operand::Copy(p) if p.proj.is_empty() => self.slot(p.base),
                _ => {
                    return Err(CodegenError::Unsupported(format!(
                        "print receiver not a bare local in `{}`",
                        self.func.name
                    )))
                }
            },
            _ => {
                return Err(CodegenError::Unsupported(format!(
                    "print without a value receiver in `{}`",
                    self.func.name
                )))
            }
        };

        // The format string is the first arg; the second is the (empty) tuple.
        let bytes = self.print_literal_bytes(args)?;
        let len = bytes.len();
        let data_off = rodata.intern(&bytes);

        // write(fd, ptr, len): rax=1, rdi=fd, rsi=ptr, rdx=len.
        // Load fd into RDI from the writer-token slot.
        self.asm.mov_load(Gpr::Rdi, fd_disp);
        self.asm.mov_ri_data(Gpr::Rsi, data_off);
        self.asm.mov_ri(Gpr::Rdx, len as i64);
        self.asm.mov_ri(Gpr::Rax, sys::WRITE);
        self.asm.syscall();
        // Yield the Ok sentinel (0 => `discr.ErrorUnion` reads "not an error").
        self.asm.mov_ri(Gpr::Rax, 0);
        Ok(())
    }

    /// Extracts the literal bytes a `print` will emit: the format-string constant,
    /// rejected unless the argument tuple is empty and the string carries no
    /// `{...}` placeholder (runtime formatting is v0.15). A doubled `{{`/`}}` is
    /// resolved to a single brace so the emitted bytes match the VM's renderer.
    fn print_literal_bytes(&self, args: &[Operand]) -> Result<Vec<u8>, CodegenError> {
        // args[0] = format string (Const::Str), args[1] = tuple aggregate.
        let fmt_id = match args.first() {
            Some(Operand::Const(Const::Str(id))) => *id,
            _ => {
                return Err(CodegenError::Unsupported(format!(
                    "print with a non-constant format string in `{}`",
                    self.func.name
                )))
            }
        };
        let raw = match &self.prog.consts[fmt_id.0 as usize] {
            ConstData::Bytes(b) => b.clone(),
            ConstData::Aggregate(_) => {
                return Err(CodegenError::Unsupported(format!(
                    "print of a non-byte constant in `{}`",
                    self.func.name
                )))
            }
        };
        // The argument tuple must be empty (no runtime-formatted values).
        let tuple_empty = match args.get(1) {
            None => true,
            Some(Operand::Const(Const::Void)) => true,
            Some(Operand::Copy(_)) => {
                // The tuple is a separate local holding an empty aggregate.
                // Accept only when the format string has no placeholders (checked
                // below); the bytes are emitted verbatim regardless.
                true
            }
            Some(_) => false,
        };
        if !tuple_empty {
            return Err(CodegenError::Unsupported(format!(
                "print with a non-empty argument tuple in `{}` (runtime formatting is v0.15)",
                self.func.name
            )));
        }
        // Reject real placeholders; resolve escaped braces to literal bytes.
        let rendered = render_braces(&raw).ok_or_else(|| {
            CodegenError::Unsupported(format!(
                "print with format placeholders in `{}` (runtime formatting is v0.15)",
                self.func.name
            ))
        })?;
        Ok(rendered)
    }

    /// Lowers a `@no_*_overflow` / `narrow_fits` safety predicate into a width-
    /// exact boolean in RAX, matching the VM's predicate so a Debug build's
    /// branch-to-trap fires identically.
    fn lower_safety_predicate(&mut self, name: &str, args: &[Operand]) -> Result<(), CodegenError> {
        // The predicate type (the masked width) is the `undef` carrier's type,
        // which is the last argument; fall back to the first operand's type.
        let ty = self.predicate_type(args);
        match name {
            "no_add_overflow" => self.overflow_predicate(args, ty, ArithKind::Add),
            "no_sub_overflow" => self.overflow_predicate(args, ty, ArithKind::Sub),
            "no_mul_overflow" => self.overflow_predicate(args, ty, ArithKind::Mul),
            "no_div_overflow" => self.div_overflow_predicate(args, ty),
            "no_neg_overflow" => self.neg_overflow_predicate(args, ty),
            "narrow_fits" => self.narrow_fits_predicate(args, ty),
            other => Err(CodegenError::Unsupported(format!(
                "safety intrinsic @{other} in `{}`",
                self.func.name
            ))),
        }
    }

    /// The type a check predicate guards: the `undef` carrier (last arg) when it
    /// names a sized integer, else the first operand's type.
    fn predicate_type(&self, args: &[Operand]) -> TypeId {
        if let Some(Operand::Const(Const::Undef { ty })) = args.last() {
            let r = repr_of(self.prog, *ty);
            if r.width != 0 {
                return *ty;
            }
        }
        // Fall back to the first sized operand.
        for a in args {
            if let Some(ty) = self.operand_type(a) {
                let r = repr_of(self.prog, ty);
                if r.width != 0 {
                    return ty;
                }
            }
        }
        // Last resort: the first operand's type (may be comptime -> no masking).
        args.first()
            .and_then(|a| self.operand_type(a))
            .unwrap_or(self.func.ret)
    }

    /// The type of an operand, if it is a bare local or a typed integer const.
    fn operand_type(&self, op: &Operand) -> Option<TypeId> {
        match op {
            Operand::Copy(p) if p.proj.is_empty() => Some(self.func.locals[p.base.index()].ty),
            Operand::Const(Const::Int { ty, .. }) => Some(*ty),
            _ => None,
        }
    }

    /// `no_{add,sub,mul}_overflow(a, b)`: `true` if `a OP b` fits the type width.
    ///
    /// For a **sub-64** width the op cannot overflow the 64-bit register, so the
    /// result is computed at full precision and re-narrowed to the type width in
    /// RCX; equality with the un-narrowed result means "fits".
    ///
    /// For a **full 64-bit** width (`u64`/`i64`/`usize`/`isize`) the re-narrow
    /// trick cannot work — there is no wider register to detect the carry/borrow
    /// in — so we read the CPU's overflow flags directly, matching the VM's
    /// i128-precision `no_overflow` check:
    ///
    /// * add/sub — perform the op and test OF (`seto`) for a signed carrier or
    ///   CF (`setc`) for an unsigned one; the op overflows iff that flag is set.
    /// * mul — use the **one-operand** form so the flag reflects the true 128-bit
    ///   product: signed `imul rcx` sets OF, unsigned `mul rcx` sets CF, iff the
    ///   product does not fit 64 bits.
    ///
    /// In every case `ok = !overflowed`, materialized as a 0/1 bool in RAX.
    fn overflow_predicate(
        &mut self,
        args: &[Operand],
        ty: TypeId,
        kind: ArithKind,
    ) -> Result<(), CodegenError> {
        let r = repr_of(self.prog, ty);
        if r.width == 0 {
            // A genuine `comptime_int` carrier has no finite width to overflow;
            // the VM treats it as unbounded, so the op always "fits". Yield 1.
            self.asm.mov_ri(Gpr::Rax, 1);
            return Ok(());
        }
        if r.width >= 64 {
            return self.overflow_predicate_64(args, r, kind);
        }
        self.operand_to(arg(args, 0)?, Gpr::Rax)?;
        self.operand_to(arg(args, 1)?, Gpr::Rcx)?;
        match kind {
            ArithKind::Add => self.asm.add_rr(Gpr::Rax, Gpr::Rcx),
            ArithKind::Sub => self.asm.sub_rr(Gpr::Rax, Gpr::Rcx),
            ArithKind::Mul => self.asm.imul_rr(Gpr::Rax, Gpr::Rcx),
        }
        // RCX := normalize(RAX to ty); fits iff RAX == RCX.
        self.asm.mov_rr(Gpr::Rcx, Gpr::Rax);
        self.normalize(Gpr::Rcx, ty);
        self.asm.cmp_rr(Gpr::Rax, Gpr::Rcx);
        self.asm.setcc_al(Cc::E);
        self.asm.movzx_al(Gpr::Rax);
        Ok(())
    }

    /// The full-64-bit overflow predicate (see [`FnLower::overflow_predicate`]):
    /// performs the arithmetic and reads the OF (signed) / CF (unsigned) flag the
    /// hardware sets, yielding `ok = !overflowed`. The flag-reading `setcc` must
    /// immediately follow the flag-setting op, so RAX/RCX are loaded first and no
    /// other flag-clobbering instruction intervenes.
    fn overflow_predicate_64(
        &mut self,
        args: &[Operand],
        r: Repr,
        kind: ArithKind,
    ) -> Result<(), CodegenError> {
        self.operand_to(arg(args, 0)?, Gpr::Rax)?;
        self.operand_to(arg(args, 1)?, Gpr::Rcx)?;
        // The "did it overflow" condition: OF for signed, CF for unsigned.
        let overflow_cc = if r.signed { Cc::O } else { Cc::C };
        match kind {
            ArithKind::Add => self.asm.add_rr(Gpr::Rax, Gpr::Rcx),
            ArithKind::Sub => self.asm.sub_rr(Gpr::Rax, Gpr::Rcx),
            ArithKind::Mul => {
                // One-operand multiply: RDX:RAX = RAX * RCX. Signed `imul` sets
                // OF, unsigned `mul` sets CF, iff the product exceeds 64 bits.
                if r.signed {
                    self.asm.imul_r1(Gpr::Rcx);
                } else {
                    self.asm.mul_r(Gpr::Rcx);
                }
            }
        }
        // AL = overflowed (1/0); invert to `ok` and zero-extend to the full RAX.
        self.asm.setcc_al(overflow_cc);
        self.asm.movzx_al(Gpr::Rax);
        self.asm.xor_ri(Gpr::Rax, 1); // ok = !overflowed
        Ok(())
    }

    /// `no_div_overflow(a, b)`: `true` unless `signed && a == MIN && b == -1`.
    fn div_overflow_predicate(&mut self, args: &[Operand], ty: TypeId) -> Result<(), CodegenError> {
        let r = repr_of(self.prog, ty);
        if !r.signed || r.width == 0 {
            self.asm.mov_ri(Gpr::Rax, 1);
            return Ok(());
        }
        // ok = !(a == MIN && b == -1)  ==>  ok = (a != MIN) || (b != -1).
        // Compute (a == MIN) into RAX bool, (b == -1) into a slot, AND, invert.
        let min = type_min(r);
        // a == MIN  ->  RAX bool
        self.operand_to(arg(args, 0)?, Gpr::Rax)?;
        self.asm.mov_ri(Gpr::Rcx, min);
        self.asm.cmp_rr(Gpr::Rax, Gpr::Rcx);
        self.asm.setcc_al(Cc::E);
        self.asm.movzx_al(Gpr::Rax);
        // Save (a==MIN) on the C-scratch by leaving it in RAX; compute (b==-1)
        // into RCX bool, then AND. We need a second scratch — use RDX.
        self.asm.mov_rr(Gpr::Rdx, Gpr::Rax); // RDX = (a == MIN)
        self.operand_to(arg(args, 1)?, Gpr::Rax)?;
        self.asm.mov_ri(Gpr::Rcx, -1);
        self.asm.cmp_rr(Gpr::Rax, Gpr::Rcx);
        self.asm.setcc_al(Cc::E);
        self.asm.movzx_al(Gpr::Rax); // RAX = (b == -1)
        self.asm.and_rr(Gpr::Rax, Gpr::Rdx); // RAX = (a==MIN) && (b==-1)
        self.asm.xor_ri(Gpr::Rax, 1); // ok = !bad
        Ok(())
    }

    /// `no_neg_overflow(a)`: `true` unless `signed && a == MIN`.
    fn neg_overflow_predicate(&mut self, args: &[Operand], ty: TypeId) -> Result<(), CodegenError> {
        let r = repr_of(self.prog, ty);
        if !r.signed || r.width == 0 {
            self.asm.mov_ri(Gpr::Rax, 1);
            return Ok(());
        }
        let min = type_min(r);
        self.operand_to(arg(args, 0)?, Gpr::Rax)?;
        self.asm.mov_ri(Gpr::Rcx, min);
        self.asm.cmp_rr(Gpr::Rax, Gpr::Rcx);
        self.asm.setcc_al(Cc::Ne); // ok = (a != MIN)
        self.asm.movzx_al(Gpr::Rax);
        Ok(())
    }

    /// `narrow_fits(value)`: `true` if `value` re-narrowed to the target type
    /// equals `value`. The target type is the `undef` carrier (last arg).
    ///
    /// For a **sub-64** target the value is re-narrowed in RCX and compared for
    /// equality. For a **full 64-bit** target the only lossy case is a
    /// *signedness reinterpretation* (`u64`→`i64` of a value above `i64::MAX`, or
    /// `i64`→`u64` of a negative value) — there is no narrower width to lose bits
    /// to. Both lossy cases are exactly "the register's signed interpretation is
    /// negative", so `ok = (value as i64) >= 0`; a same-signedness 64-bit "narrow"
    /// is the identity and always fits. This matches the VM's `narrow_fits`, which
    /// range-checks the source value against the concrete target's bounds.
    fn narrow_fits_predicate(&mut self, args: &[Operand], ty: TypeId) -> Result<(), CodegenError> {
        let r = repr_of(self.prog, ty);
        if r.width == 0 {
            // Unknown / comptime target: an unconstrained narrow always fits.
            self.asm.mov_ri(Gpr::Rax, 1);
            return Ok(());
        }
        if r.width >= 64 {
            // A 64-bit target only loses information across a signedness change.
            let src_signed = self
                .operand_type(arg(args, 0)?)
                .map(|t| repr_of(self.prog, t).signed)
                .unwrap_or(r.signed);
            if src_signed == r.signed {
                // Same signedness at full width: the narrow is the identity.
                self.asm.mov_ri(Gpr::Rax, 1);
                return Ok(());
            }
            // Differing signedness: fits iff the value's sign bit is clear, i.e.
            // its signed interpretation is non-negative (compare against 0).
            self.operand_to(arg(args, 0)?, Gpr::Rax)?;
            self.asm.mov_ri(Gpr::Rcx, 0);
            self.asm.cmp_rr(Gpr::Rax, Gpr::Rcx);
            self.asm.setcc_al(Cc::Ge); // ok = (value >= 0) signed
            self.asm.movzx_al(Gpr::Rax);
            return Ok(());
        }
        self.operand_to(arg(args, 0)?, Gpr::Rax)?;
        self.asm.mov_rr(Gpr::Rcx, Gpr::Rax);
        self.normalize(Gpr::Rcx, ty);
        self.asm.cmp_rr(Gpr::Rax, Gpr::Rcx);
        self.asm.setcc_al(Cc::E);
        self.asm.movzx_al(Gpr::Rax);
        Ok(())
    }

    // ---------------------------------------------------------------------
    //  Terminators
    // ---------------------------------------------------------------------

    /// Lowers a block terminator.
    fn lower_terminator(
        &mut self,
        term: &Terminator,
        rodata: &mut RoData,
    ) -> Result<(), CodegenError> {
        match term {
            Terminator::Goto(t) => {
                self.asm.jmp(LabelId(t.index() as u32));
                Ok(())
            }
            Terminator::Branch {
                cond,
                then_bb,
                else_bb,
            } => {
                self.operand_to(cond, Gpr::Rax)?;
                self.asm.test_rr(Gpr::Rax, Gpr::Rax);
                self.asm.jcc(Cc::Ne, LabelId(then_bb.index() as u32));
                self.asm.jmp(LabelId(else_bb.index() as u32));
                Ok(())
            }
            Terminator::Switch {
                scrutinee,
                targets,
                default,
            } => {
                self.operand_to(scrutinee, Gpr::Rax)?;
                for (value, t) in targets {
                    // Compare the scrutinee against the arm value and jump-if-equal.
                    // Small arm values use the compact `cmp r64, imm32` form;
                    // larger ones are materialized into RCX first.
                    if let Ok(imm) = i32::try_from(*value) {
                        self.asm.cmp_ri(Gpr::Rax, imm);
                    } else {
                        self.asm.mov_ri(Gpr::Rcx, *value as i64);
                        self.asm.cmp_rr(Gpr::Rax, Gpr::Rcx);
                    }
                    self.asm.jcc(Cc::E, LabelId(t.index() as u32));
                }
                self.asm.jmp(LabelId(default.index() as u32));
                Ok(())
            }
            Terminator::Return { value } => self.lower_return(value),
            Terminator::Trap { reason } => self.lower_trap(*reason, rodata),
            // A genuinely-unreachable fall-through: trap like the VM (it never
            // executes on a correct program, but emitting a clean exit is safe).
            Terminator::Unreachable => self.lower_trap(TrapReason::Unreachable, rodata),
        }
    }

    /// Lowers a `Return`. For `main` (mapped to a process exit by `_start`), RAX
    /// carries the exit code: 0 on the void/Ok path, 1 when an error value
    /// escapes. For a helper, RAX carries the SysV return value, then `leave;ret`.
    fn lower_return(&mut self, value: &Operand) -> Result<(), CodegenError> {
        match self.entry_kind {
            EntryKind::VoidEntry => {
                // Map !void to an exit code: an error operand -> 1, else 0.
                let code = if self.returns_error(value) { 1 } else { 0 };
                self.asm.mov_ri(Gpr::Rax, code);
            }
            EntryKind::IntEntry => {
                // Propagate the integer result as the process exit code.
                self.operand_to(value, Gpr::Rax)?;
                self.normalize(Gpr::Rax, self.func.ret);
            }
            EntryKind::Helper => {
                // A normal helper: load the return value into RAX (SysV return).
                self.operand_to(value, Gpr::Rax)?;
                self.normalize(Gpr::Rax, self.func.ret);
            }
        }
        self.asm.leave();
        self.asm.ret();
        Ok(())
    }

    /// `true` if a `main` return operand carries an error (an error-union or
    /// error-set typed local). The recognized print/stdout intrinsics never fail,
    /// so this is statically `false` for the subset, but emitting the mapping
    /// keeps the exit code provably 0 on success and 1 on a (future) error path.
    fn returns_error(&self, value: &Operand) -> bool {
        match value {
            Operand::Const(Const::ErrVal { .. }) => true,
            Operand::Copy(p) if p.proj.is_empty() => {
                matches!(
                    self.prog.arena.get(self.func.locals[p.base.index()].ty),
                    Type::ErrorUnion { .. } | Type::ErrorSet(_) | Type::AnyError
                )
            }
            _ => false,
        }
    }

    /// Lowers a `Trap`: write a fixed `panic: <reason>\n` line to stderr (fd 2)
    /// then `exit(134)`, so native panics are observable and the exit code
    /// matches the VM.
    fn lower_trap(&mut self, reason: TrapReason, rodata: &mut RoData) -> Result<(), CodegenError> {
        let msg = format!("panic: {}\n", trap_message(reason));
        let bytes = msg.into_bytes();
        let len = bytes.len();
        let data_off = rodata.intern(&bytes);
        // write(2, msg, len).
        self.asm.mov_ri(Gpr::Rdi, 2);
        self.asm.mov_ri_data(Gpr::Rsi, data_off);
        self.asm.mov_ri(Gpr::Rdx, len as i64);
        self.asm.mov_ri(Gpr::Rax, sys::WRITE);
        self.asm.syscall();
        // exit(134).
        self.asm.mov_ri(Gpr::Rdi, PANIC_EXIT);
        self.asm.mov_ri(Gpr::Rax, sys::EXIT);
        self.asm.syscall();
        Ok(())
    }
}

/// Which arithmetic op an overflow predicate guards.
#[derive(Clone, Copy)]
enum ArithKind {
    Add,
    Sub,
    Mul,
}

/// The signed `MIN` value of a repr, as a 64-bit two's-complement pattern.
fn type_min(r: Repr) -> i64 {
    if r.width == 0 || r.width >= 64 {
        i64::MIN
    } else {
        -(1i64 << (r.width - 1))
    }
}

/// `true` for a relational/equality operator (its result is `bool`).
fn is_comparison(op: BinOp) -> bool {
    use BinOp::*;
    matches!(op, Eq | Ne | Lt | Le | Gt | Ge)
}

/// The condition code a comparison maps to, picking the signed or unsigned
/// variant for the ordering operators (equality is signedness-agnostic).
fn compare_cc(op: BinOp, signed: bool) -> Cc {
    use BinOp::*;
    match op {
        Eq => Cc::E,
        Ne => Cc::Ne,
        Lt => {
            if signed {
                Cc::L
            } else {
                Cc::B
            }
        }
        Le => {
            if signed {
                Cc::Le
            } else {
                Cc::Be
            }
        }
        Gt => {
            if signed {
                Cc::G
            } else {
                Cc::A
            }
        }
        Ge => {
            if signed {
                Cc::Ge
            } else {
                Cc::Ae
            }
        }
        _ => unreachable!("compare_cc on a non-comparison op"),
    }
}

/// Renders a format string's escaped braces (`{{` -> `{`, `}}` -> `}`) into
/// literal bytes, returning `None` if a real `{...}` placeholder is present
/// (which would need runtime formatting — out of the v0.14 subset).
fn render_braces(fmt: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(fmt.len());
    let mut i = 0;
    while i < fmt.len() {
        match fmt[i] {
            b'{' => {
                if i + 1 < fmt.len() && fmt[i + 1] == b'{' {
                    out.push(b'{');
                    i += 2;
                } else {
                    return None; // a real placeholder
                }
            }
            b'}' => {
                if i + 1 < fmt.len() && fmt[i + 1] == b'}' {
                    out.push(b'}');
                    i += 2;
                } else {
                    // A bare `}` is passed through literally (matches the VM).
                    out.push(b'}');
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    Some(out)
}

/// The human-readable reason string for a trap, matching the VM's panic wording
/// closely enough for an observable stderr line (the exit code is the asserted
/// differential; the message is advisory).
fn trap_message(reason: TrapReason) -> &'static str {
    match reason {
        TrapReason::Bounds => "index out of bounds",
        TrapReason::Overflow => "integer overflow",
        TrapReason::DivByZero => "division by zero",
        TrapReason::NegOverflow => "negation overflow",
        TrapReason::NarrowLoss => "cast truncates value",
        TrapReason::LenMismatch => "for-loop length mismatch",
        TrapReason::Unreachable => "reached unreachable code",
        TrapReason::Panic => "panic",
    }
}

/// A short name for an rvalue kind (used in `Unsupported` messages).
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

/// Returns `args[i]` or an `Unsupported` error (a malformed intrinsic arg list).
fn arg(args: &[Operand], i: usize) -> Result<&Operand, CodegenError> {
    args.get(i)
        .ok_or_else(|| CodegenError::Unsupported(format!("intrinsic missing argument {i}")))
}
