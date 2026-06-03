//! The comptime [`Value`] model: the data a sandboxed comptime evaluation
//! produces and consumes.
//!
//! This module is the one comptime-engine file that is *not* an `impl` block on
//! [`Checker`](crate::check::Checker): it owns pure data definitions plus a
//! handful of small, allocation-free accessors. Everything that needs the type
//! arena (constructing a value's arena type, comparing types, building a
//! reflection descriptor) lives in [`comptime`](crate::comptime),
//! [`generic`](crate::generic), or [`reflect`](crate::reflect).
//!
//! ## `type` is a first-class value
//!
//! The defining fact of k2 comptime (spec §07.5) is that `type` is itself a
//! value: [`Value::Type`] wraps an interned [`TypeId`]. Because the arena
//! interns nominal types by identity and the generic instantiation cache (see
//! [`generic`](crate::generic)) reuses one [`TypeId`] per distinct argument
//! tuple, type equality `List(u32) == List(u32)` is a `u32` compare. That is
//! what makes `T == U`, `info == .Struct`, and the `@Type`/`@typeInfo`
//! round-trip decidable at comptime.
//!
//! ## Representation choices
//!
//! Composite values hold a declared [`TypeId`] plus their contents in `Vec`s /
//! `Box`es, mirroring how [`Type`](crate::ty::Type) holds `TypeId`s rather than
//! `Box<Type>`. The integer working width is `i128`: an operation whose true
//! result would exceed `i128` is rejected at the producing op (matching the
//! existing `fold_comptime_int` contract) rather than silently wrapping.

use k2_resolve::DefId;

use crate::ty::TypeId;

/// A comptime integer: its exact value plus the arena type it carries.
///
/// `ty` is `comptime_int` for an unconstrained literal/result, or a sized
/// `Int`/`usize` once a coercion has fixed the width. `comptime_int` is
/// arbitrary precision in the language; v0.6 tracks it in `i128` and reports a
/// precise out-of-range diagnostic if an op would exceed `i128`, rather than
/// claiming a value it cannot represent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ComptimeInt {
    /// The exact integer value (the working width is `i128`).
    pub v: i128,
    /// The arena type this integer inhabits (`comptime_int` or a sized int).
    pub ty: TypeId,
}

/// A compile-time-known value produced by the comptime evaluator.
///
/// Equality is structural and, crucially, [`Value::Type`] equality is a
/// `TypeId` compare — see the module docs. Every variant is allocation-free in
/// the sense that matters for the sandbox: its storage lives in the compiler's
/// own `Vec`/`Box`, never in a k2 runtime allocator.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Value {
    /// An integer (`comptime_int` or a sized int after coercion).
    Int(ComptimeInt),
    /// A float (`comptime_float` or a sized float). `f64` is the working width.
    Float {
        /// The exact value (the working width is `f64`).
        v: f64,
        /// The arena type this float inhabits.
        ty: TypeId,
    },
    /// A `bool`.
    Bool(bool),
    /// A string value (`[]const u8`): the decoded bytes of a string literal,
    /// used by `++`, `@typeName`, `@field`'s name argument, and
    /// `@compileError` messages.
    Str(String),
    /// `type` as a value: an interned arena [`TypeId`]. This single variant is
    /// the whole of generics and reflection.
    Type(TypeId),
    /// A struct value: its declared struct type plus the field values in
    /// declaration (layout) order.
    Struct {
        /// The declared struct type.
        ty: TypeId,
        /// The field values, one per declared field, in order.
        fields: Vec<Value>,
    },
    /// An enum value: its declared enum type plus the active variant index.
    Enum {
        /// The declared enum type.
        ty: TypeId,
        /// The active variant's declaration index.
        which: u32,
    },
    /// A tagged-union value: declared type, active variant, and payload.
    Union {
        /// The declared union type.
        ty: TypeId,
        /// The active variant's declaration index.
        which: u32,
        /// The active variant's payload value.
        payload: Box<Value>,
    },
    /// A fixed array / comptime sequence. Also used to carry the
    /// `@typeInfo(...).Struct.fields` descriptor list (an array of
    /// `StructField` structs) and `&.{...}` comptime slices.
    Array {
        /// The declared array/slice type, or `Deferred` for a bare sequence.
        ty: TypeId,
        /// The element values, in order.
        elems: Vec<Value>,
    },
    /// A positional tuple / anonymous `.{a, b}` whose target type is not yet
    /// known (it is resolved on coercion into a struct/array).
    Tuple(Vec<Value>),
    /// An anonymous single-key tagged value `.{ .Tag = payload }`, used for an
    /// untyped `@typeInfo`-shaped argument to `@Type` (`@Type(.{ .Int = ... })`)
    /// before its target union type is known. `payload` is the inner value.
    AnonTagged {
        /// The single active tag name.
        tag: String,
        /// The tag's payload value.
        payload: Box<Value>,
    },
    /// An anonymous multi-field initializer `.{ .a = x, .b = y }` whose target
    /// struct type is not yet known, carried as ordered `(name, value)` pairs.
    AnonStruct(Vec<(String, Value)>),
    /// An error value of a one-member error set (`error.Foo`).
    ErrVal {
        /// The (single-member) error set type.
        set: TypeId,
        /// The member name.
        name: String,
    },
    /// A reference to a callable definition (a fn used as a value / argument).
    Fn(DefId),
    /// `void`.
    Void,
    /// `undefined`: a value with a known *type* but unknown bits. Reading a
    /// field of it for reflection is allowed; using it numerically diverges.
    Undefined(TypeId),
}

impl Value {
    /// The integer value, if this is an [`Value::Int`].
    pub(crate) fn as_int(&self) -> Option<i128> {
        match self {
            Value::Int(ci) => Some(ci.v),
            _ => None,
        }
    }

    /// The boolean value, if this is a [`Value::Bool`].
    pub(crate) fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// The wrapped [`TypeId`], if this is a [`Value::Type`].
    pub(crate) fn as_type(&self) -> Option<TypeId> {
        match self {
            Value::Type(t) => Some(*t),
            _ => None,
        }
    }

    /// The string bytes, if this is a [`Value::Str`].
    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }
}
