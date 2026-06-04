//! Dead-code / dead-store / dead-local elimination.
//!
//! Three related cleanups, all driven by a classic backward liveness over bare
//! locals:
//!
//! 1. **Dead store**: an `Assign` to a bare local whose value is never read again
//!    and whose rvalue is *pure* is deleted. If the rvalue is *impure* (a `Call`
//!    or `Intrinsic` that may print/alloc/trap), the store is **not** deleted —
//!    instead it is demoted to an `Eval` so the effect is preserved but the dead
//!    write is dropped.
//! 2. **Dead local**: a local that is live nowhere (and is neither a parameter nor
//!    the return slot) is removed, and the remaining locals are densely
//!    renumbered.
//! 3. **Advisory cleanup**: `StorageLive`/`StorageDead`/`Note` for removed locals
//!    are dropped (they never affect execution; the VM ignores `Note`).
//!
//! ## Verify preservation
//!
//! The renumber contract keeps local 0 (the return slot) and locals
//! `1..=params.len()` at their indices, renumbering only temporaries; this
//! guarantees `params[i] == local i+1` survives. Every `Place.base`,
//! `Operand::Copy` base, `Index` operand, `StorageLive/Dead`, and terminator
//! operand is rewritten through the remap, so no reference dangles. Blocks and
//! terminators are otherwise untouched, so reachability is unchanged.
//!
//! Address-taken locals are never considered dead (their value may be observed
//! through a pointer), so their stores and slots always survive.

use std::collections::HashSet;

use k2_mir::{LocalId, MirFunction, Operand, Place, Proj, Rvalue, Statement, Terminator};

use crate::facts::reverse_postorder;
use crate::pass::rvalue_is_pure;
use crate::OptStats;

/// Runs dead-store + dead-local elimination on `func`. Returns `true` if anything
/// changed.
pub(crate) fn run(func: &mut MirFunction, stats: &mut OptStats) -> bool {
    let mut changed = false;
    // Iterate store-removal to a local fixpoint: removing one dead store can make
    // a previously-live local dead.
    loop {
        let live = compute_liveness(func);
        if !remove_dead_stores_mut(func, &live, stats) {
            break;
        }
        changed = true;
    }
    // Finally, drop globally-dead locals and renumber.
    changed |= remove_dead_locals(func, stats);
    changed
}

// =========================================================================
//  Liveness
// =========================================================================

/// `live_in[b]` / `live_out[b]` over bare locals, computed by a backward RPO
/// fixpoint. Returns the per-block live-in sets indexed by block index.
struct Liveness {
    /// Bare locals live on entry to each block.
    live_in: Vec<HashSet<LocalId>>,
}

/// Computes per-block live-in sets.
fn compute_liveness(func: &MirFunction) -> Liveness {
    let n = func.blocks.len();
    let mut live_in: Vec<HashSet<LocalId>> = vec![HashSet::new(); n];
    // Address-taken locals are treated as always-live: their value can be read
    // through an alias the analysis does not model. Seed them everywhere so no
    // store to them is ever removed.
    let always_live: HashSet<LocalId> = func
        .locals
        .iter()
        .filter(|l| l.address_taken)
        .map(|l| l.id)
        .collect();

    // Iterate to a fixpoint over reverse-RPO (a cheap approximation of postorder
    // for backward flow; correctness comes from iterating, order only affects
    // speed).
    let mut order = reverse_postorder(func);
    order.reverse();
    let mut changed = true;
    while changed {
        changed = false;
        for &bid in &order {
            let bi = bid.index();
            // live_out = union of successors' live_in.
            let mut live: HashSet<LocalId> = HashSet::new();
            for succ in func.blocks[bi].term.successors() {
                if succ.index() < n {
                    live.extend(live_in[succ.index()].iter().copied());
                }
            }
            // Terminator reads.
            for l in terminator_reads(&func.blocks[bi].term) {
                live.insert(l);
            }
            // Walk statements backward.
            for stmt in func.blocks[bi].stmts.iter().rev() {
                apply_stmt_backward(stmt, &mut live);
            }
            live.extend(always_live.iter().copied());
            if live != live_in[bi] {
                live_in[bi] = live;
                changed = true;
            }
        }
    }
    Liveness { live_in }
}

/// Updates the live set for a statement walked backward: remove the def, add the
/// uses.
fn apply_stmt_backward(stmt: &Statement, live: &mut HashSet<LocalId>) {
    match stmt {
        Statement::Assign { place, rvalue, .. } => {
            if place.is_local() {
                // A bare-local assign DEFs the base: it kills the base's liveness
                // (a later read is satisfied by this def). Then the rvalue's reads
                // are uses.
                live.remove(&place.base);
            } else {
                // A projected assign READS the base (and its index operands) and
                // does not fully define it.
                live.insert(place.base);
                for l in place_index_locals(place) {
                    live.insert(l);
                }
            }
            for l in rvalue_reads(rvalue) {
                live.insert(l);
            }
        }
        Statement::Eval { rvalue, .. } => {
            for l in rvalue_reads(rvalue) {
                live.insert(l);
            }
        }
        // Storage markers and notes do not affect liveness analysis (a
        // StorageLive/Dead is advisory; we never treat it as a use that keeps a
        // local alive, else a never-read local could not be collected).
        Statement::StorageLive(_) | Statement::StorageDead(_) | Statement::Note(_) => {}
        Statement::Check(_) => {}
    }
}

/// The bare locals a rvalue reads.
fn rvalue_reads(rvalue: &Rvalue) -> Vec<LocalId> {
    let mut out = Vec::new();
    collect_rvalue_reads(rvalue, &mut out);
    out
}

/// Appends the bare locals a rvalue reads (operands + ref'd place roots + index
/// operands) to `out`.
fn collect_rvalue_reads(rvalue: &Rvalue, out: &mut Vec<LocalId>) {
    let op = |o: &Operand, out: &mut Vec<LocalId>| {
        if let Operand::Copy(p) = o {
            collect_place_reads(p, out);
        }
    };
    match rvalue {
        Rvalue::Use(o)
        | Rvalue::MakeSome(o, _)
        | Rvalue::MakeOk(o, _)
        | Rvalue::Cast { operand: o, .. }
        | Rvalue::Unary { operand: o, .. }
        | Rvalue::Discriminant { operand: o, .. } => op(o, out),
        Rvalue::Ref { place, .. } => collect_place_reads(place, out),
        Rvalue::Binary { lhs, rhs, .. } => {
            op(lhs, out);
            op(rhs, out);
        }
        Rvalue::MakeSlice {
            ptr, offset, len, ..
        } => {
            op(ptr, out);
            op(offset, out);
            op(len, out);
        }
        Rvalue::Aggregate { fields, .. } => {
            for f in fields {
                op(f, out);
            }
        }
        Rvalue::Call { args, .. } => {
            for a in args {
                op(a, out);
            }
        }
        Rvalue::Intrinsic { path, args, .. } => {
            if let k2_mir::IntrinsicRoot::Value(o) = &path.root {
                op(o, out);
            }
            for a in args {
                op(a, out);
            }
        }
        Rvalue::MakeNull(_) | Rvalue::MakeErr(_, _) => {}
    }
}

/// Appends a place's root and any `Index`-projection locals to `out`.
fn collect_place_reads(place: &Place, out: &mut Vec<LocalId>) {
    out.push(place.base);
    for l in place_index_locals(place) {
        out.push(l);
    }
}

/// The locals appearing in a place's `Index` projections.
fn place_index_locals(place: &Place) -> Vec<LocalId> {
    let mut out = Vec::new();
    for proj in &place.proj {
        if let Proj::Index {
            index: Operand::Copy(p),
            ..
        } = proj
        {
            out.push(p.base);
        }
    }
    out
}

/// The bare locals a terminator reads.
fn terminator_reads(term: &Terminator) -> Vec<LocalId> {
    let mut out = Vec::new();
    let mut op = |o: &Operand| {
        if let Operand::Copy(p) = o {
            collect_place_reads(p, &mut out);
        }
    };
    match term {
        Terminator::Branch { cond, .. } => op(cond),
        Terminator::Switch { scrutinee, .. } => op(scrutinee),
        Terminator::Return { value, .. } => op(value),
        Terminator::Goto(_) | Terminator::Trap { .. } | Terminator::Unreachable => {}
    }
    out
}

// =========================================================================
//  Dead-local removal + renumber
// =========================================================================

/// Removes locals that are live nowhere (excluding params and the return slot)
/// and densely renumbers the survivors, rewriting every reference. Returns
/// whether anything changed.
fn remove_dead_locals(func: &mut MirFunction, stats: &mut OptStats) -> bool {
    let live = compute_liveness(func);
    // A local is "used" if it appears in any block's live-in, or is read/written
    // anywhere (a write to a never-read local still references the slot, but if it
    // is a dead store it should already be gone; to be safe we mark a local used
    // if it is read anywhere or is a param / return slot / address-taken).
    let nlocals = func.locals.len();
    let mut used = vec![false; nlocals];
    // Params and return slot are always kept.
    used[0] = true; // return slot (local 0)
    for p in &func.params {
        used[p.index()] = true;
    }
    for (i, l) in func.locals.iter().enumerate() {
        if l.address_taken {
            used[i] = true;
        }
    }
    // Any local live at a block entry is used.
    for set in &live.live_in {
        for l in set {
            if l.index() < nlocals {
                used[l.index()] = true;
            }
        }
    }
    // Any local read/written anywhere (statement or terminator) is used. A bare
    // store to an otherwise-dead local has already been removed by the dead-store
    // pass; what remains here are stores whose destinations are live or projected.
    for block in &func.blocks {
        for stmt in &block.stmts {
            for l in stmt_all_locals(stmt) {
                if l.index() < nlocals {
                    used[l.index()] = true;
                }
            }
        }
        for l in terminator_reads(&block.term) {
            if l.index() < nlocals {
                used[l.index()] = true;
            }
        }
    }

    if used.iter().all(|&u| u) {
        return false;
    }

    // Build the remap: keep slot 0 and params at their indices, renumber the rest
    // densely among the survivors, preserving order.
    let nparams = func.params.len();
    let mut remap: Vec<Option<u32>> = vec![None; nlocals];
    let mut next = (nparams + 1) as u32;
    for i in 0..nlocals {
        if i <= nparams {
            // Slot 0 (ret) and 1..=nparams (params) keep their index.
            remap[i] = Some(i as u32);
        } else if used[i] {
            remap[i] = Some(next);
            next += 1;
        }
    }

    let removed = used.iter().filter(|&&u| !u).count() as u32;

    // Rebuild the locals vector.
    let mut new_locals = Vec::with_capacity(next as usize);
    for (i, mut local) in std::mem::take(&mut func.locals).into_iter().enumerate() {
        if let Some(new) = remap[i] {
            local.id = LocalId(new);
            new_locals.push(local);
        }
    }
    func.locals = new_locals;

    // Rewrite params (indices unchanged, but rebuild to be safe).
    for p in &mut func.params {
        if let Some(new) = remap[p.index()] {
            *p = LocalId(new);
        }
    }

    // Rewrite every reference in every block + terminator.
    for block in &mut func.blocks {
        block
            .stmts
            .retain(|s| !is_storage_marker_for_removed(s, &remap));
        for stmt in &mut block.stmts {
            remap_stmt_locals(stmt, &remap);
        }
        remap_terminator_locals(&mut block.term, &remap);
    }

    // Removing a local also strips its storage markers (handled by the `retain`
    // above), which is the only statement-count change this step makes.
    stats.stmts_removed += removed;
    removed > 0
}

/// `true` if `s` is a `StorageLive`/`StorageDead` for a removed local.
fn is_storage_marker_for_removed(s: &Statement, remap: &[Option<u32>]) -> bool {
    match s {
        Statement::StorageLive(l) | Statement::StorageDead(l) => {
            l.index() >= remap.len() || remap[l.index()].is_none()
        }
        _ => false,
    }
}

/// Every local a statement references (reads and writes).
fn stmt_all_locals(stmt: &Statement) -> Vec<LocalId> {
    stmt.referenced_locals()
}

/// Rewrites every local id a statement references through `remap`.
fn remap_stmt_locals(stmt: &mut Statement, remap: &[Option<u32>]) {
    let map = |l: &mut LocalId| {
        if let Some(new) = remap[l.index()] {
            *l = LocalId(new);
        }
    };
    match stmt {
        Statement::Assign { place, rvalue, .. } => {
            remap_place(place, &map);
            remap_rvalue(rvalue, &map);
        }
        Statement::Eval { rvalue, .. } => remap_rvalue(rvalue, &map),
        Statement::StorageLive(l) | Statement::StorageDead(l) => map(l),
        Statement::Check(_) | Statement::Note(_) => {}
    }
}

/// Rewrites the base + index-operand locals of a place.
fn remap_place(place: &mut Place, map: &impl Fn(&mut LocalId)) {
    map(&mut place.base);
    for proj in &mut place.proj {
        if let Proj::Index { index, .. } = proj {
            remap_operand(index, map);
        }
    }
}

/// Rewrites the base of an operand (if it is a `Copy`).
fn remap_operand(op: &mut Operand, map: &impl Fn(&mut LocalId)) {
    if let Operand::Copy(p) = op {
        remap_place(p, map);
    }
}

/// Rewrites every local an rvalue references.
fn remap_rvalue(rvalue: &mut Rvalue, map: &impl Fn(&mut LocalId)) {
    match rvalue {
        Rvalue::Use(o)
        | Rvalue::MakeSome(o, _)
        | Rvalue::MakeOk(o, _)
        | Rvalue::Cast { operand: o, .. }
        | Rvalue::Unary { operand: o, .. }
        | Rvalue::Discriminant { operand: o, .. } => remap_operand(o, map),
        Rvalue::Ref { place, .. } => remap_place(place, map),
        Rvalue::Binary { lhs, rhs, .. } => {
            remap_operand(lhs, map);
            remap_operand(rhs, map);
        }
        Rvalue::MakeSlice {
            ptr, offset, len, ..
        } => {
            remap_operand(ptr, map);
            remap_operand(offset, map);
            remap_operand(len, map);
        }
        Rvalue::Aggregate { fields, .. } => {
            for f in fields {
                remap_operand(f, map);
            }
        }
        Rvalue::Call { args, .. } => {
            for a in args {
                remap_operand(a, map);
            }
        }
        Rvalue::Intrinsic { path, args, .. } => {
            if let k2_mir::IntrinsicRoot::Value(o) = &mut path.root {
                remap_operand(o, map);
            }
            for a in args {
                remap_operand(a, map);
            }
        }
        Rvalue::MakeNull(_) | Rvalue::MakeErr(_, _) => {}
    }
}

/// Rewrites the operand local of a terminator.
fn remap_terminator_locals(term: &mut Terminator, remap: &[Option<u32>]) {
    let map = |l: &mut LocalId| {
        if let Some(new) = remap[l.index()] {
            *l = LocalId(new);
        }
    };
    match term {
        Terminator::Branch { cond, .. } => remap_operand(cond, &map),
        Terminator::Switch { scrutinee, .. } => remap_operand(scrutinee, &map),
        Terminator::Return { value, .. } => remap_operand(value, &map),
        Terminator::Goto(_) | Terminator::Trap { .. } | Terminator::Unreachable => {}
    }
}

// =========================================================================
//  Dead-store removal (mutable implementation)
// =========================================================================

/// The real dead-store pass: walks each block backward over a live set seeded
/// from successors, deleting pure dead stores and demoting impure ones to `Eval`.
/// Returns whether anything changed. Invoked from [`run`] via the wrapper below.
fn remove_dead_stores_mut(func: &mut MirFunction, live: &Liveness, stats: &mut OptStats) -> bool {
    let n = func.blocks.len();
    let mut changed = false;
    for bi in 0..n {
        // Seed live-after-block from successors' live-in.
        let mut live_after: HashSet<LocalId> = HashSet::new();
        for succ in func.blocks[bi].term.successors() {
            if succ.index() < n {
                live_after.extend(live.live_in[succ.index()].iter().copied());
            }
        }
        for l in terminator_reads(&func.blocks[bi].term) {
            live_after.insert(l);
        }
        // Address-taken locals are always live.
        for l in &func.locals {
            if l.address_taken {
                live_after.insert(l.id);
            }
        }

        // Walk statements backward, deciding per statement.
        let block = &mut func.blocks[bi];
        let mut new_rev: Vec<Statement> = Vec::with_capacity(block.stmts.len());
        for stmt in block.stmts.drain(..).rev() {
            let keep = decide_store(&stmt, &mut live_after, stats, &mut changed);
            if let Some(s) = keep {
                // Update live set with this (kept) statement's effect.
                apply_stmt_backward(&s, &mut live_after);
                new_rev.push(s);
            }
        }
        new_rev.reverse();
        block.stmts = new_rev;
    }
    changed
}

/// Decides the fate of a statement during backward dead-store removal. Returns
/// `Some(stmt)` to keep (possibly rewritten) or `None` to delete. Mutates
/// `live_after` only via the caller (so the caller can apply the kept statement's
/// effect); here we only inspect it.
fn decide_store(
    stmt: &Statement,
    live_after: &mut HashSet<LocalId>,
    stats: &mut OptStats,
    changed: &mut bool,
) -> Option<Statement> {
    if let Statement::Assign {
        place,
        rvalue,
        span,
    } = stmt
    {
        if place.is_local() && !live_after.contains(&place.base) {
            // The destination is dead after this point.
            if rvalue_is_pure(rvalue) {
                // Pure + dead result: delete the whole statement.
                stats.stmts_removed += 1;
                *changed = true;
                return None;
            } else {
                // Impure + dead result: keep the effect, drop the store.
                stats.stmts_removed += 1;
                *changed = true;
                return Some(Statement::Eval {
                    rvalue: rvalue.clone(),
                    span: *span,
                });
            }
        }
    }
    Some(stmt.clone())
}
