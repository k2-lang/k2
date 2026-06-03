//! Sound elimination of provably-redundant safety checks (ReleaseSafe only).
//!
//! By the time this pass runs, `k2-mir`'s check splitter has already turned every
//! `Statement::Check` into a *realized* branch: a block ends in
//! `Branch { cond, then_bb: cont, else_bb: panic }`, where `cond` is a freshly
//! computed "ok" boolean (`true` => safe) and `panic` is the function's shared
//! trap block. The preceding const-fold + copy-prop have, where the check's
//! operands were constants, folded that `cond`'s defining assignment into a
//! constant `Use(Const::Bool(b))`.
//!
//! This pass proves a check redundant by the *only* sound route available without
//! introducing a trap: it removes a check **iff** its `ok` condition is provably
//! `true`. Concretely, it finds a `Branch` whose `else_bb` is a panic block,
//! traces `cond` to the constant it was folded to (either inline in the
//! terminator or via the bool temp's defining `Use` in the same block), and if
//! that constant is truthy, rewrites the terminator to `Goto(then_bb)`. The
//! now-unreachable panic edge is collected by the trailing `simplify_cfg`, and
//! the now-dead bool-temp store is collected by DCE.
//!
//! It NEVER rewrites to `Goto(panic)` — we never introduce a trap on a maybe-live
//! path. A check whose condition is unknown (`Top`) or provably `false` is left
//! exactly as is, so ReleaseSafe keeps every check it cannot prove redundant.
//!
//! Because the const lattice over-approximates the real runtime states, a `cond`
//! that the lattice shows is the constant `true` is `true` in *every* execution —
//! so the removed panic edge was genuinely dead. That is the soundness argument.

use std::collections::HashMap;

use k2_mir::{Const, LocalId, MirFunction, Operand, Rvalue, Statement, Terminator};
use k2_types::TypeArena;

use crate::OptStats;

/// Removes provably-redundant realized safety checks in `func`. Returns `true` if
/// any check was eliminated.
pub(crate) fn run(_arena: &TypeArena, func: &mut MirFunction, stats: &mut OptStats) -> bool {
    // Which blocks are panic/trap blocks (the `else` target of a realized check).
    let is_panic: Vec<bool> = func.blocks.iter().map(|b| b.is_panic).collect();

    let mut changed = false;
    for bi in 0..func.blocks.len() {
        // Only a `Branch` whose else-arm is a panic block is a realized check.
        let (cond, then_bb, else_bb) = match &func.blocks[bi].term {
            Terminator::Branch {
                cond,
                then_bb,
                else_bb,
            } => (cond.clone(), *then_bb, *else_bb),
            _ => continue,
        };
        if !is_panic.get(else_bb.index()).copied().unwrap_or(false) {
            continue;
        }

        // Resolve the condition to a constant, either directly or via the bool
        // temp's defining `Use` within this block.
        let proven_true = match resolve_cond_const(&func.blocks[bi].stmts, &cond) {
            Some(Const::Bool(b)) => b,
            Some(Const::Int { value, .. }) => value != 0,
            _ => false,
        };

        if proven_true {
            // The success edge is always taken: drop the check.
            func.blocks[bi].term = Terminator::Goto(then_bb);
            stats.checks_removed += 1;
            changed = true;
        }
    }
    changed
}

/// Resolves a branch condition operand to the constant it holds, if any. Handles
/// both an inline `Const` operand and a `Copy(bool_temp)` whose temp is assigned a
/// constant `Use` earlier in the same block (the realized-check shape).
fn resolve_cond_const(stmts: &[Statement], cond: &Operand) -> Option<Const> {
    match cond {
        Operand::Const(c) => Some(c.clone()),
        Operand::Copy(p) if p.is_local() => {
            // Find the LAST assignment to this local in the block (the one that
            // reaches the terminator) and read its constant, if it is a `Use`.
            last_const_assignment(stmts, p.base)
        }
        Operand::Copy(_) => None,
    }
}

/// The constant value most recently assigned to bare local `l` by a
/// `Use(Const)` rvalue within `stmts`, if the final such assignment is a
/// constant. Returns `None` if `l` is assigned a non-constant after any constant,
/// or never assigned a constant.
fn last_const_assignment(stmts: &[Statement], l: LocalId) -> Option<Const> {
    // Track the latest known constant for `l`; a non-constant assign to `l` clears
    // it (the value reaching the terminator is then unknown).
    let mut latest: HashMap<LocalId, Option<Const>> = HashMap::new();
    for stmt in stmts {
        if let Statement::Assign { place, rvalue, .. } = stmt {
            if place.is_local() && place.base == l {
                match rvalue {
                    Rvalue::Use(Operand::Const(c)) => {
                        latest.insert(l, Some(c.clone()));
                    }
                    _ => {
                        latest.insert(l, None);
                    }
                }
            }
        }
    }
    latest.get(&l).cloned().flatten()
}
