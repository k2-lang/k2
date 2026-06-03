//! Conservative leak / escape analysis.
//!
//! A post-lowering MIR walk that flags only the two unambiguous bug shapes the
//! milestone lists, with **zero false positives** on the corpus. It is a
//! *diagnostic*, not a borrow checker: when in any doubt it stays silent.
//!
//! ## Pattern A — obvious missing free in a simple linear scope
//!
//! A local is assigned from an *allocating* intrinsic (final member ∈
//! `{alloc, create, dupe}`), and on the normal-return path there is no paired
//! release (`free`/`destroy`/`deinit`) of it via a `defer`/`errdefer`/explicit
//! call, and it is not returned / transferred. To stay conservative the pass
//! bails entirely on any function whose body contains a loop (a free could
//! happen across iterations) and treats any local that flows into a `Return`
//! (directly or via an aggregate) as transferred.
//!
//! ## Pattern B — pointer to a stack local escapes via return
//!
//! A `Ref` whose rooted place is a stack-rooted local (a `Binding`/`Temp`, not a
//! parameter pointer or a heap-backed slice) flows into the function's `Return`.
//! Returning `*T`/`[]T` that points at a stack local is a guaranteed dangling
//! reference. A `&items[0]` whose root is a slice *parameter* (heap-backed view)
//! is NOT flagged — only genuinely stack-rooted escapes are.

use std::collections::HashMap;
use std::collections::HashSet;

use crate::ir::*;
use crate::lower::FnAllocInfo;
use k2_syntax::Span;

/// Runs the conservative leak/escape analysis over the whole program, returning
/// diagnostics (each an error). `alloc_info` carries the per-fn defer-release and
/// loop facts the lowerer recorded.
pub(crate) fn analyze(
    prog: &MirProgram,
    alloc_info: &HashMap<FnId, FnAllocInfo>,
) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    for func in &prog.funcs {
        let info = alloc_info.get(&func.id).cloned().unwrap_or_default();
        analyze_fn(func, &info, &mut diags);
    }
    diags
}

/// Analyzes one function for the two flagged patterns.
fn analyze_fn(func: &MirFunction, info: &FnAllocInfo, diags: &mut Vec<Diagnostic>) {
    pattern_b_escape(func, diags);
    pattern_a_missing_free(func, info, diags);
}

// =========================================================================
//  Pattern B: escaping pointer to a stack local
// =========================================================================

/// Flags a `&local` to a stack-rooted local that flows into a `Return`.
fn pattern_b_escape(func: &MirFunction, diags: &mut Vec<Diagnostic>) {
    // Map: local -> the span where its address was taken into a pointer, if the
    // pointed-to root is stack-rooted.
    let mut ref_of: HashMap<LocalId, (LocalId, Span)> = HashMap::new();
    for b in &func.blocks {
        for s in &b.stmts {
            if let Statement::Assign {
                place,
                rvalue: Rvalue::Ref { place: src, .. },
                span,
            } = s
            {
                if place.is_local() && self_is_stack_rooted(func, src.base) {
                    ref_of.insert(place.base, (src.base, *span));
                }
            }
        }
    }
    if ref_of.is_empty() {
        return;
    }
    // A pointer local "escapes" if it is returned (directly or via an aggregate
    // that is returned). We compute the set of locals that flow into a Return.
    let returned = returned_locals(func);
    for (ptr_local, (_root, span)) in &ref_of {
        if returned.contains(ptr_local) {
            diags.push(Diagnostic::error(
                *span,
                "pointer to a stack local escapes via return (dangling reference)",
            ));
        }
    }
}

/// `true` if `local` is a stack-rooted slot (a binding/temp/ret), NOT a parameter
/// (a parameter pointer/slice points at caller-owned, heap-backed storage).
fn self_is_stack_rooted(func: &MirFunction, local: LocalId) -> bool {
    matches!(
        func.locals[local.index()].origin,
        LocalOrigin::Binding(_) | LocalOrigin::Temp
    )
}

/// The set of locals that flow into a `Return` terminator (directly, through a
/// `Use`/`MakeSlice`/`Aggregate` chain assigned to the returned local or ret
/// slot).
fn returned_locals(func: &MirFunction) -> HashSet<LocalId> {
    // Seed with the operands returned by each Return terminator, plus the ret
    // slot (local 0), then propagate backwards through assignments to those.
    let mut seed: HashSet<LocalId> = HashSet::new();
    seed.insert(LocalId(0));
    for b in &func.blocks {
        if let Terminator::Return { value } = &b.term {
            collect_operand_locals(value, &mut seed);
        }
    }
    // Fixed-point: any assignment whose destination is in `seed` contributes its
    // source locals.
    let mut changed = true;
    while changed {
        changed = false;
        for b in &func.blocks {
            for s in &b.stmts {
                if let Statement::Assign { place, rvalue, .. } = s {
                    if place.is_local() && seed.contains(&place.base) {
                        let before = seed.len();
                        collect_rvalue_locals(rvalue, &mut seed);
                        if seed.len() != before {
                            changed = true;
                        }
                    }
                }
            }
        }
    }
    seed
}

/// Adds the base locals referenced by an operand to `set`.
fn collect_operand_locals(op: &Operand, set: &mut HashSet<LocalId>) {
    if let Operand::Copy(p) = op {
        set.insert(p.base);
        collect_place_index_locals(p, set);
    }
}

/// Adds locals appearing in a place's `Index` projections (the index operands).
fn collect_place_index_locals(p: &Place, set: &mut HashSet<LocalId>) {
    for proj in &p.proj {
        if let Proj::Index { index, .. } = proj {
            collect_operand_locals(index, set);
        }
    }
}

/// Adds the base locals referenced by an rvalue to `set`.
fn collect_rvalue_locals(rv: &Rvalue, set: &mut HashSet<LocalId>) {
    match rv {
        Rvalue::Use(o)
        | Rvalue::MakeSome(o, _)
        | Rvalue::MakeOk(o, _)
        | Rvalue::Cast { operand: o, .. }
        | Rvalue::Unary { operand: o, .. }
        | Rvalue::Discriminant { operand: o, .. } => collect_operand_locals(o, set),
        Rvalue::Ref { place, .. } => {
            set.insert(place.base);
        }
        Rvalue::Binary { lhs, rhs, .. } => {
            collect_operand_locals(lhs, set);
            collect_operand_locals(rhs, set);
        }
        Rvalue::MakeSlice { ptr, len, .. } => {
            collect_operand_locals(ptr, set);
            collect_operand_locals(len, set);
        }
        Rvalue::Aggregate { fields, .. } => {
            for f in fields {
                collect_operand_locals(f, set);
            }
        }
        // A call/intrinsic RESULT is a fresh value; it does not alias its
        // pointer arguments. So escape/return flow does not propagate through a
        // call's arguments (a `&local` merely *passed* to a callee is a borrow,
        // not an escape — only a *returned* `&local` is flagged).
        Rvalue::Call { .. } | Rvalue::Intrinsic { .. } => {}
        Rvalue::MakeNull(_) | Rvalue::MakeErr(_, _) => {}
    }
}

// =========================================================================
//  Pattern A: missing free in a simple linear scope
// =========================================================================

/// Flags a local assigned from an allocating intrinsic that is never released
/// and never transferred, in a loop-free function.
fn pattern_a_missing_free(func: &MirFunction, info: &FnAllocInfo, diags: &mut Vec<Diagnostic>) {
    // Conservative bail: any loop in the function means a free could cross
    // iterations — do not analyze missing frees here.
    if info.has_loop {
        return;
    }
    // Locals released by some defer/errdefer/explicit free.
    let released: HashSet<LocalId> = info.released.iter().copied().collect();
    // Locals transferred out via return (ownership handed to the caller).
    let returned = returned_locals(func);
    // Locals consumed by a release intrinsic anywhere in the body (in case a
    // free is written without a defer).
    let freed_inline = inline_released_locals(func);

    for b in &func.blocks {
        for s in &b.stmts {
            if let Statement::Assign {
                place,
                rvalue: Rvalue::Intrinsic { path, .. },
                span,
            } = s
            {
                if place.is_local() && is_allocating(path) {
                    // The allocating intrinsic's result usually lands in a temp
                    // (an error union); the owned resource is its PAYLOAD, bound
                    // via an unwrap (`x = t.payload`). Compute the set of locals
                    // that own the *payload* (not the raw error-union wrapper) and
                    // treat the allocation as handled if ANY of them is freed,
                    // returned, or passed to a call. This is conservative: a
                    // resource we cannot trace stays silent.
                    let owners = owning_locals(func, place.base);
                    let handled = owners.iter().any(|l| {
                        released.contains(l)
                            || returned.contains(l)
                            || freed_inline.contains(l)
                            || passed_to_call(func, *l)
                    });
                    if !handled {
                        diags.push(Diagnostic::error(
                            *span,
                            "allocated value is never freed in this scope nor returned \
                             (possible memory leak)",
                        ));
                    }
                }
            }
        }
    }
}

/// The locals that own the *resource payload* of an allocation whose error-union
/// (or raw) result is in `alloc_temp`.
///
/// This is the crux of the `try alloc.alloc(...)` fix. The `try` desugar returns
/// the **raw error-union wrapper** on the error path (`err = eu; return err`,
/// where `eu` is `alloc_temp`), so a plain whole-value copy of `alloc_temp` is
/// the *error*, NOT the owned resource. Ownership of the resource only flows
/// through the payload unwrap `x = alloc_temp.payload`. We therefore:
///
/// 1. find every local extracted from `alloc_temp` via a `Proj::Payload` read
///    (the unwrapped resource), and forward-flow those through plain copies/casts;
/// 2. if the allocation is **never** unwrapped via a payload (the alloc result is
///    used directly — no error union), fall back to treating `alloc_temp` itself
///    and its plain-copy forward flow as the owners.
///
/// A whole-value copy of `alloc_temp` (the `try`/`catch` error wrapper) is never
/// treated as owning the payload, so the error-path `return` does not mask a
/// genuine missing free.
fn owning_locals(func: &MirFunction, alloc_temp: LocalId) -> HashSet<LocalId> {
    // Seed: locals unwrapped from the alloc temp via a `.payload` read.
    let mut owners: HashSet<LocalId> = HashSet::new();
    let mut unwrapped = false;
    for b in &func.blocks {
        for s in &b.stmts {
            if let Statement::Assign {
                place,
                rvalue: Rvalue::Use(Operand::Copy(src)),
                ..
            } = s
            {
                if place.is_local()
                    && src.base == alloc_temp
                    && src.proj.iter().any(|p| matches!(p, Proj::Payload { .. }))
                {
                    owners.insert(place.base);
                    unwrapped = true;
                }
            }
        }
    }
    // No payload unwrap: the alloc result is the resource directly.
    if !unwrapped {
        owners.insert(alloc_temp);
    }
    // Forward-flow the payload owners through plain whole-value copies/casts. A
    // whole-value copy of the raw alloc temp is deliberately NOT a seed, so the
    // error-union wrapper that `try` returns never enters this set.
    let mut changed = true;
    while changed {
        changed = false;
        for b in &func.blocks {
            for s in &b.stmts {
                if let Statement::Assign { place, rvalue, .. } = s {
                    if !place.is_local() {
                        continue;
                    }
                    let src = match rvalue {
                        Rvalue::Use(Operand::Copy(p))
                        | Rvalue::Cast {
                            operand: Operand::Copy(p),
                            ..
                        } if p.is_local() => Some(p.base),
                        _ => None,
                    };
                    if let Some(src) = src {
                        if owners.contains(&src) && owners.insert(place.base) {
                            changed = true;
                        }
                    }
                }
            }
        }
    }
    owners
}

/// `true` if an intrinsic path is an *allocating* operation whose result is an
/// owned resource (final member ∈ the small allocator set).
fn is_allocating(path: &IntrinsicPath) -> bool {
    matches!(path.last(), Some("alloc" | "create" | "dupe"))
}

/// `true` if an intrinsic path *releases* a resource.
fn is_releasing(path: &IntrinsicPath) -> bool {
    matches!(path.last(), Some("free" | "destroy" | "deinit"))
}

/// Locals consumed by a release intrinsic anywhere in the body (the first
/// argument of `free`/`destroy`, or the receiver value of `deinit`).
fn inline_released_locals(func: &MirFunction) -> HashSet<LocalId> {
    let mut set = HashSet::new();
    for b in &func.blocks {
        for s in &b.stmts {
            let rv = match s {
                Statement::Assign { rvalue, .. } | Statement::Eval { rvalue, .. } => Some(rvalue),
                _ => None,
            };
            if let Some(Rvalue::Intrinsic { path, args, .. }) = rv {
                if is_releasing(path) {
                    // The released resource is the receiver (for deinit, the root
                    // value operand) or the first argument (for free/destroy).
                    if let IntrinsicRoot::Value(op) = &path.root {
                        collect_operand_locals(op, &mut set);
                    }
                    for a in args {
                        collect_operand_locals(a, &mut set);
                    }
                }
            }
        }
    }
    set
}

/// `true` if `local` is passed as an argument to any call/intrinsic (which could
/// take ownership), so we conservatively do not flag it as leaked.
fn passed_to_call(func: &MirFunction, local: LocalId) -> bool {
    for b in &func.blocks {
        for s in &b.stmts {
            let rv = match s {
                Statement::Assign { rvalue, .. } | Statement::Eval { rvalue, .. } => Some(rvalue),
                _ => None,
            };
            match rv {
                Some(Rvalue::Call { args, .. }) => {
                    for a in args {
                        if operand_mentions(a, local) {
                            return true;
                        }
                    }
                }
                Some(Rvalue::Intrinsic { path, args, .. }) => {
                    // The allocating assignment itself reads no resource; skip it.
                    if is_allocating(path) {
                        continue;
                    }
                    if let IntrinsicRoot::Value(op) = &path.root {
                        if operand_mentions(op, local) {
                            return true;
                        }
                    }
                    for a in args {
                        if operand_mentions(a, local) {
                            return true;
                        }
                    }
                }
                _ => {}
            }
        }
    }
    false
}

/// `true` if an operand reads the given local.
fn operand_mentions(op: &Operand, local: LocalId) -> bool {
    matches!(op, Operand::Copy(p) if p.base == local)
}
