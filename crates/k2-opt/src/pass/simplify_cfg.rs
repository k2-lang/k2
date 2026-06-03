//! CFG simplification.
//!
//! The only pass that changes the *shape* of the control-flow graph. It performs
//! three local rewrites, then garbage-collects whatever became unreachable:
//!
//! 1. **Jump threading**: a `Goto(B)`/`Branch{…→B}`/`Switch{…→B}` edge whose
//!    target `B` is a *pure forwarding block* (no statements, terminator
//!    `Goto(T)`) is retargeted straight to `T`, skipping the empty hop.
//! 2. **Block merging**: when block `A` ends in `Goto(B)` and `B`'s only
//!    predecessor is `A`, `B`'s statements and terminator are appended to `A`
//!    (and `B` becomes unreachable, to be collected).
//! 3. **Unreachable GC**: the existing `MirFunction::gc_unreachable_blocks`
//!    densely renumbers survivors and fixes `entry`/`panic_block`.
//!
//! Each retarget keeps every target in range; the trailing GC restores dense
//! block ids and the "no unreachable block" invariant. Param/local slots are
//! never touched, and the entry block id is preserved (the entry is only ever
//! merged *into* — it absorbs its successor, keeping id 0). So `verify` holds
//! after the pass completes.

use k2_mir::{BlockId, MirFunction, Terminator};

use crate::OptStats;

/// Simplifies the CFG of `func`. Returns `true` if anything changed.
pub(crate) fn run(func: &mut MirFunction, stats: &mut OptStats) -> bool {
    let before_blocks = func.blocks.len();
    let mut changed = false;

    // Iterate the local rewrites to a fixpoint (a thread can expose a merge and
    // vice versa). Bounded by the block count: each rewrite strictly reduces the
    // number of edges or blocks, so it terminates.
    let mut iters = 0;
    loop {
        let mut local_changed = false;
        local_changed |= thread_jumps(func);
        local_changed |= merge_blocks(func);
        changed |= local_changed;
        iters += 1;
        if !local_changed || iters > before_blocks + 4 {
            break;
        }
    }

    // Collect anything the rewrites (or an upstream const-fold of a branch) left
    // unreachable; this is what restores `verify`.
    func.gc_unreachable_blocks();

    let after_blocks = func.blocks.len();
    if after_blocks < before_blocks {
        stats.blocks_removed += (before_blocks - after_blocks) as u32;
        changed = true;
    }
    changed
}

/// Retargets edges that point at a pure forwarding block (empty statements,
/// `Goto(T)`) directly to `T`. Never threads a block onto itself (a self-loop
/// forwarder would diverge; we leave it). Returns whether anything changed.
fn thread_jumps(func: &mut MirFunction) -> bool {
    let n = func.blocks.len();
    // Precompute the forwarding target of each block, if it is a pure forwarder.
    let forward: Vec<Option<BlockId>> = (0..n)
        .map(|i| {
            let b = &func.blocks[i];
            if b.stmts.is_empty() {
                if let Terminator::Goto(t) = &b.term {
                    // Do not forward a block to itself.
                    if t.index() != i {
                        return Some(*t);
                    }
                }
            }
            None
        })
        .collect();

    // Resolve chains: follow forwarders until a non-forwarder (cap by n to avoid a
    // cycle of empty gotos looping forever).
    let resolve = |start: BlockId| -> BlockId {
        let mut cur = start;
        for _ in 0..=n {
            match forward.get(cur.index()).copied().flatten() {
                Some(next) if next != cur => cur = next,
                _ => break,
            }
        }
        cur
    };

    let mut changed = false;
    for i in 0..n {
        // Skip rewriting the forwarder blocks themselves (they get collected).
        let mut term = std::mem::replace(&mut func.blocks[i].term, Terminator::Unreachable);
        changed |= retarget_terminator(&mut term, &resolve);
        func.blocks[i].term = term;
    }
    changed
}

/// Applies `resolve` to every target of a terminator, returning whether any
/// target moved.
fn retarget_terminator(term: &mut Terminator, resolve: &impl Fn(BlockId) -> BlockId) -> bool {
    let mut changed = false;
    let mut go = |t: &mut BlockId| {
        let r = resolve(*t);
        if r != *t {
            *t = r;
            changed = true;
        }
    };
    match term {
        Terminator::Goto(t) => go(t),
        Terminator::Branch {
            then_bb, else_bb, ..
        } => {
            go(then_bb);
            go(else_bb);
        }
        Terminator::Switch {
            targets, default, ..
        } => {
            for (_, t) in targets {
                go(t);
            }
            go(default);
        }
        Terminator::Return { .. } | Terminator::Trap { .. } | Terminator::Unreachable => {}
    }
    changed
}

/// Merges a block into its sole predecessor when that predecessor ends in an
/// unconditional `Goto` to it. Returns whether anything changed.
fn merge_blocks(func: &mut MirFunction) -> bool {
    let n = func.blocks.len();
    // Count predecessors per block (over current terminators).
    let mut pred_count = vec![0usize; n];
    for b in &func.blocks {
        for s in b.term.successors() {
            if s.index() < n {
                pred_count[s.index()] += 1;
            }
        }
    }

    // Find a mergeable (A -> B) pair: A ends in Goto(B), B != A, B is not the
    // entry, and B has exactly one predecessor (A).
    let mut merge: Option<(usize, usize)> = None;
    for (ai, b) in func.blocks.iter().enumerate() {
        if let Terminator::Goto(target) = &b.term {
            let bi = target.index();
            if bi != ai && bi != func.entry.index() && pred_count[bi] == 1 {
                merge = Some((ai, bi));
                break;
            }
        }
    }

    let Some((ai, bi)) = merge else {
        return false;
    };

    // Move B's statements and terminator into A. B is left empty/unreachable for
    // the GC. We must not invalidate other blocks' ids, so we leave B in place
    // with an `Unreachable` terminator and no statements.
    let b_stmts = std::mem::take(&mut func.blocks[bi].stmts);
    let b_term = std::mem::replace(&mut func.blocks[bi].term, Terminator::Unreachable);
    // If B was a panic block, that role moves with its terminator; clearing the
    // flag on the now-empty B is harmless (GC drops it).
    func.blocks[bi].is_panic = false;
    func.blocks[ai].stmts.extend(b_stmts);
    func.blocks[ai].term = b_term;
    true
}
