//! Function inlining / devirtualization, and dead-function elimination.
//!
//! The MIR is already monomorphized: every `Rvalue::Call { func: FnId }` is a
//! *direct* call to a concrete function. "Devirtualization" in k2 is therefore
//! free — the capability/zero-cost-abstraction indirections that monomorphize to
//! a single tiny forwarding callee are inlined by exactly the same machinery as
//! any other small call. There is no points-to set to compute; the callee is the
//! `FnId` in the call.
//!
//! ## The transform
//!
//! To inline `callee` into `caller` at a call statement in block `B`:
//! * append fresh clones of every callee local (offset by `caller.locals.len()`),
//!   mapping the callee's return slot and parameters to fresh caller temps;
//! * split `B` at the call into a head (statements before) and a continuation
//!   block `CB` (statements after + `B`'s original terminator);
//! * seed the callee's parameter temps from the call's argument operands;
//! * point the head at the callee's (remapped) entry;
//! * clone every callee block with all locals/targets remapped and every
//!   `Return{value}` rewritten to `dst = value; Goto(CB)`;
//! * `gc_unreachable_blocks()` to re-densify.
//!
//! ## Soundness & termination
//!
//! Locals/blocks are *appended* into fresh disjoint ranges, so no existing id
//! changes and `params[i] == i+1` of the caller is preserved; the trailing GC
//! restores dense block ids and the no-unreachable invariant; the callee's
//! internal `Call.func`s are global FnIds that stay valid (no function is removed
//! during inlining). Intrinsics are copied verbatim, so their dispatch is
//! unchanged. Budgets (size, recursion depth, caller cap, global cap) bound the
//! work and prevent code blow-up / nontermination; a worklist (not recursion)
//! drives the process so it strictly terminates.
//!
//! We conservatively do **not** inline a callee that takes the address of any
//! parameter (`address_taken` param): the VM boxes address-taken params at frame
//! entry, and replicating that frame-entry boxing in an inlined copy is subtle;
//! the corpus's forwarders have none, so we simply skip those callees.

use k2_mir::{
    BasicBlock, BlockId, FnId, LocalId, MirFunction, MirProgram, Operand, Place, Proj, Rvalue,
    Statement, Terminator,
};

use crate::OptStats;

/// Maximum callee statement count to inline (a small-callee gate).
const SIZE_BUDGET: usize = 40;
/// Maximum caller statement count after an inline (a blow-up gate).
const CALLER_SIZE_CAP: usize = 2000;
/// How many times one *recursive* callee (a self-call or an SCC member) may be
/// inlined into a single caller across the WHOLE program run, not per outer pass.
/// A cyclic call graph is the pathological case: every inlined copy of a recursive
/// callee reintroduces the recursive call sites, which the next outer pass would
/// unroll again. Holding this accounting program-global (see [`InlineState`])
/// makes the bound a true total so an SCC cannot be unrolled OUTER_BUDGET times
/// over.
const RECURSION_BUDGET: u32 = 2;
/// Absolute cap on total inline operations program-wide. Threaded across outer
/// passes via [`InlineState`] so it is a genuine program-wide total.
const GLOBAL_INLINE_BUDGET: u32 = 5000;
/// Maximum number of inlines performed into a *single* caller across the whole
/// run. This stops one strongly-connected component (mutually-recursive cycle)
/// from consuming the entire [`GLOBAL_INLINE_BUDGET`] and blowing up that one
/// caller's body: the recursion gate alone bounds each individual callee, but a
/// large SCC has many distinct callees, so we also cap the per-caller total.
const PER_CALLER_INLINE_CAP: u32 = 64;

/// A read-only summary of a callee's *structural* properties, computed once before
/// inlining. The callee's statement count is intentionally NOT cached here — it is
/// measured live at each candidate site, because a callee on a cyclic call graph
/// can itself grow as it is inlined into, and the gate must size the clone we are
/// actually about to make.
#[derive(Clone)]
struct CalleeSummary {
    /// `true` if the callee has a usable body (non-empty blocks, reachable
    /// `Return`).
    has_body: bool,
    /// `true` if any parameter has its address taken (then we skip inlining it).
    addr_taken_param: bool,
}

/// Program-global inlining accounting, threaded across *every* outer pass-manager
/// iteration (see [`crate::optimize`]). Holding this state outside [`run`] is what
/// makes [`RECURSION_BUDGET`] / [`GLOBAL_INLINE_BUDGET`] / [`PER_CALLER_INLINE_CAP`]
/// true program-wide totals: previously the per-caller depth map was reborn each
/// time `run` was called (once per outer pass), so a recursive callee could be
/// unrolled `RECURSION_BUDGET * OUTER_BUDGET` times into one caller and each copy
/// reintroduced call sites the next pass unrolled again — a superlinear blow-up on
/// cyclic call graphs.
#[derive(Default)]
pub(crate) struct InlineState {
    /// Total inline operations performed so far, program-wide.
    total_inlined: u32,
    /// How many times callee `FnId` has been inlined into caller index `usize`,
    /// keyed by the *original* (pre-DFE) identity. Because dead-function
    /// elimination can renumber FnIds between outer passes, we key on the caller's
    /// current index and the callee's current FnId; the count is only ever an
    /// upper bound that grows, so even if a remap blurs an entry the budget stays
    /// conservative (it can only stop *more* inlining, never permit a blow-up).
    inlines: std::collections::HashMap<(usize, FnId), u32>,
    /// Total inlines into caller index `usize`, program-wide.
    per_caller: std::collections::HashMap<usize, u32>,
}

/// Runs whole-program inlining. Returns `true` if any call was inlined. `state`
/// persists across outer passes so all budgets are program-global totals.
pub(crate) fn run(prog: &mut MirProgram, stats: &mut OptStats, state: &mut InlineState) -> bool {
    let summaries = compute_summaries(prog);
    let recursive = compute_recursive(prog);

    let mut changed = false;

    // Process one function at a time. We scan the caller for the next inlinable
    // call, RESUMING from the block where the previous inline landed rather than
    // rescanning the whole (growing) body from scratch after every inline; and we
    // re-densify ONCE per caller at the end, not after every inline. Together these
    // turn the old O(inlines x caller_size) inner loop into a single linear sweep
    // of the final body per caller.
    for ci in 0..prog.funcs.len() {
        let mut caller_dirtied = false;
        // Where to resume scanning from. After an inline at `site.block`, that
        // block and everything before it no longer hold an un-inlined call we want
        // (the call was consumed; preceding blocks are untouched), so the next scan
        // can start at `site.block` — the freshly appended continuation/callee
        // blocks live at the end and are reached by continuing the sweep.
        let mut resume_block = 0usize;
        loop {
            if state.total_inlined >= GLOBAL_INLINE_BUDGET {
                break;
            }
            if state.per_caller.get(&ci).copied().unwrap_or(0) >= PER_CALLER_INLINE_CAP {
                break;
            }
            let Some(site) =
                find_inlinable_call(prog, ci, resume_block, &summaries, &recursive, state)
            else {
                break;
            };
            let callee = site.callee;
            resume_block = site.block;
            inline_one(prog, ci, site);
            *state.inlines.entry((ci, callee)).or_insert(0) += 1;
            *state.per_caller.entry(ci).or_insert(0) += 1;
            state.total_inlined += 1;
            stats.calls_inlined += 1;
            changed = true;
            caller_dirtied = true;
        }
        // Densify once, after all inlines into this caller are done.
        if caller_dirtied {
            prog.funcs[ci].gc_unreachable_blocks();
        }
    }
    changed
}

/// Computes per-function summaries.
fn compute_summaries(prog: &MirProgram) -> Vec<CalleeSummary> {
    prog.funcs
        .iter()
        .map(|f| {
            let has_return = f
                .blocks
                .iter()
                .any(|b| matches!(b.term, Terminator::Return { .. }));
            let addr_taken_param = f.params.iter().any(|p| {
                f.locals
                    .get(p.index())
                    .map(|l| l.address_taken)
                    .unwrap_or(true)
            });
            CalleeSummary {
                has_body: !f.blocks.is_empty() && has_return,
                addr_taken_param,
            }
        })
        .collect()
}

/// Computes the set of functions that can transitively call themselves (SCC
/// members of size > 1, or a direct self-call), via a simple reachability over the
/// call graph from each function back to itself.
fn compute_recursive(prog: &MirProgram) -> Vec<bool> {
    let n = prog.funcs.len();
    // Build the direct call edges.
    let mut callees: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, f) in prog.funcs.iter().enumerate() {
        for b in &f.blocks {
            for s in &b.stmts {
                if let Some(Rvalue::Call { func, .. }) = rvalue_of(s) {
                    if func.index() < n {
                        callees[i].push(func.index());
                    }
                }
            }
        }
    }
    // A function is recursive if it can reach itself over >=1 edge.
    (0..n)
        .map(|start| {
            let mut seen = vec![false; n];
            let mut stack = callees[start].clone();
            while let Some(x) = stack.pop() {
                if x == start {
                    return true;
                }
                if !seen[x] {
                    seen[x] = true;
                    stack.extend(callees[x].iter().copied());
                }
            }
            false
        })
        .collect()
}

/// The rvalue of a statement, if it has one.
fn rvalue_of(s: &Statement) -> Option<&Rvalue> {
    match s {
        Statement::Assign { rvalue, .. } | Statement::Eval { rvalue, .. } => Some(rvalue),
        _ => None,
    }
}

/// A located, inlinable call site.
struct CallSite {
    block: usize,
    stmt: usize,
    callee: FnId,
    /// The destination place (for an `Assign`), or `None` for an `Eval`.
    dst: Option<Place>,
    /// The call's argument operands.
    args: Vec<Operand>,
}

/// Finds the next inlinable call in `prog.funcs[ci]` at or after block
/// `resume_block`, per the budgets. Returns the site, or `None` if there is
/// nothing left to inline. The scan resumes from `resume_block` (blocks before it
/// were already swept and hold nothing we want to inline), which keeps the
/// per-caller cost linear in the final body size rather than quadratic.
fn find_inlinable_call(
    prog: &MirProgram,
    ci: usize,
    resume_block: usize,
    summaries: &[CalleeSummary],
    recursive: &[bool],
    state: &InlineState,
) -> Option<CallSite> {
    let caller_stmts: usize = prog.funcs[ci].blocks.iter().map(|b| b.stmts.len()).sum();

    for (bi, block) in prog.funcs[ci].blocks.iter().enumerate().skip(resume_block) {
        for (si, stmt) in block.stmts.iter().enumerate() {
            let (dst, rvalue) = match stmt {
                Statement::Assign { place, rvalue, .. } => (Some(place.clone()), rvalue),
                Statement::Eval { rvalue, .. } => (None, rvalue),
                _ => continue,
            };
            let Rvalue::Call { func, args, .. } = rvalue else {
                continue;
            };
            let callee = *func;
            if callee.index() >= summaries.len() {
                continue;
            }
            let s = &summaries[callee.index()];

            // Gate 1: usable body, no address-taken params.
            if !s.has_body || s.addr_taken_param {
                continue;
            }
            // Gate 2: recursion budget. A recursive callee (incl. self-call) may
            // be inlined only up to RECURSION_BUDGET times into this caller across
            // the WHOLE program run (the count lives in `state`, not a per-pass
            // map), so a cyclic call graph cannot be re-unrolled each outer pass.
            let is_recursive =
                recursive.get(callee.index()).copied().unwrap_or(false) || callee.index() == ci;
            if is_recursive {
                let used = state.inlines.get(&(ci, callee)).copied().unwrap_or(0);
                if used >= RECURSION_BUDGET {
                    continue;
                }
            }
            // Gate 3: size budget + caller cap. We size the callee by its CURRENT
            // body (it may itself have been inlined into and grown since the
            // summaries were computed), not the stale precomputed count. On a
            // cyclic call graph the callee `f_next` is processed before this caller
            // and clones of an already-grown `f_next` are what blow the body up;
            // measuring it live makes the SIZE_BUDGET / CALLER_SIZE_CAP gates bind
            // on the real cost of the clone we are about to make.
            let callee_stmts: usize = prog.funcs[callee.index()]
                .blocks
                .iter()
                .map(|b| b.stmts.len())
                .sum();
            if callee_stmts > SIZE_BUDGET {
                continue;
            }
            if caller_stmts + callee_stmts > CALLER_SIZE_CAP {
                continue;
            }

            return Some(CallSite {
                block: bi,
                stmt: si,
                callee,
                dst,
                args: args.clone(),
            });
        }
    }
    None
}

/// Performs a single inline of `site` into `prog.funcs[ci]`.
fn inline_one(prog: &mut MirProgram, ci: usize, site: CallSite) {
    // Clone the callee body out first (we must not hold a borrow on `prog.funcs`
    // while mutating the caller).
    let callee = prog.funcs[site.callee.index()].clone_body();

    let base_local = prog.funcs[ci].locals.len() as u32;

    // (1) Append clones of the callee's locals, renumbered by +base_local.
    {
        let caller = &mut prog.funcs[ci];
        for mut local in callee.locals.iter().cloned() {
            local.id = LocalId(local.id.0 + base_local);
            caller.locals.push(local);
        }
    }

    // (2) Split the call block FIRST, so the continuation block claims the next
    //     block id; the callee's cloned blocks are then appended *after* it. We
    //     compute `base_block` after the split so the callee-block remap cannot
    //     collide with the continuation's id.
    let split = split_call_block(&mut prog.funcs[ci], site.block, site.stmt);
    let base_block = prog.funcs[ci].blocks.len() as u32;

    // The callee's return slot is its local 0 -> caller local base_local. Its
    // params are callee locals 1..=P -> caller locals base_local+1..=base_local+P.
    let ret_tmp = LocalId(base_local); // callee local 0 remapped
    let remap_local = |l: LocalId| LocalId(l.0 + base_local);
    let remap_block = |b: BlockId| BlockId(b.0 + base_block);

    // (3) Seed parameter temps from the argument operands, inserted at the END of
    //     the head block (after the statements that precede the call).
    {
        let caller = &mut prog.funcs[ci];
        let head = &mut caller.blocks[site.block];
        for (k, arg) in site.args.iter().enumerate() {
            let param_tmp = LocalId(base_local + 1 + k as u32);
            head.stmts.push(Statement::Assign {
                place: Place::local(param_tmp),
                rvalue: Rvalue::Use(arg.clone()),
                span: callee.span,
            });
        }
        // (4) The head jumps to the callee's (remapped) entry.
        head.term = Terminator::Goto(remap_block(callee.entry));
    }

    // (5) Clone every callee block, remapping locals + targets, and rewriting
    //     Return into `dst = value; Goto(cont)`.
    let cont = split.cont;
    let dst = site.dst.clone();
    for cb in &callee.blocks {
        let mut stmts: Vec<Statement> = Vec::with_capacity(cb.stmts.len());
        for s in &cb.stmts {
            let mut s2 = s.clone();
            remap_stmt(&mut s2, &remap_local);
            stmts.push(s2);
        }
        let term = match &cb.term {
            Terminator::Return { value } => {
                // Write the returned value into the call's destination, then jump
                // to the continuation.
                let mut v = value.clone();
                remap_operand(&mut v, &remap_local);
                if let Some(dst_place) = &dst {
                    stmts.push(Statement::Assign {
                        place: dst_place.clone(),
                        rvalue: Rvalue::Use(v),
                        span: callee.span,
                    });
                } else {
                    // Evaluated for effect: the value is discarded, but writing it
                    // to the throwaway ret_tmp keeps a single canonical shape (and
                    // DCE removes it). Using ret_tmp avoids needing the dst type.
                    stmts.push(Statement::Assign {
                        place: Place::local(ret_tmp),
                        rvalue: Rvalue::Use(v),
                        span: callee.span,
                    });
                }
                Terminator::Goto(cont)
            }
            other => {
                let mut t = other.clone();
                // Remap BOTH the target block ids AND the operand locals (a
                // `Branch` condition / `Switch` scrutinee is a callee local that
                // must move into the appended range, else it aliases a caller
                // local of the same number — a miscompile).
                remap_terminator_blocks(&mut t, &remap_block);
                remap_terminator_operands(&mut t, &remap_local);
                t
            }
        };
        let new_id = remap_block(cb.id);
        prog.funcs[ci].blocks.push(BasicBlock {
            id: new_id,
            stmts,
            term,
            is_panic: cb.is_panic,
        });
    }
}

/// The result of splitting a call block: the id of the continuation block holding
/// the post-call statements and the original terminator.
struct Split {
    cont: BlockId,
}

/// Splits block `bi` of `func` at statement index `si` (the call). The head block
/// keeps `stmts[0..si]`; a new continuation block gets `stmts[si+1..]` and the
/// head's original terminator. The head's terminator and the seeding of params
/// are set by the caller.
fn split_call_block(func: &mut MirFunction, bi: usize, si: usize) -> Split {
    let cont_id = BlockId(func.blocks.len() as u32);
    // Take the trailing statements after the call, and the original terminator.
    let trailing: Vec<Statement> = func.blocks[bi].stmts.split_off(si + 1);
    // Drop the call statement itself.
    func.blocks[bi].stmts.pop();
    let orig_term = std::mem::replace(&mut func.blocks[bi].term, Terminator::Unreachable);
    func.blocks.push(BasicBlock {
        id: cont_id,
        stmts: trailing,
        term: orig_term,
        is_panic: false,
    });
    Split { cont: cont_id }
}

// =========================================================================
//  Local / block remapping helpers (for cloned callee bodies)
// =========================================================================

/// Remaps every local id a statement references through `f`.
fn remap_stmt(stmt: &mut Statement, f: &impl Fn(LocalId) -> LocalId) {
    match stmt {
        Statement::Assign { place, rvalue, .. } => {
            remap_place(place, f);
            remap_rvalue(rvalue, f);
        }
        Statement::Eval { rvalue, .. } => remap_rvalue(rvalue, f),
        Statement::StorageLive(l) | Statement::StorageDead(l) => *l = f(*l),
        Statement::Check(_) | Statement::Note(_) => {}
    }
}

/// Remaps the base + index-operand locals of a place.
fn remap_place(place: &mut Place, f: &impl Fn(LocalId) -> LocalId) {
    place.base = f(place.base);
    for proj in &mut place.proj {
        if let Proj::Index { index, .. } = proj {
            remap_operand(index, f);
        }
    }
}

/// Remaps the base of an operand (if it is a `Copy`).
fn remap_operand(op: &mut Operand, f: &impl Fn(LocalId) -> LocalId) {
    if let Operand::Copy(p) = op {
        remap_place(p, f);
    }
}

/// Remaps every local an rvalue references.
fn remap_rvalue(rvalue: &mut Rvalue, f: &impl Fn(LocalId) -> LocalId) {
    match rvalue {
        Rvalue::Use(o)
        | Rvalue::MakeSome(o, _)
        | Rvalue::MakeOk(o, _)
        | Rvalue::Cast { operand: o, .. }
        | Rvalue::Unary { operand: o, .. }
        | Rvalue::Discriminant { operand: o, .. } => remap_operand(o, f),
        Rvalue::Ref { place, .. } => remap_place(place, f),
        Rvalue::Binary { lhs, rhs, .. } => {
            remap_operand(lhs, f);
            remap_operand(rhs, f);
        }
        Rvalue::MakeSlice {
            ptr, offset, len, ..
        } => {
            remap_operand(ptr, f);
            remap_operand(offset, f);
            remap_operand(len, f);
        }
        Rvalue::Aggregate { fields, .. } => {
            for x in fields {
                remap_operand(x, f);
            }
        }
        Rvalue::Call { args, .. } => {
            for a in args {
                remap_operand(a, f);
            }
        }
        Rvalue::Intrinsic { path, args, .. } => {
            if let k2_mir::IntrinsicRoot::Value(o) = &mut path.root {
                remap_operand(o, f);
            }
            for a in args {
                remap_operand(a, f);
            }
        }
        Rvalue::MakeNull(_) | Rvalue::MakeErr(_, _) => {}
    }
}

/// Remaps the operand locals a terminator reads (`Branch` cond, `Switch`
/// scrutinee, `Return` value) through `f`.
fn remap_terminator_operands(term: &mut Terminator, f: &impl Fn(LocalId) -> LocalId) {
    match term {
        Terminator::Branch { cond, .. } => remap_operand(cond, f),
        Terminator::Switch { scrutinee, .. } => remap_operand(scrutinee, f),
        Terminator::Return { value } => remap_operand(value, f),
        Terminator::Goto(_) | Terminator::Trap { .. } | Terminator::Unreachable => {}
    }
}

/// Remaps every target block id of a terminator through `f`.
fn remap_terminator_blocks(term: &mut Terminator, f: &impl Fn(BlockId) -> BlockId) {
    match term {
        Terminator::Goto(t) => *t = f(*t),
        Terminator::Branch {
            then_bb, else_bb, ..
        } => {
            *then_bb = f(*then_bb);
            *else_bb = f(*else_bb);
        }
        Terminator::Switch {
            targets, default, ..
        } => {
            for (_, t) in targets {
                *t = f(*t);
            }
            *default = f(*default);
        }
        Terminator::Return { .. } | Terminator::Trap { .. } | Terminator::Unreachable => {}
    }
}

// =========================================================================
//  Dead-function elimination
// =========================================================================

/// Removes functions unreachable from any entry (after inlining), applying a
/// global FnId remap to funcs / call sites / entries / `by_inst`. Returns whether
/// anything changed. This is the FnId analogue of `gc_unreachable_blocks`.
pub(crate) fn dead_function_elimination(prog: &mut MirProgram, stats: &mut OptStats) -> bool {
    let n = prog.funcs.len();
    if n == 0 {
        return false;
    }
    // Mark reachable funcs: BFS from entries over Call edges, always keeping
    // entries themselves.
    let mut reachable = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    for e in &prog.entries {
        if e.index() < n && !reachable[e.index()] {
            reachable[e.index()] = true;
            stack.push(e.index());
        }
    }
    // Also always keep a function literally named `main` (the VM's `find_main`
    // scans by name first), even if it is not in `entries`.
    for (i, f) in prog.funcs.iter().enumerate() {
        if f.name == "main" && !reachable[i] {
            reachable[i] = true;
            stack.push(i);
        }
    }
    while let Some(fi) = stack.pop() {
        for b in &prog.funcs[fi].blocks {
            for s in &b.stmts {
                if let Some(Rvalue::Call { func, .. }) = rvalue_of(s) {
                    let ti = func.index();
                    if ti < n && !reachable[ti] {
                        reachable[ti] = true;
                        stack.push(ti);
                    }
                }
            }
        }
    }

    if reachable.iter().all(|&r| r) {
        return false;
    }

    // Build the old->new dense FnId remap over survivors (preserving order).
    let mut remap: Vec<Option<u32>> = vec![None; n];
    let mut next = 0u32;
    for (i, &r) in reachable.iter().enumerate() {
        if r {
            remap[i] = Some(next);
            next += 1;
        }
    }
    let removed = reachable.iter().filter(|&&r| !r).count() as u32;

    // Rewrite survivors: new id, and every Call.func through the remap.
    let mut new_funcs: Vec<MirFunction> = Vec::with_capacity(next as usize);
    for (i, mut f) in std::mem::take(&mut prog.funcs).into_iter().enumerate() {
        if let Some(new) = remap[i] {
            f.id = FnId(new);
            for b in &mut f.blocks {
                for s in &mut b.stmts {
                    remap_call_fnid(s, &remap);
                }
            }
            new_funcs.push(f);
        }
    }
    prog.funcs = new_funcs;

    // Rewrite entries and the by_inst table.
    for e in &mut prog.entries {
        if let Some(new) = remap[e.index()] {
            *e = FnId(new);
        }
    }
    prog.by_inst.retain(|_, fid| remap[fid.index()].is_some());
    for fid in prog.by_inst.values_mut() {
        if let Some(new) = remap[fid.index()] {
            *fid = FnId(new);
        }
    }

    // Dead-function removal does not delete statements; it shrinks the function
    // table. We do not bump a stat counter for it (the bench harness measures the
    // effect via executed-instruction counts), but we report change to the
    // pass-manager fixpoint via the return value.
    let _ = stats;
    removed > 0
}

/// Rewrites a `Call`'s callee FnId through the function remap.
fn remap_call_fnid(stmt: &mut Statement, remap: &[Option<u32>]) {
    let rvalue = match stmt {
        Statement::Assign { rvalue, .. } | Statement::Eval { rvalue, .. } => rvalue,
        _ => return,
    };
    if let Rvalue::Call { func, .. } = rvalue {
        if let Some(new) = remap[func.index()] {
            *func = FnId(new);
        }
    }
}

// =========================================================================
//  MirFunction body cloning
// =========================================================================

/// A lightweight clone of just the parts of a [`MirFunction`] inlining needs: its
/// locals, blocks, entry, and span. Implemented as a free helper because
/// `MirFunction` is not `Clone` in `k2-mir`.
trait CloneBody {
    fn clone_body(&self) -> ClonedBody;
}

/// The cloned callee body.
struct ClonedBody {
    locals: Vec<k2_mir::Local>,
    blocks: Vec<BasicBlock>,
    entry: BlockId,
    span: k2_syntax::Span,
}

impl CloneBody for MirFunction {
    fn clone_body(&self) -> ClonedBody {
        ClonedBody {
            locals: self.locals.clone(),
            blocks: self
                .blocks
                .iter()
                .map(|b| BasicBlock {
                    id: b.id,
                    stmts: b.stmts.clone(),
                    term: b.term.clone(),
                    is_panic: b.is_panic,
                })
                .collect(),
            entry: self.entry,
            span: self.span,
        }
    }
}
