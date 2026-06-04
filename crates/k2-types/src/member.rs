//! Member, field, enum-literal, and error-literal resolution against concrete
//! types — the work the resolver recorded as `DeferredMember` and left for the
//! type layer, completed here now that base types are known.
//!
//! On a concrete `struct`/`enum`/`union`/slice/array base, a missing member is a
//! real error (the "field access of a non-existent field" check). On a
//! `module`/`anytype`/`deferred` base, member access yields [`Type::Deferred`]
//! with no diagnostic — this is exactly what lets `std.heap.X`, `out.print(...)`
//! (where `out: anytype`), and `info.Struct.fields` type-check without a false
//! positive.

use k2_resolve::Resolution;
use k2_syntax::{Expr, Span};

use crate::ty::{MemberRes, Type, TypeId};

impl crate::check::Checker<'_> {
    /// Synthesizes the type of a `base.field` access and records which member it
    /// resolved to.
    pub(crate) fn synth_field(&mut self, base: &Expr, field: &str, span: Span) -> TypeId {
        let bt = self.synth(base);
        // Auto-deref one pointer layer: `p.x` on `p: *T` accesses `T`'s member.
        let base_ty = match self.arena.get(bt).clone() {
            Type::Pointer { pointee, .. } => pointee,
            _ => bt,
        };

        // Namespace access on a *type-valued* base (`std.heap`, `std.ArrayList`,
        // `std.heap.GeneralPurposeAllocator`): the base denotes a `struct`/`enum`/
        // `union` *type* (an embedded `std` module, a nested const namespace),
        // even though the base expression's *value* type is `type`. Resolve the
        // member against that denoted aggregate's decls so `std.heap.X.init`
        // monomorphizes as a real associated call, not an opaque intrinsic. This
        // is what makes the bundled std a real, compiled module rather than an
        // opaque namespace.
        if matches!(self.arena.get(base_ty), Type::TypeType) {
            if let Some(denoted) = self.denoted_aggregate_of(base) {
                if let Some(ty) = self.member_of_aggregate(denoted, field, span) {
                    // If this member is *itself* a type-valued decl (a nested
                    // namespace or a non-generic nested struct type), record this
                    // access span as type-valued so a further `.member` or an
                    // associated `.init(...)` call resolves against it too.
                    if let Some(def) = self.member_decl_def(denoted, field) {
                        if let Some(&inner) = self.item_types.get(&def) {
                            self.type_valued_spans.insert((span.start, span.end), inner);
                        }
                    }
                    return ty;
                }
            }
        }

        match self.arena.get(base_ty).clone() {
            Type::Struct(id) => {
                let info = &self.arena.structs[id.0 as usize];
                if let Some((idx, f)) = info
                    .fields
                    .iter()
                    .enumerate()
                    .find(|(_, f)| f.name == field)
                {
                    let ty = f.ty;
                    self.record_member(span, MemberRes::Field(idx as u32));
                    return ty;
                }
                if let Some(d) = info.decls.iter().find(|d| d.name == field) {
                    let ty = d.ty;
                    let def = d.def;
                    self.record_member(span, MemberRes::Decl(def));
                    return ty;
                }
                let sname = info.name.clone();
                self.error(span, format!("no field `{field}` on struct `{sname}`"));
                self.record_member(span, MemberRes::Deferred);
                self.arena.t_error()
            }
            Type::Enum(id) => {
                let info = &self.arena.enums[id.0 as usize];
                if let Some(d) = info.decls.iter().find(|d| d.name == field) {
                    let ty = d.ty;
                    let def = d.def;
                    self.record_member(span, MemberRes::Decl(def));
                    return ty;
                }
                // `.Variant` accessed as `Enum.Variant` yields the enum value.
                if let Some((idx, _)) = info
                    .variants
                    .iter()
                    .enumerate()
                    .find(|(_, v)| v.name == field)
                {
                    self.record_member(span, MemberRes::Variant(idx as u32));
                    return base_ty;
                }
                let ename = info.name.clone();
                self.error(span, format!("no member `{field}` on enum `{ename}`"));
                self.record_member(span, MemberRes::Deferred);
                self.arena.t_error()
            }
            Type::Union(id) => {
                let info = &self.arena.unions[id.0 as usize];
                if let Some((idx, v)) = info
                    .variants
                    .iter()
                    .enumerate()
                    .find(|(_, v)| v.name == field)
                {
                    let ty = v.payload;
                    self.record_member(span, MemberRes::Variant(idx as u32));
                    return ty;
                }
                if let Some(d) = info.decls.iter().find(|d| d.name == field) {
                    let ty = d.ty;
                    let def = d.def;
                    self.record_member(span, MemberRes::Decl(def));
                    return ty;
                }
                let uname = info.name.clone();
                self.error(span, format!("no member `{field}` on union `{uname}`"));
                self.record_member(span, MemberRes::Deferred);
                self.arena.t_error()
            }
            Type::Slice { is_const, elem } => match field {
                "len" => {
                    self.record_member(span, MemberRes::BuiltinField);
                    self.arena.t_usize()
                }
                "ptr" => {
                    self.record_member(span, MemberRes::BuiltinField);
                    self.arena.ptr(is_const, elem)
                }
                _ => {
                    self.error(span, format!("no field `{field}` on slice"));
                    self.record_member(span, MemberRes::Deferred);
                    self.arena.t_error()
                }
            },
            Type::Array { .. } => match field {
                "len" => {
                    self.record_member(span, MemberRes::BuiltinField);
                    self.arena.t_usize()
                }
                _ => {
                    self.error(span, format!("no field `{field}` on array"));
                    self.record_member(span, MemberRes::Deferred);
                    self.arena.t_error()
                }
            },
            // A module / deferred / anytype / opaque base: member is comptime,
            // resolved by v0.6. No diagnostic; the access is Deferred.
            Type::Module(_)
            | Type::Deferred
            | Type::AnyType
            | Type::Error
            | Type::Opaque(_)
            | Type::TypeType => {
                self.record_member(span, MemberRes::Deferred);
                self.arena.t_deferred()
            }
            _ => {
                // A field access on a concrete non-aggregate (e.g. `i32`): only
                // report if the base is a genuinely concrete scalar.
                self.error(
                    span,
                    format!("type `{}` has no field `{field}`", self.arena.fmt(base_ty)),
                );
                self.record_member(span, MemberRes::Deferred);
                self.arena.t_error()
            }
        }
    }

    /// Recovers the denoted aggregate (`struct`/`enum`/`union`) type of a
    /// *type-valued* base expression — a reference to a type-valued item or a
    /// chain of namespace accesses into nested type-valued decls. Returns `None`
    /// for a base that does not denote a known aggregate (so the caller falls
    /// back to the ordinary, value-typed member path / Deferred).
    ///
    /// This drives namespace member resolution for the bundled std module:
    /// `std` denotes the std `struct`; `std.heap` a nested namespace `struct`;
    /// `std.heap.GeneralPurposeAllocator` a leaf `struct` whose `init`/`deinit`/
    /// `allocator` decls are then resolvable as real associated calls.
    fn denoted_aggregate_of(&self, base: &Expr) -> Option<TypeId> {
        match base {
            // A bare ident referring to a type-valued item: its denoted type is
            // recorded in `item_types` (a `const X = struct{...}` / `@import`).
            Expr::Ident { span, .. } => {
                let res = self.resolution_at(*span)?;
                let def = match res {
                    Resolution::Def(d) | Resolution::Predeclared(d) => d,
                    _ => return None,
                };
                self.item_types.get(&def).copied()
            }
            // A nested namespace access: this very field-access span was recorded
            // as type-valued when its parent was resolved (see `synth_field`).
            Expr::Field { span, .. } => {
                self.type_valued_spans.get(&(span.start, span.end)).copied()
            }
            _ => None,
        }
    }

    /// Resolves `field` against an aggregate (`struct`/`enum`/`union`) type used
    /// as a *namespace* (the base denotes the type itself). Records the
    /// [`MemberRes`] for the access span and returns the member's type, or `None`
    /// if the aggregate has no such member (the caller reports / defers).
    fn member_of_aggregate(&mut self, agg: TypeId, field: &str, span: Span) -> Option<TypeId> {
        let decls = match self.arena.get(agg) {
            Type::Struct(id) => &self.arena.structs[id.0 as usize].decls,
            Type::Enum(id) => &self.arena.enums[id.0 as usize].decls,
            Type::Union(id) => &self.arena.unions[id.0 as usize].decls,
            _ => return None,
        };
        if let Some(d) = decls.iter().find(|d| d.name == field) {
            let ty = d.ty;
            let def = d.def;
            self.record_member(span, MemberRes::Decl(def));
            return Some(ty);
        }
        None
    }

    /// The [`DefId`] of the named decl on an aggregate namespace type (used to
    /// look up the member's own denoted type in `item_types`).
    fn member_decl_def(&self, agg: TypeId, field: &str) -> Option<k2_resolve::DefId> {
        let decls = match self.arena.get(agg) {
            Type::Struct(id) => &self.arena.structs[id.0 as usize].decls,
            Type::Enum(id) => &self.arena.enums[id.0 as usize].decls,
            Type::Union(id) => &self.arena.unions[id.0 as usize].decls,
            _ => return None,
        };
        decls.iter().find(|d| d.name == field).map(|d| d.def)
    }

    /// Checks a bare `.Name` enum literal against an expected enum type.
    pub(crate) fn check_enum_literal(
        &mut self,
        name: &str,
        span: Span,
        expected: TypeId,
    ) -> TypeId {
        if self.arena.is_bottom(expected) {
            self.record_member(span, MemberRes::Deferred);
            return expected;
        }
        match self.arena.get(expected).clone() {
            Type::Enum(id) => {
                let info = &self.arena.enums[id.0 as usize];
                if let Some((idx, _)) = info
                    .variants
                    .iter()
                    .enumerate()
                    .find(|(_, v)| v.name == name)
                {
                    self.record_member(span, MemberRes::Variant(idx as u32));
                    expected
                } else {
                    let ename = info.name.clone();
                    self.error(span, format!("enum `{ename}` has no variant `.{name}`"));
                    self.record_member(span, MemberRes::Deferred);
                    self.arena.t_error()
                }
            }
            Type::Union(id) => {
                let info = &self.arena.unions[id.0 as usize];
                if let Some((idx, _)) = info
                    .variants
                    .iter()
                    .enumerate()
                    .find(|(_, v)| v.name == name)
                {
                    self.record_member(span, MemberRes::Variant(idx as u32));
                    expected
                } else {
                    let uname = info.name.clone();
                    self.error(span, format!("union `{uname}` has no variant `.{name}`"));
                    self.record_member(span, MemberRes::Deferred);
                    self.arena.t_error()
                }
            }
            // `.Struct` against a comptime reflection enum (e.g. `info != .Struct`):
            // the comparison base is Deferred, so the literal is too.
            _ => {
                self.record_member(span, MemberRes::Deferred);
                self.arena.t_deferred()
            }
        }
    }

    /// Synthesizes a bare `.Name` enum literal with no expectation (Deferred).
    pub(crate) fn synth_enum_literal(&mut self, span: Span) -> TypeId {
        self.record_member(span, MemberRes::Deferred);
        self.arena.t_deferred()
    }

    /// Synthesizes an `error.Name` literal: an [`Type::ErrorSet`] of just `Name`,
    /// which coerces into any error set/union containing it.
    pub(crate) fn synth_error_literal(&mut self, name: &str, span: Span) -> TypeId {
        self.record_member(span, MemberRes::ErrorMember);
        let id = self.arena.intern_errset(vec![name.to_string()]);
        self.arena.intern(Type::ErrorSet(id))
    }
}
