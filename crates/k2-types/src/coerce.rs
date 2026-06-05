//! Literal coercion: integer-literal range checks against a sized target, the
//! `null`-against-optional rule, and the bidirectional special-casing for the
//! literals/initializers that need their expected type.
//!
//! These are the `check`-direction rules that cannot be done bottom-up: an
//! integer literal's *type* depends on the target it is checked against (and the
//! range check is only knowable there), `null` only makes sense against an
//! optional, and an anonymous initializer `.{...}` needs the target struct/array
//! type to type its fields.

use k2_syntax::{Expr, InitBody, Span, UnOp};

use crate::eval::parse_int_literal;
use crate::ty::{IntBits, Type, TypeId};

impl crate::check::Checker<'_> {
    /// `true` if `e` is a compile-time integer literal whose value is statically
    /// knowable for an at-the-coercion-site range check: a bare integer literal,
    /// or a `-`-negated integer literal. (Char literals also synthesize to
    /// `comptime_int`, but only a true integer literal carries the radix text the
    /// range check needs, so they go through the ordinary widening path.)
    pub(crate) fn is_int_literal_expr(&self, e: &Expr) -> bool {
        match e {
            Expr::Int { .. } => true,
            Expr::Unary {
                op: UnOp::Neg,
                operand,
                ..
            } => matches!(operand.as_ref(), Expr::Int { .. }),
            _ => false,
        }
    }

    /// Range-checks a (possibly `-`-negated) integer literal against `expected`,
    /// emitting the same out-of-range diagnostic the direct `const x: T = <lit>`
    /// path produces. Used by `@as(T, <lit>)` so a compile-time-known coercion
    /// that does not fit is an error at the coercion site (spec §02).
    pub(crate) fn check_int_value_against(&mut self, e: &Expr, expected: TypeId) {
        match e {
            Expr::Int { .. } => {
                let _ = self.check_int_literal(e, expected);
            }
            Expr::Unary {
                op: UnOp::Neg,
                operand,
                span,
            } => {
                if let Expr::Int { text, base, .. } = operand.as_ref() {
                    self.check_negated_int_literal(text, *base, *span, expected);
                }
            }
            _ => {}
        }
    }

    /// Range-checks `-<lit>` against a sized-integer `expected`. The negation is
    /// applied to the parsed magnitude before the fit test, so `@as(u8, -1)` and
    /// `@as(i8, -200)` are rejected while `@as(i8, -128)` is accepted.
    fn check_negated_int_literal(
        &mut self,
        text: &str,
        base: k2_syntax::IntBase,
        span: Span,
        expected: TypeId,
    ) {
        if self.arena.is_bottom(expected) {
            self.record(span, expected);
            return;
        }
        if let Type::Int { signed, bits } = self.arena.get(expected).clone() {
            match parse_int_literal(text, base).and_then(|v| v.checked_neg()) {
                Some(v) => {
                    if int_fits(v, signed, bits) {
                        self.record(span, expected);
                    } else {
                        let (lo, hi) = int_range(signed, bits);
                        self.error(
                            span,
                            format!(
                                "integer literal `-{}` out of range for `{}` ({}..={})",
                                trimmed(text),
                                self.arena.fmt(expected),
                                lo,
                                hi
                            ),
                        );
                    }
                }
                None => {
                    // A magnitude too large for `i128` to negate directly. It
                    // fits only a 128-bit (or wider) signed target, and then only
                    // when the magnitude is ≤ 2^127 (so `-mag` ≥ `i128::MIN`).
                    let fits = match int_width(signed, bits) {
                        w if w < 128 => false,
                        w if w > 128 => true,
                        _ => {
                            signed
                                && parse_int_literal(text, base).is_none()
                                && parse_uint_literal_eq_min(text, base)
                        }
                    };
                    if !fits {
                        let (lo, hi) = int_range(signed, bits);
                        self.error(
                            span,
                            format!(
                                "integer literal `-{}` out of range for `{}` ({}..={})",
                                trimmed(text),
                                self.arena.fmt(expected),
                                lo,
                                hi
                            ),
                        );
                    }
                }
            }
        } else {
            // A non-integer target (`@as(f64, -1)`): the float/comptime path
            // already accepts a negated comptime_int; nothing to range-check.
            self.record(span, expected);
        }
    }

    /// Checks an integer literal against an expected type, range-checking it when
    /// the target is a sized integer. Returns the type actually used.
    pub(crate) fn check_int_literal(&mut self, e: &Expr, expected: TypeId) -> TypeId {
        let (text, base, span) = match e {
            Expr::Int { text, base, span } => (text, *base, *span),
            _ => unreachable!("check_int_literal called on a non-int"),
        };
        if self.arena.is_bottom(expected) {
            let t = self.arena.t_comptime_int();
            self.record(span, expected);
            return self.coerce_or_bottom(t, expected);
        }
        match self.arena.get(expected).clone() {
            Type::Int { signed, bits } => {
                if let Some(v) = parse_int_literal(text, base) {
                    if int_fits(v, signed, bits) {
                        self.record(span, expected);
                        expected
                    } else {
                        let (lo, hi) = int_range(signed, bits);
                        self.error(
                            span,
                            format!(
                                "integer literal `{}` out of range for `{}` ({}..={})",
                                trimmed(text),
                                self.arena.fmt(expected),
                                lo,
                                hi
                            ),
                        );
                        self.arena.t_error()
                    }
                } else {
                    // A literal too large for `i128`. A value that does not fit
                    // `i128` cannot fit any int narrower than 128 bits, so against
                    // such a target it is trivially out of range. For a target of
                    // exactly 128 bits the value may still fit (a `u128` literal in
                    // `2^127..2^128-1`, or `i128`'s own `2^127` boundary), so we
                    // re-parse as `u128` and range-check; only wider-than-128 ints
                    // defer to the comptime engine.
                    if int_fits_unparsed(text, base, signed, bits) {
                        self.record(span, expected);
                        expected
                    } else {
                        let (lo, hi) = int_range(signed, bits);
                        self.error(
                            span,
                            format!(
                                "integer literal `{}` out of range for `{}` ({}..={})",
                                trimmed(text),
                                self.arena.fmt(expected),
                                lo,
                                hi
                            ),
                        );
                        self.arena.t_error()
                    }
                }
            }
            Type::ComptimeInt | Type::ComptimeFloat | Type::Float { .. } => {
                self.record(span, expected);
                expected
            }
            // An integer literal flows into the ok payload of an optional or
            // error-union target (`return 1;` from `fn() error{A}!u8`).
            Type::Optional(inner) => {
                let _ = self.check_int_literal(e, inner);
                self.record(span, expected);
                expected
            }
            Type::ErrorUnion { ok, .. } => {
                let _ = self.check_int_literal(e, ok);
                self.record(span, expected);
                expected
            }
            _ => {
                self.error(
                    span,
                    format!(
                        "expected `{}`, found integer literal",
                        self.arena.fmt(expected)
                    ),
                );
                self.arena.t_error()
            }
        }
    }

    /// Checks a `null` literal against an expected type: it must be an optional.
    pub(crate) fn check_null(&mut self, span: Span, expected: TypeId) -> TypeId {
        if self.arena.is_bottom(expected) {
            self.record(span, expected);
            return expected;
        }
        if matches!(self.arena.get(expected), Type::Optional(_)) {
            self.record(span, expected);
            expected
        } else {
            self.error(
                span,
                format!(
                    "`null` requires an optional type, found `{}`",
                    self.arena.fmt(expected)
                ),
            );
            self.arena.t_error()
        }
    }

    /// Checks an anonymous initializer `.{...}` against an expected struct/array.
    pub(crate) fn check_anon_init(
        &mut self,
        body: &InitBody,
        span: Span,
        expected: TypeId,
    ) -> TypeId {
        if self.arena.is_bottom(expected) {
            // Still synth field values to catch concrete errors inside.
            self.synth_init_body(body);
            self.record(span, expected);
            return expected;
        }
        match self.arena.get(expected).clone() {
            Type::Struct(id) => {
                if let InitBody::Fields(fields) = body {
                    for fi in fields {
                        let info = &self.arena.structs[id.0 as usize];
                        let ft = info.fields.iter().find(|f| f.name == fi.name).map(|f| f.ty);
                        match ft {
                            Some(ft) => {
                                self.check(&fi.value, ft);
                            }
                            None => {
                                let sname = self.arena.structs[id.0 as usize].name.clone();
                                self.error(
                                    fi.span,
                                    format!("no field `{}` on struct `{sname}`", fi.name),
                                );
                            }
                        }
                    }
                } else {
                    self.synth_init_body(body);
                }
                self.record(span, expected);
                expected
            }
            Type::Array { elem, .. } | Type::Slice { elem, .. } | Type::Vector { elem, .. } => {
                if let InitBody::Tuple(elems) = body {
                    for el in elems {
                        self.check(el, elem);
                    }
                } else {
                    self.synth_init_body(body);
                }
                self.record(span, expected);
                expected
            }
            _ => {
                // Anonymous init against a non-aggregate expectation: still synth.
                self.synth_init_body(body);
                self.record(span, expected);
                expected
            }
        }
    }

    /// Synthesizes (for effect) the values inside an init body.
    pub(crate) fn synth_init_body(&mut self, body: &InitBody) {
        match body {
            InitBody::Fields(fields) => {
                for f in fields {
                    self.synth(&f.value);
                }
            }
            InitBody::Tuple(elems) => {
                for e in elems {
                    self.synth(e);
                }
            }
        }
    }

    /// Coerces `from` to `expected`, returning `expected` on success or the
    /// bottom type unchanged.
    fn coerce_or_bottom(&self, _from: TypeId, expected: TypeId) -> TypeId {
        expected
    }

    /// The element type of an array/slice/array-pointer base, used by indexing.
    pub(crate) fn indexable_elem(&self, base: TypeId) -> Option<TypeId> {
        match self.arena.get(base).clone() {
            Type::Array { elem, .. } | Type::Slice { elem, .. } | Type::Vector { elem, .. } => {
                Some(elem)
            }
            Type::Pointer { pointee, .. } => match self.arena.get(pointee) {
                Type::Array { elem, .. } => Some(*elem),
                _ => None,
            },
            _ => None,
        }
    }

    /// The slice type produced by slicing an array/slice/array-pointer base.
    pub(crate) fn slice_of_base(&mut self, base: TypeId) -> Option<TypeId> {
        match self.arena.get(base).clone() {
            Type::Slice { is_const, elem } => Some(self.arena.slice(is_const, elem)),
            Type::Array { elem, .. } => Some(self.arena.slice(false, elem)),
            Type::Pointer { is_const, pointee } => match self.arena.get(pointee).clone() {
                Type::Array { elem, .. } => Some(self.arena.slice(is_const, elem)),
                _ => None,
            },
            _ => None,
        }
    }

    /// `true` if a type is a numeric (int/float/comptime) type.
    pub(crate) fn numeric(&self, t: TypeId) -> bool {
        matches!(
            self.arena.get(t),
            Type::Int { .. } | Type::Float { .. } | Type::ComptimeInt | Type::ComptimeFloat
        )
    }

    /// `true` if a type is an integer (sized or comptime).
    pub(crate) fn integral(&self, t: TypeId) -> bool {
        matches!(self.arena.get(t), Type::Int { .. } | Type::ComptimeInt)
    }
}

/// The trimmed display form of an integer literal lexeme (separators removed).
fn trimmed(text: &str) -> String {
    text.chars().filter(|c| *c != '_').collect()
}

/// Crate-visible wrapper around [`int_fits`] for the comptime-fold range check
/// in [`crate::expr`].
pub(crate) fn int_fits_pub(v: i128, signed: bool, bits: IntBits) -> bool {
    int_fits(v, signed, bits)
}

/// Crate-visible wrapper around [`int_range`] for diagnostics in [`crate::expr`].
pub(crate) fn int_range_pub(signed: bool, bits: IntBits) -> (i128, i128) {
    int_range(signed, bits)
}

/// The inclusive `(min, max)` range of a sized integer, as `i128`. `usize`/`isize`
/// use the conservative 64-bit target bounds.
fn int_range(signed: bool, bits: IntBits) -> (i128, i128) {
    let width = match bits {
        IntBits::Fixed(n) => n as u32,
        IntBits::Usize | IntBits::Isize => 64,
    };
    if width == 0 {
        return (0, 0);
    }
    if signed {
        // Guard `width >= 128` exactly as the unsigned branch does: `1i128 << 127`
        // already overflows `i128` (whose max is `2^127 - 1`), so for `i128` and
        // anything wider we cannot represent the true bounds — the widest range an
        // `i128`-backed check can express is `i128::MIN..=i128::MAX`. Without this
        // guard `i128` panics in debug ('shift left with overflow') and silently
        // wraps to a nonsense range in release; see the v0.5 review finding.
        if width >= 128 {
            (i128::MIN, i128::MAX)
        } else {
            let max = (1i128 << (width - 1)) - 1;
            let min = -(1i128 << (width - 1));
            (min, max)
        }
    } else {
        let max = if width >= 128 {
            i128::MAX
        } else {
            (1i128 << width) - 1
        };
        (0, max)
    }
}

/// `true` if `v` fits in the given sized integer.
fn int_fits(v: i128, signed: bool, bits: IntBits) -> bool {
    let (lo, hi) = int_range(signed, bits);
    v >= lo && v <= hi
}

/// The bit width of a sized integer, with `usize`/`isize` treated as the
/// conservative 64-bit target width. (`signed` is unused for the width itself
/// but kept in the signature so call sites read naturally.)
fn int_width(_signed: bool, bits: IntBits) -> u32 {
    match bits {
        IntBits::Fixed(n) => n as u32,
        IntBits::Usize | IntBits::Isize => 64,
    }
}

/// `true` if the literal's magnitude is exactly `2^127`, i.e. `-magnitude`
/// equals `i128::MIN`. Used to accept the single negated value (`i128::MIN`)
/// whose magnitude overflows the `i128` parse but is still in range.
fn parse_uint_literal_eq_min(text: &str, base: k2_syntax::IntBase) -> bool {
    matches!(crate::eval::parse_uint_literal(text, base), Some(v) if v == (i128::MAX as u128) + 1)
}

/// Decides whether a (non-negative) literal that overflowed the `i128` parse
/// fits the sized target. Such a value is ≥ 2^127, so it never fits an int
/// narrower than 128 bits. For a 128-bit target we re-parse as `u128` and bound
/// against the type's true max (`2^128-1` for `u128`, `2^127-1` for `i128`).
/// Wider-than-128-bit ints conservatively accept (their exact bound is for the
/// comptime engine). `usize`/`isize` use the conservative 64-bit width.
fn int_fits_unparsed(text: &str, base: k2_syntax::IntBase, signed: bool, bits: IntBits) -> bool {
    let width = int_width(signed, bits);
    if width < 128 {
        return false;
    }
    if width > 128 {
        return true;
    }
    match crate::eval::parse_uint_literal(text, base) {
        Some(v) => {
            if signed {
                v <= i128::MAX as u128
            } else {
                true // any `u128`-parseable value fits `u128`.
            }
        }
        // A value too large for `u128` cannot fit any 128-bit int.
        None => false,
    }
}
