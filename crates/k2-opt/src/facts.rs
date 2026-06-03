//! Control-flow facts computed once per function and shared by the passes:
//! predecessor lists, reverse-postorder, and the single soundness gate the whole
//! optimizer leans on — the set of *address-taken* locals.
//!
//! There is no SSA here. The MIR's copy-on-read discipline (every place read is a
//! copy; the only way to mutate a local is an `Assign` to it) means a simple
//! per-block, RPO-iterated data-flow is sound. The one thing that discipline does
//! *not* cover is aliasing: a local whose address is taken (`&local`) may be
//! mutated through a pointer the optimizer cannot follow. The
//! [`CfgFacts::address_taken`] set names exactly those locals, and every pass
//! treats them as opaque: never const/copy-propagated as a source, never folded
//! away, never dead-store-eliminated. This single rule makes the optimizer sound
//! with respect to pointers and the heap without any alias analysis.

use k2_mir::{BlockId, LocalId, MirFunction};

/// Per-function control-flow facts.
pub(crate) struct CfgFacts {
    /// `preds[b]` is the list of blocks whose terminator may jump to block `b`.
    pub preds: Vec<Vec<BlockId>>,
    /// The reverse-postorder of reachable blocks (entry first), the order in which
    /// forward data-flow converges fastest.
    pub rpo: Vec<BlockId>,
    /// `address_taken[l]` is `true` if local `l`'s address is ever taken. Such a
    /// local is opaque to value tracking.
    pub address_taken: Vec<bool>,
}

impl CfgFacts {
    /// Computes the facts for `func`.
    pub fn new(func: &MirFunction) -> CfgFacts {
        let n = func.blocks.len();
        let mut preds: Vec<Vec<BlockId>> = vec![Vec::new(); n];
        for b in &func.blocks {
            for succ in b.term.successors() {
                if succ.index() < n {
                    preds[succ.index()].push(b.id);
                }
            }
        }
        let address_taken: Vec<bool> = func.locals.iter().map(|l| l.address_taken).collect();
        CfgFacts {
            preds,
            rpo: reverse_postorder(func),
            address_taken,
        }
    }

    /// `true` if local `l` is address-taken (and therefore opaque to value
    /// tracking). An out-of-range local conservatively reads as address-taken.
    pub fn is_address_taken(&self, l: LocalId) -> bool {
        self.address_taken.get(l.index()).copied().unwrap_or(true)
    }
}

/// Computes the reverse-postorder of the reachable blocks of `func` (entry
/// first). Unreachable blocks are omitted; callers that need every block iterate
/// `func.blocks` directly.
pub(crate) fn reverse_postorder(func: &MirFunction) -> Vec<BlockId> {
    let n = func.blocks.len();
    let mut visited = vec![false; n];
    let mut post: Vec<BlockId> = Vec::with_capacity(n);
    // Iterative postorder DFS: a node is emitted to `post` after all its
    // successors, using an explicit stack of (block, next-successor-index).
    let mut stack: Vec<(usize, usize)> = Vec::new();
    let entry = func.entry.index();
    if entry < n {
        visited[entry] = true;
        stack.push((entry, 0));
    }
    while let Some(&(bi, si)) = stack.last() {
        let succs = func.blocks[bi].term.successors();
        if si < succs.len() {
            stack.last_mut().unwrap().1 += 1;
            let next = succs[si].index();
            if next < n && !visited[next] {
                visited[next] = true;
                stack.push((next, 0));
            }
        } else {
            post.push(BlockId(bi as u32));
            stack.pop();
        }
    }
    post.reverse();
    post
}
