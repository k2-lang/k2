//! The k2 [`Type`] representation and its nominal side tables.
//!
//! Types are interned into a [`TypeArena`](crate::arena::TypeArena) and named by
//! a small [`TypeId`] handle, so structural type identity is a `u32` compare and
//! a per-occurrence type map is `Copy`-valued and cheap. This module owns the
//! data definitions (the [`Type`] enum, its scalar sub-enums, and the
//! `*Info`/`*Sig` records the arena keeps in parallel `Vec`s); the arena owns
//! interning, the well-known cache, equality, coercion, and rendering.
//!
//! ## Why composite variants hold `TypeId`s, not `Box<Type>`
//!
//! Every composite variant ([`Type::Optional`], [`Type::Pointer`], …) stores an
//! interned [`TypeId`] for its payload rather than a boxed `Type`. That keeps
//! `Type` `Clone + Hash + Eq` (so it can be the key of the interning map) and
//! makes interning structural and recursive-by-construction: a sub-type is
//! always interned before the type that mentions it. It also matches the
//! existing crate style, where `DefId`/`ScopeId`/`ModuleId` are `u32` newtypes.
//!
//! ## The comptime-deferral boundary
//!
//! Two distinguished bottom types live here. [`Type::Deferred`] is the
//! *comptime-unknown* type, produced ONLY at the four genuine comptime
//! boundaries (a generic/`comptime`-parameter call, a reflection builtin, an
//! annotation/initializer that is itself deferred, or member access on a
//! deferred/`anytype`/module base). [`Type::Error`] marks a node for which a
//! diagnostic was ALREADY emitted. Both are *bottom-compatible* — compatible
//! with every expectation in either direction — which is the single mechanism
//! that suppresses dependent cascades. The difference is intent: `Deferred`
//! means "legitimately unknown until comptime"; `Error` means "already
//! reported, do not report again". Neither ever silences the checking of the
//! concrete code *around* it.

use k2_resolve::{DefId, ModuleId};
use k2_syntax::Span;

/// A handle into the [`TypeArena`](crate::arena::TypeArena). Structural-equality
/// of two interned types is exactly `a == b`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct TypeId(pub u32);

impl TypeId {
    /// The underlying index, for arena lookups (`arena.get(id)`).
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// The bit width of an integer type. [`IntBits::Fixed`] covers the whole
/// `u<N>`/`i<N>` family (including the canonical `8/16/32/64/128` widths);
/// `Usize`/`Isize` are the two target-width integers used for slice/array
/// lengths and indices.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum IntBits {
    /// A fixed bit width `N` in `0..=65535` (`u8`, `i32`, `u1`, `i7`, …).
    Fixed(u16),
    /// `usize` — the target's unsigned pointer-width integer.
    Usize,
    /// `isize` — the target's signed pointer-width integer.
    Isize,
}

/// The length of an array type. A non-`Known` length defers an exhaustive
/// element count (an inferred `[_]T`, or a comptime-computed length).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ArrayLen {
    /// A statically known element count (`[5]u8`).
    Known(u64),
    /// An inferred length (`[_]T`).
    Inferred,
    /// A comptime-computed length whose value is not statically known here
    /// (`[serializedSize(Packet)]u8`).
    Deferred,
}

/// A reference to the error set inside an [`Type::ErrorUnion`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ErrSetRef {
    /// A concrete, closed error set (interned by sorted member list).
    Set(ErrSetId),
    /// `anyerror` — the open superset of every set.
    Any,
    /// An inferred set (`!T`): the body computes `E`. Treated conservatively.
    Inferred,
    /// A comptime/unevaluated set (e.g. an unresolved `||` merge).
    Deferred,
}

/// A `u32` handle into the arena's interned error-set table.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct ErrSetId(pub u32);
/// A `u32` handle into the arena's struct-info table.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct StructId(pub u32);
/// A `u32` handle into the arena's enum-info table.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct EnumId(pub u32);
/// A `u32` handle into the arena's union-info table.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct UnionId(pub u32);
/// A `u32` handle into the arena's function-signature table.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct FnSigId(pub u32);
/// An opaque predeclared marker type (`System`, `Allocator`, `Build`,
/// `anyopaque`-as-namespace). Member access on it yields [`Type::Deferred`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct OpaqueId(pub u32);

/// A k2 type. Composite variants hold interned [`TypeId`]s (or side-table ids),
/// never `Box<Type>`, so `Type` is `Clone + Hash + Eq` and interning is
/// structural and recursive-by-construction.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum Type {
    // ---- Scalars / primitives ------------------------------------------
    /// A signed/unsigned integer of a given width (`i32`, `u8`, `usize`, `u1`).
    Int { signed: bool, bits: IntBits },
    /// A float of width `16 | 32 | 64 | 128`.
    Float { bits: u16 },
    /// `bool`.
    Bool,
    /// `void`.
    Void,
    /// `type`: the type of types. A *value* of this type IS a type.
    TypeType,
    /// `noreturn`: the type of an expression that never returns.
    NoReturn,
    /// `anyopaque`: an unsized type, only usable behind a pointer.
    AnyOpaque,
    /// `comptime_int`: an arbitrary-precision compile-time integer.
    ComptimeInt,
    /// `comptime_float`: an arbitrary-precision compile-time float.
    ComptimeFloat,

    // ---- Errors --------------------------------------------------------
    /// `anyerror`: the open superset of every error set.
    AnyError,
    /// A concrete, closed error set, interned by its sorted member-name list.
    ErrorSet(ErrSetId),
    /// An error union `E!T` (or inferred `!T` when `err == Inferred`).
    ErrorUnion { err: ErrSetRef, ok: TypeId },

    // ---- Postfix composites --------------------------------------------
    /// `?T` — optional.
    Optional(TypeId),
    /// `*T` / `*const T` — single-item pointer.
    Pointer { is_const: bool, pointee: TypeId },
    /// `[]T` / `[]const T` — slice.
    Slice { is_const: bool, elem: TypeId },
    /// `[N]T` — array (`[_]T` yields `len == Inferred`).
    Array { len: ArrayLen, elem: TypeId },
    /// `@Vector(N, T)` — a fixed-length SIMD vector of `N` numeric elements
    /// (spec §02). `elem` must be an integer/float/bool; `len` is comptime-known.
    /// Laid out as `N` contiguous elements, alignment a power of two capped at 16.
    Vector { len: u32, elem: TypeId },

    // ---- Nominal aggregates --------------------------------------------
    /// A `struct {...}` type (nominal; identified by its declaring node).
    Struct(StructId),
    /// An `enum {...}` type (nominal).
    Enum(EnumId),
    /// A `union {...}` type (nominal).
    Union(UnionId),

    // ---- Callables / namespaces ----------------------------------------
    /// A function (or fn-pointer) type, `fn(params) Ret`.
    Fn(FnSigId),
    /// An `@import` namespace (opaque in v0.5).
    Module(ModuleId),
    /// A predeclared opaque/capability type (`System`/`Allocator`/`Build`).
    Opaque(OpaqueId),

    // ---- The comptime-deferral boundary --------------------------------
    /// comptime-unknown: produced ONLY at genuine comptime boundaries. Compatible
    /// with every expectation; never silences concrete checks.
    Deferred,
    /// `anytype`: an inferred parameter marker. Bottom-compatible like
    /// [`Type::Deferred`] for coercion and member access, but a distinct
    /// sentinel so v0.5 refuses to monomorphize it and the dump labels it
    /// `anytype`.
    AnyType,
    /// A node for which a diagnostic was ALREADY emitted. Suppresses cascades.
    Error,
}

/// How a `struct`'s fields are laid out in memory.
///
/// [`StructLayout::Auto`] is the default field-ordered layout with natural
/// per-field alignment and padding. [`StructLayout::Extern`] is the C-ABI
/// `extern struct` layout (currently identical to `Auto`, but a distinct flag so
/// FFI-representability checks can require it). [`StructLayout::Packed`] is a
/// gap-free, least-significant-bit-first bit packing into a single backing
/// unsigned integer (spec §02): each field occupies exactly its *bit width* and
/// `@sizeOf`/`@bitSizeOf` reflect the packed total (see
/// [`reflect::layout_depth`](crate::reflect)).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum StructLayout {
    /// The default: declaration-ordered fields with natural alignment/padding.
    Auto,
    /// `extern struct {...}` — the stable C-ABI layout.
    Extern,
    /// `packed struct {...}` — LSB-first gap-free bit packing.
    Packed,
}

/// One field of a [`StructInfo`].
#[derive(Clone, Debug)]
pub struct FieldInfo {
    /// The field name.
    pub name: String,
    /// The field's type.
    pub ty: TypeId,
    /// `true` if the field has a default value (`= v`).
    pub has_default: bool,
    /// `true` for a `comptime` struct field.
    pub is_comptime: bool,
    /// An explicit `align(N)` clause on the field (already parsed), evaluated to
    /// its byte count. Raises (never lowers) the field's alignment and so the
    /// containing struct's alignment (spec §03). `None` => natural alignment.
    pub align: Option<u64>,
    /// For a field of a [`StructLayout::Packed`] struct: the field's first bit's
    /// offset into the backing integer (LSB-first). `None` for a non-packed
    /// struct's field. Filled by [`crate::check::Checker::eval_struct`].
    pub bit_offset: Option<u32>,
    /// For a packed-struct field: the field's exact bit width. `None` otherwise.
    pub bit_width: Option<u32>,
    /// Source span of the field.
    pub span: Span,
}

/// A nested `const`/`var`/`fn` declaration of a container (a method or a
/// member constant), recorded so member access can resolve it.
#[derive(Clone, Debug)]
pub struct MemberDecl {
    /// The declaration name.
    pub name: String,
    /// `true` if the member is `pub`.
    pub is_pub: bool,
    /// The member's defining definition.
    pub def: DefId,
    /// The member's value/fn type (a [`Type::Fn`] for a method).
    pub ty: TypeId,
}

/// The body of a `struct` type.
#[derive(Clone, Debug)]
pub struct StructInfo {
    /// The defining `const Name = struct{...}` item, if the struct is named.
    pub def: Option<DefId>,
    /// A display name (the binding's name, or `struct` for an anonymous one).
    pub name: String,
    /// The defining span (used as the nominal identity key and for dumps).
    pub span: Span,
    /// How the struct's fields are laid out (`Auto`/`Extern`/`Packed`).
    pub layout: StructLayout,
    /// The fields in declaration (layout) order.
    pub fields: Vec<FieldInfo>,
    /// Nested const/var/fn members, by name.
    pub decls: Vec<MemberDecl>,
}

/// Maps the parser's `(is_extern, is_packed)` qualifier flags to a
/// [`StructLayout`]. `packed` wins over `extern` (the parser already forbids
/// `packed extern struct`, so at most one is set).
pub fn struct_layout_of(is_extern: bool, is_packed: bool) -> StructLayout {
    if is_packed {
        StructLayout::Packed
    } else if is_extern {
        StructLayout::Extern
    } else {
        StructLayout::Auto
    }
}

impl StructInfo {
    /// `true` for an `extern struct` (the FFI-representable layout). Kept as a
    /// helper so the ~handful of `is_extern` read sites do not churn after the
    /// `is_extern: bool` field became the richer [`StructLayout`] enum.
    pub fn is_extern(&self) -> bool {
        matches!(self.layout, StructLayout::Extern)
    }

    /// `true` for a `packed struct` (LSB-first bit packing).
    pub fn is_packed(&self) -> bool {
        matches!(self.layout, StructLayout::Packed)
    }
}

/// One variant of an [`EnumInfo`].
#[derive(Clone, Debug)]
pub struct EnumVariant {
    /// The variant name.
    pub name: String,
    /// The variant's integer tag value: an explicit `= N`, or the previous
    /// variant's value + 1 (the first defaults to 0). This is what `@intFromEnum`
    /// yields and what a `switch` prong matches — distinct from the variant's
    /// *declaration index* (used for reflection ordering), which differ only when
    /// explicit values are given.
    pub value: i128,
    /// Source span of the variant.
    pub span: Span,
}

/// The body of an `enum` type.
#[derive(Clone, Debug)]
pub struct EnumInfo {
    /// The defining item, if named.
    pub def: Option<DefId>,
    /// A display name.
    pub name: String,
    /// The defining span (nominal identity key).
    pub span: Span,
    /// The backing integer type (explicit, or an inferred unsigned).
    pub tag: TypeId,
    /// The variants in declaration order.
    pub variants: Vec<EnumVariant>,
    /// Nested const/var/fn members.
    pub decls: Vec<MemberDecl>,
}

/// One variant of a [`UnionInfo`].
#[derive(Clone, Debug)]
pub struct UnionVariant {
    /// The variant name.
    pub name: String,
    /// The variant payload type (`void` for a payload-less variant).
    pub payload: TypeId,
    /// Source span of the variant.
    pub span: Span,
}

/// How a `union`'s tag is determined.
#[derive(Clone, Copy, Debug)]
pub enum UnionTagKind {
    /// A bare `union {...}` — no tag.
    None,
    /// `union(enum) {...}` — an inferred tag enum.
    Inferred,
    /// `union(TagType) {...}` — an explicit tag type.
    Typed,
}

/// The body of a `union` type.
#[derive(Clone, Debug)]
pub struct UnionInfo {
    /// The defining item, if named.
    pub def: Option<DefId>,
    /// A display name.
    pub name: String,
    /// The defining span (nominal identity key).
    pub span: Span,
    /// How the tag is determined.
    pub tag: UnionTagKind,
    /// The variants in declaration order.
    pub variants: Vec<UnionVariant>,
    /// Nested const/var/fn members.
    pub decls: Vec<MemberDecl>,
}

/// An interned error set: its member names, sorted and deduplicated, so two
/// `error{A, B}` written anywhere intern to the same [`ErrSetId`].
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct ErrSetInfo {
    /// The member names, sorted and deduplicated.
    pub members: Vec<String>,
}

/// One parameter of a [`FnSig`].
#[derive(Clone, Debug)]
pub struct ParamInfo {
    /// The parameter name (`""`/`_` for unnamed/discard).
    pub name: String,
    /// The parameter's type.
    pub ty: TypeId,
    /// `true` if the parameter is `comptime`-qualified.
    pub is_comptime: bool,
    /// Source span of the parameter.
    pub span: Span,
}

/// A function signature: parameters, varargs flag, return type, plus the two
/// flags that drive call-result deferral (a `comptime` or `anytype` parameter
/// makes a call a generic instantiation, whose result type is
/// [`Type::Deferred`]).
#[derive(Clone, Debug)]
pub struct FnSig {
    /// The parameters in declaration order.
    pub params: Vec<ParamInfo>,
    /// `true` if the function takes trailing varargs.
    pub is_varargs: bool,
    /// The declared return type.
    pub ret: TypeId,
    /// `true` if any parameter is `comptime` (call -> Deferred result).
    pub has_comptime_param: bool,
    /// `true` if any parameter is `anytype` (call -> Deferred result).
    pub has_anytype_param: bool,
}

/// The C-interop linkage of a `fn` (v0.19). [`ExternKind::Extern`] is a
/// body-less declaration of a C function the k2 program *calls* (an undefined
/// external symbol the system linker resolves against libc); [`ExternKind::Export`]
/// exposes a defined k2 function to C under a stable, un-mangled C symbol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExternKind {
    /// `extern fn name(...) Ret;` — an undefined C symbol the program calls.
    Extern,
    /// `export fn name(...) Ret { ... }` — a defined global C symbol.
    Export,
}

/// The recorded FFI metadata for one `extern`/`export` function, keyed by its
/// [`DefId`] in [`crate::Typed::extern_fns`]. The `abi_name` is the un-mangled C
/// symbol (the k2 fn name in v0.19); `varargs` marks a `...`-variadic extern (so
/// the backend zeroes `AL` before a call per the SysV ABI).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExternInfo {
    /// Whether this is an `extern` (undefined) or `export` (defined) C symbol.
    pub kind: ExternKind,
    /// The un-mangled C symbol name (defaults to the k2 function name).
    pub abi_name: String,
    /// `true` for a `...`-variadic extern declaration (printf-class).
    pub varargs: bool,
}

/// The bit position, width, and signedness of a [`StructLayout::Packed`] field,
/// resolved once so both backends do an identical shift+mask without re-deriving
/// the layout. `off`/`width` are bit positions into the backing integer (LSB
/// first); `signed` drives sign-extension on read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PackedField {
    /// The field's first bit's offset into the backing integer (LSB-first).
    pub off: u32,
    /// The field's exact bit width.
    pub width: u32,
    /// `true` if the field's type is signed (sign-extend on read).
    pub signed: bool,
}

/// What a previously-`DeferredMember` occurrence resolved to in the type layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemberRes {
    /// A struct field at the given index.
    Field(u32),
    /// A [`StructLayout::Packed`] struct field: its index plus the bit
    /// descriptor so the MIR can attach it to `Proj::Field` for shift+mask
    /// access on both backends.
    PackedField(u32, PackedField),
    /// A nested member declaration (method/const) of the given definition.
    Decl(DefId),
    /// An enum/union variant at the given index.
    Variant(u32),
    /// A slice/array built-in member (`.len`, `.ptr`).
    BuiltinField,
    /// An error-set member (an `error.Name` literal or `.Name` arm).
    ErrorMember,
    /// Resolved against a deferred/module/anytype base — no concrete target.
    Deferred,
}
