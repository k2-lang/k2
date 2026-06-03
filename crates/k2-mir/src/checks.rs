//! Safety-check realization: turning [`Statement::Check`] markers into explicit
//! branches to a shared panic block.
//!
//! Lowering inserts [`Statement::Check`] markers inline (gated by the build mode;
//! see [`crate::lower`]). This post-pass walks each function and *splits* every
//! block at its first remaining `Check`: the statements before the check stay,
//! the check becomes a [`Terminator::Branch`] on a freshly-computed "ok"
//! condition (true on success), the success edge continues to a new block holding
//! the rest of the statements, and the failure edge jumps to the function's
//! single shared panic block (whose terminator is a [`Terminator::Trap`] with the
//! reason derived from the check kind).
//!
//! Sharing one panic block per function keeps the CFG small while still making
//! the check *verifiable in the dump* (a `*_check ... else -> bb_panic` line) and
//! its absence in `ReleaseFast` equally verifiable (there are no `Check`
//! statements to split, so no panic edges appear).

use crate::ir::*;
use k2_types::TypeId;

/// Realizes every [`Statement::Check`] in `func` as a branch to the shared panic
/// block. Idempotent: a function with no checks is left unchanged.
pub(crate) fn split_checks(func: &mut MirFunction) {
    // Fast exit: nothing to do if there are no checks at all.
    let has_check = func
        .blocks
        .iter()
        .any(|b| b.stmts.iter().any(|s| matches!(s, Statement::Check(_))));
    if !has_check {
        return;
    }

    // Process blocks by index. Splitting appends new blocks, which we also visit
    // (their `Check`s, if any, get split in turn) — so iterate by a moving index.
    let mut bi = 0;
    while bi < func.blocks.len() {
        // Find the first Check in this block.
        let check_pos = func.blocks[bi]
            .stmts
            .iter()
            .position(|s| matches!(s, Statement::Check(_)));
        let Some(pos) = check_pos else {
            bi += 1;
            continue;
        };
        // Extract the check and the trailing statements.
        let Statement::Check(check) = func.blocks[bi].stmts[pos].clone() else {
            bi += 1;
            continue;
        };
        let trailing: Vec<Statement> = func.blocks[bi].stmts.split_off(pos + 1);
        // Drop the Check marker itself (it becomes the branch).
        func.blocks[bi].stmts.pop();

        // The continuation block holds the trailing statements + the old term.
        let cont = func.new_block();
        let old_term = std::mem::replace(&mut func.blocks[bi].term, Terminator::Unreachable);
        func.blocks[cont.index()].stmts = trailing;
        func.blocks[cont.index()].term = old_term;

        // The shared panic block (created lazily) for this reason set.
        let reason = trap_reason(&check.kind);
        let panic_bb = ensure_panic_block(func, reason);

        // A dump note naming the check, so Debug-mode safety scaffolding stays
        // verifiable even after the check is realized as a branch.
        func.blocks[bi]
            .stmts
            .push(Statement::Note(check_note(&check, panic_bb)));

        // Compute the "ok" condition into a fresh bool temp, then branch.
        let cond = emit_check_condition(func, bi, &check);
        func.blocks[bi].term = Terminator::Branch {
            cond,
            then_bb: cont,
            else_bb: panic_bb,
        };
        // Continue scanning the *continuation* (it may hold more checks); we do
        // not advance `bi` past it because new blocks were appended after it.
        bi += 1;
    }
}

/// Returns (creating if needed) a shared panic block for `func` whose terminator
/// traps with `reason`. To keep one block per reason we encode the reason in the
/// block; the first panic block created becomes `func.panic_block`.
fn ensure_panic_block(func: &mut MirFunction, reason: TrapReason) -> BlockId {
    // Reuse an existing panic block with the same reason if present.
    for b in &func.blocks {
        if b.is_panic {
            if let Terminator::Trap { reason: r } = b.term {
                if std::mem::discriminant(&r) == std::mem::discriminant(&reason) {
                    return b.id;
                }
            }
        }
    }
    let id = func.new_block();
    func.blocks[id.index()].is_panic = true;
    func.blocks[id.index()].term = Terminator::Trap { reason };
    if func.panic_block.is_none() {
        func.panic_block = Some(id);
    }
    id
}

/// Emits the statements computing a check's "ok" condition into block `bi`,
/// returning the boolean operand (true == safe).
fn emit_check_condition(func: &mut MirFunction, bi: usize, check: &SafetyCheck) -> Operand {
    // We model each check as a boolean computed by a small expression. To keep
    // the IR explicit and the VM trivial, we materialize the condition into a
    // fresh bool temp via a chain of Binary rvalues. For the milestone the exact
    // arithmetic-overflow predicate is left to the VM (it knows the type widths);
    // we emit a `CheckCond` placeholder bool that the VM evaluates from the check
    // payload. To stay fully explicit *and* dumpable, we encode the canonical
    // comparison where it is expressible with the operands we hold.
    let span = check.span;
    let bool_ty_temp = func.new_temp_bool();
    let stmt = |func: &mut MirFunction, rv: Rvalue| {
        func.blocks[bi].stmts.push(Statement::Assign {
            place: Place::local(bool_ty_temp),
            rvalue: rv,
            span,
        });
    };
    match &check.kind {
        CheckKind::Bounds { index, len } => {
            // ok = index < len  (and index >= 0 holds for usize).
            stmt(
                func,
                Rvalue::Binary {
                    op: BinOp::Lt,
                    lhs: index.clone(),
                    rhs: len.clone(),
                    ty: bool_ty(func),
                },
            );
        }
        CheckKind::SliceRange { lo, hi, len } => {
            // ok = (lo <= hi) and (hi <= len). Compute in two temps + And.
            let bt = bool_ty(func);
            let t1 = func.new_temp_bool();
            func.blocks[bi].stmts.push(Statement::Assign {
                place: Place::local(t1),
                rvalue: Rvalue::Binary {
                    op: BinOp::Le,
                    lhs: lo.clone(),
                    rhs: hi.clone(),
                    ty: bt,
                },
                span,
            });
            let t2 = func.new_temp_bool();
            func.blocks[bi].stmts.push(Statement::Assign {
                place: Place::local(t2),
                rvalue: Rvalue::Binary {
                    op: BinOp::Le,
                    lhs: hi.clone(),
                    rhs: len.clone(),
                    ty: bt,
                },
                span,
            });
            stmt(
                func,
                Rvalue::Binary {
                    op: BinOp::BitAnd,
                    lhs: Operand::local(t1),
                    rhs: Operand::local(t2),
                    ty: bool_ty(func),
                },
            );
        }
        CheckKind::DivByZero { b, ty } => {
            // ok = b != 0.
            stmt(
                func,
                Rvalue::Binary {
                    op: BinOp::Ne,
                    lhs: b.clone(),
                    rhs: Operand::Const(Const::Int { value: 0, ty: *ty }),
                    ty: bool_ty(func),
                },
            );
        }
        CheckKind::LenEq { a, b } => {
            // ok = a == b.
            stmt(
                func,
                Rvalue::Binary {
                    op: BinOp::Eq,
                    lhs: a.clone(),
                    rhs: b.clone(),
                    ty: bool_ty(func),
                },
            );
        }
        // The arithmetic-width predicates (overflow / negation / narrowing) are
        // VM-evaluated from the check payload, which the VM has the type widths
        // for. We emit an explicit `Intrinsic` boolean so the condition is still a
        // first-class, dumpable operand rather than hidden control flow.
        CheckKind::AddOverflow { op, a, b, ty } => {
            let name = match op {
                ArithOp::Add => "no_add_overflow",
                ArithOp::Sub => "no_sub_overflow",
                ArithOp::Mul => "no_mul_overflow",
            };
            stmt(
                func,
                Rvalue::Intrinsic {
                    path: IntrinsicPath {
                        root: IntrinsicRoot::Builtin(name.to_string()),
                        members: Vec::new(),
                        is_call: true,
                    },
                    args: vec![
                        a.clone(),
                        b.clone(),
                        Operand::Const(Const::Undef { ty: *ty }),
                    ],
                    ty: bool_ty(func),
                },
            );
        }
        CheckKind::NegOverflow { a, ty } => {
            stmt(
                func,
                Rvalue::Intrinsic {
                    path: IntrinsicPath {
                        root: IntrinsicRoot::Builtin("no_neg_overflow".to_string()),
                        members: Vec::new(),
                        is_call: true,
                    },
                    args: vec![a.clone(), Operand::Const(Const::Undef { ty: *ty })],
                    ty: bool_ty(func),
                },
            );
        }
        CheckKind::NarrowFits { value, ty } => {
            stmt(
                func,
                Rvalue::Intrinsic {
                    path: IntrinsicPath {
                        root: IntrinsicRoot::Builtin("narrow_fits".to_string()),
                        members: Vec::new(),
                        is_call: true,
                    },
                    args: vec![value.clone(), Operand::Const(Const::Undef { ty: *ty })],
                    ty: bool_ty(func),
                },
            );
        }
        CheckKind::Unreachable => {
            // ok = false (always trap).
            stmt(func, Rvalue::Use(Operand::Const(Const::Bool(false))));
        }
    }
    Operand::local(bool_ty_temp)
}

/// The `bool` type id, cached on the function by lowering so the check-splitting
/// pass can build boolean conditions without the arena.
fn bool_ty(func: &MirFunction) -> TypeId {
    func.bool_ty
        .expect("bool type must be set before splitting checks")
}

/// A short dump note naming the realized check and its panic target.
fn check_note(check: &SafetyCheck, panic_bb: BlockId) -> String {
    let name = match &check.kind {
        CheckKind::Bounds { .. } | CheckKind::SliceRange { .. } => "bounds_check",
        CheckKind::AddOverflow { .. } | CheckKind::NegOverflow { .. } => "overflow_check",
        CheckKind::DivByZero { .. } => "divzero_check",
        CheckKind::NarrowFits { .. } => "narrow_check",
        CheckKind::LenEq { .. } => "len_eq_check",
        CheckKind::Unreachable => "unreachable_check",
    };
    format!("{name} else -> bb{}", panic_bb.0)
}

/// Maps a check kind to the trap reason its failure reports.
fn trap_reason(kind: &CheckKind) -> TrapReason {
    match kind {
        CheckKind::Bounds { .. } | CheckKind::SliceRange { .. } => TrapReason::Bounds,
        CheckKind::AddOverflow { .. } => TrapReason::Overflow,
        CheckKind::DivByZero { .. } => TrapReason::DivByZero,
        CheckKind::NegOverflow { .. } => TrapReason::NegOverflow,
        CheckKind::NarrowFits { .. } => TrapReason::NarrowLoss,
        CheckKind::LenEq { .. } => TrapReason::LenMismatch,
        CheckKind::Unreachable => TrapReason::Unreachable,
    }
}
