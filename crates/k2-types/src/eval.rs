//! `eval_type`: the type-level interpreter that turns a type-position [`Expr`]
//! into a [`TypeId`].
//!
//! Because k2 types are ordinary expressions, every annotation, parameter type,
//! return type, `@as` target, field type, and array length is an `Expr`. This
//! module evaluates those expressions *as types*, separately from value
//! synthesis (see [`crate::check`]). It is where four of the language's type
//! constructors (optionals, pointers, slices, arrays), the error types, the
//! container literals, and the function types are interned, and where the
//! comptime-deferral boundary is honored for type-producing positions.

use k2_resolve::{DefKind, Resolution};
use k2_syntax::{Expr, IntBase};

use crate::ty::{ArrayLen, ErrSetRef, Type, TypeId};

impl crate::check::Checker<'_> {
    /// Evaluates a type-position expression into its [`TypeId`].
    pub(crate) fn eval_type(&mut self, e: &Expr) -> TypeId {
        match e {
            // ---- Primitive / named type paths ---------------------------
            Expr::Ident { name, span } => self.eval_type_ident(name, *span),

            // ---- Postfix-modifier chain --------------------------------
            Expr::Optional { inner, .. } => {
                let i = self.eval_type(inner);
                self.arena.optional(i)
            }
            Expr::Pointer {
                is_const, inner, ..
            } => {
                let p = self.eval_type(inner);
                self.arena.ptr(*is_const, p)
            }
            Expr::Slice {
                is_const, inner, ..
            } => {
                let el = self.eval_type(inner);
                self.arena.slice(*is_const, el)
            }
            Expr::ArrayType { len, inner, .. } => {
                let el = self.eval_type(inner);
                let l = self.eval_array_len(len);
                self.arena.intern(Type::Array { len: l, elem: el })
            }

            // ---- Error types -------------------------------------------
            Expr::ErrorSet { fields, .. } => {
                let id = self.arena.intern_errset(fields.clone());
                self.arena.intern(Type::ErrorSet(id))
            }
            Expr::ErrorUnion { err, ok, .. } => {
                let ok_ty = self.eval_type(ok);
                let err_ref = match err {
                    None => ErrSetRef::Inferred,
                    Some(e) => {
                        let et = self.eval_type(e);
                        self.errset_ref_of(et)
                    }
                };
                self.arena.intern(Type::ErrorUnion {
                    err: err_ref,
                    ok: ok_ty,
                })
            }
            // `(A || B)` in a type position merges two error sets.
            Expr::Binary {
                op: k2_syntax::BinOp::ErrSetMerge,
                lhs,
                rhs,
                ..
            } => self.eval_errset_merge(lhs, rhs),

            // ---- Fn types ----------------------------------------------
            Expr::FnType { params, ret, .. } => {
                let sig = self.build_fn_sig(params, ret);
                self.arena.intern_fn(sig)
            }
            Expr::AnyType { .. } => self.arena.t_anytype(),

            // ---- Container type literals -------------------------------
            Expr::Container(c) => self.eval_container(c, "struct"),

            // ---- Type-via-builtin / call -------------------------------
            Expr::Builtin { name, args, span } => self.eval_type_builtin(name, args, *span),
            Expr::Call { .. } => {
                // A type produced by a call (generic instantiation) is comptime;
                // still synth the call to catch concrete errors in its args.
                let _ = self.synth(e);
                self.arena.t_deferred()
            }
            Expr::Field { .. } => {
                // `std.X` / `info.Struct.fields` as a type: synth the member; on a
                // module/deferred base it is Deferred.
                let t = self.synth(e);
                // If the member synthed to `type`, we still cannot denote the
                // concrete type without comptime; treat as Deferred.
                if matches!(self.arena.get(t), Type::TypeType) {
                    self.arena.t_deferred()
                } else {
                    t
                }
            }
            Expr::Comptime { inner, .. } => self.eval_type(inner),

            // Anything else in a type position is not a type expression; synth it
            // and, if it is not `type`-valued, report. Deferred stays Deferred.
            _ => {
                let t = self.synth(e);
                if self.arena.is_bottom(t) {
                    return self.arena.t_deferred();
                }
                if matches!(self.arena.get(t), Type::TypeType) {
                    self.arena.t_deferred()
                } else {
                    self.error(
                        e.span(),
                        format!(
                            "expected a type, found a value of type `{}`",
                            self.arena.fmt(t)
                        ),
                    );
                    self.arena.t_error()
                }
            }
        }
    }

    /// Evaluates an identifier in a type position into the type it names.
    fn eval_type_ident(&mut self, name: &str, span: k2_syntax::Span) -> TypeId {
        match self.resolution_at(span) {
            Some(Resolution::Predeclared(_)) => self.predeclared_type(name),
            Some(Resolution::Def(id)) => {
                // A `const Name = <type-expr>` used as a type: the type it denotes.
                if let Some(&t) = self.item_types.get(&id) {
                    return t;
                }
                // A non-type binding (a value) used in a type position.
                let bt = self
                    .binding_types
                    .get(&id)
                    .copied()
                    .unwrap_or_else(|| self.arena.t_deferred());
                if self.arena.is_bottom(bt) {
                    return self.arena.t_deferred();
                }
                if matches!(self.arena.get(bt), Type::TypeType) {
                    // The binding is a type value but we have no denoted type
                    // cached (e.g. a generic-call result): defer.
                    self.arena.t_deferred()
                } else if matches!(self.resolved.defs[id.index()].kind, DefKind::Param) {
                    // A param used as a type (e.g. `value: T`): its denoted type
                    // is the param's own type if that is `type`, else Deferred.
                    self.arena.t_deferred()
                } else {
                    self.error(
                        span,
                        format!(
                            "expected a type, found value `{name}` of type `{}`",
                            self.arena.fmt(bt)
                        ),
                    );
                    self.arena.t_error()
                }
            }
            Some(Resolution::Module(id)) => self.arena.intern(Type::Module(module_id(self, id))),
            Some(Resolution::DeferredMember) => self.arena.t_deferred(),
            Some(Resolution::Error) | None => self.arena.t_error(),
        }
    }

    /// Maps a predeclared name to its [`Type`].
    pub(crate) fn predeclared_type(&mut self, name: &str) -> TypeId {
        if let Some((signed, bits)) = parse_int_name(name) {
            return self.arena.intern(Type::Int { signed, bits });
        }
        match name {
            "f16" => self.arena.intern(Type::Float { bits: 16 }),
            "f32" => self.arena.intern(Type::Float { bits: 32 }),
            "f64" => self.arena.intern(Type::Float { bits: 64 }),
            "f128" => self.arena.intern(Type::Float { bits: 128 }),
            "bool" => self.arena.t_bool(),
            "void" => self.arena.t_void(),
            "type" => self.arena.t_type(),
            "noreturn" => self.arena.t_noreturn(),
            "anyerror" => self.arena.t_anyerror(),
            "anyopaque" => self.arena.t_anyopaque(),
            "comptime_int" => self.arena.t_comptime_int(),
            "comptime_float" => self.arena.t_comptime_float(),
            "anytype" => self.arena.t_anytype(),
            // Capability / opaque predeclared types.
            "System" | "Allocator" | "Build" => self.arena.intern_opaque(name),
            // C-interop integer aliases: model as deferred-width ints (treated
            // permissively; none appear in the corpus).
            _ if name.starts_with("c_") => self.arena.t_deferred(),
            // A predeclared name that is not a type lattice member.
            _ => self.arena.t_deferred(),
        }
    }

    /// Evaluates an array length expression into an [`ArrayLen`].
    fn eval_array_len(&mut self, len: &Expr) -> ArrayLen {
        match len {
            Expr::Ident { name, .. } if name == "_" => ArrayLen::Inferred,
            Expr::Int { text, base, .. } => match parse_int_literal(text, *base) {
                Some(v) if v >= 0 => ArrayLen::Known(v as u64),
                _ => ArrayLen::Deferred,
            },
            // A comptime-computed length (a call, a const, an expression).
            _ => {
                // Still synth it to catch concrete errors.
                let _ = self.synth(len);
                ArrayLen::Deferred
            }
        }
    }

    /// Maps an evaluated type to the error-set reference used in an error union.
    fn errset_ref_of(&self, et: TypeId) -> ErrSetRef {
        match self.arena.get(et) {
            Type::ErrorSet(id) => ErrSetRef::Set(*id),
            Type::AnyError => ErrSetRef::Any,
            _ => ErrSetRef::Deferred,
        }
    }

    /// Merges two error sets (`A || B`) in a type position into one set.
    fn eval_errset_merge(&mut self, lhs: &Expr, rhs: &Expr) -> TypeId {
        let a = self.eval_type(lhs);
        let b = self.eval_type(rhs);
        let mut members = Vec::new();
        let mut concrete = true;
        for t in [a, b] {
            match self.arena.get(t).clone() {
                Type::ErrorSet(id) => {
                    members.extend(self.arena.errsets[id.0 as usize].members.iter().cloned());
                }
                Type::AnyError => return self.arena.t_anyerror(),
                _ if self.arena.is_bottom(t) => concrete = false,
                _ => concrete = false,
            }
        }
        if concrete {
            let id = self.arena.intern_errset(members);
            self.arena.intern(Type::ErrorSet(id))
        } else {
            self.arena.t_deferred()
        }
    }

    /// Evaluates a type-producing builtin in a type position.
    fn eval_type_builtin(&mut self, name: &str, args: &[Expr], _span: k2_syntax::Span) -> TypeId {
        match name {
            // `@TypeOf(e)` denotes the (possibly deferred) type of `e`.
            "@TypeOf" => {
                if let Some(first) = args.first() {
                    self.synth(first)
                } else {
                    self.arena.t_deferred()
                }
            }
            "@This" => self
                .self_stack
                .last()
                .copied()
                .unwrap_or_else(|| self.arena.t_deferred()),
            // Reflection / type-producing builtins are deferred; still synth args.
            "@typeInfo" | "@Type" | "@import" | "@field" | "@hasField" => {
                for a in args {
                    let _ = self.synth(a);
                }
                self.arena.t_deferred()
            }
            _ => {
                for a in args {
                    let _ = self.synth(a);
                }
                self.arena.t_deferred()
            }
        }
    }
}

/// Looks up (or interns) the resolver `ModuleId` behind a module def. The
/// resolver already gives us a `ModuleId` directly on the def.
fn module_id(c: &crate::check::Checker<'_>, def: k2_resolve::DefId) -> k2_resolve::ModuleId {
    c.resolved.defs[def.index()]
        .module
        .unwrap_or(k2_resolve::ModuleId(0))
}

/// Parses a predeclared integer type name (`u8`, `i32`, `usize`, `u1`, …) into
/// `(signed, bits)`. Returns `None` for a non-integer name.
fn parse_int_name(name: &str) -> Option<(bool, crate::ty::IntBits)> {
    use crate::ty::IntBits;
    match name {
        "usize" => return Some((false, IntBits::Usize)),
        "isize" => return Some((true, IntBits::Isize)),
        _ => {}
    }
    let bytes = name.as_bytes();
    let signed = match bytes.first() {
        Some(b'u') => false,
        Some(b'i') => true,
        _ => return None,
    };
    let digits = &name[1..];
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if digits.len() > 1 && digits.as_bytes()[0] == b'0' {
        return None;
    }
    let width: u32 = digits.parse().ok()?;
    if width > 65535 {
        return None;
    }
    Some((signed, IntBits::Fixed(width as u16)))
}

/// Parses an integer literal lexeme into an `i128` value, honoring the radix and
/// retaining digit separators. Returns `None` on overflow or a malformed lexeme.
pub(crate) fn parse_int_literal(text: &str, base: IntBase) -> Option<i128> {
    let (radix, body) = radix_and_body(text, base);
    i128::from_str_radix(&body, radix).ok()
}

/// Parses a (non-negative) integer literal into a `u128`, used to range-check a
/// literal that overflows `i128` against a 128-bit target (where the value may
/// still fit, e.g. `2^127 .. 2^128-1` into `u128`). Returns `None` if it does not
/// even fit `u128` (a value ≥ 2^128, which cannot fit any 128-bit int).
pub(crate) fn parse_uint_literal(text: &str, base: IntBase) -> Option<u128> {
    let (radix, body) = radix_and_body(text, base);
    u128::from_str_radix(&body, radix).ok()
}

/// Splits a literal lexeme into its `(radix, digit-body)`, stripping the base
/// prefix and digit separators.
fn radix_and_body(text: &str, base: IntBase) -> (u32, String) {
    let cleaned: String = text.chars().filter(|c| *c != '_').collect();
    match base {
        IntBase::Dec => (10, cleaned),
        IntBase::Hex => (
            16,
            cleaned.strip_prefix("0x").unwrap_or(&cleaned).to_string(),
        ),
        IntBase::Oct => (
            8,
            cleaned.strip_prefix("0o").unwrap_or(&cleaned).to_string(),
        ),
        IntBase::Bin => (
            2,
            cleaned.strip_prefix("0b").unwrap_or(&cleaned).to_string(),
        ),
    }
}
