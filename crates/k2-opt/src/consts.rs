//! The constant kernel: a width-correct, VM-faithful evaluator for the subset of
//! [`Rvalue`]s the optimizer can fold, plus the `Const` equality and integer
//! representation helpers the lattice and check-eliminator share.
//!
//! ## Why this lives next to the VM semantics
//!
//! The single non-negotiable soundness property of constant folding is that the
//! value the *optimizer* computes for a constant expression is **bit-for-bit the
//! value the VM would have computed at runtime**. If they ever disagree, an
//! optimized program produces a different result than the unoptimized one — a
//! miscompile. To make that property auditable, every function in this module is
//! a direct mirror of the corresponding VM path in `k2-vm`'s `vm.rs`:
//!
//! * integer width/sign masking matches [`IntReprLite::normalize`] ==
//!   `IntRepr::normalize`;
//! * `Add`/`Sub`/`Mul` use `wrapping_*`, exactly as `eval_binary`;
//! * `Div`/`Rem` reproduce the VM's defined behavior for a zero divisor (`0`)
//!   and for the `MIN / -1` overflow case (the masked `MIN`);
//! * `Shl`/`Shr` reduce the shift amount with the same `rem_euclid` rule as
//!   `shift_amount`, and `Shr` is logical for unsigned and arithmetic for signed;
//! * comparisons use the operands' own width/sign, matching `int_cmp` +
//!   `operand_cmp_repr`.
//!
//! Floats are intentionally **not** folded for arithmetic: IEEE rounding and
//! `NaN`/`-0.0` corner cases plus the formatter's float rendering make a folded
//! float a behavior risk for zero instruction-count benefit on the corpus. We
//! fold integer/bool/compare/bitwise/shift and constant casts only — the exact
//! set the benchmarks exercise — and leave everything else untouched.

use k2_mir::{BinOp, CastKind, Const, UnOp};
use k2_types::{IntBits, Type, TypeArena, TypeId};

/// A compact integer representation (width + signedness) resolved from a
/// [`TypeId`]. This is a local mirror of `k2-vm`'s `IntRepr` so the optimizer can
/// mask/compare integers identically to the VM without depending on `k2-vm`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct IntReprLite {
    /// The bit width (`8`, `16`, …, `128`; `64` for `usize`/`isize`). `0` means an
    /// unbounded `comptime_int` that is never masked.
    pub width: u16,
    /// `true` for a signed integer.
    pub signed: bool,
}

impl IntReprLite {
    /// The unbounded `comptime_int` representation (no masking).
    pub const COMPTIME: IntReprLite = IntReprLite {
        width: 0,
        signed: true,
    };

    /// Masks `v` to this width and sign-extends a signed top bit — the exact
    /// `IntRepr::normalize` algorithm from the VM.
    pub fn normalize(self, v: i128) -> i128 {
        if self.width == 0 || self.width >= 128 {
            return v;
        }
        let bits = self.width as u32;
        let mask: u128 = (1u128 << bits) - 1;
        let masked = (v as u128) & mask;
        if self.signed && (masked >> (bits - 1)) & 1 == 1 {
            (masked | !mask) as i128
        } else {
            masked as i128
        }
    }

    /// The inclusive maximum value of this repr.
    pub fn max_value(self) -> i128 {
        if self.width == 0 || self.width >= 128 {
            return i128::MAX;
        }
        let bits = self.width as u32;
        if self.signed {
            (1i128 << (bits - 1)) - 1
        } else {
            (1i128 << bits) - 1
        }
    }

    /// The inclusive minimum value of this repr.
    pub fn min_value(self) -> i128 {
        if self.width == 0 || self.width >= 128 {
            return i128::MIN;
        }
        if self.signed {
            -(1i128 << (self.width as u32 - 1))
        } else {
            0
        }
    }
}

/// Resolves the [`IntReprLite`] of a type id, mirroring `k2-vm`'s `int_repr_of`
/// exactly (including the `usize`/`isize` -> 64-bit mapping and the
/// non-integer -> `COMPTIME` fallback).
pub fn int_repr_of(arena: &TypeArena, ty: TypeId) -> IntReprLite {
    match arena.get(ty) {
        Type::Int { signed, bits } => IntReprLite {
            width: match bits {
                IntBits::Fixed(n) => *n,
                IntBits::Usize | IntBits::Isize => 64,
            },
            signed: *signed,
        },
        Type::ComptimeInt => IntReprLite::COMPTIME,
        _ => IntReprLite::COMPTIME,
    }
}

/// The [`IntReprLite`] an arithmetic/bitwise/unary result should be masked to,
/// mirroring the VM's `FnCompiler::result_repr` **exactly**: the rvalue's own
/// result type, unless that is an unsized `comptime_int` (width 0) and the
/// destination local has a concrete sized integer repr — in which case the
/// destination repr is used so a comptime-folded sized value is masked to its real
/// width.
///
/// This is the soundness pin for finding v0.9-#3: when the optimizer folds a
/// comptime-typed `Binary`/`Unary` whose mathematical result exceeds the sized
/// destination's width, it must mask to the SAME repr the VM would (the
/// destination's), otherwise the optimized program keeps an unmasked wide value
/// the unoptimized VM would have truncated — a divergence. Comparisons (result
/// `bool`, width 0) and casts (which mask to the cast's own target type) do NOT
/// take this fallback, matching the VM.
pub fn result_repr(arena: &TypeArena, ty: TypeId, dst_ty: Option<TypeId>) -> IntReprLite {
    let r = int_repr_of(arena, ty);
    if r.width == 0 {
        if let Some(dty) = dst_ty {
            let dr = int_repr_of(arena, dty);
            if dr.width != 0 {
                return dr;
            }
        }
    }
    r
}

/// The TYPE a folded integer constant should carry. The folder masks the *value*
/// to [`result_repr`] (the sized destination repr when the rvalue's own result
/// type is an unsized `comptime_int`), but the propagated `Const::Int` must carry
/// a *type* that is consistent with that masked value — otherwise a negative or
/// otherwise comptime-typed fold flows downstream as `Type::ComptimeInt`, which
/// the native print formatter (and any backend deriving a field type from the
/// const) treats as a non-integer field and rejects in release builds (an
/// opt-vs-unopt native divergence). So when the rvalue result type is `comptime_int`
/// (width 0) and a sized integer destination is available, stamp the constant with
/// the SIZED destination type. The value is already masked to that repr, so type
/// and value stay consistent and the constant is a genuine sized integer
/// everywhere downstream.
fn stamp_ty(arena: &TypeArena, ty: TypeId, dst_ty: Option<TypeId>) -> TypeId {
    if int_repr_of(arena, ty).width == 0 {
        if let Some(dty) = dst_ty {
            if int_repr_of(arena, dty).width != 0 {
                return dty;
            }
        }
    }
    ty
}

/// `true` if `ty` is a float type (so the folder skips it).
pub fn is_float_ty(arena: &TypeArena, ty: TypeId) -> bool {
    matches!(arena.get(ty), Type::Float { .. } | Type::ComptimeFloat)
}

/// Structural equality for the value-bearing `Const` variants. Two constants that
/// are *provably* the same runtime value compare equal; anything whose runtime
/// identity the optimizer cannot be certain of (`Undef`, `Aggregate`, `Str`,
/// `EmptySlice`) compares **unequal**, so a fold/propagation never assumes an
/// equality it cannot guarantee.
///
/// Integers compare by numeric value (after the producer already normalized them
/// to their type), matching the VM's `values_eq`, which ignores the carried repr.
pub fn const_eq(a: &Const, b: &Const) -> bool {
    match (a, b) {
        (Const::Int { value: x, .. }, Const::Int { value: y, .. }) => x == y,
        (Const::Bool(x), Const::Bool(y)) => x == y,
        (Const::Void, Const::Void) => true,
        (Const::EnumVal { variant: x, .. }, Const::EnumVal { variant: y, .. }) => x == y,
        (Const::ErrVal { tag: x, .. }, Const::ErrVal { tag: y, .. }) => x == y,
        // Floats: compare by bit pattern only when both are floats; this is used
        // for branch folding on `==`/`!=` between identical literals, never for
        // arithmetic. A `NaN` bit pattern equals itself here, which is *more*
        // conservative than IEEE `==`; since we never fold a float `==` whose
        // result we are unsure of (see `fold_compare`), this is safe.
        _ => false,
    }
}

/// The integer value of a `Const`, if it reads as one (ints, bools, enum tags,
/// error tags), mirroring `Value::as_i128`.
pub fn const_as_i128(c: &Const) -> Option<i128> {
    match c {
        Const::Int { value, .. } => Some(*value),
        Const::Bool(b) => Some(*b as i128),
        Const::EnumVal { variant, .. } => Some(*variant as i128),
        Const::ErrVal { tag, .. } => Some(tag.0 as i128),
        _ => None,
    }
}

/// The repr a `Const` integer carries (its own type's repr), or `None` if it is
/// not a plain sized/comptime integer. Used to compare two operands under the
/// operand width, mirroring `operand_cmp_repr`.
fn const_repr(arena: &TypeArena, c: &Const) -> Option<IntReprLite> {
    match c {
        Const::Int { ty, .. } => Some(int_repr_of(arena, *ty)),
        _ => None,
    }
}

/// Folds a binary op over two constants, returning the resulting `Const` iff the
/// fold is sound and the inputs are foldable integers/bools. Floats are never
/// folded (returns `None`). `ty` is the *result* type of the `Binary` rvalue;
/// `dst_ty` is the type of the local/place the result is stored into (or `None`
/// for an `Eval` whose result is discarded), used by [`result_repr`] to mask a
/// comptime-typed result to its sized destination exactly as the VM does.
pub fn fold_binary(
    arena: &TypeArena,
    op: BinOp,
    lhs: &Const,
    rhs: &Const,
    ty: TypeId,
    dst_ty: Option<TypeId>,
) -> Option<Const> {
    // Refuse anything but integer-like operands; this rejects floats, strings,
    // aggregates, undef, etc. (matching the VM's integer path; the VM's float
    // path is deliberately not mirrored here).
    let x = const_as_i128(lhs)?;
    let y = const_as_i128(rhs)?;

    // Arithmetic/bitwise/shift produce an integer of the *result* repr, falling
    // back to the destination's sized repr for a comptime-typed (width-0) result —
    // exactly the VM's `result_repr`. Comparisons (below) use the OPERANDS' repr.
    let res_repr = result_repr(arena, ty, dst_ty);
    // The type the folded integer constant carries: the sized destination when the
    // rvalue's own result type is `comptime_int` (see `stamp_ty`), so a folded
    // value never propagates as a bare `comptime_int` the backend cannot format.
    let res_ty = stamp_ty(arena, ty, dst_ty);

    // Comparisons use the OPERANDS' repr (the result repr is `bool`, width 0),
    // exactly like the VM. Prefer whichever operand is a sized integer.
    let cmp_repr = const_repr(arena, lhs)
        .filter(|r| r.width != 0)
        .or_else(|| const_repr(arena, rhs).filter(|r| r.width != 0))
        .unwrap_or(res_repr);

    let v = match op {
        BinOp::Add => Const::Int {
            value: res_repr.normalize(x.wrapping_add(y)),
            ty: res_ty,
        },
        BinOp::Sub => Const::Int {
            value: res_repr.normalize(x.wrapping_sub(y)),
            ty: res_ty,
        },
        BinOp::Mul => Const::Int {
            value: res_repr.normalize(x.wrapping_mul(y)),
            ty: res_ty,
        },
        BinOp::Div => {
            let v = if y == 0 {
                0
            } else if res_repr.signed && x == res_repr.min_value() && y == -1 {
                res_repr.min_value()
            } else {
                x.wrapping_div(y)
            };
            Const::Int {
                value: res_repr.normalize(v),
                ty: res_ty,
            }
        }
        BinOp::Rem => {
            let v = if y == 0 { 0 } else { x.wrapping_rem(y) };
            Const::Int {
                value: res_repr.normalize(v),
                ty: res_ty,
            }
        }
        BinOp::BitAnd => Const::Int {
            value: res_repr.normalize(x & y),
            ty: res_ty,
        },
        BinOp::BitOr => Const::Int {
            value: res_repr.normalize(x | y),
            ty: res_ty,
        },
        BinOp::BitXor => Const::Int {
            value: res_repr.normalize(x ^ y),
            ty: res_ty,
        },
        BinOp::Shl => {
            let sh = shift_amount(y, res_repr);
            Const::Int {
                value: res_repr.normalize(x.wrapping_shl(sh)),
                ty: res_ty,
            }
        }
        BinOp::Shr => {
            let sh = shift_amount(y, res_repr);
            let v = if res_repr.signed {
                x >> sh
            } else {
                ((x as u128) >> sh) as i128
            };
            Const::Int {
                value: res_repr.normalize(v),
                ty: res_ty,
            }
        }
        BinOp::Eq => Const::Bool(x == y),
        BinOp::Ne => Const::Bool(x != y),
        BinOp::Lt => Const::Bool(int_cmp(x, y, cmp_repr).is_lt()),
        BinOp::Le => Const::Bool(int_cmp(x, y, cmp_repr).is_le()),
        BinOp::Gt => Const::Bool(int_cmp(x, y, cmp_repr).is_gt()),
        BinOp::Ge => Const::Bool(int_cmp(x, y, cmp_repr).is_ge()),
    };
    Some(v)
}

/// Folds a unary op over a constant, mirroring `eval_unary`. Floats are not
/// folded (returns `None`). `ty` is the result type; `dst_ty` is the destination
/// local/place type (or `None` for an `Eval`), used by [`result_repr`] to mask a
/// comptime-typed `Neg`/`BitNot` result to its sized destination exactly as the
/// VM's Unary lowering does.
pub fn fold_unary(
    arena: &TypeArena,
    op: UnOp,
    operand: &Const,
    ty: TypeId,
    dst_ty: Option<TypeId>,
) -> Option<Const> {
    match op {
        UnOp::Not => match operand {
            Const::Bool(b) => Some(Const::Bool(!b)),
            // An integer `not` reads truthiness like `as_bool`.
            Const::Int { value, .. } => Some(Const::Bool(*value == 0)),
            _ => None,
        },
        UnOp::Neg => {
            let x = const_as_i128(operand)?;
            // A float operand would have `ty` float; refuse it (keep behavior).
            if is_float_ty(arena, ty) {
                return None;
            }
            let repr = result_repr(arena, ty, dst_ty);
            Some(Const::Int {
                value: repr.normalize(x.wrapping_neg()),
                ty: stamp_ty(arena, ty, dst_ty),
            })
        }
        UnOp::BitNot => {
            let x = const_as_i128(operand)?;
            let repr = result_repr(arena, ty, dst_ty);
            Some(Const::Int {
                value: repr.normalize(!x),
                ty: stamp_ty(arena, ty, dst_ty),
            })
        }
    }
}

/// Folds a constant cast, mirroring `eval_cast`. Only integer-targeted casts of
/// integer constants are folded (the corpus's `@truncate`/`@as`/`@intCast`);
/// float-involving casts are left untouched so the float path stays VM-owned.
pub fn fold_cast(arena: &TypeArena, kind: CastKind, operand: &Const, ty: TypeId) -> Option<Const> {
    // Never fold to/from float here.
    if is_float_ty(arena, ty) {
        return None;
    }
    match kind {
        CastKind::Widen | CastKind::IntNarrow => {
            let x = const_as_i128(operand)?;
            let repr = int_repr_of(arena, ty);
            Some(Const::Int {
                value: repr.normalize(x),
                ty,
            })
        }
        // Pointer reinterpret of a constant is rare and the operand is not an
        // integer we model; leave it. Int<->float casts stay VM-owned.
        CastKind::PtrReinterpret | CastKind::IntToFloat | CastKind::FloatToInt => None,
    }
}

/// The shift amount reduction, mirroring `shift_amount`.
fn shift_amount(y: i128, repr: IntReprLite) -> u32 {
    let w = if repr.width == 0 {
        128
    } else {
        repr.width as u32
    };
    ((y.rem_euclid(w as i128)) as u32) % w
}

/// Compares two raw `i128`s under `repr`, mirroring `int_cmp`: unsigned values
/// are compared as `u128` so a high-bit-set width (e.g. a `u128` whose top bit is
/// set, stored as a negative `i128`) orders correctly.
fn int_cmp(x: i128, y: i128, repr: IntReprLite) -> std::cmp::Ordering {
    if repr.signed {
        x.cmp(&y)
    } else {
        (x as u128).cmp(&(y as u128))
    }
}
