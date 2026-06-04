//! # k2-mir — mid-level IR, monomorphization, safety checks, and leak analysis
//!
//! k2 — *Kardashev Type II*: total control over the machine, with zero waste.
//!
//! This crate is the **MIR layer (v0.7)** of the k2 front-end. It consumes the
//! type-checked, comptime-folded program (a [`SourceFile`](k2_syntax::SourceFile)
//! plus the [`Resolved`](k2_resolve::Resolved) side-table plus the
//! [`Typed`](k2_types::Typed) result) and lowers it to a backend-agnostic,
//! executable **MIR** that the v0.8 bytecode VM runs.
//!
//! The pipeline this crate implements:
//!
//! 1. **Data model** ([`ir`]) — a typed, explicit, CFG-based, mostly-three-
//!    address IR. A [`MirProgram`] is a set of monomorphized [`MirFunction`]s
//!    sharing the type arena; each function is a graph of basic blocks ending in
//!    explicit terminators. There is **no hidden control flow**.
//! 2. **Lowering** ([`lower`]) — AST -> MIR. Desugars *all* control flow
//!    (`if`/`while`/`for`/`switch`, labeled `break`/`continue`, while
//!    continue-expr, short-circuit `and`/`or`), the cleanup/error forms
//!    (`defer`/`errdefer` LIFO with error-path-only `errdefer`, `try`, `catch`,
//!    `orelse`, optional `.?`, error-union construction/unwrap, payload
//!    captures), and method-call receiver sugar. It **monomorphizes** generics
//!    (one function per reached `(fn, comptime-arg)` instantiation, reusing the
//!    k2-types instantiation identity) and inlines comptime-known values as
//!    constants. Still-`Deferred` std/sys/build member calls become opaque
//!    [`Rvalue::Intrinsic`] nodes — lowering never fails on them.
//! 3. **Safety checks** ([`checks`]) — gated by [`BuildMode`]. In Debug &
//!    ReleaseSafe the lowerer inserts explicit bounds / integer-overflow /
//!    division-by-zero / narrowing-cast / lockstep-length checks and lowers
//!    `unreachable` to a trap; a failed check branches to a shared panic block.
//!    In ReleaseFast the checks never exist.
//! 4. **Leak / escape analysis** ([`leak`]) — a conservative, post-lowering pass
//!    that flags only the two unambiguous bug shapes (an obvious missing free in
//!    a simple linear scope; a pointer to a stack local that escapes via return),
//!    with **zero false positives** on the corpus.
//!
//! The pretty-printer ([`pretty`]) renders a readable MIR dump for the
//! `k2c mir` subcommand.
//!
//! ## Entry point
//!
//! [`lower_program`] lowers a fully type-checked file to MIR under a chosen
//! [`BuildMode`].

mod checks;
mod ir;
mod leak;
mod lower;
mod pretty;

#[cfg(test)]
mod tests;

pub use ir::{
    AggKind, ArithOp, BasicBlock, BinOp, BlockId, BuildMode, CastKind, CheckKind, Const, ConstData,
    ConstId, Diagnostic, DiscrKind, ErrTag, FnAbi, FnId, InstArgKey, InstId, IntrinsicPath,
    IntrinsicRoot, Linkage, Local, LocalId, LocalOrigin, MirFunction, MirProgram, Operand, Place,
    Proj, Rvalue, SafetyCheck, Severity, SliceMeta, Statement, Terminator, TrapReason, UnOp,
};
pub use pretty::dump_mir;

use k2_resolve::Resolved;
use k2_syntax::SourceFile;
use k2_types::Typed;

/// Lowers a fully type-checked file to MIR under `mode`.
///
/// The returned [`MirProgram`] carries every lowering + leak diagnostic in its
/// `diagnostics` field (errors there gate codegen; the caller decides exit
/// status). `Err` is returned only on a genuinely un-lowerable program (an
/// internal invariant that the checker should have prevented). Deferred
/// std/sys/build member calls become [`Rvalue::Intrinsic`] nodes and never fail
/// lowering. The `typed` value is taken by value because the program *moves* the
/// type arena out of it (so the VM owns the layouts).
pub fn lower_program(
    file: &SourceFile,
    resolved: &Resolved,
    typed: Typed,
    mode: BuildMode,
) -> Result<MirProgram, Vec<Diagnostic>> {
    lower::lower_program(file, resolved, typed, mode)
}

impl MirProgram {
    /// A well-formedness check used by tests and (debug-assert) by lowering:
    /// every block has a terminator, every referenced `BlockId`/`LocalId`/`FnId`
    /// is in range, and `params` is a prefix of `locals`. Returns a list of
    /// problems (empty when the program is well-formed).
    pub fn verify(&self) -> Vec<Diagnostic> {
        let mut problems = Vec::new();
        for (fi, func) in self.funcs.iter().enumerate() {
            if func.id.index() != fi {
                problems.push(Diagnostic::error(
                    func.span,
                    format!("fn `{}` id {} != slot {fi}", func.name, func.id.0),
                ));
            }
            // Local 0 is always the return-value slot; parameters follow it in
            // declaration order (locals 1..=params.len()).
            for (i, p) in func.params.iter().enumerate() {
                if p.index() != i + 1 {
                    problems.push(Diagnostic::error(
                        func.span,
                        format!(
                            "fn `{}` param {i} is local {} (expected {})",
                            func.name,
                            p.0,
                            i + 1
                        ),
                    ));
                }
            }
            for (bi, b) in func.blocks.iter().enumerate() {
                if b.id.index() != bi {
                    problems.push(Diagnostic::error(
                        func.span,
                        format!("fn `{}` block id {} != slot {bi}", func.name, b.id.0),
                    ));
                }
                // Every terminator references in-range blocks.
                let check_block = |id: BlockId, problems: &mut Vec<Diagnostic>| {
                    if id.index() >= func.blocks.len() {
                        problems.push(Diagnostic::error(
                            func.span,
                            format!(
                                "fn `{}` block {bi} jumps to out-of-range block {}",
                                func.name, id.0
                            ),
                        ));
                    }
                };
                match &b.term {
                    Terminator::Goto(t) => check_block(*t, &mut problems),
                    Terminator::Branch {
                        then_bb, else_bb, ..
                    } => {
                        check_block(*then_bb, &mut problems);
                        check_block(*else_bb, &mut problems);
                    }
                    Terminator::Switch {
                        targets, default, ..
                    } => {
                        for (_, t) in targets {
                            check_block(*t, &mut problems);
                        }
                        check_block(*default, &mut problems);
                    }
                    Terminator::Return { .. }
                    | Terminator::Trap { .. }
                    | Terminator::Unreachable => {}
                }
            }
            // Every Call references an in-range FnId, and no statement/terminator
            // references a local outside `0..locals.len()`.
            let nlocals = func.locals.len();
            for b in &func.blocks {
                for s in &b.stmts {
                    for l in s.referenced_locals() {
                        if l.index() >= nlocals {
                            problems.push(Diagnostic::error(
                                func.span,
                                format!(
                                    "fn `{}` block {} references undefined local {}",
                                    func.name, b.id.0, l.0
                                ),
                            ));
                        }
                    }
                    let rv = match s {
                        Statement::Assign { rvalue, .. } | Statement::Eval { rvalue, .. } => {
                            Some(rvalue)
                        }
                        _ => None,
                    };
                    if let Some(Rvalue::Call { func: callee, .. }) = rv {
                        if callee.index() >= self.funcs.len() {
                            problems.push(Diagnostic::error(
                                func.span,
                                format!("fn `{}` calls out-of-range fn {}", func.name, callee.0),
                            ));
                        }
                    }
                }
            }
            // No unreachable (non-entry, zero-predecessor) blocks remain: every
            // block other than the entry must be a successor of some terminator.
            let mut reachable = vec![false; func.blocks.len()];
            if !func.blocks.is_empty() {
                let mut stack = vec![func.entry.index()];
                reachable[func.entry.index()] = true;
                while let Some(bi) = stack.pop() {
                    for succ in func.blocks[bi].term.successors() {
                        let si = succ.index();
                        if si < reachable.len() && !reachable[si] {
                            reachable[si] = true;
                            stack.push(si);
                        }
                    }
                }
            }
            for (bi, r) in reachable.iter().enumerate() {
                if !r {
                    problems.push(Diagnostic::error(
                        func.span,
                        format!("fn `{}` has an unreachable block {bi}", func.name),
                    ));
                }
            }
        }
        problems
    }
}
