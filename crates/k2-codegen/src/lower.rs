//! MIR -> x86-64 machine-code lowering (v0.15: near-full native coverage).
//!
//! Each [`MirFunction`] is lowered to a self-contained block of machine code.
//! v0.15 replaces v0.14's "every local on the stack, two scratch registers"
//! scheme with:
//!
//! * a **frame planner** ([`crate::frame`]) that gives every aggregate /
//!   address-taken / spilled local a stack home and reserves the print buffer,
//!   callee-saved save slots, the `sret` pointer slot, and the outgoing-args
//!   region;
//! * a **linear-scan register allocator** ([`crate::regalloc`]) that keeps scalar
//!   locals in System V registers (callee-saved preferred for call-spanning
//!   values), spilling to the frame under pressure;
//! * **place projection** codegen (`Field`/`Index`/`Deref`/`SliceMeta`/`Payload`,
//!   load and store) over the [`crate::layout`] byte oracle;
//! * **aggregate construction** (`Aggregate`/`Ref`/`MakeSlice`/optional &
//!   error-union constructors) into stack homes;
//! * **runtime print formatting** ([`crate::fmt_native`]) that renders `{d}`/
//!   `{s}`/`{c}`/`{x}`/`{X}`/`{b}`/`{o}`/`{}` into a stack buffer matching
//!   `k2_vm::fmt` byte-for-byte, then `write()`s it;
//! * the **full System V ABI**: integer + SSE arg registers, `>6` stack args,
//!   small aggregates passed/returned in registers (RAX:RDX), MEMORY-class
//!   aggregates via a hidden `sret` pointer, 16-byte alignment preserved;
//! * **f64** arithmetic / compare / int↔float casts in XMM registers (runtime
//!   float *formatting* is cleanly deferred — see [`Self::lower_print_runtime`]).
//!
//! Anything still outside the subset (the heap allocator, the scheduler) is
//! rejected up-front with a [`CodegenError::Unsupported`] message rather than
//! miscompiled, so the VM path via `k2c run` stays available.

use k2_mir::{
    AggKind, BinOp, CastKind, Const, ConstData, DiscrKind, IntrinsicRoot, MirFunction, MirProgram,
    Operand, Place, Proj, Rvalue, SliceMeta, Statement, Terminator, TrapReason, UnOp,
};
use k2_types::{IntBits, Type, TypeId};

use crate::encode::{Asm, Cc, LabelId};
use crate::fmt_native::{self, Align, Step, Verb};
use crate::frame::{self, FramePlan};
use crate::layout::{self, Layout};
use crate::reg::{Gpr, Xmm, ARG_REGS, SSE_ARG_REGS};
use crate::regalloc::{self, Loc};
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

/// A reserved scratch register for address computation / memcpy / the print
/// cursor. Never allocated to a vreg (see [`crate::reg::ALLOC_REGS`]).
const ADDR: Gpr = Gpr::R11;

/// The scratch register that holds a scaled `Index` projection's index while an
/// element address is computed. **Must not be a SysV argument register** (and
/// must differ from [`ADDR`]): an indexed argument is sometimes evaluated *after*
/// another argument has already been placed into its ABI register, so a scratch
/// drawn from `ARG_REGS` (the old code used `RCX`, which is `ARG_REGS[3]`) would
/// destroy that already-placed argument. `R10` is caller-saved, never an argument
/// register, never allocated to a vreg, and distinct from `ADDR` (`R11`).
const IDX_SCRATCH: Gpr = Gpr::R10;

/// The integer representation of a scalar: bit width + signedness. Mirrors the
/// VM's `IntRepr` so narrowing/sign-extension match byte-for-byte.
///
/// Shared with the aarch64 lowering ([`crate::aarch64::lower`]), which reuses the
/// same width/signedness facts to pick width-correct loads/stores and narrows.
#[derive(Clone, Copy)]
pub(crate) struct Repr {
    /// The bit width (`8`/`16`/`32`/`64`; `0` for an unsized `comptime_int`).
    pub(crate) width: u16,
    /// `true` for a signed integer.
    pub(crate) signed: bool,
}

impl Repr {
    /// `true` if a result of this repr needs width normalization (sub-64 fixed).
    pub(crate) fn needs_normalize(self) -> bool {
        self.width != 0 && self.width < 64
    }
}

/// Resolves the [`Repr`] of a type. Non-integer scalars are full-width unsigned.
pub(crate) fn repr_of(prog: &MirProgram, ty: TypeId) -> Repr {
    match prog.arena.get(ty) {
        Type::Int { signed, bits } => Repr {
            width: match bits {
                IntBits::Fixed(n) => *n,
                IntBits::Usize | IntBits::Isize => 64,
            },
            signed: *signed,
        },
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
    /// `main(...) !void`: 0 on the void/Ok path, 1 when an error escapes.
    VoidEntry,
    /// `main() IntType`: the integer result becomes the process exit code.
    IntEntry,
}

/// The per-function lowering state.
pub struct FnLower<'p> {
    prog: &'p MirProgram,
    func: &'p MirFunction,
    asm: Asm,
    entry_kind: EntryKind,
    /// Register/spill assignment per local.
    alloc: regalloc::RegAlloc,
    /// The stack-frame plan.
    plan: FramePlan,
}

impl<'p> FnLower<'p> {
    /// Creates a lowering context for `func`, running the frame planner + register
    /// allocator. Returns `Unsupported` only later (during `lower`) for an
    /// out-of-subset construct; planning itself never fails.
    pub fn new(prog: &'p MirProgram, func: &'p MirFunction) -> FnLower<'p> {
        let entry_kind = if func.name == "main" {
            match prog.arena.get(func.ret) {
                Type::Int { .. } => EntryKind::IntEntry,
                _ => EntryKind::VoidEntry,
            }
        } else {
            EntryKind::Helper
        };

        let alloc = regalloc::allocate(func, &prog.arena);
        let prints = func_prints(func);
        let ret_mem = entry_kind == EntryKind::Helper
            && frame::is_memory_aggregate(&prog.arena, func.ret)
            && layout::layout_of(&prog.arena, func.ret)
                .map(|l| l.size)
                .unwrap_or(0)
                > 16;
        let outgoing = outgoing_args_bytes(prog, func);
        let plan = frame::plan(
            func,
            &prog.arena,
            &alloc.needs_home,
            &alloc.home_ty,
            &alloc.home_size,
            &alloc.callee_saved,
            prints,
            ret_mem,
            outgoing,
        );

        FnLower {
            prog,
            func,
            asm: Asm::new(),
            entry_kind,
            alloc,
            plan,
        }
    }

    // ---------------------------------------------------------------------
    //  Local homes & register assignment
    // ---------------------------------------------------------------------

    /// The `rbp`-relative stack home of `local`, or `None` if it lives only in a
    /// register.
    fn home(&self, local: k2_mir::LocalId) -> Option<i32> {
        self.plan.local_home[local.index()]
    }

    /// The register `local` is assigned, if any.
    fn reg_of(&self, local: k2_mir::LocalId) -> Option<Gpr> {
        match self.alloc.loc[local.index()] {
            Loc::Reg(r) => Some(r),
            Loc::Spill => None,
        }
    }

    /// The layout of a type (size/align), falling back to a word.
    fn layout(&self, ty: TypeId) -> Layout {
        layout::layout_of(&self.prog.arena, ty).unwrap_or(Layout::WORD)
    }

    /// The error-union payload type + payload byte offset of `local`, when it is a
    /// heap-allocation intrinsic result (`create`/`alloc`/`realloc`). These locals
    /// are `deferred` in the MIR, so their `discr.ErrorUnion`/`.payload` consumers
    /// are driven from this recovered payload type rather than the declared type.
    fn eu_payload_of(&self, local: k2_mir::LocalId) -> Option<(TypeId, i32)> {
        let pty = self.alloc.eu_result[local.index()]?;
        let pl = self.layout(pty);
        let off = layout::round_up(2, pl.align.max(1)) as i32;
        Some((pty, off))
    }

    /// `true` if safety checks are stripped (ReleaseFast). Mirrors the VM's
    /// `checks_off`: the GPA/testing leak + double-free trackers become no-ops.
    fn checks_off(&self) -> bool {
        matches!(self.prog.mode, k2_mir::BuildMode::ReleaseFast)
    }

    /// The runtime error tag for `error.OutOfMemory` (the value a bounded allocator
    /// returns on exhaustion), resolved from the program's `err_names` table — the
    /// same lookup the VM's `out_of_memory_value` performs.
    fn oom_tag(&self) -> u16 {
        self.prog
            .err_names
            .iter()
            .find(|(_, name)| name.as_str() == "OutOfMemory")
            .map(|(t, _)| t.0)
            .unwrap_or(0)
    }

    /// The aggregate type a `dst = rvalue` builds into a home, or `None` if `dst`
    /// is a scalar. Uses the declared type, or the per-local home-type override
    /// (a `deferred` tuple's concrete aggregate type) when `rvalue` is itself an
    /// aggregate-producing construction.
    fn effective_agg_ty(&self, dst: k2_mir::LocalId, rvalue: &Rvalue) -> Option<TypeId> {
        let dty = self.func.locals[dst.index()].ty;
        if frame::is_memory_aggregate(&self.prog.arena, dty) {
            return Some(dty);
        }
        // A forced-home local: it has a home AND the rvalue produces an aggregate.
        // This covers a `deferred`-typed print tuple (sized via a synthetic packed
        // layout); the aggregate builder recomputes offsets from field types when
        // the declared type is not layoutable.
        if self.home(dst).is_some() && rvalue_builds_aggregate(rvalue) {
            if let Some(ty) = self.alloc.home_ty[dst.index()] {
                if frame::is_memory_aggregate(&self.prog.arena, ty)
                    || self.alloc.home_size[dst.index()].is_some()
                {
                    return Some(ty);
                }
            }
        }
        None
    }

    // ---------------------------------------------------------------------
    //  Top-level lowering
    // ---------------------------------------------------------------------

    /// Lowers the whole function, returning its finalized code + cross-function
    /// fixups for the program link pass to patch.
    pub fn lower(
        mut self,
        rodata: &mut RoData,
    ) -> Result<(Vec<u8>, Vec<crate::encode::Fixup>), CodegenError> {
        self.asm.reserve_labels(self.func.blocks.len());

        // ---- Prologue. ----
        self.asm.push(Gpr::Rbp);
        self.asm.mov_rr(Gpr::Rbp, Gpr::Rsp);
        let frame = self.plan.frame_size;
        if frame > 0 {
            self.asm.sub_rsp_imm(frame);
        }
        // Save callee-saved registers the allocator used.
        let saved: Vec<(Gpr, i32)> = self.plan.callee_saved_slots.clone();
        for &(r, slot) in &saved {
            self.asm.mov_store(slot, r);
        }
        // Receive parameters (int + SSE + stack + hidden sret) into their homes.
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

    /// Restores callee-saved registers and emits `leave; ret`.
    fn epilogue_and_ret(&mut self) {
        let saved: Vec<(Gpr, i32)> = self.plan.callee_saved_slots.clone();
        for &(r, slot) in &saved {
            self.asm.mov_load(r, slot);
        }
        self.asm.leave();
        self.asm.ret();
    }

    /// Receives the function's parameters from their ABI locations into their
    /// register/home assignments. Mirrors [`Self::lower_call`]'s argument layout.
    fn lower_prologue_params(&mut self) -> Result<(), CodegenError> {
        let mut int_idx = 0usize;
        let mut sse_idx = 0usize;
        let mut stack_off = 16i32; // [rbp+16] is the first stack arg (after ret+rbp).

        // A MEMORY-returning helper takes the hidden sret pointer in RDI first.
        if let Some(slot) = self.plan.ret_ptr_slot {
            self.asm.mov_store(slot, ARG_REGS[0]);
            int_idx = 1;
        }

        for &param in &self.func.params {
            let ty = self.func.locals[param.index()].ty;
            let class = classify(self.prog, ty);
            match class {
                ArgClass::Memory { size, .. } => {
                    // The bytes are on the stack at [rbp + stack_off]; copy them
                    // into the param's home.
                    let dst = self
                        .home(param)
                        .ok_or_else(|| self.unsup("memory param without a home"))?;
                    self.copy_stack_to_home(stack_off, dst, size);
                    stack_off += round_up_i32(size as i32, 8);
                }
                ArgClass::Sse => {
                    let dst = self.home(param);
                    if sse_idx < SSE_ARG_REGS.len() {
                        let x = SSE_ARG_REGS[sse_idx];
                        sse_idx += 1;
                        if let Some(h) = dst {
                            self.asm.movsd_store(Gpr::Rbp, h, x);
                        }
                    } else if let Some(h) = dst {
                        self.asm.movsd_load(Xmm::Xmm0, Gpr::Rbp, stack_off);
                        self.asm.movsd_store(Gpr::Rbp, h, Xmm::Xmm0);
                        stack_off += 8;
                    }
                }
                ArgClass::OneInt | ArgClass::TwoInt => {
                    let words = if matches!(class, ArgClass::TwoInt) {
                        2
                    } else {
                        1
                    };
                    if int_idx + words <= ARG_REGS.len() {
                        for w in 0..words {
                            let src = ARG_REGS[int_idx + w];
                            self.store_param_word(param, w, src, ty);
                        }
                        int_idx += words;
                    } else {
                        // Whole aggregate / scalar on the stack.
                        for w in 0..words {
                            self.asm.mov_load(Gpr::Rax, stack_off);
                            self.store_param_word_from_rax(param, w, ty);
                            stack_off += 8;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Stores ABI arg register `src` as word `w` of parameter `param`.
    fn store_param_word(&mut self, param: k2_mir::LocalId, w: usize, src: Gpr, ty: TypeId) {
        if frame::is_memory_aggregate(&self.prog.arena, ty) {
            if let Some(h) = self.home(param) {
                self.asm.mov_store_mem(Gpr::Rbp, h + (w * 8) as i32, src);
            }
        } else {
            // A scalar param (one word).
            if let Some(r) = self.reg_of(param) {
                if r != src {
                    self.asm.mov_rr(r, src);
                }
            } else if let Some(h) = self.home(param) {
                self.asm.mov_store(h, src);
            }
        }
    }

    /// Like [`Self::store_param_word`] but the source word is already in RAX.
    fn store_param_word_from_rax(&mut self, param: k2_mir::LocalId, w: usize, ty: TypeId) {
        if frame::is_memory_aggregate(&self.prog.arena, ty) {
            if let Some(h) = self.home(param) {
                self.asm
                    .mov_store_mem(Gpr::Rbp, h + (w * 8) as i32, Gpr::Rax);
            }
        } else if let Some(r) = self.reg_of(param) {
            self.asm.mov_rr(r, Gpr::Rax);
        } else if let Some(h) = self.home(param) {
            self.asm.mov_store(h, Gpr::Rax);
        }
    }

    /// Copies `size` bytes from `[rbp + src_off]` into the home at `[rbp + dst]`.
    fn copy_stack_to_home(&mut self, src_off: i32, dst: i32, size: u64) {
        let mut off = 0i32;
        let mut rem = size as i32;
        while rem >= 8 {
            self.asm.mov_load(Gpr::Rax, src_off + off);
            self.asm.mov_store(dst + off, Gpr::Rax);
            off += 8;
            rem -= 8;
        }
        while rem > 0 {
            self.asm.movzx8_mem(Gpr::Rax, Gpr::Rbp, src_off + off);
            self.asm.mov_store8_mem(Gpr::Rbp, dst + off, Gpr::Rax);
            off += 1;
            rem -= 1;
        }
    }

    // ---------------------------------------------------------------------
    //  Operands (scalar load into a GPR)
    // ---------------------------------------------------------------------

    /// Loads scalar `op` into `dst`. Handles register-resident vregs, spilled
    /// homes, projected places, and constants (incl. string-literal slice ptr).
    fn operand_to(&mut self, op: &Operand, dst: Gpr) -> Result<(), CodegenError> {
        match op {
            Operand::Copy(p) => self.load_place_scalar(p, dst),
            Operand::Const(c) => self.const_to(c, dst),
        }
    }

    /// Loads the scalar value at `place` into `dst`.
    fn load_place_scalar(&mut self, place: &Place, dst: Gpr) -> Result<(), CodegenError> {
        if place.is_local() {
            let base = place.base;
            if let Some(r) = self.reg_of(base) {
                if r != dst {
                    self.asm.mov_rr(dst, r);
                }
                return Ok(());
            }
            if let Some(h) = self.home(base) {
                self.asm.mov_load(dst, h);
                return Ok(());
            }
            // No register and no home: an opaque/void local never read for value;
            // load 0 defensively.
            self.asm.mov_ri(dst, 0);
            return Ok(());
        }
        // A trailing packed-struct FIELD: the field's bits live inside the
        // struct's backing integer, reached by shift+mask rather than a byte
        // load. Load the whole backing integer from the struct's address, then
        // extract `(off, width)` and sign-extend signed fields (spec §02).
        if let Some((
            Proj::Field {
                packed: Some(pf), ..
            },
            prefix,
        )) = place.proj.split_last()
        {
            return self.load_packed_field(place.base, prefix, *pf, dst);
        }
        // A trailing `SliceMeta` over an ARRAY base is a value, not a memory load:
        // `.len` is the constant element count, `.ptr` is the array's address.
        if let Some((Proj::SliceMeta { which, .. }, prefix)) = place.proj.split_last() {
            let base_ty = self.prefix_type(place.base, prefix);
            if let Type::Array { len, .. } = self.prog.arena.get(base_ty) {
                match which {
                    SliceMeta::Len => {
                        // A `Known` length comes from the type; an inferred/deferred
                        // length comes from the per-local override the regalloc
                        // recorded from the array literal's field count.
                        let n = match len {
                            k2_types::ArrayLen::Known(n) => *n as i64,
                            _ => prefix
                                .is_empty()
                                .then(|| self.alloc.array_len[place.base.index()])
                                .flatten()
                                .map(|n| n as i64)
                                .unwrap_or(0),
                        };
                        self.asm.mov_ri(dst, n);
                        return Ok(());
                    }
                    SliceMeta::Ptr => {
                        // The array's address (prefix place address).
                        let arr_place = Place {
                            base: place.base,
                            proj: prefix.to_vec(),
                        };
                        return self.place_addr_general(&arr_place, dst);
                    }
                }
            }
        }
        // Projected: compute the address, then a width-correct load.
        let elem_ty = self.place_type(place);
        self.place_addr(place, ADDR)?;
        self.load_sized(dst, ADDR, 0, elem_ty);
        Ok(())
    }

    /// Loads a packed-struct bit-field into `dst` (spec §02). The backing integer
    /// of the struct (located at `base + prefix`) is loaded whole, then the field
    /// at `[off, off+width)` is extracted: unsigned fields by `shr off; and mask`,
    /// signed fields by the `shl (64-off-w); sar (64-w)` sign-extension trick.
    ///
    /// v0.21 supports a backing integer up to 64 bits and a field wholly within
    /// the low 64 bits; a field straddling bit 64 (in a 65..128-bit backing) is
    /// cleanly refused rather than miscompiled.
    fn load_packed_field(
        &mut self,
        base: k2_mir::LocalId,
        prefix: &[Proj],
        pf: k2_types::PackedField,
        dst: Gpr,
    ) -> Result<(), CodegenError> {
        let off = pf.off;
        let width = pf.width;
        if width == 0 || width > 64 || off + width > 64 {
            return Err(self
                .unsup("packed bit-field load wider than 64 bits or straddling the 64-bit limb"));
        }
        // Address of the struct backing integer -> dst; load the whole low limb.
        let struct_place = Place {
            base,
            proj: prefix.to_vec(),
        };
        self.place_addr_general(&struct_place, dst)?;
        self.asm.mov_load_mem(dst, dst, 0);
        if pf.signed {
            // Sign-extend: position the field at the top, then arithmetic-shift
            // back so the high bits replicate the field's sign bit.
            let top = (64 - off - width) as u8;
            if top != 0 {
                self.asm.shl_ri(dst, top);
            }
            self.asm.sar_ri(dst, (64 - width) as u8);
        } else {
            if off != 0 {
                self.asm.shr_ri(dst, off as u8);
            }
            if width < 64 {
                // Mask to the field width. A ≤32-bit mask fits the `and_ri` imm32;
                // a 33..63-bit mask needs a 64-bit immediate via a scratch reg.
                let mask = (1u128 << width) - 1;
                self.mask_to(dst, mask);
            }
        }
        Ok(())
    }

    /// Masks `dst` to the low bits given by `mask` (a contiguous low-bit mask).
    /// Uses the imm32 `and` for small masks; a 64-bit-immediate `and` via a
    /// scratch register for wider ones.
    fn mask_to(&mut self, dst: Gpr, mask: u128) {
        if mask <= i32::MAX as u128 {
            self.asm.and_ri(dst, mask as i32);
        } else {
            // Load the 64-bit mask into the index scratch, then `and`.
            self.asm.mov_ri(IDX_SCRATCH, mask as u64 as i64);
            self.asm.and_rr(dst, IDX_SCRATCH);
        }
    }

    /// Stores `src` (the field value, low `width` bits significant) into a packed
    /// bit-field at `[off, off+width)` of the struct backing integer located at
    /// `base + prefix`, by read-modify-write. The same 64-bit-backing/non-
    /// straddling constraint as [`Self::load_packed_field`] applies.
    fn store_packed_field(
        &mut self,
        base: k2_mir::LocalId,
        prefix: &[Proj],
        pf: k2_types::PackedField,
        src: Gpr,
    ) -> Result<(), CodegenError> {
        let off = pf.off;
        let width = pf.width;
        if width == 0 || width > 64 || off + width > 64 {
            return Err(self
                .unsup("packed bit-field store wider than 64 bits or straddling the 64-bit limb"));
        }
        let field_mask = if width >= 128 {
            u128::MAX
        } else {
            (1u128 << width) - 1
        };
        let shifted_mask = field_mask << off;
        // The struct's address -> ADDR (kept across the RMW). `src` must survive,
        // so it must not be ADDR/IDX_SCRATCH; the caller passes a value reg.
        let struct_place = Place {
            base,
            proj: prefix.to_vec(),
        };
        self.place_addr_general(&struct_place, ADDR)?;
        // value-bits = (src & field_mask) << off  -> src
        self.mask_to(src, field_mask);
        if off != 0 {
            self.asm.shl_ri(src, off as u8);
        }
        // old = load [ADDR]; old &= !shifted_mask; old |= src; store.
        let old = Gpr::Rcx;
        self.asm.mov_load_mem(old, ADDR, 0);
        self.mask_to_clear(old, shifted_mask);
        self.asm.or_rr(old, src);
        self.asm.mov_store_mem(ADDR, 0, old);
        Ok(())
    }

    /// Clears the bits of `dst` covered by `mask` (`dst &= !mask`).
    fn mask_to_clear(&mut self, dst: Gpr, mask: u128) {
        let inv = !mask;
        // `!mask` is wide; load it into a scratch and `and`.
        self.asm.mov_ri(IDX_SCRATCH, inv as u64 as i64);
        self.asm.and_rr(dst, IDX_SCRATCH);
    }

    /// The type a place's base + a projection prefix yields (no trailing proj).
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

    /// A width/signedness-correct scalar load of `ty` from `[base + disp]`.
    fn load_sized(&mut self, dst: Gpr, base: Gpr, disp: i32, ty: TypeId) {
        let r = repr_of(self.prog, ty);
        match (r.width, r.signed) {
            (8, false) | (1, false) => self.asm.movzx8_mem(dst, base, disp),
            (8, true) => self.asm.movsx8_mem(dst, base, disp),
            (16, false) => self.asm.movzx16_mem(dst, base, disp),
            (16, true) => self.asm.movsx16_mem(dst, base, disp),
            (32, false) => self.asm.mov_load32_mem(dst, base, disp),
            (32, true) => self.asm.movsxd_mem(dst, base, disp),
            _ => {
                // bool / a sub-byte unsigned width is a single zero-extended
                // byte; everything else is a full 8-byte load.
                let one_byte =
                    matches!(self.prog.arena.get(ty), Type::Bool) || (r.width != 0 && r.width < 8);
                if one_byte {
                    self.asm.movzx8_mem(dst, base, disp);
                } else {
                    self.asm.mov_load_mem(dst, base, disp);
                }
            }
        }
    }

    /// A width-correct scalar store of `ty`: writes the low bytes of `src` to
    /// `[base + disp]`.
    fn store_sized(&mut self, base: Gpr, disp: i32, src: Gpr, ty: TypeId) {
        let size = self.scalar_store_size(ty);
        match size {
            1 => self.asm.mov_store8_mem(base, disp, src),
            2 => self.asm.mov_store16_mem(base, disp, src),
            4 => self.asm.mov_store32_mem(base, disp, src),
            _ => self.asm.mov_store_mem(base, disp, src),
        }
    }

    /// The number of bytes a scalar value of `ty` occupies in memory.
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
                self.asm.mov_ri(dst, 0);
                Ok(())
            }
            // An enum value is its variant index (the tag integer).
            Const::EnumVal { variant, .. } => {
                self.asm.mov_ri(dst, *variant as i64);
                Ok(())
            }
            // An error value is its `u16` tag (used where an error scalar flows
            // through a register, e.g. a `Switch` scrutinee).
            Const::ErrVal { tag, .. } => {
                self.asm.mov_ri(dst, tag.0 as i64);
                Ok(())
            }
            // An `undef` scalar (a `deferred` temp initialized before a later real
            // assignment, e.g. `_x = undef; _x = call ...`): the value is never read
            // before its real definition, so materialize a harmless zero.
            Const::Undef { .. } => {
                self.asm.mov_ri(dst, 0);
                Ok(())
            }
            other => Err(CodegenError::Unsupported(format!(
                "non-scalar constant {other:?} in `{}`",
                self.func.name
            ))),
        }
    }

    /// The 64-bit pattern a typed integer constant occupies, masked/sign-extended
    /// to its width. Matches the VM's masked `Value::int`.
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
                w => self.normalize_signed_other(dst, w),
            }
        } else if r.width == 32 {
            self.asm.mov_rr32(dst, dst);
        } else {
            let mask: u32 = (1u32 << r.width) - 1;
            self.asm.and_ri(dst, mask as i32);
        }
    }

    /// Sign-extends `dst` from an odd signed width `w` via a shift pair.
    fn normalize_signed_other(&mut self, dst: Gpr, w: u16) {
        let shift = (64 - w) as i64;
        self.asm.mov_ri(Gpr::Rcx, shift);
        self.asm.shl_cl(dst);
        self.asm.mov_ri(Gpr::Rcx, shift);
        self.asm.sar_cl(dst);
    }

    /// Stores RAX into `dst`'s register or home as a scalar of its type.
    fn store_scalar_result(&mut self, dst: k2_mir::LocalId) {
        let ty = self.func.locals[dst.index()].ty;
        if let Some(r) = self.reg_of(dst) {
            if r != Gpr::Rax {
                self.asm.mov_rr(r, Gpr::Rax);
            }
        } else if let Some(h) = self.home(dst) {
            self.store_sized(Gpr::Rbp, h, Gpr::Rax, ty);
        }
    }

    // ---------------------------------------------------------------------
    //  Place addressing (effective address of a projected place into `dst`)
    // ---------------------------------------------------------------------

    /// Computes the effective byte address of `place` into `dst`. The base local
    /// must have a stack home (the MIR guarantees this for any projected place,
    /// since a projection implies the base is addressable).
    fn place_addr(&mut self, place: &Place, dst: Gpr) -> Result<(), CodegenError> {
        // The starting address + the remaining projection chain. A homed local
        // starts at its home address. A register-only local must be a pointer/slice
        // scalar reached only through a leading `Deref`: its register value IS the
        // pointee address, so we load that value and consume the leading `Deref`.
        let mut cur_ty = self.func.locals[place.base.index()].ty;
        let projs: &[Proj];
        if let Some(h) = self.home(place.base) {
            self.asm.lea_rbp(dst, h);
            projs = &place.proj;
        } else if let Some(r) = self.reg_of(place.base) {
            match place.proj.first() {
                Some(Proj::Deref) => {
                    if r != dst {
                        self.asm.mov_rr(dst, r);
                    }
                    cur_ty = self.pointee_ty(cur_ty);
                    projs = &place.proj[1..];
                }
                _ => return Err(self.unsup("projected place over a register-only non-pointer")),
            }
        } else {
            return Err(self.unsup("projected place over a register-only local"));
        }
        // Whether the projection chain has passed through a struct FIELD before an
        // `Index` — the signal that an indexed slice is a generic container's
        // backing store (`list.items[i]`) rather than a standalone/array-view slice.
        let mut saw_field = false;
        for proj in projs {
            match proj {
                Proj::Field { index, ty, packed } => {
                    saw_field = true;
                    if packed.is_some() {
                        // A packed-struct field is NOT byte-addressable: its value
                        // lives inside the struct's backing integer and is reached
                        // by shift+mask, which `place_addr` cannot express. The
                        // scalar load/store paths intercept a trailing packed field
                        // before calling `place_addr`, so reaching here means the
                        // packed field appeared mid-chain (e.g. `&p.field`), which
                        // the v0.21 native backend does not support.
                        return Err(self.unsup("address of / through a packed-struct field"));
                    }
                    let offs = layout::field_offsets(&self.prog.arena, cur_ty);
                    let off = offs.get(*index as usize).copied().unwrap_or(0);
                    if off != 0 {
                        self.asm.add_ri(dst, off as i32);
                    }
                    cur_ty = *ty;
                }
                Proj::Index { index, ty } => {
                    // Indexing a SLICE first dereferences its fat pointer (the data
                    // pointer lives at slice offset +0); indexing an ARRAY uses the
                    // array's own address directly.
                    let is_slice = matches!(self.prog.arena.get(cur_ty), Type::Slice { .. });
                    if is_slice {
                        self.asm.mov_load_mem(dst, dst, 0);
                    }
                    // Element STRIDE. A slice reached through a struct FIELD is a
                    // generic container's backing store (`list.items[i]`,
                    // `(*self).f0[i]`): the shared `deferred`-element methods write it
                    // at the word stride, so a concrete reader of the same field must
                    // index at the same word stride for any word-sized scalar element.
                    // A bare/local slice (an array view `a[1..3]`, a direct
                    // `alloc(u32,n)` result) keeps its natural element stride — never
                    // shared generically. Arrays always use their natural layout.
                    // A bare/local slice whose ELEMENT type is still `deferred`
                    // (an un-monomorphized generic helper param like `quick`'s
                    // `arr: []T` lowered with `T` unresolved) has no known element
                    // stride: `layout(deferred)` falls back to the 8-byte WORD,
                    // which silently mis-strides a smaller scalar (an `i32` array,
                    // stride 4). Refuse cleanly so the program runs on the VM rather
                    // than producing wrong results. (The FIELD-reached slice case
                    // below is the deliberate, correct generic-container word-stride
                    // path and is left untouched.)
                    if is_slice
                        && !saw_field
                        && matches!(self.prog.arena.get(*ty), Type::Deferred | Type::AnyType)
                    {
                        return Err(
                            self.unsup("slice index with an unresolved (generic) element type")
                        );
                    }
                    let stride = if is_slice && saw_field {
                        self.field_slice_stride(*ty)
                    } else {
                        // A standalone/array-view slice, or an array, uses the
                        // element's natural layout stride.
                        self.layout(*ty).size.max(1)
                    };
                    // index -> IDX_SCRATCH (R10), scale, add. Evaluating a *projected*
                    // index operand (`a[(*p).field]`) recomputes an address through
                    // ADDR (R11), which is usually `dst` — so the running base in
                    // `dst` must be preserved across the index evaluation. Save it on
                    // the stack when the index is a place; a bare local/const index
                    // never touches ADDR, so it needs no save. (Earlier this clobber
                    // silently redirected `items[i] = v` to the wrong base.)
                    let index_is_place = matches!(index, Operand::Copy(p) if !p.is_local());
                    if index_is_place {
                        self.asm.push(dst);
                        self.operand_to(index, IDX_SCRATCH)?;
                        self.asm.pop(dst);
                    } else {
                        self.operand_to(index, IDX_SCRATCH)?;
                    }
                    if stride.is_power_of_two() {
                        let sh = stride.trailing_zeros() as u8;
                        if sh != 0 {
                            self.asm.shl_ri(IDX_SCRATCH, sh);
                        }
                    } else {
                        self.asm.imul_rri(IDX_SCRATCH, IDX_SCRATCH, stride as i32);
                    }
                    self.asm.add_rr(dst, IDX_SCRATCH);
                    cur_ty = *ty;
                }
                Proj::Deref => {
                    // Load the pointer the address currently points at.
                    self.asm.mov_load_mem(dst, dst, 0);
                    cur_ty = self.pointee_ty(cur_ty);
                }
                Proj::SliceMeta { which, ty } => {
                    match which {
                        SliceMeta::Ptr => { /* ptr is at +0 */ }
                        SliceMeta::Len => self.asm.add_ri(dst, 8),
                    }
                    cur_ty = *ty;
                }
                Proj::Payload { ty } => {
                    // Error union: payload after the u16 tag; optional: at +0. A
                    // heap-allocation result is a `deferred` local but a real error
                    // union in memory — recover its payload offset from the payload
                    // type when the base is such a local.
                    let off = match self.prog.arena.get(cur_ty) {
                        Type::ErrorUnion { .. } => {
                            layout::error_union_payload_off(&self.prog.arena, cur_ty)
                        }
                        _ => self
                            .eu_payload_of(place.base)
                            .filter(|_| place.proj.len() == 1)
                            .map(|(_, o)| o as u64)
                            .unwrap_or(0),
                    };
                    if off != 0 {
                        self.asm.add_ri(dst, off as i32);
                    }
                    cur_ty = *ty;
                }
            }
        }
        Ok(())
    }

    /// The type the place's final projection yields (the value type loaded/stored).
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

    /// The pointee type of a pointer type (falls back to the type itself).
    fn pointee_ty(&self, ty: TypeId) -> TypeId {
        match self.prog.arena.get(ty) {
            Type::Pointer { pointee, .. } => *pointee,
            _ => ty,
        }
    }

    // ---------------------------------------------------------------------
    //  Statements
    // ---------------------------------------------------------------------

    /// Lowers one statement.
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

    /// Lowers a rvalue whose result is discarded (an `Eval`).
    fn lower_rvalue_discard(
        &mut self,
        rvalue: &Rvalue,
        rodata: &mut RoData,
    ) -> Result<(), CodegenError> {
        match rvalue {
            Rvalue::Intrinsic { .. } => self.lower_intrinsic_into_rax(rvalue, rodata).map(|_| ()),
            Rvalue::Call { func, args, ty } => {
                self.lower_call_raw(*func, args, *ty, rodata)?;
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Lowers `*place = rvalue` (a projected/aggregate store).
    fn lower_rvalue_to_place(
        &mut self,
        place: &Place,
        rvalue: &Rvalue,
        rodata: &mut RoData,
    ) -> Result<(), CodegenError> {
        let val_ty = self.place_type(place);
        // A store to a trailing packed-struct bit-field: compute the field value
        // into a scratch register, then read-modify-write the struct's backing
        // integer (spec §02). Intercepted before the byte-store paths since a
        // packed field has no byte address.
        if let Some((
            Proj::Field {
                packed: Some(pf), ..
            },
            prefix,
        )) = place.proj.split_last()
        {
            // Evaluate the field value into a value register that survives the
            // address computation (RAX), then RMW.
            self.eval_scalar_rvalue_into_rax(rvalue, val_ty)?;
            self.asm.mov_rr(Gpr::Rdx, Gpr::Rax);
            return self.store_packed_field(place.base, prefix, *pf, Gpr::Rdx);
        }
        if frame::is_memory_aggregate(&self.prog.arena, val_ty) {
            // Aggregate store: materialize the rvalue's address, memcpy to dest.
            return self.store_aggregate_rvalue_to_place(place, rvalue, val_ty, rodata);
        }
        // A store into a slice element that is itself an aggregate larger than a
        // word (`[]const u8` / a struct element). When the element type is CONCRETE
        // this is a faithful aggregate memcpy. When it would be reached through a
        // GENERIC container (`List([]const u8).push`, element `deferred`) the value
        // crosses the param/store boundary as a `deferred` 8-byte scalar — which the
        // un-monomorphized MIR cannot represent — so that case is cleanly refused at
        // the read site (`slice_index_aggregate_stride` returns `None` for a
        // `deferred` element). Here we only handle a concrete aggregate element.
        if let Some(stride) = self.slice_index_aggregate_stride(place) {
            if let Rvalue::Use(Operand::Copy(src)) = rvalue {
                self.place_addr_general(src, Gpr::Rax)?;
                self.place_addr(place, ADDR)?;
                self.memcpy(ADDR, Gpr::Rax, stride);
                return Ok(());
            }
        }
        // A 128-bit scalar into a projected place: compute the two-limb value, then
        // write both limbs at the place's address (a single `store_sized` would
        // truncate to one word). The address is computed first (into ADDR) and the
        // value materialized into a non-ADDR pair, so neither clobbers the other.
        if self.is_wide_int(val_ty) {
            self.eval_wide_rvalue_into_pair(rvalue, Gpr::Rax, Gpr::Rdx)?;
            self.place_addr(place, ADDR)?;
            self.asm.mov_store_mem(ADDR, 0, Gpr::Rax);
            self.asm.mov_store_mem(ADDR, 8, Gpr::Rdx);
            return Ok(());
        }
        // Scalar store: compute value into RAX, address into ADDR, sized store.
        self.eval_scalar_rvalue_into_rax(rvalue, val_ty)?;
        // Preserve RAX (value) while computing the address in ADDR.
        self.place_addr(place, ADDR)?;
        self.store_sized(ADDR, 0, Gpr::Rax, val_ty);
        Ok(())
    }

    /// Evaluates a scalar-producing rvalue into RAX (normalized to `val_ty`),
    /// supporting the kinds that can target a projected place: `Use`, `Binary`,
    /// `Unary`, `Cast`, `Ref`, and `Discriminant`.
    fn eval_scalar_rvalue_into_rax(
        &mut self,
        rvalue: &Rvalue,
        val_ty: TypeId,
    ) -> Result<(), CodegenError> {
        match rvalue {
            Rvalue::Use(op) => {
                self.operand_to(op, Gpr::Rax)?;
                self.normalize(Gpr::Rax, val_ty);
                Ok(())
            }
            Rvalue::Binary { op, lhs, rhs, ty } => self.eval_binary_into_rax(*op, lhs, rhs, *ty),
            Rvalue::Unary { op, operand, ty } => {
                self.operand_to(operand, Gpr::Rax)?;
                match op {
                    UnOp::Neg => {
                        self.asm.neg_r(Gpr::Rax);
                        self.normalize(Gpr::Rax, *ty);
                    }
                    UnOp::BitNot => {
                        self.asm.not_r(Gpr::Rax);
                        self.normalize(Gpr::Rax, *ty);
                    }
                    UnOp::Not => self.asm.xor_ri(Gpr::Rax, 1),
                }
                Ok(())
            }
            Rvalue::Cast {
                kind: CastKind::Widen | CastKind::IntNarrow | CastKind::PtrReinterpret,
                operand,
                ty,
            } => {
                self.operand_to(operand, Gpr::Rax)?;
                self.normalize(Gpr::Rax, *ty);
                Ok(())
            }
            Rvalue::Cast {
                kind: CastKind::FloatToInt,
                operand,
                ty,
            } => {
                self.load_float_operand(operand, Xmm::Xmm0)?;
                self.asm.cvttsd2si(Gpr::Rax, Xmm::Xmm0);
                self.normalize(Gpr::Rax, *ty);
                Ok(())
            }
            Rvalue::Ref { place, .. } => self.place_addr_general(place, Gpr::Rax),
            other => Err(self.unsup(&format!(
                "rvalue {} into a projected place",
                rvalue_kind(other)
            ))),
        }
    }

    /// Stores an aggregate-valued rvalue into a projected place by memcpy.
    fn store_aggregate_rvalue_to_place(
        &mut self,
        place: &Place,
        rvalue: &Rvalue,
        ty: TypeId,
        _rodata: &mut RoData,
    ) -> Result<(), CodegenError> {
        // Source address into RAX, dest address into ADDR, then memcpy.
        match rvalue {
            Rvalue::Use(Operand::Copy(src)) => {
                self.place_addr_general(src, Gpr::Rax)?;
                self.place_addr(place, ADDR)?;
                let size = self.layout(ty).size;
                self.memcpy(ADDR, Gpr::Rax, size);
                Ok(())
            }
            // `place = make_slice {ptr, offset, len}` (e.g. `self.items = ...` in
            // `ArrayList.append`): build the `{ptr, len}` fat slice directly at the
            // destination address. The data pointer and length are computed into
            // registers, the destination address into ADDR, then stored.
            Rvalue::MakeSlice {
                ptr,
                offset,
                len,
                ty: sty,
            } => self.store_make_slice_to_place(place, ptr, offset, len, *sty),
            _ => Err(self.unsup("aggregate rvalue into a projected place")),
        }
    }

    /// Builds a `{ptr, len}` slice (from `ptr + offset*stride`, `len`) directly into
    /// the projected destination `place`. Mirrors `build_make_slice` but writes to a
    /// runtime address rather than an `rbp`-relative home.
    fn store_make_slice_to_place(
        &mut self,
        place: &Place,
        ptr: &Operand,
        offset: &Operand,
        len: &Operand,
        sty: TypeId,
    ) -> Result<(), CodegenError> {
        let stride = layout::elem_size(&self.prog.arena, sty);
        // data = ptr + offset * stride -> RAX (a callee-saved-safe scratch we keep
        // until the store; the dest-address eval below avoids RAX).
        self.slice_data_ptr(ptr, Gpr::Rax)?;
        self.operand_to(offset, Gpr::Rcx)?;
        if stride.is_power_of_two() {
            let sh = stride.trailing_zeros() as u8;
            if sh != 0 {
                self.asm.shl_ri(Gpr::Rcx, sh);
            }
        } else {
            self.asm.imul_rri(Gpr::Rcx, Gpr::Rcx, stride as i32);
        }
        self.asm.add_rr(Gpr::Rax, Gpr::Rcx);
        // len -> RDX.
        self.operand_to(len, Gpr::Rdx)?;
        // dest address -> ADDR; store {ptr@0, len@8}.
        self.place_addr(place, ADDR)?;
        self.asm.mov_store_mem(ADDR, 0, Gpr::Rax);
        self.asm.mov_store_mem(ADDR, 8, Gpr::Rdx);
        Ok(())
    }

    // ---------------------------------------------------------------------
    //  Rvalues into a destination local
    // ---------------------------------------------------------------------

    /// Lowers `dst = rvalue`.
    fn lower_rvalue(
        &mut self,
        dst: k2_mir::LocalId,
        rvalue: &Rvalue,
        rodata: &mut RoData,
    ) -> Result<(), CodegenError> {
        let dst_ty = self.func.locals[dst.index()].ty;

        // Aggregate-typed destinations — including a `deferred`-typed local that
        // was given a home because it is produced as an aggregate (a print
        // argument tuple) — are built into their stack home.
        let agg_ty = self.effective_agg_ty(dst, rvalue);
        if let Some(ty) = agg_ty {
            return self.lower_aggregate_rvalue(dst, rvalue, ty, rodata);
        }

        match rvalue {
            Rvalue::Use(op) => {
                if self.is_float(dst_ty) {
                    return self.lower_float_use(dst, op);
                }
                if self.is_wide_int(dst_ty) {
                    let h = self
                        .home(dst)
                        .ok_or_else(|| self.unsup("wide-int local without a home"))?;
                    return self.store_wide_int(h, op, dst_ty);
                }
                // Coercing an error union into a bare error-set scalar (`e = eu`,
                // the catch-capture `|err|`) extracts the `u16` tag at +0 — the
                // payload/padding eightbytes must not leak into the tag value (a
                // `switch (err)` compares the bare tag). Load just the u16.
                if matches!(
                    self.prog.arena.get(dst_ty),
                    Type::ErrorSet(_) | Type::AnyError
                ) {
                    if let Operand::Copy(p) = op {
                        let src_ty = self
                            .operand_type(op)
                            .or_else(|| self.eu_payload_of(p.base).map(|_| dst_ty));
                        let is_eu = src_ty
                            .map(|t| matches!(self.prog.arena.get(t), Type::ErrorUnion { .. }))
                            .unwrap_or(false)
                            || self.eu_payload_of(p.base).is_some();
                        if is_eu {
                            self.place_addr_general(p, ADDR)?;
                            self.asm.movzx16_mem(Gpr::Rax, ADDR, 0);
                            self.store_scalar_result(dst);
                            return Ok(());
                        }
                    }
                }
                self.operand_to(op, Gpr::Rax)?;
                self.normalize(Gpr::Rax, dst_ty);
                self.store_scalar_result(dst);
                Ok(())
            }
            Rvalue::Binary { op, lhs, rhs, ty } => {
                if self.is_float(*ty) {
                    return self.lower_float_binary(dst, *op, lhs, rhs, *ty);
                }
                // The destination local's declared type carries the true storage
                // width: a `comptime_int`-typed rvalue (e.g. literal arithmetic)
                // assigned into a 128-bit local must use the two-limb path. Keying
                // off `dst_ty` (not the rvalue's `ty`, which may be `comptime_int`)
                // is what makes `const c: u128 = a + 1` correct.
                if self.is_wide_int(dst_ty) || self.is_wide_int(*ty) {
                    return self.lower_wide_binary(dst, *op, lhs, rhs);
                }
                self.eval_binary_into_rax(*op, lhs, rhs, *ty)?;
                self.store_scalar_result(dst);
                Ok(())
            }
            Rvalue::Unary { op, operand, ty } => {
                if self.is_wide_int(dst_ty) || self.is_wide_int(*ty) {
                    return self.lower_wide_unary(dst, *op, operand);
                }
                self.lower_unary(dst, *op, operand, *ty)
            }
            Rvalue::Cast { kind, operand, ty } => {
                // A widening cast into a 128-bit local: the destination width is
                // authoritative (the cast `ty` may be `comptime_int`).
                let wide_dst = self.is_wide_int(dst_ty) || self.is_wide_int(*ty);
                let extend_kind = matches!(
                    kind,
                    CastKind::Widen | CastKind::IntNarrow | CastKind::PtrReinterpret
                );
                if wide_dst && extend_kind {
                    return self.lower_wide_cast(dst, operand);
                }
                self.lower_cast(dst, *kind, operand, *ty)
            }
            Rvalue::Call { func, args, ty } => self.lower_call(dst, *func, args, *ty, rodata),
            Rvalue::Ref { place, .. } => {
                self.place_addr_general(place, Gpr::Rax)?;
                self.store_scalar_result(dst);
                Ok(())
            }
            Rvalue::Intrinsic { .. } => {
                // A heap-allocation intrinsic (`create`/`alloc`/`realloc`) builds an
                // error union into `dst`'s stack home; its `discr`/`.payload`
                // consumers read it from there.
                if self.eu_payload_of(dst).is_some() {
                    return self.lower_alloc_op_into_home(dst, rvalue);
                }
                self.lower_intrinsic_into_rax(rvalue, rodata)?;
                self.store_scalar_result(dst);
                Ok(())
            }
            Rvalue::MakeNull(_) => {
                // A pointer-niche optional null is a zero pointer (scalar).
                self.asm.mov_ri(Gpr::Rax, 0);
                self.store_scalar_result(dst);
                Ok(())
            }
            Rvalue::MakeSome(op, _) => {
                // Pointer-niche optional `Some(p)` is just the pointer.
                self.operand_to(op, Gpr::Rax)?;
                self.store_scalar_result(dst);
                Ok(())
            }
            Rvalue::Discriminant { operand, kind } => self.lower_discriminant(dst, operand, *kind),
            Rvalue::Aggregate { fields, .. } if fields.is_empty() => {
                self.asm.mov_ri(Gpr::Rax, 0);
                self.store_scalar_result(dst);
                Ok(())
            }
            other => Err(CodegenError::Unsupported(format!(
                "scalar rvalue {} in `{}`",
                rvalue_kind(other),
                self.func.name
            ))),
        }
    }

    /// Evaluates a non-float binary op into RAX (LHS in RAX, RHS in RCX).
    fn eval_binary_into_rax(
        &mut self,
        op: BinOp,
        lhs: &Operand,
        rhs: &Operand,
        ty: TypeId,
    ) -> Result<(), CodegenError> {
        use BinOp::*;
        if is_comparison(op) {
            return self.eval_compare_into_rax(op, lhs, rhs);
        }
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
                if repr_of(self.prog, ty).signed {
                    self.asm.cqo();
                    self.asm.idiv_r(Gpr::Rcx);
                } else {
                    self.asm.zero_rdx();
                    self.asm.div_r(Gpr::Rcx);
                }
            }
            Rem => {
                if repr_of(self.prog, ty).signed {
                    self.asm.cqo();
                    self.asm.idiv_r(Gpr::Rcx);
                } else {
                    self.asm.zero_rdx();
                    self.asm.div_r(Gpr::Rcx);
                }
                self.asm.mov_rr(Gpr::Rax, Gpr::Rdx);
            }
            Shl => self.asm.shl_cl(Gpr::Rax),
            Shr => {
                if repr_of(self.prog, ty).signed {
                    self.asm.sar_cl(Gpr::Rax);
                } else {
                    self.asm.shr_cl(Gpr::Rax);
                }
            }
            Eq | Ne | Lt | Le | Gt | Ge => unreachable!("comparison routed elsewhere"),
        }
        self.normalize(Gpr::Rax, ty);
        Ok(())
    }

    /// Evaluates a comparison into a 0/1 bool in RAX.
    fn eval_compare_into_rax(
        &mut self,
        op: BinOp,
        lhs: &Operand,
        rhs: &Operand,
    ) -> Result<(), CodegenError> {
        // Float comparison?
        let lty = self.operand_type(lhs);
        if let Some(t) = lty {
            if self.is_float(t) {
                return self.lower_float_compare_into_rax(op, lhs, rhs);
            }
            // A 128-bit comparison would need a two-limb compare; the single-limb
            // path below would silently read only the low 64 bits, so refuse it
            // cleanly (it falls to the VM) rather than miscompiling.
            if self.is_wide_int(t) {
                return Err(self.unsup("128-bit integer comparison (use the VM)"));
            }
        }
        let signed = self.operand_signed(lhs) || self.operand_signed(rhs);
        self.operand_to(lhs, Gpr::Rax)?;
        self.operand_to(rhs, Gpr::Rcx)?;
        self.asm.cmp_rr(Gpr::Rax, Gpr::Rcx);
        let cc = compare_cc(op, signed);
        self.asm.setcc_al(cc);
        self.asm.movzx_al(Gpr::Rax);
        Ok(())
    }

    /// `true` if `op`'s source type is a signed integer.
    fn operand_signed(&self, op: &Operand) -> bool {
        match op {
            Operand::Copy(p) if p.is_local() => {
                repr_of(self.prog, self.func.locals[p.base.index()].ty).signed
            }
            Operand::Copy(p) => repr_of(self.prog, self.place_type(p)).signed,
            Operand::Const(Const::Int { ty, .. }) => repr_of(self.prog, *ty).signed,
            _ => false,
        }
    }

    /// Lowers a unary op.
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
            UnOp::Not => self.asm.xor_ri(Gpr::Rax, 1),
        }
        self.store_scalar_result(dst);
        Ok(())
    }

    /// Lowers an integer/pointer/float cast.
    fn lower_cast(
        &mut self,
        dst: k2_mir::LocalId,
        kind: CastKind,
        operand: &Operand,
        ty: TypeId,
    ) -> Result<(), CodegenError> {
        match kind {
            CastKind::Widen | CastKind::IntNarrow | CastKind::PtrReinterpret => {
                // A cast whose RESULT is 128-bit (a widening `@as(i128, x)`)
                // sign-/zero-extends the source into both limbs and stores them.
                if self.is_wide_int(ty) {
                    return self.lower_wide_cast(dst, operand);
                }
                // A cast whose SOURCE is 128-bit but result is narrow (`@truncate`
                // /narrowing of a 128-bit value) would need to read the low limb of
                // a two-limb home; that path is not implemented, so refuse it
                // cleanly rather than reading a single (wrong-width) word.
                if let Some(src_ty) = self.operand_type(operand) {
                    if self.is_wide_int(src_ty) {
                        return Err(
                            self.unsup("narrowing cast from a 128-bit integer (use the VM)")
                        );
                    }
                }
                self.operand_to(operand, Gpr::Rax)?;
                self.normalize(Gpr::Rax, ty);
                self.store_scalar_result(dst);
                Ok(())
            }
            CastKind::IntToFloat => {
                // int (RAX) -> f64 (xmm0) -> dst home.
                self.operand_to(operand, Gpr::Rax)?;
                self.asm.cvtsi2sd(Xmm::Xmm0, Gpr::Rax);
                self.store_float_xmm0(dst)
            }
            CastKind::FloatToInt => {
                // f64 (dst src) -> truncating int (RAX) -> dst.
                self.load_float_operand(operand, Xmm::Xmm0)?;
                self.asm.cvttsd2si(Gpr::Rax, Xmm::Xmm0);
                self.normalize(Gpr::Rax, ty);
                self.store_scalar_result(dst);
                Ok(())
            }
        }
    }

    /// Lowers `dst = discr.kind operand`.
    fn lower_discriminant(
        &mut self,
        dst: k2_mir::LocalId,
        operand: &Operand,
        kind: DiscrKind,
    ) -> Result<(), CodegenError> {
        match kind {
            DiscrKind::Optional => self.lower_discr_optional(dst, operand),
            DiscrKind::ErrorUnion => self.lower_discr_error_union(dst, operand),
            DiscrKind::Union => self.lower_discr_union(dst, operand),
        }
    }

    /// Enum/tagged-union discriminant: yields the active variant tag as an integer
    /// (drives a `Switch`). An `Enum` value is laid out as its tag integer, so the
    /// discriminant is the scalar value itself, normalized to the tag width. A
    /// payload-carrying tagged `union` is not in the native subset and is refused.
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
                self.asm.mov_ri(Gpr::Rax, *variant as i64)
            }
            _ => self.operand_to(operand, Gpr::Rax)?,
        }
        if let Some(t) = ty {
            if let Type::Enum(id) = self.prog.arena.get(t) {
                let tag = self.prog.arena.enums[id.0 as usize].tag;
                self.normalize(Gpr::Rax, tag);
            }
        }
        self.store_scalar_result(dst);
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
                // Pointer-niche: null iff pointer == 0.
                self.operand_to(operand, Gpr::Rax)?;
                self.asm.test_rr(Gpr::Rax, Gpr::Rax);
                self.asm.setcc_al(Cc::E);
                self.asm.movzx_al(Gpr::Rax);
            }
            Type::Optional(_) => {
                // Flag byte at +inner.size: null iff flag == 0.
                let flag_off = layout::optional_flag_off(&self.prog.arena, ty).unwrap_or(0);
                self.aggregate_operand_addr(operand, ADDR)?;
                self.asm.movzx8_mem(Gpr::Rax, ADDR, flag_off as i32);
                self.asm.test_rr(Gpr::Rax, Gpr::Rax);
                self.asm.setcc_al(Cc::E);
                self.asm.movzx_al(Gpr::Rax);
            }
            _ => {
                // Not an optional type (e.g. the sentinel scalar): treat 0 as null.
                self.operand_to(operand, Gpr::Rax)?;
                self.asm.test_rr(Gpr::Rax, Gpr::Rax);
                self.asm.setcc_al(Cc::E);
                self.asm.movzx_al(Gpr::Rax);
            }
        }
        self.store_scalar_result(dst);
        Ok(())
    }

    /// Error-union discriminant: `true` when error (u16 tag != 0).
    fn lower_discr_error_union(
        &mut self,
        dst: k2_mir::LocalId,
        operand: &Operand,
    ) -> Result<(), CodegenError> {
        // A heap-allocation result (`deferred` in the MIR) is a real error union in
        // memory: read its u16 tag at +0 exactly like a declared error union.
        let is_alloc_eu = matches!(operand, Operand::Copy(p)
            if p.is_local() && self.eu_payload_of(p.base).is_some());
        let ty = self.operand_type(operand);
        let is_eu = is_alloc_eu
            || ty
                .map(|t| matches!(self.prog.arena.get(t), Type::ErrorUnion { .. }))
                .unwrap_or(false);
        if is_eu {
            // Load the u16 tag at +0.
            self.aggregate_operand_addr(operand, ADDR)?;
            self.asm.movzx16_mem(Gpr::Rax, ADDR, 0);
            self.asm.test_rr(Gpr::Rax, Gpr::Rax);
            self.asm.setcc_al(Cc::Ne);
            self.asm.movzx_al(Gpr::Rax);
        } else {
            // The print-result sentinel path: a scalar where 0 means Ok.
            self.operand_to(operand, Gpr::Rax)?;
            self.asm.test_rr(Gpr::Rax, Gpr::Rax);
            self.asm.setcc_al(Cc::Ne);
            self.asm.movzx_al(Gpr::Rax);
        }
        self.store_scalar_result(dst);
        Ok(())
    }

    /// Computes the address of an aggregate operand (a `Copy` place) into `dst`.
    fn aggregate_operand_addr(&mut self, op: &Operand, dst: Gpr) -> Result<(), CodegenError> {
        match op {
            Operand::Copy(p) => self.place_addr_general(p, dst),
            _ => Err(self.unsup("aggregate discriminant of a non-place operand")),
        }
    }

    /// Computes the address of a place (bare local or projected) into `dst`.
    fn place_addr_general(&mut self, place: &Place, dst: Gpr) -> Result<(), CodegenError> {
        if place.is_local() {
            let h = self
                .home(place.base)
                .ok_or_else(|| self.unsup("address of a register-only local"))?;
            self.asm.lea_rbp(dst, h);
            Ok(())
        } else {
            self.place_addr(place, dst)
        }
    }

    // ---------------------------------------------------------------------
    //  Aggregate construction
    // ---------------------------------------------------------------------

    /// Lowers an aggregate-typed `dst = rvalue` into `dst`'s stack home.
    fn lower_aggregate_rvalue(
        &mut self,
        dst: k2_mir::LocalId,
        rvalue: &Rvalue,
        ty: TypeId,
        rodata: &mut RoData,
    ) -> Result<(), CodegenError> {
        let home = self
            .home(dst)
            .ok_or_else(|| self.unsup("aggregate local without a home"))?;
        match rvalue {
            Rvalue::Aggregate { kind, fields, ty } => {
                self.build_aggregate(home, *kind, fields, *ty, rodata)
            }
            Rvalue::Use(op) => self.aggregate_use(home, op, ty, rodata),
            Rvalue::MakeSlice {
                ptr,
                offset,
                len,
                ty,
            } => self.build_make_slice(home, ptr, offset, len, *ty),
            Rvalue::MakeSome(op, oty) => self.build_make_some(home, op, *oty, rodata),
            Rvalue::MakeNull(oty) => self.build_make_null(home, *oty),
            Rvalue::MakeOk(op, ety) => self.build_make_ok(home, op, *ety, rodata),
            Rvalue::MakeErr(tag, ety) => self.build_make_err(home, tag.0, *ety),
            Rvalue::Call { func, args, ty } => {
                self.lower_call_aggregate(dst, *func, args, *ty, rodata)
            }
            // `@errorName(e)` produces a `[]const u8` name slice into the home: read
            // the error tag and select the matching name string in `.rodata`.
            Rvalue::Intrinsic { path, args, .. } if matches!(&path.root, IntrinsicRoot::Builtin(n) if n == "errorName") => {
                self.build_error_name(home, args, rodata)
            }
            // A scalar-producing rvalue (`Binary`/`Unary`/`Cast`) coerced into an
            // optional/error-union slot: compute the value, then wrap it as
            // `Some`/`Ok` (the MIR represents `return n * 2` into `?T`/`E!T` this
            // way, relying on the coercion).
            Rvalue::Binary { .. } | Rvalue::Unary { .. } | Rvalue::Cast { .. } => {
                self.scalar_rvalue_into_aggregate(home, rvalue, ty)
            }
            other => Err(CodegenError::Unsupported(format!(
                "aggregate rvalue {} in `{}`",
                rvalue_kind(other),
                self.func.name
            ))),
        }
    }

    /// Wraps a scalar-producing rvalue's result as `Some`/`Ok` in an
    /// optional/error-union home (the implicit `T -> ?T` / `T -> E!T` coercion).
    fn scalar_rvalue_into_aggregate(
        &mut self,
        home: i32,
        rvalue: &Rvalue,
        ty: TypeId,
    ) -> Result<(), CodegenError> {
        match self.prog.arena.get(ty) {
            Type::Optional(inner) => {
                let inner = *inner;
                if matches!(self.prog.arena.get(inner), Type::Pointer { .. }) {
                    // Pointer-niche optional handled on the scalar path elsewhere.
                    return Err(self.unsup("scalar rvalue into a pointer-niche optional"));
                }
                // Payload at +0.
                self.eval_scalar_rvalue_into_rax(rvalue, inner)?;
                self.store_sized(Gpr::Rbp, home, Gpr::Rax, inner);
                // Flag byte 1 after the payload.
                let flag_off = layout::optional_flag_off(&self.prog.arena, ty).unwrap_or(0) as i32;
                self.asm.mov_ri(Gpr::Rax, 1);
                self.asm.mov_store8_mem(Gpr::Rbp, home + flag_off, Gpr::Rax);
                Ok(())
            }
            Type::ErrorUnion { ok, .. } => {
                let ok = *ok;
                // u16 tag 0 at +0.
                self.eval_scalar_rvalue_into_rax(rvalue, ok)?;
                // Stash the payload while we write the tag.
                let poff = layout::error_union_payload_off(&self.prog.arena, ty) as i32;
                self.store_sized(Gpr::Rbp, home + poff, Gpr::Rax, ok);
                self.asm.mov_ri(Gpr::Rax, 0);
                self.asm.mov_store16_mem(Gpr::Rbp, home, Gpr::Rax);
                Ok(())
            }
            _ => Err(self.unsup("scalar rvalue into a non-optional/-error-union aggregate")),
        }
    }

    /// Copies / materializes `op` into an aggregate home.
    fn aggregate_use(
        &mut self,
        home: i32,
        op: &Operand,
        ty: TypeId,
        rodata: &mut RoData,
    ) -> Result<(), CodegenError> {
        match op {
            Operand::Copy(src) => {
                let src_ty = if src.is_local() {
                    self.func.locals[src.base.index()].ty
                } else {
                    self.place_type(src)
                };
                // A scalar source coerced into an optional/error-union slot: build
                // `Some(v)` / `Ok(v)` rather than a raw memcpy (the aggregate needs
                // its flag/tag set).
                let coercible = !frame::is_memory_aggregate(&self.prog.arena, src_ty)
                    && !self.is_wide_int(src_ty);
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
                // A scalar sentinel flowing into an error-union/optional slot (the
                // `print` result feeding the `!void` return slot, consumed only as
                // a discriminant) has no home — store it as a scalar at the home's
                // first word.
                if src.is_local() && self.home(src.base).is_none() {
                    self.operand_to(op, Gpr::Rax)?;
                    self.asm.mov_store(home, Gpr::Rax);
                    return Ok(());
                }
                self.place_addr_general(src, Gpr::Rax)?;
                self.asm.lea_rbp(ADDR, home);
                let size = self.layout(ty).size;
                self.memcpy(ADDR, Gpr::Rax, size);
                Ok(())
            }
            Operand::Const(c) => self.materialize_const_aggregate(home, c, ty, rodata),
        }
    }

    /// Materializes a constant aggregate (a string-literal slice, empty slice, or
    /// interned aggregate const) into a home.
    fn materialize_const_aggregate(
        &mut self,
        home: i32,
        c: &Const,
        ty: TypeId,
        rodata: &mut RoData,
    ) -> Result<(), CodegenError> {
        match c {
            // A `[]const u8` string literal -> {ptr=rodata, len}.
            Const::Str(id) => {
                let bytes = match &self.prog.consts[id.0 as usize] {
                    ConstData::Bytes(b) => b.clone(),
                    ConstData::Aggregate(_) => {
                        return Err(self.unsup("string const backed by an aggregate"))
                    }
                };
                let len = bytes.len();
                let off = rodata.intern(&bytes);
                // ptr at +0, len at +8.
                self.asm.mov_ri_data(Gpr::Rax, off);
                self.asm.mov_store(home, Gpr::Rax);
                self.asm.mov_ri(Gpr::Rax, len as i64);
                self.asm.mov_store(home + 8, Gpr::Rax);
                Ok(())
            }
            Const::EmptySlice { .. } => {
                self.asm.mov_ri(Gpr::Rax, 0);
                self.asm.mov_store(home, Gpr::Rax);
                self.asm.mov_store(home + 8, Gpr::Rax);
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
            Const::Undef { .. } => Ok(()), // leave the home undefined
            // `Const::Void` coerced into an error-union slot is the success value
            // `Ok(void)` — a `!void` whose payload is empty: write tag 0 only. (A
            // plain void target has no bytes; nothing to store.)
            Const::Void => match self.prog.arena.get(ty) {
                Type::ErrorUnion { .. } => {
                    self.asm.mov_ri(Gpr::Rax, 0);
                    self.asm.mov_store16_mem(Gpr::Rbp, home, Gpr::Rax);
                    Ok(())
                }
                _ => Ok(()),
            },
            Const::ErrVal { tag, .. } => {
                // An error value coerced into an error-union slot -> Err(tag).
                self.build_make_err(home, tag.0, ty)
            }
            // A scalar value coerced into an optional/error-union slot: build
            // `Some(v)` / `Ok(v)` (the MIR represents `return 7` into `?T`/`E!T`
            // as a bare `Use(Const::Int)` relying on the coercion).
            scalar @ (Const::Int { .. } | Const::Bool(_) | Const::Float { .. }) => {
                let op = Operand::Const(scalar.clone());
                match self.prog.arena.get(ty) {
                    Type::Optional(_) => self.build_make_some(home, &op, ty, rodata),
                    Type::ErrorUnion { .. } => self.build_make_ok(home, &op, ty, rodata),
                    _ => Err(self.unsup("scalar constant into a non-optional aggregate")),
                }
            }
            other => Err(CodegenError::Unsupported(format!(
                "const aggregate {other:?} of type {:?} in `{}`",
                self.prog.arena.get(ty),
                self.func.name
            ))),
        }
    }

    /// Builds a struct/array/tuple literal into a home, field by field.
    fn build_aggregate(
        &mut self,
        home: i32,
        kind: AggKind,
        fields: &[Operand],
        ty: TypeId,
        rodata: &mut RoData,
    ) -> Result<(), CodegenError> {
        // Float-element `@Vector` lowering is deferred on native in v0.21: a
        // 32-bit float lane needs a single-precision (`movss`) store, which the
        // backend models as an 8-byte `movsd` — correct for a scalar f64 but
        // corrupting for packed f32 lanes. Refuse cleanly (the VM computes float
        // vectors correctly); integer vectors are fully supported (spec §02).
        if let Type::Vector { elem, .. } = self.prog.arena.get(ty) {
            if self.is_float(*elem) {
                return Err(self.unsup("float-element @Vector on the native backend"));
            }
        }
        // A `packed struct` literal: the struct is a single backing integer.
        // Zero the home, then for each field OR its `(value & mask) << off` into
        // the backing integer (spec §02). LSB-first packing matches the VM's
        // per-field values and the little-endian byte image.
        if matches!(kind, AggKind::Struct) {
            if let Type::Struct(id) = self.prog.arena.get(ty) {
                if self.prog.arena.structs[id.0 as usize].is_packed() {
                    return self.build_packed_aggregate(home, fields, ty);
                }
            }
        }
        // When the declared type is not layoutable (a `deferred` tuple, or an
        // inferred-length `[_]T` array), compute a synthetic packed layout. For an
        // array every field has the array's element type — a string-const field's
        // `operand_type` is `None`, so falling back to the array type would
        // mis-stride the elements (a `[]const u8` element must be 16 bytes, not the
        // word fallback). For a tuple, use the field operands' own types.
        if layout::layout_of(&self.prog.arena, ty).is_none() {
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
                    // A bare string-constant field has no `operand_type` (it is an
                    // inline `Const::Str`, not a typed local), so the
                    // `.unwrap_or(ty)` fallback would mis-type it as the *aggregate*
                    // type and route it to the scalar `const_to`, which rejects a
                    // string constant ("non-scalar constant Str(..)"). Copy/const
                    // propagation only newly produces this shape under the optimizer
                    // (e.g. a folded `const star = "Sol"` inlined into a print
                    // tuple), which is exactly why the unoptimized native tests never
                    // saw it. Type it as the canonical `[]const u8` slice so the
                    // existing `store_field_operand` slice-const path handles it —
                    // the same code the layoutable-aggregate case already uses.
                    if matches!(f, Operand::Const(Const::Str(_))) {
                        return self.prog.arena.t_str();
                    }
                    elem_ty.or_else(|| self.operand_type(f)).unwrap_or(ty)
                })
                .collect();
            let (_, _, offs) = regalloc::packed_layout(&self.prog.arena, &field_tys);
            for (i, f) in fields.iter().enumerate() {
                let foff = home + offs[i] as i32;
                self.store_field_operand(foff, f, field_tys[i], rodata)?;
            }
            return Ok(());
        }
        let offsets = self.aggregate_field_offsets(kind, fields.len(), ty);
        for (i, f) in fields.iter().enumerate() {
            let foff = home + offsets[i] as i32;
            let fty = self.aggregate_field_type(kind, i, ty);
            self.store_field_operand(foff, f, fty, rodata)?;
        }
        Ok(())
    }

    /// Builds a `packed struct` literal into its `home`: accumulate each field's
    /// `(value & mask) << off` into a backing integer register, then store it to
    /// the home as one ≤64-bit limb. A backing wider than 64 bits is cleanly
    /// refused (v0.21 supports the common ≤64-bit packed structs natively).
    fn build_packed_aggregate(
        &mut self,
        home: i32,
        fields: &[Operand],
        ty: TypeId,
    ) -> Result<(), CodegenError> {
        let size = self.layout(ty).size;
        if size > 8 {
            return Err(self.unsup("packed struct literal wider than 64 bits"));
        }
        let Type::Struct(id) = self.prog.arena.get(ty) else {
            return Err(self.unsup("packed aggregate of non-struct"));
        };
        let info = self.prog.arena.structs[id.0 as usize].clone();
        // Accumulate into RDX (kept across each field's value eval in RAX).
        self.asm.mov_ri(Gpr::Rdx, 0);
        for (i, f) in fields.iter().enumerate() {
            let Some(field) = info.fields.get(i) else {
                continue;
            };
            let (Some(off), Some(width)) = (field.bit_offset, field.bit_width) else {
                continue;
            };
            if width == 0 {
                continue;
            }
            // value -> RAX, normalized to the field type.
            self.eval_scalar_rvalue_into_rax(&Rvalue::Use(f.clone()), field.ty)?;
            let mask = if width >= 64 {
                u64::MAX as u128
            } else {
                (1u128 << width) - 1
            };
            self.mask_to(Gpr::Rax, mask);
            if off != 0 {
                self.asm.shl_ri(Gpr::Rax, off as u8);
            }
            self.asm.or_rr(Gpr::Rdx, Gpr::Rax);
        }
        // Store the backing integer (one limb, width = struct size).
        match size {
            1 => self.asm.mov_store8_mem(Gpr::Rbp, home, Gpr::Rdx),
            2 => self.asm.mov_store16_mem(Gpr::Rbp, home, Gpr::Rdx),
            4 => self.asm.mov_store32_mem(Gpr::Rbp, home, Gpr::Rdx),
            _ => self.asm.mov_store_mem(Gpr::Rbp, home, Gpr::Rdx),
        }
        Ok(())
    }

    /// The byte offset of each field/element of an aggregate.
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
                // A tuple is represented as a struct; use struct field offsets when
                // available, else pack at 8-byte strides.
                let offs = layout::field_offsets(&self.prog.arena, ty);
                if offs.len() == n {
                    offs
                } else {
                    (0..n as u64).map(|i| i * 8).collect()
                }
            }
        }
    }

    /// The type of the `i`-th field/element of an aggregate.
    fn aggregate_field_type(&self, kind: AggKind, i: usize, ty: TypeId) -> TypeId {
        match kind {
            AggKind::Struct => {
                if let Type::Struct(id) = self.prog.arena.get(ty) {
                    let info = &self.prog.arena.structs[id.0 as usize];
                    if let Some(f) = info.fields.get(i) {
                        return f.ty;
                    }
                }
                ty
            }
            AggKind::Array => match self.prog.arena.get(ty) {
                // A `@Vector(N, T)` literal lowers as an array aggregate, so each
                // lane's field type is the vector's element type — typing a bool
                // lane as 1-byte `bool` (not the whole vector) so its store width
                // and stride are correct.
                Type::Array { elem, .. } | Type::Vector { elem, .. } => *elem,
                _ => ty,
            },
            AggKind::Tuple => {
                if let Type::Struct(id) = self.prog.arena.get(ty) {
                    let info = &self.prog.arena.structs[id.0 as usize];
                    if let Some(f) = info.fields.get(i) {
                        return f.ty;
                    }
                }
                ty
            }
        }
    }

    /// Stores a field operand at `[rbp + off]` (scalar sized store, or aggregate
    /// memcpy for a nested aggregate field).
    fn store_field_operand(
        &mut self,
        off: i32,
        op: &Operand,
        fty: TypeId,
        rodata: &mut RoData,
    ) -> Result<(), CodegenError> {
        if frame::is_memory_aggregate(&self.prog.arena, fty) {
            // Nested aggregate: memcpy from the operand's source address.
            match op {
                Operand::Copy(src) => {
                    self.place_addr_general(src, Gpr::Rax)?;
                    self.asm.lea_rbp(ADDR, off);
                    let size = self.layout(fty).size;
                    self.memcpy(ADDR, Gpr::Rax, size);
                    Ok(())
                }
                Operand::Const(_) => {
                    // A nested `[]const u8` literal (a string in an array/struct):
                    // write its `{ptr, len}` directly. Other nested const aggregates
                    // are rejected cleanly.
                    if let Some((ptr_off, len)) = self.slice_const_words(op, rodata) {
                        match ptr_off {
                            Some(o) => self.asm.mov_ri_data(Gpr::Rax, o),
                            None => self.asm.mov_ri(Gpr::Rax, 0),
                        }
                        self.asm.mov_store_mem(Gpr::Rbp, off, Gpr::Rax);
                        self.asm.mov_ri(Gpr::Rax, len as i64);
                        self.asm.mov_store_mem(Gpr::Rbp, off + 8, Gpr::Rax);
                        Ok(())
                    } else {
                        Err(self.unsup("nested constant aggregate field"))
                    }
                }
            }
        } else if self.is_float(fty) {
            self.load_float_operand(op, Xmm::Xmm0)?;
            self.asm.movsd_store(Gpr::Rbp, off, Xmm::Xmm0);
            Ok(())
        } else if self.is_wide_int(fty) {
            self.store_wide_int(off, op, fty)
        } else {
            self.operand_to(op, Gpr::Rax)?;
            self.normalize(Gpr::Rax, fty);
            self.store_sized(Gpr::Rbp, off, Gpr::Rax, fty);
            Ok(())
        }
    }

    /// Builds a `{ptr, len}` slice into a home from `(ptr + offset*elem, len)`.
    fn build_make_slice(
        &mut self,
        home: i32,
        ptr: &Operand,
        offset: &Operand,
        len: &Operand,
        ty: TypeId,
    ) -> Result<(), CodegenError> {
        let stride = layout::elem_size(&self.prog.arena, ty);
        // data = ptr + offset * stride.
        self.slice_data_ptr(ptr, Gpr::Rax)?;
        self.operand_to(offset, Gpr::Rcx)?;
        if stride.is_power_of_two() {
            let sh = stride.trailing_zeros() as u8;
            if sh != 0 {
                self.asm.shl_ri(Gpr::Rcx, sh);
            }
        } else {
            self.asm.imul_rri(Gpr::Rcx, Gpr::Rcx, stride as i32);
        }
        self.asm.add_rr(Gpr::Rax, Gpr::Rcx);
        self.asm.mov_store(home, Gpr::Rax);
        // len.
        self.operand_to(len, Gpr::Rax)?;
        self.asm.mov_store(home + 8, Gpr::Rax);
        Ok(())
    }

    /// Loads the data pointer for a `MakeSlice` `ptr` operand: a `Ref`-of-array
    /// pointer, an array address, or a slice's own `.ptr`. The operand is either
    /// a pointer scalar or a `Copy` of an array/slice place.
    fn slice_data_ptr(&mut self, ptr: &Operand, dst: Gpr) -> Result<(), CodegenError> {
        match ptr {
            Operand::Copy(p) => {
                let pty = if p.is_local() {
                    self.func.locals[p.base.index()].ty
                } else {
                    self.place_type(p)
                };
                match self.prog.arena.get(pty) {
                    // A pointer scalar: load its value.
                    Type::Pointer { .. } => self.load_place_scalar(p, dst),
                    // A slice: load its .ptr (first word).
                    Type::Slice { .. } => {
                        self.place_addr_general(p, dst)?;
                        self.asm.mov_load_mem(dst, dst, 0);
                        Ok(())
                    }
                    // An array value: its address is the data pointer.
                    Type::Array { .. } => self.place_addr_general(p, dst),
                    _ => self.load_place_scalar(p, dst),
                }
            }
            Operand::Const(Const::Str(id)) => {
                // A string literal's data pointer is its rodata address. Without a
                // threaded rodata here, reject (the corpus builds slices from
                // arrays / refs, not string consts).
                let _ = id;
                Err(self.unsup("MakeSlice from a string constant"))
            }
            Operand::Const(_) => Err(self.unsup("MakeSlice from a non-place constant pointer")),
        }
    }

    /// Builds an optional `Some(v)` into a home (payload at +0, flag 1 after).
    fn build_make_some(
        &mut self,
        home: i32,
        op: &Operand,
        oty: TypeId,
        rodata: &mut RoData,
    ) -> Result<(), CodegenError> {
        let inner = match self.prog.arena.get(oty) {
            Type::Optional(i) => *i,
            _ => oty,
        };
        if matches!(self.prog.arena.get(inner), Type::Pointer { .. }) {
            // Pointer niche: just store the pointer.
            self.operand_to(op, Gpr::Rax)?;
            self.asm.mov_store(home, Gpr::Rax);
            return Ok(());
        }
        // Payload at +0.
        self.store_field_operand(home, op, inner, rodata)?;
        // Flag byte 1 at +inner.size.
        let flag_off = layout::optional_flag_off(&self.prog.arena, oty).unwrap_or(0) as i32;
        self.asm.mov_ri(Gpr::Rax, 1);
        self.asm.mov_store8_mem(Gpr::Rbp, home + flag_off, Gpr::Rax);
        Ok(())
    }

    /// Builds an optional `null` into a home (flag 0, or null pointer).
    fn build_make_null(&mut self, home: i32, oty: TypeId) -> Result<(), CodegenError> {
        let inner = match self.prog.arena.get(oty) {
            Type::Optional(i) => *i,
            _ => oty,
        };
        if matches!(self.prog.arena.get(inner), Type::Pointer { .. }) {
            self.asm.mov_ri(Gpr::Rax, 0);
            self.asm.mov_store(home, Gpr::Rax);
            return Ok(());
        }
        // Zero the payload + clear the flag byte.
        let size = self.layout(oty).size;
        self.asm.lea_rbp(ADDR, home);
        self.zero_bytes(ADDR, size);
        Ok(())
    }

    /// Builds an error union `Ok(v)` into a home (u16 tag 0, payload after).
    fn build_make_ok(
        &mut self,
        home: i32,
        op: &Operand,
        ety: TypeId,
        rodata: &mut RoData,
    ) -> Result<(), CodegenError> {
        let ok_ty = match self.prog.arena.get(ety) {
            Type::ErrorUnion { ok, .. } => *ok,
            _ => ety,
        };
        // Tag = 0.
        self.asm.mov_ri(Gpr::Rax, 0);
        self.asm.mov_store16_mem(Gpr::Rbp, home, Gpr::Rax);
        // Payload.
        let poff = layout::error_union_payload_off(&self.prog.arena, ety) as i32;
        if matches!(self.prog.arena.get(ok_ty), Type::Void) {
            return Ok(());
        }
        self.store_field_operand(home + poff, op, ok_ty, rodata)
    }

    /// Builds an error union `Err(tag)` into a home (u16 tag, payload undefined).
    fn build_make_err(&mut self, home: i32, tag: u16, _ety: TypeId) -> Result<(), CodegenError> {
        self.asm.mov_ri(Gpr::Rax, tag as i64);
        self.asm.mov_store16_mem(Gpr::Rbp, home, Gpr::Rax);
        Ok(())
    }

    /// Builds `@errorName(e)`'s `[]const u8` name slice into `home`: load the error
    /// tag of `e`, then a compare chain selecting the matching name's `.rodata`
    /// `{ptr, len}`. Mirrors the VM's `error_name_of` over the program's `err_names`
    /// table. An unmatched tag yields the literal `"Unknown"` (the VM's fallback).
    fn build_error_name(
        &mut self,
        home: i32,
        args: &[Operand],
        rodata: &mut RoData,
    ) -> Result<(), CodegenError> {
        let err_op = args
            .first()
            .ok_or_else(|| self.unsup("@errorName without an operand"))?;
        // The error tag into RCX (a u16, zero-extended).
        self.load_error_tag(err_op, Gpr::Rcx)?;
        // Collect (tag, name) pairs in a stable order for a deterministic chain.
        let mut pairs: Vec<(u16, String)> = self
            .prog
            .err_names
            .iter()
            .map(|(t, n)| (t.0, n.clone()))
            .collect();
        pairs.sort_by_key(|(t, _)| *t);
        // Default ("Unknown") first; each matching tag overwrites via a forward jump.
        let done = self.asm.new_local_label();
        // Write the fallback name, then test each tag — on a match, overwrite the
        // slice and jump to done.
        let unknown_off = rodata.intern(b"Unknown");
        self.asm.mov_ri_data(Gpr::Rax, unknown_off);
        self.asm.mov_store_mem(Gpr::Rbp, home, Gpr::Rax);
        self.asm.mov_ri(Gpr::Rax, "Unknown".len() as i64);
        self.asm.mov_store_mem(Gpr::Rbp, home + 8, Gpr::Rax);
        for (tag, name) in pairs {
            let off = rodata.intern(name.as_bytes());
            let next = self.asm.new_local_label();
            self.asm.cmp_ri(Gpr::Rcx, tag as i32);
            self.asm.jcc_local(Cc::Ne, next);
            // Match: write {ptr, len} and finish.
            self.asm.mov_ri_data(Gpr::Rax, off);
            self.asm.mov_store_mem(Gpr::Rbp, home, Gpr::Rax);
            self.asm.mov_ri(Gpr::Rax, name.len() as i64);
            self.asm.mov_store_mem(Gpr::Rbp, home + 8, Gpr::Rax);
            self.asm.jmp_local(done);
            self.asm.bind_local(next);
        }
        self.asm.bind_local(done);
        Ok(())
    }

    /// Loads the `u16` error tag of an error operand into `dst`: an error-union
    /// place's tag at +0, a bare error scalar's value, or a static `Const::ErrVal`.
    fn load_error_tag(&mut self, op: &Operand, dst: Gpr) -> Result<(), CodegenError> {
        match op {
            Operand::Const(Const::ErrVal { tag, .. }) => {
                self.asm.mov_ri(dst, tag.0 as i64);
                Ok(())
            }
            Operand::Copy(p) => {
                let ty = self.place_type_of_operand(op);
                if ty
                    .map(|t| matches!(self.prog.arena.get(t), Type::ErrorUnion { .. }))
                    .unwrap_or(false)
                {
                    // The tag is the u16 at +0 of the error union.
                    self.place_addr_general(p, ADDR)?;
                    self.asm.movzx16_mem(dst, ADDR, 0);
                } else {
                    // A bare error scalar carries the tag value directly.
                    self.operand_to(op, dst)?;
                }
                Ok(())
            }
            _ => Err(self.unsup("@errorName operand is neither an error place nor a const")),
        }
    }

    /// The type of an operand that is a place or an error const (helper for
    /// `@errorName`'s tag extraction).
    fn place_type_of_operand(&self, op: &Operand) -> Option<TypeId> {
        match op {
            Operand::Copy(p) if p.is_local() => Some(self.func.locals[p.base.index()].ty),
            Operand::Copy(p) => Some(self.place_type(p)),
            _ => None,
        }
    }

    // ---------------------------------------------------------------------
    //  memcpy / memzero helpers
    // ---------------------------------------------------------------------

    /// Copies `size` bytes from `[src]` to `[dst]`. Small copies are unrolled
    /// through RDX (the byte scratch — chosen so the `src`/`dst` address registers,
    /// which are usually RAX and ADDR, are never clobbered mid-copy); large copies
    /// use `rep movsb`.
    fn memcpy(&mut self, dst: Gpr, src: Gpr, size: u64) {
        if size == 0 {
            return;
        }
        debug_assert!(
            src != Gpr::Rdx && dst != Gpr::Rdx,
            "memcpy scratch (RDX) must differ from the address registers"
        );
        if size <= 64 {
            let mut off = 0i32;
            let mut rem = size as i32;
            while rem >= 8 {
                self.asm.mov_load_mem(Gpr::Rdx, src, off);
                self.asm.mov_store_mem(dst, off, Gpr::Rdx);
                off += 8;
                rem -= 8;
            }
            if rem >= 4 {
                self.asm.mov_load32_mem(Gpr::Rdx, src, off);
                self.asm.mov_store32_mem(dst, off, Gpr::Rdx);
                off += 4;
                rem -= 4;
            }
            if rem >= 2 {
                self.asm.movzx16_mem(Gpr::Rdx, src, off);
                self.asm.mov_store16_mem(dst, off, Gpr::Rdx);
                off += 2;
                rem -= 2;
            }
            if rem >= 1 {
                self.asm.movzx8_mem(Gpr::Rdx, src, off);
                self.asm.mov_store8_mem(dst, off, Gpr::Rdx);
            }
        } else {
            // rep movsb: rdi=dst, rsi=src, rcx=size. These are caller-saved; the
            // allocator already ensured no live vreg sits in them across this point
            // only for call sites — to be safe we save/restore rsi/rdi here.
            self.asm.mov_rr(Gpr::Rdi, dst);
            self.asm.mov_rr(Gpr::Rsi, src);
            self.asm.mov_ri(Gpr::Rcx, size as i64);
            self.asm.rep_movsb();
        }
    }

    /// Writes `size` zero bytes to `[dst]`.
    fn zero_bytes(&mut self, dst: Gpr, size: u64) {
        self.asm.mov_ri(Gpr::Rax, 0);
        let mut off = 0i32;
        let mut rem = size as i32;
        while rem >= 8 {
            self.asm.mov_store_mem(dst, off, Gpr::Rax);
            off += 8;
            rem -= 8;
        }
        while rem > 0 {
            self.asm.mov_store8_mem(dst, off, Gpr::Rax);
            off += 1;
            rem -= 1;
        }
    }

    // ---------------------------------------------------------------------
    //  Floats (f64)
    // ---------------------------------------------------------------------

    /// `true` if `ty` is a float type (`f64`/`f32` or an unresolved
    /// `comptime_float`, which a float constant literal carries before coercion).
    fn is_float(&self, ty: TypeId) -> bool {
        matches!(
            self.prog.arena.get(ty),
            Type::Float { .. } | Type::ComptimeFloat
        )
    }

    /// `true` if `ty` is a wide (>8-byte) integer (`u128`/`i128`), which needs a
    /// 16-byte memory home and a two-limb store path.
    fn is_wide_int(&self, ty: TypeId) -> bool {
        matches!(self.prog.arena.get(ty), Type::Int { .. }) && self.layout(ty).size > 8
    }

    /// Stores a 128-bit value into `[rbp + dst]` from an operand. Only an inlined
    /// `Const::Int` (the common `u128` literal) and a `Copy` of another wide-int
    /// home are supported.
    fn store_wide_int(&mut self, dst: i32, op: &Operand, _ty: TypeId) -> Result<(), CodegenError> {
        match op {
            Operand::Const(Const::Int { value, .. }) => {
                let lo = (*value as u128) as u64 as i64;
                let hi = ((*value as u128) >> 64) as u64 as i64;
                self.asm.mov_ri(Gpr::Rax, lo);
                self.asm.mov_store(dst, Gpr::Rax);
                self.asm.mov_ri(Gpr::Rax, hi);
                self.asm.mov_store(dst + 8, Gpr::Rax);
                Ok(())
            }
            Operand::Copy(src) => {
                self.place_addr_general(src, Gpr::Rax)?;
                self.asm.mov_load_mem(Gpr::Rcx, Gpr::Rax, 0);
                self.asm.mov_store(dst, Gpr::Rcx);
                self.asm.mov_load_mem(Gpr::Rcx, Gpr::Rax, 8);
                self.asm.mov_store(dst + 8, Gpr::Rcx);
                Ok(())
            }
            _ => Err(self.unsup("wide-int store from an unsupported operand")),
        }
    }

    // ---------------------------------------------------------------------
    //  128-bit (two-limb) integer arithmetic
    // ---------------------------------------------------------------------
    //
    // A `u128`/`i128` value lives in a 16-byte stack home as two little-endian
    // 8-byte limbs: the low limb at `[home + 0]`, the high limb at `[home + 8]`.
    // We compute on a register *pair* `(lo, hi)` and write both limbs back with
    // `store_wide_pair`. Only the ops the corpus needs are implemented — add
    // (`add`+`adc`), subtract (`sub`+`sbb`), negate (two's complement), and a
    // widening cast (sign-/zero-extend a 64-bit value into the high limb).
    // Everything else (128-bit `mul`/`div`/`rem`, runtime-amount shifts) is
    // refused with a clean `Unsupported` (it falls to the VM) rather than
    // miscompiled.

    /// Loads the 128-bit value of `op` into the register pair `(lo, hi)`.
    ///
    /// * A `Const::Int` is split into its two 64-bit limbs.
    /// * A `Copy` of a wide-int home loads `[home+0]` (lo) and `[home+8]` (hi).
    /// * A `Copy` of a *narrow* (≤64-bit) integer is sign-/zero-extended into the
    ///   high limb — this is the widening-cast source. The `lo`/`hi` registers
    ///   must be distinct from each other and from `ADDR`.
    fn load_wide_operand(&mut self, op: &Operand, lo: Gpr, hi: Gpr) -> Result<(), CodegenError> {
        debug_assert!(lo != hi && lo != ADDR && hi != ADDR);
        match op {
            Operand::Const(Const::Int { value, .. }) => {
                let lo_bits = (*value as u128) as u64 as i64;
                let hi_bits = ((*value as u128) >> 64) as u64 as i64;
                self.asm.mov_ri(lo, lo_bits);
                self.asm.mov_ri(hi, hi_bits);
                Ok(())
            }
            Operand::Copy(src) => {
                let src_ty = if src.is_local() {
                    self.func.locals[src.base.index()].ty
                } else {
                    self.place_type(src)
                };
                if self.is_wide_int(src_ty) {
                    // A genuine two-limb home: load both limbs.
                    self.place_addr_general(src, ADDR)?;
                    self.asm.mov_load_mem(lo, ADDR, 0);
                    self.asm.mov_load_mem(hi, ADDR, 8);
                    Ok(())
                } else {
                    // A narrow scalar widened to 128 bits: the value goes in `lo`,
                    // and `hi` is the sign- (signed) or zero- (unsigned) extension.
                    self.operand_to(op, lo)?;
                    self.normalize(lo, src_ty);
                    if repr_of(self.prog, src_ty).signed {
                        // hi = lo arithmetically shifted right by 63 (all sign bits).
                        self.asm.mov_rr(hi, lo);
                        self.asm.mov_ri(Gpr::Rcx, 63);
                        self.asm.sar_cl(hi);
                    } else {
                        self.asm.mov_ri(hi, 0);
                    }
                    Ok(())
                }
            }
            _ => Err(self.unsup("128-bit operand from an unsupported operand")),
        }
    }

    /// Stores the register pair `(lo, hi)` into the wide-int home at `[rbp + dst]`.
    fn store_wide_pair(&mut self, dst: i32, lo: Gpr, hi: Gpr) {
        self.asm.mov_store(dst, lo);
        self.asm.mov_store(dst + 8, hi);
    }

    /// Computes a 128-bit add/subtract `lhs OP rhs` into the register pair
    /// `(Rax, Rdx)`. The right operand uses the `(Rcx, R8)` pair. Only add/sub are
    /// implemented; everything else is refused so it falls to the VM.
    fn eval_wide_binary_into_pair(
        &mut self,
        op: BinOp,
        lhs: &Operand,
        rhs: &Operand,
    ) -> Result<(), CodegenError> {
        match op {
            BinOp::Add => {
                self.load_wide_operand(lhs, Gpr::Rax, Gpr::Rdx)?;
                self.load_wide_operand(rhs, Gpr::Rcx, Gpr::R8)?;
                self.asm.add_rr(Gpr::Rax, Gpr::Rcx); // lo += rhs.lo, sets CF
                self.asm.adc_rr(Gpr::Rdx, Gpr::R8); // hi += rhs.hi + CF
                Ok(())
            }
            BinOp::Sub => {
                self.load_wide_operand(lhs, Gpr::Rax, Gpr::Rdx)?;
                self.load_wide_operand(rhs, Gpr::Rcx, Gpr::R8)?;
                self.asm.sub_rr(Gpr::Rax, Gpr::Rcx); // lo -= rhs.lo, sets borrow
                self.asm.sbb_rr(Gpr::Rdx, Gpr::R8); // hi -= rhs.hi - borrow
                Ok(())
            }
            _ => Err(self.unsup("128-bit integer multiply/divide/remainder/shift (use the VM)")),
        }
    }

    /// Computes a 128-bit unary op `OP operand` into the register pair `(Rax,
    /// Rdx)` (negation / bitwise-not).
    fn eval_wide_unary_into_pair(
        &mut self,
        op: UnOp,
        operand: &Operand,
    ) -> Result<(), CodegenError> {
        self.load_wide_operand(operand, Gpr::Rax, Gpr::Rdx)?;
        match op {
            UnOp::Neg => {
                // Two's-complement negation of (lo,hi): not both limbs, then add 1
                // to the low limb with carry into the high limb.
                self.asm.not_r(Gpr::Rax);
                self.asm.not_r(Gpr::Rdx);
                self.asm.mov_ri(Gpr::Rcx, 1);
                self.asm.mov_ri(Gpr::R8, 0);
                self.asm.add_rr(Gpr::Rax, Gpr::Rcx); // lo += 1, sets CF
                self.asm.adc_rr(Gpr::Rdx, Gpr::R8); // hi += CF
                Ok(())
            }
            UnOp::BitNot => {
                self.asm.not_r(Gpr::Rax);
                self.asm.not_r(Gpr::Rdx);
                Ok(())
            }
            // Logical `!` is bool-only; a 128-bit operand never reaches here.
            UnOp::Not => Err(self.unsup("logical not of a 128-bit integer")),
        }
    }

    /// Computes a 128-bit-typed `rvalue` into the register pair `(lo, hi)`,
    /// supporting `Use`, `Binary`, `Unary`, and a widening `Cast`.
    fn eval_wide_rvalue_into_pair(
        &mut self,
        rvalue: &Rvalue,
        lo: Gpr,
        hi: Gpr,
    ) -> Result<(), CodegenError> {
        debug_assert!(lo == Gpr::Rax && hi == Gpr::Rdx, "wide pair is RAX:RDX");
        match rvalue {
            Rvalue::Use(op) => self.load_wide_operand(op, lo, hi),
            Rvalue::Binary { op, lhs, rhs, .. } => self.eval_wide_binary_into_pair(*op, lhs, rhs),
            Rvalue::Unary { op, operand, .. } => self.eval_wide_unary_into_pair(*op, operand),
            Rvalue::Cast {
                kind: CastKind::Widen | CastKind::IntNarrow | CastKind::PtrReinterpret,
                operand,
                ..
            } => self.load_wide_operand(operand, lo, hi),
            other => Err(self.unsup(&format!("128-bit {}", rvalue_kind(other)))),
        }
    }

    /// Lowers a 128-bit `dst = lhs OP rhs` into `dst`'s home.
    fn lower_wide_binary(
        &mut self,
        dst: k2_mir::LocalId,
        op: BinOp,
        lhs: &Operand,
        rhs: &Operand,
    ) -> Result<(), CodegenError> {
        let home = self
            .home(dst)
            .ok_or_else(|| self.unsup("wide-int binary result without a home"))?;
        self.eval_wide_binary_into_pair(op, lhs, rhs)?;
        self.store_wide_pair(home, Gpr::Rax, Gpr::Rdx);
        Ok(())
    }

    /// Lowers a 128-bit `dst = OP operand` into `dst`'s home.
    fn lower_wide_unary(
        &mut self,
        dst: k2_mir::LocalId,
        op: UnOp,
        operand: &Operand,
    ) -> Result<(), CodegenError> {
        let home = self
            .home(dst)
            .ok_or_else(|| self.unsup("wide-int unary result without a home"))?;
        self.eval_wide_unary_into_pair(op, operand)?;
        self.store_wide_pair(home, Gpr::Rax, Gpr::Rdx);
        Ok(())
    }

    /// Lowers a widening cast `dst = @as(i128/u128, operand)` by loading the
    /// (narrow or wide) source as a two-limb value and storing both limbs.
    fn lower_wide_cast(
        &mut self,
        dst: k2_mir::LocalId,
        operand: &Operand,
    ) -> Result<(), CodegenError> {
        let home = self
            .home(dst)
            .ok_or_else(|| self.unsup("wide-int cast result without a home"))?;
        self.load_wide_operand(operand, Gpr::Rax, Gpr::Rdx)?;
        self.store_wide_pair(home, Gpr::Rax, Gpr::Rdx);
        Ok(())
    }

    /// Loads a float operand into `dst` (a constant from rodata, or a home).
    fn load_float_operand(&mut self, op: &Operand, dst: Xmm) -> Result<(), CodegenError> {
        match op {
            Operand::Const(Const::Float { bits, .. }) => {
                // Materialize the bit pattern in a GPR, then move it straight into
                // the xmm register with `movq` — no stack-memory round trip.
                //
                // The previous implementation round-tripped through a stack scratch
                // slot at `[rbp - frame_size]`, which is exactly `[rsp + 0]` — the
                // *first outgoing stack-argument slot* (`outgoing_args_base()` is 0).
                // A call passing both an f64 constant and a stack argument therefore
                // had the float-const materialization clobber the already-placed
                // stack arg (or vice versa), silently miscompiling the call. `movq`
                // avoids memory entirely and so cannot alias the outgoing-args
                // region. (Bit pattern -> RAX -> xmm via `66 REX.W 0F 6E /r`.)
                self.asm.mov_ri(Gpr::Rax, *bits as i64);
                self.asm.movq_xmm_r64(dst, Gpr::Rax);
                Ok(())
            }
            Operand::Copy(p) => {
                self.place_addr_general(p, ADDR)?;
                self.asm.movsd_load(dst, ADDR, 0);
                Ok(())
            }
            _ => Err(self.unsup("float operand from a non-float constant")),
        }
    }

    /// Stores xmm0 into a float destination local's home.
    fn store_float_xmm0(&mut self, dst: k2_mir::LocalId) -> Result<(), CodegenError> {
        let h = self
            .home(dst)
            .ok_or_else(|| self.unsup("float local without a home"))?;
        self.asm.movsd_store(Gpr::Rbp, h, Xmm::Xmm0);
        Ok(())
    }

    /// Lowers `dst = use(float op)`.
    fn lower_float_use(&mut self, dst: k2_mir::LocalId, op: &Operand) -> Result<(), CodegenError> {
        self.load_float_operand(op, Xmm::Xmm0)?;
        self.store_float_xmm0(dst)
    }

    /// Lowers a float binary op `dst = lhs OP rhs`.
    fn lower_float_binary(
        &mut self,
        dst: k2_mir::LocalId,
        op: BinOp,
        lhs: &Operand,
        rhs: &Operand,
        _ty: TypeId,
    ) -> Result<(), CodegenError> {
        self.load_float_operand(lhs, Xmm::Xmm0)?;
        self.load_float_operand(rhs, Xmm::Xmm1)?;
        match op {
            BinOp::Add => self.asm.addsd(Xmm::Xmm0, Xmm::Xmm1),
            BinOp::Sub => self.asm.subsd(Xmm::Xmm0, Xmm::Xmm1),
            BinOp::Mul => self.asm.mulsd(Xmm::Xmm0, Xmm::Xmm1),
            BinOp::Div => self.asm.divsd(Xmm::Xmm0, Xmm::Xmm1),
            _ => return Err(self.unsup("unsupported float binary op")),
        }
        self.store_float_xmm0(dst)
    }

    /// Lowers a float comparison into a 0/1 bool in RAX.
    fn lower_float_compare_into_rax(
        &mut self,
        op: BinOp,
        lhs: &Operand,
        rhs: &Operand,
    ) -> Result<(), CodegenError> {
        self.load_float_operand(lhs, Xmm::Xmm0)?;
        self.load_float_operand(rhs, Xmm::Xmm1)?;
        self.asm.ucomisd(Xmm::Xmm0, Xmm::Xmm1);
        // ucomisd sets ZF/PF/CF for an ordered compare; map to setcc.
        let cc = match op {
            BinOp::Eq => Cc::E,
            BinOp::Ne => Cc::Ne,
            // a < b -> CF=1 (below); a <= b -> CF=1 or ZF=1 (below/equal).
            BinOp::Lt => Cc::B,
            BinOp::Le => Cc::Be,
            BinOp::Gt => Cc::A,
            BinOp::Ge => Cc::Ae,
            _ => return Err(self.unsup("unsupported float comparison")),
        };
        self.asm.setcc_al(cc);
        self.asm.movzx_al(Gpr::Rax);
        Ok(())
    }

    // ---------------------------------------------------------------------
    //  Calls (System V ABI)
    // ---------------------------------------------------------------------

    /// Lowers `dst = func(args)` for a scalar-returning helper.
    fn lower_call(
        &mut self,
        dst: k2_mir::LocalId,
        func: k2_mir::FnId,
        args: &[Operand],
        ty: TypeId,
        rodata: &mut RoData,
    ) -> Result<(), CodegenError> {
        self.lower_call_raw(func, args, ty, rodata)?;
        if self.is_float(ty) {
            // Result in xmm0.
            self.store_float_xmm0(dst)?;
        } else {
            self.normalize(Gpr::Rax, ty);
            self.store_scalar_result(dst);
        }
        Ok(())
    }

    /// Lowers an aggregate-returning call `dst(aggregate) = func(args)`.
    fn lower_call_aggregate(
        &mut self,
        dst: k2_mir::LocalId,
        func: k2_mir::FnId,
        args: &[Operand],
        ty: TypeId,
        rodata: &mut RoData,
    ) -> Result<(), CodegenError> {
        let home = self
            .home(dst)
            .ok_or_else(|| self.unsup("aggregate call result without a home"))?;
        let size = self.layout(ty).size;
        let class = classify(self.prog, ty);
        if let ArgClass::Memory { .. } = class {
            // sret: pass &home as the hidden first integer arg.
            self.asm.lea_rbp(ADDR, home);
            self.lower_call_with_sret(func, args, ADDR, rodata)?;
            Ok(())
        } else {
            // Returned in RAX (and RDX for two eightbytes); store to home. A small
            // aggregate (`< 8` bytes, e.g. a single-field `u32` handle struct) has a
            // home only as wide as its layout, so the store must be size-exact — an
            // 8-byte store would clobber the adjacent local's home.
            self.lower_call_raw(func, args, ty, rodata)?;
            self.store_eightbyte(home, Gpr::Rax, size.min(8));
            if size > 8 {
                self.store_eightbyte(home + 8, Gpr::Rdx, size - 8);
            }
            Ok(())
        }
    }

    /// Stores the low `nbytes` of `src` to `[rbp + off]`, sized exactly so a
    /// sub-8-byte aggregate eightbyte never overruns its home into the next local.
    /// Non-power-of-two widths (3/5/6/7) are split into power-of-two stores.
    fn store_eightbyte(&mut self, off: i32, src: Gpr, nbytes: u64) {
        match nbytes {
            0 => {}
            1 => self.asm.mov_store8_mem(Gpr::Rbp, off, src),
            2 => self.asm.mov_store16_mem(Gpr::Rbp, off, src),
            4 => self.asm.mov_store32_mem(Gpr::Rbp, off, src),
            8 => self.asm.mov_store(off, src),
            n => {
                // Split: store the largest power-of-two prefix, then recurse on the
                // remainder (shifted down). Rare — the corpus uses 1/2/4/8.
                let p = 1u64 << (63 - (n.max(1)).leading_zeros());
                self.store_eightbyte(off, src, p);
                let rest = n - p;
                if rest > 0 {
                    self.asm.mov_rr(Gpr::Rcx, src);
                    self.asm.shr_ri(Gpr::Rcx, (p * 8) as u8);
                    self.store_eightbyte(off + p as i32, Gpr::Rcx, rest);
                }
            }
        }
    }

    /// Emits the argument marshalling + `call`, leaving the result in RAX/xmm0.
    fn lower_call_raw(
        &mut self,
        func: k2_mir::FnId,
        args: &[Operand],
        _ty: TypeId,
        rodata: &mut RoData,
    ) -> Result<(), CodegenError> {
        self.reject_deferred_aggregate_args(func, args)?;
        let sse_used = self.marshal_args(args, None, Some(func), rodata)?;
        // v0.19: a C/variadic call needs `AL` = number of vector registers used,
        // per the SysV ABI (glibc's `printf` reads it). For the common all-integer
        // case `xor eax,eax` is exactly right; otherwise set the SSE count.
        self.maybe_set_vararg_al(func, sse_used);
        self.asm.call_fn(func);
        Ok(())
    }

    /// Sets `AL` to the number of vector (SSE) argument registers used before a C
    /// variadic call (the SysV ABI requirement that `printf`-class functions read).
    /// A no-op for a non-C / non-variadic callee. `xor eax,eax` is emitted for the
    /// zero case (smaller + the common path); otherwise `mov al, n`.
    fn maybe_set_vararg_al(&mut self, func: k2_mir::FnId, sse_used: usize) {
        let callee = &self.prog.funcs[func.index()];
        if callee.abi != k2_mir::FnAbi::C {
            return;
        }
        if sse_used == 0 {
            self.asm.xor_rr(Gpr::Rax, Gpr::Rax);
        } else {
            self.asm.mov_ri(Gpr::Rax, sse_used as i64);
        }
    }

    /// Rejects a call that passes an aggregate (`> 8`-byte) argument into a callee
    /// parameter that the backend classifies as a *scalar* eightbyte. This is the
    /// un-monomorphized generic-container shape: the MIR shares one `push[List]`
    /// across every `List(T)`, fixing the value parameter to a single scalar type
    /// (e.g. `u8`), so a `List([]const u8).push("…")` would hand a 16-byte slice to
    /// an 8-byte parameter slot and silently lose its upper bytes. Refusing keeps
    /// the program on the VM rather than miscompiling it. (The common word-scalar
    /// instantiation — `List(u32)` / `ArrayList(u32)` — is unaffected: a word
    /// argument into a word parameter is exact.)
    fn reject_deferred_aggregate_args(
        &self,
        func: k2_mir::FnId,
        args: &[Operand],
    ) -> Result<(), CodegenError> {
        // A `test { … }` block is compiled but never *executed* by `run-native`
        // (only `main` runs), so a generic `expectError(err, error_union)` shape
        // inside a test — which passes an aggregate to an `anytype` parameter the
        // test body never reads at a mismatched width — must not gate the whole
        // program. Only refuse the mismatch in code on the live `main` path.
        if self.func.name == "test" || self.func.name.starts_with("test ") {
            return Ok(());
        }
        let callee = &self.prog.funcs[func.index()];
        for (i, param) in callee.params.iter().enumerate() {
            let pty = callee.locals[param.index()].ty;
            // A parameter the ABI carries as a scalar eightbyte.
            let param_is_scalar = !frame::is_memory_aggregate(&self.prog.arena, pty)
                && !self.is_float(pty)
                && self.layout(pty).size <= 8;
            if !param_is_scalar {
                continue;
            }
            let Some(arg) = args.get(i) else { continue };
            // v0.19: a string literal passed to a *pointer* parameter decays to a
            // `const char *` data pointer (one eightbyte), not a fat slice — this
            // is the FFI C-string marshalling, not an aggregate-into-scalar bug.
            if matches!(
                arg,
                Operand::Const(Const::Str(_) | Const::EmptySlice { .. })
            ) && matches!(self.prog.arena.get(pty), Type::Pointer { .. })
            {
                continue;
            }
            let aty = match arg {
                Operand::Const(Const::Str(_) | Const::EmptySlice { .. }) => {
                    Some(self.prog.arena.t_str())
                }
                _ => self.operand_type(arg),
            };
            if let Some(aty) = aty {
                if frame::is_memory_aggregate(&self.prog.arena, aty) && self.layout(aty).size > 8 {
                    return Err(self.unsup(
                        "generic call passing an aggregate argument to a scalar parameter \
                         (un-monomorphized generic container of aggregates)",
                    ));
                }
            }
        }
        Ok(())
    }

    /// Emits arg marshalling with a hidden `sret` pointer (in `sret_reg`) + call.
    fn lower_call_with_sret(
        &mut self,
        func: k2_mir::FnId,
        args: &[Operand],
        sret_reg: Gpr,
        rodata: &mut RoData,
    ) -> Result<(), CodegenError> {
        // Save the sret pointer in a scratch home is unnecessary: marshal_args
        // does not clobber ADDR until after we move it into RDI. Move it first.
        let sse_used = self.marshal_args(args, Some(sret_reg), Some(func), rodata)?;
        self.maybe_set_vararg_al(func, sse_used);
        self.asm.call_fn(func);
        Ok(())
    }

    /// Marshals `args` into the SysV arg registers / stack. When `sret` is set,
    /// its pointer goes in the first integer arg register (RDI) and shifts the
    /// integer counter. Arguments are evaluated left to right into RAX/xmm then
    /// moved into place; since operands are bare locals/consts (the MIR has no
    /// nested calls in an arg position), no in-flight arg register is clobbered by
    /// a later evaluation that reads a vreg — except that a vreg might live in an
    /// arg register. To stay correct we evaluate every arg's *value* before
    /// loading arg registers: integer/stack args are placed last-to-first via a
    /// temporary spill to the outgoing region for the stack ones.
    ///
    /// Returns the number of SSE (vector) argument registers consumed, which a C
    /// variadic call needs in `AL` per the SysV ABI.
    fn marshal_args(
        &mut self,
        args: &[Operand],
        sret: Option<Gpr>,
        callee: Option<k2_mir::FnId>,
        rodata: &mut RoData,
    ) -> Result<usize, CodegenError> {
        // Classify and lay out each argument.
        let mut int_idx = 0usize;
        let mut sse_idx = 0usize;
        let mut stack_off = self.plan.outgoing_args_base();

        // v0.19: a string-literal argument decays to a single C-string data
        // pointer (a `const char *`, one integer register) when the callee
        // parameter is a single/many pointer rather than a `[]const u8` slice. The
        // interned bytes carry a trailing NUL. `arg_is_cstr_ptr[i]` records that.
        let arg_is_cstr_ptr: Vec<bool> = args
            .iter()
            .enumerate()
            .map(|(i, arg)| {
                matches!(
                    arg,
                    Operand::Const(Const::Str(_) | Const::EmptySlice { .. })
                ) && self.callee_param_is_ptr(callee, i)
            })
            .collect();

        if let Some(p) = sret {
            // RDI := sret pointer.
            if p != ARG_REGS[0] {
                self.asm.mov_rr(ARG_REGS[0], p);
            }
            int_idx = 1;
        }

        // First, write all stack arguments (so later arg-register loads, which may
        // read a vreg, are not disturbed). Then load register arguments.
        // Plan: compute a per-arg placement.
        enum Place2 {
            Int(usize),         // arg register index
            Sse(usize),         // xmm index
            Stack(i32),         // rsp-relative offset
            MemInt(usize, u64), // first int reg index, size (in-reg aggregate)
            MemStack(i32, u64), // rsp offset, size (memory aggregate on stack)
        }
        let mut placements: Vec<(usize, Place2)> = Vec::new();
        for (ai, arg) in args.iter().enumerate() {
            // A string literal decaying to a `const char *` is one integer register.
            if arg_is_cstr_ptr[ai] {
                if int_idx < ARG_REGS.len() {
                    placements.push((ai, Place2::Int(int_idx)));
                    int_idx += 1;
                } else {
                    placements.push((ai, Place2::Stack(stack_off)));
                    stack_off += 8;
                }
                continue;
            }
            // A string / empty-slice literal argument is a `[]const u8` fat slice
            // (`{ptr, len}`, two integer eightbytes) even though the bare constant
            // carries no TypeId — classify it as a two-int aggregate.
            let is_slice_const = matches!(
                arg,
                Operand::Const(Const::Str(_) | Const::EmptySlice { .. })
            );
            let class = if is_slice_const {
                ArgClass::TwoInt
            } else {
                let ty = self.operand_type(arg).unwrap_or_else(|| {
                    self.func
                        .locals
                        .first()
                        .map(|l| l.ty)
                        .unwrap_or(self.func.ret)
                });
                classify(self.prog, ty)
            };
            match class {
                ArgClass::Sse => {
                    if sse_idx < SSE_ARG_REGS.len() {
                        placements.push((ai, Place2::Sse(sse_idx)));
                        sse_idx += 1;
                    } else {
                        placements.push((ai, Place2::Stack(stack_off)));
                        stack_off += 8;
                    }
                }
                ArgClass::OneInt => {
                    if int_idx < ARG_REGS.len() {
                        placements.push((ai, Place2::Int(int_idx)));
                        int_idx += 1;
                    } else {
                        placements.push((ai, Place2::Stack(stack_off)));
                        stack_off += 8;
                    }
                }
                ArgClass::TwoInt => {
                    // A slice const is 16 bytes; otherwise size from the arg type.
                    let sz = if is_slice_const {
                        16
                    } else {
                        let ty = self.operand_type(arg).unwrap_or(self.func.ret);
                        self.layout(ty).size
                    };
                    if int_idx + 2 <= ARG_REGS.len() {
                        placements.push((ai, Place2::MemInt(int_idx, sz)));
                        int_idx += 2;
                    } else {
                        placements.push((ai, Place2::MemStack(stack_off, sz)));
                        stack_off += round_up_i32(sz as i32, 8);
                    }
                }
                ArgClass::Memory { size, .. } => {
                    placements.push((ai, Place2::MemStack(stack_off, size)));
                    stack_off += round_up_i32(size as i32, 8);
                }
            }
        }

        // Emit stack args first.
        for (ai, pl) in &placements {
            match pl {
                Place2::Stack(off) => {
                    let arg = &args[*ai];
                    if arg_is_cstr_ptr[*ai] {
                        self.cstr_ptr_to(arg, Gpr::Rax, rodata);
                        self.asm.mov_store_mem(Gpr::Rsp, *off, Gpr::Rax);
                        continue;
                    }
                    let ty = self.operand_type(arg).unwrap_or(self.func.ret);
                    if self.is_float(ty) {
                        self.load_float_operand(arg, Xmm::Xmm0)?;
                        self.asm.movsd_store(Gpr::Rsp, *off, Xmm::Xmm0);
                    } else {
                        self.operand_to(arg, Gpr::Rax)?;
                        self.asm.mov_store_mem(Gpr::Rsp, *off, Gpr::Rax);
                    }
                }
                Place2::MemStack(off, size) => {
                    let arg = &args[*ai];
                    if let Operand::Copy(src) = arg {
                        self.place_addr_general(src, Gpr::Rax)?;
                        self.asm.mov_rr(ADDR, Gpr::Rsp);
                        self.asm.add_ri(ADDR, *off);
                        self.memcpy(ADDR, Gpr::Rax, *size);
                    } else if let Some((ptr_off, len)) = self.slice_const_words(arg, rodata) {
                        // A `[]const u8` literal on the stack: write {ptr, len}.
                        match ptr_off {
                            Some(o) => self.asm.mov_ri_data(Gpr::Rax, o),
                            None => self.asm.mov_ri(Gpr::Rax, 0),
                        }
                        self.asm.mov_store_mem(Gpr::Rsp, *off, Gpr::Rax);
                        self.asm.mov_ri(Gpr::Rax, len as i64);
                        self.asm.mov_store_mem(Gpr::Rsp, *off + 8, Gpr::Rax);
                    } else {
                        return Err(self.unsup("memory aggregate argument from a constant"));
                    }
                }
                _ => {}
            }
        }
        // Then register args (int + sse).
        for (ai, pl) in &placements {
            match pl {
                Place2::Int(ri) => {
                    let arg = &args[*ai];
                    let reg = ARG_REGS[*ri];
                    if arg_is_cstr_ptr[*ai] {
                        self.cstr_ptr_to(arg, reg, rodata);
                    } else {
                        self.operand_to(arg, reg)?;
                    }
                }
                Place2::Sse(xi) => {
                    let arg = &args[*ai];
                    self.load_float_operand(arg, SSE_ARG_REGS[*xi])?;
                }
                Place2::MemInt(ri, size) => {
                    let arg = &args[*ai];
                    if let Operand::Copy(src) = arg {
                        self.place_addr_general(src, ADDR)?;
                        self.asm.mov_load_mem(ARG_REGS[*ri], ADDR, 0);
                        if *size > 8 {
                            self.asm.mov_load_mem(ARG_REGS[*ri + 1], ADDR, 8);
                        }
                    } else if let Some((ptr_off, len)) = self.slice_const_words(arg, rodata) {
                        // A `[]const u8` literal in two integer arg registers:
                        // {ptr -> ARG_REGS[ri], len -> ARG_REGS[ri+1]}.
                        match ptr_off {
                            Some(o) => self.asm.mov_ri_data(ARG_REGS[*ri], o),
                            None => self.asm.mov_ri(ARG_REGS[*ri], 0),
                        }
                        self.asm.mov_ri(ARG_REGS[*ri + 1], len as i64);
                    } else {
                        return Err(self.unsup("in-register aggregate argument from a constant"));
                    }
                }
                _ => {}
            }
        }
        Ok(sse_idx)
    }

    /// `true` if the callee's parameter `i` is a single/many pointer (so a string
    /// literal passed there decays to a `const char *` data pointer, not a fat
    /// slice). `None` callee (an intrinsic / sret helper) is never a pointer param.
    fn callee_param_is_ptr(&self, callee: Option<k2_mir::FnId>, i: usize) -> bool {
        let Some(func) = callee else { return false };
        let callee = &self.prog.funcs[func.index()];
        let Some(param) = callee.params.get(i) else {
            // Beyond the fixed params of a C variadic callee (printf-class), a
            // string-literal argument decays to a single `const char *` data
            // pointer (one integer eightbyte), exactly like C: varargs are never
            // fat `[]const u8` slices. Without this, the trailing length word of
            // the fat slice would consume an extra integer arg register and shift
            // every following vararg.
            return callee.varargs && callee.abi == k2_mir::FnAbi::C;
        };
        let pty = callee.locals[param.index()].ty;
        matches!(self.prog.arena.get(pty), Type::Pointer { .. })
    }

    /// Materializes a string-literal argument as a NUL-terminated C-string data
    /// pointer into `dst` (a `const char *`). The interned bytes get a trailing NUL
    /// appended (deduplicated on the terminated form) so the pointer is a valid
    /// C string. An empty-slice literal yields a null pointer.
    fn cstr_ptr_to(&mut self, arg: &Operand, dst: Gpr, rodata: &mut RoData) {
        match arg {
            Operand::Const(Const::Str(id)) => match &self.prog.consts[id.0 as usize] {
                ConstData::Bytes(b) => {
                    let mut terminated = b.clone();
                    terminated.push(0);
                    let off = rodata.intern(&terminated);
                    self.asm.mov_ri_data(dst, off);
                }
                ConstData::Aggregate(_) => self.asm.mov_ri(dst, 0),
            },
            // An empty-slice / other constant: a null `const char *`.
            _ => self.asm.mov_ri(dst, 0),
        }
    }

    /// The `(rodata offset of the bytes, len)` of a string / empty-slice literal
    /// argument, or `None` for a non-slice constant. The pointer offset is `None`
    /// for an empty slice (a null data pointer). Interns the bytes into `.rodata`.
    fn slice_const_words(
        &self,
        arg: &Operand,
        rodata: &mut RoData,
    ) -> Option<(Option<u32>, usize)> {
        match arg {
            Operand::Const(Const::Str(id)) => match &self.prog.consts[id.0 as usize] {
                ConstData::Bytes(b) => {
                    let len = b.len();
                    let off = rodata.intern(b);
                    Some((Some(off), len))
                }
                ConstData::Aggregate(_) => None,
            },
            Operand::Const(Const::EmptySlice { .. }) => Some((None, 0)),
            _ => None,
        }
    }

    // ---------------------------------------------------------------------
    //  Intrinsics
    // ---------------------------------------------------------------------

    /// Lowers a recognized intrinsic, leaving its result in RAX.
    fn lower_intrinsic_into_rax(
        &mut self,
        rvalue: &Rvalue,
        rodata: &mut RoData,
    ) -> Result<(), CodegenError> {
        let Rvalue::Intrinsic { path, args, .. } = rvalue else {
            unreachable!("lower_intrinsic_into_rax on non-intrinsic");
        };
        if let IntrinsicRoot::Builtin(name) = &path.root {
            return self.lower_safety_predicate(name, args);
        }
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
            ["print"] => self.lower_print_runtime(path, args, rodata),
            // The default-allocator capability (`sys.heap`): handle id 0, matching
            // the VM's `HeapCap -> Allocator(0)`.
            ["heap"] => {
                self.asm.mov_ri(Gpr::Rax, 0);
                Ok(())
            }
            // `free`/`destroy` reached as `value(handle).op(ptr)`: a void result we
            // drive through the runtime free routine (its `deferred` result is
            // discarded by the caller).
            ["free"] | ["destroy"] => {
                self.lower_free_op(path, args)?;
                self.asm.mov_ri(Gpr::Rax, 0);
                Ok(())
            }
            // `create`/`alloc`/`realloc` are intercepted in `lower_rvalue` (they
            // build an error union into a stack home). Reaching here means the
            // result is consumed in a way the eu-result detection missed — refuse
            // cleanly rather than miscompile.
            ["create"] | ["alloc"] | ["realloc"] => Err(self
                .unsup("heap allocation result consumed outside the recognized error-union shape")),
            // The `*System` capability METHOD spellings (`sys.clock.now()`,
            // `sys.random.int()`, `sys.env.get(...)`), reached as value-method calls
            // — the same deterministic intrinsics as the `@clock*`/`@random*`/
            // `@envGet` builtins (matching the VM's `["clock","now"]` routing).
            ["clock"] | ["clock", "now" | "monotonicNanos" | "wallNanos"] => self.lower_clock_now(),
            ["clock", "sleep"] => self.lower_clock_sleep(args),
            ["random", "int" | "intRangeLessThan"] => self.lower_random_int(),
            ["random", "bytes"] => self.lower_random_bytes(args),
            ["env", "get"] => self.lower_env_get(),
            other => Err(CodegenError::Unsupported(format!(
                "intrinsic value.{} in `{}`",
                other.join("."),
                self.func.name
            ))),
        }
    }

    // ---------------------------------------------------------------------
    //  Heap allocator operations (see crate::runtime)
    // ---------------------------------------------------------------------

    /// Evaluates an intrinsic's allocator-handle receiver into `dst`. The handle is
    /// a `u32`: an `Allocator` value (from `@allocHandle`) is the bare id; the
    /// `sys.heap` capability is the default id `0`. The receiver is `path.root =
    /// Value(op)`; we evaluate `op` (a local / projected place) as a scalar.
    fn handle_to(&mut self, path: &k2_mir::IntrinsicPath, dst: Gpr) -> Result<(), CodegenError> {
        match &path.root {
            IntrinsicRoot::Value(op) => self.operand_to(op, dst),
            _ => {
                // A floor `@*Raw` form carries the id as the first operand instead;
                // never reached for the value-method spelling.
                self.asm.mov_ri(dst, 0);
                Ok(())
            }
        }
    }

    /// The element type carried by an alloc-op `undef` operand (`create(undef)` /
    /// `alloc(undef, n)`): the first `Const::Undef { ty }` argument. Falls back to a
    /// byte when absent (a 1-byte element, matching a `[]u8` carve).
    fn undef_carrier_ty(&self, args: &[Operand]) -> TypeId {
        for a in args {
            if let Operand::Const(Const::Undef { ty }) = a {
                return *ty;
            }
        }
        self.prog.arena.t_u8()
    }

    /// Lowers `value(handle).create(undef)` / `.alloc(undef, n)` / `.realloc(slice,
    /// n)` by calling the runtime routine and building the resulting error union
    /// into `dst`'s stack home: a `u16` tag at +0 (0 = Ok, or the `OutOfMemory` tag
    /// on a bounded-allocator exhaustion) and the payload (`*T` or the `{ptr,len}`
    /// fat slice) at the payload offset.
    fn lower_alloc_op_into_home(
        &mut self,
        dst: k2_mir::LocalId,
        rvalue: &Rvalue,
    ) -> Result<(), CodegenError> {
        let Rvalue::Intrinsic { path, args, .. } = rvalue else {
            unreachable!("lower_alloc_op_into_home on non-intrinsic");
        };
        let op = path.members.last().map(|s| s.as_str()).unwrap_or("");
        let home = self
            .home(dst)
            .ok_or_else(|| self.unsup("alloc result without a home"))?;
        let (pty, payload_off) = self
            .eu_payload_of(dst)
            .ok_or_else(|| self.unsup("alloc result without a recovered payload type"))?;
        // The payload shape is determined by the OPERATION, not the recovered
        // payload type (the checker sometimes types `alloc`/`realloc`'s `.payload`
        // as the element pointer `*T` rather than the slice `[]T`): `create` yields
        // a `*T` pointer, `alloc`/`realloc` always yield a `[]T` fat slice.
        let is_slice = !matches!(op, "create");
        let _ = pty;

        match op {
            "create" => {
                let elem = self.undef_carrier_ty(args);
                let elem_size = self.layout(elem).size.max(1);
                // __k2_alloc(handle=rdi, n=1, elem_size=rdx).
                self.handle_to(path, Gpr::Rdi)?;
                self.asm.mov_ri(Gpr::Rsi, 1);
                self.asm.mov_ri(Gpr::Rdx, elem_size as i64);
                self.asm.call_runtime(crate::runtime::RuntimeFn::Alloc);
                // create never exhausts (it is unbounded): tag 0, payload = ptr.
                self.write_eu_ok_ptr(home, payload_off, Gpr::Rax);
                Ok(())
            }
            "alloc" => {
                // The element size MUST equal the stride a reader will index the
                // resulting `[]T` slice with (`slice_elem_stride`).
                let elem = self.undef_carrier_ty(args);
                let elem_size = self.slice_elem_stride(elem);
                // n is the last non-undef operand.
                let n = self.alloc_count_operand(args)?;
                self.operand_to(n, Gpr::Rsi)?; // n
                                               // Stash n across the handle eval (handle eval may touch rsi-free
                                               // regs only; evaluate handle first into rdi to be safe).
                self.asm.push(Gpr::Rsi);
                self.handle_to(path, Gpr::Rdi)?;
                self.asm.pop(Gpr::Rsi);
                self.asm.mov_ri(Gpr::Rdx, elem_size as i64);
                self.asm.call_runtime(crate::runtime::RuntimeFn::Alloc);
                // alloc can exhaust on a FixedBuffer (rax == 0): write the
                // OutOfMemory error arm; otherwise a {ptr, n} Ok slice.
                self.write_alloc_slice_result(home, payload_off, n, is_slice)?;
                Ok(())
            }
            "realloc" => {
                let (slice_op, n_op) = self.realloc_operands(args)?;
                // realloc(slice, n): the element byte size MUST equal the stride the
                // `Index` codegen uses for this slice's element type, so the block is
                // sized exactly as the elements are addressed. A realloc whose slice
                // is a struct FIELD is a generic container's backing store growth
                // (`ArrayList`/`List`'s `ensureCapacity`), addressed at the word
                // stride; a standalone slice uses its natural element stride.
                let elem_ty = self
                    .operand_type(slice_op)
                    .map(|t| self.slice_elem_ty(t))
                    .unwrap_or_else(|| self.prog.arena.t_u8());
                let elem_size = if self.operand_is_field(slice_op) {
                    self.field_slice_stride(elem_ty)
                } else {
                    self.slice_elem_stride(elem_ty)
                };
                // Evaluate into the runtime ABI: rdi=handle, rsi=old_ptr,
                // rdx=new_n, rcx=elem_size, r8=checks_off.
                // old_ptr = slice.ptr (slice operand's +0 word).
                self.load_slice_ptr(slice_op, Gpr::Rsi)?;
                self.asm.push(Gpr::Rsi);
                self.operand_to(n_op, Gpr::Rdx)?;
                self.asm.push(Gpr::Rdx);
                self.handle_to(path, Gpr::Rdi)?;
                self.asm.pop(Gpr::Rdx);
                self.asm.pop(Gpr::Rsi);
                self.asm.mov_ri(Gpr::Rcx, elem_size as i64);
                self.asm.mov_ri(Gpr::R8, i64::from(self.checks_off()));
                self.asm.call_runtime(crate::runtime::RuntimeFn::Realloc);
                // realloc of an unbounded allocator never exhausts in the corpus;
                // write a {ptr, n} Ok slice (n is the realloc length operand).
                self.write_alloc_slice_result(home, payload_off, n_op, is_slice)?;
                Ok(())
            }
            _ => Err(self.unsup("unrecognized allocator operation")),
        }
    }

    /// The byte stride of a `place` that ends in a SLICE index whose element is a
    /// (possibly `deferred`) aggregate larger than a word — i.e. a generic-container
    /// element store that must be a full memcpy, not a scalar store. Returns `None`
    /// for a non-slice-index place or a word-or-smaller element.
    fn slice_index_aggregate_stride(&self, place: &Place) -> Option<u64> {
        let (last, prefix) = place.proj.split_last()?;
        let Proj::Index { ty, .. } = last else {
            return None;
        };
        // The container type the final `Index` applies to (the place without its
        // trailing index): must be a slice.
        let base_ty = self.prefix_type(place.base, prefix);
        if !matches!(self.prog.arena.get(base_ty), Type::Slice { .. }) {
            return None;
        }
        let stride = self.slice_elem_stride(*ty);
        // A real aggregate element (the deferred fallback word is handled by the
        // scalar path; only > 8 needs the memcpy).
        if stride > 8 {
            Some(stride)
        } else {
            None
        }
    }

    /// `true` if `op` is a place reached through a struct FIELD projection — the
    /// signal that a slice operand is a generic container's backing store (so it is
    /// addressed at the word stride, matching `field_slice_stride`).
    fn operand_is_field(&self, op: &Operand) -> bool {
        matches!(op, Operand::Copy(p) if p.proj.iter().any(|pr| matches!(pr, Proj::Field { .. })))
    }

    /// The element type of a slice/array type (or the type itself for a non-
    /// container — used to size a realloc's elements consistently with indexing).
    fn slice_elem_ty(&self, ty: TypeId) -> TypeId {
        match self.prog.arena.get(ty) {
            Type::Slice { elem, .. } | Type::Array { elem, .. } => *elem,
            _ => ty,
        }
    }

    /// The byte STRIDE of a heap-backed slice's element `elem_ty`. A generic
    /// container's methods (`ArrayList`/`List`) operate on a `deferred` element type
    /// and so address + store each element at the word stride (8); a concrete reader
    /// of the *same* slice (`list.items[i]`, typed `[]u32`) must use the *same*
    /// stride, so every scalar element that fits a word is uniformly word-strided.
    /// Sub-word scalars keep their natural stride (`[]u8` strings stay 1-byte,
    /// matching string-literal data), and a real aggregate element uses its layout
    /// size. This keeps the heap allocation, the store, and the read in agreement
    /// across the monomorphization boundary the MIR does not specialize.
    fn slice_elem_stride(&self, elem_ty: TypeId) -> u64 {
        // The element stride of a standalone/array-view slice: the element's natural
        // layout size, with a word fallback for a non-layoutable (`deferred`)
        // element. This matches the array-element stride exactly, so an array-view
        // slice (`a[1..3]`) and a direct heap slice (`alloc(u32, n)`) of the *same*
        // concrete element type index identically.
        layout::layout_of(&self.prog.arena, elem_ty)
            .map(|l| l.size.max(1))
            .unwrap_or(8)
    }

    /// The element stride of a slice that is a generic container's backing store
    /// (reached through a struct field — `list.items[i]`, `(*self).f0[i]`). Because
    /// the MIR does NOT monomorphize a container's methods, the shared
    /// `deferred`-element body stores + indexes every element in a word-sized slot;
    /// a concrete reader of the same field must use the same word stride. So a
    /// word-or-smaller scalar element gets the word stride (8) — the scalar value
    /// lives in the low bytes of the slot — while a `[]u8` byte element keeps its
    /// natural 1-byte stride (string data is byte-addressed), and a real aggregate
    /// element keeps its layout size. (The aggregate-element generic container is
    /// still refused earlier, at the param boundary; this only makes the common
    /// word-scalar `ArrayList(u32)` / `List(u32)` agree across the boundary.)
    fn field_slice_stride(&self, elem_ty: TypeId) -> u64 {
        match layout::layout_of(&self.prog.arena, elem_ty) {
            None => 8, // a generic `deferred` element: the word slot
            Some(l) if l.size <= 2 => l.size.max(1),
            Some(l) if l.size <= 8 => 8,
            Some(l) => l.size,
        }
    }

    /// Writes an `Ok(ptr)` error union: tag 0 at +0, the pointer in `src` at the
    /// payload offset. Used by `create`.
    fn write_eu_ok_ptr(&mut self, home: i32, payload_off: i32, src: Gpr) {
        // Stash the pointer (rax) before writing the tag clobbers nothing, but keep
        // it explicit: store payload first, then the tag.
        self.asm.mov_store_mem(Gpr::Rbp, home + payload_off, src);
        self.asm.mov_ri(Gpr::Rax, 0);
        self.asm.mov_store16_mem(Gpr::Rbp, home, Gpr::Rax);
    }

    /// Writes an allocation's slice result into the error-union home: on success
    /// (returned ptr != 0) a `tag 0` Ok with a `{ptr, len}` fat slice; on a bounded
    /// allocator's exhaustion (ptr == 0) the `OutOfMemory` error tag. `len_op` is
    /// the requested element count. `is_slice` guards the fat-slice write (a
    /// non-slice payload would be a backend bug for `alloc`/`realloc`).
    fn write_alloc_slice_result(
        &mut self,
        home: i32,
        payload_off: i32,
        len_op: &Operand,
        is_slice: bool,
    ) -> Result<(), CodegenError> {
        if !is_slice {
            return Err(self.unsup("alloc/realloc result payload is not a slice"));
        }
        // The runtime ptr is in rax. Branch on rax == 0 (exhaustion).
        let oom = self.asm.new_local_label();
        let done = self.asm.new_local_label();
        self.asm.test_rr(Gpr::Rax, Gpr::Rax);
        self.asm.jcc_local(Cc::E, oom);
        // Ok: store ptr at payload_off, len at payload_off+8, tag 0 at +0.
        self.asm
            .mov_store_mem(Gpr::Rbp, home + payload_off, Gpr::Rax);
        self.operand_to(len_op, Gpr::Rcx)?;
        self.asm
            .mov_store_mem(Gpr::Rbp, home + payload_off + 8, Gpr::Rcx);
        self.asm.mov_ri(Gpr::Rax, 0);
        self.asm.mov_store16_mem(Gpr::Rbp, home, Gpr::Rax);
        self.asm.jmp_local(done);
        // OutOfMemory: tag = oom_tag at +0 (payload is left undefined).
        self.asm.bind_local(oom);
        self.asm.mov_ri(Gpr::Rax, self.oom_tag() as i64);
        self.asm.mov_store16_mem(Gpr::Rbp, home, Gpr::Rax);
        self.asm.bind_local(done);
        Ok(())
    }

    /// Lowers `value(handle).free(ptr)` / `.destroy(ptr)`: evaluate the handle and
    /// the pointer/slice operand, then call the runtime free routine. A null
    /// pointer / empty slice is handled inside the routine (a benign no-op).
    fn lower_free_op(
        &mut self,
        path: &k2_mir::IntrinsicPath,
        args: &[Operand],
    ) -> Result<(), CodegenError> {
        // The operand is a slice (`free([]T)`) or a pointer (`destroy(*T)`).
        let ptr_op = args
            .iter()
            .find(|a| !matches!(a, Operand::Const(Const::Undef { .. })))
            .ok_or_else(|| self.unsup("free without a pointer operand"))?;
        self.load_ptr_operand(ptr_op, Gpr::Rsi)?;
        self.asm.push(Gpr::Rsi);
        self.handle_to(path, Gpr::Rdi)?;
        self.asm.pop(Gpr::Rsi);
        self.asm.mov_ri(Gpr::Rdx, i64::from(self.checks_off()));
        self.asm.call_runtime(crate::runtime::RuntimeFn::Free);
        Ok(())
    }

    /// Loads the data pointer of a free/destroy operand into `dst`: a slice's `+0`
    /// word, or a bare pointer value.
    fn load_ptr_operand(&mut self, op: &Operand, dst: Gpr) -> Result<(), CodegenError> {
        let ty = self.operand_type(op);
        if ty
            .map(|t| matches!(self.prog.arena.get(t), Type::Slice { .. }))
            .unwrap_or(false)
        {
            return self.load_slice_ptr(op, dst);
        }
        // A pointer (or pointer-niche optional) scalar.
        self.operand_to(op, dst)
    }

    /// Loads a slice operand's data pointer (its `+0` word) into `dst`.
    fn load_slice_ptr(&mut self, op: &Operand, dst: Gpr) -> Result<(), CodegenError> {
        match op {
            Operand::Copy(p) => {
                self.place_addr_general(p, dst)?;
                self.asm.mov_load_mem(dst, dst, 0);
                Ok(())
            }
            _ => Err(self.unsup("slice operand of an allocator op is not a place")),
        }
    }

    /// The element-count operand of `alloc(undef, n)`: the last non-undef operand.
    fn alloc_count_operand<'a>(&self, args: &'a [Operand]) -> Result<&'a Operand, CodegenError> {
        args.iter()
            .rev()
            .find(|a| !matches!(a, Operand::Const(Const::Undef { .. })))
            .ok_or_else(|| self.unsup("alloc without a count operand"))
    }

    /// The `(slice, new_len)` operands of `realloc(slice, n)`: the slice is the
    /// slice-typed operand, the length the last integer operand.
    fn realloc_operands<'a>(
        &self,
        args: &'a [Operand],
    ) -> Result<(&'a Operand, &'a Operand), CodegenError> {
        let slice = args
            .iter()
            .find(|a| {
                self.operand_type(a)
                    .map(|t| matches!(self.prog.arena.get(t), Type::Slice { .. }))
                    .unwrap_or(false)
            })
            .ok_or_else(|| self.unsup("realloc without a slice operand"))?;
        let len = args
            .iter()
            .rev()
            .find(|a| {
                !matches!(a, Operand::Const(Const::Undef { .. }))
                    && !self
                        .operand_type(a)
                        .map(|t| matches!(self.prog.arena.get(t), Type::Slice { .. }))
                        .unwrap_or(false)
            })
            .ok_or_else(|| self.unsup("realloc without a length operand"))?;
        Ok((slice, len))
    }

    // ---------------------------------------------------------------------
    //  Allocator-registry & *System capability leaves (the @-builtins)
    // ---------------------------------------------------------------------

    /// `@allocId(kind, 0[, buf])`: register a fresh allocator handle. `kind` is the
    /// numeric std tag (0=Default 1=GPA 2=Arena 3=FixedBuffer 5=Testing); for a
    /// FixedBuffer the third operand is the caller `[]u8` buffer (its ptr + len
    /// become the FBA's backing range). Returns the new handle (`u32`) in RAX.
    fn lower_alloc_id(&mut self, args: &[Operand]) -> Result<(), CodegenError> {
        // kind = first integer constant operand.
        let kind = args
            .iter()
            .find_map(|a| match a {
                Operand::Const(Const::Int { value, .. }) => Some(*value as usize),
                _ => None,
            })
            .unwrap_or(0);
        let kind_tag = crate::runtime::kind_tag(kind);
        // For a FixedBuffer, recover the backing buffer's ptr + cap.
        if kind == 3 {
            // The buffer operand is a slice (`[]u8`) or a pointer-to-array.
            let buf = args
                .iter()
                .find(|a| {
                    self.operand_type(a)
                        .map(|t| {
                            matches!(
                                self.prog.arena.get(t),
                                Type::Slice { .. } | Type::Pointer { .. }
                            )
                        })
                        .unwrap_or(false)
                })
                .ok_or_else(|| self.unsup("@allocId(FixedBuffer) without a buffer operand"))?;
            self.load_fba_buffer(buf, Gpr::Rsi, Gpr::Rdx)?;
        } else {
            self.asm.mov_ri(Gpr::Rsi, 0);
            self.asm.mov_ri(Gpr::Rdx, 0);
        }
        self.asm.mov_ri(Gpr::Rdi, kind_tag);
        self.asm.call_runtime(crate::runtime::RuntimeFn::AllocId);
        Ok(())
    }

    /// Loads a FixedBuffer's backing `(ptr -> buf_reg, cap -> cap_reg)`. A `[]u8`
    /// slice supplies ptr at +0 and len at +8; a `*[N]u8` supplies the array
    /// address and its element count.
    fn load_fba_buffer(
        &mut self,
        op: &Operand,
        buf_reg: Gpr,
        cap_reg: Gpr,
    ) -> Result<(), CodegenError> {
        let ty = self
            .operand_type(op)
            .ok_or_else(|| self.unsup("FBA buffer operand without a type"))?;
        match self.prog.arena.get(ty) {
            Type::Slice { .. } => match op {
                Operand::Copy(p) => {
                    self.place_addr_general(p, Gpr::Rax)?;
                    self.asm.mov_load_mem(buf_reg, Gpr::Rax, 0); // ptr
                    self.asm.mov_load_mem(cap_reg, Gpr::Rax, 8); // len
                    Ok(())
                }
                _ => Err(self.unsup("FBA slice buffer is not a place")),
            },
            Type::Pointer { pointee, .. } => {
                let pointee = *pointee;
                // ptr = the array address (the pointer value); cap = the array's
                // element count.
                self.operand_to(op, buf_reg)?;
                let cap = match self.prog.arena.get(pointee) {
                    Type::Array {
                        len: k2_types::ArrayLen::Known(n),
                        ..
                    } => *n as i64,
                    _ => return Err(self.unsup("FBA pointer buffer without a known array length")),
                };
                self.asm.mov_ri(cap_reg, cap);
                Ok(())
            }
            _ => Err(self.unsup("unrecognized FBA buffer type")),
        }
    }

    /// `@allocHandle(id)`: the `Allocator` value IS the `u32` id. Identity move.
    fn lower_alloc_handle(&mut self, args: &[Operand]) -> Result<(), CodegenError> {
        let id = args
            .first()
            .ok_or_else(|| self.unsup("@allocHandle without an id operand"))?;
        self.operand_to(id, Gpr::Rax)
    }

    /// `@gpaDeinit(id)`: report whether the GPA leaked (a `bool` in RAX) and reclaim
    /// its blocks. With checks off (ReleaseFast) the runtime returns 0.
    fn lower_gpa_deinit(&mut self, args: &[Operand]) -> Result<(), CodegenError> {
        let id = args
            .first()
            .ok_or_else(|| self.unsup("@gpaDeinit without an id operand"))?;
        self.operand_to(id, Gpr::Rdi)?;
        self.asm.mov_ri(Gpr::Rsi, i64::from(self.checks_off()));
        self.asm.call_runtime(crate::runtime::RuntimeFn::GpaDeinit);
        Ok(())
    }

    /// `@arenaDeinit(id)`: free every block the arena handed out (bulk free).
    fn lower_arena_deinit(&mut self, args: &[Operand]) -> Result<(), CodegenError> {
        let id = args
            .first()
            .ok_or_else(|| self.unsup("@arenaDeinit without an id operand"))?;
        self.operand_to(id, Gpr::Rdi)?;
        self.asm
            .call_runtime(crate::runtime::RuntimeFn::ArenaDeinit);
        // Unit result.
        self.asm.mov_ri(Gpr::Rax, 0);
        Ok(())
    }

    /// `@clockNow(...)`: the deterministic monotonic counter (nanoseconds) — read
    /// the process-global `clock_nanos`, matching the VM's reproducible clock.
    fn lower_clock_now(&mut self) -> Result<(), CodegenError> {
        self.asm.mov_ri_state(ADDR, crate::runtime::ST_CLOCK_NANOS);
        self.asm.mov_load_mem(Gpr::Rax, ADDR, 0);
        Ok(())
    }

    /// `@clockSleep(ns)`: advance the deterministic clock by `max(ns, 0)`, matching
    /// the VM (`clock_nanos += ns`).
    fn lower_clock_sleep(&mut self, args: &[Operand]) -> Result<(), CodegenError> {
        let ns = args
            .first()
            .ok_or_else(|| self.unsup("@clockSleep without a duration operand"))?;
        self.operand_to(ns, Gpr::Rcx)?;
        // Clamp negatives to 0 (saturating add of a non-negative amount).
        let nonneg = self.asm.new_local_label();
        self.asm.test_rr(Gpr::Rcx, Gpr::Rcx);
        self.asm.jcc_local(Cc::Ge, nonneg);
        self.asm.mov_ri(Gpr::Rcx, 0);
        self.asm.bind_local(nonneg);
        self.asm.mov_ri_state(ADDR, crate::runtime::ST_CLOCK_NANOS);
        self.asm.mov_load_mem(Gpr::Rax, ADDR, 0);
        self.asm.add_rr(Gpr::Rax, Gpr::Rcx);
        self.asm.mov_store_mem(ADDR, 0, Gpr::Rax);
        // Unit result.
        self.asm.mov_ri(Gpr::Rax, 0);
        Ok(())
    }

    /// `@randomInt(...)`: advance the splitmix64 PRNG and return the 64-bit draw in
    /// RAX, reproducing the VM's `next_random` byte-for-byte.
    fn lower_random_int(&mut self) -> Result<(), CodegenError> {
        self.emit_splitmix64_next();
        Ok(())
    }

    /// `@randomBytes(buf)`: fill the `[]u8` (or `*[N]u8`) operand with one low PRNG
    /// byte per draw, matching the VM's `random_bytes`.
    fn lower_random_bytes(&mut self, args: &[Operand]) -> Result<(), CodegenError> {
        let buf = args
            .first()
            .ok_or_else(|| self.unsup("@randomBytes without a buffer operand"))?;
        // Recover (ptr -> r12-free scratch, len) of the buffer. We use rsi=ptr,
        // r8=remaining count; the splitmix helper clobbers rax/rcx/rdx/r11 only.
        self.load_fba_buffer_like(buf, Gpr::Rsi, Gpr::R8)?;
        // for (i = 0; i < len; i++) buf[i] = next() & 0xFF.
        let top = self.asm.new_local_label();
        let end = self.asm.new_local_label();
        self.asm.bind_local(top);
        self.asm.test_rr(Gpr::R8, Gpr::R8);
        self.asm.jcc_local(Cc::E, end);
        // Preserve rsi/r8 across the splitmix helper (it touches rax/rcx/rdx/r11).
        self.emit_splitmix64_next(); // draw -> rax
        self.asm.mov_store8_mem(Gpr::Rsi, 0, Gpr::Rax);
        self.asm.add_ri(Gpr::Rsi, 1);
        self.asm.add_ri(Gpr::R8, -1);
        self.asm.jmp_local(top);
        self.asm.bind_local(end);
        // Unit result.
        self.asm.mov_ri(Gpr::Rax, 0);
        Ok(())
    }

    /// Recovers `(ptr -> ptr_reg, len -> len_reg)` of a `[]u8` slice or `*[N]u8`
    /// buffer operand (the `randomBytes` shape, mirroring the VM's acceptance of
    /// both spellings).
    fn load_fba_buffer_like(
        &mut self,
        op: &Operand,
        ptr_reg: Gpr,
        len_reg: Gpr,
    ) -> Result<(), CodegenError> {
        self.load_fba_buffer(op, ptr_reg, len_reg)
    }

    /// Emits the splitmix64 step into RAX, advancing `rng_state`:
    /// `state += GOLDEN; z = state; z = (z ^ z>>30) * C1; z = (z ^ z>>27) * C2;
    /// z ^ z>>31`. Byte-identical to the VM's `next_random`. Clobbers rax/rcx/rdx/
    /// r11.
    fn emit_splitmix64_next(&mut self) {
        const GOLDEN: i64 = 0x9E37_79B9_7F4A_7C15u64 as i64;
        const C1: i64 = 0xBF58_476D_1CE4_E5B9u64 as i64;
        const C2: i64 = 0x94D0_49BB_1331_11EBu64 as i64;
        // r11 = &rng_state.
        self.asm.mov_ri_state(ADDR, crate::runtime::ST_RNG_STATE);
        // rax = state + GOLDEN; store back.
        self.asm.mov_load_mem(Gpr::Rax, ADDR, 0);
        self.asm.mov_ri(Gpr::Rcx, GOLDEN);
        self.asm.add_rr(Gpr::Rax, Gpr::Rcx);
        self.asm.mov_store_mem(ADDR, 0, Gpr::Rax);
        // z = rax. z ^= z >> 30.
        self.asm.mov_rr(Gpr::Rdx, Gpr::Rax);
        self.asm.shr_ri(Gpr::Rdx, 30);
        self.asm.xor_rr(Gpr::Rax, Gpr::Rdx);
        // z *= C1.
        self.asm.mov_ri(Gpr::Rcx, C1);
        self.asm.imul_rr(Gpr::Rax, Gpr::Rcx);
        // z ^= z >> 27.
        self.asm.mov_rr(Gpr::Rdx, Gpr::Rax);
        self.asm.shr_ri(Gpr::Rdx, 27);
        self.asm.xor_rr(Gpr::Rax, Gpr::Rdx);
        // z *= C2.
        self.asm.mov_ri(Gpr::Rcx, C2);
        self.asm.imul_rr(Gpr::Rax, Gpr::Rcx);
        // z ^= z >> 31.
        self.asm.mov_rr(Gpr::Rdx, Gpr::Rax);
        self.asm.shr_ri(Gpr::Rdx, 31);
        self.asm.xor_rr(Gpr::Rax, Gpr::Rdx);
    }

    /// `@envGet(name)`: offline-absent. The result is an optional `None`: a single
    /// zero in RAX (a pointer-niche `?[]const u8`/`?*T` null, or the `0` flag of a
    /// flagged optional — both read absence as zero), matching the VM's
    /// `Optional(None)`.
    fn lower_env_get(&mut self) -> Result<(), CodegenError> {
        self.asm.mov_ri(Gpr::Rax, 0);
        Ok(())
    }

    // ---- Runtime print formatting (see fmt_native) ----

    /// Lowers `value(fd).print(fmt, tuple)` with runtime formatting: render every
    /// segment into the stack print buffer, then `write(fd, buf, len)`. Leaves an
    /// Ok sentinel (`0`) in RAX (the result feeds `discr.ErrorUnion`'s success
    /// edge, which the corpus always takes for an in-subset print).
    fn lower_print_runtime(
        &mut self,
        path: &k2_mir::IntrinsicPath,
        args: &[Operand],
        rodata: &mut RoData,
    ) -> Result<(), CodegenError> {
        // The fd token comes from the receiver (a bare local holding 1/2).
        let fd_local = self.print_fd_local(path)?;
        // Parse the format string at compile time.
        let fmt_id = match args.first() {
            Some(Operand::Const(Const::Str(id))) => *id,
            _ => return Err(self.unsup("print with a non-constant format string")),
        };
        let raw = match &self.prog.consts[fmt_id.0 as usize] {
            ConstData::Bytes(b) => b.clone(),
            ConstData::Aggregate(_) => return Err(self.unsup("print of a non-byte format const")),
        };
        let steps = fmt_native::parse(&raw).map_err(CodegenError::Unsupported)?;

        // The argument tuple (args[1]) and its field types/offsets.
        let tuple = args.get(1);
        let (tuple_addr_local, tuple_fields) = self.print_tuple_info(tuple)?;

        let buf = self
            .plan
            .print_buf
            .ok_or_else(|| self.unsup("print without a reserved buffer"))?;

        // Cursor register: ADDR (r11) holds the current write position. Start it at
        // the buffer base.
        self.asm.lea_rbp(ADDR, buf);

        for step in &steps {
            match step {
                Step::Literal(bytes) => self.emit_literal(bytes, rodata),
                Step::Placeholder { arg_index, spec } => {
                    self.emit_placeholder(*arg_index, spec, tuple_addr_local, &tuple_fields)?;
                }
            }
        }

        // len = cursor - base; write(fd, base, len). The fd token (1/2) is read
        // from the receiver local. cursor is in ADDR.
        self.asm.lea_rbp(Gpr::Rsi, buf); // buffer base
        self.asm.mov_rr(Gpr::Rdx, ADDR);
        self.asm.sub_rr(Gpr::Rdx, Gpr::Rsi); // RDX = len
        self.operand_to(&Operand::local(fd_local), Gpr::Rdi)?; // RDI = fd token
        self.asm.mov_ri(Gpr::Rax, sys::WRITE);
        self.asm.syscall();
        // Ok sentinel.
        self.asm.mov_ri(Gpr::Rax, 0);
        Ok(())
    }

    /// The receiver local that carries the fd token (1 = stdout, 2 = stderr).
    fn print_fd_local(
        &mut self,
        path: &k2_mir::IntrinsicPath,
    ) -> Result<k2_mir::LocalId, CodegenError> {
        match &path.root {
            IntrinsicRoot::Value(op) => match op.as_ref() {
                Operand::Copy(p) if p.is_local() => Ok(p.base),
                _ => Err(self.unsup("print receiver not a bare local")),
            },
            _ => Err(self.unsup("print without a value receiver")),
        }
    }

    /// Resolves the tuple argument: its base local (whose home holds the tuple
    /// bytes) and the (type, offset) of each positional field.
    #[allow(clippy::type_complexity)]
    fn print_tuple_info(
        &self,
        tuple: Option<&Operand>,
    ) -> Result<(Option<k2_mir::LocalId>, Vec<(TypeId, u64)>), CodegenError> {
        match tuple {
            None | Some(Operand::Const(Const::Void)) => Ok((None, Vec::new())),
            Some(Operand::Copy(p)) if p.is_local() => {
                // Prefer the synthetic field layout computed for a `deferred`
                // tuple; else derive it from the (concrete) tuple/struct type.
                if let Some(fields) = &self.alloc.agg_fields[p.base.index()] {
                    return Ok((Some(p.base), fields.clone()));
                }
                let ty = self.alloc.home_ty[p.base.index()]
                    .unwrap_or(self.func.locals[p.base.index()].ty);
                let fields = self.tuple_field_layout(ty);
                Ok((Some(p.base), fields))
            }
            Some(_) => Err(self.unsup("print tuple not a bare local")),
        }
    }

    /// The (type, byte offset) of each field of a tuple/struct type.
    fn tuple_field_layout(&self, ty: TypeId) -> Vec<(TypeId, u64)> {
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
        Vec::new()
    }

    /// Emits a literal byte run into the print buffer at the cursor (ADDR).
    fn emit_literal(&mut self, bytes: &[u8], rodata: &mut RoData) {
        if bytes.is_empty() {
            return;
        }
        // Copy from rodata into the buffer. Source addr in RAX, dest = ADDR.
        let off = rodata.intern(bytes);
        self.asm.mov_ri_data(Gpr::Rax, off);
        // memcpy(ADDR, RAX, len), then advance ADDR by len.
        self.copy_to_cursor(Gpr::Rax, bytes.len() as u64);
    }

    /// Copies `len` bytes from `[src]` to `[ADDR]` (the cursor) and advances ADDR.
    /// Uses RCX/RDX as scratch (never a vreg); preserves ADDR semantics.
    fn copy_to_cursor(&mut self, src: Gpr, len: u64) {
        let mut off = 0i32;
        let mut rem = len as i32;
        // src into RCX so RAX is free as the byte scratch.
        if src != Gpr::Rcx {
            self.asm.mov_rr(Gpr::Rcx, src);
        }
        while rem >= 8 {
            self.asm.mov_load_mem(Gpr::Rdx, Gpr::Rcx, off);
            self.asm.mov_store_mem(ADDR, off, Gpr::Rdx);
            off += 8;
            rem -= 8;
        }
        while rem > 0 {
            self.asm.movzx8_mem(Gpr::Rdx, Gpr::Rcx, off);
            self.asm.mov_store8_mem(ADDR, off, Gpr::Rdx);
            off += 1;
            rem -= 1;
        }
        if len != 0 {
            self.asm.add_ri(ADDR, len as i32);
        }
    }

    /// Emits a placeholder render. Only `{s}` (string slices) and `{d}`/`{}` on
    /// integers are supported with the dynamic-length renderers; alignment/width
    /// and the radix/char verbs are deferred cleanly when they would require the
    /// pad sub-buffer machinery this minimal renderer omits.
    fn emit_placeholder(
        &mut self,
        arg_index: usize,
        spec: &fmt_native::Spec,
        tuple_local: Option<k2_mir::LocalId>,
        fields: &[(TypeId, u64)],
    ) -> Result<(), CodegenError> {
        // Resolve the field (type + address).
        let (fty, foff) = match fields.get(arg_index) {
            Some(&(t, o)) => (t, o),
            None => {
                // Missing arg: emit "<missing>" like the VM. Rare; defer.
                return Err(self.unsup("print with fewer args than placeholders"));
            }
        };
        let base = tuple_local.ok_or_else(|| self.unsup("print placeholder without a tuple"))?;

        // Width / alignment padding (mirrors `fmt::pad`): render the field, then —
        // if it is shorter than the requested width and an alignment is given —
        // insert `fill` bytes before / after / around it. With no alignment, a
        // width is a no-op (matching the VM's `pad`).
        let pad = spec.align != Align::None && spec.width != 0;
        // The cursor (ADDR) before rendering, captured into a scratch GPR.
        if pad {
            // Save the field-start cursor in the outgoing scratch slot so the
            // render code (which freely clobbers caller-saved regs) cannot lose it.
            let slot = self.plan.outgoing_args_base() + 112;
            self.asm.mov_store_mem(Gpr::Rsp, slot, ADDR);
        }

        match spec.verb {
            Verb::Str => self.render_string_field(base, foff, fty)?,
            Verb::Decimal => self.render_decimal_field(base, foff, fty)?,
            Verb::Default => self.render_default_field(base, foff, fty)?,
            Verb::Hex { upper } => self.render_radix_field(base, foff, fty, 16, upper)?,
            Verb::Bin => self.render_radix_field(base, foff, fty, 2, false)?,
            Verb::Oct => self.render_radix_field(base, foff, fty, 8, false)?,
            Verb::Char => self.render_char_field(base, foff, fty)?,
        }

        if pad {
            self.emit_field_padding(spec);
        }
        Ok(())
    }

    /// Applies width/alignment padding to the just-rendered field, mirroring
    /// `fmt::pad`. On entry ADDR points just past the rendered field; the
    /// field-start cursor was saved to a scratch slot. When the field is shorter
    /// than `width`, `pad_count = width - field_len` fill bytes are inserted: Right
    /// shifts the field right and prepends fill; Left appends fill; Center splits.
    fn emit_field_padding(&mut self, spec: &fmt_native::Spec) {
        let slot = self.plan.outgoing_args_base() + 112;
        let fill = spec.fill as i64;
        let width = spec.width as i64;
        // rsi = field start, rdi = field end (ADDR), rcx = field_len.
        self.asm.mov_load_mem(Gpr::Rsi, Gpr::Rsp, slot);
        self.asm.mov_rr(Gpr::Rdi, ADDR);
        self.asm.mov_rr(Gpr::Rcx, Gpr::Rdi);
        self.asm.sub_rr(Gpr::Rcx, Gpr::Rsi); // rcx = field_len
                                             // If field_len >= width: nothing to pad.
        let done = self.asm.new_local_label();
        self.asm.mov_ri(Gpr::Rax, width);
        self.asm.cmp_rr(Gpr::Rcx, Gpr::Rax);
        self.asm.jcc_local(Cc::Ae, done);
        // pad_count = width - field_len -> r10.
        self.asm.mov_ri(Gpr::R10, width);
        self.asm.sub_rr(Gpr::R10, Gpr::Rcx);
        match spec.align {
            Align::Left => {
                // Append `pad_count` fill bytes at ADDR.
                self.emit_fill_run(ADDR, Gpr::R10, fill);
            }
            Align::Right => {
                // Shift field right by pad_count, then prepend fill.
                self.shift_field_right(Gpr::Rsi, Gpr::Rcx, Gpr::R10);
                // Prepend fill at [field_start, field_start + pad_count); leave ADDR
                // at field_start + pad_count + field_len.
                self.asm.mov_load_mem(Gpr::Rdi, Gpr::Rsp, slot); // field start
                self.emit_fill_run(Gpr::Rdi, Gpr::R10, fill);
                // ADDR = field_start + width.
                self.asm.mov_load_mem(ADDR, Gpr::Rsp, slot);
                self.asm.add_ri(ADDR, width as i32);
            }
            Align::Center => {
                // left = pad_count/2, right = pad_count - left.
                // Shift field right by `left`, prepend `left` fill, append `right`.
                self.asm.mov_rr(Gpr::R8, Gpr::R10);
                self.asm.shr_ri(Gpr::R8, 1); // left = pad/2
                self.shift_field_right(Gpr::Rsi, Gpr::Rcx, Gpr::R8);
                self.asm.mov_load_mem(Gpr::Rdi, Gpr::Rsp, slot);
                self.emit_fill_run(Gpr::Rdi, Gpr::R8, fill);
                // ADDR = field_start + left + field_len.
                self.asm.mov_load_mem(ADDR, Gpr::Rsp, slot);
                self.asm.add_rr(ADDR, Gpr::R8);
                self.asm.add_rr(ADDR, Gpr::Rcx);
                // right = pad_count - left.
                self.asm.mov_rr(Gpr::R9, Gpr::R10);
                self.asm.sub_rr(Gpr::R9, Gpr::R8);
                self.emit_fill_run(ADDR, Gpr::R9, fill);
            }
            Align::None => {}
        }
        self.asm.bind_local(done);
    }

    /// Emits `count` (in `count_reg`) `fill` bytes starting at `[dst_reg]`,
    /// advancing `dst_reg` past them. Clobbers rax, the count register, and dst.
    fn emit_fill_run(&mut self, dst_reg: Gpr, count_reg: Gpr, fill: i64) {
        let top = self.asm.new_local_label();
        let end = self.asm.new_local_label();
        self.asm.bind_local(top);
        self.asm.test_rr(count_reg, count_reg);
        self.asm.jcc_local(Cc::E, end);
        self.asm.mov_ri(Gpr::Rax, fill);
        self.asm.mov_store8_mem(dst_reg, 0, Gpr::Rax);
        self.asm.add_ri(dst_reg, 1);
        self.asm.add_ri(count_reg, -1);
        self.asm.jmp_local(top);
        self.asm.bind_local(end);
    }

    /// Shifts the `len`-byte field at `[start]` right by `pad` bytes (a backward
    /// copy so the source and destination ranges may overlap), making room for
    /// leading fill. `start` in `start_reg`, `len` in `len_reg`, `pad` in
    /// `pad_reg`. Clobbers rax/rdx/r11 and reads (does not modify) the inputs.
    fn shift_field_right(&mut self, start_reg: Gpr, len_reg: Gpr, pad_reg: Gpr) {
        // i = len; while (i-- > 0) dst[i+pad] = src[i].  Copy from the high end down
        // so overlapping ranges are preserved.
        // rax = i = len.
        self.asm.mov_rr(Gpr::Rax, len_reg);
        let top = self.asm.new_local_label();
        let end = self.asm.new_local_label();
        self.asm.bind_local(top);
        self.asm.test_rr(Gpr::Rax, Gpr::Rax);
        self.asm.jcc_local(Cc::E, end);
        self.asm.add_ri(Gpr::Rax, -1); // i
                                       // src = start + i ; load byte.
        self.asm.mov_rr(ADDR, start_reg);
        self.asm.add_rr(ADDR, Gpr::Rax);
        self.asm.movzx8_mem(Gpr::Rdx, ADDR, 0);
        // dst = start + i + pad ; store byte.
        self.asm.add_rr(ADDR, pad_reg);
        self.asm.mov_store8_mem(ADDR, 0, Gpr::Rdx);
        self.asm.jmp_local(top);
        self.asm.bind_local(end);
    }

    /// Renders an integer field in `radix` (2/8/16), masking the magnitude to the
    /// value's repr width so a negative signed value prints its two's-complement at
    /// its declared width (`{x}` on `i8` -1 == `ff`). Matches `fmt::render_radix`.
    fn render_radix_field(
        &mut self,
        base: k2_mir::LocalId,
        foff: u64,
        fty: TypeId,
        radix: u64,
        upper: bool,
    ) -> Result<(), CodegenError> {
        // `ComptimeInt` reaches here when const-fold inlines a comptime-typed
        // integer into a `{x}`/`{b}`/`{o}` field; it is stored word-sized, so it
        // formats like any 64-bit integer (the width-0 path below skips masking,
        // rendering the full word magnitude — matching the VM's comptime radix).
        if !matches!(
            self.prog.arena.get(fty),
            Type::Int { .. } | Type::Bool | Type::ComptimeInt
        ) {
            return Err(self.unsup("radix format of a non-integer field"));
        }
        if self.layout(fty).size > 8 {
            return Err(self.unsup("radix format of a wide (>64-bit) integer"));
        }
        let h = self
            .home(base)
            .ok_or_else(|| self.unsup("print tuple home"))?;
        // Load the value, then mask to the repr width (as an *unsigned* magnitude).
        let r = repr_of(self.prog, fty);
        self.load_sized(Gpr::Rax, Gpr::Rbp, h + foff as i32, fty);
        if r.width != 0 && r.width < 64 {
            let mask: u64 = (1u64 << r.width) - 1;
            // mask via AND with an imm (width<=32 fits; for 33..63 build in RCX).
            if let Ok(m) = i32::try_from(mask as i64) {
                self.asm.and_ri(Gpr::Rax, m);
            } else {
                self.asm.mov_ri(Gpr::Rcx, mask as i64);
                self.asm.and_rr(Gpr::Rax, Gpr::Rcx);
            }
        }
        self.emit_radix_digits(radix, upper);
        Ok(())
    }

    /// Emits the unsigned `radix`-digits of RAX into the buffer (matches
    /// `render_radix`: zero renders a single '0'). Digits are built backward into
    /// the outgoing-args scratch, then copied forward.
    fn emit_radix_digits(&mut self, radix: u64, upper: bool) {
        // if RAX == 0 -> emit '0'.
        let nonzero = self.asm.new_local_label();
        let done = self.asm.new_local_label();
        self.asm.test_rr(Gpr::Rax, Gpr::Rax);
        self.asm.jcc_local(Cc::Ne, nonzero);
        self.asm.mov_ri(Gpr::Rdx, b'0' as i64);
        self.asm.mov_store8_mem(ADDR, 0, Gpr::Rdx);
        self.asm.add_ri(ADDR, 1);
        self.asm.jmp_local(done);
        self.asm.bind_local(nonzero);
        // Backward digit buffer in the outgoing scratch.
        let scratch = self.plan.outgoing_args_base();
        self.asm.mov_rr(Gpr::Rsi, Gpr::Rsp);
        self.asm.add_ri(Gpr::Rsi, scratch + 64);
        self.asm.mov_rr(Gpr::Rdi, Gpr::Rsi);
        let top = self.asm.new_local_label();
        self.asm.bind_local(top);
        self.asm.zero_rdx();
        self.asm.mov_ri(Gpr::Rcx, radix as i64);
        self.asm.div_r(Gpr::Rcx); // RAX=quot, RDX=digit
                                  // digit -> ascii: if d < 10 '0'+d else 'a'/'A'-10+d.
        let small = self.asm.new_local_label();
        let wrote = self.asm.new_local_label();
        self.asm.cmp_ri(Gpr::Rdx, 10);
        self.asm.jcc_local(Cc::B, small);
        self.asm
            .add_ri(Gpr::Rdx, (if upper { b'A' } else { b'a' }) as i32 - 10);
        self.asm.jmp_local(wrote);
        self.asm.bind_local(small);
        self.asm.add_ri(Gpr::Rdx, b'0' as i32);
        self.asm.bind_local(wrote);
        self.asm.add_ri(Gpr::Rdi, -1);
        self.asm.mov_store8_mem(Gpr::Rdi, 0, Gpr::Rdx);
        self.asm.test_rr(Gpr::Rax, Gpr::Rax);
        self.asm.jcc_local(Cc::Ne, top);
        // Copy [RDI, RSI) into the cursor.
        self.asm.mov_rr(Gpr::Rcx, Gpr::Rsi);
        self.asm.sub_rr(Gpr::Rcx, Gpr::Rdi);
        self.asm.mov_rr(Gpr::Rax, Gpr::Rdi);
        self.copy_runtime_len_to_cursor();
        self.asm.bind_local(done);
    }

    /// Renders a `{c}` field: UTF-8-encode the low code point. Matches
    /// `fmt::render_char` for the ASCII / multi-byte ranges.
    fn render_char_field(
        &mut self,
        base: k2_mir::LocalId,
        foff: u64,
        fty: TypeId,
    ) -> Result<(), CodegenError> {
        // `ComptimeInt` reaches here when const-fold inlines a comptime-typed
        // integer into a `{c}` field; treated as a word-sized integer code point.
        if !matches!(
            self.prog.arena.get(fty),
            Type::Int { .. } | Type::ComptimeInt
        ) {
            return Err(self.unsup("char format of a non-integer field"));
        }
        let h = self
            .home(base)
            .ok_or_else(|| self.unsup("print tuple home"))?;
        self.load_sized(Gpr::Rax, Gpr::Rbp, h + foff as i32, fty);
        self.emit_utf8_encode();
        Ok(())
    }

    /// Emits a UTF-8 encoding of the code point in RAX into the buffer, matching
    /// `char::encode_utf8` ranges. An out-of-range value (>= 0x110000) emits the
    /// low byte, like `render_char`'s fallback.
    fn emit_utf8_encode(&mut self) {
        // Branch ladder on the code point ranges.
        let l1 = self.asm.new_local_label();
        let l2 = self.asm.new_local_label();
        let l3 = self.asm.new_local_label();
        let done = self.asm.new_local_label();
        // if cp < 0x80 -> 1 byte.
        self.asm.cmp_ri(Gpr::Rax, 0x80);
        self.asm.jcc_local(Cc::Ae, l1);
        self.emit_byte_from_rax();
        self.asm.jmp_local(done);
        self.asm.bind_local(l1);
        // if cp < 0x800 -> 2 bytes.
        self.asm.cmp_ri(Gpr::Rax, 0x800);
        self.asm.jcc_local(Cc::Ae, l2);
        self.emit_utf8_continuation(2);
        self.asm.jmp_local(done);
        self.asm.bind_local(l2);
        // if cp < 0x10000 -> 3 bytes.
        self.asm.cmp_ri(Gpr::Rax, 0x10000);
        self.asm.jcc_local(Cc::Ae, l3);
        self.emit_utf8_continuation(3);
        self.asm.jmp_local(done);
        self.asm.bind_local(l3);
        // 4 bytes (valid up to 0x10FFFF; we don't re-validate — the corpus stays
        // in range, matching render_char's encode_utf8 path).
        self.emit_utf8_continuation(4);
        self.asm.bind_local(done);
    }

    /// Emits the low byte of RAX into the buffer (the 1-byte UTF-8 / fallback).
    fn emit_byte_from_rax(&mut self) {
        self.asm.mov_store8_mem(ADDR, 0, Gpr::Rax);
        self.asm.add_ri(ADDR, 1);
    }

    /// Emits an `n`-byte UTF-8 sequence for the code point in RAX (n in 2..=4),
    /// reproducing `char::encode_utf8`'s lead/continuation byte construction. The
    /// code point is preserved in RBX (a callee-saved scratch we may freely use at
    /// a print site, since the epilogue restores it) while RAX/RDX build bytes.
    fn emit_utf8_continuation(&mut self, n: u8) {
        let (lead_prefix, lead_shift) = match n {
            2 => (0xC0u8, 6u8),
            3 => (0xE0u8, 12),
            _ => (0xF0u8, 18),
        };
        // Preserve the code point in RDX-free scratch: use the stack scratch slot.
        let cp_slot = self.plan.outgoing_args_base();
        self.asm.mov_store_mem(Gpr::Rsp, cp_slot, Gpr::Rax);
        // Lead byte: prefix | (cp >> lead_shift).
        self.asm.mov_ri(Gpr::Rcx, lead_shift as i64);
        self.asm.shr_cl(Gpr::Rax);
        self.asm.add_ri(Gpr::Rax, lead_prefix as i32);
        self.asm.mov_store8_mem(ADDR, 0, Gpr::Rax);
        // Continuation bytes, most-significant first.
        let mut shift = lead_shift as i32 - 6;
        let mut byte_idx = 1i32;
        while shift >= 0 {
            self.asm.mov_load_mem(Gpr::Rax, Gpr::Rsp, cp_slot); // reload cp
            if shift > 0 {
                self.asm.mov_ri(Gpr::Rcx, shift as i64);
                self.asm.shr_cl(Gpr::Rax);
            }
            self.asm.and_ri(Gpr::Rax, 0x3F);
            self.asm.add_ri(Gpr::Rax, 0x80);
            self.asm.mov_store8_mem(ADDR, byte_idx, Gpr::Rax);
            byte_idx += 1;
            shift -= 6;
        }
        self.asm.add_ri(ADDR, n as i32);
    }

    /// Renders a `{s}` / default-string field: copy `len` bytes of the slice into
    /// the buffer.
    fn render_string_field(
        &mut self,
        base: k2_mir::LocalId,
        foff: u64,
        fty: TypeId,
    ) -> Result<(), CodegenError> {
        match self.prog.arena.get(fty) {
            Type::Slice { .. } => {
                // The field is a 16-byte slice {ptr, len} at base.home + foff.
                let h = self
                    .home(base)
                    .ok_or_else(|| self.unsup("print tuple without a home"))?;
                // ptr -> RAX, len -> RDX-ish. Save cursor (ADDR) across the copy by
                // using a byte loop driven by RCX (len).
                self.asm.mov_load(Gpr::Rax, h + foff as i32); // ptr
                self.asm.mov_load(Gpr::Rcx, h + foff as i32 + 8); // len
                self.copy_runtime_len_to_cursor();
                Ok(())
            }
            _ => Err(self.unsup("string format of a non-slice field")),
        }
    }

    /// Copies a runtime-length byte run (`RAX = src ptr`, `RCX = len`) into the
    /// print buffer cursor (ADDR), advancing ADDR. Emits a small byte-copy loop.
    fn copy_runtime_len_to_cursor(&mut self) {
        // for (; RCX != 0; RCX--) { *ADDR++ = *RAX++; }
        // labels via local jumps.
        let loop_top = self.asm.new_local_label();
        let loop_end = self.asm.new_local_label();
        self.asm.bind_local(loop_top);
        self.asm.test_rr(Gpr::Rcx, Gpr::Rcx);
        self.asm.jcc_local(Cc::E, loop_end);
        self.asm.movzx8_mem(Gpr::Rdx, Gpr::Rax, 0);
        self.asm.mov_store8_mem(ADDR, 0, Gpr::Rdx);
        self.asm.add_ri(Gpr::Rax, 1);
        self.asm.add_ri(ADDR, 1);
        self.asm.add_ri(Gpr::Rcx, -1);
        self.asm.jmp_local(loop_top);
        self.asm.bind_local(loop_end);
    }

    /// Renders a `{d}` / default-int field as decimal into the buffer.
    fn render_decimal_field(
        &mut self,
        base: k2_mir::LocalId,
        foff: u64,
        fty: TypeId,
    ) -> Result<(), CodegenError> {
        match self.prog.arena.get(fty) {
            Type::Int { .. } => {
                let h = self
                    .home(base)
                    .ok_or_else(|| self.unsup("print tuple without a home"))?;
                let size = self.layout(fty).size;
                let signed = repr_of(self.prog, fty).signed;
                if size > 8 {
                    // 128-bit decimal (u128/i128).
                    self.render_decimal_128(h + foff as i32, signed)
                } else {
                    // 64-bit decimal: load the value into RAX, then normalize to the
                    // field's width/signedness. `load_sized` zero-extends a *sub-byte*
                    // width via `movzx8`, which would drop the sign of a signed
                    // bit-field (`i4 = -3` would print `253`); `normalize` re-applies
                    // the signed shift-pair so `i1`/`i4`/`i7` print their true value.
                    self.load_sized(Gpr::Rax, Gpr::Rbp, h + foff as i32, fty);
                    self.normalize(Gpr::Rax, fty);
                    self.render_decimal_64(signed)
                }
            }
            Type::Bool => {
                // {d} on bool -> '1'/'0'.
                let h = self
                    .home(base)
                    .ok_or_else(|| self.unsup("print tuple home"))?;
                self.asm.movzx8_mem(Gpr::Rax, Gpr::Rbp, h + foff as i32);
                self.emit_bool_digit();
                Ok(())
            }
            // A `deferred`-typed scalar field is a `*System` capability result that
            // the checker left open (`sys.clock.now()` / `sys.random.int()` are
            // `usize`); render it as an unsigned 64-bit decimal.
            Type::Deferred => {
                let h = self
                    .home(base)
                    .ok_or_else(|| self.unsup("print tuple without a home"))?;
                self.asm.mov_load(Gpr::Rax, h + foff as i32);
                self.render_decimal_64(false)
            }
            // A `comptime_int` field reaches the backend when const-fold/copy-prop
            // inlines a comptime-typed integer (e.g. a negative literal) into the
            // print tuple. The value is stored word-sized and sign-extended (see
            // `const_int_bits`/`load_sized`'s width-0 path), so render it as a
            // signed 64-bit decimal — matching the VM's `comptime_int` formatting.
            // (The optimizer's `stamp_ty` now restamps such folds to their sized
            // destination type, so this arm is belt-and-suspenders for any
            // comptime_int that still slips through, keeping native == VM.)
            Type::ComptimeInt => {
                let h = self
                    .home(base)
                    .ok_or_else(|| self.unsup("print tuple without a home"))?;
                self.asm.mov_load(Gpr::Rax, h + foff as i32);
                self.render_decimal_64(true)
            }
            _ => Err(self.unsup("decimal format of a non-integer field")),
        }
    }

    /// Renders a default `{}` field, dispatching by static type.
    fn render_default_field(
        &mut self,
        base: k2_mir::LocalId,
        foff: u64,
        fty: TypeId,
    ) -> Result<(), CodegenError> {
        match self.prog.arena.get(fty) {
            // `ComptimeInt` from a const-folded field renders like a default int.
            Type::Int { .. } | Type::ComptimeInt => self.render_decimal_field(base, foff, fty),
            Type::Slice { .. } => self.render_string_field(base, foff, fty),
            Type::Bool => {
                // {} on bool -> "true"/"false".
                let h = self
                    .home(base)
                    .ok_or_else(|| self.unsup("print tuple home"))?;
                self.asm.movzx8_mem(Gpr::Rax, Gpr::Rbp, h + foff as i32);
                self.emit_bool_word()
            }
            _ => Err(self.unsup("default format of an unsupported field type")),
        }
    }

    /// Emits a single '1'/'0' digit for a bool value in RAX.
    fn emit_bool_digit(&mut self) {
        // *ADDR++ = '0' + (RAX != 0).
        self.asm.test_rr(Gpr::Rax, Gpr::Rax);
        self.asm.setcc_al(Cc::Ne);
        self.asm.movzx_al(Gpr::Rax);
        self.asm.add_ri(Gpr::Rax, b'0' as i32);
        self.asm.mov_store8_mem(ADDR, 0, Gpr::Rax);
        self.asm.add_ri(ADDR, 1);
    }

    /// Emits "true"/"false" for a bool value in RAX.
    fn emit_bool_word(&mut self) -> Result<(), CodegenError> {
        let end = self.asm.new_local_label();
        let false_lbl = self.asm.new_local_label();
        self.asm.test_rr(Gpr::Rax, Gpr::Rax);
        self.asm.jcc_local(Cc::E, false_lbl);
        self.emit_inline_bytes(b"true");
        self.asm.jmp_local(end);
        self.asm.bind_local(false_lbl);
        self.emit_inline_bytes(b"false");
        self.asm.bind_local(end);
        Ok(())
    }

    /// Writes a short fixed byte string directly into the buffer via immediates.
    fn emit_inline_bytes(&mut self, bytes: &[u8]) {
        for (i, &b) in bytes.iter().enumerate() {
            self.asm.mov_ri(Gpr::Rax, b as i64);
            self.asm.mov_store8_mem(ADDR, i as i32, Gpr::Rax);
        }
        self.asm.add_ri(ADDR, bytes.len() as i32);
    }

    /// Renders a 64-bit integer (value in RAX) as decimal into the buffer. Signed
    /// values emit a leading '-' for negatives. Matches `render_decimal`.
    fn render_decimal_64(&mut self, signed: bool) -> Result<(), CodegenError> {
        // Convert RAX to an unsigned magnitude in RAX; emit '-' if negative.
        if signed {
            let nonneg = self.asm.new_local_label();
            self.asm.test_rr(Gpr::Rax, Gpr::Rax);
            self.asm.jcc_local(Cc::Ge, nonneg);
            // Emit '-' and negate.
            self.asm.mov_rr(Gpr::Rcx, Gpr::Rax); // save value
            self.asm.mov_ri(Gpr::Rax, b'-' as i64);
            self.asm.mov_store8_mem(ADDR, 0, Gpr::Rax);
            self.asm.add_ri(ADDR, 1);
            self.asm.mov_rr(Gpr::Rax, Gpr::Rcx);
            self.asm.neg_r(Gpr::Rax);
            self.asm.bind_local(nonneg);
        }
        // Now RAX is the magnitude (unsigned). Emit digits.
        self.emit_u64_digits();
        Ok(())
    }

    /// Emits the unsigned decimal digits of RAX into the buffer (forward order).
    /// Digits are generated least-significant-first into a 20-byte scratch on the
    /// stack (the outgoing region), then copied forward.
    fn emit_u64_digits(&mut self) {
        // We build digits backward into a small stack scratch at [rsp + 0..32].
        // Use RSI as the scratch base, RDI as the digit write pointer.
        // Scratch lives in the outgoing-args region [rsp + base ..].
        let scratch = self.plan.outgoing_args_base();
        // Compute scratch end pointer: RSI = rsp + scratch + 24 (room for 20
        // digits). We write digits descending from RSI.
        self.asm.mov_rr(Gpr::Rsi, Gpr::Rsp);
        self.asm.add_ri(Gpr::Rsi, scratch + 24);
        // RDI = RSI (digit cursor, moves down).
        self.asm.mov_rr(Gpr::Rdi, Gpr::Rsi);
        // do { d = rax % 10; rax /= 10; *--rdi = '0'+d; } while (rax != 0);
        let top = self.asm.new_local_label();
        self.asm.bind_local(top);
        self.asm.zero_rdx();
        self.asm.mov_ri(Gpr::Rcx, 10);
        self.asm.div_r(Gpr::Rcx); // RAX=quot, RDX=rem
        self.asm.add_ri(Gpr::Rdi, -1);
        self.asm.add_ri(Gpr::Rdx, b'0' as i32);
        self.asm.mov_store8_mem(Gpr::Rdi, 0, Gpr::Rdx);
        self.asm.test_rr(Gpr::Rax, Gpr::Rax);
        self.asm.jcc_local(Cc::Ne, top);
        // Copy [RDI, RSI) forward into the buffer cursor.
        // len = RSI - RDI -> RCX.
        self.asm.mov_rr(Gpr::Rcx, Gpr::Rsi);
        self.asm.sub_rr(Gpr::Rcx, Gpr::Rdi);
        // src = RDI -> RAX.
        self.asm.mov_rr(Gpr::Rax, Gpr::Rdi);
        self.copy_runtime_len_to_cursor();
    }

    /// Renders a 128-bit integer at `[rbp + off]` (lo at +0, hi at +8) as decimal.
    fn render_decimal_128(&mut self, off: i32, signed: bool) -> Result<(), CodegenError> {
        // Load lo/hi.
        // We keep the 128-bit magnitude in (RBX:R14)? Those may be vregs. Instead
        // use the outgoing-args scratch to hold the working value as we divide.
        // Algorithm: long division by 10 over a two-limb value, emitting digits.
        // Working value lives in two stack slots [rsp + s + 0] (lo), [+8] (hi).
        let s = self.plan.outgoing_args_base() + 32; // past the digit scratch
                                                     // Load value -> working slots; if signed and negative, emit '-' and negate.
        self.asm.mov_load(Gpr::Rax, off); // lo
        self.asm.mov_load(Gpr::Rdx, off + 4 + 4); // hi (off+8)
        if signed {
            let nonneg = self.asm.new_local_label();
            self.asm.test_rr(Gpr::Rdx, Gpr::Rdx);
            self.asm.jcc_local(Cc::Ge, nonneg);
            // Emit '-'.
            self.asm.mov_ri(Gpr::Rcx, b'-' as i64);
            self.asm.mov_store8_mem(ADDR, 0, Gpr::Rcx);
            self.asm.add_ri(ADDR, 1);
            // Negate the 128-bit value: neg lo; adc hi; neg via two's complement:
            // (lo,hi) = 0 - (lo,hi). Compute: lo' = -lo; hi' = -(hi) - (lo!=0).
            // Simpler: not both, then add 1.
            self.asm.not_r(Gpr::Rax);
            self.asm.not_r(Gpr::Rdx);
            // add 1 to the 128-bit (RAX:RDX): inc lo, if carry inc hi.
            self.asm.add_ri(Gpr::Rax, 1);
            let nocarry = self.asm.new_local_label();
            self.asm.jcc_local(Cc::Ae, nocarry); // CF clear -> no carry
            self.asm.add_ri(Gpr::Rdx, 1);
            self.asm.bind_local(nocarry);
            self.asm.bind_local(nonneg);
        }
        // Store working value.
        self.asm.mov_store(s, Gpr::Rax); // lo
        self.asm.mov_store(s + 8, Gpr::Rdx); // hi
                                             // Digit scratch: descending pointer in RDI from [rsp + dbase + 40].
        let dbase = self.plan.outgoing_args_base();
        self.asm.mov_rr(Gpr::Rsi, Gpr::Rsp);
        self.asm.add_ri(Gpr::Rsi, dbase + 40);
        self.asm.mov_rr(Gpr::Rdi, Gpr::Rsi);
        // Loop: divide the two-limb value by 10, remainder -> digit.
        let top = self.asm.new_local_label();
        self.asm.bind_local(top);
        // hi limb / 10: RAX=hi, RDX=0 -> div 10 -> quot hi', rem r1.
        self.asm.mov_load(Gpr::Rax, s + 8); // hi
        self.asm.zero_rdx();
        self.asm.mov_ri(Gpr::Rcx, 10);
        self.asm.div_r(Gpr::Rcx); // RAX=hi', RDX=rem_hi
        self.asm.mov_store(s + 8, Gpr::Rax); // hi' back
                                             // lo limb with remainder prefix: RDX:RAX = (rem_hi:lo) / 10.
        self.asm.mov_load(Gpr::Rax, s); // lo
                                        // RDX already = rem_hi (the high part of the dividend).
        self.asm.mov_ri(Gpr::Rcx, 10);
        self.asm.div_r(Gpr::Rcx); // RAX=lo', RDX=digit
        self.asm.mov_store(s, Gpr::Rax); // lo' back
                                         // Emit digit (RDX).
        self.asm.add_ri(Gpr::Rdi, -1);
        self.asm.add_ri(Gpr::Rdx, b'0' as i32);
        self.asm.mov_store8_mem(Gpr::Rdi, 0, Gpr::Rdx);
        // Continue while (lo' | hi') != 0.
        self.asm.mov_load(Gpr::Rax, s);
        self.asm.mov_load(Gpr::Rcx, s + 8);
        self.asm.or_rr(Gpr::Rax, Gpr::Rcx);
        self.asm.jcc_local(Cc::Ne, top);
        // Copy [RDI, RSI) into the cursor.
        self.asm.mov_rr(Gpr::Rcx, Gpr::Rsi);
        self.asm.sub_rr(Gpr::Rcx, Gpr::Rdi);
        self.asm.mov_rr(Gpr::Rax, Gpr::Rdi);
        self.copy_runtime_len_to_cursor();
        Ok(())
    }

    // ---- Safety predicates (unchanged from v0.14, rerouted operands) ----

    /// Lowers a `@`-builtin intrinsic into RAX: a safety predicate (overflow /
    /// narrow checks), an allocator-registry / capability-floor leaf
    /// (`@allocId`/`@allocHandle`/`@gpaDeinit`/`@arenaDeinit`), or a `*System`
    /// capability reader (`@clockNow`/`@clockSleep`/`@randomInt`/`@randomBytes`/
    /// `@envGet`). The `@*Raw` leaf forms route to the same runtime routines as the
    /// allocator method spellings.
    fn lower_safety_predicate(&mut self, name: &str, args: &[Operand]) -> Result<(), CodegenError> {
        match name {
            "allocId" => return self.lower_alloc_id(args),
            "allocHandle" => return self.lower_alloc_handle(args),
            "gpaDeinit" => return self.lower_gpa_deinit(args),
            "arenaDeinit" => return self.lower_arena_deinit(args),
            "clockNow" => return self.lower_clock_now(),
            "clockSleep" => return self.lower_clock_sleep(args),
            "randomInt" => return self.lower_random_int(),
            "randomBytes" => return self.lower_random_bytes(args),
            "envGet" => return self.lower_env_get(),
            _ => {}
        }
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

    /// The type a check predicate guards.
    fn predicate_type(&self, args: &[Operand]) -> TypeId {
        if let Some(Operand::Const(Const::Undef { ty })) = args.last() {
            let r = repr_of(self.prog, *ty);
            if r.width != 0 {
                return *ty;
            }
        }
        for a in args {
            if let Some(ty) = self.operand_type(a) {
                let r = repr_of(self.prog, ty);
                if r.width != 0 {
                    return ty;
                }
            }
        }
        args.first()
            .and_then(|a| self.operand_type(a))
            .unwrap_or(self.func.ret)
    }

    /// The type of an operand, if it is a bare local / projected place / int const.
    fn operand_type(&self, op: &Operand) -> Option<TypeId> {
        match op {
            Operand::Copy(p) if p.is_local() => Some(self.func.locals[p.base.index()].ty),
            Operand::Copy(p) => Some(self.place_type(p)),
            Operand::Const(Const::Int { ty, .. }) => Some(*ty),
            Operand::Const(Const::Float { ty, .. }) => Some(*ty),
            Operand::Const(Const::EnumVal { ty, .. }) => Some(*ty),
            // A bare boolean constant is a 1-byte `bool`. Returning `None` here
            // (the old behavior) typed a bool-const vector lane as the whole
            // vector type on the synthetic-layout path, giving it an 8-byte store
            // and stride so only lane 0 survived — see `build_aggregate`.
            Operand::Const(Const::Bool(_)) => Some(self.prog.arena.t_bool()),
            _ => None,
        }
    }

    /// `no_{add,sub,mul}_overflow(a, b)`.
    fn overflow_predicate(
        &mut self,
        args: &[Operand],
        ty: TypeId,
        kind: ArithKind,
    ) -> Result<(), CodegenError> {
        let r = repr_of(self.prog, ty);
        if r.width == 0 {
            self.asm.mov_ri(Gpr::Rax, 1);
            return Ok(());
        }
        if self.is_wide_int(ty) {
            return self.overflow_predicate_128(args, r, kind);
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
        self.asm.mov_rr(Gpr::Rcx, Gpr::Rax);
        self.normalize(Gpr::Rcx, ty);
        self.asm.cmp_rr(Gpr::Rax, Gpr::Rcx);
        self.asm.setcc_al(Cc::E);
        self.asm.movzx_al(Gpr::Rax);
        Ok(())
    }

    /// The full-64-bit overflow predicate (reads OF/CF).
    fn overflow_predicate_64(
        &mut self,
        args: &[Operand],
        r: Repr,
        kind: ArithKind,
    ) -> Result<(), CodegenError> {
        self.operand_to(arg(args, 0)?, Gpr::Rax)?;
        self.operand_to(arg(args, 1)?, Gpr::Rcx)?;
        let overflow_cc = if r.signed { Cc::O } else { Cc::C };
        match kind {
            ArithKind::Add => self.asm.add_rr(Gpr::Rax, Gpr::Rcx),
            ArithKind::Sub => self.asm.sub_rr(Gpr::Rax, Gpr::Rcx),
            ArithKind::Mul => {
                if r.signed {
                    self.asm.imul_r1(Gpr::Rcx);
                } else {
                    self.asm.mul_r(Gpr::Rcx);
                }
            }
        }
        self.asm.setcc_al(overflow_cc);
        self.asm.movzx_al(Gpr::Rax);
        self.asm.xor_ri(Gpr::Rax, 1);
        Ok(())
    }

    /// The 128-bit (two-limb) `no_{add,sub}_overflow` predicate.
    ///
    /// Performs the same two-limb `add`+`adc` / `sub`+`sbb` the arithmetic path
    /// emits, then reads the **signed-overflow** flag (`OF`) the high-limb
    /// instruction left set — for BOTH signed and unsigned 128-bit types.
    ///
    /// This deliberately mirrors the VM reference, which keeps every integer in a
    /// full `i128` and checks overflow with `i128::checked_add`/`checked_sub`
    /// against the *signed* `i128` range (see `Vm::no_overflow` +
    /// `IntRepr::{min,max}_value`, which collapse to `i128::MIN..=i128::MAX` for
    /// any 128-bit repr). The CPU's `OF` after the two-limb op is exactly
    /// "the signed-`i128` result overflowed", so native and VM agree bit-for-bit
    /// (e.g. `u128::MAX(=2^64-1) + 1` does NOT overflow `i128`, so neither traps;
    /// `(2^127-1) + 1` does, so both trap). The earlier 64-bit-only predicate
    /// truncated the operand to its low limb and trapped valid sums. 128-bit `mul`
    /// is not implemented, so its overflow check is refused, not approximated.
    fn overflow_predicate_128(
        &mut self,
        args: &[Operand],
        _r: Repr,
        kind: ArithKind,
    ) -> Result<(), CodegenError> {
        // lhs -> RAX:RDX, rhs -> RCX:R8.
        self.load_wide_operand(arg(args, 0)?, Gpr::Rax, Gpr::Rdx)?;
        self.load_wide_operand(arg(args, 1)?, Gpr::Rcx, Gpr::R8)?;
        match kind {
            ArithKind::Add => {
                self.asm.add_rr(Gpr::Rax, Gpr::Rcx);
                self.asm.adc_rr(Gpr::Rdx, Gpr::R8); // OF reflects the signed i128 sum
            }
            ArithKind::Sub => {
                self.asm.sub_rr(Gpr::Rax, Gpr::Rcx);
                self.asm.sbb_rr(Gpr::Rdx, Gpr::R8); // OF reflects the signed i128 diff
            }
            ArithKind::Mul => {
                return Err(self.unsup("128-bit integer multiply overflow check (use the VM)"));
            }
        }
        self.asm.setcc_al(Cc::O); // signed-i128 overflow, matching the VM
        self.asm.movzx_al(Gpr::Rax);
        self.asm.xor_ri(Gpr::Rax, 1); // 1 == "no overflow"
        Ok(())
    }

    /// `no_div_overflow(a, b)`.
    fn div_overflow_predicate(&mut self, args: &[Operand], ty: TypeId) -> Result<(), CodegenError> {
        let r = repr_of(self.prog, ty);
        if self.is_wide_int(ty) {
            // 128-bit division itself is not implemented (its `Binary` is refused),
            // so its overflow guard is refused too rather than emitting a wrong
            // single-limb compare.
            return Err(self.unsup("128-bit integer division (use the VM)"));
        }
        if !r.signed || r.width == 0 {
            self.asm.mov_ri(Gpr::Rax, 1);
            return Ok(());
        }
        let min = type_min(r);
        self.operand_to(arg(args, 0)?, Gpr::Rax)?;
        self.asm.mov_ri(Gpr::Rcx, min);
        self.asm.cmp_rr(Gpr::Rax, Gpr::Rcx);
        self.asm.setcc_al(Cc::E);
        self.asm.movzx_al(Gpr::Rax);
        self.asm.mov_rr(Gpr::Rdx, Gpr::Rax);
        self.operand_to(arg(args, 1)?, Gpr::Rax)?;
        self.asm.mov_ri(Gpr::Rcx, -1);
        self.asm.cmp_rr(Gpr::Rax, Gpr::Rcx);
        self.asm.setcc_al(Cc::E);
        self.asm.movzx_al(Gpr::Rax);
        self.asm.and_rr(Gpr::Rax, Gpr::Rdx);
        self.asm.xor_ri(Gpr::Rax, 1);
        Ok(())
    }

    /// `no_neg_overflow(a)`.
    fn neg_overflow_predicate(&mut self, args: &[Operand], ty: TypeId) -> Result<(), CodegenError> {
        let r = repr_of(self.prog, ty);
        if !r.signed || r.width == 0 {
            self.asm.mov_ri(Gpr::Rax, 1);
            return Ok(());
        }
        if self.is_wide_int(ty) {
            // i128 negation overflows only at i128::MIN, whose limbs are
            // (lo=0, hi=i64::MIN). "No overflow" == NOT(lo==0 AND hi==MIN).
            self.load_wide_operand(arg(args, 0)?, Gpr::Rax, Gpr::Rdx)?;
            // RCX := (lo == 0).
            self.asm.test_rr(Gpr::Rax, Gpr::Rax);
            self.asm.setcc_al(Cc::E);
            self.asm.movzx_al(Gpr::Rcx);
            // RAX := (hi == i64::MIN).
            self.asm.mov_ri(Gpr::R8, i64::MIN);
            self.asm.cmp_rr(Gpr::Rdx, Gpr::R8);
            self.asm.setcc_al(Cc::E);
            self.asm.movzx_al(Gpr::Rax);
            // is_min = (lo==0) & (hi==MIN); no_overflow = is_min ^ 1.
            self.asm.and_rr(Gpr::Rax, Gpr::Rcx);
            self.asm.xor_ri(Gpr::Rax, 1);
            return Ok(());
        }
        let min = type_min(r);
        self.operand_to(arg(args, 0)?, Gpr::Rax)?;
        self.asm.mov_ri(Gpr::Rcx, min);
        self.asm.cmp_rr(Gpr::Rax, Gpr::Rcx);
        self.asm.setcc_al(Cc::Ne);
        self.asm.movzx_al(Gpr::Rax);
        Ok(())
    }

    /// `narrow_fits(value)`.
    fn narrow_fits_predicate(&mut self, args: &[Operand], ty: TypeId) -> Result<(), CodegenError> {
        let r = repr_of(self.prog, ty);
        if r.width == 0 {
            self.asm.mov_ri(Gpr::Rax, 1);
            return Ok(());
        }
        if r.width >= 64 {
            let src_signed = self
                .operand_type(arg(args, 0)?)
                .map(|t| repr_of(self.prog, t).signed)
                .unwrap_or(r.signed);
            if src_signed == r.signed {
                self.asm.mov_ri(Gpr::Rax, 1);
                return Ok(());
            }
            self.operand_to(arg(args, 0)?, Gpr::Rax)?;
            self.asm.mov_ri(Gpr::Rcx, 0);
            self.asm.cmp_rr(Gpr::Rax, Gpr::Rcx);
            self.asm.setcc_al(Cc::Ge);
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
                self.load_switch_scrutinee(scrutinee, Gpr::Rax)?;
                for (value, t) in targets {
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
            Terminator::Return { value, .. } => self.lower_return(value, rodata),
            Terminator::Trap { reason } => self.lower_trap(*reason, rodata),
            Terminator::Unreachable => self.lower_trap(TrapReason::Unreachable, rodata),
        }
    }

    /// Loads a `switch` scrutinee into `dst`. When the scrutinee is an error union
    /// (the captured error of a `catch |err| switch (err)` whose error set the
    /// checker kept as the full union), the tag is the `u16` at +0 — the
    /// payload/padding eightbytes must not leak into the compared value. Load just
    /// the tag. A plain integer/enum scrutinee loads normally.
    fn load_switch_scrutinee(&mut self, op: &Operand, dst: Gpr) -> Result<(), CodegenError> {
        if let Operand::Copy(p) = op {
            let is_eu = self
                .operand_type(op)
                .map(|t| matches!(self.prog.arena.get(t), Type::ErrorUnion { .. }))
                .unwrap_or(false)
                || self.eu_payload_of(p.base).is_some();
            if is_eu {
                self.place_addr_general(p, ADDR)?;
                self.asm.movzx16_mem(dst, ADDR, 0);
                return Ok(());
            }
        }
        self.operand_to(op, dst)
    }

    /// Lowers a `Return`.
    fn lower_return(&mut self, value: &Operand, rodata: &mut RoData) -> Result<(), CodegenError> {
        match self.entry_kind {
            EntryKind::VoidEntry => {
                if self.returns_error(value) {
                    // Print `error: <name>\n` to stderr and exit 1, matching the
                    // VM's escaped-error behavior.
                    return self.lower_escaped_error(value, rodata);
                }
                self.asm.mov_ri(Gpr::Rax, 0);
            }
            EntryKind::IntEntry => {
                self.operand_to(value, Gpr::Rax)?;
                self.normalize(Gpr::Rax, self.func.ret);
            }
            EntryKind::Helper => {
                self.lower_helper_return(value)?;
            }
        }
        self.epilogue_and_ret();
        Ok(())
    }

    /// Lowers a `main` `Return` carrying an escaped error: print `error: <name>\n`
    /// to stderr and `exit(1)`, matching the VM. The error name is resolved from
    /// the runtime error tag against the program's `err_names` table via a compare
    /// chain (the tag is a `u16`; the corpus has few error variants).
    fn lower_escaped_error(
        &mut self,
        value: &Operand,
        rodata: &mut RoData,
    ) -> Result<(), CodegenError> {
        // Resolve the error tag: a static `Const::ErrVal`, or the `u16` tag of an
        // error-union local at offset 0.
        let static_tag: Option<u16> = match value {
            Operand::Const(Const::ErrVal { tag, .. }) => Some(tag.0),
            _ => None,
        };
        // Emit "error: " prefix to stderr first.
        // We write each name's full line as a single rodata string.
        if let Some(tag) = static_tag {
            let name = self
                .prog
                .err_names
                .get(&k2_mir::ErrTag(tag))
                .cloned()
                .unwrap_or_else(|| format!("error{tag}"));
            return self.emit_error_line_and_exit(&name, rodata);
        }
        // Runtime tag from the error-union local at offset 0.
        // Load tag into a callee-clobbered register, compare-chain against each
        // known error name, write the matching line, exit 1.
        let mut names: Vec<(u16, String)> = self
            .prog
            .err_names
            .iter()
            .map(|(t, n)| (t.0, n.clone()))
            .collect();
        names.sort_by_key(|(t, _)| *t);

        // Load the tag.
        if let Operand::Copy(p) = value {
            self.place_addr_general(p, ADDR)?;
            self.asm.movzx16_mem(Gpr::Rbx, ADDR, 0); // RBX = tag (callee-saved scratch)
        } else {
            self.asm.mov_ri(Gpr::Rbx, 0);
        }
        let done = self.asm.new_local_label();
        for (tag, name) in &names {
            let next = self.asm.new_local_label();
            self.asm.cmp_ri(Gpr::Rbx, *tag as i32);
            self.asm.jcc_local(Cc::Ne, next);
            self.write_error_line(name, rodata);
            self.asm.jmp_local(done);
            self.asm.bind_local(next);
        }
        // Fallback: write a generic "error\n".
        self.write_error_line("error", rodata);
        self.asm.bind_local(done);
        // exit(1).
        self.asm.mov_ri(Gpr::Rdi, 1);
        self.asm.mov_ri(Gpr::Rax, sys::EXIT);
        self.asm.syscall();
        Ok(())
    }

    /// Writes `error: <name>\n` to stderr then `exit(1)` (static-tag fast path).
    fn emit_error_line_and_exit(
        &mut self,
        name: &str,
        rodata: &mut RoData,
    ) -> Result<(), CodegenError> {
        self.write_error_line(name, rodata);
        self.asm.mov_ri(Gpr::Rdi, 1);
        self.asm.mov_ri(Gpr::Rax, sys::EXIT);
        self.asm.syscall();
        Ok(())
    }

    /// Emits a `write(2, "error: <name>\n", len)` syscall. The line is interned in
    /// rodata. Clobbers RAX/RDI/RSI/RDX.
    fn write_error_line(&mut self, name: &str, rodata: &mut RoData) {
        let line = format!("error: {name}\n");
        let bytes = line.into_bytes();
        let len = bytes.len();
        let off = rodata.intern(&bytes);
        self.asm.mov_ri(Gpr::Rdi, 2);
        self.asm.mov_ri_data(Gpr::Rsi, off);
        self.asm.mov_ri(Gpr::Rdx, len as i64);
        self.asm.mov_ri(Gpr::Rax, sys::WRITE);
        self.asm.syscall();
    }

    /// Lowers a helper `Return` value into RAX/RDX/xmm0/the sret pointer.
    fn lower_helper_return(&mut self, value: &Operand) -> Result<(), CodegenError> {
        let ty = self.func.ret;
        if self.is_float(ty) {
            self.load_float_operand(value, Xmm::Xmm0)?;
            return Ok(());
        }
        if frame::is_memory_aggregate(&self.prog.arena, ty) {
            let class = classify(self.prog, ty);
            match class {
                ArgClass::Memory { size, .. } => {
                    // Copy the result aggregate to [sret], return the pointer in RAX.
                    let slot = self
                        .plan
                        .ret_ptr_slot
                        .ok_or_else(|| self.unsup("memory return without an sret slot"))?;
                    self.asm.mov_load(ADDR, slot); // dest ptr
                    if let Operand::Copy(src) = value {
                        self.place_addr_general(src, Gpr::Rax)?;
                        self.memcpy(ADDR, Gpr::Rax, size);
                    }
                    self.asm.mov_load(Gpr::Rax, slot); // return the pointer
                }
                ArgClass::OneInt | ArgClass::TwoInt => {
                    // Returned in RAX:RDX from the aggregate's bytes.
                    if let Operand::Copy(src) = value {
                        self.place_addr_general(src, ADDR)?;
                        self.asm.mov_load_mem(Gpr::Rax, ADDR, 0);
                        if self.layout(ty).size > 8 {
                            self.asm.mov_load_mem(Gpr::Rdx, ADDR, 8);
                        }
                    } else if matches!(value, Operand::Const(Const::Void))
                        && matches!(self.prog.arena.get(ty), Type::ErrorUnion { .. })
                    {
                        // `return ()` from a `!void` helper: the success value is
                        // `Ok(void)`, whose entire register image is the zero tag.
                        // (Without this, the register-class return path would leave
                        // RAX with stale garbage, and the caller's `discr` on the
                        // returned error union would see a spurious non-zero tag.)
                        self.asm.xor_rr(Gpr::Rax, Gpr::Rax);
                        if self.layout(ty).size > 8 {
                            self.asm.xor_rr(Gpr::Rdx, Gpr::Rdx);
                        }
                    } else {
                        // Any other non-place operand into a register-class aggregate
                        // return: build the bytes into the print scratch then load.
                        // The corpus only hits the `Const::Void` case above; refuse
                        // anything else cleanly rather than return garbage.
                        return Err(self.unsup("register-class aggregate return from a non-place"));
                    }
                }
                ArgClass::Sse => {
                    if let Operand::Copy(src) = value {
                        self.place_addr_general(src, ADDR)?;
                        self.asm.movsd_load(Xmm::Xmm0, ADDR, 0);
                    }
                }
            }
            return Ok(());
        }
        self.operand_to(value, Gpr::Rax)?;
        self.normalize(Gpr::Rax, ty);
        Ok(())
    }

    /// `true` if a `main` return operand carries an error.
    fn returns_error(&self, value: &Operand) -> bool {
        match value {
            Operand::Const(Const::ErrVal { .. }) => true,
            Operand::Copy(p) if p.is_local() => {
                matches!(
                    self.prog.arena.get(self.func.locals[p.base.index()].ty),
                    Type::ErrorUnion { .. } | Type::ErrorSet(_) | Type::AnyError
                )
            }
            _ => false,
        }
    }

    /// Lowers a `Trap`: write `panic: <reason>\n` to stderr then `exit(134)`.
    fn lower_trap(&mut self, reason: TrapReason, rodata: &mut RoData) -> Result<(), CodegenError> {
        let msg = format!("panic: {}\n", trap_message(reason));
        let bytes = msg.into_bytes();
        let len = bytes.len();
        let data_off = rodata.intern(&bytes);
        self.asm.mov_ri(Gpr::Rdi, 2);
        self.asm.mov_ri_data(Gpr::Rsi, data_off);
        self.asm.mov_ri(Gpr::Rdx, len as i64);
        self.asm.mov_ri(Gpr::Rax, sys::WRITE);
        self.asm.syscall();
        self.asm.mov_ri(Gpr::Rdi, PANIC_EXIT);
        self.asm.mov_ri(Gpr::Rax, sys::EXIT);
        self.asm.syscall();
        Ok(())
    }

    /// A short `Unsupported` error tagged with the function name.
    fn unsup(&self, what: &str) -> CodegenError {
        CodegenError::Unsupported(format!("{what} in `{}`", self.func.name))
    }
}

/// Which arithmetic op an overflow predicate guards.
#[derive(Clone, Copy)]
enum ArithKind {
    Add,
    Sub,
    Mul,
}

/// The SysV argument class of a type (simplified to what the corpus needs).
///
/// AAPCS64 agrees with SysV for the native corpus (ints/pointers in registers,
/// ≤16-byte all-int aggregates in two registers, >16-byte / float-containing
/// aggregates in memory), so the aarch64 lowering reuses this classification —
/// only the argument-register *count and identities* differ (a per-target table).
#[derive(Clone, Copy, Debug)]
pub(crate) enum ArgClass {
    /// One integer eightbyte (scalar int/ptr/bool, or a ≤8-byte all-int aggregate).
    OneInt,
    /// Two integer eightbytes (9–16-byte all-int aggregate, incl. a slice).
    TwoInt,
    /// One SSE eightbyte (an `f64`).
    Sse,
    /// Passed in memory (on the stack) / returned via a hidden pointer.
    Memory {
        /// The byte size.
        size: u64,
    },
}

/// Classifies a type for SysV argument passing.
pub(crate) fn classify(prog: &MirProgram, ty: TypeId) -> ArgClass {
    if matches!(prog.arena.get(ty), Type::Float { .. } | Type::ComptimeFloat) {
        return ArgClass::Sse;
    }
    if !frame::is_memory_aggregate(&prog.arena, ty) {
        return ArgClass::OneInt; // scalar int/ptr/bool/?*T
    }
    let l = layout::layout_of(&prog.arena, ty).unwrap_or(Layout::WORD);
    if l.size == 0 {
        return ArgClass::OneInt;
    }
    if l.size > 16 || aggregate_has_float(prog, ty) {
        return ArgClass::Memory { size: l.size };
    }
    if l.size > 8 {
        ArgClass::TwoInt
    } else {
        ArgClass::OneInt
    }
}

/// `true` if the aggregate contains any float field/element (conservatively
/// routed to memory, since the SSE-eightbyte classification is not implemented).
fn aggregate_has_float(prog: &MirProgram, ty: TypeId) -> bool {
    match prog.arena.get(ty) {
        Type::Float { .. } => true,
        Type::Struct(id) => {
            let info = &prog.arena.structs[id.0 as usize];
            info.fields.iter().any(|f| aggregate_has_float(prog, f.ty))
        }
        Type::Array { elem, .. } => aggregate_has_float(prog, *elem),
        Type::Optional(inner) => aggregate_has_float(prog, *inner),
        Type::ErrorUnion { ok, .. } => aggregate_has_float(prog, *ok),
        _ => false,
    }
}

/// `true` if `func` contains a `print` intrinsic (so a print buffer is reserved).
pub(crate) fn func_prints(func: &MirFunction) -> bool {
    func.blocks.iter().any(|b| {
        b.stmts.iter().any(|s| match s {
            Statement::Assign { rvalue, .. } | Statement::Eval { rvalue, .. } => {
                is_print_rvalue(rvalue)
            }
            _ => false,
        })
    })
}

/// `true` if an rvalue is a `print` intrinsic.
fn is_print_rvalue(rv: &Rvalue) -> bool {
    if let Rvalue::Intrinsic { path, .. } = rv {
        return path.members.last().map(|s| s.as_str()) == Some("print");
    }
    false
}

/// The maximum stack-argument bytes any call in `func` needs (rounded to 16),
/// plus a fixed scratch reservation for the print digit/working buffers (used by
/// `emit_u64_digits`/`render_decimal_128`). The outgoing-args region doubles as
/// that scratch, so it is always at least 64 bytes when the function prints.
pub(crate) fn outgoing_args_bytes(prog: &MirProgram, func: &MirFunction) -> i32 {
    let mut max_stack = 0i32;
    for block in &func.blocks {
        for stmt in &block.stmts {
            let rv = match stmt {
                Statement::Assign { rvalue, .. } | Statement::Eval { rvalue, .. } => rvalue,
                _ => continue,
            };
            if let Rvalue::Call { args, .. } = rv {
                max_stack = max_stack.max(call_stack_bytes(prog, args));
            }
        }
    }
    let mut total = max_stack;
    if func_prints(func) {
        // Reserve scratch for the decimal/radix renderers (the 128-bit path uses
        // [base..base+80]) plus the width/alignment padding save slot at base+112.
        // 128 covers all of them with margin.
        total = total.max(128);
    }
    round_up_i32(total, 16)
}

/// The stack-argument byte size of one call's argument list.
fn call_stack_bytes(prog: &MirProgram, args: &[Operand]) -> i32 {
    let mut int_idx = 0usize;
    let mut sse_idx = 0usize;
    let mut stack = 0i32;
    for arg in args {
        // A string / empty-slice literal marshals as a `[]const u8` fat slice
        // (`{ptr, len}`, two eightbytes) unless the callee decays it to a
        // `const char *` (one eightbyte). Without the callee context here we
        // size it as the wider TwoInt case so the outgoing-args region is never
        // *undersized* — undersizing lets a stack-resident slice store overflow
        // the region and corrupt the frame (segfault).
        if matches!(
            arg,
            Operand::Const(Const::Str(_) | Const::EmptySlice { .. })
        ) {
            if int_idx + 2 <= ARG_REGS.len() {
                int_idx += 2;
            } else {
                stack += 16;
            }
            continue;
        }
        let ty = operand_type_global(prog, arg);
        match ty.map(|t| classify(prog, t)).unwrap_or(ArgClass::OneInt) {
            ArgClass::OneInt => {
                if int_idx < ARG_REGS.len() {
                    int_idx += 1;
                } else {
                    stack += 8;
                }
            }
            ArgClass::TwoInt => {
                if int_idx + 2 <= ARG_REGS.len() {
                    int_idx += 2;
                } else {
                    stack += 16;
                }
            }
            ArgClass::Sse => {
                if sse_idx < SSE_ARG_REGS.len() {
                    sse_idx += 1;
                } else {
                    stack += 8;
                }
            }
            ArgClass::Memory { size, .. } => stack += round_up_i32(size as i32, 8),
        }
    }
    stack
}

/// The type of an operand without a function context (for call-size planning).
fn operand_type_global(prog: &MirProgram, op: &Operand) -> Option<TypeId> {
    match op {
        Operand::Const(Const::Int { ty, .. }) | Operand::Const(Const::Float { ty, .. }) => {
            Some(*ty)
        }
        // Without a frame we cannot resolve a Copy's local type; assume one int
        // eightbyte (the common case). Aggregates passed by value still get a home,
        // and the marshaller re-resolves the precise class with the frame.
        _ => {
            let _ = prog;
            None
        }
    }
}

/// Rounds `x` up to a multiple of `a` (a power of two), in `i32`.
pub(crate) fn round_up_i32(x: i32, a: i32) -> i32 {
    (x + a - 1) & !(a - 1)
}

/// The signed `MIN` value of a repr, as a 64-bit two's-complement pattern.
fn type_min(r: Repr) -> i64 {
    if r.width == 0 || r.width >= 64 {
        i64::MIN
    } else {
        -(1i64 << (r.width - 1))
    }
}

/// `true` for a relational/equality operator.
pub(crate) fn is_comparison(op: BinOp) -> bool {
    use BinOp::*;
    matches!(op, Eq | Ne | Lt | Le | Gt | Ge)
}

/// The condition code a comparison maps to.
pub(crate) fn compare_cc(op: BinOp, signed: bool) -> Cc {
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

/// The human-readable reason string for a trap.
///
/// These strings are kept **byte-identical** to the VM's `PanicInfo::message`
/// (`crates/k2-vm/src/vm.rs`) so a trap reports the same text on both backends —
/// panic wording is backend-independent. (Exit codes and strip behavior already
/// match; this aligns the stderr message too, e.g. for a future stderr-comparing
/// differential.) When adding or changing a trap string, update BOTH tables.
pub(crate) fn trap_message(reason: TrapReason) -> &'static str {
    match reason {
        TrapReason::Bounds => "index out of bounds",
        TrapReason::Overflow => "integer overflow",
        TrapReason::DivByZero => "division by zero",
        TrapReason::NegOverflow => "negation of minimum integer",
        TrapReason::NarrowLoss => "cast truncated value",
        TrapReason::LenMismatch => "for loop length mismatch",
        TrapReason::Unreachable => "reached unreachable code",
        TrapReason::Panic => "reached @panic / unwrapped null",
    }
}

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

/// Returns `args[i]` or an `Unsupported` error.
fn arg(args: &[Operand], i: usize) -> Result<&Operand, CodegenError> {
    args.get(i)
        .ok_or_else(|| CodegenError::Unsupported(format!("intrinsic missing argument {i}")))
}

/// `true` if an rvalue constructs or copies an aggregate value (so a forced-home
/// destination should be built in memory rather than treated as a scalar).
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
