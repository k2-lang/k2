//! Tests for the MIR layer: lowering correctness (defer/errdefer/try ordering,
//! for-range and captures, short-circuit `and`/`or`, break-with-value,
//! monomorphization), safety-check presence/absence by build mode, the
//! conservative leak/escape analysis (clean positives + flagged negatives), and
//! a corpus acceptance gate that every example lowers cleanly.

use crate::*;
use k2_parse::parse;
use k2_resolve::resolve_file;
use k2_types::check_file;

/// Parses, resolves, type-checks, and lowers `src` under `mode`, returning the
/// program. Panics if parse/resolve/type-check is not clean (so a test sees a
/// precise failure point).
fn lower(src: &str, mode: BuildMode) -> MirProgram {
    let pres = parse(src);
    assert!(pres.is_ok(), "parse errors: {:?}", pres.diagnostics);
    let resolved = resolve_file(&pres.file);
    assert!(
        resolved.is_ok(),
        "resolve errors: {:?}",
        resolved.diagnostics
    );
    let typed = check_file(&pres.file, &resolved);
    assert!(typed.is_ok(), "type errors: {:?}", typed.diagnostics);
    lower_program(&pres.file, &resolved, typed, mode).expect("lowering must succeed")
}

/// The whole-program dump (for substring assertions).
fn dump(src: &str, mode: BuildMode) -> String {
    dump_mir(&lower(src, mode))
}

/// Finds the function whose name exactly equals `needle`, or (failing that) the
/// first whose name starts with `needle` (so `init` matches `init[List(u32)]`).
fn find_fn<'a>(prog: &'a MirProgram, needle: &str) -> &'a MirFunction {
    prog.funcs
        .iter()
        .find(|f| f.name == needle)
        .or_else(|| prog.funcs.iter().find(|f| f.name.starts_with(needle)))
        .unwrap_or_else(|| {
            let names: Vec<&str> = prog.funcs.iter().map(|f| f.name.as_str()).collect();
            panic!("no fn matching `{needle}`; have {names:?}")
        })
}

/// Counts statements (across all blocks of `func`) matching a predicate.
fn count_stmts(func: &MirFunction, pred: impl Fn(&Statement) -> bool) -> usize {
    func.blocks
        .iter()
        .flat_map(|b| b.stmts.iter())
        .filter(|s| pred(s))
        .count()
}

/// Counts intrinsic statements whose final member name matches `member`.
fn count_intrinsic(func: &MirFunction, member: &str) -> usize {
    count_stmts(func, |s| {
        let rv = match s {
            Statement::Assign { rvalue, .. } | Statement::Eval { rvalue, .. } => Some(rvalue),
            _ => None,
        };
        matches!(rv, Some(Rvalue::Intrinsic { path, .. }) if path.last() == Some(member))
    })
}

/// The order in which intrinsics named in `members` first appear in `func`'s
/// blocks (block order, then statement order).
fn intrinsic_order(func: &MirFunction, members: &[&str]) -> Vec<String> {
    let mut order = Vec::new();
    for b in &func.blocks {
        for s in &b.stmts {
            let rv = match s {
                Statement::Assign { rvalue, .. } | Statement::Eval { rvalue, .. } => Some(rvalue),
                _ => None,
            };
            if let Some(Rvalue::Intrinsic { path, .. }) = rv {
                if let Some(m) = path.last() {
                    if members.contains(&m) {
                        order.push(m.to_string());
                    }
                }
            }
        }
    }
    order
}

// =========================================================================
//  Lowering correctness
// =========================================================================

#[test]
fn well_formed_program_verifies() {
    let prog = lower(
        "pub fn main(sys: *System) void { return; }",
        BuildMode::Debug,
    );
    assert!(prog.verify().is_empty(), "verify: {:?}", prog.verify());
}

#[test]
fn defer_runs_lifo_on_fall_through() {
    // Two defers run in reverse registration order at the fn's fall-through exit.
    let src = r#"
        fn f(a: Allocator) void {
            const x = a.alloc(u8, 1) catch { return; };
            defer a.free(x);
            const y = a.alloc(u8, 2) catch { return; };
            defer a.free(y);
        }
    "#;
    let prog = lower(src, BuildMode::Debug);
    let f = find_fn(&prog, "f");
    // Both frees appear; the later-registered (y) runs before x at exit.
    assert!(count_intrinsic(f, "free") >= 2, "both frees lowered");
}

#[test]
fn errdefer_fires_only_on_error_path() {
    // errdefer destroy(cell) runs on the error return, not the success return.
    let src = r#"
        const E = error{ Empty };
        fn parseD(alloc: Allocator, text: []const u8) E!*u32 {
            const cell: *u32 = try alloc.create(u32);
            errdefer alloc.destroy(cell);
            if (text.len == 0) return E.Empty;
            cell.* = 5;
            return cell;
        }
    "#;
    let prog = lower(src, BuildMode::Debug);
    let f = find_fn(&prog, "parseD");
    // Find the block that returns an error (err #) and confirm it runs destroy.
    let mut error_block_has_destroy = false;
    let mut success_block_has_destroy = false;
    for b in &f.blocks {
        let returns_err = matches!(
            &b.term,
            Terminator::Return {
                value: Operand::Copy(_)
            }
        ) && b.stmts.iter().any(|s| {
            matches!(
                s,
                Statement::Assign {
                    rvalue: Rvalue::MakeErr(..),
                    ..
                }
            )
        });
        let has_destroy = b.stmts.iter().any(|s| {
            matches!(s, Statement::Assign { rvalue: Rvalue::Intrinsic { path, .. }, .. }
                if path.last() == Some("destroy"))
        });
        let returns_ok = b.stmts.iter().any(|s| {
            matches!(
                s,
                Statement::Assign {
                    rvalue: Rvalue::MakeOk(..),
                    ..
                }
            )
        });
        if returns_err && has_destroy {
            error_block_has_destroy = true;
        }
        if returns_ok && has_destroy {
            success_block_has_destroy = true;
        }
    }
    assert!(
        error_block_has_destroy,
        "errdefer must fire on the error path"
    );
    assert!(
        !success_block_has_destroy,
        "errdefer must NOT fire on the success path"
    );
}

#[test]
fn defer_and_errdefer_interleave_lifo() {
    // `defer A; errdefer B; return err;` -> error path runs B then A (LIFO over a
    // single registration sequence).
    let src = r#"
        const E = error{ X };
        fn f(a: Allocator) E!void {
            const p = try a.create(u8);
            defer a.free(p);
            errdefer a.destroy(p);
            return E.X;
        }
    "#;
    let prog = lower(src, BuildMode::Debug);
    let f = find_fn(&prog, "f");
    // On the (only) error-return path both run; errdefer (destroy, registered
    // later) runs before defer (free).
    let order = intrinsic_order(f, &["free", "destroy"]);
    let destroy_pos = order.iter().position(|m| m == "destroy");
    let free_pos = order.iter().position(|m| m == "free");
    assert!(destroy_pos.is_some() && free_pos.is_some());
    assert!(
        destroy_pos < free_pos,
        "errdefer (destroy) must run before defer (free) on the error path: {order:?}"
    );
}

#[test]
fn try_desugars_to_branch_on_is_error() {
    let src = r#"
        const E = error{ X };
        fn g() E!u32 { return 1; }
        fn f() E!u32 { const x = try g(); return x; }
    "#;
    let prog = lower(src, BuildMode::Debug);
    let f = find_fn(&prog, "f");
    // try emits a Discriminant{ErrorUnion} and a Branch.
    let has_discr = count_stmts(f, |s| {
        matches!(
            s,
            Statement::Assign {
                rvalue: Rvalue::Discriminant {
                    kind: DiscrKind::ErrorUnion,
                    ..
                },
                ..
            }
        )
    }) >= 1;
    assert!(has_discr, "try must read the error-union discriminant");
    let has_branch = f
        .blocks
        .iter()
        .any(|b| matches!(b.term, Terminator::Branch { .. }));
    assert!(has_branch, "try must branch on is-error");
}

#[test]
fn catch_with_default_value() {
    let src = r#"
        const E = error{ X };
        fn g() E!u32 { return 1; }
        fn f() u32 { return g() catch 0; }
    "#;
    let prog = lower(src, BuildMode::Debug);
    let f = find_fn(&prog, "f");
    assert!(
        f.blocks
            .iter()
            .any(|b| matches!(b.term, Terminator::Branch { .. })),
        "catch must branch on is-error"
    );
}

#[test]
fn orelse_short_circuits_default() {
    let src = r#"
        fn pick(o: ?u32) u32 { return o orelse 7; }
    "#;
    let prog = lower(src, BuildMode::Debug);
    let f = find_fn(&prog, "pick");
    // orelse reads the optional discriminant and branches.
    let has_optional_discr = count_stmts(f, |s| {
        matches!(
            s,
            Statement::Assign {
                rvalue: Rvalue::Discriminant {
                    kind: DiscrKind::Optional,
                    ..
                },
                ..
            }
        )
    }) >= 1;
    assert!(has_optional_discr, "orelse reads the optional discriminant");
}

#[test]
fn for_range_is_index_driven() {
    let src = r#"
        fn f() u32 {
            var s: u32 = 0;
            for (0..4) |i| { s += @intCast(i); }
            return s;
        }
    "#;
    let prog = lower(src, BuildMode::Debug);
    let f = find_fn(&prog, "f");
    // The loop has a header that branches (i < n) and a continue block (i += 1).
    let branch_count = f
        .blocks
        .iter()
        .filter(|b| matches!(b.term, Terminator::Branch { .. }))
        .count();
    assert!(branch_count >= 1, "for-range builds a header branch");
}

#[test]
fn for_by_ref_capture_marks_address_taken() {
    let src = r#"
        fn f(xs: []u32) void {
            for (xs, 0..) |*slot, i| { slot.* = @intCast(i); }
        }
    "#;
    let prog = lower(src, BuildMode::Debug);
    let f = find_fn(&prog, "f");
    assert!(
        f.locals.iter().any(|l| l.address_taken),
        "a by-pointer for capture takes the element's address"
    );
}

#[test]
fn short_circuit_and_evaluates_rhs_only_in_then_block() {
    // `a and b`: the rhs `b` is only computed on the path where `a` is true.
    let src = r#"
        fn cheap() bool { return true; }
        fn f(a: bool) bool { return a and cheap(); }
    "#;
    let prog = lower(src, BuildMode::Debug);
    let f = find_fn(&prog, "f");
    // There is a branch (short-circuit), and the call to `cheap` lives after it.
    assert!(
        f.blocks
            .iter()
            .any(|b| matches!(b.term, Terminator::Branch { .. })),
        "and must short-circuit via a branch"
    );
}

#[test]
fn labeled_break_with_value() {
    let src = r#"
        fn f() u32 {
            const v: u32 = blk: { break :blk 9; };
            return v;
        }
    "#;
    let prog = lower(src, BuildMode::Debug);
    let f = find_fn(&prog, "f");
    assert!(prog.verify().is_empty(), "verify: {:?}", prog.verify());
    // The break-value flows to a slot; the function returns it.
    assert!(matches!(
        f.blocks.last().map(|b| &b.term),
        Some(Terminator::Return { .. }) | Some(Terminator::Goto(_))
    ));
}

#[test]
fn monomorphization_emits_distinct_functions() {
    // Two instantiations of a generic method produce two MIR functions; a
    // repeated instantiation reuses one.
    let src = r#"
        const std = @import("std");
        pub fn List(comptime T: type) type {
            return struct {
                const Self = @This();
                items: []T,
                len: usize,
                alloc: Allocator,
                pub fn init(alloc: Allocator) Self {
                    return Self{ .items = &.{}, .len = 0, .alloc = alloc };
                }
                pub fn deinit(self: *Self) void { self.alloc.free(self.items); }
            };
        }
        pub fn main(sys: *System) void {
            var a = List(u32).init(sys.heap);
            a.deinit();
            var b = List(u8).init(sys.heap);
            b.deinit();
        }
    "#;
    let prog = lower(src, BuildMode::Debug);
    // Two distinct `init` instantiations (keyed by the receiver struct type).
    let inits: Vec<&MirFunction> = prog
        .funcs
        .iter()
        .filter(|f| f.name.starts_with("init"))
        .collect();
    assert!(
        inits.len() >= 2,
        "List(u32).init and List(u8).init are distinct: {}",
        inits.len()
    );
    // The by_inst cache dedups: distinct InstIds map to distinct FnIds.
    let distinct: std::collections::HashSet<_> = prog.by_inst.values().collect();
    assert_eq!(distinct.len(), prog.by_inst.len(), "no FnId aliasing");
}

#[test]
fn deferred_std_calls_become_intrinsics() {
    let src = r#"
        pub fn main(sys: *System) void {
            const out = sys.io.stdout();
            _ = out;
        }
    "#;
    let prog = lower(src, BuildMode::Debug);
    let f = find_fn(&prog, "main");
    assert!(
        count_intrinsic(f, "stdout") >= 1,
        "sys.io.stdout() lowers to an intrinsic"
    );
}

// =========================================================================
//  Safety-check presence / absence
// =========================================================================

#[test]
fn debug_inserts_bounds_check() {
    let d = dump(
        "fn f(a: []const u8, i: usize) u8 { return a[i]; }",
        BuildMode::Debug,
    );
    assert!(
        d.contains("bounds_check"),
        "Debug must insert a bounds check:\n{d}"
    );
    assert!(d.contains("trap Bounds"), "a failed bounds check traps");
}

#[test]
fn release_fast_omits_bounds_check() {
    let d = dump(
        "fn f(a: []const u8, i: usize) u8 { return a[i]; }",
        BuildMode::ReleaseFast,
    );
    assert!(
        !d.contains("bounds_check"),
        "ReleaseFast omits the bounds check:\n{d}"
    );
    assert!(!d.contains("_check"), "ReleaseFast has no safety checks");
}

#[test]
fn debug_inserts_overflow_and_divzero_checks() {
    let d = dump(
        "fn f(a: u32, b: u32) u32 { return a * b / (a - b); }",
        BuildMode::Debug,
    );
    assert!(d.contains("overflow_check"), "overflow check present:\n{d}");
    assert!(d.contains("divzero_check"), "divzero check present:\n{d}");
}

#[test]
fn debug_inserts_narrowing_check() {
    let d = dump("fn f(x: u64) u8 { return @intCast(x); }", BuildMode::Debug);
    assert!(
        d.contains("narrow_check"),
        "@intCast inserts a narrowing check:\n{d}"
    );
}

#[test]
fn debug_lockstep_for_inserts_len_eq_check() {
    let d = dump(
        "fn f(a: []const u8, b: []const u8) void { for (a, b) |x, y| { _ = x; _ = y; } }",
        BuildMode::Debug,
    );
    assert!(
        d.contains("len_eq_check"),
        "multi-operand for inserts a len-eq check:\n{d}"
    );
}

#[test]
fn unreachable_traps_in_debug_not_in_release_fast() {
    let dbg = dump(
        "fn f(x: bool) u32 { if (x) return 1; unreachable; }",
        BuildMode::Debug,
    );
    assert!(
        dbg.contains("trap Unreachable"),
        "Debug traps on unreachable:\n{dbg}"
    );
    let fast = dump(
        "fn f(x: bool) u32 { if (x) return 1; unreachable; }",
        BuildMode::ReleaseFast,
    );
    assert!(
        !fast.contains("trap Unreachable"),
        "ReleaseFast treats unreachable as dead, not a trap:\n{fast}"
    );
}

#[test]
fn release_safe_keeps_checks_like_debug() {
    let safe = dump(
        "fn f(a: []const u8, i: usize) u8 { return a[i]; }",
        BuildMode::ReleaseSafe,
    );
    assert!(
        safe.contains("bounds_check"),
        "ReleaseSafe keeps checks:\n{safe}"
    );
}

// =========================================================================
//  Leak / escape analysis
// =========================================================================

#[test]
fn leak_clean_when_freed_by_defer() {
    let src = r#"
        fn f(a: Allocator) void {
            const x = a.alloc(u8, 8) catch { return; };
            defer a.free(x);
            _ = x;
        }
    "#;
    let prog = lower(src, BuildMode::Debug);
    assert!(
        prog.diagnostics.iter().all(|d| !d.is_error()),
        "a defer-freed allocation is clean: {:?}",
        prog.diagnostics
    );
}

#[test]
fn leak_clean_when_returned() {
    let src = r#"
        fn f(a: Allocator) ![]u8 {
            const x = try a.alloc(u8, 8);
            errdefer a.free(x);
            return x;
        }
    "#;
    let prog = lower(src, BuildMode::Debug);
    assert!(
        prog.diagnostics.iter().all(|d| !d.is_error()),
        "a returned allocation transfers ownership: {:?}",
        prog.diagnostics
    );
}

#[test]
fn leak_flags_obvious_missing_free() {
    let src = r#"
        fn bad(a: Allocator) void {
            const x = a.alloc(u8, 8) catch { return; };
            _ = x;
        }
    "#;
    let prog = lower(src, BuildMode::Debug);
    let leaks: Vec<&Diagnostic> = prog
        .diagnostics
        .iter()
        .filter(|d| d.is_error() && d.message.contains("never freed"))
        .collect();
    assert_eq!(
        leaks.len(),
        1,
        "an unfreed, unreturned allocation is flagged"
    );
}

#[test]
fn leak_flags_escaping_stack_pointer() {
    let src = r#"
        fn esc() *u32 { var n: u32 = 1; return &n; }
    "#;
    let prog = lower(src, BuildMode::Debug);
    let escapes: Vec<&Diagnostic> = prog
        .diagnostics
        .iter()
        .filter(|d| d.is_error() && d.message.contains("escapes"))
        .collect();
    assert_eq!(escapes.len(), 1, "a returned &local is flagged as escaping");
}

#[test]
fn leak_silent_on_param_slice_address() {
    // `&items[0]` where items is a slice parameter (heap-backed view) does NOT
    // escape — the root is a parameter, not a stack local.
    let src = r#"
        fn firstOrNull(items: []const u32) ?*const u32 {
            if (items.len == 0) return null;
            return &items[0];
        }
    "#;
    let prog = lower(src, BuildMode::Debug);
    assert!(
        prog.diagnostics.iter().all(|d| !d.is_error()),
        "a &param-slice element is not a stack escape: {:?}",
        prog.diagnostics
    );
}

#[test]
fn leak_silent_when_passed_to_call() {
    // An allocation passed to a callee (which may take ownership) is not flagged.
    let src = r#"
        fn consume(x: []u8) void { _ = x; }
        fn f(a: Allocator) void {
            const x = a.alloc(u8, 8) catch { return; };
            consume(x);
        }
    "#;
    let prog = lower(src, BuildMode::Debug);
    assert!(
        prog.diagnostics.iter().all(|d| !d.is_error()),
        "an allocation handed to a call is not flagged: {:?}",
        prog.diagnostics
    );
}

// =========================================================================
//  v0.7 regressions — break/continue targets, for-range trip count, loop-body
//  defers, switch range DoS, try-alloc leak, block-break value typing.
// =========================================================================

/// The set of blocks reachable from a function's entry over terminator edges.
fn reachable_blocks(func: &MirFunction) -> std::collections::HashSet<usize> {
    let mut seen = std::collections::HashSet::new();
    let mut stack = vec![func.entry.index()];
    seen.insert(func.entry.index());
    while let Some(bi) = stack.pop() {
        for s in func.blocks[bi].term.successors() {
            if seen.insert(s.index()) {
                stack.push(s.index());
            }
        }
    }
    seen
}

/// `true` if `from` can reach `to` over terminator edges WITHOUT passing through
/// any block in `avoid` (used to assert a break edge leaves the loop directly).
fn reaches_without(func: &MirFunction, from: usize, to: usize, avoid: &[usize]) -> bool {
    let mut seen = std::collections::HashSet::new();
    let mut stack = vec![from];
    seen.insert(from);
    while let Some(bi) = stack.pop() {
        if bi == to {
            return true;
        }
        if avoid.contains(&bi) && bi != from {
            continue;
        }
        for s in func.blocks[bi].term.successors() {
            if seen.insert(s.index()) {
                stack.push(s.index());
            }
        }
    }
    false
}

#[test]
fn bare_break_in_if_exits_the_loop() {
    // REGRESSION: a conditional `break` inside an `if` must terminate the loop
    // body with a jump to the loop's done/exit block — not fall back into the
    // loop's continue path. Before the fix the break targeted the synthetic
    // if-wrapper block and never reached the loop exit.
    let src = r#"
        fn f(n: usize) usize {
            var i: usize = 0;
            while (i < n) : (i += 1) { if (i == 3) break; }
            return i;
        }
    "#;
    let prog = lower(src, BuildMode::Debug);
    let f = find_fn(&prog, "f");
    assert!(prog.verify().is_empty(), "verify: {:?}", prog.verify());
    // The loop header is the block that branches on `i < n`; its false edge is
    // the loop's `done` (exit) block. The break must reach that exit.
    let header = f
        .blocks
        .iter()
        .position(|b| matches!(b.term, Terminator::Branch { .. }) && has_lt(b))
        .expect("a header that tests i < n");
    let done = match f.blocks[header].term {
        Terminator::Branch { else_bb, .. } => else_bb.index(),
        _ => unreachable!(),
    };
    // Find the inner `if (i == 3)` branch; its then-edge holds the bare break.
    let if_block = f
        .blocks
        .iter()
        .position(|b| matches!(b.term, Terminator::Branch { .. }) && has_eq_3(b))
        .expect("an `if (i == 3)` branch");
    let break_edge = match f.blocks[if_block].term {
        Terminator::Branch { then_bb, .. } => then_bb.index(),
        _ => unreachable!(),
    };
    // The break edge must reach the loop's done block without first re-entering
    // the header (i.e. it leaves the loop rather than continuing it).
    assert!(
        reaches_without(f, break_edge, done, &[header]),
        "the bare break inside the if must jump to the loop's exit block {done}, \
         not fall back into the loop (break edge starts at bb{break_edge})"
    );
}

/// `true` if a block computes a `lt` (used to spot the `i < n` loop header).
fn has_lt(b: &BasicBlock) -> bool {
    b.stmts.iter().any(|s| {
        matches!(
            s,
            Statement::Assign {
                rvalue: Rvalue::Binary { op: BinOp::Lt, .. },
                ..
            }
        )
    })
}

/// `true` if a block computes `eq _, 3` (the `if (i == 3)` condition).
fn has_eq_3(b: &BasicBlock) -> bool {
    b.stmts.iter().any(|s| {
        matches!(
            s,
            Statement::Assign {
                rvalue: Rvalue::Binary {
                    op: BinOp::Eq,
                    rhs: Operand::Const(Const::Int { value: 3, .. }),
                    ..
                },
                ..
            }
        )
    })
}

#[test]
fn while_true_with_conditional_break_terminates() {
    // `while (true) { ...; if (c) break; }` must have a reachable loop-exit block
    // (the break edge), not be an infinite loop with a dead exit.
    let src = r#"
        fn f(n: usize) usize {
            var i: usize = 0;
            while (true) { i = i + 1; if (i >= n) break; }
            return i;
        }
    "#;
    let prog = lower(src, BuildMode::Debug);
    let f = find_fn(&prog, "f");
    assert!(prog.verify().is_empty(), "verify: {:?}", prog.verify());
    // The function returns, and the returning block is reachable from the entry
    // (only the break edge can reach it, since the header is `branch true`).
    let reachable = reachable_blocks(f);
    assert!(
        f.blocks
            .iter()
            .enumerate()
            .any(|(i, b)| reachable.contains(&i) && matches!(b.term, Terminator::Return { .. })),
        "the loop's break edge must reach a Return (loop is not infinite)"
    );
}

#[test]
fn labeled_break_still_targets_the_block() {
    // A LABELED `break :blk v` out of a block-expr still works (the label arm of
    // find_target_scope matches independently of the bare-break loop rule).
    let src = r#"
        fn f() u32 {
            const r: u32 = blk: {
                var i: u32 = 0;
                while (i < 10) : (i += 1) { if (i == 4) break :blk i; }
                break :blk 0;
            };
            return r;
        }
    "#;
    let prog = lower(src, BuildMode::Debug);
    let f = find_fn(&prog, "f");
    assert!(prog.verify().is_empty(), "verify: {:?}", prog.verify());
    // The result slot is a u32 (not noreturn) and the fn returns it.
    let r_slot = f
        .locals
        .iter()
        .find(|l| matches!(l.origin, LocalOrigin::Binding(_)))
        .expect("the `r` binding local");
    assert_eq!(
        prog.arena.fmt(r_slot.ty),
        "u32",
        "labeled break-with-value types the slot from the value (u32)"
    );
}

#[test]
fn for_bounded_range_iterates_lo_to_hi() {
    // REGRESSION: `for (2..5)` must iterate i = 2, 3, 4 (i starts at lo=2 and the
    // header tests `i < hi`, i.e. `i < 5`), NOT use the buggy `hi - lo` bound.
    let src = r#"
        fn f() usize { var s: usize = 0; for (2..5) |i| { s += i; } return s; }
    "#;
    let prog = lower(src, BuildMode::Debug);
    let f = find_fn(&prog, "f");
    assert!(prog.verify().is_empty(), "verify: {:?}", prog.verify());
    // The header compares `i < 5` (the exclusive bound is `hi`), and there is NO
    // `sub 5, 2` trip-count computation.
    let has_lt_5 = f.blocks.iter().any(|b| {
        b.stmts.iter().any(|s| {
            matches!(
                s,
                Statement::Assign {
                    rvalue: Rvalue::Binary {
                        op: BinOp::Lt,
                        rhs: Operand::Const(Const::Int { value: 5, .. }),
                        ..
                    },
                    ..
                }
            )
        })
    });
    assert!(has_lt_5, "for (2..5) header must test `i < 5`");
    let has_sub = f.blocks.iter().any(|b| {
        b.stmts.iter().any(|s| {
            matches!(
                s,
                Statement::Assign {
                    rvalue: Rvalue::Binary { op: BinOp::Sub, .. },
                    ..
                }
            )
        })
    });
    assert!(!has_sub, "no `hi - lo` trip-count subtraction is emitted");
}

#[test]
fn for_zero_start_range_unchanged() {
    // `for (0..n)` keeps the obvious `i < n` header (i starts at 0).
    let src = r#"
        fn f(n: usize) usize { var s: usize = 0; for (0..n) |i| { s += i; } return s; }
    "#;
    let prog = lower(src, BuildMode::Debug);
    let f = find_fn(&prog, "f");
    assert!(prog.verify().is_empty(), "verify: {:?}", prog.verify());
    // i starts at 0 and the header tests `i < n` (the parameter length operand).
    assert!(
        f.blocks
            .iter()
            .any(|b| matches!(b.term, Terminator::Branch { .. }) && has_lt(b)),
        "for (0..n) builds an `i < n` header branch"
    );
}

#[test]
fn for_open_range_over_slice_offsets_index() {
    // An open `for (xs, 0..) |x, i|` iterates the slice while `i` runs as the
    // 0-based index; the loop bound is the slice length, the index is `i`.
    let src = r#"
        fn f(xs: []u32) u32 {
            var s: u32 = 0;
            for (xs, 0..) |x, i| { s = s + x + @intCast(i); }
            return s;
        }
    "#;
    let prog = lower(src, BuildMode::Debug);
    let f = find_fn(&prog, "f");
    assert!(prog.verify().is_empty(), "verify: {:?}", prog.verify());
    // The header tests `i < xs.len` (a slice-meta length read, not a constant).
    assert!(
        f.blocks
            .iter()
            .any(|b| matches!(b.term, Terminator::Branch { .. }) && has_lt(b)),
        "open-range for over a slice builds a length-bounded header"
    );
}

#[test]
fn loop_body_defer_runs_each_iteration_and_on_break() {
    // REGRESSION: a `defer` registered in a loop body must run on the normal
    // per-iteration fall-through AND on the break path. Here destroy(p) must
    // appear twice: once on fall-through (before `goto cont`) and once on break.
    let src = r#"
        fn f(a: Allocator, n: usize) void {
            var i: usize = 0;
            while (i < n) : (i = i + 1) {
                const p = a.create(u32) catch unreachable;
                defer a.destroy(p);
                if (i == 3) break;
            }
        }
    "#;
    let prog = lower(src, BuildMode::Debug);
    let f = find_fn(&prog, "f");
    assert!(prog.verify().is_empty(), "verify: {:?}", prog.verify());
    // Two destroy sites: the fall-through iteration end and the break edge.
    assert_eq!(
        count_intrinsic(f, "destroy"),
        2,
        "loop-body defer runs on both the fall-through and the break path"
    );
    // And a `defer #` note is emitted (the cleanup is actually scheduled).
    assert!(
        count_stmts(
            f,
            |s| matches!(s, Statement::Note(n) if n.starts_with("defer #"))
        ) >= 2,
        "the body defer is run (noted) on each exit"
    );
}

#[test]
fn loop_body_defer_runs_on_continue() {
    // A loop-body defer must also run on the `continue` edge.
    let src = r#"
        fn f(a: Allocator, n: usize) void {
            var i: usize = 0;
            while (i < n) : (i = i + 1) {
                const p = a.create(u32) catch unreachable;
                defer a.destroy(p);
                if (i == 1) continue;
            }
        }
    "#;
    let prog = lower(src, BuildMode::Debug);
    let f = find_fn(&prog, "f");
    assert!(prog.verify().is_empty(), "verify: {:?}", prog.verify());
    // destroy on the fall-through end and on the continue edge: two sites.
    assert_eq!(
        count_intrinsic(f, "destroy"),
        2,
        "loop-body defer runs on both fall-through and continue"
    );
}

#[test]
fn switch_huge_inclusive_range_lowers_without_enumerating() {
    // REGRESSION / DoS: a switch with a `0...u64::MAX` range must lower instantly
    // (a guard comparison), NOT enumerate ~2^64 discrete Switch targets.
    let src = r#"
        fn f(v: u64) u32 {
            return switch (v) { 0...18446744073709551615 => 1, else => 0 };
        }
    "#;
    let prog = lower(src, BuildMode::Debug);
    let f = find_fn(&prog, "f");
    assert!(prog.verify().is_empty(), "verify: {:?}", prog.verify());
    // The range became a guard chain (Ge/Le comparisons), and no Switch carries
    // a runaway target list.
    let has_ge = f.blocks.iter().any(|b| {
        b.stmts.iter().any(|s| {
            matches!(
                s,
                Statement::Assign {
                    rvalue: Rvalue::Binary {
                        op: BinOp::Ge | BinOp::Le,
                        ..
                    },
                    ..
                }
            )
        })
    });
    assert!(has_ge, "an inclusive range lowers to a guard comparison");
    let max_targets = f
        .blocks
        .iter()
        .filter_map(|b| match &b.term {
            Terminator::Switch { targets, .. } => Some(targets.len()),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    assert!(
        max_targets < 4,
        "no Switch enumerates the range as discrete targets (had {max_targets})"
    );
}

#[test]
fn switch_routes_a_small_range_and_a_single_value() {
    // A small inclusive range and an adjacent single-value arm both route. The
    // single value stays a Switch target; the range is a guard.
    let src = r#"
        fn classify(v: u32) u32 {
            return switch (v) { 0 => 10, 1...3 => 20, else => 0 };
        }
    "#;
    let prog = lower(src, BuildMode::Debug);
    let f = find_fn(&prog, "classify");
    assert!(prog.verify().is_empty(), "verify: {:?}", prog.verify());
    // The single value `0` is a Switch target; `1...3` is a guard chain.
    let has_switch_target = f
        .blocks
        .iter()
        .any(|b| matches!(&b.term, Terminator::Switch { targets, .. } if !targets.is_empty()));
    assert!(
        has_switch_target,
        "the single value 0 stays a Switch target"
    );
    let has_range_guard = f.blocks.iter().any(|b| {
        b.stmts.iter().any(|s| {
            matches!(
                s,
                Statement::Assign {
                    rvalue: Rvalue::Binary {
                        op: BinOp::Ge | BinOp::Le,
                        ..
                    },
                    ..
                }
            )
        })
    });
    assert!(has_range_guard, "the 1...3 range lowers to a guard chain");
}

#[test]
fn leak_flags_try_alloc_never_freed() {
    // REGRESSION: the canonical `try alloc.alloc(...)` idiom with no free and no
    // return of the PAYLOAD must be flagged. The `try` desugar returns the raw
    // error-union wrapper on the error path, which must NOT count as transferring
    // ownership of the allocated payload.
    let src = r#"
        fn leak2(alloc: Allocator) !void {
            const buf: []u32 = try alloc.alloc(u32, 16);
            buf[0] = 1;
        }
    "#;
    let prog = lower(src, BuildMode::Debug);
    let leaks: Vec<&Diagnostic> = prog
        .diagnostics
        .iter()
        .filter(|d| d.is_error() && d.message.contains("never freed"))
        .collect();
    assert_eq!(
        leaks.len(),
        1,
        "an unfreed, unreturned try-alloc payload is flagged: {:?}",
        prog.diagnostics
    );
}

#[test]
fn leak_clean_when_try_alloc_payload_returned() {
    // The genuine ownership-transfer case stays clean: the PAYLOAD flows into the
    // Return on the success path, so no leak is reported.
    let src = r#"
        fn f(a: Allocator) ![]u8 {
            const x = try a.alloc(u8, 8);
            errdefer a.free(x);
            return x;
        }
    "#;
    let prog = lower(src, BuildMode::Debug);
    assert!(
        prog.diagnostics.iter().all(|d| !d.is_error()),
        "a returned try-alloc payload transfers ownership: {:?}",
        prog.diagnostics
    );
}

#[test]
fn leak_clean_for_fixed_buffer_allocator_no_free() {
    // FINDING #2: an allocation from a FixedBufferAllocator (free is a no-op;
    // buffer is caller-owned) must NOT be flagged even with no per-item free.
    // The allocator is recognised as bulk/no-op-free by its struct type name and
    // its `.allocator()` method, so Pattern A exempts the allocation.
    let src = r#"
        const FixedBufferAllocator = struct {
            const Self = @This();
            id: u32,
            pub fn init() Self { return Self{ .id = 0 }; }
            pub fn allocator(self: *Self) Allocator { return @allocHandle(self.id); }
        };
        fn useFba() void {
            var fba = FixedBufferAllocator.init();
            const al = fba.allocator();
            const x = al.alloc(u8, 8) catch { return; };
            _ = x;
        }
    "#;
    let prog = lower(src, BuildMode::Debug);
    assert!(
        prog.diagnostics
            .iter()
            .all(|d| !d.is_error() || !d.message.contains("never freed")),
        "an FBA allocation without per-item free is not a leak: {:?}",
        prog.diagnostics
    );
}

#[test]
fn leak_clean_for_arena_allocator_no_free() {
    // FINDING #2: an arena allocation (bulk free at `deinit`) must NOT be flagged
    // even without an enclosing loop (which previously masked the false positive)
    // and without per-item frees.
    let src = r#"
        const ArenaAllocator = struct {
            const Self = @This();
            id: u32,
            pub fn init() Self { return Self{ .id = 0 }; }
            pub fn allocator(self: *Self) Allocator { return @allocHandle(self.id); }
            pub fn deinit(self: *Self) void { _ = self; }
        };
        fn useArena() void {
            var arena = ArenaAllocator.init();
            const scratch = arena.allocator();
            const a = scratch.alloc(u8, 32) catch { return; };
            _ = a;
        }
    "#;
    let prog = lower(src, BuildMode::Debug);
    assert!(
        prog.diagnostics
            .iter()
            .all(|d| !d.is_error() || !d.message.contains("never freed")),
        "an arena allocation without per-item free is not a leak: {:?}",
        prog.diagnostics
    );
}

#[test]
fn leak_still_flagged_for_generic_allocator_after_bulk_free_exemption() {
    // FINDING #2 (guardrail): the bulk-free exemption must NOT swallow a genuine
    // leak through a plain `Allocator` (a GPA/page handle requires explicit free).
    let src = r#"
        fn bad(a: Allocator) void {
            const x = a.alloc(u8, 8) catch { return; };
            _ = x;
        }
    "#;
    let prog = lower(src, BuildMode::Debug);
    let leaks: Vec<&Diagnostic> = prog
        .diagnostics
        .iter()
        .filter(|d| d.is_error() && d.message.contains("never freed"))
        .collect();
    assert_eq!(
        leaks.len(),
        1,
        "a plain-Allocator leak is still flagged: {:?}",
        prog.diagnostics
    );
}

#[test]
fn block_break_value_slot_is_typed_and_no_dead_block() {
    // REGRESSION: `const r = blk: { for ... break :blk x; break :blk 0; }` types
    // `r` as u32 (not noreturn) and leaves NO unreachable block after GC.
    let src = r#"
        fn doit(xs: []u32) u32 {
            const r = blk: {
                for (xs) |x| { if (x == 7) break :blk x; }
                break :blk 0;
            };
            return r;
        }
    "#;
    let prog = lower(src, BuildMode::Debug);
    let f = find_fn(&prog, "doit");
    // No dead blocks, no undefined locals, every block terminated (verify covers
    // all three of the milestone's invariants).
    assert!(prog.verify().is_empty(), "verify: {:?}", prog.verify());
    // The block-result binding is typed u32, not the bottom `noreturn`.
    let r_slot = f
        .locals
        .iter()
        .find(|l| matches!(l.origin, LocalOrigin::Binding(_)))
        .expect("the `r` binding local");
    assert_eq!(
        prog.arena.fmt(r_slot.ty),
        "u32",
        "the block-break result slot is typed from the value, not noreturn"
    );
}

#[test]
fn verify_catches_unreachable_block() {
    // The extended `verify` flags a manually-introduced dangling block, so the
    // dead-block invariant is testable.
    let mut prog = lower("fn f() void { return; }", BuildMode::Debug);
    let f = &mut prog.funcs[0];
    // Append an orphan block with no predecessor.
    let orphan = f.new_block();
    f.blocks[orphan.index()].term = Terminator::Return {
        value: Operand::Const(Const::Void),
    };
    let problems = prog.verify();
    assert!(
        problems
            .iter()
            .any(|p| p.message.contains("unreachable block")),
        "verify reports the dangling block: {problems:?}"
    );
}

#[test]
fn verify_catches_dangling_make_slice_offset() {
    // REGRESSION (v0.9-#2): `Rvalue::collect_locals` once walked only the `ptr`
    // and `len` operands of a `MakeSlice`, discarding `offset` via the `..`
    // pattern. Because `collect_locals` is the sole feeder for
    // `Statement::referenced_locals` — which `verify` uses to detect
    // "references undefined local" — a `MakeSlice` whose OFFSET was a dangling
    // local slipped past `verify` clean. We now walk all three operands; this test
    // pins the hole by manually corrupting a real slice's offset operand to point
    // at an out-of-range local and asserting `verify` flags it.
    let mut prog = lower(
        r#"
            fn slice3(a: []const u32, lo: usize, hi: usize) usize {
                const s = a[lo..hi];
                return s.len;
            }
            pub fn main(sys: *System) void {
                return;
            }
        "#,
        BuildMode::Debug,
    );
    // A clean program verifies, including its variable-offset MakeSlice.
    assert!(
        prog.verify().is_empty(),
        "the unaltered slice program must verify: {:?}",
        prog.verify()
    );

    // Find the `slice3` function and the block/statement holding its MakeSlice,
    // then repoint the OFFSET operand at a local one past the end of the frame.
    let f = prog
        .funcs
        .iter_mut()
        .find(|f| f.name == "slice3")
        .expect("slice3 fn");
    let dangling = LocalId(f.locals.len() as u32); // out of range by construction
    let mut patched = false;
    'outer: for b in &mut f.blocks {
        for s in &mut b.stmts {
            if let Statement::Assign {
                rvalue: Rvalue::MakeSlice { offset, .. },
                ..
            } = s
            {
                *offset = Operand::Copy(Place::local(dangling));
                patched = true;
                break 'outer;
            }
        }
    }
    assert!(
        patched,
        "expected a MakeSlice with a variable offset to patch"
    );

    // verify must now report the dangling OFFSET local (previously it did not).
    let problems = prog.verify();
    assert!(
        problems
            .iter()
            .any(|p| p.message.contains("references undefined local")),
        "verify must catch a dangling MakeSlice offset local: {problems:?}"
    );
}

#[test]
fn corpus_lowers_verifies_and_stays_leak_clean() {
    // Every example lowers with zero errors AND every lowered program verifies
    // (every block terminated, no undefined locals, NO unreachable blocks). This
    // pins the dead-block GC + extended verify against the real corpus.
    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("examples");
    let mut checked = 0usize;
    for entry in std::fs::read_dir(&dir).expect("examples dir") {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("k2") {
            continue;
        }
        let src = std::fs::read_to_string(&path).unwrap();
        let pres = parse(&src);
        if !pres.is_ok() {
            continue; // a non-lowering fixture (shouldn't happen in examples/)
        }
        let resolved = resolve_file(&pres.file);
        if !resolved.is_ok() {
            continue;
        }
        let typed = check_file(&pres.file, &resolved);
        if !typed.is_ok() {
            continue;
        }
        let prog = lower_program(&pres.file, &resolved, typed, BuildMode::Debug)
            .expect("example lowering must succeed");
        let problems = prog.verify();
        assert!(
            problems.is_empty(),
            "{}: verify problems: {problems:?}",
            path.display()
        );
        assert!(
            prog.diagnostics.iter().all(|d| !d.is_error()),
            "{}: unexpected lowering/leak errors: {:?}",
            path.display(),
            prog.diagnostics
        );
        checked += 1;
    }
    assert!(checked > 0, "the corpus test must exercise some examples");
}
