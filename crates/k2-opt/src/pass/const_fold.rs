//! Constant folding.
//!
//! This pass rewrites the *contents* of rvalues and terminators, never the
//! function's structure (no local/block/param/id changes), so it trivially
//! preserves [`k2_mir::MirProgram::verify`] — with one caveat: folding a
//! `Branch` whose condition is a constant `bool` into a `Goto` can leave the
//! other arm unreachable. That transiently violates the "no unreachable block"
//! invariant, which the trailing `simplify_cfg` in the same pipeline body
//! repairs via `gc_unreachable_blocks`. The pass manager only re-verifies after
//! that trailing step, so the window is closed before any assertion fires.
//!
//! What it folds:
//! * `Binary`/`Unary`/`Cast` over constant operands -> a single constant
//!   `Use` (using the VM-faithful kernel in [`crate::consts`]);
//! * a `Branch` on a constant `bool` -> an unconditional `Goto` to the taken arm;
//! * a `Switch` on a constant integer -> a `Goto` to the matching arm (or the
//!   default).

use k2_mir::{BasicBlock, Const, MirFunction, Operand, Place, Proj, Rvalue, Statement, Terminator};
use k2_types::{Type, TypeArena, TypeId};

use crate::consts::{const_as_i128, fold_binary, fold_cast, fold_unary};
use crate::OptStats;

/// Folds constant expressions and constant branches in `func`. Returns `true` if
/// anything changed.
pub(crate) fn run(arena: &TypeArena, func: &mut MirFunction, stats: &mut OptStats) -> bool {
    let mut changed = false;
    // We need the destination type of an `Assign` to mask a folded comptime-typed
    // result to its sized destination exactly as the VM does (see `fold_binary` /
    // `fold_unary` / `crate::consts::result_repr`). Borrow `locals` separately from
    // `blocks` so we can read a local's type while mutating a block's statements.
    let MirFunction { locals, blocks, .. } = func;
    for block in blocks.iter_mut() {
        changed |= fold_block_statements(arena, locals, block, stats);
        changed |= fold_terminator(block, stats);
    }
    changed
}

/// Folds each foldable rvalue in a block's statements into a constant `Use`.
fn fold_block_statements(
    arena: &TypeArena,
    locals: &[k2_mir::Local],
    block: &mut BasicBlock,
    stats: &mut OptStats,
) -> bool {
    let mut changed = false;
    for stmt in &mut block.stmts {
        // The destination type drives the comptime->sized masking fallback; an
        // `Eval` discards its result, so it has no destination (matching the VM's
        // `compile_stmt`, which passes `dst_ty = None` for `Eval`).
        let (dst_ty, rvalue) = match stmt {
            Statement::Assign { place, rvalue, .. } => {
                (Some(place_target_ty(arena, locals, place)), rvalue)
            }
            Statement::Eval { rvalue, .. } => (None, rvalue),
            _ => continue,
        };
        if let Some(folded) = try_fold_rvalue(arena, rvalue, dst_ty) {
            *rvalue = Rvalue::Use(Operand::Const(folded));
            stats.const_folded += 1;
            changed = true;
        }
    }
    changed
}

/// The type a place ultimately writes to (walking its projection chain), mirroring
/// the VM's `FnCompiler::place_target_ty` so the fold kernel resolves the same
/// destination repr the VM would.
fn place_target_ty(arena: &TypeArena, locals: &[k2_mir::Local], place: &Place) -> TypeId {
    let mut ty = locals[place.base.index()].ty;
    for proj in &place.proj {
        ty = match proj {
            Proj::Deref => match arena.get(ty) {
                Type::Pointer { pointee, .. } => *pointee,
                _ => ty,
            },
            Proj::Field { ty: fty, .. } => *fty,
            Proj::Index { ty: ety, .. } => *ety,
            Proj::SliceMeta { ty: mty, .. } => *mty,
            Proj::Payload { ty: pty } => *pty,
        };
    }
    ty
}

/// Attempts to fold an rvalue to a single constant. Returns `Some(const)` only
/// when every needed operand is already a constant and the kernel can evaluate it
/// soundly (integers/bools; floats are left untouched). `dst_ty` is the
/// destination place's type (or `None` for an `Eval`), threaded so a comptime
/// result is masked to its sized destination as the VM does.
fn try_fold_rvalue(arena: &TypeArena, rvalue: &Rvalue, dst_ty: Option<TypeId>) -> Option<Const> {
    match rvalue {
        Rvalue::Binary { op, lhs, rhs, ty } => {
            let l = const_of(lhs)?;
            let r = const_of(rhs)?;
            fold_binary(arena, *op, l, r, *ty, dst_ty)
        }
        Rvalue::Unary { op, operand, ty } => {
            let o = const_of(operand)?;
            fold_unary(arena, *op, o, *ty, dst_ty)
        }
        Rvalue::Cast { kind, operand, ty } => {
            let o = const_of(operand)?;
            fold_cast(arena, *kind, o, *ty)
        }
        // A `Use` of a constant is already folded; nothing to do. Everything else
        // (Ref, MakeSlice, aggregates, calls, …) is not a scalar fold target.
        _ => None,
    }
}

/// The constant inside an operand, if it is one.
fn const_of(op: &Operand) -> Option<&Const> {
    match op {
        Operand::Const(c) => Some(c),
        Operand::Copy(_) => None,
    }
}

/// Folds a constant-condition `Branch` or constant-scrutinee `Switch` into an
/// unconditional `Goto`. The now-unreachable arm is left for `simplify_cfg`.
fn fold_terminator(block: &mut BasicBlock, stats: &mut OptStats) -> bool {
    let new_term = match &block.term {
        Terminator::Branch {
            cond: Operand::Const(c),
            then_bb,
            else_bb,
        } => {
            let taken = match c {
                Const::Bool(b) => *b,
                // An integer condition (a comparison result stored as a const)
                // reads as truthy iff nonzero, matching `Value::as_bool`.
                Const::Int { value, .. } => *value != 0,
                _ => return false,
            };
            Some(Terminator::Goto(if taken { *then_bb } else { *else_bb }))
        }
        Terminator::Switch {
            scrutinee: Operand::Const(c),
            targets,
            default,
        } => match const_as_i128(c) {
            Some(v) => {
                let target = targets
                    .iter()
                    .find(|(val, _)| *val == v)
                    .map(|(_, t)| *t)
                    .unwrap_or(*default);
                Some(Terminator::Goto(target))
            }
            None => None,
        },
        _ => None,
    };
    if let Some(t) = new_term {
        block.term = t;
        stats.const_folded += 1;
        true
    } else {
        false
    }
}
