//! A linear-scan register allocator over MIR scalar locals.
//!
//! The allocator assigns architectural registers to **vregs** — the subset of
//! MIR locals that are scalar (integer/bool/pointer/`?*T`), not `address_taken`,
//! and not a memory aggregate. Everything else (aggregates, address-taken locals)
//! always lives in a stack home ([`crate::frame`]) and is never considered here.
//!
//! The scheme is classic Poletto–Sarkar linear scan:
//!
//! 1. **Liveness** — number every instruction in block order and compute, by a
//!    backward dataflow fixpoint over the CFG, the set of vregs live across each
//!    program point. Each vreg's *interval* is `[min, max]` over every point where
//!    it is defined, used, or live-out. Because this front end emits blocks in a
//!    forward-dominant order with contiguous loop bodies, the `[min, max]` span
//!    conservatively covers loop back-edges — sound, if occasionally pessimistic.
//! 2. **Scan** — walk intervals in start order; expire intervals whose end has
//!    passed (freeing their register), then assign a free register from the pool
//!    or, if none is free, spill the interval with the farthest end.
//! 3. **Call handling** — a vreg whose interval spans any call point and which
//!    landed in a **caller-saved** register is conservatively spilled (it gets a
//!    home and is reloaded around the call). Callee-saved registers survive calls,
//!    so they are preferred for long-lived vregs and need no per-call traffic.
//!
//! The result is a [`RegAlloc`]: a per-local [`Loc`] map, the set of callee-saved
//! registers actually used (for prologue save/restore), and the set of vregs that
//! still need a stack home (spills). Correctness is the only goal at v0.15; the
//! allocator is deliberately simple and conservative.

use std::collections::HashSet;

use crate::reg::{is_caller_saved, Gpr, ALLOC_REGS};
use crate::{frame, layout};
use k2_mir::{LocalId, MirFunction, Operand, Rvalue, Statement, Terminator};
use k2_types::{Type, TypeArena, TypeId};

/// Where a vreg's value lives.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Loc {
    /// In an integer register.
    Reg(Gpr),
    /// In a stack home (a spill, or a value that simply could not get a register).
    /// The actual offset is filled in by the frame plan via `needs_home`.
    Spill,
}

/// The allocation result for one function.
pub struct RegAlloc {
    /// `loc[i]` = where local `i` lives. Memory-aggregate / address-taken locals
    /// are `Spill` (they always have a home). Scalars are `Reg` or `Spill`.
    pub loc: Vec<Loc>,
    /// `needs_home[i]` = local `i` needs a stack home (aggregate, address-taken,
    /// or a spilled scalar). Drives [`crate::frame::plan`].
    pub needs_home: Vec<bool>,
    /// `home_ty[i]` = the type to *size* local `i`'s home by, overriding the
    /// local's declared type when it is `Deferred`/opaque but produced as a
    /// concrete aggregate (a print argument tuple). `None` uses the declared type.
    pub home_ty: Vec<Option<TypeId>>,
    /// `home_size[i]` = an explicit byte size to reserve for local `i`'s home,
    /// overriding any type-derived size. Used for a `deferred`-typed aggregate
    /// (a print tuple) whose type is not layoutable: the size is computed from the
    /// field operand types. `None` falls back to the type-derived size.
    pub home_size: Vec<Option<(u64, u64)>>,
    /// `agg_fields[i]` = the `(field type, byte offset)` of each field of local
    /// `i`, for a forced-home synthetic aggregate (a `deferred` tuple). Lets the
    /// print path resolve the tuple's field layout. `None` for non-aggregates.
    pub agg_fields: Vec<Option<Vec<(TypeId, u64)>>>,
    /// `array_len[i]` = the element count of local `i`, when it is an
    /// inferred-length (`[_]T`) array assigned an array literal — the type carries
    /// no `Known` length, so `.len` reads this. `None` otherwise.
    pub array_len: Vec<Option<u64>>,
    /// The callee-saved registers the allocator assigned (prologue save set).
    pub callee_saved: Vec<Gpr>,
}

/// `true` if `ty` is a scalar the allocator may place in an integer register
/// (integer/bool/pointer/pointer-niche-optional/enum/capability handle). Floats,
/// memory aggregates, and wide (>8-byte, i.e. `u128`/`i128`) integers are
/// excluded — a wide integer needs a 16-byte memory home.
fn is_int_scalar(arena: &TypeArena, ty: TypeId) -> bool {
    if frame::is_memory_aggregate(arena, ty) {
        return false;
    }
    match arena.get(ty) {
        Type::Float { .. } => false,
        Type::Int { bits, .. } => layout::int_byte_size(*bits) <= 8,
        _ => true,
    }
}

/// A half-open `[start, end]` live interval over the linear program numbering.
#[derive(Clone, Copy)]
struct Interval {
    local: LocalId,
    start: usize,
    end: usize,
    /// `true` if the interval spans any call/clobber point.
    spans_call: bool,
}

/// Computes the allocation for `func`.
pub fn allocate(func: &MirFunction, arena: &TypeArena) -> RegAlloc {
    let n = func.locals.len();
    let mut needs_home = vec![false; n];
    let mut loc = vec![Loc::Spill; n];

    // Locals whose *declared* type is `Deferred`/opaque but which are produced as
    // an aggregate (a tuple/array/struct/slice/optional/error-union literal) or
    // consumed by an aggregate projection need a memory home too — the MIR can
    // leave a print argument tuple typed `deferred`. Detect those + the type to
    // size their home by.
    let mut home_ty: Vec<Option<TypeId>> = vec![None; n];
    let mut home_size: Vec<Option<(u64, u64)>> = vec![None; n];
    let mut agg_fields: Vec<Option<Vec<(TypeId, u64)>>> = vec![None; n];
    let mut array_len: Vec<Option<u64>> = vec![None; n];
    let forced_home = forced_home_locals(
        func,
        arena,
        &mut home_ty,
        &mut home_size,
        &mut agg_fields,
        &mut array_len,
    );

    // Decide which locals are vregs (register-allocatable scalars). Aggregates and
    // address-taken locals always need a home and never get a register.
    let mut is_vreg = vec![false; n];
    for (i, local) in func.locals.iter().enumerate() {
        if frame::is_memory_aggregate(arena, local.ty) || local.address_taken || forced_home[i] {
            needs_home[i] = true;
        } else if is_int_scalar(arena, local.ty) {
            is_vreg[i] = true;
        } else {
            // A float scalar (no float-reg allocator at v0.15) — give it a home.
            needs_home[i] = true;
        }
    }

    // ---- Linearize and gather per-point def/use and call points. ----
    let points = linearize(func);
    let np = points.len();

    // Backward liveness fixpoint over the CFG. We work at block granularity for
    // live-in/out, then refine to per-point intervals.
    let live = compute_liveness(func, &points, &is_vreg);

    // Build intervals: [min,max] over def/use/live points; spans_call if any call
    // point lies within the interval.
    let mut intervals: Vec<Interval> = Vec::new();
    let mut first = vec![usize::MAX; n];
    let mut last = vec![0usize; n];
    let mut seen = vec![false; n];
    let touch = |i: usize, p: usize, first: &mut [usize], last: &mut [usize], seen: &mut [bool]| {
        if !seen[i] {
            seen[i] = true;
            first[i] = p;
            last[i] = p;
        } else {
            first[i] = first[i].min(p);
            last[i] = last[i].max(p);
        }
    };
    for (p, pt) in points.iter().enumerate() {
        for &d in &pt.defs {
            if is_vreg[d.index()] {
                touch(d.index(), p, &mut first, &mut last, &mut seen);
            }
        }
        for &u in &pt.uses {
            if is_vreg[u.index()] {
                touch(u.index(), p, &mut first, &mut last, &mut seen);
            }
        }
        // Liveness contributes points too: a vreg live across point p must keep
        // its register there.
        for &v in &live[p] {
            if is_vreg[v.index()] {
                touch(v.index(), p, &mut first, &mut last, &mut seen);
            }
        }
    }
    let call_points: Vec<usize> = (0..np).filter(|&p| points[p].is_call).collect();
    for i in 0..n {
        if !seen[i] {
            continue;
        }
        let (s, e) = (first[i], last[i]);
        let spans_call = call_points.iter().any(|&c| c >= s && c <= e);
        intervals.push(Interval {
            local: LocalId(i as u32),
            start: s,
            end: e,
            spans_call,
        });
    }

    // ---- The scan. ----
    intervals.sort_by_key(|iv| iv.start);
    // Free registers, partitioned so callee-saved are preferred for call-spanning
    // intervals and caller-saved for short-lived ones.
    let mut free: Vec<Gpr> = ALLOC_REGS.to_vec();
    // active: (interval index in `intervals`, assigned reg), sorted by end.
    let mut active: Vec<(usize, Gpr)> = Vec::new();
    let mut callee_used: HashSet<Gpr> = HashSet::new();

    for idx in 0..intervals.len() {
        let iv = intervals[idx];
        // Expire intervals that ended before this one starts.
        active.retain(|&(aidx, areg)| {
            if intervals[aidx].end < iv.start {
                free.push(areg);
                false
            } else {
                true
            }
        });
        // Choose a register. A call-spanning interval must use a callee-saved reg
        // (otherwise it would be clobbered); pick one if available.
        let want_callee = iv.spans_call;
        let pick = pick_register(&mut free, want_callee);
        match pick {
            Some(r) => {
                loc[iv.local.index()] = Loc::Reg(r);
                if !is_caller_saved(r) {
                    callee_used.insert(r);
                }
                active.push((idx, r));
                active.sort_by_key(|&(aidx, _)| intervals[aidx].end);
            }
            None => {
                // Spill: this interval (or the farthest-ending active one). Pick
                // the farthest end to spill, classic heuristic.
                spill_farthest(idx, &mut active, &intervals, &mut loc, &mut needs_home);
            }
        }
    }

    // Any vreg that did not get a register needs a home.
    for i in 0..n {
        if is_vreg[i] && matches!(loc[i], Loc::Spill) {
            needs_home[i] = true;
        }
    }

    let mut callee_saved: Vec<Gpr> = callee_used.into_iter().collect();
    // Deterministic order for a stable prologue/epilogue.
    callee_saved.sort_by_key(|r| r.num());

    RegAlloc {
        loc,
        needs_home,
        home_ty,
        home_size,
        agg_fields,
        array_len,
        callee_saved,
    }
}

/// The element type of an array/slice type (falls back to the type itself).
fn elem_of(arena: &TypeArena, ty: TypeId) -> TypeId {
    match arena.get(ty) {
        Type::Array { elem, .. } | Type::Slice { elem, .. } => *elem,
        _ => ty,
    }
}

/// The declared type of an operand (for synthetic tuple sizing): a bare local's
/// type, the result type of a projected place (walking field/index/slice-meta/
/// payload/deref projections), or `None` for a constant.
fn operand_decl_type(func: &MirFunction, arena: &TypeArena, op: &Operand) -> Option<TypeId> {
    match op {
        Operand::Copy(p) => Some(place_result_type(func, arena, p)),
        _ => None,
    }
}

/// The type a place yields after its projection chain (mirrors `lower::place_type`).
fn place_result_type(func: &MirFunction, arena: &TypeArena, place: &k2_mir::Place) -> TypeId {
    let mut cur = func.locals[place.base.index()].ty;
    for proj in &place.proj {
        cur = match proj {
            k2_mir::Proj::Field { ty, .. }
            | k2_mir::Proj::Index { ty, .. }
            | k2_mir::Proj::SliceMeta { ty, .. }
            | k2_mir::Proj::Payload { ty } => *ty,
            k2_mir::Proj::Deref => match arena.get(cur) {
                Type::Pointer { pointee, .. } => *pointee,
                _ => cur,
            },
        };
    }
    cur
}

/// Computes the packed (struct-style) layout of a tuple/struct from its field
/// types: running `offset = round_up(offset, falign); offset += fsize`, with
/// `align = max field align`. Returns `(size, align, field_offsets)`. Matches the
/// `reflect`/`layout` struct rule so a synthetic tuple's interior agrees with
/// `@sizeOf`-derived expectations elsewhere.
pub fn packed_layout(arena: &TypeArena, field_tys: &[TypeId]) -> (u64, u64, Vec<u64>) {
    let mut offset = 0u64;
    let mut max_align = 1u64;
    let mut offs = Vec::with_capacity(field_tys.len());
    for &ft in field_tys {
        let l = layout::layout_of(arena, ft).unwrap_or(layout::Layout::WORD);
        let a = l.align.max(1);
        offset = layout::round_up(offset, a);
        offs.push(offset);
        offset += l.size;
        max_align = max_align.max(a);
    }
    (layout::round_up(offset, max_align), max_align, offs)
}

/// Detects locals that must have a memory home despite a `Deferred`/opaque
/// declared type, because they are produced as a concrete aggregate (a literal,
/// slice, optional, or error-union construction) — chiefly the print argument
/// tuple, which the MIR types `deferred`. Records the aggregate's concrete type
/// in `home_ty` so the frame planner can size the home.
fn forced_home_locals(
    func: &MirFunction,
    arena: &TypeArena,
    home_ty: &mut [Option<TypeId>],
    home_size: &mut [Option<(u64, u64)>],
    agg_fields: &mut [Option<Vec<(TypeId, u64)>>],
    array_len: &mut [Option<u64>],
) -> Vec<bool> {
    let mut forced = vec![false; func.locals.len()];
    for block in &func.blocks {
        for stmt in &block.stmts {
            let (dst, rv) = match stmt {
                Statement::Assign { place, rvalue, .. } if place.is_local() => (place.base, rvalue),
                _ => continue,
            };
            let di = dst.index();
            // An inferred-length array (`[_]T`) assigned a literal: record its
            // element count for `.len` and ensure its home is sized correctly.
            if let Rvalue::Aggregate {
                kind: k2_mir::AggKind::Array,
                fields,
                ty,
            } = rv
            {
                if matches!(
                    arena.get(*ty),
                    Type::Array {
                        len: k2_types::ArrayLen::Inferred | k2_types::ArrayLen::Deferred,
                        ..
                    }
                ) {
                    array_len[di] = Some(fields.len() as u64);
                    let esz = layout::elem_size(arena, *ty);
                    let ea = layout::layout_of(arena, elem_of(arena, *ty))
                        .map(|l| l.align)
                        .unwrap_or(1)
                        .max(1);
                    home_size[di] = Some(((esz * fields.len() as u64).max(1), ea));
                    home_ty[di] = Some(*ty);
                    forced[di] = true;
                    continue;
                }
            }
            // A local already typed as a memory aggregate is sized by the planner
            // from its declared type; nothing to force.
            if frame::is_memory_aggregate(arena, func.locals[di].ty) {
                continue;
            }
            match rv {
                // A non-empty aggregate literal always needs a home. Its declared
                // `ty` may be `deferred` (a print argument tuple) — record it
                // anyway; the lowering computes a synthetic layout from the field
                // operand types when `ty` is not layoutable.
                Rvalue::Aggregate { fields, ty, .. } if !fields.is_empty() => {
                    forced[di] = true;
                    home_ty[di] = Some(*ty);
                    // When the declared aggregate type is not layoutable (a
                    // `deferred` tuple), size the home from the field operand
                    // types via a synthetic packed layout.
                    if layout::layout_of(arena, *ty).is_none() {
                        let field_tys: Vec<TypeId> = fields
                            .iter()
                            .map(|f| operand_decl_type(func, arena, f).unwrap_or(*ty))
                            .collect();
                        let (sz, al, offs) = packed_layout(arena, &field_tys);
                        home_size[di] = Some((sz.max(1), al.max(1)));
                        agg_fields[di] = Some(
                            field_tys
                                .iter()
                                .copied()
                                .zip(offs.iter().copied())
                                .collect(),
                        );
                    }
                }
                Rvalue::MakeSlice { ty, .. }
                | Rvalue::MakeSome(_, ty)
                | Rvalue::MakeNull(ty)
                | Rvalue::MakeOk(_, ty)
                | Rvalue::MakeErr(_, ty) => {
                    if frame::is_memory_aggregate(arena, *ty) {
                        forced[di] = true;
                        home_ty[di] = Some(*ty);
                    }
                }
                _ => {}
            }
        }
    }
    forced
}

/// Picks a free register. When `want_callee` is set, prefer a callee-saved one
/// (so the value survives a call); otherwise prefer a caller-saved one (cheaper).
fn pick_register(free: &mut Vec<Gpr>, want_callee: bool) -> Option<Gpr> {
    // `free` is in ALLOC_REGS preference order (callee-saved first). Find the best
    // candidate by the requested class, falling back to any.
    let find = |free: &[Gpr], callee: bool| free.iter().position(|&r| is_caller_saved(r) != callee);
    let pos = if want_callee {
        find(free, true).or_else(|| find(free, false))
    } else {
        find(free, false).or_else(|| find(free, true))
    };
    pos.map(|p| free.remove(p))
}

/// Spills either the new interval `idx` or the farthest-ending active interval
/// (whichever ends later), freeing a register for the other. The spilled local
/// gets a home; the surviving one keeps/takes the freed register.
fn spill_farthest(
    idx: usize,
    active: &mut [(usize, Gpr)],
    intervals: &[Interval],
    loc: &mut [Loc],
    needs_home: &mut [bool],
) {
    let new_iv = intervals[idx];
    // The active interval with the farthest end.
    if let Some(&(far_aidx, far_reg)) = active.iter().max_by_key(|&&(aidx, _)| intervals[aidx].end)
    {
        if intervals[far_aidx].end > new_iv.end {
            // Spill the active one; give its register to the new interval.
            let spilled = intervals[far_aidx].local;
            loc[spilled.index()] = Loc::Spill;
            needs_home[spilled.index()] = true;
            loc[new_iv.local.index()] = Loc::Reg(far_reg);
            // Rewire the active slot to the new interval.
            for slot in active.iter_mut() {
                if slot.0 == far_aidx {
                    *slot = (idx, far_reg);
                    break;
                }
            }
            return;
        }
    }
    // Otherwise spill the new interval itself.
    loc[new_iv.local.index()] = Loc::Spill;
    needs_home[new_iv.local.index()] = true;
}

/// One linearized program point: the def/use locals and whether it is a call.
struct Point {
    defs: Vec<LocalId>,
    uses: Vec<LocalId>,
    is_call: bool,
}

/// Numbers every statement + terminator in block order into a flat point list.
fn linearize(func: &MirFunction) -> Vec<Point> {
    let mut points = Vec::new();
    for block in &func.blocks {
        for stmt in &block.stmts {
            points.push(stmt_point(stmt));
        }
        points.push(term_point(&block.term));
    }
    points
}

/// The def/use point of one statement.
fn stmt_point(stmt: &Statement) -> Point {
    let mut defs = Vec::new();
    let mut uses = Vec::new();
    let mut is_call = false;
    match stmt {
        Statement::Assign { place, rvalue, .. } => {
            if place.is_local() {
                defs.push(place.base);
            } else {
                uses.push(place.base);
                index_uses(place, &mut uses);
            }
            rvalue_uses(rvalue, &mut uses);
            is_call = rvalue_is_call(rvalue);
        }
        Statement::Eval { rvalue, .. } => {
            rvalue_uses(rvalue, &mut uses);
            is_call = rvalue_is_call(rvalue);
        }
        Statement::StorageLive(_)
        | Statement::StorageDead(_)
        | Statement::Check(_)
        | Statement::Note(_) => {}
    }
    Point {
        defs,
        uses,
        is_call,
    }
}

/// The use point of a terminator.
fn term_point(term: &Terminator) -> Point {
    let mut uses = Vec::new();
    match term {
        Terminator::Branch { cond, .. } => operand_uses(cond, &mut uses),
        Terminator::Switch { scrutinee, .. } => operand_uses(scrutinee, &mut uses),
        Terminator::Return { value } => operand_uses(value, &mut uses),
        Terminator::Goto(_) | Terminator::Trap { .. } | Terminator::Unreachable => {}
    }
    Point {
        defs: Vec::new(),
        uses,
        is_call: false,
    }
}

/// `true` if an rvalue performs a `call`/`syscall` (clobbering caller-saved regs).
fn rvalue_is_call(rv: &Rvalue) -> bool {
    matches!(rv, Rvalue::Call { .. } | Rvalue::Intrinsic { .. })
}

/// Appends the locals an operand reads.
fn operand_uses(op: &Operand, out: &mut Vec<LocalId>) {
    if let Operand::Copy(p) = op {
        out.push(p.base);
        index_uses(p, out);
    }
}

/// Appends locals used inside a place's `Index` projections.
fn index_uses(p: &k2_mir::Place, out: &mut Vec<LocalId>) {
    for proj in &p.proj {
        if let k2_mir::Proj::Index { index, .. } = proj {
            operand_uses(index, out);
        }
    }
}

/// Appends the locals an rvalue reads.
fn rvalue_uses(rv: &Rvalue, out: &mut Vec<LocalId>) {
    match rv {
        Rvalue::Use(o)
        | Rvalue::MakeSome(o, _)
        | Rvalue::MakeOk(o, _)
        | Rvalue::Cast { operand: o, .. }
        | Rvalue::Unary { operand: o, .. }
        | Rvalue::Discriminant { operand: o, .. } => operand_uses(o, out),
        Rvalue::Ref { place, .. } => {
            out.push(place.base);
            index_uses(place, out);
        }
        Rvalue::Binary { lhs, rhs, .. } => {
            operand_uses(lhs, out);
            operand_uses(rhs, out);
        }
        Rvalue::MakeSlice {
            ptr, offset, len, ..
        } => {
            operand_uses(ptr, out);
            operand_uses(offset, out);
            operand_uses(len, out);
        }
        Rvalue::Aggregate { fields, .. } => {
            for f in fields {
                operand_uses(f, out);
            }
        }
        Rvalue::Call { args, .. } => {
            for a in args {
                operand_uses(a, out);
            }
        }
        Rvalue::Intrinsic { path, args, .. } => {
            if let k2_mir::IntrinsicRoot::Value(op) = &path.root {
                operand_uses(op, out);
            }
            for a in args {
                operand_uses(a, out);
            }
        }
        Rvalue::MakeNull(_) | Rvalue::MakeErr(_, _) => {}
    }
}

/// Backward block-level liveness, refined to a per-point `live` set: `live[p]` is
/// the set of vregs live immediately *after* point `p` executes (i.e. used later).
fn compute_liveness(func: &MirFunction, points: &[Point], is_vreg: &[bool]) -> Vec<Vec<LocalId>> {
    let nb = func.blocks.len();
    // Per-block point ranges.
    let mut block_range = vec![(0usize, 0usize); nb];
    {
        let mut p = 0;
        for (bi, block) in func.blocks.iter().enumerate() {
            let start = p;
            p += block.stmts.len() + 1; // +1 terminator
            block_range[bi] = (start, p);
        }
    }
    // Successors per block.
    let succs: Vec<Vec<usize>> = func
        .blocks
        .iter()
        .map(|b| b.term.successors().iter().map(|s| s.index()).collect())
        .collect();

    // Block-level use/def (gen/kill) for live-in/out.
    let mut live_in: Vec<HashSet<LocalId>> = vec![HashSet::new(); nb];
    let mut live_out: Vec<HashSet<LocalId>> = vec![HashSet::new(); nb];

    let mut changed = true;
    while changed {
        changed = false;
        for bi in (0..nb).rev() {
            // out = union of successors' in.
            let mut out: HashSet<LocalId> = HashSet::new();
            for &s in &succs[bi] {
                for &v in &live_in[s] {
                    out.insert(v);
                }
            }
            // in = transfer(out) over the block's points, backward.
            let (start, end) = block_range[bi];
            let mut cur = out.clone();
            for p in (start..end).rev() {
                for &d in &points[p].defs {
                    cur.remove(&d);
                }
                for &u in &points[p].uses {
                    if is_vreg[u.index()] {
                        cur.insert(u);
                    }
                }
            }
            if cur != live_in[bi] {
                live_in[bi] = cur;
                changed = true;
            }
            if out != live_out[bi] {
                live_out[bi] = out;
                changed = true;
            }
        }
    }

    // Per-point live-after sets.
    let np = points.len();
    let mut live = vec![Vec::new(); np];
    for bi in 0..nb {
        let (start, end) = block_range[bi];
        let mut cur = live_out[bi].clone();
        for p in (start..end).rev() {
            // live-after this point = cur (before applying this point's def/use).
            live[p] = cur.iter().copied().collect();
            for &d in &points[p].defs {
                cur.remove(&d);
            }
            for &u in &points[p].uses {
                if is_vreg[u.index()] {
                    cur.insert(u);
                }
            }
        }
    }
    live
}
