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

    // ---- The std allocator floor (handle-based) -------------------------
    //
    // The std `Allocator` and the `std.heap.*` allocators are handle-based: an
    // `Allocator` is a `u32` id that selects an allocator *instance* in the VM's
    // per-run registry, so different kinds (GPA, arena, fixed-buffer) behave
    // differently with no fn-pointer vtables. These leaf intrinsics are what the
    // std method bodies call, passing that id.
    /// `@allocId(kind, backing_id)` -> registers a fresh allocator instance and
    /// returns its `u32` id. `kind`: 0=Default 1=GPA 2=Arena 3=FixedBuffer.
    AllocId,
    /// `@allocHandle(id)` -> the opaque `Allocator` value carrying `id`.
    AllocHandle,
    /// `@allocRaw(id, T, n)` -> `Ok([]T)` (the kind's allocate-many).
    AllocRaw,
    /// `@reallocRaw(id, slice, n)` -> `Ok([]T)`, copying contents, freeing the old.
    ReallocRaw,
    /// `@freeRaw(id, slice)` -> release; GPA/testing trap on double/foreign free.
    FreeRaw,
    /// `@createRaw(id, T)` -> `Ok(*T)` (the kind's allocate-one).
    CreateRaw,
    /// `@destroyRaw(id, ptr)` -> release one; GPA/testing trap on double free.
    DestroyRaw,
    /// `@arenaDeinit(id)` -> free every cell the arena handed out, at once.
    ArenaDeinit,
    /// `@gpaDeinit(id)` -> `true` if anything leaked (drops the tracker).
    GpaDeinit,

    // ---- The *System capability floor -----------------------------------
    /// `@clockNow(which)` -> a `u64` nanosecond reading (0=monotonic, 1=wall).
    ClockNow,
    /// `@clockSleep(ns)` -> advance the VM's monotonic clock by `ns`.
    ClockSleep,
    /// `@randomBytes(buf)` -> fill `buf: []u8` with PRNG bytes.
    RandomBytes,
    /// `@randomInt(T)` -> a `u64` PRNG draw (the caller narrows).
    RandomInt,
    /// `@envGet(name)` -> `?[]const u8`, the value of env var `name`.
    EnvGet,
    /// `@bufPrint(buf, fmt, args)` -> `Ok([]u8)` formatted into `buf`, or
    /// `error.NoSpaceLeft`.
    BufPrint,
    /// `slice.len` on a still-`deferred`-typed receiver -> its `usize` length.
    SliceLen,
    /// `slice.ptr` on a still-`deferred`-typed receiver -> its data pointer.
    SlicePtr,

    // ---- The concurrency / scheduler floor (v0.11) ----------------------
    //
    // These back the std `Executor`/`Task`/`Channel(T)`/`Mutex`/`atomic.Value(T)`/
    // `WaitGroup` types over the deterministic cooperative fiber scheduler
    // (`crate::sched`). The *blocking* ones (`@schedAwait`, `@chanSend/Recv`,
    // `@mutexLock`, `@wgWait`) may suspend the current fiber and resume into their
    // `dst` register when their waker fires.
    /// `@schedSpawn(fn, args_tuple)` -> a `u32` task/fiber id. `fn` is an `FnRef`.
    SchedSpawn,
    /// `@schedYield()` -> void. Cooperatively yields the current fiber.
    SchedYield,
    /// `@schedAwait(task_id)` -> the task's result (blocks until it completes).
    SchedAwait,
    /// `@schedRun()` -> void. Drives the ready set to quiescence (loop/waitIdle).
    SchedRun,
    /// `@chanMake(cap)` -> a `u32` channel id (`cap < 0` is unbounded).
    ChanMake,
    /// `@chanSend(chan, value)` -> `bool` (false if the channel is closed). Blocks
    /// when a bounded channel is full.
    ChanSend,
    /// `@chanRecv(chan)` -> `?T` (`null` when closed and drained). Blocks when the
    /// queue is empty and the channel is open.
    ChanRecv,
    /// `@chanClose(chan)` -> void.
    ChanClose,
    /// `@chanLen(chan)` -> `usize`, the buffered count.
    ChanLen,
    /// `@mutexMake()` -> a `u32` mutex id.
    MutexMake,
    /// `@mutexLock(m)` -> void. Blocks while the lock is held by another fiber.
    MutexLock,
    /// `@mutexUnlock(m)` -> void. Hands the lock to the first waiter, if any.
    MutexUnlock,
    /// `@atomicMake(init)` -> a `u32` atomic id.
    AtomicMake,
    /// `@atomicLoad(a)` -> the cell value.
    AtomicLoad,
    /// `@atomicStore(a, v)` -> void.
    AtomicStore,
    /// `@atomicFetchAdd(a, delta)` -> the previous value.
    AtomicFetchAdd,
    /// `@atomicSwap(a, v)` -> the previous value.
    AtomicSwap,
    /// `@atomicCas(a, expected, new)` -> `?T` (`null` on success, else the actual
    /// witnessed value).
    AtomicCas,
    /// `@wgMake()` -> a `u32` wait-group id.
    WgMake,
    /// `@wgAdd(wg, n)` -> void.
    WgAdd,
    /// `@wgDone(wg)` -> void. Wakes waiters when the counter reaches zero.
    WgDone,
    /// `@wgWait(wg)` -> void. Blocks until the counter reaches zero.
    WgWait,

    // ---- The *Build capability floor (v0.12) ----------------------------
    //
    // These back the bundled `build` module's `Build`/`Artifact`/`Step` methods
    // (see `crates/k2-std/std/build.k2`). They are pure RECORDERS: every one
    // pushes a node/edge into the VM's `BuildGraph` (or reads a driver-seeded
    // `-D` value) and performs NO I/O and NO real allocation — honoring the
    // comptime sandbox (spec §06.1 / §08.6.1). `build(b)` runs on the ordinary VM
    // with a `*Build` capability; afterward `k2c build` reads the graph back.
    /// `@buildStdTarget()` -> the resolved `Target` struct (from `-Dtarget`).
    BuildStdTarget,
    /// `@buildStdOptimize()` -> the `OptimizeMode` enum value (from `-Doptimize`).
    BuildStdOptimize,
    /// `@buildOption(kind, name, desc)` -> `?T`: looks up `name` in the `-D` map,
    /// records the declared option, and returns `null` (→ `orelse`) when absent.
    /// `kind`: 0=bool 1=string 2=int.
    BuildOption,
    /// `b.addLibrary(cfg)` -> the new library artifact's `u32` id. The `cfg` is
    /// the anonymous config struct, read positionally (field 0 = name, field 1 =
    /// root_source).
    BuildAddLibrary,
    /// `b.addExecutable(cfg)` -> the new executable artifact's `u32` id.
    BuildAddExecutable,
    /// `b.addTest(cfg)` -> the new test artifact's `u32` id.
    BuildAddTest,
    /// `@buildArtifactOption(id, name, value)` -> void. Appends a build-option.
    BuildArtifactOption,
    /// `@buildArtifactModule(id, name, mod_id)` -> void. Wires a named module in.
    BuildArtifactModule,
    /// `@buildArtifactModuleSelf(id)` -> a `Module` id for artifact `id`.
    BuildArtifactModuleSelf,
    /// `@buildArtifactForwardArgs(id)` -> void. Flags a run-artifact to forward
    /// `--`-args.
    BuildArtifactForwardArgs,
    /// `@buildAddRun(exe_id)` -> a new `Run` artifact's `u32` id.
    BuildAddRun,
    /// `@buildInstall(id)` -> void. Adds `id` to the install step's deps.
    BuildInstall,
    /// `@buildStep(name, desc)` -> the new step's `u32` id.
    BuildStep,
    /// `@buildStepDependOn(step_id, dep_step_id)` -> void. Adds a DAG edge.
    BuildStepDependOn,
    /// `b.path(rel)` -> the relative path string (identity); the artifact records
    /// it at creation time.
    BuildPath,
    /// `b.fmt(template, args)` -> a formatted `[]const u8`, via the shared format
    /// engine into a fresh string (no build-time I/O).
    BuildFmt,
    /// `target.arch` / `target.os` / `target.abi` -> the corresponding enum field
    /// of the `Target` struct (field 0/1/2), reached on the deferred-typed
    /// `target` value `b.standardTarget()` returns.
    BuildTargetArch,
    BuildTargetOs,
    BuildTargetAbi,
    /// `artifact.step` (a FIELD read, not the `b.step(...)` method) -> a `Step`
    /// handle for the artifact's embedded step, so `&run_exe.step` is a real step
    /// a user step can `dependOn`.
    BuildArtifactStep,

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
    /// `@bitCast` of a **packed struct** into its backing integer:
    /// `dst = OR_i ((a.field[i] & mask(width_i)) << off_i) : to`. The native
    /// backend stores a packed struct as one little-endian integer, so a bitcast
    /// to that integer is exactly this LSB-first bit-packing of the per-field
    /// values; the VM (which stores a packed struct as a `Value::Struct` of
    /// per-field values) reproduces it here so packed-struct `@bitCast` matches
    /// native (spec §02). Compiled from a [`super::Rvalue::Cast`] whose *source*
    /// type is a packed struct (see `compile.rs`).
    PackStruct {
        /// Destination register (the backing integer).
        dst: Reg,
        /// Source register (the packed-struct `Value::Struct`).
        a: Reg,
        /// `(bit_offset, bit_width)` of each field, in declaration order.
        fields: std::rc::Rc<Vec<(u32, u32)>>,
        /// The backing-integer representation (width/signedness of the target).
        to: IntRepr,
    },
    /// `@bitCast` of a backing integer into a **packed struct**: the inverse of
    /// [`Instr::PackStruct`]. `dst.field[i] = extract(a, off_i, width_i)` with
    /// sign-extension for a signed field, mirroring the native
    /// `load_packed_field` shift+mask (spec §02). Compiled from a
    /// [`super::Rvalue::Cast`] whose *target* type is a packed struct.
    UnpackStruct {
        /// Destination register (the packed-struct `Value::Struct`).
        dst: Reg,
        /// Source register (the backing integer).
        a: Reg,
        /// `(bit_offset, bit_width, signed)` and the field's [`IntRepr`] for each
        /// field, in declaration order.
        fields: std::rc::Rc<Vec<(u32, u32, IntRepr)>>,
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
    /// Return the value in `src`, *and* record an error-return-trace frame for
    /// the `try` site identified by `site` (an index into the [`CompiledFn`]'s
    /// `trace_sites`). Emitted only in Debug/ReleaseSafe for a `try`-propagating
    /// return; in ReleaseFast the compiler lowers the same MIR to a plain
    /// [`Instr::Return`], so the trace machinery has zero cost there.
    ReturnErr {
        /// The register holding the returned (error-union) value.
        src: Reg,
        /// Index into [`CompiledFn::trace_sites`] for this propagation point.
        site: u32,
    },
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
    /// The error-return-trace sites referenced by [`Instr::ReturnErr`] in this
    /// function's `code`. Empty in ReleaseFast (the trace machinery is stripped).
    pub trace_sites: Vec<TraceSite>,
}

/// One error-return-trace site: the source location of a `try` that re-throws.
/// Recorded per [`CompiledFn`] and pushed onto the running fiber's trace buffer
/// when the corresponding [`Instr::ReturnErr`] executes.
#[derive(Clone, Debug)]
pub struct TraceSite {
    /// The display name of the function the `try` is in.
    pub fn_name: String,
    /// 1-based source line of the `try` site.
    pub line: u32,
    /// 1-based source column of the `try` site.
    pub col: u32,
}
