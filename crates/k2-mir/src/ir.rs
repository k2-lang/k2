//! The MIR data model: a backend-agnostic, executable mid-level IR.
//!
//! A [`MirProgram`] is a set of *monomorphized* [`MirFunction`]s — one per used
//! `(fn, comptime-arg)` instantiation — sharing the interning [`TypeArena`] so
//! the v0.8 bytecode VM has every layout. Each function is a control-flow graph
//! of [`BasicBlock`]s; each block is a list of [`Statement`]s ending in a
//! [`Terminator`]. The design target is a *typed, explicit, mostly-three-address*
//! IR with **no hidden control flow**: all branches are terminators, all
//! desugaring is done (`defer`/`errdefer`/`try`/`catch`/`orelse`/`for`/labeled
//! `break`/short-circuit `and`/`or` are expanded into blocks and branches),
//! generics are monomorphized, and comptime-known values are inlined as
//! constants.
//!
//! ## The Place / Operand / Rvalue factoring
//!
//! Following Rust's MIR, a [`Place`] is an addressable location (an lvalue: a
//! local plus a chain of [`Proj`]ections), an [`Operand`] is a readable value (a
//! copy of a place, or a constant), and an [`Rvalue`] computes a fresh value.
//! [`Statement::Assign`] writes an `Rvalue` to a `Place`. Every place read is a
//! copy and mutation happens only through `Assign`, so a stack/register VM lowers
//! each node directly with no aliasing surprises.
//!
//! ## The opaque-intrinsic boundary
//!
//! Calls to still-`Deferred` std/sys/build members (`alloc.alloc(...)`,
//! `out.print(...)`, `b.option(...)`) cannot be lowered to a concrete callee —
//! they are the stdlib boundary the VM fills in later. They become
//! [`Rvalue::Intrinsic`] nodes carrying the member path verbatim, so lowering
//! never fails on them.

use std::collections::HashMap;

use k2_resolve::DefId;
use k2_syntax::Span;
use k2_types::{TypeArena, TypeId};

// =========================================================================
//  Identifiers
// =========================================================================

/// A handle into [`MirProgram::funcs`] — one monomorphized function.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct FnId(pub u32);
/// A handle into a [`MirFunction`]'s `blocks`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct BlockId(pub u32);
/// A handle into a [`MirFunction`]'s `locals`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct LocalId(pub u32);
/// A handle into [`MirProgram::consts`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct ConstId(pub u32);

impl FnId {
    /// The underlying index, for `prog.funcs[id.index()]`.
    pub fn index(self) -> usize {
        self.0 as usize
    }
}
impl BlockId {
    /// The underlying index, for `func.blocks[id.index()]`.
    pub fn index(self) -> usize {
        self.0 as usize
    }
}
impl LocalId {
    /// The underlying index, for `func.locals[id.index()]`.
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

// =========================================================================
//  Monomorphization identity
// =========================================================================

/// A monomorphization identity. For a generic function it is the source fn
/// [`DefId`] plus the normalized comptime-argument tuple — reusing the k2-types
/// instantiation identity, where a `type` argument is keyed by its interned
/// [`TypeId`], so `List(u32)` is one key and `List([]const u8)` another. For a
/// non-generic function the `args` vector is empty.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct InstId {
    /// The source function's definition.
    pub fn_def: DefId,
    /// The normalized comptime-argument tuple (empty for a plain function).
    pub args: Vec<InstArgKey>,
}

impl InstId {
    /// A plain (non-generic) instantiation of `fn_def`.
    pub fn plain(fn_def: DefId) -> InstId {
        InstId {
            fn_def,
            args: Vec::new(),
        }
    }
}

/// One normalized comptime argument inside an [`InstId`].
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum InstArgKey {
    /// A `type` argument, keyed by its interned [`TypeId`].
    Type(TypeId),
    /// A comptime integer argument.
    Int(i128),
    /// A comptime bool argument.
    Bool(bool),
    /// A comptime string argument.
    Str(String),
}

// =========================================================================
//  Build mode & diagnostics
// =========================================================================

/// The build mode a program is lowered under. It is the single knob that decides
/// whether the safety-check pass runs: checks are ON for [`BuildMode::Debug`] and
/// [`BuildMode::ReleaseSafe`], and OFF for [`BuildMode::ReleaseFast`] (violated
/// assumptions become undefined behavior, by design, for raw throughput).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum BuildMode {
    /// Cranelift + the full safety toolkit (the default).
    #[default]
    Debug,
    /// Optimized, but the safety checks are kept.
    ReleaseSafe,
    /// Optimized; the safety checks are stripped entirely.
    ReleaseFast,
}

impl BuildMode {
    /// `true` when the safety-check pass should insert bounds/overflow/narrowing/
    /// `unreachable` guards.
    pub fn checks_enabled(self) -> bool {
        !matches!(self, BuildMode::ReleaseFast)
    }
}

/// The severity of a MIR diagnostic.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Severity {
    /// A hard error that gates codegen.
    Error,
    /// An advisory finding.
    Warning,
}

/// A MIR diagnostic, mirroring the shared `{ span, severity, message }` shape so
/// the driver prints it with the existing formatter.
#[derive(Clone, Debug)]
pub struct Diagnostic {
    /// The source span the finding is anchored at.
    pub span: Span,
    /// The severity.
    pub severity: Severity,
    /// The human-readable message.
    pub message: String,
}

impl Diagnostic {
    /// Builds an error-severity diagnostic.
    pub fn error(span: Span, m: impl Into<String>) -> Diagnostic {
        Diagnostic {
            span,
            severity: Severity::Error,
            message: m.into(),
        }
    }

    /// Builds a warning-severity diagnostic.
    pub fn warning(span: Span, m: impl Into<String>) -> Diagnostic {
        Diagnostic {
            span,
            severity: Severity::Warning,
            message: m.into(),
        }
    }

    /// `true` if this diagnostic is an error.
    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}

// =========================================================================
//  Program / function / locals / blocks
// =========================================================================

/// The whole lowered program: one monomorphized function per used
/// `(fn, comptime-arg)` instantiation, plus the shared type/layout arena.
pub struct MirProgram {
    /// The interning type arena, MOVED out of `Typed`. Carries every layout the
    /// VM needs (`arena.get(ty)`, struct/enum/union/fnsig side tables).
    pub arena: TypeArena,
    /// Every monomorphized function, indexed by [`FnId`].
    pub funcs: Vec<MirFunction>,
    /// Stable lookup: an instantiation key -> its [`FnId`] (dedup + recursion).
    pub by_inst: HashMap<InstId, FnId>,
    /// The entry points reached for this build (`main` + every `test`).
    pub entries: Vec<FnId>,
    /// Interned constant data (string-literal bytes, large aggregates) too big to
    /// inline directly as [`Operand`]s.
    pub consts: Vec<ConstData>,
    /// Lowering + leak diagnostics (errors here gate codegen).
    pub diagnostics: Vec<Diagnostic>,
    /// The build mode this program was lowered under (drives check presence).
    pub mode: BuildMode,
    /// The error-tag -> name map, interned during lowering. The v0.8 VM uses it
    /// to implement `@errorName` and to print the name of an error that escapes
    /// `main` (per the hello.k2 docs). Empty for a program with no error values.
    pub err_names: HashMap<ErrTag, String>,
}

impl MirProgram {
    /// `true` if lowering produced no error-severity diagnostics.
    pub fn is_ok(&self) -> bool {
        self.diagnostics.iter().all(|d| !d.is_error())
    }

    /// The total number of basic blocks across every function (a dump summary).
    pub fn block_count(&self) -> usize {
        self.funcs.iter().map(|f| f.blocks.len()).sum()
    }

    /// The total number of inserted safety checks across every function. Counts
    /// both the pre-split [`Statement::Check`] markers and the post-split check
    /// notes (`*_check`), so the count is stable whether or not the check-
    /// splitting pass has run.
    pub fn check_count(&self) -> usize {
        self.funcs
            .iter()
            .flat_map(|f| f.blocks.iter())
            .flat_map(|b| b.stmts.iter())
            .filter(|s| match s {
                Statement::Check(_) => true,
                Statement::Note(n) => n.contains("_check"),
                _ => false,
            })
            .count()
    }
}

/// A piece of interned constant data referenced by a [`Const`].
#[derive(Clone, Debug)]
pub enum ConstData {
    /// The decoded bytes of a string literal (`[]const u8`).
    Bytes(Vec<u8>),
    /// A materialized aggregate constant: its element/field operands in order.
    Aggregate(Vec<Operand>),
}

/// One monomorphized function: typed locals, parameters, and a control-flow graph.
pub struct MirFunction {
    /// This function's id.
    pub id: FnId,
    /// A display name, e.g. `main`, `List(u32).push`, `parseDoubled`.
    pub name: String,
    /// The source `fn` definition (`None` for a synthesized wrapper).
    pub def: Option<DefId>,
    /// The instantiation identity (empty args for a plain function).
    pub inst: InstId,
    /// Parameter locals, in declaration order (a prefix of `locals`). For a
    /// method, slot 0 is the receiver.
    pub params: Vec<LocalId>,
    /// The function's result type (the success type; an error-union return type
    /// is modeled explicitly via the error-union constructors).
    pub ret: TypeId,
    /// Every local/temporary, indexed by `LocalId.0`.
    pub locals: Vec<Local>,
    /// The basic blocks, indexed by `BlockId.0`. `entry` is always `BlockId(0)`.
    pub blocks: Vec<BasicBlock>,
    /// The entry block.
    pub entry: BlockId,
    /// The shared panic/trap block for this fn (created lazily by the
    /// safety-check splitter), if any check or trap targets it.
    pub panic_block: Option<BlockId>,
    /// The `bool` type id, cached so the check-splitting post-pass can build
    /// boolean condition temporaries without the arena. Set during lowering.
    pub bool_ty: Option<TypeId>,
    /// The defining span.
    pub span: Span,
}

impl MirFunction {
    /// Allocates a fresh temporary local of type `ty` and returns its id.
    pub fn new_temp(&mut self, ty: TypeId, span: Span) -> LocalId {
        let id = LocalId(self.locals.len() as u32);
        self.locals.push(Local {
            id,
            ty,
            origin: LocalOrigin::Temp,
            address_taken: false,
            span,
        });
        id
    }

    /// Allocates a fresh `bool` temporary (for the check-splitting pass).
    pub fn new_temp_bool(&mut self) -> LocalId {
        let ty = self.bool_ty.expect("bool_ty must be set");
        let span = self.span;
        self.new_temp(ty, span)
    }

    /// Allocates a fresh empty block and returns its id.
    pub fn new_block(&mut self) -> BlockId {
        let id = BlockId(self.blocks.len() as u32);
        self.blocks.push(BasicBlock {
            id,
            stmts: Vec::new(),
            term: Terminator::Unreachable,
            is_panic: false,
        });
        id
    }

    /// Garbage-collects unreachable basic blocks: any non-entry block with no
    /// predecessor (reachable from no terminator) is dropped, and the survivors
    /// are renumbered into a dense `0..n` range with their ids/terminator targets
    /// rewritten. Lowering can leave a dangling join (e.g. a labeled block whose
    /// every exit is a `break :blk`), which is malformed MIR per the milestone's
    /// "no dangling block" invariant; this pass removes that class.
    pub fn gc_unreachable_blocks(&mut self) {
        // Reachability from the entry over the terminator edges.
        let n = self.blocks.len();
        let mut reachable = vec![false; n];
        let mut stack = vec![self.entry.index()];
        reachable[self.entry.index()] = true;
        while let Some(bi) = stack.pop() {
            for succ in self.blocks[bi].term.successors() {
                let si = succ.index();
                if si < n && !reachable[si] {
                    reachable[si] = true;
                    stack.push(si);
                }
            }
        }
        if reachable.iter().all(|&r| r) {
            return; // nothing to collect
        }
        // Old index -> new index for the surviving blocks (entry stays first).
        let mut remap: Vec<Option<u32>> = vec![None; n];
        let mut next = 0u32;
        for (i, &r) in reachable.iter().enumerate() {
            if r {
                remap[i] = Some(next);
                next += 1;
            }
        }
        let mut new_blocks: Vec<BasicBlock> = Vec::with_capacity(next as usize);
        for (i, mut b) in std::mem::take(&mut self.blocks).into_iter().enumerate() {
            if !reachable[i] {
                continue;
            }
            b.id = BlockId(remap[i].unwrap());
            b.term.remap_blocks(&remap);
            new_blocks.push(b);
        }
        self.blocks = new_blocks;
        self.entry = BlockId(remap[self.entry.index()].unwrap());
        if let Some(pb) = self.panic_block {
            self.panic_block = remap[pb.index()].map(BlockId);
        }
    }
}

impl Terminator {
    /// The block ids this terminator may transfer control to.
    pub fn successors(&self) -> Vec<BlockId> {
        match self {
            Terminator::Goto(t) => vec![*t],
            Terminator::Branch {
                then_bb, else_bb, ..
            } => vec![*then_bb, *else_bb],
            Terminator::Switch {
                targets, default, ..
            } => {
                let mut v: Vec<BlockId> = targets.iter().map(|(_, t)| *t).collect();
                v.push(*default);
                v
            }
            Terminator::Return { .. } | Terminator::Trap { .. } | Terminator::Unreachable => {
                Vec::new()
            }
        }
    }

    /// Rewrites every target block id through `remap` (old index -> new id). Used
    /// by [`MirFunction::gc_unreachable_blocks`] after renumbering survivors.
    fn remap_blocks(&mut self, remap: &[Option<u32>]) {
        let map = |b: &mut BlockId| {
            if let Some(new) = remap[b.index()] {
                *b = BlockId(new);
            }
        };
        match self {
            Terminator::Goto(t) => map(t),
            Terminator::Branch {
                then_bb, else_bb, ..
            } => {
                map(then_bb);
                map(else_bb);
            }
            Terminator::Switch {
                targets, default, ..
            } => {
                for (_, t) in targets {
                    map(t);
                }
                map(default);
            }
            Terminator::Return { .. } | Terminator::Trap { .. } | Terminator::Unreachable => {}
        }
    }
}

/// A typed local slot: a parameter, a source binding, a compiler temporary, or
/// the return-value slot.
#[derive(Clone, Debug)]
pub struct Local {
    /// This local's id.
    pub id: LocalId,
    /// The slot's type.
    pub ty: TypeId,
    /// Where the slot came from.
    pub origin: LocalOrigin,
    /// `true` if the address of this local is ever taken (`&local`). Drives the
    /// escape check and tells the VM this slot needs an addressable home.
    pub address_taken: bool,
    /// The defining span.
    pub span: Span,
}

/// The origin of a [`Local`].
#[derive(Clone, Copy, Debug)]
pub enum LocalOrigin {
    /// A function parameter, carrying its source definition.
    Param(DefId),
    /// A `const`/`var`/capture binding, carrying its source definition.
    Binding(DefId),
    /// A compiler-introduced temporary.
    Temp,
    /// The return-value slot.
    Ret,
}

/// A basic block: a list of statements ending in a single terminator.
pub struct BasicBlock {
    /// This block's id.
    pub id: BlockId,
    /// The straight-line statements, in execution order.
    pub stmts: Vec<Statement>,
    /// The single control-flow exit.
    pub term: Terminator,
    /// `true` if this block is a synthesized panic/trap target.
    pub is_panic: bool,
}

// =========================================================================
//  Places, projections, operands, constants
// =========================================================================

/// An lvalue: a base local plus a chain of projections (deref, field, index, …).
#[derive(Clone, Debug)]
pub struct Place {
    /// The rooted local.
    pub base: LocalId,
    /// The projection chain, applied left-to-right.
    pub proj: Vec<Proj>,
}

impl Place {
    /// A bare place that is just the local (no projections).
    pub fn local(base: LocalId) -> Place {
        Place {
            base,
            proj: Vec::new(),
        }
    }

    /// `true` if this place is a bare local with no projections.
    pub fn is_local(&self) -> bool {
        self.proj.is_empty()
    }

    /// A copy of this place with `p` appended to the projection chain.
    pub fn project(&self, p: Proj) -> Place {
        let mut proj = self.proj.clone();
        proj.push(p);
        Place {
            base: self.base,
            proj,
        }
    }
}

/// A single place projection.
#[derive(Clone, Debug)]
pub enum Proj {
    /// `*p` — follow a pointer. The result type is the pointee.
    Deref,
    /// `.field` by layout index into a struct/union, carrying the field type.
    Field {
        /// The layout index.
        index: u32,
        /// The field's type.
        ty: TypeId,
    },
    /// `s[i]` — index a slice/array by an [`Operand`]. Bounds checks are emitted
    /// as separate [`Statement::Check`]s before the access, never hidden here.
    Index {
        /// The index operand.
        index: Operand,
        /// The element type.
        ty: TypeId,
    },
    /// `.len` / `.ptr` of a slice (the VM reads the fat-pointer half).
    SliceMeta {
        /// Which half.
        which: SliceMeta,
        /// The resulting type (`usize` for `.len`, `*elem` for `.ptr`).
        ty: TypeId,
    },
    /// The success payload inside an error union, or the inner of an optional,
    /// AFTER the discriminant has been checked. Used by desugared `try`/`.?`/
    /// `catch`/`orelse`.
    Payload {
        /// The payload type.
        ty: TypeId,
    },
}

/// Which half of a slice's fat pointer a [`Proj::SliceMeta`] reads.
#[derive(Clone, Copy, Debug)]
pub enum SliceMeta {
    /// The data pointer.
    Ptr,
    /// The length.
    Len,
}

/// A readable value: a copy of a place, or an inlined constant.
#[derive(Clone, Debug)]
pub enum Operand {
    /// Read (copy) the value at a place.
    Copy(Place),
    /// A scalar/aggregate constant inlined at the use site.
    Const(Const),
}

impl Operand {
    /// A bare-local read.
    pub fn local(base: LocalId) -> Operand {
        Operand::Copy(Place::local(base))
    }

    /// Appends every local this operand references (a `Copy` place's root plus any
    /// locals in its `Index` projections) to `out`.
    fn collect_locals(&self, out: &mut Vec<LocalId>) {
        if let Operand::Copy(p) = self {
            p.collect_locals(out);
        }
    }
}

impl Place {
    /// Appends this place's root local and any `Index`-projection locals to `out`.
    fn collect_locals(&self, out: &mut Vec<LocalId>) {
        out.push(self.base);
        for proj in &self.proj {
            if let Proj::Index { index, .. } = proj {
                index.collect_locals(out);
            }
        }
    }
}

impl Rvalue {
    /// Appends every local this rvalue reads (operands, ref'd places) to `out`.
    fn collect_locals(&self, out: &mut Vec<LocalId>) {
        match self {
            Rvalue::Use(o)
            | Rvalue::MakeSome(o, _)
            | Rvalue::MakeOk(o, _)
            | Rvalue::Cast { operand: o, .. }
            | Rvalue::Unary { operand: o, .. }
            | Rvalue::Discriminant { operand: o, .. } => o.collect_locals(out),
            Rvalue::Ref { place, .. } => place.collect_locals(out),
            Rvalue::Binary { lhs, rhs, .. } => {
                lhs.collect_locals(out);
                rhs.collect_locals(out);
            }
            Rvalue::MakeSlice { ptr, len, .. } => {
                ptr.collect_locals(out);
                len.collect_locals(out);
            }
            Rvalue::Aggregate { fields, .. } => {
                for f in fields {
                    f.collect_locals(out);
                }
            }
            Rvalue::Call { args, .. } => {
                for a in args {
                    a.collect_locals(out);
                }
            }
            Rvalue::Intrinsic { path, args, .. } => {
                if let IntrinsicRoot::Value(op) = &path.root {
                    op.collect_locals(out);
                }
                for a in args {
                    a.collect_locals(out);
                }
            }
            Rvalue::MakeNull(_) | Rvalue::MakeErr(_, _) => {}
        }
    }
}

impl Statement {
    /// The locals this statement reads or writes (used by [`MirProgram::verify`]
    /// to catch references to out-of-range locals). Safety-`Check` operands are
    /// not walked here — checks are realized into branches before verification.
    pub fn referenced_locals(&self) -> Vec<LocalId> {
        let mut out = Vec::new();
        match self {
            Statement::Assign { place, rvalue, .. } => {
                place.collect_locals(&mut out);
                rvalue.collect_locals(&mut out);
            }
            Statement::Eval { rvalue, .. } => rvalue.collect_locals(&mut out),
            Statement::StorageLive(l) | Statement::StorageDead(l) => out.push(*l),
            Statement::Check(_) | Statement::Note(_) => {}
        }
        out
    }
}

/// An inlined constant value.
#[derive(Clone, Debug)]
pub enum Const {
    /// A sized or `comptime_int` integer (already folded).
    Int {
        /// The value (working width `i128`).
        value: i128,
        /// The integer type.
        ty: TypeId,
    },
    /// A float, carried as its IEEE-754 bit pattern.
    Float {
        /// The `f64` bit pattern.
        bits: u64,
        /// The float type.
        ty: TypeId,
    },
    /// A `bool`.
    Bool(bool),
    /// `void`.
    Void,
    /// A `[]const u8` string literal whose bytes live in [`MirProgram::consts`].
    Str(ConstId),
    /// An enum value (variant index + enum type).
    EnumVal {
        /// The active variant index.
        variant: u32,
        /// The enum type.
        ty: TypeId,
    },
    /// An error value: its global tag + the (single-member) set type.
    ErrVal {
        /// The error tag.
        tag: ErrTag,
        /// The error-set type.
        ty: TypeId,
    },
    /// The all-zero "empty slice" `&.{}` (ptr=null sentinel, len=0).
    EmptySlice {
        /// The slice type.
        ty: TypeId,
    },
    /// `undefined`: typed but unspecified bits.
    Undef {
        /// The value's type.
        ty: TypeId,
    },
    /// A large/aggregate constant materialized in [`MirProgram::consts`].
    Aggregate {
        /// The interned aggregate id.
        id: ConstId,
        /// The aggregate's type.
        ty: TypeId,
    },
}

/// A globally-stable error tag (spec §6.1.1: a nonzero `u16`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ErrTag(pub u16);

// =========================================================================
//  Rvalues
// =========================================================================

/// A binary operator over scalar operands. This op set is CHECK-FREE; overflow/
/// div-zero checks are emitted as separate [`SafetyCheck`]s around it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BinOp {
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/`
    Div,
    /// `%`
    Rem,
    /// `&`
    BitAnd,
    /// `|`
    BitOr,
    /// `^`
    BitXor,
    /// `<<`
    Shl,
    /// `>>`
    Shr,
    /// `==`
    Eq,
    /// `!=`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
}

/// A unary operator over a scalar operand.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UnOp {
    /// `-x` (arithmetic negation).
    Neg,
    /// `~x` (bitwise complement).
    BitNot,
    /// `not x` (boolean negation).
    Not,
}

/// Which half of an aggregate a [`Rvalue::Aggregate`] builds.
#[derive(Clone, Copy, Debug)]
pub enum AggKind {
    /// A struct literal (fields in layout order).
    Struct,
    /// An array literal.
    Array,
    /// A positional tuple.
    Tuple,
}

/// What a [`Rvalue::Discriminant`] reads.
#[derive(Clone, Copy, Debug)]
pub enum DiscrKind {
    /// An optional: `true` when it is `null`.
    Optional,
    /// An error union: `true` when it holds an error.
    ErrorUnion,
    /// A tagged union: yields the active variant index.
    Union,
}

/// An explicit numeric/pointer conversion.
#[derive(Clone, Copy, Debug)]
pub enum CastKind {
    /// `@as` widening / representation change (lossless): no check.
    Widen,
    /// `@intCast`: narrowing; gets a paired `NarrowFits` check in safe builds.
    IntNarrow,
    /// `@ptrCast`: pointer reinterpretation; never checked.
    PtrReinterpret,
    /// Integer -> float (lossless by representation).
    IntToFloat,
    /// Float -> integer (truncating).
    FloatToInt,
}

/// The right-hand side computations. This is the executable heart of the IR.
#[derive(Clone, Debug)]
pub enum Rvalue {
    /// Move/copy an operand into the destination.
    Use(Operand),

    /// Pure binary arithmetic / comparison / bitwise / shift.
    Binary {
        /// The operator.
        op: BinOp,
        /// The left operand.
        lhs: Operand,
        /// The right operand.
        rhs: Operand,
        /// The result type.
        ty: TypeId,
    },
    /// Pure unary negation / complement.
    Unary {
        /// The operator.
        op: UnOp,
        /// The operand.
        operand: Operand,
        /// The result type.
        ty: TypeId,
    },

    /// `&place` — address-of. `is_const` mirrors `*T` vs `*const T`.
    Ref {
        /// The place whose address is taken.
        place: Place,
        /// `true` for a `*const T` reference.
        is_const: bool,
        /// The resulting pointer type.
        ty: TypeId,
    },

    /// An explicit numeric/pointer conversion.
    Cast {
        /// The conversion kind.
        kind: CastKind,
        /// The source operand.
        operand: Operand,
        /// The target type.
        ty: TypeId,
    },

    /// Build a slice `{ptr,len}` from a base pointer + length (array->slice,
    /// sub-slice `base[lo..hi]`, `&arr`). Bounds already checked upstream.
    MakeSlice {
        /// The data pointer.
        ptr: Operand,
        /// The element offset to add to `ptr` (the sub-slice low bound `lo`; `0`
        /// for a whole-array/whole-slice view). Carried explicitly because the IR
        /// has no pointer-arithmetic rvalue, and a `base[lo..hi]` slice must start
        /// at element `lo`, not at the base.
        offset: Operand,
        /// The length.
        len: Operand,
        /// The slice type.
        ty: TypeId,
    },

    /// Optional construction `Some(v)`.
    MakeSome(Operand, TypeId),
    /// Optional construction `null`.
    MakeNull(TypeId),

    /// Error-union construction `Ok(v)`.
    MakeOk(Operand, TypeId),
    /// Error-union construction `Err(tag)`.
    MakeErr(ErrTag, TypeId),

    /// Read the discriminant of an optional/error-union/tagged-union (drives the
    /// desugared branches; never hidden).
    Discriminant {
        /// The aggregate operand.
        operand: Operand,
        /// What discriminant to read.
        kind: DiscrKind,
    },

    /// Aggregate construction: struct/array/tuple literal from field operands in
    /// layout order.
    Aggregate {
        /// Which aggregate kind.
        kind: AggKind,
        /// The field/element operands, in order.
        fields: Vec<Operand>,
        /// The aggregate type.
        ty: TypeId,
    },

    /// A direct, fully-monomorphized call.
    Call {
        /// The callee function.
        func: FnId,
        /// The argument operands, in order.
        args: Vec<Operand>,
        /// The result type.
        ty: TypeId,
    },

    /// An OPAQUE intrinsic call for a still-`Deferred` std/sys/build member. The
    /// VM dispatches on `path` at runtime. This is how `alloc.alloc`,
    /// `out.print`, `gpa.deinit`, `b.option` survive lowering without failing.
    Intrinsic {
        /// The member path captured verbatim from the source chain.
        path: IntrinsicPath,
        /// The argument operands, in order.
        args: Vec<Operand>,
        /// The result type (often `Deferred`/opaque/an error union).
        ty: TypeId,
    },
}

/// The member path of an intrinsic, captured verbatim from the source chain so
/// the v0.8 VM can bind it to a Rust-implemented stdlib op.
#[derive(Clone, Debug)]
pub struct IntrinsicPath {
    /// The root: how the chain started (so the VM knows the capability root).
    pub root: IntrinsicRoot,
    /// The dotted member names after the root, e.g. `["io", "stdout"]` or
    /// `["alloc"]` for `alloc.alloc`. Captured from the AST `Field` chain.
    pub members: Vec<String>,
    /// `true` if the final member was *called* (vs. a field read).
    pub is_call: bool,
}

impl IntrinsicPath {
    /// A dotted rendering of the path's members (the dump form), e.g.
    /// `alloc.alloc` or `io.stdout`.
    pub fn dotted(&self) -> String {
        self.members.join(".")
    }

    /// The final member name (the operation the VM dispatches), if any.
    pub fn last(&self) -> Option<&str> {
        self.members.last().map(|s| s.as_str())
    }
}

/// How an intrinsic chain began.
#[derive(Clone, Debug)]
pub enum IntrinsicRoot {
    /// A module namespace, e.g. `@import("std")` / `@import("build")`.
    Module(DefId),
    /// A value whose type is `Deferred`/`Opaque` (`sys: *System`, `b: *Build`,
    /// and any value derived from a `Deferred` member, e.g. `alloc`, `out`).
    Value(Box<Operand>),
    /// A compile-time builtin that stays opaque to the front end but is a runtime
    /// op (e.g. `@errorName`).
    Builtin(String),
}

// =========================================================================
//  Statements & safety checks
// =========================================================================

/// One straight-line statement in a basic block.
#[derive(Clone, Debug)]
pub enum Statement {
    /// `place = rvalue;`
    Assign {
        /// The destination place.
        place: Place,
        /// The computed right-hand side.
        rvalue: Rvalue,
        /// The source span.
        span: Span,
    },
    /// Evaluate an rvalue for effect, discarding its result (e.g. a void
    /// intrinsic call).
    Eval {
        /// The rvalue to evaluate.
        rvalue: Rvalue,
        /// The source span.
        span: Span,
    },
    /// An inserted safety check. The block-splitter realizes it as a branch to
    /// the function's shared panic block on failure (see [`crate::checks`]).
    Check(SafetyCheck),
    /// A storage marker: a local's live range begins. Advisory.
    StorageLive(LocalId),
    /// A storage marker: a local's live range ends. Advisory.
    StorageDead(LocalId),
    /// A no-op carrying a comment for the dump (e.g. `defer #2 (LIFO)`).
    Note(String),
}

/// An inserted safety check: a condition that, on failure, branches to the
/// panic block.
#[derive(Clone, Debug)]
pub struct SafetyCheck {
    /// What the check verifies.
    pub kind: CheckKind,
    /// The source span the failure is reported at.
    pub span: Span,
}

/// The arithmetic op a [`CheckKind::AddOverflow`] guards.
#[derive(Clone, Copy, Debug)]
pub enum ArithOp {
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
}

/// What a [`SafetyCheck`] verifies.
#[derive(Clone, Debug)]
pub enum CheckKind {
    /// `0 <= index < len` (slice/array index or sub-slice).
    Bounds {
        /// The index operand.
        index: Operand,
        /// The length operand.
        len: Operand,
    },
    /// `lo <= hi <= len` (sub-slice `base[lo..hi]`).
    SliceRange {
        /// The low bound.
        lo: Operand,
        /// The high bound.
        hi: Operand,
        /// The base length.
        len: Operand,
    },
    /// `+`,`-`,`*` did not overflow the sized type.
    AddOverflow {
        /// Which op.
        op: ArithOp,
        /// The left operand.
        a: Operand,
        /// The right operand.
        b: Operand,
        /// The operand type.
        ty: TypeId,
    },
    /// `b != 0` for `/` and `%`.
    DivByZero {
        /// The divisor.
        b: Operand,
        /// The divisor's integer type (for the `b != 0` comparison).
        ty: TypeId,
    },
    /// Signed `/` and `%` do not overflow: the result of `type-MIN / -1`
    /// (mathematically `-MIN`, which does not fit the type) is rejected. The
    /// failure case is exactly `a == type-MIN && b == -1`.
    DivOverflow {
        /// The dividend.
        a: Operand,
        /// The divisor.
        b: Operand,
        /// The operands' signed integer type.
        ty: TypeId,
    },
    /// Negating `MIN` of a signed type does not overflow.
    NegOverflow {
        /// The operand.
        a: Operand,
        /// The operand type.
        ty: TypeId,
    },
    /// `@intCast`: the value fits the narrower target width.
    NarrowFits {
        /// The source value.
        value: Operand,
        /// The target type.
        ty: TypeId,
    },
    /// Multiple `for` operands must have equal length (spec §5.4).
    LenEq {
        /// The first length.
        a: Operand,
        /// The second length.
        b: Operand,
    },
    /// `unreachable` was reached (an always-fail trap in safe builds).
    Unreachable,
}

// =========================================================================
//  Terminators
// =========================================================================

/// The single control-flow exit of a basic block.
#[derive(Clone, Debug)]
pub enum Terminator {
    /// Unconditional jump.
    Goto(BlockId),
    /// Two-way branch on a `bool` operand.
    Branch {
        /// The condition.
        cond: Operand,
        /// The target when `cond` is true.
        then_bb: BlockId,
        /// The target when `cond` is false.
        else_bb: BlockId,
    },
    /// N-way branch on an integer operand (enum tag / switch scrutinee).
    Switch {
        /// The integer scrutinee.
        scrutinee: Operand,
        /// The `(value, target)` arms.
        targets: Vec<(i128, BlockId)>,
        /// The fall-through target.
        default: BlockId,
    },
    /// Return the function result. `value` is provided for VMs that prefer an
    /// explicit operand; the value also lives in the return slot.
    Return {
        /// The returned value.
        value: Operand,
    },
    /// Diverge into the panic/trap block, carrying the reason for the message.
    Trap {
        /// Why the trap fires.
        reason: TrapReason,
    },
    /// Statically unreachable (post-`noreturn`, e.g. after `@panic`). In safe
    /// builds the lowerer turns an `unreachable` *expression* into a `Trap`; this
    /// terminator marks genuinely-dead fall-through the VM may assume is unused.
    Unreachable,
}

/// Why a [`Terminator::Trap`] fires (drives the panic message).
#[derive(Clone, Copy, Debug)]
pub enum TrapReason {
    /// An out-of-bounds index/slice.
    Bounds,
    /// An integer arithmetic overflow.
    Overflow,
    /// A division/remainder by zero.
    DivByZero,
    /// A signed negation overflow.
    NegOverflow,
    /// A narrowing cast that lost information.
    NarrowLoss,
    /// A lockstep-`for` length mismatch.
    LenMismatch,
    /// An `unreachable` expression was reached.
    Unreachable,
    /// An explicit `@panic` / `.?`-on-null.
    Panic,
}
