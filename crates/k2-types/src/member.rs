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
