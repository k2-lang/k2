//! Reflection: the `@typeInfo`/`@Type` round-trip, the `@sizeOf`/`@alignOf`
//! layout algorithm, `@hasField`/`@field`/`@This`/`@TypeOf`/`@typeName`, plus
//! the value-side builtin dispatcher and the field/index/enum-literal evaluation
//! the engine needs.
//!
//! The descriptor types (`TypeInfo`, `StructField`, `Signedness`, …) are
//! synthesized once into the arena at [`Checker::new`](crate::check::Checker)
//! time by [`Checker::install_reflection_types`], so they have stable
//! [`TypeId`]s and `@typeInfo(T) == @typeInfo(T)` holds. `@typeInfo(T)` reads a
//! [`Type`] and builds the matching [`Value::Union`]; [`Self::reify_type`] is its
//! inverse, interning the arena [`Type`] a descriptor denotes so that
//! `@Type(@typeInfo(T)) == T` for the primitive/pointer/array/optional forms.

use std::collections::HashMap;

use k2_syntax::{Expr, Span};

use crate::comptime::{Diverge, Env, EvalResult};
use crate::ty::{
    ArrayLen, EnumInfo, EnumVariant, FieldInfo, IntBits, StructInfo, Type, TypeId, UnionInfo,
    UnionTagKind, UnionVariant,
};
use crate::value::{ComptimeInt, Value};

/// The maximum integer bit width the language admits (`u0..=u65535`), matching
/// the `u<N>`/`i<N>` name parser in [`crate::eval`]. A `@Type` descriptor whose
/// `.bits` exceeds this (or is negative) is a diagnostic, not a wrapped width.
pub(crate) const MAX_INT_BITS: u16 = u16::MAX;

/// A computed memory layout: a size and an alignment, both in bytes, for the
/// 64-bit target the spec fixes (spec §02). Pointers and `usize` are 8 bytes.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Layout {
    /// The type's size in bytes.
    pub size: u64,
    /// The type's alignment in bytes (always a power of two).
    pub align: u64,
}

/// The cached [`TypeId`]s of the synthesized reflection descriptor types.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ReflectTypes {
    /// The `TypeInfo` tagged union (`@typeInfo`'s result type).
    pub type_info: TypeId,
    /// The `Signedness` enum (`.signed` / `.unsigned`).
    pub signedness: TypeId,
    /// The `StructField` descriptor struct.
    pub struct_field: TypeId,
    /// The `EnumField` descriptor struct.
    pub enum_field: TypeId,
}

/// The `TypeInfo` union variant order. Index into this list IS the union's
/// `which` tag, so it must match [`Checker::install_reflection_types`] exactly.
pub(crate) const TYPE_INFO_VARIANTS: &[&str] = &[
    "Int",
    "Float",
    "Bool",
    "Void",
    "Type",
    "NoReturn",
    "ComptimeInt",
    "ComptimeFloat",
    "Pointer",
    "Array",
    "Optional",
    "ErrorUnion",
    "ErrorSet",
    "Struct",
    "Enum",
    "Union",
];

/// Returns the `which` index of a `TypeInfo` variant by name.
fn type_info_tag(name: &str) -> u32 {
    TYPE_INFO_VARIANTS
        .iter()
        .position(|v| *v == name)
        .map(|i| i as u32)
        .unwrap_or(0)
}

impl crate::check::Checker<'_> {
    // =====================================================================
    //  One-time descriptor-type installation
    // =====================================================================

    /// Synthesizes the reflection descriptor types into the arena and returns
    /// their stable ids. Called once from [`Checker::new`].
    pub(crate) fn install_reflection_types(arena: &mut crate::arena::TypeArena) -> ReflectTypes {
        // Synthetic spans, distinct so the nominal aggregates do not collide
        // with any user type (whose spans live within the real source range).
        // We use a high base offset reserved for compiler-synthesized nodes.
        let mut next = 0xF000_0000u32;
        let mut span = || {
            let s = Span::new(next, next + 1, 0, 0);
            next += 2;
            s
        };

        let signedness = arena.intern_enum(EnumInfo {
            def: None,
            name: "Signedness".to_string(),
            span: span(),
            tag: arena.t_u8(),
            variants: vec![
                EnumVariant {
                    name: "signed".to_string(),
                    span: span(),
                },
                EnumVariant {
                    name: "unsigned".to_string(),
                    span: span(),
                },
            ],
            decls: Vec::new(),
        });

        let usize_t = arena.t_usize();
        let type_t = arena.t_type();
        let str_t = arena.t_str();
        let bool_t = arena.t_bool();
        let u16_t = arena.intern(Type::Int {
            signed: false,
            bits: IntBits::Fixed(16),
        });
        let ci_t = arena.t_comptime_int();

        let struct_field = arena.intern_struct(StructInfo {
            def: None,
            name: "StructField".to_string(),
            span: span(),
            layout: crate::ty::StructLayout::Auto,
            fields: vec![
                field("name", str_t, span()),
                field("type", type_t, span()),
                field("alignment", usize_t, span()),
            ],
            decls: Vec::new(),
        });

        let enum_field = arena.intern_struct(StructInfo {
            def: None,
            name: "EnumField".to_string(),
            span: span(),
            layout: crate::ty::StructLayout::Auto,
            fields: vec![field("name", str_t, span()), field("value", ci_t, span())],
            decls: Vec::new(),
        });

        // The `TypeInfo` union. Variant payloads are synthesized structs (each
        // interned by its own span) holding the per-kind fields the spec lists.
        let int_payload = arena.intern_struct(StructInfo {
            def: None,
            name: "Type.Int".to_string(),
            span: span(),
            layout: crate::ty::StructLayout::Auto,
            fields: vec![
                field("signedness", signedness, span()),
                field("bits", u16_t, span()),
            ],
            decls: Vec::new(),
        });
        let float_payload = arena.intern_struct(StructInfo {
            def: None,
            name: "Type.Float".to_string(),
            span: span(),
            layout: crate::ty::StructLayout::Auto,
            fields: vec![field("bits", u16_t, span())],
            decls: Vec::new(),
        });
        let struct_fields_slice = arena.slice(true, struct_field);
        let struct_payload = arena.intern_struct(StructInfo {
            def: None,
            name: "Type.Struct".to_string(),
            span: span(),
            layout: crate::ty::StructLayout::Auto,
            fields: vec![
                field("fields", struct_fields_slice, span()),
                field("is_tuple", bool_t, span()),
            ],
            decls: Vec::new(),
        });
        let enum_fields_slice = arena.slice(true, enum_field);
        let enum_payload = arena.intern_struct(StructInfo {
            def: None,
            name: "Type.Enum".to_string(),
            span: span(),
            layout: crate::ty::StructLayout::Auto,
            fields: vec![
                field("tag_type", type_t, span()),
                field("fields", enum_fields_slice, span()),
            ],
            decls: Vec::new(),
        });
        let ptr_payload = arena.intern_struct(StructInfo {
            def: None,
            name: "Type.Pointer".to_string(),
            span: span(),
            layout: crate::ty::StructLayout::Auto,
            fields: vec![
                field("size", signedness, span()),
                field("is_const", bool_t, span()),
                field("child", type_t, span()),
            ],
            decls: Vec::new(),
        });
        let child_payload = arena.intern_struct(StructInfo {
            def: None,
            name: "Type.Child".to_string(),
            span: span(),
            layout: crate::ty::StructLayout::Auto,
            fields: vec![field("child", type_t, span())],
            decls: Vec::new(),
        });
        let array_payload = arena.intern_struct(StructInfo {
            def: None,
            name: "Type.Array".to_string(),
            span: span(),
            layout: crate::ty::StructLayout::Auto,
            fields: vec![
                field("len", usize_t, span()),
                field("child", type_t, span()),
            ],
            decls: Vec::new(),
        });

        let void_t = arena.t_void();
        let variant = |name: &str, payload: TypeId| UnionVariant {
            name: name.to_string(),
            payload,
            span: Span::new(0xE000_0000, 0xE000_0001, 0, 0),
        };
        // Build with distinct spans per variant.
        let mut variants = Vec::new();
        for (name, payload) in [
            ("Int", int_payload),
            ("Float", float_payload),
            ("Bool", void_t),
            ("Void", void_t),
            ("Type", void_t),
            ("NoReturn", void_t),
            ("ComptimeInt", void_t),
            ("ComptimeFloat", void_t),
            ("Pointer", ptr_payload),
            ("Array", array_payload),
            ("Optional", child_payload),
            ("ErrorUnion", child_payload),
            ("ErrorSet", void_t),
            ("Struct", struct_payload),
            ("Enum", enum_payload),
            ("Union", enum_payload),
        ] {
            let mut v = variant(name, payload);
            v.span = span();
            variants.push(v);
        }

        let type_info = arena.intern_union(UnionInfo {
            def: None,
            name: "TypeInfo".to_string(),
            span: span(),
            tag: UnionTagKind::Inferred,
            variants,
            decls: Vec::new(),
        });

        ReflectTypes {
            type_info,
            signedness,
            struct_field,
            enum_field,
        }
    }

    // =====================================================================
    //  @typeInfo
    // =====================================================================

    /// Builds the `@typeInfo(T)` descriptor value for an arena type.
    pub(crate) fn type_info(&mut self, tid: TypeId) -> EvalResult {
        let ty = self.arena.get(tid).clone();
        let union_ty = self.reflect.type_info;
        let make = |which: u32, payload: Value| Value::Union {
            ty: union_ty,
            which,
            payload: Box::new(payload),
        };
        match ty {
            Type::Int { signed, bits } => {
                let width = self.int_bits_value(bits);
                let signedness = self.signedness_value(signed);
                let payload = Value::Struct {
                    ty: self.struct_payload_int(),
                    fields: vec![signedness, width],
                };
                Ok(make(type_info_tag("Int"), payload))
            }
            Type::ComptimeInt => {
                let width = Value::Int(ComptimeInt {
                    v: 0,
                    ty: self.arena.intern(Type::Int {
                        signed: false,
                        bits: IntBits::Fixed(16),
                    }),
                });
                let signedness = self.signedness_value(true);
                let payload = Value::Struct {
                    ty: self.struct_payload_int(),
                    fields: vec![signedness, width],
                };
                Ok(make(type_info_tag("Int"), payload))
            }
            Type::Float { bits } => {
                let width = Value::Int(ComptimeInt {
                    v: bits as i128,
                    ty: self.arena.intern(Type::Int {
                        signed: false,
                        bits: IntBits::Fixed(16),
                    }),
                });
                let payload = Value::Struct {
                    ty: self.struct_payload_float(),
                    fields: vec![width],
                };
                Ok(make(type_info_tag("Float"), payload))
            }
            Type::Bool => Ok(make(type_info_tag("Bool"), Value::Void)),
            Type::Void => Ok(make(type_info_tag("Void"), Value::Void)),
            Type::TypeType => Ok(make(type_info_tag("Type"), Value::Void)),
            Type::NoReturn => Ok(make(type_info_tag("NoReturn"), Value::Void)),
            Type::ComptimeFloat => Ok(make(type_info_tag("ComptimeFloat"), Value::Void)),
            Type::Pointer { is_const, pointee } => {
                let payload = self.pointer_payload(false, is_const, pointee);
                Ok(make(type_info_tag("Pointer"), payload))
            }
            Type::Slice { is_const, elem } => {
                let payload = self.pointer_payload(true, is_const, elem);
                Ok(make(type_info_tag("Pointer"), payload))
            }
            Type::Array { len, elem } => {
                let n = match len {
                    ArrayLen::Known(n) => n as i128,
                    _ => 0,
                };
                let usize_t = self.arena.t_usize();
                let payload = Value::Struct {
                    ty: self.struct_payload_array(),
                    fields: vec![
                        Value::Int(ComptimeInt { v: n, ty: usize_t }),
                        Value::Type(elem),
                    ],
                };
                Ok(make(type_info_tag("Array"), payload))
            }
            Type::Optional(inner) => {
                let payload = self.child_payload(inner);
                Ok(make(type_info_tag("Optional"), payload))
            }
            Type::ErrorUnion { ok, .. } => {
                let payload = self.child_payload(ok);
                Ok(make(type_info_tag("ErrorUnion"), payload))
            }
            Type::ErrorSet(_) | Type::AnyError => Ok(make(type_info_tag("ErrorSet"), Value::Void)),
            Type::Struct(id) => {
                let fields = self.arena.structs[id.0 as usize].fields.clone();
                let mut descs = Vec::with_capacity(fields.len());
                for f in &fields {
                    descs.push(self.struct_field_value(&f.name, f.ty));
                }
                let slice_ty = {
                    let sf = self.reflect.struct_field;
                    self.arena.slice(true, sf)
                };
                let payload = Value::Struct {
                    ty: self.struct_payload_struct(),
                    fields: vec![
                        Value::Array {
                            ty: slice_ty,
                            elems: descs,
                        },
                        Value::Bool(false),
                    ],
                };
                Ok(make(type_info_tag("Struct"), payload))
            }
            Type::Enum(id) => {
                let info = self.arena.enums[id.0 as usize].clone();
                let mut descs = Vec::with_capacity(info.variants.len());
                let ci = self.arena.t_comptime_int();
                for (i, v) in info.variants.iter().enumerate() {
                    descs.push(self.enum_field_value(&v.name, i as i128, ci));
                }
                let slice_ty = {
                    let ef = self.reflect.enum_field;
                    self.arena.slice(true, ef)
                };
                let payload = Value::Struct {
                    ty: self.struct_payload_enum(),
                    fields: vec![
                        Value::Type(info.tag),
                        Value::Array {
                            ty: slice_ty,
                            elems: descs,
                        },
                    ],
                };
                Ok(make(type_info_tag("Enum"), payload))
            }
            Type::Union(_) => Ok(make(type_info_tag("Union"), Value::Void)),
            // Genuinely opaque/comptime-unknown: stay deferred.
            _ => Err(Diverge::NotComptime),
        }
    }

    /// The `StructField` descriptor value for one field.
    fn struct_field_value(&mut self, name: &str, ty: TypeId) -> Value {
        let usize_t = self.arena.t_usize();
        let align = self.layout(ty).map(|l| l.align).unwrap_or(1);
        Value::Struct {
            ty: self.reflect.struct_field,
            fields: vec![
                Value::Str(name.to_string()),
                Value::Type(ty),
                Value::Int(ComptimeInt {
                    v: align as i128,
                    ty: usize_t,
                }),
            ],
        }
    }

    /// The `EnumField` descriptor value for one variant.
    fn enum_field_value(&mut self, name: &str, value: i128, ci: TypeId) -> Value {
        Value::Struct {
            ty: self.reflect.enum_field,
            fields: vec![
                Value::Str(name.to_string()),
                Value::Int(ComptimeInt { v: value, ty: ci }),
            ],
        }
    }

    /// The `.Int`/`.Pointer`/etc. payload struct ids, looked up by name from the
    /// `TypeInfo` union's variant payload types.
    fn struct_payload_int(&self) -> TypeId {
        self.type_info_payload("Int")
    }
    fn struct_payload_float(&self) -> TypeId {
        self.type_info_payload("Float")
    }
    fn struct_payload_array(&self) -> TypeId {
        self.type_info_payload("Array")
    }
    fn struct_payload_struct(&self) -> TypeId {
        self.type_info_payload("Struct")
    }
    fn struct_payload_enum(&self) -> TypeId {
        self.type_info_payload("Enum")
    }

    /// The payload struct type of a named `TypeInfo` variant.
    fn type_info_payload(&self, variant: &str) -> TypeId {
        if let Type::Union(id) = self.arena.get(self.reflect.type_info) {
            let info = &self.arena.unions[id.0 as usize];
            if let Some(v) = info.variants.iter().find(|v| v.name == variant) {
                return v.payload;
            }
        }
        self.arena.t_deferred()
    }

    /// The `Signedness` enum value for a sign flag.
    fn signedness_value(&self, signed: bool) -> Value {
        Value::Enum {
            ty: self.reflect.signedness,
            which: if signed { 0 } else { 1 },
        }
    }

    /// A `u16` bit-width value for an integer type.
    fn int_bits_value(&mut self, bits: IntBits) -> Value {
        let n = match bits {
            IntBits::Fixed(n) => n as i128,
            IntBits::Usize | IntBits::Isize => 64,
        };
        let u16_t = self.arena.intern(Type::Int {
            signed: false,
            bits: IntBits::Fixed(16),
        });
        Value::Int(ComptimeInt { v: n, ty: u16_t })
    }

    /// The `.Pointer` payload value (`size`, `is_const`, `child`).
    fn pointer_payload(&mut self, is_slice: bool, is_const: bool, child: TypeId) -> Value {
        // `size` is modelled with the `Signedness` enum stand-in: variant 0 =
        // `.One`-like, variant 1 = `.Slice`-like. The reflection examples in the
        // corpus only ever compare `p.size != .Slice`, so a 2-state tag suffices.
        let size = Value::Enum {
            ty: self.reflect.signedness,
            which: if is_slice { 1 } else { 0 },
        };
        Value::Struct {
            ty: self.type_info_payload("Pointer"),
            fields: vec![size, Value::Bool(is_const), Value::Type(child)],
        }
    }

    /// A single-`child` payload value (`Optional`/`ErrorUnion`).
    fn child_payload(&mut self, child: TypeId) -> Value {
        Value::Struct {
            ty: self.type_info_payload("Optional"),
            fields: vec![Value::Type(child)],
        }
    }

    // =====================================================================
    //  @Type (reconstruct a type from a descriptor)
    // =====================================================================

    /// Reconstructs an arena type from a `@typeInfo`-shaped descriptor value.
    /// `span` locates any range/validity diagnostic the descriptor itself causes
    /// (an out-of-range `.bits`).
    pub(crate) fn reify_type(&mut self, info: &Value, span: Span) -> Result<TypeId, Diverge> {
        // The descriptor is either a `Value::Union` (from `@typeInfo`) or an
        // anonymous `.{ .Int = .{...} }` we represent as a one-variant union.
        let (which_name, payload) = match info {
            Value::Union { ty, which, payload } => {
                let name = self
                    .variant_name_of(*ty, *which)
                    .ok_or(Diverge::NotComptime)?;
                (name, payload.as_ref().clone())
            }
            Value::AnonTagged { tag, payload } => (tag.clone(), payload.as_ref().clone()),
            _ => return Err(Diverge::NotComptime),
        };
        match which_name.as_str() {
            "Int" => {
                let (signed, bits) = self.read_int_descriptor(&payload, span)?;
                Ok(self.arena.intern(Type::Int {
                    signed,
                    bits: IntBits::Fixed(bits),
                }))
            }
            "Float" => {
                let raw = self.read_field_int(&payload, "bits")?;
                // Float widths are also descriptor-supplied: range-check instead
                // of wrapping with `as u16`.
                let bits = self.checked_int_bits(raw, span)?;
                Ok(self.arena.intern(Type::Float { bits }))
            }
            "Bool" => Ok(self.arena.t_bool()),
            "Void" => Ok(self.arena.t_void()),
            "Optional" => {
                let child = self.read_field_type(&payload, "child")?;
                Ok(self.arena.optional(child))
            }
            "Pointer" => {
                let child = self.read_field_type(&payload, "child")?;
                let is_const = self.read_field_bool(&payload, "is_const").unwrap_or(true);
                // Honor the descriptor's `size` field: a `.One`-like pointer
                // (`pointer_payload` stores `which == 0`) must reify back to a
                // single-item `*T`, a `.Slice`-like one (`which == 1`) to `[]T`.
                // Discarding this turned every `*T` round-trip into a slice, the
                // v0.6 bug; both `@Type(@typeInfo(*u8)) == *u8` and `[]u8` now hold.
                let is_slice = self.pointer_descriptor_is_slice(&payload);
                if is_slice {
                    Ok(self.arena.slice(is_const, child))
                } else {
                    Ok(self.arena.ptr(is_const, child))
                }
            }
            "Array" => {
                let len = self.read_field_int(&payload, "len").unwrap_or(0);
                let child = self.read_field_type(&payload, "child")?;
                // Checked, never truncating: an out-of-range length defers rather
                // than fabricating a different (wrapped) array size.
                let len = match u64::try_from(len) {
                    Ok(n) => ArrayLen::Known(n),
                    Err(_) => ArrayLen::Deferred,
                };
                Ok(self.arena.intern(Type::Array { len, elem: child }))
            }
            "Struct" => self.reify_struct(&payload),
            _ => Err(Diverge::NotComptime),
        }
    }

    /// Builds a fresh struct type from a `.Struct` descriptor's field list.
    fn reify_struct(&mut self, payload: &Value) -> Result<TypeId, Diverge> {
        let fields_val = self
            .read_field(payload, "fields")
            .ok_or(Diverge::NotComptime)?;
        let elems = match fields_val {
            Value::Array { elems, .. } => elems,
            _ => return Err(Diverge::NotComptime),
        };
        let mut fields = Vec::with_capacity(elems.len());
        // Content key so `@Type` of a structurally-identical field list dedups
        // to one nominal struct (identity-stability of the round-trip).
        let mut key_parts: Vec<String> = Vec::new();
        for fv in &elems {
            let name = self
                .read_field(fv, "name")
                .and_then(|v| v.as_str().map(|s| s.to_string()))
                .ok_or(Diverge::NotComptime)?;
            let fty = self.read_field_type(fv, "type")?;
            key_parts.push(format!("{name}:{}", fty.0));
            fields.push(field(&name, fty, Span::default()));
        }
        let key = key_parts.join(",");
        if let Some(&existing) = self.reify_struct_cache.get(&key) {
            return Ok(existing);
        }
        let span = self.fresh_synthetic_span();
        let tid = self.arena.intern_struct(StructInfo {
            def: None,
            name: "struct".to_string(),
            span,
            layout: crate::ty::StructLayout::Auto,
            fields,
            decls: Vec::new(),
        });
        self.reify_struct_cache.insert(key, tid);
        Ok(tid)
    }

    /// Decides whether a `.Pointer` descriptor's `size` field denotes a slice.
    ///
    /// `pointer_payload` stores `size` as the `Signedness` enum stand-in
    /// (`which == 1` => `.Slice`, `which == 0` => `.One`); a hand-written
    /// `.{ .size = .Slice }` arrives as a bare tag string instead. A missing or
    /// unrecognized `size` conservatively defaults to a single-item pointer (the
    /// less surprising of the two, and the one that round-trips `*T`).
    fn pointer_descriptor_is_slice(&self, payload: &Value) -> bool {
        match self.read_field(payload, "size") {
            Some(Value::Enum { which, .. }) => which == 1,
            Some(Value::Str(s)) => s == "Slice" || s == "slice",
            _ => false,
        }
    }

    /// Reads a `.Int` descriptor's `(signed, bits)`, range-checking the width.
    fn read_int_descriptor(&mut self, payload: &Value, span: Span) -> Result<(bool, u16), Diverge> {
        let raw = self.read_field_int(payload, "bits")?;
        let bits = self.checked_int_bits(raw, span)?;
        let signed = match self.read_field(payload, "signedness") {
            Some(Value::Enum { which, .. }) => which == 0,
            // `.signedness = .unsigned` written as a bare tag literal.
            Some(Value::Str(s)) => s == "signed",
            _ => false,
        };
        Ok((signed, bits))
    }

    /// Validates a descriptor-supplied bit width: rejects a negative width or one
    /// exceeding [`MAX_INT_BITS`] with a precise diagnostic (spec §07 robustness)
    /// instead of the old `as u16` wrap that silently fabricated a giant type.
    fn checked_int_bits(&mut self, raw: i128, span: Span) -> Result<u16, Diverge> {
        if !(0..=MAX_INT_BITS as i128).contains(&raw) {
            self.comptime_error(
                span,
                format!("`@Type` bit width {raw} out of range (0..={MAX_INT_BITS})"),
            );
            return Err(Diverge::CompileError);
        }
        Ok(raw as u16)
    }

    /// Looks up a named field of a struct value (by the declared struct layout),
    /// or of an anonymous initializer value (by written name).
    fn read_field(&self, v: &Value, name: &str) -> Option<Value> {
        match v {
            Value::Struct { ty, fields } => {
                if let Type::Struct(id) = self.arena.get(*ty) {
                    let idx = self.arena.structs[id.0 as usize]
                        .fields
                        .iter()
                        .position(|f| f.name == name)?;
                    return fields.get(idx).cloned();
                }
                None
            }
            Value::AnonStruct(pairs) => pairs
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| v.clone()),
            _ => None,
        }
    }

    /// Reads a named `type`-valued field.
    fn read_field_type(&self, v: &Value, name: &str) -> Result<TypeId, Diverge> {
        self.read_field(v, name)
            .and_then(|v| v.as_type())
            .ok_or(Diverge::NotComptime)
    }

    /// Reads a named integer field.
    fn read_field_int(&self, v: &Value, name: &str) -> Result<i128, Diverge> {
        self.read_field(v, name)
            .and_then(|v| v.as_int())
            .ok_or(Diverge::NotComptime)
    }

    /// Reads a named bool field.
    fn read_field_bool(&self, v: &Value, name: &str) -> Option<bool> {
        self.read_field(v, name).and_then(|v| v.as_bool())
    }

    /// The variant name at index `which` of `ty` (enum or union).
    fn variant_name_of(&self, ty: TypeId, which: u32) -> Option<String> {
        match self.arena.get(ty) {
            Type::Union(id) => self.arena.unions[id.0 as usize]
                .variants
                .get(which as usize)
                .map(|v| v.name.clone()),
            Type::Enum(id) => self.arena.enums[id.0 as usize]
                .variants
                .get(which as usize)
                .map(|v| v.name.clone()),
            _ => None,
        }
    }

    // =====================================================================
    //  Layout: @sizeOf / @alignOf
    // =====================================================================

    /// Computes the concrete byte layout of a type, or `None` if a length is not
    /// comptime-known.
    pub(crate) fn layout(&self, tid: TypeId) -> Option<Layout> {
        self.layout_depth(tid, 0)
    }

    /// The TRUE bit width of a type for `@bitSizeOf` (spec §02). Scalars report
    /// their representational bit count directly — `Int{bits}` => the actual
    /// width (`usize`/`isize` => 64), `Bool` => 1, `Float{bits}` => `bits` — so
    /// `@bitSizeOf(u7) == 7` and `@bitSizeOf(bool) == 1` rather than the
    /// rounded-up `byte_size * 8`. Aggregates and pointers have no narrower-than-
    /// byte representation, so they fall back to `layout.size * 8`.
    pub(crate) fn bit_size(&self, tid: TypeId) -> Option<u64> {
        match self.arena.get(tid) {
            Type::Int { bits, .. } => Some(match bits {
                IntBits::Fixed(n) => *n as u64,
                IntBits::Usize | IntBits::Isize => 64,
            }),
            Type::Bool => Some(1),
            Type::Float { bits } => Some(*bits as u64),
            // A `packed struct`'s bit size is its exact field-bit total, NOT the
            // rounded-up byte size (spec §02): `packed struct { a:u3, b:u3, c:u3 }`
            // is `@bitSizeOf == 9`, `@sizeOf == 2`.
            Type::Struct(id) if self.arena.structs[id.0 as usize].is_packed() => {
                let fields = self.arena.structs[id.0 as usize].fields.clone();
                packed_struct_size(&self.arena, &fields).map(|(bits, _)| bits)
            }
            _ => self.layout(tid).map(|l| l.size * 8),
        }
    }

    /// The byte offset of a named field of a struct type, honoring per-field
    /// `align(N)` (spec §03). A packed-struct field is bit-addressable and has no
    /// byte offset, so it is a diagnostic; an unknown field name or non-struct
    /// type defers.
    fn field_byte_offset(&mut self, tid: TypeId, fname: &str, span: Span) -> Result<u64, Diverge> {
        let Type::Struct(id) = self.arena.get(tid).clone() else {
            return Err(Diverge::NotComptime);
        };
        let info = &self.arena.structs[id.0 as usize];
        if info.is_packed() {
            self.comptime_error(
                span,
                "`@offsetOf` on a packed-struct field is not byte-addressable",
            );
            return Err(Diverge::CompileError);
        }
        let fields = info.fields.clone();
        let mut offset = 0u64;
        for f in &fields {
            let fl = self.layout_depth(f.ty, 0).ok_or(Diverge::NotComptime)?;
            let fa = f.align.map(|a| a.max(fl.align)).unwrap_or(fl.align).max(1);
            offset = round_up(offset, fa);
            if f.name == fname {
                return Ok(offset);
            }
            offset += fl.size;
        }
        Err(Diverge::NotComptime)
    }

    /// Layout with a recursion-depth guard so a cyclic struct cannot overflow.
    fn layout_depth(&self, tid: TypeId, depth: u32) -> Option<Layout> {
        if depth > 64 {
            return None;
        }
        match self.arena.get(tid).clone() {
            Type::Int { bits, .. } => {
                let size = int_byte_size(bits);
                Some(Layout { size, align: size })
            }
            Type::Float { bits } => {
                let size = (bits as u64).div_ceil(8).next_power_of_two();
                Some(Layout { size, align: size })
            }
            Type::Bool => Some(Layout { size: 1, align: 1 }),
            Type::Void | Type::NoReturn => Some(Layout { size: 0, align: 1 }),
            Type::Pointer { .. } => Some(Layout { size: 8, align: 8 }),
            Type::Slice { .. } => Some(Layout { size: 16, align: 8 }),
            Type::Optional(inner) => {
                // `?*T` keeps the null-pointer niche; otherwise a flag byte.
                let il = self.layout_depth(inner, depth + 1)?;
                if matches!(self.arena.get(inner), Type::Pointer { .. }) {
                    Some(il)
                } else {
                    let align = il.align.max(1);
                    let size = round_up(il.size + 1, align);
                    Some(Layout { size, align })
                }
            }
            Type::ErrorUnion { ok, .. } => {
                let ol = self.layout_depth(ok, depth + 1)?;
                let align = ol.align.max(2);
                let size = round_up(round_up(2, ol.align.max(1)) + ol.size, align);
                Some(Layout { size, align })
            }
            Type::Array { len, elem } => {
                let n = match len {
                    ArrayLen::Known(n) => n,
                    _ => return None,
                };
                let el = self.layout_depth(elem, depth + 1)?;
                Some(Layout {
                    size: el.size.saturating_mul(n),
                    align: el.align,
                })
            }
            Type::Vector { len, elem } => {
                // `@sizeOf(@Vector(N,T)) = round_up(N * @sizeOf(T), align)`, where
                // `align = min(16, (N*elem).next_power_of_two())` (the XMM cap).
                let el = self.layout_depth(elem, depth + 1)?;
                let raw = el.size.saturating_mul(len as u64);
                let align = raw.max(1).next_power_of_two().min(16);
                Some(Layout {
                    size: round_up(raw, align),
                    align,
                })
            }
            Type::Struct(id) => {
                let info = &self.arena.structs[id.0 as usize];
                let is_packed = info.is_packed();
                let fields = info.fields.clone();
                if is_packed {
                    // A packed struct is one backing integer: size = the bit total
                    // rounded to a natural integer width, align = size (unless a
                    // field raises it). `@bitSizeOf` reports the exact bit total.
                    let (_bits, size) = packed_struct_size(&self.arena, &fields)?;
                    let field_align = fields.iter().filter_map(|f| f.align).max();
                    let align = field_align.map(|a| a.max(size)).unwrap_or(size).max(1);
                    return Some(Layout { size, align });
                }
                let mut offset = 0u64;
                let mut max_align = 1u64;
                for f in &fields {
                    let fl = self.layout_depth(f.ty, depth + 1)?;
                    // `align(N)` raises (never lowers) the field's alignment (spec
                    // §03), so it bumps both the field offset and the struct align.
                    let fa = f.align.map(|a| a.max(fl.align)).unwrap_or(fl.align).max(1);
                    offset = round_up(offset, fa);
                    offset += fl.size;
                    max_align = max_align.max(fa);
                }
                Some(Layout {
                    size: round_up(offset, max_align),
                    align: max_align,
                })
            }
            Type::Enum(id) => {
                let tag = self.arena.enums[id.0 as usize].tag;
                self.layout_depth(tag, depth + 1)
            }
            _ => None,
        }
    }
}

impl crate::check::Checker<'_> {
    // =====================================================================
    //  Value-level field / index / enum-literal evaluation
    // =====================================================================

    /// Evaluates `base.field` over a comptime value: project into a struct /
    /// union / typeInfo descriptor, read `.len`, or recover an enum/error member
    /// from a `type` base.
    pub(crate) fn eval_field(
        &mut self,
        env: &mut Env,
        base: &Expr,
        field: &str,
        _span: Span,
    ) -> EvalResult {
        let bv = self.eval_expr(env, base)?;
        self.project_field(&bv, field)
    }

    /// Projects a named field/member out of an already-evaluated value.
    pub(crate) fn project_field(&mut self, bv: &Value, field: &str) -> EvalResult {
        match bv {
            Value::Struct { ty, fields } => {
                if let Type::Struct(id) = self.arena.get(*ty) {
                    if let Some(idx) = self.arena.structs[id.0 as usize]
                        .fields
                        .iter()
                        .position(|f| f.name == field)
                    {
                        return fields.get(idx).cloned().ok_or(Diverge::NotComptime);
                    }
                }
                Err(Diverge::NotComptime)
            }
            Value::AnonStruct(pairs) => pairs
                .iter()
                .find(|(n, _)| n == field)
                .map(|(_, v)| v.clone())
                .ok_or(Diverge::NotComptime),
            // `info.Struct` / `info.Int` — narrow a typeInfo union to its active
            // payload when the requested variant is the active one.
            Value::Union { ty, which, payload } => {
                if let Some(name) = self.variant_name_of(*ty, *which) {
                    if name == field {
                        return Ok(payload.as_ref().clone());
                    }
                }
                Err(Diverge::NotComptime)
            }
            Value::AnonTagged { tag, payload } => {
                if tag == field {
                    Ok(payload.as_ref().clone())
                } else {
                    Err(Diverge::NotComptime)
                }
            }
            // `.len` of a comptime sequence.
            Value::Array { elems, .. } if field == "len" => {
                let usize_t = self.arena.t_usize();
                Ok(Value::Int(ComptimeInt {
                    v: elems.len() as i128,
                    ty: usize_t,
                }))
            }
            Value::Tuple(elems) if field == "len" => {
                let usize_t = self.arena.t_usize();
                Ok(Value::Int(ComptimeInt {
                    v: elems.len() as i128,
                    ty: usize_t,
                }))
            }
            // `EnumType.Variant` / `ErrorSet.Member` recovered from a `type` base.
            Value::Type(t) => self.project_type_member(*t, field),
            _ => Err(Diverge::NotComptime),
        }
    }

    /// Recovers a member from a comptime `type` base: an enum variant value, or
    /// an error-set member error value.
    fn project_type_member(&mut self, t: TypeId, field: &str) -> EvalResult {
        match self.arena.get(t).clone() {
            Type::Enum(id) => {
                if let Some((idx, _)) = self.arena.enums[id.0 as usize]
                    .variants
                    .iter()
                    .enumerate()
                    .find(|(_, v)| v.name == field)
                {
                    return Ok(Value::Enum {
                        ty: t,
                        which: idx as u32,
                    });
                }
                Err(Diverge::NotComptime)
            }
            Type::ErrorSet(id) => {
                if self.arena.errsets[id.0 as usize]
                    .members
                    .iter()
                    .any(|m| m == field)
                {
                    return Ok(Value::ErrVal {
                        set: t,
                        name: field.to_string(),
                    });
                }
                Err(Diverge::NotComptime)
            }
            _ => Err(Diverge::NotComptime),
        }
    }

    /// Evaluates `base[index]` over a comptime array/tuple/sequence.
    pub(crate) fn eval_index(
        &mut self,
        env: &mut Env,
        base: &Expr,
        index: &Expr,
        span: Span,
    ) -> EvalResult {
        let bv = self.eval_expr(env, base)?;
        let i = self
            .eval_expr(env, index)?
            .as_int()
            .ok_or(Diverge::NotComptime)?;
        let i = usize::try_from(i).map_err(|_| Diverge::NotComptime)?;
        let elems = match &bv {
            Value::Array { elems, .. } => elems,
            Value::Tuple(elems) => elems,
            _ => return Err(Diverge::NotComptime),
        };
        if i >= elems.len() {
            self.comptime_error(span, "comptime index out of bounds");
            return Err(Diverge::CompileError);
        }
        Ok(elems[i].clone())
    }

    /// Evaluates a bare `.Name` enum/tag literal. Without an expected enum the
    /// engine represents it as its tag name (a `Str`), so descriptor reads
    /// (`.signedness = .unsigned`) and tag comparisons both work.
    pub(crate) fn eval_enum_literal(&mut self, name: &str, _span: Span) -> EvalResult {
        Ok(Value::Str(name.to_string()))
    }

    // =====================================================================
    //  Value-level builtins
    // =====================================================================

    /// Dispatches a comptime builtin to its value result.
    pub(crate) fn eval_builtin_value(
        &mut self,
        env: &mut Env,
        name: &str,
        args: &[Expr],
        span: Span,
    ) -> EvalResult {
        match name {
            "@typeInfo" => {
                let t = self.arg_type(env, args, 0)?;
                self.type_info(t)
            }
            "@Type" => {
                let arg = self.arg_at(name, args, 0, 1, span)?;
                let info = self.eval_expr(env, arg)?;
                let t = self.reify_type(&info, span)?;
                Ok(Value::Type(t))
            }
            "@Vector" => {
                // `@Vector(N, T)` -> the vector type. `N` is a comptime length and
                // `T` a numeric element type (int/float/bool); anything else is a
                // diagnostic rather than a malformed type (spec §02).
                self.arg_at(name, args, 1, 2, span)?;
                let n_arg = self.arg_at(name, args, 0, 2, span)?;
                let n = self
                    .eval_expr(env, n_arg)?
                    .as_int()
                    .ok_or(Diverge::NotComptime)?;
                let len = u32::try_from(n).map_err(|_| {
                    self.comptime_error(span, "`@Vector` length must fit a u32");
                    Diverge::CompileError
                })?;
                let elem = self.arg_type(env, args, 1)?;
                if !matches!(
                    self.arena.get(elem),
                    Type::Int { .. } | Type::Float { .. } | Type::Bool
                ) {
                    self.comptime_error(
                        span,
                        "`@Vector` element type must be an integer, float, or bool",
                    );
                    return Err(Diverge::CompileError);
                }
                Ok(Value::Type(self.arena.vector(len, elem)))
            }
            "@TypeOf" => {
                // The *type* of the operand expression. We synth it through the
                // checker (which may itself be Deferred -> not comptime).
                if let Some(first) = args.first() {
                    let t = self.synth(first);
                    if self.arena.is_bottom(t) {
                        return Err(Diverge::NotComptime);
                    }
                    return Ok(Value::Type(t));
                }
                Err(Diverge::NotComptime)
            }
            "@This" => self
                .self_stack
                .last()
                .copied()
                .map(Value::Type)
                .ok_or(Diverge::NotComptime),
            "@typeName" => {
                let t = self.arg_type(env, args, 0)?;
                Ok(Value::Str(self.arena.fmt(t)))
            }
            "@offsetOf" => {
                // `@offsetOf(T, "field")` -> the field's byte offset (honoring
                // `align(N)`). A packed-struct field has no byte address, so it is
                // a clean diagnostic rather than a misleading number (spec §02).
                self.arg_at(name, args, 1, 2, span)?;
                let t = self.arg_type(env, args, 0)?;
                let name_arg = self.arg_at(name, args, 1, 2, span)?;
                let fname = self.eval_expr(env, name_arg)?;
                let fname = fname.as_str().ok_or(Diverge::NotComptime)?.to_string();
                let off = self.field_byte_offset(t, &fname, span)?;
                let usize_t = self.arena.t_usize();
                Ok(Value::Int(ComptimeInt {
                    v: off as i128,
                    ty: usize_t,
                }))
            }
            "@sizeOf" | "@alignOf" | "@bitSizeOf" => {
                // Guard arity first so `@sizeOf()` is a diagnostic, not a silent
                // deferral; then evaluate the (single) type operand.
                self.arg_at(name, args, 0, 1, span)?;
                let t = self.arg_type(env, args, 0)?;
                let v = match name {
                    "@alignOf" => self.layout(t).ok_or(Diverge::NotComptime)?.align,
                    // `@bitSizeOf` is the type's TRUE bit width, not `size*8`:
                    // sub-byte ints, `bool`, and floats are not whole-byte-wide.
                    "@bitSizeOf" => self.bit_size(t).ok_or(Diverge::NotComptime)?,
                    _ => self.layout(t).ok_or(Diverge::NotComptime)?.size,
                } as i128;
                let usize_t = self.arena.t_usize();
                Ok(Value::Int(ComptimeInt { v, ty: usize_t }))
            }
            "@hasField" => {
                // Arity-guard the *name* argument before touching it: a missing
                // arg 1 must be a diagnostic, never an index-panic (the type arg
                // is already guarded by `arg_type`/`arg_at`).
                let t = self.arg_type(env, args, 0)?;
                let name_arg = self.arg_at(name, args, 1, 2, span)?;
                let fname = self.eval_expr(env, name_arg)?;
                let fname = fname.as_str().ok_or(Diverge::NotComptime)?;
                Ok(Value::Bool(self.type_has_field(t, fname)))
            }
            "@field" => {
                let base_arg = self.arg_at(name, args, 0, 2, span)?;
                let name_arg = self.arg_at(name, args, 1, 2, span)?;
                let base = self.eval_expr(env, base_arg)?;
                let fname = self.eval_expr(env, name_arg)?;
                let fname = fname.as_str().ok_or(Diverge::NotComptime)?.to_string();
                self.project_field(&base, &fname)
            }
            "@intCast" | "@truncate" | "@as" | "@bitCast" => {
                // Value-preserving comptime casts: evaluate the (last) operand.
                let operand = args.last().ok_or(Diverge::NotComptime)?;
                self.eval_expr(env, operand)
            }
            "@compileError" => {
                let msg = match args.first() {
                    Some(a) => self
                        .eval_expr(env, a)
                        .ok()
                        .and_then(|v| v.as_str().map(|s| s.to_string()))
                        .unwrap_or_else(|| "compile error".to_string()),
                    None => "compile error".to_string(),
                };
                self.error(span, msg);
                Err(Diverge::CompileError)
            }
            "@compileLog" => {
                let mut parts = Vec::new();
                for a in args {
                    match self.eval_expr(env, a) {
                        Ok(v) => parts.push(self.display_value(&v)),
                        Err(Diverge::NotComptime) => parts.push("<runtime>".to_string()),
                        Err(other) => return Err(other),
                    }
                }
                self.warn(span, format!("@compileLog: {}", parts.join(", ")));
                Ok(Value::Void)
            }
            "@panic" => {
                self.comptime_error(span, "reached `@panic` at comptime");
                Err(Diverge::CompileError)
            }
            _ => Err(Diverge::NotComptime),
        }
    }

    /// The arena type a comptime value inhabits (for recording a builtin's
    /// result type so the checker is concrete instead of Deferred).
    pub(crate) fn value_type(&mut self, v: &Value) -> TypeId {
        match v {
            Value::Int(ci) => ci.ty,
            Value::Float { ty, .. } => *ty,
            Value::Bool(_) => self.arena.t_bool(),
            Value::Str(_) => self.arena.t_str(),
            Value::Type(_) => self.arena.t_type(),
            Value::Struct { ty, .. } => *ty,
            Value::Enum { ty, .. } => *ty,
            Value::Union { ty, .. } => *ty,
            Value::Array { ty, .. } => *ty,
            Value::ErrVal { set, .. } => *set,
            Value::Void => self.arena.t_void(),
            Value::Undefined(t) => *t,
            // Anonymous / tuple / fn values have no single concrete arena type
            // yet: leave them deferred for the surrounding checker.
            Value::Tuple(_) | Value::AnonTagged { .. } | Value::AnonStruct(_) | Value::Fn(_) => {
                self.arena.t_deferred()
            }
        }
    }

    /// Bounds-checks a fixed builtin argument before indexing it, emitting a
    /// precise `builtin @X expects N argument(s)` diagnostic (and returning
    /// [`Diverge::CompileError`]) when the call is under-applied. This is the one
    /// guard that turns a malformed reflection call into a diagnostic instead of
    /// the index-panic that crashed the compiler in v0.6 (spec §07.10: comptime
    /// must never panic). `expected` is the builtin's full required arity, used
    /// only for the message.
    fn arg_at<'a>(
        &mut self,
        builtin: &str,
        args: &'a [Expr],
        n: usize,
        expected: usize,
        span: Span,
    ) -> Result<&'a Expr, Diverge> {
        match args.get(n) {
            Some(a) => Ok(a),
            None => {
                self.comptime_error(
                    span,
                    format!(
                        "builtin `{builtin}` expects {expected} argument(s), found {}",
                        args.len()
                    ),
                );
                Err(Diverge::CompileError)
            }
        }
    }

    /// Evaluates the `n`th argument expecting a `type` value.
    fn arg_type(&mut self, env: &mut Env, args: &[Expr], n: usize) -> Result<TypeId, Diverge> {
        let a = args.get(n).ok_or(Diverge::NotComptime)?;
        // First try the comptime-env type evaluation (a bound `T` param).
        match self.eval_type_comptime(env, a) {
            Ok(t) if !self.arena.is_bottom(t) => Ok(t),
            _ => match self.eval_expr(env, a)? {
                Value::Type(t) => Ok(t),
                _ => Err(Diverge::NotComptime),
            },
        }
    }

    /// `true` if `t` has a field/variant named `name`.
    fn type_has_field(&self, t: TypeId, name: &str) -> bool {
        match self.arena.get(t) {
            Type::Struct(id) => self.arena.structs[id.0 as usize]
                .fields
                .iter()
                .any(|f| f.name == name),
            Type::Enum(id) => self.arena.enums[id.0 as usize]
                .variants
                .iter()
                .any(|v| v.name == name),
            Type::Union(id) => self.arena.unions[id.0 as usize]
                .variants
                .iter()
                .any(|v| v.name == name),
            _ => false,
        }
    }

    /// A short display rendering of a comptime value, for `@compileLog`.
    fn display_value(&self, v: &Value) -> String {
        match v {
            Value::Int(ci) => ci.v.to_string(),
            Value::Float { v, .. } => v.to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Str(s) => format!("\"{s}\""),
            Value::Type(t) => self.arena.fmt(*t),
            Value::Void => "void".to_string(),
            _ => "<value>".to_string(),
        }
    }
}

/// Builds a [`FieldInfo`] for a synthesized descriptor field.
fn field(name: &str, ty: TypeId, span: Span) -> FieldInfo {
    FieldInfo {
        name: name.to_string(),
        ty,
        has_default: false,
        is_comptime: false,
        align: None,
        bit_offset: None,
        bit_width: None,
        span,
    }
}

/// The exact bit width of a type for **packed-struct field placement** (spec
/// §02). A bit-addressable type — an integer (its true width), `bool` (1), an
/// `enum` (its tag's width), or a nested `packed struct` (its total bit width) —
/// returns `Some(width)`; any other type (pointer/slice/array/`Auto` struct/
/// float) returns `None`, which the checker turns into a "field not
/// bit-addressable" diagnostic. This is the authority both the type checker and
/// the codegen mirror call so packing is byte-identical on both backends.
pub fn packed_bit_width(arena: &crate::arena::TypeArena, ty: TypeId) -> Option<u64> {
    packed_bit_width_depth(arena, ty, 0)
}

/// Depth-guarded recursion behind [`packed_bit_width`] (a nested packed struct
/// recurses through its fields).
fn packed_bit_width_depth(arena: &crate::arena::TypeArena, ty: TypeId, depth: u32) -> Option<u64> {
    if depth > 64 {
        return None;
    }
    match arena.get(ty) {
        Type::Int { bits, .. } => Some(match bits {
            IntBits::Fixed(n) => *n as u64,
            IntBits::Usize | IntBits::Isize => 64,
        }),
        Type::Bool => Some(1),
        Type::Enum(id) => {
            let tag = arena.enums[id.0 as usize].tag;
            packed_bit_width_depth(arena, tag, depth + 1)
        }
        Type::Struct(id) => {
            let info = &arena.structs[id.0 as usize];
            if !info.is_packed() {
                return None;
            }
            // Sum the nested packed struct's field widths.
            let mut bits = 0u64;
            for f in &info.fields {
                bits += packed_bit_width_depth(arena, f.ty, depth + 1)?;
            }
            Some(bits)
        }
        _ => None,
    }
}

/// The total bit width and the resulting byte size of a `packed struct` — the
/// sum of its fields' bit widths, with the byte size rounded so the backing
/// integer fits a natural width (`int_byte_size` of the bit total). Returns
/// `None` if any field is not bit-addressable or the total exceeds 128 bits.
fn packed_struct_size(arena: &crate::arena::TypeArena, fields: &[FieldInfo]) -> Option<(u64, u64)> {
    let mut bits = 0u64;
    for f in fields {
        bits += packed_bit_width(arena, f.ty)?;
    }
    if bits > 128 {
        return None;
    }
    let size = int_byte_size(IntBits::Fixed(bits as u16)).max(if bits == 0 { 0 } else { 1 });
    Some((bits, size))
}

/// The byte size of an integer type, rounding sub-byte widths up to a power of
/// two (`u1..u8`->1, `u9..u16`->2, `u17..u32`->4, …), `usize`/`isize`->8.
fn int_byte_size(bits: IntBits) -> u64 {
    match bits {
        IntBits::Usize | IntBits::Isize => 8,
        IntBits::Fixed(0) => 0,
        IntBits::Fixed(n) => (n as u64).div_ceil(8).next_power_of_two(),
    }
}

/// Rounds `x` up to the next multiple of `a` (a power of two).
fn round_up(x: u64, a: u64) -> u64 {
    if a <= 1 {
        return x;
    }
    (x + a - 1) & !(a - 1)
}

/// The reification cache type alias (struct content-key -> nominal id).
pub(crate) type ReifyStructCache = HashMap<String, TypeId>;
