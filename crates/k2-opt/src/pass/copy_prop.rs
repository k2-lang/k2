//! Constant & copy propagation.
//!
//! A flow-sensitive, RPO-iterated forward analysis tracks, for each local at
//! each program point, whether it is known to equal a constant
//! ([`Fact::Const`]) or to be a copy of another local ([`Fact::Copy`]). Reads of
//! such locals are then rewritten to the constant / the copied local. This is the
//! pass that turns `x = 1; y = x + 2;` into `y = 1 + 2`, which the following
//! const-fold collapses to `y = 3`.
//!
//! ## Soundness
//!
//! The analysis is sound because:
//! * **Address-taken locals are never tracked and never substituted.** A local
//!   whose address escaped may be mutated through an aliased pointer the pass
//!   cannot see, so its value is always [`Fact::Top`]. This is the single alias
//!   gate (see [`crate::facts`]).
//! * **Only bare-local definitions create facts.** An assign to a *projected*
//!   place (`a.f = …`, `a[i] = …`, `*p = …`) does not define the base to a known
//!   value; it conservatively kills the base's fact and any `Copy` facts that
//!   referenced it.
//! * **Joins meet predecessor states**: a local known to be the *same* constant /
//!   the *same* copy on every incoming edge keeps that fact; otherwise it is
//!   `Top`. We iterate over RPO until the per-block entry states stop changing.
//! * The substitution never changes a place's *base for a write*, only the values
//!   read, and a `Copy(M)` is only substituted when `M` is itself not
//!   address-taken (so reading `M` is equivalent to reading the original).
//!
//! The pass only rewrites operands — it adds/removes no locals, blocks, or
//! params — so it preserves `verify` unconditionally.

use std::collections::HashMap;

use k2_mir::{Const, LocalId, MirFunction, Operand, Place, Rvalue, Statement};

use crate::consts::const_eq;
use crate::facts::CfgFacts;
use crate::pass::{
    for_each_place_index_operand_mut, for_each_rvalue_operand_mut, for_each_terminator_operand_mut,
};
use crate::OptStats;

/// What is known about a local at a program point.
#[derive(Clone)]
enum Fact {
    /// Unknown — could be anything.
    Top,
    /// Known to equal this constant.
    Const(Const),
    /// Known to equal a copy of this (non-address-taken) local.
    Copy(LocalId),
    /// Known to be an array aggregate of this many elements. Used to fold a
    /// `.len` read of a statically-sized array literal into its constant length —
    /// the fact that makes a constant index into a constant-length array a
    /// provably-in-bounds access (see the check eliminator).
    ArrayLen(usize),
}

/// `Const` does not implement `PartialEq` (it carries `TypeId`/`ConstId` handles
/// whose structural equality is subtle), so `Fact` equality routes constants
/// through the value-aware [`const_eq`]. This is used only to detect a dataflow
/// fixpoint, so a *conservative* "not equal" (treating two equal-but-unprovable
/// constants as different) would merely cost an extra iteration, never
/// correctness.
impl PartialEq for Fact {
    fn eq(&self, other: &Fact) -> bool {
        match (self, other) {
            (Fact::Top, Fact::Top) => true,
            (Fact::Const(a), Fact::Const(b)) => const_eq(a, b),
            (Fact::Copy(a), Fact::Copy(b)) => a == b,
            (Fact::ArrayLen(a), Fact::ArrayLen(b)) => a == b,
            _ => false,
        }
    }
}

impl Fact {
    /// The meet (join in the dataflow sense) of two facts: equal facts survive,
    /// disagreements drop to `Top`.
    fn meet(&self, other: &Fact) -> Fact {
        match (self, other) {
            (Fact::Const(a), Fact::Const(b)) if const_eq(a, b) => Fact::Const(a.clone()),
            (Fact::Copy(a), Fact::Copy(b)) if a == b => Fact::Copy(*a),
            (Fact::ArrayLen(a), Fact::ArrayLen(b)) if a == b => Fact::ArrayLen(*a),
            _ => Fact::Top,
        }
    }
}

/// The per-local fact map at a program point.
type State = HashMap<LocalId, Fact>;

/// Runs constant & copy propagation on `func`. Returns `true` if any operand was
/// rewritten.
pub(crate) fn run(
    _arena: &k2_types::TypeArena,
    func: &mut MirFunction,
    stats: &mut OptStats,
) -> bool {
    let facts = CfgFacts::new(func);

    // Compute the entry state of every block by an RPO fixpoint, then do a single
    // rewriting walk using each block's entry state propagated through its
    // statements. Recomputing the in-block transfer during rewrite keeps the two
    // phases consistent.
    let entry_states = compute_entry_states(func, &facts);

    let mut changed = false;
    for block in &mut func.blocks {
        let mut state = entry_states
            .get(&block.id.index())
            .cloned()
            .unwrap_or_default();
        for stmt in &mut block.stmts {
            // Rewrite reads first (using the state *before* this statement's
            // definition takes effect).
            changed |= rewrite_stmt_reads(stmt, &state, &facts, stats);
            // Then update the state with this statement's effect.
            transfer_stmt(stmt, &mut state, &facts);
        }
        // Rewrite the terminator's operand using the block-exit state.
        changed |= rewrite_terminator_reads(&mut block.term, &state, &facts, stats);
    }
    changed
}

/// Computes each block's entry state via an RPO fixpoint over the meet of
/// predecessor exit states.
fn compute_entry_states(func: &MirFunction, facts: &CfgFacts) -> HashMap<usize, State> {
    let n = func.blocks.len();
    let mut entry: HashMap<usize, State> = HashMap::new();
    // The function entry block starts with no facts (params are unknown values).
    entry.insert(func.entry.index(), State::new());

    let mut iterating = true;
    let mut rounds = 0;
    // A finite-height lattice bounded by #locals * (#consts + #locals); a few
    // rounds suffice. Cap defensively.
    while iterating && rounds < n + 4 {
        iterating = false;
        rounds += 1;
        for &bid in &facts.rpo {
            let bi = bid.index();
            // Meet all predecessor exit states.
            let mut new_in: Option<State> = None;
            for &pred in &facts.preds[bi] {
                let pred_exit = exit_state_of(func, pred.index(), &entry, facts);
                new_in = Some(match new_in {
                    None => pred_exit,
                    Some(acc) => meet_states(&acc, &pred_exit),
                });
            }
            let new_in = match new_in {
                Some(s) => s,
                None => {
                    // No predecessors: only the entry block legitimately has
                    // none; keep its (empty) seed.
                    entry.get(&bi).cloned().unwrap_or_default()
                }
            };
            if entry.get(&bi) != Some(&new_in) {
                entry.insert(bi, new_in);
                iterating = true;
            }
        }
    }
    entry
}

/// Computes the exit state of block `bi` by running its statements' transfer over
/// its (already-computed) entry state.
fn exit_state_of(
    func: &MirFunction,
    bi: usize,
    entry: &HashMap<usize, State>,
    facts: &CfgFacts,
) -> State {
    let mut state = entry.get(&bi).cloned().unwrap_or_default();
    for stmt in &func.blocks[bi].stmts {
        transfer_stmt(stmt, &mut state, facts);
    }
    state
}

/// The meet of two states: a local is known only if both states agree.
fn meet_states(a: &State, b: &State) -> State {
    let mut out = State::new();
    for (k, va) in a {
        if let Some(vb) = b.get(k) {
            let m = va.meet(vb);
            if !matches!(m, Fact::Top) {
                out.insert(*k, m);
            }
        }
    }
    out
}

/// Updates `state` with the effect of `stmt`.
fn transfer_stmt(stmt: &Statement, state: &mut State, facts: &CfgFacts) {
    match stmt {
        Statement::Assign { place, rvalue, .. } => {
            if place.is_local() && !facts.is_address_taken(place.base) {
                // A bare-local definition: compute its new fact from the rvalue.
                let new_fact = fact_of_rvalue(rvalue, state, facts);
                kill_local(state, place.base);
                if !matches!(new_fact, Fact::Top) {
                    state.insert(place.base, new_fact);
                }
            } else {
                // A projected assign (or to an address-taken local): we do not
                // learn the base's value, and a store through it could change the
                // base, so kill the base's fact and any copies of it.
                kill_local(state, place.base);
            }
        }
        Statement::Eval { .. } => {
            // An evaluated-for-effect rvalue defines nothing.
        }
        Statement::StorageLive(l) | Statement::StorageDead(l) => {
            // Storage markers reset the slot; drop any stale fact.
            kill_local(state, *l);
        }
        Statement::Check(_) | Statement::Note(_) => {}
    }
}

/// Removes `l`'s fact and invalidates any `Copy(l)` facts (other locals that were
/// copies of `l` are no longer known to equal it after `l` is redefined).
fn kill_local(state: &mut State, l: LocalId) {
    state.remove(&l);
    state.retain(|_, f| !matches!(f, Fact::Copy(m) if *m == l));
}

/// Derives the fact a bare-local destination gets from its rvalue.
fn fact_of_rvalue(rvalue: &Rvalue, state: &State, facts: &CfgFacts) -> Fact {
    match rvalue {
        Rvalue::Use(Operand::Const(c)) => Fact::Const(c.clone()),
        Rvalue::Use(Operand::Copy(p)) if p.is_local() && !facts.is_address_taken(p.base) => {
            // `dst = src` where `src` is a tracked local: dst is a copy of src,
            // OR, if src is itself known constant / a known-length array, dst
            // inherits that fact.
            match state.get(&p.base) {
                Some(Fact::Const(c)) => Fact::Const(c.clone()),
                Some(Fact::Copy(m)) => Fact::Copy(*m),
                Some(Fact::ArrayLen(n)) => Fact::ArrayLen(*n),
                _ => Fact::Copy(p.base),
            }
        }
        // A statically-sized array literal: remember its element count so a later
        // `.len` read folds to a constant.
        Rvalue::Aggregate {
            kind: k2_mir::AggKind::Array,
            fields,
            ..
        } => Fact::ArrayLen(fields.len()),
        _ => Fact::Top,
    }
}

/// Rewrites every read in a statement using the current state. Returns whether
/// anything changed.
fn rewrite_stmt_reads(
    stmt: &mut Statement,
    state: &State,
    facts: &CfgFacts,
    stats: &mut OptStats,
) -> bool {
    let mut changed = false;
    match stmt {
        Statement::Assign { place, rvalue, .. } => {
            changed |= rewrite_place_indices(place, state, facts, stats);
            changed |= rewrite_rvalue_reads(rvalue, state, facts, stats);
        }
        Statement::Eval { rvalue, .. } => {
            changed |= rewrite_rvalue_reads(rvalue, state, facts, stats);
        }
        _ => {}
    }
    changed
}

/// Rewrites the index operands of a place.
fn rewrite_place_indices(
    place: &mut Place,
    state: &State,
    facts: &CfgFacts,
    stats: &mut OptStats,
) -> bool {
    let mut changed = false;
    for_each_place_index_operand_mut(place, &mut |op| {
        changed |= rewrite_operand(op, state, facts, stats);
    });
    changed
}

/// Rewrites every operand an rvalue reads.
fn rewrite_rvalue_reads(
    rvalue: &mut Rvalue,
    state: &State,
    facts: &CfgFacts,
    stats: &mut OptStats,
) -> bool {
    let mut changed = false;
    for_each_rvalue_operand_mut(rvalue, &mut |op| {
        changed |= rewrite_operand(op, state, facts, stats);
    });
    // Also rewrite index operands inside a `Ref`'s place projections, which the
    // operand walker above already covers via `for_each_place_index_operand_mut`.
    changed
}

/// Rewrites a terminator's read operand.
fn rewrite_terminator_reads(
    term: &mut k2_mir::Terminator,
    state: &State,
    facts: &CfgFacts,
    stats: &mut OptStats,
) -> bool {
    let mut changed = false;
    for_each_terminator_operand_mut(term, |op| {
        changed |= rewrite_operand(op, state, facts, stats);
    });
    changed
}

/// Rewrites a single operand: a `Copy(local)` whose local is known constant
/// becomes that constant; whose local is a known copy of another local becomes a
/// copy of that other local; a `Copy(base.len)` of a known-length array literal
/// becomes the constant length. Returns whether it changed.
fn rewrite_operand(
    op: &mut Operand,
    state: &State,
    facts: &CfgFacts,
    stats: &mut OptStats,
) -> bool {
    // Special case: a `.len` read of a statically-sized array literal.
    if let Some(folded) = try_fold_len_read(op, state, facts) {
        *op = folded;
        stats.copies_propagated += 1;
        return true;
    }

    let base = match op {
        Operand::Copy(p) if p.is_local() => p.base,
        _ => return false,
    };
    if facts.is_address_taken(base) {
        return false;
    }
    match state.get(&base) {
        Some(Fact::Const(c)) => {
            *op = Operand::Const(c.clone());
            stats.copies_propagated += 1;
            true
        }
        Some(Fact::Copy(m)) if *m != base && !facts.is_address_taken(*m) => {
            *op = Operand::local(*m);
            stats.copies_propagated += 1;
            true
        }
        _ => false,
    }
}

/// If `op` is `Copy(base.len)` where `base` is a known-length array literal,
/// returns the constant length operand of the projection's (`usize`) type.
fn try_fold_len_read(op: &Operand, state: &State, facts: &CfgFacts) -> Option<Operand> {
    let p = match op {
        Operand::Copy(p) => p,
        _ => return None,
    };
    // The place must be exactly `base.len` (a single trailing SliceMeta::Len).
    if p.proj.len() != 1 {
        return None;
    }
    let len_ty = match &p.proj[0] {
        k2_mir::Proj::SliceMeta {
            which: k2_mir::SliceMeta::Len,
            ty,
        } => *ty,
        _ => return None,
    };
    if facts.is_address_taken(p.base) {
        return None;
    }
    match state.get(&p.base) {
        Some(Fact::ArrayLen(n)) => Some(Operand::Const(Const::Int {
            value: *n as i128,
            ty: len_ty,
        })),
        _ => None,
    }
}
