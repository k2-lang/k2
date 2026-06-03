//! The register-machine instruction set the MIR compiles to.
//!
//! MIR is already three-address with explicit locals, so the natural target is a
//! flat register file per call frame where register `i` is MIR local `i`. The
//! compiler ([`crate::compile`]) lays out each basic block's statements into a
//! flat `Vec<Instr>`, recording each block's start offset, then patches every
//! jump target to an instruction offset. The VM ([`crate::vm`]) is a `loop`
//! advancing a program counter over that vector.
//!
//! Constants too large to keep inline (string bytes, error tags, aggregate
//! shapes) live in the per-function constant pool and are referenced by index.

use k2_mir::{BinOp, CastKind, DiscrKind, FnId, SliceMeta, TrapReason, UnOp};

use crate::value::{IntRepr, Value};

/// A register index within a call frame (1:1 with a MIR `LocalId`).
pub type Reg = u32;

/// A constant-pool index within a [`CompiledFn`].
pub type KIdx = u32;

/// A resolved intrinsic operation. The MIR's textual [`IntrinsicPath`] is
/// matched to one of these at compile time so dispatch in the hot loop is a
/// cheap `match`, never a string compare.
#[derive(Clone, Debug)]
pub enum IntrinsicId {
    /// `sys.io.stdout()` -> a stdout writer capability.
    StdoutWriter,
    /// `sys.io.stderr()` -> a stderr writer capability.
    StderrWriter,
    /// `sys.io` -> the io namespace capability (rarely materialized alone).
    IoCap,
    /// `sys.heap` -> the allocator capability.
    HeapCap,
    /// `writer.print(fmt, args)` -> format + write, yielding `Ok(void)`.
    Print,
    /// `alloc.create(T)` -> a one-cell allocation, yielding `Ok(*T)`. The element
    /// type is read from the `undef` carrier arg at run time.
    Create,
    /// `alloc.destroy(ptr)` -> free the cell, yielding void.
    Destroy,
    /// `alloc.alloc(T, n)` -> an n-cell allocation, yielding `Ok([]T)`. The
    /// element type is read from the `undef` carrier arg at run time.
    Alloc,
    /// `alloc.free(slice)` -> free the slice's cell, yielding void.
    Free,
    /// `@errorName(e)` -> the error's name as a `[]const u8`.
    ErrorName,
    /// `@typeName(T)` -> the type's name as a `[]const u8`.
    TypeName,
    /// `@no_add_overflow(a, b, T)` -> `true` if `a + b` fits `T`.
    NoAddOverflow,
    /// `@no_sub_overflow(a, b, T)` -> `true` if `a - b` fits `T`.
    NoSubOverflow,
    /// `@no_mul_overflow(a, b, T)` -> `true` if `a * b` fits `T`.
    NoMulOverflow,
    /// `@no_div_overflow(a, b, T)` -> `true` unless `a == MIN(T) && b == -1` for
    /// a signed `T` (the only signed `/`/`%` overflow).
    NoDivOverflow,
    /// `@no_neg_overflow(a, T)` -> `true` unless `a == MIN` of a signed `T`.
    NoNegOverflow,
    /// `@narrow_fits(v, T)` -> `true` if `v` fits the narrower `T`.
    NarrowFits,
    /// An intrinsic the VM does not implement (e.g. a `std.testing.*` member
    /// reached outside the `run` path). Dispatch yields a clean panic naming it.
    Unsupported(String),
}

/// A single VM instruction. Block targets in control-flow instructions are
/// instruction offsets *after* the compiler's patch pass.
#[derive(Clone, Debug)]
pub enum Instr {
    /// `dst = const_pool[k]`.
    ConstK { dst: Reg, k: KIdx },
    /// `dst = src` (a value copy).
    Move { dst: Reg, src: Reg },
    /// `dst = a <op> b`, with the result width/signedness in `repr`.
    Bin {
        /// Destination register.
        dst: Reg,
        /// The operator.
        op: BinOp,
        /// Left operand register.
        a: Reg,
        /// Right operand register.
        b: Reg,
        /// The result integer representation (ignored for float/bool results).
        repr: IntRepr,
    },
    /// `dst = <op> a`.
    Un {
        /// Destination register.
        dst: Reg,
        /// The operator.
        op: UnOp,
        /// Operand register.
        a: Reg,
        /// The result integer representation.
        repr: IntRepr,
    },
    /// `dst = cast(kind, a) : to`.
    Cast {
        /// Destination register.
        dst: Reg,
        /// The cast kind.
        kind: CastKind,
        /// Source register.
        a: Reg,
        /// The target integer representation (for int casts).
        to: IntRepr,
        /// `true` if the target type is a float (drives int<->float casts).
        to_float: bool,
    },
    /// Unconditional jump to an instruction offset.
    Jump { target: usize },
    /// Branch on a boolean register to one of two instruction offsets.
    Branch {
        /// The condition register.
        cond: Reg,
        /// Target when the condition is true.
        then_pc: usize,
        /// Target when the condition is false.
        else_pc: usize,
    },
    /// Switch on an integer register.
    Switch {
        /// The scrutinee register.
        scrut: Reg,
        /// The `(value, target)` arms.
        arms: Vec<(i128, usize)>,
        /// The default target.
        default: usize,
    },
    /// Return the value in `src` from the current frame.
    Return { src: Reg },
    /// Diverge into a clean panic with the given reason.
    Trap { reason: TrapReason },
    /// Statically-unreachable fall-through (treated as a defensive panic).
    Unreachable,
    /// `dst = func(args...)`.
    Call {
        /// Destination register for the result.
        dst: Reg,
        /// The callee.
        func: FnId,
        /// The argument registers, in order.
        args: Vec<Reg>,
    },
    /// `dst = intrinsic(args...)`.
    Intrinsic {
        /// Destination register for the result.
        dst: Reg,
        /// The resolved intrinsic.
        id: IntrinsicId,
        /// The receiver register, if the intrinsic root was a value chain.
        recv: Option<Reg>,
        /// The argument registers, in order.
        args: Vec<Reg>,
    },
    /// `dst = base.field` (load a struct/tuple field by index).
    LoadField { dst: Reg, base: Reg, idx: u32 },
    /// `dst = base[i]` (load a slice/array element).
    LoadIndex { dst: Reg, base: Reg, index: Reg },
    /// `dst = *ptr` (load through a pointer).
    LoadDeref { dst: Reg, ptr: Reg },
    /// `dst = base.len` / `base.ptr` (read a slice's fat-pointer half).
    LoadSliceMeta {
        /// Destination register.
        dst: Reg,
        /// The slice register.
        base: Reg,
        /// Which half.
        which: SliceMeta,
    },
    /// `dst = payload(base)` — unwrap an optional's `Some` or an error union's
    /// `Ok`.
    LoadPayload { dst: Reg, base: Reg },
    /// `dst = discr(base)` — read an optional/error-union/tagged-union
    /// discriminant.
    Discr {
        /// Destination register.
        dst: Reg,
        /// The aggregate register.
        base: Reg,
        /// What discriminant to read.
        kind: DiscrKind,
    },
    /// A store to a projected place. The place is a base register plus a chain of
    /// projection steps applied to locate the slot, then `src` is written there.
    Store {
        /// The base register the place is rooted at.
        base: Reg,
        /// The projection steps to the destination slot.
        steps: Vec<StoreStep>,
        /// The source register.
        src: Reg,
    },
    /// `dst = &place` — take the address of a place, boxing a local home if the
    /// base is a bare local.
    Ref {
        /// Destination register for the pointer.
        dst: Reg,
        /// The base register the place is rooted at.
        base: Reg,
        /// `true` if the base is a bare local (box its current value into a fresh
        /// heap cell on the fly).
        base_is_local: bool,
        /// The projection steps to the addressed slot.
        steps: Vec<StoreStep>,
    },
    /// `dst = make_slice(ptr, len)`.
    MakeSlice {
        dst: Reg,
        ptr: Reg,
        offset: Reg,
        len: Reg,
    },
    /// `dst = Some(src)`.
    MakeSome { dst: Reg, src: Reg },
    /// `dst = null`.
    MakeNull { dst: Reg },
    /// `dst = Ok(src)`.
    MakeOk { dst: Reg, src: Reg },
    /// `dst = Err(tag)`.
    MakeErr { dst: Reg, tag: u16 },
    /// `dst = aggregate(kind, fields...)`.
    Aggregate {
        /// Destination register.
        dst: Reg,
        /// The aggregate kind.
        kind: AggregateKind,
        /// The field registers, in layout order.
        fields: Vec<Reg>,
    },
    /// Initialize an `address_taken` local's heap home (boxing its current value
    /// into a fresh cell and replacing its register with the pointer). A no-op
    /// for ordinary locals.
    InitAddrLocal { reg: Reg, init_k: KIdx },
}

/// The aggregate kind a [`Instr::Aggregate`] builds (a VM-local mirror of the
/// MIR's `AggKind`, kept here so the ISA does not leak the MIR enum into the
/// hot loop's matching surface).
#[derive(Clone, Copy, Debug)]
pub enum AggregateKind {
    /// A struct literal (fields in layout order).
    Struct,
    /// An array literal.
    Array,
    /// A positional tuple.
    Tuple,
}

/// One step in locating a projected place for a store or address-of. The chain
/// is walked left to right from the base value to the destination slot.
#[derive(Clone, Debug)]
pub enum StoreStep {
    /// Follow a pointer (`*p`).
    Deref,
    /// Descend into a struct/tuple field by index.
    Field(u32),
    /// Index a slice/array by the value in a register.
    Index(Reg),
}

/// A compiled function: its instruction stream, constant pool, and the number of
/// registers (= MIR locals) a frame needs.
pub struct CompiledFn {
    /// The flat instruction stream.
    pub code: Vec<Instr>,
    /// The constant pool.
    pub consts: Vec<Value>,
    /// The number of registers a frame for this function needs.
    pub num_regs: usize,
    /// The registers (by index) that are `address_taken` and so need a heap home.
    pub addr_taken: Vec<bool>,
}
