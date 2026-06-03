//! # k2-opt — the MIR optimizer (v0.9)
//!
//! k2 — *Kardashev Type II*: total control over the machine, with zero waste.
//!
//! This crate turns a verified, monomorphized [`MirProgram`] into a
//! *behaviorally identical* but cheaper one. It is the v0.9 optimizer: a pass
//! pipeline run to a fixpoint, wired to the build mode the program was lowered
//! under.
//!
//! ## The contract
//!
//! The optimizer is **sound by construction**. Each pass only ever:
//! 1. replaces an operand/rvalue with a provably-equal one (const fold, copy
//!    propagation),
//! 2. deletes an instruction whose result is provably dead *and* whose evaluation
//!    is provably effect-free (DCE; an impure dead-result assign is demoted to an
//!    `Eval` so its effect is kept),
//! 3. rewrites the CFG preserving reachability and behavior (CFG simplification,
//!    inlining), or
//! 4. deletes a realized safety check whose success edge is provably always taken
//!    (check elimination, ReleaseSafe only).
//!
//! Three structural facts about the host system constrain every pass and are
//! never violated:
//!
//! * **Registers are 1:1 with MIR locals** in the VM, so [`MirProgram::verify`]'s
//!   `params[i] == local i+1` and dense-local invariants must hold after every
//!   pass. DCE renumbers *only* temporaries (never a param or the return slot).
//! * **Safety checks are split by build mode at lowering time.** In ReleaseFast
//!   there are no checks to strip; the optimizer's only safety-check work is
//!   removing *provably-redundant realized checks* in ReleaseSafe.
//! * **The VM decrements its budget once per executed instruction**, so fewer MIR
//!   instructions executed is the metric the bench harness reports.
//!
//! [`optimize`] runs `debug_assert!(prog.verify().is_empty())` after each
//! pipeline body and after each whole-program pass, so a miscompiling pass is
//! caught immediately in a test build.

mod consts;
mod facts;
mod pass;

#[cfg(test)]
mod tests;

pub use consts::IntReprLite;

use k2_mir::{BuildMode, MirProgram};

/// What the optimizer is allowed to do. Derived from the program's [`BuildMode`]
/// at the call site (see [`optimize_for_mode`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OptLevel {
    /// Debug: identity. The program is returned byte-for-byte unchanged.
    None,
    /// ReleaseSafe: the full pipeline EXCEPT no check stripping; instead, only
    /// provably-redundant realized checks are removed (the check eliminator).
    Safe,
    /// ReleaseFast: the full pipeline. Safety checks are already absent from the
    /// MIR (lowering), so this is pure speed; the check eliminator finds nothing
    /// to remove.
    Fast,
}

/// Per-run statistics for the bench harness and an optional `--opt-report`.
#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct OptStats {
    /// How many outer pass-manager iterations ran.
    pub iterations: u32,
    /// Constant operands/rvalues materialized by folding.
    pub const_folded: u32,
    /// Operands rewritten by constant/copy propagation.
    pub copies_propagated: u32,
    /// Statements removed by dead-code/dead-store elimination.
    pub stmts_removed: u32,
    /// Blocks removed by CFG simplification (merge + unreachable GC).
    pub blocks_removed: u32,
    /// Call sites inlined.
    pub calls_inlined: u32,
    /// Provably-redundant realized safety checks removed (ReleaseSafe only).
    pub checks_removed: u32,
}

/// Outer pass-manager iteration budget. Inlining a callee can expose constants
/// that fold inside the inlined copy, which can devirtualize a further call; we
/// loop the whole-program + per-function pipeline a few times to let those
/// chains settle. The corpus converges in one.
const OUTER_BUDGET: u32 = 3;

/// Optimizes a verified MIR program in place.
///
/// PRECONDITION: `prog.verify()` is empty and `prog.is_ok()`.
/// POSTCONDITION: `prog.verify()` is empty, observable behavior is identical, and
/// `prog.mode` is unchanged. [`OptLevel::None`] returns immediately.
pub fn optimize(prog: &mut MirProgram, level: OptLevel) -> OptStats {
    let mut stats = OptStats::default();
    if level == OptLevel::None {
        return stats;
    }
    debug_assert!(
        prog.verify().is_empty(),
        "optimize precondition: input MIR must verify"
    );

    let safe = level == OptLevel::Safe;
    // Inlining accounting is threaded across every outer pass so its
    // recursion/global/per-caller budgets are true program-wide totals; without
    // this the budgets reset each outer pass and a cyclic call graph re-unrolls,
    // blowing up compile time and code size superlinearly.
    let mut inline_state = pass::inline::InlineState::default();
    let mut iter = 0u32;
    loop {
        let mut changed = false;

        // Whole-program inlining/devirtualization first: it creates the
        // intra-function constant-argument opportunities the local pipeline then
        // folds away.
        changed |= pass::inline::run(prog, &mut stats, &mut inline_state);
        debug_assert!(prog.verify().is_empty(), "verify must hold after inlining");

        // The per-function local fixpoint.
        for fi in 0..prog.funcs.len() {
            changed |= run_fn_pipeline(prog, fi, safe, &mut stats);
        }

        // Drop functions no longer reachable from any entry (post-inlining).
        changed |= pass::inline::dead_function_elimination(prog, &mut stats);
        debug_assert!(
            prog.verify().is_empty(),
            "verify must hold after dead-function elimination"
        );

        iter += 1;
        stats.iterations = iter;
        if !changed || iter >= OUTER_BUDGET {
            break;
        }
    }

    debug_assert!(
        prog.verify().is_empty(),
        "optimize postcondition: output MIR must verify"
    );
    stats
}

/// Runs the per-function local pipeline to a fixpoint on `prog.funcs[fi]`.
///
/// The 6-step body is the fixpoint unit; see the crate docs for why this order is
/// chosen. The hard cap is a belt-and-suspenders guard — the corpus converges in
/// at most three iterations.
fn run_fn_pipeline(prog: &mut MirProgram, fi: usize, safe: bool, stats: &mut OptStats) -> bool {
    const FN_FIXPOINT_BUDGET: u32 = 10;
    let mut any = false;
    let mut iters = 0u32;
    loop {
        let mut changed = false;

        // The arena is borrowed immutably by the per-function passes; split the
        // borrow so we can pass `&prog.arena` alongside `&mut prog.funcs[fi]`.
        let MirProgram { arena, funcs, .. } = prog;
        let func = &mut funcs[fi];

        changed |= pass::const_fold::run(arena, func, stats);
        changed |= pass::copy_prop::run(arena, func, stats);
        changed |= pass::const_fold::run(arena, func, stats);
        if safe {
            changed |= pass::check_elim::run(arena, func, stats);
        }
        changed |= pass::dce::run(func, stats);
        changed |= pass::simplify_cfg::run(func, stats);

        debug_assert!(
            func_verifies(prog, fi),
            "verify must hold after the per-function pipeline body"
        );

        any |= changed;
        iters += 1;
        if !changed || iters >= FN_FIXPOINT_BUDGET {
            break;
        }
    }
    any
}

/// A debug-only single-function well-formedness probe used inside the fixpoint.
/// We re-run the whole-program `verify` (cheap on the small corpus) and check it
/// is clean, which subsumes the per-function invariants.
#[cfg(debug_assertions)]
fn func_verifies(prog: &MirProgram, _fi: usize) -> bool {
    prog.verify().is_empty()
}
#[cfg(not(debug_assertions))]
fn func_verifies(_prog: &MirProgram, _fi: usize) -> bool {
    true
}

/// Convenience: derive the [`OptLevel`] from the program's own [`BuildMode`] and
/// optimize. Debug is identity; ReleaseSafe keeps non-redundant checks;
/// ReleaseFast is pure speed.
pub fn optimize_for_mode(prog: &mut MirProgram) -> OptStats {
    let level = match prog.mode {
        BuildMode::Debug => OptLevel::None,
        BuildMode::ReleaseSafe => OptLevel::Safe,
        BuildMode::ReleaseFast => OptLevel::Fast,
    };
    optimize(prog, level)
}
