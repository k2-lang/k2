//! The MIR -> bytecode compiler.
//!
//! Each [`MirFunction`] is compiled once, at program load, into a [`CompiledFn`]:
//! a flat instruction stream plus a constant pool. Registers map 1:1 to MIR
//! locals (register `i` is local `i`), so no allocation pass is needed — a frame
//! is a flat `Vec<Value>` sized to `locals.len()`.
//!
//! Compilation is two passes per function. The first lays out each basic block's
//! statements and terminator into the instruction vector, recording each block's
//! start offset and remembering which control-flow instructions still reference a
//! `BlockId`. The second patches every such reference to the recorded offset.

use k2_mir::{
    AggKind, Const, ConstData, IntrinsicPath, IntrinsicRoot, MirFunction, MirProgram, Operand,
    Place, Proj, Rvalue, Statement, Terminator,
};
use k2_types::{ArrayLen, IntBits, Type, TypeArena, TypeId};

use crate::isa::{AggregateKind, CompiledFn, Instr, IntrinsicId, KIdx, Reg, StoreStep};
use crate::value::{IntRepr, Value};

/// Resolves the [`IntRepr`] of an integer type. A non-integer type (it should not
/// occur where a repr is asked for) falls back to the unbounded `comptime`
/// representation, which is harmless (no masking).
pub fn int_repr_of(arena: &TypeArena, ty: TypeId) -> IntRepr {
    match arena.get(ty) {
        Type::Int { signed, bits } => IntRepr {
            width: match bits {
                IntBits::Fixed(n) => *n,
                IntBits::Usize | IntBits::Isize => 64,
            },
            signed: *signed,
        },
        Type::ComptimeInt => IntRepr::COMPTIME,
        // `bool`/enum discriminants flow through integer ops too; treat them as
        // unbounded so equality/branch logic is never masked.
        _ => IntRepr::COMPTIME,
    }
}

/// Decodes a MIR [`Const`] into a runtime [`Value`], pulling interned bytes /
/// aggregates from the program's constant table.
pub fn const_to_value(prog: &MirProgram, c: &Const) -> Value {
    let arena = &prog.arena;
    match c {
        Const::Int { value, ty } => Value::int(*value, int_repr_of(arena, *ty)),
        Const::Float { bits, .. } => Value::Float(f64::from_bits(*bits)),
        Const::Bool(b) => Value::Bool(*b),
        Const::Void => Value::Unit,
        Const::Str(id) => match &prog.consts[id.0 as usize] {
            ConstData::Bytes(b) => Value::Str(std::rc::Rc::new(b.clone())),
            ConstData::Aggregate(_) => Value::Str(std::rc::Rc::new(Vec::new())),
        },
        Const::EnumVal { variant, .. } => Value::Enum {
            tag: *variant,
            payload: Box::new(Value::Unit),
        },
        Const::ErrVal { tag, .. } => Value::ErrVal(tag.0),
        Const::EmptySlice { .. } => Value::Slice {
            ptr: crate::heap::Ptr::NULL,
            len: 0,
        },
        Const::Undef { ty } => Value::Undef(*ty),
        Const::Aggregate { id, ty } => aggregate_const_value(prog, *id, *ty),
        // The v0.11 spawn tag: a function reference, carried by value so the VM's
        // `@schedSpawn` can build the new fiber's root frame.
        Const::FnRef(f) => Value::FnRef(*f),
    }
}

/// Materializes an interned aggregate constant (`agg#k`) into a [`Value`].
fn aggregate_const_value(prog: &MirProgram, id: k2_mir::ConstId, ty: TypeId) -> Value {
    let ConstData::Aggregate(fields) = &prog.consts[id.0 as usize] else {
        return Value::Unit;
    };
    let elems: Vec<Value> = fields
        .iter()
        .map(|op| match op {
            Operand::Const(c) => const_to_value(prog, c),
            // A non-const element in an interned aggregate cannot be resolved
            // without a frame; such aggregates do not occur in the corpus.
            Operand::Copy(_) => Value::Unit,
        })
        .collect();
    match prog.arena.get(ty) {
        Type::Array { .. } => Value::Array(std::rc::Rc::new(elems)),
        _ => Value::Struct(std::rc::Rc::new(elems)),
    }
}

/// Compiles every function in `prog` into a [`CompiledFn`].
pub fn compile_program(prog: &MirProgram) -> Vec<CompiledFn> {
    prog.funcs.iter().map(|f| compile_fn(prog, f)).collect()
}

/// The per-function compilation state.
struct FnCompiler<'p> {
    prog: &'p MirProgram,
    func: &'p MirFunction,
    code: Vec<Instr>,
    consts: Vec<Value>,
    /// `BlockId.index() -> instruction offset of that block's first instr`.
    block_offsets: Vec<usize>,
    /// The width of the operand-scratch window: the largest number of non-trivial
    /// operands any single rvalue (an `Aggregate`/`Call`/`Intrinsic`) in this
    /// function materializes, with a small floor. The receiver / projected-store /
    /// eval scratch slots live *past* this window so they never collide with an
    /// operand slot, and the frame is sized to cover them all. See
    /// [`max_operand_scratch`] and [`FnCompiler::op_scratch`].
    op_window: usize,
}

impl<'p> FnCompiler<'p> {
    /// Interns a constant value into the pool, returning its index.
    fn intern_const(&mut self, v: Value) -> KIdx {
        let k = self.consts.len() as KIdx;
        self.consts.push(v);
        k
    }

    /// `true` if `ty` is an `Optional`, used for the bare-value -> optional
    /// coercion rule on `Use`.
    fn is_optional(&self, ty: TypeId) -> bool {
        matches!(self.prog.arena.get(ty), Type::Optional(_))
    }

    /// The [`IntRepr`] an arithmetic/bitwise result should carry: the rvalue's own
    /// result type, unless that is an unsized `comptime_int` (width 0) and the
    /// destination local has a concrete sized integer repr — in which case the
    /// destination repr is used so the stored value is masked to its real width.
    fn result_repr(&self, ty: TypeId, dst_ty: Option<TypeId>) -> IntRepr {
        let r = int_repr_of(&self.prog.arena, ty);
        if r.width == 0 {
            if let Some(dty) = dst_ty {
                let dr = int_repr_of(&self.prog.arena, dty);
                if dr.width != 0 {
                    return dr;
                }
            }
        }
        r
    }

    /// Emits an instruction, returning its index (so a later patch pass can fix a
    /// block target embedded in it).
    fn emit(&mut self, instr: Instr) -> usize {
        let idx = self.code.len();
        self.code.push(instr);
        idx
    }

    /// Lowers an [`Operand`] into a register holding its value, emitting any
    /// const-load / projection-load needed. `scratch` is a fresh temp register the
    /// caller guarantees is unused at this point.
    fn operand_to_reg(&mut self, op: &Operand, scratch: Reg) -> Reg {
        match op {
            Operand::Copy(place) => self.place_to_reg(place, scratch),
            Operand::Const(c) => {
                let v = const_to_value(self.prog, c);
                let k = self.intern_const(v);
                self.emit(Instr::ConstK { dst: scratch, k });
                scratch
            }
        }
    }

    /// Lowers a [`Place`] read into a register holding the projected value. A
    /// bare local reads its own register directly; a projected place threads
    /// `Load*` instructions into `scratch`.
    fn place_to_reg(&mut self, place: &Place, scratch: Reg) -> Reg {
        if place.proj.is_empty() {
            return place.base.0;
        }
        // Start from the base register, then apply each projection into scratch.
        let mut cur = place.base.0;
        for (i, proj) in place.proj.iter().enumerate() {
            let dst = scratch;
            match proj {
                Proj::Deref => {
                    self.emit(Instr::LoadDeref { dst, ptr: cur });
                }
                Proj::Field { index, .. } => {
                    self.emit(Instr::LoadField {
                        dst,
                        base: cur,
                        idx: *index,
                    });
                }
                Proj::Index { index, .. } => {
                    // The index operand may itself be a place/const; load it into
                    // a *separate* scratch so it does not clobber `cur`. We use
                    // the destination's own register for the base load and a
                    // reserved high temp for the index; to stay within the frame
                    // we reuse `scratch` only after consuming the base. Because
                    // index operands in the corpus are bare locals or consts, we
                    // materialize them via a dedicated helper register.
                    let idx_reg = self.operand_to_index_reg(index);
                    self.emit(Instr::LoadIndex {
                        dst,
                        base: cur,
                        index: idx_reg,
                    });
                }
                Proj::SliceMeta { which, .. } => {
                    self.emit(Instr::LoadSliceMeta {
                        dst,
                        base: cur,
                        which: *which,
                    });
                }
                Proj::Payload { .. } => {
                    self.emit(Instr::LoadPayload { dst, base: cur });
                }
            }
            cur = dst;
            let _ = i;
        }
        cur
    }

    /// Materializes an index operand into a register. A bare-local index reads its
    /// own register; a const index is loaded into a reserved scratch slot at the
    /// top of the frame.
    fn operand_to_index_reg(&mut self, op: &Operand) -> Reg {
        match op {
            Operand::Copy(p) if p.proj.is_empty() => p.base.0,
            Operand::Copy(p) => {
                // A projected index (rare) — load it into the index scratch.
                let scratch = self.index_scratch();
                self.place_to_reg(p, scratch)
            }
            Operand::Const(c) => {
                let v = const_to_value(self.prog, c);
                let k = self.intern_const(v);
                let scratch = self.index_scratch();
                self.emit(Instr::ConstK { dst: scratch, k });
                scratch
            }
        }
    }

    /// The reserved scratch register for index materialization (one past the last
    /// MIR local). The frame is sized to include these scratch slots.
    fn index_scratch(&self) -> Reg {
        self.func.locals.len() as Reg
    }

    /// The `n`-th operand-scratch register. Operand windows occupy
    /// `op_scratch(0)..op_scratch(op_window - 1)`; `n` must be `< op_window`
    /// (guaranteed because `op_window` is sized to the function's widest operand
    /// list — see [`max_operand_scratch`]). Slot layout in the frame is:
    /// `[locals..][index_scratch][op_scratch 0..op_window-1][recv][store][eval]`.
    fn op_scratch(&self, n: usize) -> Reg {
        debug_assert!(
            n < self.op_window,
            "op_scratch index {n} exceeds operand window {}",
            self.op_window
        );
        self.func.locals.len() as Reg + 1 + n as Reg
    }

    /// The dedicated receiver scratch slot for a value-rooted intrinsic chain. It
    /// sits *past* the operand window so a many-argument intrinsic call cannot
    /// overwrite the receiver register with one of its own operand scratch slots.
    fn recv_scratch(&self) -> Reg {
        self.func.locals.len() as Reg + 1 + self.op_window as Reg
    }

    /// The dedicated scratch slot for a projected-destination store (compute the
    /// rvalue here, then store it through the place chain).
    fn store_scratch(&self) -> Reg {
        self.recv_scratch() + 1
    }

    /// The dedicated scratch slot for a discarded `Eval` rvalue.
    fn eval_scratch(&self) -> Reg {
        self.recv_scratch() + 2
    }

    /// Builds the `StoreStep` chain and base info for a projected place, lowering
    /// any index operands into registers.
    fn place_steps(&mut self, place: &Place) -> (Reg, Vec<StoreStep>) {
        let mut steps = Vec::with_capacity(place.proj.len());
        for proj in &place.proj {
            match proj {
                Proj::Deref => steps.push(StoreStep::Deref),
                Proj::Field { index, .. } => steps.push(StoreStep::Field(*index)),
                Proj::Index { index, .. } => {
                    let r = self.operand_to_index_reg(index);
                    steps.push(StoreStep::Index(r));
                }
                Proj::SliceMeta { .. } | Proj::Payload { .. } => {
                    // SliceMeta/Payload never appear on the *destination* of an
                    // assign in the corpus (they are read-only projections); model
                    // them defensively as a field 0 step.
                    steps.push(StoreStep::Field(0));
                }
            }
        }
        (place.base.0, steps)
    }

    /// Compiles an rvalue, writing its result into `dst`.
    fn compile_rvalue(&mut self, dst: Reg, rvalue: &Rvalue, dst_ty: Option<TypeId>) {
        match rvalue {
            Rvalue::Use(op) => {
                let src = self.operand_to_reg(op, dst);
                if src != dst {
                    self.emit(Instr::Move { dst, src });
                }
                // Optional coercion: a bare value stored into a `?T` slot (the
                // `_9: ?i32 = 7` shape) is wrapped as `Some`, unless it already is
                // an optional/undef (those are handled at runtime by Discr).
                if let Some(ty) = dst_ty {
                    if self.is_optional(ty) && !self.operand_is_optional_like(op) {
                        self.emit(Instr::MakeSome { dst, src: dst });
                    }
                }
            }
            Rvalue::Binary { op, lhs, rhs, ty } => {
                let a = self.operand_to_reg(lhs, self.op_scratch(0));
                let b = self.operand_to_reg(rhs, self.op_scratch(1));
                // Use the result type's repr, falling back to the destination
                // local's sized repr for a comptime-typed result (so a
                // comptime-folded sized value is masked to its real width). A
                // comparison result is `bool` (width 0) and must NOT borrow the
                // destination width — the VM derives the comparison repr from the
                // live operands instead, so leaving width 0 here is correct.
                let repr = if is_comparison(*op) {
                    int_repr_of(&self.prog.arena, *ty)
                } else {
                    self.result_repr(*ty, dst_ty)
                };
                self.emit(Instr::Bin {
                    dst,
                    op: *op,
                    a,
                    b,
                    repr,
                });
            }
            Rvalue::Unary { op, operand, ty } => {
                let a = self.operand_to_reg(operand, self.op_scratch(0));
                // A comptime-typed negation (`const c: i32 = -1` lowers to
                // `neg 1` whose own type is `comptime_int`) carries no width, so
                // its result would print at full 128-bit width. Prefer the
                // destination local's sized repr when the rvalue type is unsized,
                // so the stored value is masked/sign-extended to its real width.
                let repr = self.result_repr(*ty, dst_ty);
                self.emit(Instr::Un {
                    dst,
                    op: *op,
                    a,
                    repr,
                });
            }
            Rvalue::Ref { place, .. } => {
                let base_is_local = place.proj.is_empty();
                let (base, steps) = self.place_steps(place);
                self.emit(Instr::Ref {
                    dst,
                    base,
                    base_is_local,
                    steps,
                });
            }
            Rvalue::Cast { kind, operand, ty } => {
                let a = self.operand_to_reg(operand, self.op_scratch(0));
                let (to, to_float) = match self.prog.arena.get(*ty) {
                    Type::Float { .. } => (IntRepr::COMPTIME, true),
                    _ => (int_repr_of(&self.prog.arena, *ty), false),
                };
                self.emit(Instr::Cast {
                    dst,
                    kind: *kind,
                    a,
                    to,
                    to_float,
                });
            }
            Rvalue::MakeSlice {
                ptr, offset, len, ..
            } => {
                let p = self.operand_to_reg(ptr, self.op_scratch(0));
                let o = self.operand_to_reg(offset, self.op_scratch(1));
                let l = self.operand_to_reg(len, self.op_scratch(2));
                self.emit(Instr::MakeSlice {
                    dst,
                    ptr: p,
                    offset: o,
                    len: l,
                });
            }
            Rvalue::MakeSome(op, _) => {
                let src = self.operand_to_reg(op, self.op_scratch(0));
                self.emit(Instr::MakeSome { dst, src });
            }
            Rvalue::MakeNull(_) => {
                self.emit(Instr::MakeNull { dst });
            }
            Rvalue::MakeOk(op, _) => {
                let src = self.operand_to_reg(op, self.op_scratch(0));
                self.emit(Instr::MakeOk { dst, src });
            }
            Rvalue::MakeErr(tag, _) => {
                self.emit(Instr::MakeErr { dst, tag: tag.0 });
            }
            Rvalue::Discriminant { operand, kind } => {
                let base = self.operand_to_reg(operand, self.op_scratch(0));
                self.emit(Instr::Discr {
                    dst,
                    base,
                    kind: *kind,
                });
            }
            Rvalue::Aggregate { kind, fields, .. } => {
                let regs = self.operands_to_regs(fields);
                self.emit(Instr::Aggregate {
                    dst,
                    kind: agg_kind(*kind),
                    fields: regs,
                });
            }
            Rvalue::Call { func, args, .. } => {
                let regs = self.operands_to_regs(args);
                self.emit(Instr::Call {
                    dst,
                    func: *func,
                    args: regs,
                });
            }
            Rvalue::Intrinsic { path, args, .. } => {
                let id = self.resolve_intrinsic(path);
                let recv = self.intrinsic_receiver(path);
                let regs = self.operands_to_regs(args);
                self.emit(Instr::Intrinsic {
                    dst,
                    id,
                    recv,
                    args: regs,
                });
            }
        }
    }

    /// `true` if an operand already carries optional/undef-shaped data, so the
    /// `Use`-into-`?T` coercion must NOT re-wrap it.
    fn operand_is_optional_like(&self, op: &Operand) -> bool {
        match op {
            Operand::Const(Const::Undef { .. }) => true,
            Operand::Copy(p) => {
                // If the source place's type is already optional, it's a copy of
                // an optional, not a payload to wrap.
                let mut ty = self.func.locals[p.base.index()].ty;
                for proj in &p.proj {
                    ty = match proj {
                        Proj::Deref => match self.prog.arena.get(ty) {
                            Type::Pointer { pointee, .. } => *pointee,
                            _ => ty,
                        },
                        Proj::Field { ty: fty, .. } => *fty,
                        Proj::Index { ty: ety, .. } => *ety,
                        Proj::SliceMeta { ty: mty, .. } => *mty,
                        Proj::Payload { ty: pty } => *pty,
                    };
                }
                self.is_optional(ty)
            }
            _ => false,
        }
    }

    /// Lowers a list of operands into a fresh window of scratch registers,
    /// returning the register list. Operands that are bare locals reuse their own
    /// register; consts/projections are materialized into successive op-scratch
    /// slots so they do not collide.
    fn operands_to_regs(&mut self, ops: &[Operand]) -> Vec<Reg> {
        let mut regs = Vec::with_capacity(ops.len());
        let mut scratch_n = 0usize;
        for op in ops {
            match op {
                Operand::Copy(p) if p.proj.is_empty() => regs.push(p.base.0),
                _ => {
                    let s = self.op_scratch(scratch_n);
                    scratch_n += 1;
                    let r = self.operand_to_reg(op, s);
                    regs.push(r);
                }
            }
        }
        regs
    }

    /// The receiver register for a value-rooted intrinsic chain (`value(_n)....`).
    fn intrinsic_receiver(&mut self, path: &IntrinsicPath) -> Option<Reg> {
        match &path.root {
            IntrinsicRoot::Value(op) => match op.as_ref() {
                Operand::Copy(p) if p.proj.is_empty() => Some(p.base.0),
                other => Some(self.operand_to_reg(other, self.recv_scratch())),
            },
            _ => None,
        }
    }

    /// Resolves a textual intrinsic path to a concrete [`IntrinsicId`]. The
    /// width/element-type carriers for the check predicates and `create`/`alloc`
    /// are read from the live `undef` argument value at run time (the VM has it),
    /// so dispatch here is a pure name match.
    fn resolve_intrinsic(&self, path: &IntrinsicPath) -> IntrinsicId {
        match &path.root {
            IntrinsicRoot::Builtin(name) => match name.as_str() {
                "no_add_overflow" => IntrinsicId::NoAddOverflow,
                "no_sub_overflow" => IntrinsicId::NoSubOverflow,
                "no_mul_overflow" => IntrinsicId::NoMulOverflow,
                "no_div_overflow" => IntrinsicId::NoDivOverflow,
                "no_neg_overflow" => IntrinsicId::NoNegOverflow,
                "narrow_fits" => IntrinsicId::NarrowFits,
                "errorName" => IntrinsicId::ErrorName,
                "typeName" => IntrinsicId::TypeName,
                // The std allocator floor (handle-based) + the *System
                // capability readers. See `IntrinsicId` and the VM dispatcher.
                "allocId" => IntrinsicId::AllocId,
                "allocHandle" => IntrinsicId::AllocHandle,
                "allocRaw" => IntrinsicId::AllocRaw,
                "reallocRaw" => IntrinsicId::ReallocRaw,
                "freeRaw" => IntrinsicId::FreeRaw,
                "createRaw" => IntrinsicId::CreateRaw,
                "destroyRaw" => IntrinsicId::DestroyRaw,
                "arenaDeinit" => IntrinsicId::ArenaDeinit,
                "gpaDeinit" => IntrinsicId::GpaDeinit,
                "clockNow" => IntrinsicId::ClockNow,
                "clockSleep" => IntrinsicId::ClockSleep,
                "randomBytes" => IntrinsicId::RandomBytes,
                "randomInt" => IntrinsicId::RandomInt,
                "envGet" => IntrinsicId::EnvGet,
                "bufPrint" => IntrinsicId::BufPrint,
                // The concurrency / scheduler floor (v0.11). See `IntrinsicId` and
                // the VM dispatcher + `crate::sched`.
                "schedSpawn" => IntrinsicId::SchedSpawn,
                "schedYield" => IntrinsicId::SchedYield,
                "schedAwait" => IntrinsicId::SchedAwait,
                "schedRun" => IntrinsicId::SchedRun,
                "chanMake" => IntrinsicId::ChanMake,
                "chanSend" => IntrinsicId::ChanSend,
                "chanRecv" => IntrinsicId::ChanRecv,
                "chanClose" => IntrinsicId::ChanClose,
                "chanLen" => IntrinsicId::ChanLen,
                "mutexMake" => IntrinsicId::MutexMake,
                "mutexLock" => IntrinsicId::MutexLock,
                "mutexUnlock" => IntrinsicId::MutexUnlock,
                "atomicMake" => IntrinsicId::AtomicMake,
                "atomicLoad" => IntrinsicId::AtomicLoad,
                "atomicStore" => IntrinsicId::AtomicStore,
                "atomicFetchAdd" => IntrinsicId::AtomicFetchAdd,
                "atomicSwap" => IntrinsicId::AtomicSwap,
                "atomicCas" => IntrinsicId::AtomicCas,
                "wgMake" => IntrinsicId::WgMake,
                "wgAdd" => IntrinsicId::WgAdd,
                "wgDone" => IntrinsicId::WgDone,
                "wgWait" => IntrinsicId::WgWait,
                other => IntrinsicId::Unsupported(format!("@{other}")),
            },
            IntrinsicRoot::Value(_) => {
                let members: Vec<&str> = path.members.iter().map(|s| s.as_str()).collect();
                match members.as_slice() {
                    ["io", "stdout"] => IntrinsicId::StdoutWriter,
                    ["io", "stderr"] => IntrinsicId::StderrWriter,
                    ["io"] => IntrinsicId::IoCap,
                    ["stdout"] => IntrinsicId::StdoutWriter,
                    ["stderr"] => IntrinsicId::StderrWriter,
                    ["heap"] => IntrinsicId::HeapCap,
                    ["clock"] => IntrinsicId::ClockNow,
                    // The documented `*System` capability method spellings (spec
                    // §10.7). The underlying intrinsics are deterministic: the
                    // clock starts at 0 and only advances on `sleep`, the PRNG is a
                    // fixed-seed splitmix64, and env lookups are offline-absent.
                    ["clock", "now" | "monotonicNanos" | "wallNanos"] => IntrinsicId::ClockNow,
                    ["clock", "sleep"] => IntrinsicId::ClockSleep,
                    ["random", "int" | "intRangeLessThan"] => IntrinsicId::RandomInt,
                    ["random", "bytes"] => IntrinsicId::RandomBytes,
                    ["env", "get"] => IntrinsicId::EnvGet,
                    ["print"] => IntrinsicId::Print,
                    // Slice metadata reached on a still-`deferred` value (an
                    // unannotated `const g = alloc.realloc(...)` whose element
                    // type the checker left open): read the receiver's own
                    // slice length / pointer at run time.
                    ["len"] => IntrinsicId::SliceLen,
                    ["ptr"] => IntrinsicId::SlicePtr,
                    // The `Allocator` method floor, reached as member calls on an
                    // `Allocator` value (`alloc.alloc(T,n)`, `alloc.realloc(s,n)`,
                    // …). The receiver value carries the handle id (a `u32` field,
                    // or the bare `sys.heap` capability == the default id 0), and
                    // the VM routes the heap op to that allocator instance.
                    ["create"] => IntrinsicId::Create,
                    ["alloc"] => IntrinsicId::Alloc,
                    ["realloc"] => IntrinsicId::ReallocRaw,
                    ["destroy"] => IntrinsicId::Destroy,
                    ["free"] => IntrinsicId::Free,
                    // The v0.11 concurrency method floor, reached as member calls
                    // on a still-`deferred` handle (e.g. `c.mu.lock()` where the
                    // field access through `*Counter` left the `Mutex` type open).
                    // The receiver value carries the handle id in its first field;
                    // the VM reads it and routes to the scheduler. (These method
                    // names are distinct from the slice/allocator floor above.)
                    ["lock"] => IntrinsicId::MutexLock,
                    ["unlock"] => IntrinsicId::MutexUnlock,
                    ["send"] => IntrinsicId::ChanSend,
                    ["recv"] => IntrinsicId::ChanRecv,
                    ["close"] => IntrinsicId::ChanClose,
                    ["load"] => IntrinsicId::AtomicLoad,
                    ["store"] => IntrinsicId::AtomicStore,
                    ["fetchAdd"] => IntrinsicId::AtomicFetchAdd,
                    ["swap"] => IntrinsicId::AtomicSwap,
                    ["cmpxchgStrong" | "cmpxchgWeak"] => IntrinsicId::AtomicCas,
                    ["add"] => IntrinsicId::WgAdd,
                    ["done"] => IntrinsicId::WgDone,
                    ["wait"] => IntrinsicId::WgWait,
                    _ => IntrinsicId::Unsupported(format!("value.{}", path.dotted())),
                }
            }
            // Module-rooted members (`std.testing.*`) are only reached by `test`
            // blocks, which `run` does not execute. Surface a named unsupported
            // intrinsic so any stray reach is a clean panic.
            IntrinsicRoot::Module(_) => {
                IntrinsicId::Unsupported(format!("module.{}", path.dotted()))
            }
        }
    }
}

/// `true` for the relational/equality operators, whose result type is `bool`
/// (so the destination repr must not be borrowed for the result width — see
/// [`FnCompiler::compile_rvalue`]).
fn is_comparison(op: k2_mir::BinOp) -> bool {
    use k2_mir::BinOp::*;
    matches!(op, Eq | Ne | Lt | Le | Gt | Ge)
}

/// Maps a MIR aggregate kind to the ISA's.
fn agg_kind(k: AggKind) -> AggregateKind {
    match k {
        AggKind::Struct => AggregateKind::Struct,
        AggKind::Array => AggregateKind::Array,
        AggKind::Tuple => AggregateKind::Tuple,
    }
}

/// The default value for a type, used to initialize a freshly-`create`d cell or
/// an `address_taken` local's home.
pub fn default_value(arena: &TypeArena, ty: TypeId) -> Value {
    match arena.get(ty) {
        Type::Int { .. } | Type::ComptimeInt => Value::int(0, int_repr_of(arena, ty)),
        Type::Float { .. } | Type::ComptimeFloat => Value::Float(0.0),
        Type::Bool => Value::Bool(false),
        Type::Void => Value::Unit,
        Type::Optional(_) => Value::Optional(None),
        Type::Pointer { .. } => Value::Ptr(crate::heap::Ptr::NULL),
        Type::Slice { .. } => Value::Slice {
            ptr: crate::heap::Ptr::NULL,
            len: 0,
        },
        Type::Array { len, elem } => {
            let n = match len {
                ArrayLen::Known(n) => *n as usize,
                _ => 0,
            };
            let e = default_value(arena, *elem);
            Value::Array(std::rc::Rc::new(vec![e; n]))
        }
        Type::Struct(s) => {
            let fields: Vec<Value> = arena.structs[s.0 as usize]
                .fields
                .iter()
                .map(|f| default_value(arena, f.ty))
                .collect();
            Value::Struct(std::rc::Rc::new(fields))
        }
        _ => Value::Undef(ty),
    }
}

/// The width of the operand-scratch window a function needs: the largest number
/// of operands any single `Aggregate`/`Call`/`Intrinsic` rvalue materializes,
/// floored at a small constant so the common short rvalues keep a comfortable
/// margin and the terminator/binary helpers (which use `op_scratch(0..=2)`)
/// always have room.
///
/// `operands_to_regs` assigns one fresh scratch slot per *non-trivial* operand
/// (a bare-local operand reuses its own register and consumes no slot), so the
/// true demand is bounded by the total operand count — using the total is a safe
/// over-estimate. Sizing the frame from this (rather than a fixed `+16`) is what
/// keeps a 16+ element array literal, a many-argument call, or a wide `print`
/// from writing past the frame and aborting the host Rust process.
fn max_operand_scratch(func: &MirFunction) -> usize {
    /// Floor on the operand window: enough for the binary/cast/slice helpers
    /// (`op_scratch(0..=2)`) and a small margin of short aggregates.
    const OP_WINDOW_FLOOR: usize = 8;
    let mut max_ops = 0usize;
    for block in &func.blocks {
        for stmt in &block.stmts {
            let rvalue = match stmt {
                Statement::Assign { rvalue, .. } | Statement::Eval { rvalue, .. } => rvalue,
                _ => continue,
            };
            let n = match rvalue {
                Rvalue::Aggregate { fields, .. } => fields.len(),
                Rvalue::Call { args, .. } | Rvalue::Intrinsic { args, .. } => args.len(),
                _ => 0,
            };
            max_ops = max_ops.max(n);
        }
    }
    max_ops.max(OP_WINDOW_FLOOR)
}

/// Compiles a single function.
fn compile_fn(prog: &MirProgram, func: &MirFunction) -> CompiledFn {
    let op_window = max_operand_scratch(func);
    let mut c = FnCompiler {
        prog,
        func,
        code: Vec::new(),
        consts: Vec::new(),
        block_offsets: vec![usize::MAX; func.blocks.len()],
        op_window,
    };

    // Records of control-flow instructions needing a BlockId -> offset patch.
    let mut jump_patches: Vec<(usize, usize)> = Vec::new(); // (instr index, block index)
    let mut branch_patches: Vec<(usize, usize, usize)> = Vec::new();
    let mut switch_patches: Vec<(usize, Vec<usize>, usize)> = Vec::new();

    // Pre-init `address_taken` locals at the function entry: box their default
    // value into a heap cell so every access goes through a uniform pointer.
    let mut addr_taken = vec![false; func.locals.len()];
    for (i, l) in func.locals.iter().enumerate() {
        addr_taken[i] = l.address_taken;
    }

    for (bi, block) in func.blocks.iter().enumerate() {
        c.block_offsets[bi] = c.code.len();
        for stmt in &block.stmts {
            compile_stmt(&mut c, stmt);
        }
        // Terminator.
        match &block.term {
            Terminator::Goto(t) => {
                let idx = c.emit(Instr::Jump { target: 0 });
                jump_patches.push((idx, t.index()));
            }
            Terminator::Branch {
                cond,
                then_bb,
                else_bb,
            } => {
                let cond_reg = c.operand_to_reg(cond, c.op_scratch(0));
                let idx = c.emit(Instr::Branch {
                    cond: cond_reg,
                    then_pc: 0,
                    else_pc: 0,
                });
                branch_patches.push((idx, then_bb.index(), else_bb.index()));
            }
            Terminator::Switch {
                scrutinee,
                targets,
                default,
            } => {
                let scrut_reg = c.operand_to_reg(scrutinee, c.op_scratch(0));
                let idx = c.emit(Instr::Switch {
                    scrut: scrut_reg,
                    arms: targets.iter().map(|(v, _)| (*v, 0usize)).collect(),
                    default: 0,
                });
                switch_patches.push((
                    idx,
                    targets.iter().map(|(_, t)| t.index()).collect(),
                    default.index(),
                ));
            }
            Terminator::Return { value } => {
                let src = c.operand_to_reg(value, c.op_scratch(0));
                c.emit(Instr::Return { src });
            }
            Terminator::Trap { reason } => {
                c.emit(Instr::Trap { reason: *reason });
            }
            Terminator::Unreachable => {
                c.emit(Instr::Unreachable);
            }
        }
    }

    // Patch pass: rewrite every block target to an instruction offset.
    let off = |bi: usize| c.block_offsets[bi];
    for (idx, bi) in jump_patches {
        if let Instr::Jump { target } = &mut c.code[idx] {
            *target = off(bi);
        }
    }
    for (idx, tbi, ebi) in branch_patches {
        if let Instr::Branch {
            then_pc, else_pc, ..
        } = &mut c.code[idx]
        {
            *then_pc = off(tbi);
            *else_pc = off(ebi);
        }
    }
    for (idx, arm_bis, dbi) in switch_patches {
        if let Instr::Switch { arms, default, .. } = &mut c.code[idx] {
            for (arm, bi) in arms.iter_mut().zip(arm_bis) {
                arm.1 = off(bi);
            }
            *default = off(dbi);
        }
    }

    // The frame needs the MIR locals plus a band of scratch registers used for
    // index/operand materialization, sized from the function's *actual* widest
    // operand list (see `max_operand_scratch`) rather than a fixed slack. The
    // layout is:
    //   [locals..][index_scratch][op_scratch 0..op_window-1][recv][store][eval]
    // = locals.len() + 1 (index) + op_window (operands) + 3 (recv/store/eval).
    // Sizing from the real demand is what prevents a >=15-operand aggregate/call/
    // print from writing past the frame and aborting the host process.
    let num_regs = func.locals.len() + 1 + op_window + 3;

    CompiledFn {
        code: c.code,
        consts: c.consts,
        num_regs,
        addr_taken,
    }
}

/// Compiles a single statement.
fn compile_stmt(c: &mut FnCompiler<'_>, stmt: &Statement) {
    match stmt {
        Statement::Assign { place, rvalue, .. } => {
            if place.proj.is_empty() {
                // Common case: assign straight into the destination local's reg.
                let dst = place.base.0;
                let dst_ty = Some(c.func.locals[place.base.index()].ty);
                c.compile_rvalue(dst, rvalue, dst_ty);
            } else {
                // Projected destination: compute the rvalue into a scratch, then
                // store it through the place chain.
                let scratch = c.store_scratch();
                let dst_ty = Some(c.place_target_ty(place));
                c.compile_rvalue(scratch, rvalue, dst_ty);
                let (base, steps) = c.place_steps(place);
                c.emit(Instr::Store {
                    base,
                    steps,
                    src: scratch,
                });
            }
        }
        Statement::Eval { rvalue, .. } => {
            let scratch = c.eval_scratch();
            c.compile_rvalue(scratch, rvalue, None);
        }
        Statement::StorageLive(l) => {
            if c.func.locals[l.index()].address_taken {
                let ty = c.func.locals[l.index()].ty;
                let init = default_value(&c.prog.arena, ty);
                let k = c.intern_const(init);
                c.emit(Instr::InitAddrLocal {
                    reg: l.0,
                    init_k: k,
                });
            }
        }
        // StorageDead / Check / Note are advisory or already realized.
        Statement::StorageDead(_) | Statement::Check(_) | Statement::Note(_) => {}
    }
}

impl FnCompiler<'_> {
    /// The element type a projected place ultimately writes to (for the optional
    /// coercion rule on a projected store; rarely optional in practice).
    fn place_target_ty(&self, place: &Place) -> TypeId {
        let mut ty = self.func.locals[place.base.index()].ty;
        for proj in &place.proj {
            ty = match proj {
                Proj::Deref => match self.prog.arena.get(ty) {
                    Type::Pointer { pointee, .. } => *pointee,
                    _ => ty,
                },
                Proj::Field { ty: fty, .. } => *fty,
                Proj::Index { ty: ety, .. } => *ety,
                Proj::SliceMeta { ty: mty, .. } => *mty,
                Proj::Payload { ty: pty } => *pty,
            };
        }
        ty
    }
}
