//! The optimization passes.
//!
//! Each pass is a free function `run(...) -> bool` returning whether it changed
//! the program; the pass manager in [`crate`] sequences them and iterates to a
//! fixpoint. The passes split into two kinds:
//!
//! * **per-function** ([`const_fold`], [`copy_prop`], [`check_elim`], [`dce`],
//!   [`simplify_cfg`]) — they take `(&TypeArena, &mut MirFunction)` (or just the
//!   function) and never touch another function, so they run independently;
//! * **whole-program** ([`inline`]) — it rewrites call sites and the function
//!   table, so it owns the whole [`k2_mir::MirProgram`].

pub(crate) mod check_elim;
pub(crate) mod const_fold;
pub(crate) mod copy_prop;
pub(crate) mod dce;
pub(crate) mod inline;
pub(crate) mod simplify_cfg;

use k2_mir::{Operand, Place, Proj, Rvalue, Terminator};

/// Applies `f` to the index operands of any `Index` projections in `place`.
pub(crate) fn for_each_place_index_operand_mut<F: FnMut(&mut Operand)>(
    place: &mut Place,
    f: &mut F,
) {
    for proj in &mut place.proj {
        if let Proj::Index { index, .. } = proj {
            f(index);
        }
    }
}

/// Applies `f` to every operand directly read by an rvalue, including operands
/// inside the projections of a `Ref`'s place.
pub(crate) fn for_each_rvalue_operand_mut<F: FnMut(&mut Operand)>(rvalue: &mut Rvalue, f: &mut F) {
    match rvalue {
        Rvalue::Use(o)
        | Rvalue::MakeSome(o, _)
        | Rvalue::MakeOk(o, _)
        | Rvalue::MakeUnion { payload: o, .. }
        | Rvalue::Cast { operand: o, .. }
        | Rvalue::Unary { operand: o, .. }
        | Rvalue::Discriminant { operand: o, .. } => f(o),
        Rvalue::Ref { place, .. } => for_each_place_index_operand_mut(place, f),
        Rvalue::Binary { lhs, rhs, .. } => {
            f(lhs);
            f(rhs);
        }
        Rvalue::MakeSlice {
            ptr, offset, len, ..
        } => {
            f(ptr);
            f(offset);
            f(len);
        }
        Rvalue::Aggregate { fields, .. } => {
            for x in fields {
                f(x);
            }
        }
        Rvalue::Call { args, .. } => {
            for a in args {
                f(a);
            }
        }
        Rvalue::Intrinsic { path, args, .. } => {
            if let k2_mir::IntrinsicRoot::Value(op) = &mut path.root {
                f(op);
            }
            for a in args {
                f(a);
            }
        }
        Rvalue::MakeNull(_) | Rvalue::MakeErr(_, _) => {}
    }
}

/// Applies `f` to the condition/scrutinee/return operand of a terminator (the
/// only operands a terminator reads). Used by const-fold and copy-prop.
pub(crate) fn for_each_terminator_operand_mut<F: FnMut(&mut Operand)>(
    term: &mut Terminator,
    mut f: F,
) {
    match term {
        Terminator::Branch { cond, .. } => f(cond),
        Terminator::Switch { scrutinee, .. } => f(scrutinee),
        Terminator::Return { value, .. } => f(value),
        Terminator::Goto(_) | Terminator::Trap { .. } | Terminator::Unreachable => {}
    }
}

/// `true` if an rvalue is *pure*: evaluating it has no observable side effect AND
/// cannot fault, so a dead result means the whole statement can be deleted.
///
/// Two classes are impure:
/// * `Call`/`Intrinsic` — may print/alloc/trap (effects).
/// * any rvalue that reads a place through an `Index` or `Deref` projection — the
///   VM enforces memory safety and *traps* on an out-of-bounds index or a bad
///   pointer dereference even in ReleaseFast (it is a memory-safety guard, not a
///   language-level safety check). Deleting such a "dead" load would suppress a
///   trap the unoptimized program takes — a behavior change. A dead-result impure
///   rvalue is therefore demoted to an `Eval` by DCE, which keeps the load (and
///   its potential trap) while dropping only the unused write.
pub(crate) fn rvalue_is_pure(rvalue: &Rvalue) -> bool {
    if rvalue_reads_faulting_projection(rvalue) {
        return false;
    }
    match rvalue {
        Rvalue::Use(_)
        | Rvalue::Binary { .. }
        | Rvalue::Unary { .. }
        | Rvalue::Cast { .. }
        | Rvalue::Ref { .. }
        | Rvalue::MakeSlice { .. }
        | Rvalue::MakeSome(_, _)
        | Rvalue::MakeNull(_)
        | Rvalue::MakeOk(_, _)
        | Rvalue::MakeErr(_, _)
        | Rvalue::MakeUnion { .. }
        | Rvalue::Discriminant { .. }
        | Rvalue::Aggregate { .. } => true,
        Rvalue::Call { .. } | Rvalue::Intrinsic { .. } => false,
    }
}

/// `true` if the rvalue reads any operand whose place projects through an
/// `Index` or `Deref` (a load that the VM may fault on). A `Ref`'s place is an
/// address computation, not a load, so its projections do not fault — `&a[i]`
/// computes a pointer without dereferencing — and is excluded.
fn rvalue_reads_faulting_projection(rvalue: &Rvalue) -> bool {
    let mut faults = false;
    let mut check = |op: &mut Operand| {
        if let Operand::Copy(p) = op {
            if place_has_faulting_projection(p) {
                faults = true;
            }
        }
    };
    // A `Ref` does not load through its place; skip it explicitly.
    if let Rvalue::Ref { .. } = rvalue {
        return false;
    }
    // We need a `&Rvalue` walk, but `for_each_rvalue_operand_mut` takes `&mut`.
    // The caller only ever has a shared borrow here, so walk a throwaway clone's
    // operands — cheap for the small rvalues DCE inspects, and avoids duplicating
    // the operand-walk match.
    let mut tmp = rvalue.clone();
    for_each_rvalue_operand_mut(&mut tmp, &mut check);
    faults
}

/// `true` if a place's projection chain contains an `Index` or `Deref`.
fn place_has_faulting_projection(place: &Place) -> bool {
    place
        .proj
        .iter()
        .any(|p| matches!(p, Proj::Index { .. } | Proj::Deref))
}
