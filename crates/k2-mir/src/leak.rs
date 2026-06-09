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
//!
//! ## Bulk-free allocators are exempt from Pattern A
//!
//! Not every allocator requires a per-allocation `free`. An [`ArenaAllocator`]
//! frees everything at once on `deinit`, and a [`FixedBufferAllocator`] hands out
//! windows of a *caller-owned* buffer that must never be freed individually — for
//! both, omitting per-item frees is the documented, correct contract. Pattern A
//! therefore exempts an allocation whose allocator value traces back to one of
//! these bulk/no-op-free allocators (recognised by the allocator's struct type
//! name, the `.allocator()`/`.init()` method the handle came through, or an arena
//! handle that is later `deinit`'d in scope). The GPA / testing / page allocators
//! — which *do* require explicit frees — are unaffected, so a genuine GPA leak is
//! still flagged. See [`alloc_is_bulk_freed`].

use std::collections::HashMap;
use std::collections::HashSet;

use crate::ir::*;
use crate::lower::FnAllocInfo;
use k2_syntax::Span;
use k2_types::TypeId;

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
        analyze_fn(prog, func, &info, &mut diags);
    }
    diags
}

/// Analyzes one function for the two flagged patterns.
fn analyze_fn(
    prog: &MirProgram,
    func: &MirFunction,
    info: &FnAllocInfo,
    diags: &mut Vec<Diagnostic>,
) {
    pattern_b_escape(func, diags);
    pattern_a_missing_free(prog, func, info, diags);
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
        if let Terminator::Return { value, .. } = &b.term {
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
        | Rvalue::MakeUnion { payload: o, .. }
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
fn pattern_a_missing_free(
    prog: &MirProgram,
    func: &MirFunction,
    info: &FnAllocInfo,
    diags: &mut Vec<Diagnostic>,
) {
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
                    // An allocation from a bulk/no-op-free allocator (an arena or
                    // a fixed-buffer allocator) is NOT required to be freed
                    // individually — that is the documented contract — so it is
                    // never a leak. Skip it before the ownership trace.
                    if alloc_is_bulk_freed(prog, func, path) {
                        continue;
                    }
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
                            || stored_through_pointer(func, *l)
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
///
/// The v0.23 fs/net door shares two of these spellings — `sys.fs.create(path)`
/// opens a FILE (not a heap cell) and `listener.accept()`/`stream` handles are not
/// heap-owned — so a path rooted in an OS-effect namespace is explicitly NOT an
/// allocation. The OS handle owns an fd/socket released by `close()`, which the
/// leak pass does not (and should not) track as a heap allocation.
fn is_allocating(path: &IntrinsicPath) -> bool {
    if matches!(
        path.members.first().map(String::as_str),
        Some("fs" | "os" | "net" | "time")
    ) {
        return false;
    }
    matches!(path.last(), Some("alloc" | "create" | "dupe"))
}

/// The struct type names of the std allocators whose `free` is a documented
/// no-op: an arena (bulk free at `deinit`) and a fixed-buffer allocator
/// (caller-owned backing buffer). An allocation made through one of these never
/// needs an individual `free`, so Pattern A must not flag it.
const BULK_FREE_ALLOCATORS: &[&str] = &["ArenaAllocator", "FixedBufferAllocator"];

/// `true` if the allocation made by `path` flows from a bulk/no-op-free allocator
/// (an arena or fixed-buffer allocator), so omitting a per-item `free` is the
/// correct contract rather than a leak.
///
/// The allocator's concrete kind is not on the intrinsic itself (the receiver is
/// an erased `Allocator` handle), so we recover it by tracing the receiver local
/// backwards through the function's straight-line copies/refs/calls and asking,
/// at each step, whether we have reached a value that is *manifestly* an arena or
/// fixed-buffer allocator:
///
/// * a local whose **struct type** is `ArenaAllocator`/`FixedBufferAllocator`
///   (the `var fba = FixedBufferAllocator.init(...)` slot), or
/// * a value produced by a **call** whose callee is one of those types' methods
///   (`allocator[FixedBufferAllocator]`, `init[ArenaAllocator]`, …) — this is how
///   `const al = fba.allocator()` hands back the erased handle.
///
/// The trace is intentionally conservative: it only follows ownership-preserving
/// data flow (`Use`/`Cast` copies, `&x` refs, and call results) and bails to
/// `false` for anything it cannot positively classify, so a GPA/testing/page
/// allocation — which *does* require an explicit free — is never mistaken for a
/// bulk-free one and a genuine leak stays flagged.
fn alloc_is_bulk_freed(prog: &MirProgram, func: &MirFunction, path: &IntrinsicPath) -> bool {
    // The allocator value is the intrinsic's receiver (`value(recv).alloc(...)`).
    let IntrinsicRoot::Value(op) = &path.root else {
        return false;
    };
    let Operand::Copy(p) = op.as_ref() else {
        return false;
    };
    let mut worklist = vec![p.base];
    let mut seen: HashSet<LocalId> = HashSet::new();
    while let Some(local) = worklist.pop() {
        if !seen.insert(local) {
            continue;
        }
        // A local whose own type is a bulk-free allocator struct settles it.
        if type_is_bulk_free_allocator(prog, func.locals[local.index()].ty) {
            return true;
        }
        // Otherwise follow the (single) assignment that defines this local.
        for b in &func.blocks {
            for s in &b.stmts {
                let Statement::Assign { place, rvalue, .. } = s else {
                    continue;
                };
                if !place.is_local() || place.base != local {
                    continue;
                }
                match rvalue {
                    // A call result: classify by the callee's name (the method's
                    // owning type), and also chase the receiver argument so a
                    // `&fba` -> `.allocator()` chain reaches the FBA slot.
                    Rvalue::Call {
                        func: callee, args, ..
                    } => {
                        if callee_is_bulk_free_allocator(prog, *callee) {
                            return true;
                        }
                        for a in args {
                            if let Operand::Copy(ap) = a {
                                worklist.push(ap.base);
                            }
                        }
                    }
                    // Ownership-preserving copies / address-of: keep tracing the
                    // underlying value (e.g. `_7 = &_4`, `_6 = _7`).
                    Rvalue::Use(Operand::Copy(src))
                    | Rvalue::Cast {
                        operand: Operand::Copy(src),
                        ..
                    }
                    | Rvalue::Ref { place: src, .. } => {
                        worklist.push(src.base);
                    }
                    _ => {}
                }
            }
        }
    }
    false
}

/// `true` if `ty` is one of the bulk-free allocator structs (by name).
fn type_is_bulk_free_allocator(prog: &MirProgram, ty: TypeId) -> bool {
    let name = prog.arena.fmt(ty);
    // `fmt` renders a pointer-to-struct as `*Name`; strip a leading `*`/`*const`
    // so `&fba: *FixedBufferAllocator` is recognised too.
    let bare = name.trim_start_matches('*').trim_start_matches("const ");
    BULK_FREE_ALLOCATORS.contains(&bare)
}

/// `true` if `callee`'s display name is a method of a bulk-free allocator type,
/// e.g. `allocator[FixedBufferAllocator]` or `init[ArenaAllocator]`.
fn callee_is_bulk_free_allocator(prog: &MirProgram, callee: FnId) -> bool {
    let Some(f) = prog.funcs.get(callee.index()) else {
        return false;
    };
    BULK_FREE_ALLOCATORS.iter().any(|a| f.name.contains(a))
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
/// `true` if `local` is stored INTO a place that writes through a pointer (a
/// [`Proj::Deref`] step) — e.g. `(*self).left = node`. The allocation then escapes
/// via that pointer: it is reachable through the parameter / heap structure, so
/// freeing it is the owning structure's responsibility (a tree's recursive
/// teardown), NOT a leak at this allocation site. Mirrors `passed_to_call`'s
/// ownership-transfer reasoning for a store target instead of a call argument.
fn stored_through_pointer(func: &MirFunction, local: LocalId) -> bool {
    for b in &func.blocks {
        for s in &b.stmts {
            if let Statement::Assign { place, rvalue, .. } = s {
                let src_is_local = matches!(rvalue, Rvalue::Use(op) if operand_mentions(op, local));
                if src_is_local && place.proj.iter().any(|p| matches!(p, Proj::Deref)) {
                    return true;
                }
            }
        }
    }
    false
}

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
