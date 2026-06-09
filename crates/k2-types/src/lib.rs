//! # k2-types — the type system and bidirectional checker for k2
//!
//! k2 — *Kardashev Type II*: total control over the machine, with zero waste.
//!
//! This crate is the **type layer (v0.5)** of the k2 front-end. It consumes the
//! [`k2_syntax`] AST and the [`k2_resolve`] `Resolved` side-table and produces a
//! [`Typed`] result: a [`TypeId`] per expression occurrence, one function
//! signature per `fn`, a resolved member for each position the resolver left as
//! `DeferredMember`, the final type bound to each value definition, and the
//! type diagnostics. Like the resolver, it builds no new tree — it is a typed
//! side-table over the existing AST, keyed back by source span.
//!
//! ## Sound on the concrete core, conservative at comptime boundaries
//!
//! In k2 (like Zig) full semantic analysis is intertwined with comptime
//! evaluation — generics are `fn(comptime T: type) type`, and `@typeInfo`/`@Type`
//! manipulate types as values. The comptime *engine* is the next milestone
//! (v0.6). This crate therefore delivers two things:
//!
//! 1. a complete [`Type`] representation able to denote every form the language
//!    needs (see [`ty`]); and
//! 2. a bidirectional checker (see [`check`]) that is **sound on the concrete,
//!    non-generic core** — it catches assigning the wrong type to a typed
//!    binding, wrong-typed/ wrong-arity calls, non-`bool` conditions, operators
//!    on incompatible operands, wrong returns, `try` outside an error union,
//!    dereferencing a non-pointer, `.?` on a non-optional, indexing a
//!    non-array, accessing a missing struct field, calling a non-callable,
//!    non-exhaustive/duplicate switch arms, and integer-literal range errors —
//!    and **conservative at genuine comptime boundaries**, where it produces the
//!    distinguished [`Type::Deferred`] (comptime-unknown) type.
//!
//! `Deferred` is produced ONLY at the four genuine comptime boundaries: a call
//! to a function with a `comptime`/`anytype` parameter (generic instantiation),
//! a reflection/type-producing builtin (`@typeInfo`/`@Type`/`@field`/…), an
//! annotation or initializer that is itself deferred, and member access whose
//! base is deferred/`anytype`/a module namespace. A `Deferred` operand is
//! compatible with any expectation, which suppresses *dependent* errors — but it
//! never silences the checking of the concrete code around it. A node that was
//! *already reported* gets the separate [`Type::Error`] type, which likewise
//! suppresses cascades without re-reporting.
//!
//! ## Entry point
//!
//! [`check_file`] type-checks a single, already-resolved [`SourceFile`].

mod arena;
mod builtins;
mod check;
mod coerce;
mod comptime;
mod diag;
mod dump;
mod eval;
mod exhaust;
mod expr;
mod generic;
mod member;
mod reflect;
mod ty;
mod value;

use std::collections::HashMap;

pub use arena::TypeArena;
pub use diag::{Diagnostic, Severity};
pub use dump::{dump_signatures, dump_types};
pub use ty::{
    ArrayLen, EnumInfo, ErrSetInfo, ErrSetRef, ExternInfo, ExternKind, FieldInfo, FnSig, IntBits,
    MemberDecl, MemberRes, PackedField, ParamInfo, StructInfo, StructLayout, Type, TypeId,
    UnionInfo, UnionTagKind, UnionVariant,
};

use k2_resolve::{DefId, Resolved};
use k2_syntax::{SourceFile, Span};

pub use check::Checker;

/// The typed result for one file: the type arena, types per occurrence, resolved
/// members, function signatures, final binding types, and diagnostics.
pub struct Typed {
    /// The interning arena holding every [`Type`] and its side tables.
    pub arena: TypeArena,
    /// The inferred type of each expression occurrence, keyed by
    /// `(span.start, span.end)`.
    pub types: HashMap<(u32, u32), TypeId>,
    /// The resolved member/field/variant for each previously-`DeferredMember`
    /// position, keyed by the member occurrence span.
    pub members: HashMap<(u32, u32), MemberRes>,
    /// Per-instantiation member resolutions, keyed by `(enclosing instantiated
    /// struct TypeId, member occurrence span)`. The MIR lowerer consults this
    /// FIRST when lowering a method body it knows belongs to a specific generic
    /// instantiation, so a comptime-type-param member dispatch (`Context.lessThan`
    /// inside `Sorter(T, Asc)` vs `Sorter(T, Desc)`) resolves to the correct,
    /// instantiation-specific target instead of whichever instantiation the
    /// span-keyed [`members`] table happened to record last.
    pub inst_members: HashMap<(TypeId, (u32, u32)), MemberRes>,
    /// The type bound to each value definition (const/var/param/local/capture),
    /// and the [`Type::Fn`] type of each `fn`/method definition.
    pub binding_types: HashMap<DefId, TypeId>,
    /// The TYPE denoted by each type-denoting const (`const S = Sorter(T, …)` /
    /// `const P = struct {…}`), keyed by its [`DefId`]. The MIR lowerer consults
    /// this so a member call on a stored type alias (`S.binarySearch(…)`) resolves
    /// as an ASSOCIATED call (no implicit receiver), like the inline form.
    pub item_types: HashMap<DefId, TypeId>,
    /// Spans whose expression *value is a `type`*: a type-returning generic call
    /// (`List(u32)`) maps to the instantiated aggregate [`TypeId`]. The MIR
    /// lowerer reuses this as the monomorphization key for an associated call
    /// `List(u32).init(...)` and for a `[serializedSize(Packet)]u8` array length.
    pub type_valued_spans: HashMap<(u32, u32), TypeId>,
    /// The comptime-known integer value at an expression span (e.g. a `@sizeOf`
    /// or `serializedSize(T)` occurrence), so the MIR lowerer can inline a
    /// comptime array length / folded const as a literal instead of emitting
    /// comptime-only code.
    pub comptime_span_ints: HashMap<(u32, u32), i128>,
    /// The comptime-folded BYTE STRING at an expression span (a `++` string concat),
    /// which the MIR lowerer materializes as a `Const::Str` instead of `undef`.
    pub comptime_span_strs: HashMap<(u32, u32), Vec<u8>>,
    /// The compile-time-known `i128` value of each `const` binding that folded
    /// to a `comptime_int`, so a `const N = serializedSize(T)` use lowers to a
    /// literal rather than to a runtime call.
    pub comptime_int_values: HashMap<DefId, i128>,
    /// The C-interop linkage of each `extern`/`export` function, keyed by its
    /// [`DefId`] (v0.19). The MIR lowerer reads this to mark a callee as an
    /// undefined external symbol (`extern`) or a defined global C symbol
    /// (`export`), and to drive the variadic `AL`-zeroing for a printf-class call.
    pub extern_fns: HashMap<DefId, ty::ExternInfo>,
    /// Every diagnostic produced, in roughly source order.
    pub diagnostics: Vec<Diagnostic>,
}

impl Typed {
    /// `true` if type-checking produced no error-severity diagnostics.
    pub fn is_ok(&self) -> bool {
        self.diagnostics.iter().all(|d| !d.is_error())
    }

    /// An iterator over just the error-severity diagnostics.
    pub fn errors(&self) -> impl Iterator<Item = &Diagnostic> {
        self.diagnostics.iter().filter(|d| d.is_error())
    }

    /// The inferred type recorded at `span`, if any.
    pub fn type_at(&self, span: Span) -> Option<TypeId> {
        self.types.get(&(span.start, span.end)).copied()
    }
}

/// Type-checks a single, already-resolved file.
///
/// The caller must have parsed and resolved `file` with no errors; this function
/// trusts the [`Resolved`] side-table for every name lookup and never
/// re-resolves names. Member/field/enum/error positions the resolver recorded as
/// `DeferredMember` are resolved here against the now-known base types.
pub fn check_file(file: &SourceFile, resolved: &Resolved) -> Typed {
    Checker::new(resolved).run(file)
}
