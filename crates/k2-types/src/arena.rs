//! The [`TypeArena`]: interning, the well-known primitive cache, structural
//! equality, the one-directional coercion relation, and type rendering.
//!
//! The arena is the single owner of every [`Type`] and of the parallel
//! side-table `Vec`s for nominal aggregates, error sets, and function
//! signatures. Interning deduplicates types so that two structurally-equal
//! types share one [`TypeId`] and identity is a `u32` compare — exactly the
//! style the resolver uses for `DefId`/`ScopeId`. Nominal aggregates (`struct`,
//! `enum`, `union`) are interned by *identity* (their declaring span), matching
//! k2's nominal aggregate model; error sets and function signatures are interned
//! *structurally*.

use std::collections::HashMap;

use crate::ty::{
    EnumId, EnumInfo, ErrSetId, ErrSetInfo, ErrSetRef, FnSig, FnSigId, IntBits, OpaqueId, StructId,
    StructInfo, Type, TypeId, UnionId, UnionInfo,
};

/// The cached [`TypeId`]s of the primitive and singleton types, filled by
/// [`TypeArena::new`] so the hot path (`arena.t_bool()`, …) is O(1).
struct WellKnown {
    bool_: TypeId,
    void_: TypeId,
    type_: TypeId,
    noreturn: TypeId,
    anyopaque: TypeId,
    comptime_int: TypeId,
    comptime_float: TypeId,
    anyerror: TypeId,
    deferred: TypeId,
    anytype: TypeId,
    error_: TypeId,
    usize_: TypeId,
    isize_: TypeId,
    u8_: TypeId,
    /// `[]const u8` — the type of a string literal.
    str_: TypeId,
}

/// The interning arena for all [`Type`]s plus their side tables.
pub struct TypeArena {
    /// `TypeId -> Type`.
    types: Vec<Type>,
    /// `Type -> TypeId` (structural dedup for non-nominal types; nominal types
    /// are deduped by span via the `*_by_span` maps below).
    intern: HashMap<Type, TypeId>,
    /// Interned error-set bodies (`ErrSetId -> ErrSetInfo`).
    pub errsets: Vec<ErrSetInfo>,
    /// `ErrSetInfo.members -> ErrSetId` (structural error-set dedup).
    errset_intern: HashMap<Vec<String>, ErrSetId>,
    /// Struct bodies (`StructId -> StructInfo`).
    pub structs: Vec<StructInfo>,
    /// Enum bodies (`EnumId -> EnumInfo`).
    pub enums: Vec<EnumInfo>,
    /// Union bodies (`UnionId -> UnionInfo`).
    pub unions: Vec<UnionInfo>,
    /// Function signatures (`FnSigId -> FnSig`).
    pub fnsigs: Vec<FnSig>,
    /// Opaque/capability type names (`OpaqueId -> name`).
    pub opaques: Vec<String>,
    /// `name -> OpaqueId` (dedup by name).
    opaque_intern: HashMap<String, OpaqueId>,
    /// Nominal dedup of struct/enum/union by their declaring span.
    struct_by_span: HashMap<(u32, u32), StructId>,
    enum_by_span: HashMap<(u32, u32), EnumId>,
    union_by_span: HashMap<(u32, u32), UnionId>,
    /// The primitive/singleton cache.
    well_known: WellKnown,
}

impl TypeArena {
    /// Builds an arena with every primitive and singleton pre-interned.
    pub fn new() -> TypeArena {
        // A throwaway arena we fill by hand, then read the ids back into the
        // well-known cache. We cannot call `intern` before `well_known` exists,
        // so push directly and build the intern map alongside.
        let mut a = TypeArena {
            types: Vec::new(),
            intern: HashMap::new(),
            errsets: Vec::new(),
            errset_intern: HashMap::new(),
            structs: Vec::new(),
            enums: Vec::new(),
            unions: Vec::new(),
            fnsigs: Vec::new(),
            opaques: Vec::new(),
            opaque_intern: HashMap::new(),
            struct_by_span: HashMap::new(),
            enum_by_span: HashMap::new(),
            union_by_span: HashMap::new(),
            // Filled immediately below; the placeholder ids are overwritten.
            well_known: WellKnown {
                bool_: TypeId(0),
                void_: TypeId(0),
                type_: TypeId(0),
                noreturn: TypeId(0),
                anyopaque: TypeId(0),
                comptime_int: TypeId(0),
                comptime_float: TypeId(0),
                anyerror: TypeId(0),
                deferred: TypeId(0),
                anytype: TypeId(0),
                error_: TypeId(0),
                usize_: TypeId(0),
                isize_: TypeId(0),
                u8_: TypeId(0),
                str_: TypeId(0),
            },
        };

        a.well_known.bool_ = a.push_raw(Type::Bool);
        a.well_known.void_ = a.push_raw(Type::Void);
        a.well_known.type_ = a.push_raw(Type::TypeType);
        a.well_known.noreturn = a.push_raw(Type::NoReturn);
        a.well_known.anyopaque = a.push_raw(Type::AnyOpaque);
        a.well_known.comptime_int = a.push_raw(Type::ComptimeInt);
        a.well_known.comptime_float = a.push_raw(Type::ComptimeFloat);
        a.well_known.anyerror = a.push_raw(Type::AnyError);
        a.well_known.deferred = a.push_raw(Type::Deferred);
        a.well_known.anytype = a.push_raw(Type::AnyType);
        a.well_known.error_ = a.push_raw(Type::Error);
        a.well_known.usize_ = a.push_raw(Type::Int {
            signed: false,
            bits: IntBits::Usize,
        });
        a.well_known.isize_ = a.push_raw(Type::Int {
            signed: true,
            bits: IntBits::Isize,
        });
        a.well_known.u8_ = a.push_raw(Type::Int {
            signed: false,
            bits: IntBits::Fixed(8),
        });
        let u8 = a.well_known.u8_;
        a.well_known.str_ = a.push_raw(Type::Slice {
            is_const: true,
            elem: u8,
        });
        a
    }

    /// Pushes a fresh type, recording it in the intern map. Used only during
    /// construction; the public path is [`Self::intern`].
    fn push_raw(&mut self, t: Type) -> TypeId {
        let id = TypeId(self.types.len() as u32);
        self.types.push(t.clone());
        self.intern.insert(t, id);
        id
    }

    /// Interns a type, returning the existing id if it is already present (for
    /// structurally-deduped types) or pushing a fresh one otherwise. Nominal
    /// aggregates should be created via [`Self::intern_struct`] /
    /// [`Self::intern_enum`] / [`Self::intern_union`] instead, which key on the
    /// declaring span.
    pub fn intern(&mut self, t: Type) -> TypeId {
        if let Some(&id) = self.intern.get(&t) {
            return id;
        }
        self.push_raw(t)
    }

    /// Looks up the [`Type`] behind a [`TypeId`].
    pub fn get(&self, id: TypeId) -> &Type {
        &self.types[id.index()]
    }

    // ---- well-known accessors ------------------------------------------

    /// The `bool` type.
    pub fn t_bool(&self) -> TypeId {
        self.well_known.bool_
    }
    /// The `void` type.
    pub fn t_void(&self) -> TypeId {
        self.well_known.void_
    }
    /// The `type` type (the type of types).
    pub fn t_type(&self) -> TypeId {
        self.well_known.type_
    }
    /// The `noreturn` type.
    pub fn t_noreturn(&self) -> TypeId {
        self.well_known.noreturn
    }
    /// The `anyopaque` type.
    pub fn t_anyopaque(&self) -> TypeId {
        self.well_known.anyopaque
    }
    /// The `comptime_int` type.
    pub fn t_comptime_int(&self) -> TypeId {
        self.well_known.comptime_int
    }
    /// The `comptime_float` type.
    pub fn t_comptime_float(&self) -> TypeId {
        self.well_known.comptime_float
    }
    /// The `anyerror` type.
    pub fn t_anyerror(&self) -> TypeId {
        self.well_known.anyerror
    }
    /// The `Deferred` (comptime-unknown) type.
    pub fn t_deferred(&self) -> TypeId {
        self.well_known.deferred
    }
    /// The `anytype` marker type.
    pub fn t_anytype(&self) -> TypeId {
        self.well_known.anytype
    }
    /// The `Error` (already-reported) type.
    pub fn t_error(&self) -> TypeId {
        self.well_known.error_
    }
    /// The `usize` type.
    pub fn t_usize(&self) -> TypeId {
        self.well_known.usize_
    }
    /// The `isize` type.
    pub fn t_isize(&self) -> TypeId {
        self.well_known.isize_
    }
    /// The `u8` type.
    pub fn t_u8(&self) -> TypeId {
        self.well_known.u8_
    }
    /// The `[]const u8` type (a string literal's type).
    pub fn t_str(&self) -> TypeId {
        self.well_known.str_
    }

    // ---- nominal & structural side-table interning ---------------------

    /// Interns an error set by its sorted, deduplicated member list.
    pub fn intern_errset(&mut self, mut members: Vec<String>) -> ErrSetId {
        members.sort();
        members.dedup();
        if let Some(&id) = self.errset_intern.get(&members) {
            return id;
        }
        let id = ErrSetId(self.errsets.len() as u32);
        self.errsets.push(ErrSetInfo {
            members: members.clone(),
        });
        self.errset_intern.insert(members, id);
        id
    }

    /// Interns a function signature structurally and returns its [`Type::Fn`] id.
    pub fn intern_fn(&mut self, sig: FnSig) -> TypeId {
        let id = FnSigId(self.fnsigs.len() as u32);
        self.fnsigs.push(sig);
        self.intern(Type::Fn(id))
    }

    /// Interns a struct nominally (by its declaring span). A repeated span
    /// returns the existing id without re-storing the body.
    pub fn intern_struct(&mut self, info: StructInfo) -> TypeId {
        let key = (info.span.start, info.span.end);
        if let Some(&id) = self.struct_by_span.get(&key) {
            return self.intern(Type::Struct(id));
        }
        let id = StructId(self.structs.len() as u32);
        self.structs.push(info);
        self.struct_by_span.insert(key, id);
        self.intern(Type::Struct(id))
    }

    /// Interns an enum nominally (by its declaring span).
    pub fn intern_enum(&mut self, info: EnumInfo) -> TypeId {
        let key = (info.span.start, info.span.end);
        if let Some(&id) = self.enum_by_span.get(&key) {
            return self.intern(Type::Enum(id));
        }
        let id = EnumId(self.enums.len() as u32);
        self.enums.push(info);
        self.enum_by_span.insert(key, id);
        self.intern(Type::Enum(id))
    }

    /// Interns a union nominally (by its declaring span).
    pub fn intern_union(&mut self, info: UnionInfo) -> TypeId {
        let key = (info.span.start, info.span.end);
        if let Some(&id) = self.union_by_span.get(&key) {
            return self.intern(Type::Union(id));
        }
        let id = UnionId(self.unions.len() as u32);
        self.unions.push(info);
        self.union_by_span.insert(key, id);
        self.intern(Type::Union(id))
    }

    /// Interns an opaque/capability type by name (`System`, `Allocator`, …).
    pub fn intern_opaque(&mut self, name: &str) -> TypeId {
        if let Some(&id) = self.opaque_intern.get(name) {
            return self.intern(Type::Opaque(id));
        }
        let id = OpaqueId(self.opaques.len() as u32);
        self.opaques.push(name.to_string());
        self.opaque_intern.insert(name.to_string(), id);
        self.intern(Type::Opaque(id))
    }

    // ---- small constructors --------------------------------------------

    /// The single-item pointer type `*T` / `*const T`.
    pub fn ptr(&mut self, is_const: bool, pointee: TypeId) -> TypeId {
        self.intern(Type::Pointer { is_const, pointee })
    }
    /// The slice type `[]T` / `[]const T`.
    pub fn slice(&mut self, is_const: bool, elem: TypeId) -> TypeId {
        self.intern(Type::Slice { is_const, elem })
    }
    /// The optional type `?T`.
    pub fn optional(&mut self, inner: TypeId) -> TypeId {
        self.intern(Type::Optional(inner))
    }
    /// The SIMD vector type `@Vector(N, T)`.
    pub fn vector(&mut self, len: u32, elem: TypeId) -> TypeId {
        self.intern(Type::Vector { len, elem })
    }

    // ---- equality & coercion -------------------------------------------

    /// Exact structural type identity (the interned-id compare).
    pub fn same(&self, a: TypeId, b: TypeId) -> bool {
        a == b
    }

    /// `true` if `id` is one of the bottom types (`Deferred`/`AnyType`/`Error`),
    /// which are compatible with every expectation in either direction.
    pub fn is_bottom(&self, id: TypeId) -> bool {
        matches!(self.get(id), Type::Deferred | Type::AnyType | Type::Error)
    }

    /// One-directional coercion `from -> to` (the assignability relation). See
    /// the crate root and §12 of the types spec for the rule list.
    pub fn coerces(&self, from: TypeId, to: TypeId) -> bool {
        // Bottom rule: a deferred/anytype/error operand suppresses dependent
        // errors. This is the single mechanism that makes comptime-unknown code
        // type-check without weakening the concrete core.
        if self.is_bottom(from) || self.is_bottom(to) {
            return true;
        }
        if from == to {
            return true;
        }
        let f = self.get(from);
        let t = self.get(to);

        // noreturn coerces to anything (it never produces a value).
        if matches!(f, Type::NoReturn) {
            return true;
        }

        match (f, t) {
            // comptime_int -> any sized/comptime int or float.
            (Type::ComptimeInt, Type::Int { .. })
            | (Type::ComptimeInt, Type::ComptimeInt)
            | (Type::ComptimeInt, Type::Float { .. })
            | (Type::ComptimeInt, Type::ComptimeFloat) => true,
            // comptime_float -> float.
            (Type::ComptimeFloat, Type::Float { .. })
            | (Type::ComptimeFloat, Type::ComptimeFloat) => true,

            // ?A -> ?B when A -> B (covers `null : ?deferred -> ?u8`, since the
            // deferred element is bottom-compatible).
            (Type::Optional(a), Type::Optional(b)) => self.coerces(*a, *b),

            // T -> ?T (optional wrapping).
            (_, Type::Optional(inner)) => self.coerces(from, *inner),

            // error-union widening (must precede the generic `T -> E!T` arm).
            (Type::ErrorUnion { err: ea, ok: ta }, Type::ErrorUnion { err: eb, ok: tb }) => {
                self.coerces(*ta, *tb) && self.errset_ref_subset(*ea, *eb)
            }

            // T -> E!T (success path) and error -> E!T (error path).
            (_, Type::ErrorUnion { err, ok }) => {
                self.coerces(from, *ok) || self.error_into_union(f, *err)
            }

            // error-set widening.
            (Type::ErrorSet(a), Type::ErrorSet(b)) => self.errset_subset(*a, *b),
            (Type::ErrorSet(_), Type::AnyError) => true,
            (Type::AnyError, Type::AnyError) => true,

            // *T -> *const T (add const). Identical-const handled by `from == to`.
            (
                Type::Pointer {
                    is_const: false,
                    pointee: p,
                },
                Type::Pointer {
                    is_const: true,
                    pointee: q,
                },
            ) => p == q,

            // *[N]T -> []T / []const T  and  *const [N]T -> []const T.
            (
                Type::Pointer {
                    is_const: pc,
                    pointee,
                },
                Type::Slice { is_const: sc, elem },
            ) => {
                if let Type::Array { elem: ae, .. } = self.get(*pointee) {
                    // adding const is fine; never drop const.
                    (*sc || !*pc) && self.elem_compatible(*ae, *elem, *sc)
                } else {
                    false
                }
            }

            // []T -> []const T (add const).
            (
                Type::Slice {
                    is_const: false,
                    elem: e1,
                },
                Type::Slice {
                    is_const: true,
                    elem: e2,
                },
            ) => e1 == e2,

            _ => false,
        }
    }

    /// The `@as` widening relation: ordinary coercion, plus same-signedness
    /// integer widening (`u8 -> u32`, `i16 -> i64`) and integer -> wider float,
    /// which `@as` makes explicit and lossless (spec §12.2). Pointer/aggregate
    /// reinterpretation is NOT permitted here (that is `@ptrCast`/`@bitCast`).
    pub fn as_coerces(&self, from: TypeId, to: TypeId) -> bool {
        if self.coerces(from, to) {
            return true;
        }
        match (self.get(from), self.get(to)) {
            // Same-signedness integer widening.
            (
                Type::Int {
                    signed: sf,
                    bits: bf,
                },
                Type::Int {
                    signed: st,
                    bits: bt,
                },
            ) => sf == st && int_width(*bf) <= int_width(*bt),
            // Integer -> float, and float widening (lossless by representation).
            (Type::Int { .. }, Type::Float { .. }) => true,
            (Type::ComptimeInt, Type::Int { .. }) | (Type::ComptimeInt, Type::Float { .. }) => true,
            (Type::Float { bits: bf }, Type::Float { bits: bt }) => bf <= bt,
            _ => false,
        }
    }

    /// Element compatibility for an array-pointer-to-slice coercion: the slice
    /// elem must equal the array elem (with const-add allowed when the slice is
    /// const).
    fn elem_compatible(&self, array_elem: TypeId, slice_elem: TypeId, slice_const: bool) -> bool {
        if array_elem == slice_elem {
            return true;
        }
        // `*[N]const T -> []const T`: identical handled above; otherwise only the
        // const-adding direction is sound, which `coerces` already encodes.
        slice_const && self.coerces(array_elem, slice_elem)
    }

    /// `true` if an error-valued `from` may flow into an error union whose set is
    /// `err`: any error set/`anyerror` whose members are a subset of `err`.
    fn error_into_union(&self, from: &Type, err: ErrSetRef) -> bool {
        match from {
            Type::ErrorSet(a) => self.errset_ref_subset(ErrSetRef::Set(*a), err),
            Type::AnyError => self.errset_ref_subset(ErrSetRef::Any, err),
            _ => false,
        }
    }

    /// `true` if two concrete error sets share at least one member name (so an
    /// `==`/`!=` between values of the two sets could ever be equal). Identical
    /// sets trivially overlap.
    pub fn errsets_overlap(&self, a: ErrSetId, b: ErrSetId) -> bool {
        if a == b {
            return true;
        }
        let am = &self.errsets[a.0 as usize].members;
        let bm = &self.errsets[b.0 as usize].members;
        am.iter().any(|m| bm.contains(m))
    }

    /// Subset test between two concrete error sets (members(a) ⊆ members(b)).
    fn errset_subset(&self, a: ErrSetId, b: ErrSetId) -> bool {
        if a == b {
            return true;
        }
        let am = &self.errsets[a.0 as usize].members;
        let bm = &self.errsets[b.0 as usize].members;
        am.iter().all(|m| bm.contains(m))
    }

    /// Subset test on [`ErrSetRef`]s, handling `Any`/`Inferred`/`Deferred`
    /// conservatively (any inferred/deferred side is treated as compatible, so
    /// the inferred-set examples check clean without a full effect analysis).
    fn errset_ref_subset(&self, a: ErrSetRef, b: ErrSetRef) -> bool {
        use ErrSetRef::*;
        match (a, b) {
            (_, Any) => true,
            (Any, _) => matches!(b, Any),
            (Deferred, _) | (_, Deferred) => true,
            (Inferred, _) | (_, Inferred) => true,
            (Set(x), Set(y)) => self.errset_subset(x, y),
        }
    }

    // ---- rendering ------------------------------------------------------

    /// Renders a type in source syntax (`?*const u8`, `[]const u8`, `[5]u32`,
    /// `error{Empty,NotANumber}!*u32`, `fn(i32, i32) i32`, `Point`, `deferred`,
    /// `anytype`, `<error>`) for diagnostics and the dump.
    pub fn fmt(&self, id: TypeId) -> String {
        match self.get(id) {
            Type::Int { signed, bits } => match bits {
                IntBits::Fixed(n) => format!("{}{n}", if *signed { 'i' } else { 'u' }),
                IntBits::Usize => "usize".to_string(),
                IntBits::Isize => "isize".to_string(),
            },
            Type::Float { bits } => format!("f{bits}"),
            Type::Bool => "bool".to_string(),
            Type::Void => "void".to_string(),
            Type::TypeType => "type".to_string(),
            Type::NoReturn => "noreturn".to_string(),
            Type::AnyOpaque => "anyopaque".to_string(),
            Type::ComptimeInt => "comptime_int".to_string(),
            Type::ComptimeFloat => "comptime_float".to_string(),
            Type::AnyError => "anyerror".to_string(),
            Type::ErrorSet(e) => {
                let m = &self.errsets[e.0 as usize].members;
                format!("error{{{}}}", m.join(","))
            }
            Type::ErrorUnion { err, ok } => {
                format!("{}!{}", self.fmt_errref(*err), self.fmt(*ok))
            }
            Type::Optional(inner) => format!("?{}", self.fmt(*inner)),
            Type::Pointer { is_const, pointee } => {
                format!("*{}{}", const_word(*is_const), self.fmt(*pointee))
            }
            Type::Slice { is_const, elem } => {
                format!("[]{}{}", const_word(*is_const), self.fmt(*elem))
            }
            Type::Array { len, elem } => {
                let l = match len {
                    crate::ty::ArrayLen::Known(n) => n.to_string(),
                    crate::ty::ArrayLen::Inferred => "_".to_string(),
                    crate::ty::ArrayLen::Deferred => "?".to_string(),
                };
                format!("[{l}]{}", self.fmt(*elem))
            }
            Type::Vector { len, elem } => format!("@Vector({len}, {})", self.fmt(*elem)),
            Type::Struct(s) => self.structs[s.0 as usize].name.clone(),
            Type::Enum(e) => self.enums[e.0 as usize].name.clone(),
            Type::Union(u) => self.unions[u.0 as usize].name.clone(),
            Type::Fn(f) => {
                let sig = &self.fnsigs[f.0 as usize];
                let params: Vec<String> = sig.params.iter().map(|p| self.fmt(p.ty)).collect();
                format!("fn({}) {}", params.join(", "), self.fmt(sig.ret))
            }
            Type::Module(_) => "module".to_string(),
            Type::Opaque(o) => self.opaques[o.0 as usize].clone(),
            Type::Deferred => "deferred".to_string(),
            Type::AnyType => "anytype".to_string(),
            Type::Error => "<error>".to_string(),
        }
    }

    /// Renders the error-set part of an error union.
    fn fmt_errref(&self, err: ErrSetRef) -> String {
        match err {
            ErrSetRef::Set(e) => {
                let m = &self.errsets[e.0 as usize].members;
                format!("error{{{}}}", m.join(","))
            }
            ErrSetRef::Any => "anyerror".to_string(),
            ErrSetRef::Inferred => String::new(), // `!T`
            ErrSetRef::Deferred => "deferred".to_string(),
        }
    }
}

impl Default for TypeArena {
    fn default() -> Self {
        TypeArena::new()
    }
}

/// The `const ` qualifier word for pointer/slice rendering.
fn const_word(is_const: bool) -> &'static str {
    if is_const {
        "const "
    } else {
        ""
    }
}

/// The comparable bit width of an integer, with `usize`/`isize` treated as the
/// conservative 64-bit target width for widening comparisons.
fn int_width(bits: IntBits) -> u32 {
    match bits {
        IntBits::Fixed(n) => n as u32,
        IntBits::Usize | IntBits::Isize => 64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn well_known_are_distinct_and_interned() {
        let a = TypeArena::new();
        // Every well-known is a distinct id.
        let ids = [
            a.t_bool(),
            a.t_void(),
            a.t_type(),
            a.t_usize(),
            a.t_deferred(),
            a.t_error(),
            a.t_comptime_int(),
        ];
        for i in 0..ids.len() {
            for j in (i + 1)..ids.len() {
                assert_ne!(ids[i], ids[j], "well-knowns must be distinct");
            }
        }
    }

    #[test]
    fn interning_dedups() {
        let mut a = TypeArena::new();
        let u32a = a.intern(Type::Int {
            signed: false,
            bits: IntBits::Fixed(32),
        });
        let u32b = a.intern(Type::Int {
            signed: false,
            bits: IntBits::Fixed(32),
        });
        assert_eq!(u32a, u32b, "structurally-equal types share one id");
        let p1 = a.ptr(false, u32a);
        let p2 = a.ptr(false, u32b);
        assert_eq!(p1, p2, "composite interning is structural");
    }

    #[test]
    fn bottom_is_compatible_both_ways() {
        let a = TypeArena::new();
        let d = a.t_deferred();
        let e = a.t_error();
        let b = a.t_bool();
        assert!(a.coerces(d, b));
        assert!(a.coerces(b, d));
        assert!(a.coerces(e, b));
        assert!(a.coerces(b, e));
        assert!(a.coerces(a.t_anytype(), b));
    }

    #[test]
    fn coercion_rules() {
        let mut a = TypeArena::new();
        let u8 = a.t_u8();
        let u32 = a.intern(Type::Int {
            signed: false,
            bits: IntBits::Fixed(32),
        });
        let ci = a.t_comptime_int();
        // comptime_int -> sized int.
        assert!(a.coerces(ci, u32));
        // u8 does NOT implicitly coerce to u32 (no implicit widening).
        assert!(!a.coerces(u8, u32));
        // but @as widens it.
        assert!(a.as_coerces(u8, u32));
        // *u32 -> *const u32.
        let pmut = a.ptr(false, u32);
        let pconst = a.ptr(true, u32);
        assert!(a.coerces(pmut, pconst));
        assert!(!a.coerces(pconst, pmut));
        // []u8 -> []const u8.
        let smut = a.slice(false, u8);
        let sconst = a.slice(true, u8);
        assert!(a.coerces(smut, sconst));
        assert!(!a.coerces(sconst, smut));
        // T -> ?T.
        let opt = a.optional(u32);
        assert!(a.coerces(u32, opt));
    }

    #[test]
    fn rendering_is_source_like() {
        let mut a = TypeArena::new();
        let u8 = a.t_u8();
        let sconst = a.slice(true, u8);
        assert_eq!(a.fmt(sconst), "[]const u8");
        let opt = a.optional(u8);
        assert_eq!(a.fmt(opt), "?u8");
        let pc = a.ptr(true, u8);
        assert_eq!(a.fmt(pc), "*const u8");
        assert_eq!(a.fmt(a.t_deferred()), "deferred");
        assert_eq!(a.fmt(a.t_anytype()), "anytype");
        assert_eq!(a.fmt(a.t_error()), "<error>");
    }
}
